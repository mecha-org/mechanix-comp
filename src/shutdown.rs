//! Stopping on SIGTERM or SIGINT as the Quit key does: the event loop
//! returns, so the compositor's state drops (the Wayland socket goes, and
//! systemd hears it is stopping) instead of the process dying wherever the
//! signal lands.

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::signals::{Signal, Signals};
use tracing::{info, warn};

/// Stop `event_loop` when SIGTERM or SIGINT arrives.
///
/// The signals are blocked on the calling thread only, so call this right
/// after the event loop is made, before anything starts a thread: a thread
/// started earlier could take them instead. Processes started later get a
/// clean signal mask, as std's `Command` resets it.
pub fn stop_on_signals<D>(event_loop: &EventLoop<'_, D>) {
    let signals = match Signals::new(&[Signal::SIGTERM, Signal::SIGINT]) {
        Ok(signals) => signals,
        Err(err) => {
            warn!("cannot watch for SIGTERM to stop cleanly on it: {err}");
            return;
        }
    };
    let stop = event_loop.get_signal();
    let inserted = event_loop
        .handle()
        .insert_source(signals, move |event, _, _| {
            info!("Stopping on {:?}.", event.signal());
            stop.stop();
        });
    if let Err(err) = inserted {
        let err = err.error;
        warn!("cannot watch for SIGTERM to stop cleanly on it: {err}");
    }
}
