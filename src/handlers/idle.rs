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
        if self.idle_inhibiting_surfaces.insert(surface) {
            self.update_idle_inhibit();
        }
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        if self.idle_inhibiting_surfaces.remove(&surface) {
            self.update_idle_inhibit();
        }
    }
}
