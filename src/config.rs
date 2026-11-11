//! Compositor settings, from mxconfig.

use std::fs::File;
use std::io::{ErrorKind, Read};

use mxconfig::runtime::Drive;
use mxconfig::{Diagnostic, Event, Source};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::utils::Transform;
use tracing::{error, warn};

use crate::backend::{Backend, snap_scale};
use crate::state::State;

mod comp {
    mxconfig::client!("resources/org.mechanix.comp.toml");
}

use comp::{Comp, CompChange, OutputTransform};

/// The settings source, and the handle once it opens.
pub struct Config {
    source: Source,
    comp: Option<Comp>,
}

impl Config {
    /// Ask for the settings and follow changes to their files. They arrive
    /// through [`State::drain_config`].
    pub fn new<D: Backend + 'static>(handle: &LoopHandle<'static, State<D>>) -> Self {
        let source = Source::new();
        // Watching before the request reads the files once, not twice.
        watch(&source, handle);
        source.request::<Comp>();
        Self { source, comp: None }
    }

    /// The configured scale for every output.
    pub fn scale(&self) -> Option<f64> {
        self.comp.as_ref()?.output().scale().map(snap_scale)
    }

    /// The configured transform for every output.
    pub fn transform(&self) -> Option<Transform> {
        Some(match self.comp.as_ref()?.output().transform()? {
            OutputTransform::Normal => Transform::Normal,
            OutputTransform::_90 => Transform::_90,
            OutputTransform::_180 => Transform::_180,
            OutputTransform::_270 => Transform::_270,
            OutputTransform::Flipped => Transform::Flipped,
            OutputTransform::Flipped90 => Transform::Flipped90,
            OutputTransform::Flipped180 => Transform::Flipped180,
            OutputTransform::Flipped270 => Transform::Flipped270,
        })
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
            Ok(n) => {
                state.config.source.feed_inotify(&buf[..n]);
                state.drain_config();
            }
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

    /// The transform outputs get from the settings.
    pub fn output_transform(&self) -> Transform {
        self.backend_data.output_transform(self.config.transform())
    }

    /// Handle what the settings source has to say.
    pub fn drain_config(&mut self) {
        for outcome in self.config.source.outcomes() {
            if let Some(event) = outcome.event::<Comp>() {
                self.on_comp(event);
            }
        }
    }

    fn on_comp(&mut self, event: Event<Comp>) {
        match event {
            Event::Opened(comp) => {
                self.config.comp = Some(comp);
                self.reconfigure_outputs();
            }
            Event::Changed(CompChange::OutputScale(_) | CompChange::OutputTransform(_)) => {
                self.reconfigure_outputs();
            }
            Event::OpenFailed(e) => error!("config: settings did not open: {e}"),
            Event::Detached(cause) => warn!("config: settings detached: {cause}"),
            Event::Diagnosed(diagnostics) => log(&diagnostics),
            _ => {}
        }
    }

    fn reconfigure_outputs(&mut self) {
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            self.reconfigure_output(&output, None);
        }
        self.schedule_render();
    }
}
