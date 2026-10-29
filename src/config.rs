//! Compositor settings, from mxconfig.

use std::fs::File;
use std::io::{ErrorKind, Read};

use mxconfig::runtime::Drive;
use mxconfig::{Diagnostic, Event, Source};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use tracing::{error, warn};

use crate::backend::{Backend, snap_scale};
use crate::state::State;

mod comp {
    mxconfig::client!("resources/org.mechanix.comp.toml");
}

use comp::{Comp, CompChange};

/// The settings source, and the settings if they opened.
pub struct Config {
    source: Source,
    comp: Option<Comp>,
}

impl Config {
    /// Open the settings and follow changes to their files.
    pub fn load<D: Backend + 'static>(handle: &LoopHandle<'static, State<D>>) -> Self {
        let source = Source::new();
        // Watching before the request reads the files once, not twice.
        watch(&source, handle);
        source.request::<Comp>();
        let mut comp = None;
        // Without a bus, the request is answered at once.
        for outcome in source.outcomes() {
            match outcome.event::<Comp>() {
                Some(Event::Opened(opened)) => comp = Some(opened),
                Some(Event::OpenFailed(e)) => error!("config: settings did not open: {e}"),
                Some(Event::Diagnosed(diagnostics)) => log(&diagnostics),
                _ => {}
            }
        }
        Self { source, comp }
    }

    /// The configured scale for every output.
    pub fn scale(&self) -> Option<f64> {
        self.comp.as_ref()?.output().scale().map(snap_scale)
    }
}

/// Feed `source` the inotify events for its settings files.
fn watch<D: Backend + 'static>(source: &Source, handle: &LoopHandle<'static, State<D>>) {
    let fd = match source.inotify_fd() {
        Ok(fd) => File::from(fd),
        Err(e) => return error!("config: not watching the settings files: {e}"),
    };
    let inotify = Generic::new(fd, Interest::READ, Mode::Level);
    let inserted = handle.insert_source(inotify, |_, fd, state| {
        let mut buf = [0; 4096];
        match (&**fd).read(&mut buf) {
            Ok(n) => state.config_changed(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => error!("config: reading inotify: {e}"),
        }
        Ok(PostAction::Continue)
    });
    if let Err(e) = inserted {
        error!("config: not watching the settings files: {e}");
    }
}

/// Log the problems the settings report.
fn log(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        if diagnostic.is_error() {
            error!("config: {diagnostic}");
        } else {
            warn!("config: {diagnostic}");
        }
    }
}

impl<D: Backend + 'static> State<D> {
    /// `output`'s configured scale, or its backend default.
    pub fn output_scale(&self, output: &Output) -> f64 {
        let default = || self.backend_data.default_scale(output);
        self.config.scale().unwrap_or_else(default)
    }

    /// Feed inotify `events` to the settings and apply what changed.
    fn config_changed(&mut self, events: &[u8]) {
        self.config.source.feed_inotify(events);
        let mut rescale = false;
        for outcome in self.config.source.outcomes() {
            match outcome.event::<Comp>() {
                Some(Event::Changed(CompChange::OutputScale(_))) => rescale = true,
                Some(Event::Diagnosed(diagnostics)) => log(&diagnostics),
                _ => {}
            }
        }
        if rescale {
            for output in self.space.outputs().cloned().collect::<Vec<_>>() {
                self.reconfigure_output(&output, None);
            }
            self.schedule_render();
        }
    }
}
