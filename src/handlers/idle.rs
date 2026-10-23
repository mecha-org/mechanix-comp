use crate::backend::Backend;
use crate::state::State;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};

impl<BackendData: Backend + 'static> IdleNotifierHandler for State<BackendData> {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

impl<BackendData: Backend + 'static> IdleInhibitHandler for State<BackendData> {
    fn inhibit(&mut self, surface: WlSurface) {
        *self.idle_inhibitors.entry(surface).or_default() += 1;
        self.update_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        let Some(count) = self.idle_inhibitors.get_mut(&surface) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.idle_inhibitors.remove(&surface);
            self.update_idle_inhibit();
        }
    }
}
