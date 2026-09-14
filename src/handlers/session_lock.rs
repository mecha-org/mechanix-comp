use crate::backend::Backend;
use crate::state::{LockPhase, State};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::session_lock::{
    LockSurface, LockSurfaceConfigure, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

impl<BackendData: Backend + 'static> SessionLockHandler for State<BackendData> {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        if !matches!(self.lock_phase, LockPhase::Unlocked) {
            // Dropping the new locker sends `finished` to the late client.
            return;
        }
        self.lock_phase = LockPhase::Locking(confirmation);
        self.reset_lock_frames();

        // Clear keyboard focus from all normal clients immediately so they
        // cannot receive input while we are transitioning to the locked state.
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }

        self.schedule_render();
        self.maybe_send_locked();
    }

    fn unlock(&mut self) {
        self.lock_phase = LockPhase::Unlocked;
        self.reset_lock_frames();
        self.lock_surfaces.clear();
        self.focus_topmost();
        // The lock frame is still on the CRTC until the next redraw.
        self.schedule_render();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        // Find the corresponding output and its size.
        let output = smithay::output::Output::from_resource(&wl_output)
            .or_else(|| self.space.outputs().next().cloned());

        let size = output
            .and_then(|o| self.space.output_geometry(&o))
            .map(|geo| (geo.size.w as u32, geo.size.h as u32).into())
            .unwrap_or_else(|| (1920, 1080).into());

        surface.with_pending_state(|state| {
            state.size = Some(size);
        });

        // Give keyboard focus to the first lock surface created.
        if self.lock_surfaces.is_empty() {
            let serial = SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(surface.wl_surface().clone()), serial);
            }
        }

        self.lock_surfaces.push(surface);
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: LockSurfaceConfigure) {}
}
