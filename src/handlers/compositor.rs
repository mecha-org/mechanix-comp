use crate::backend::Backend;
use crate::state::State;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::commit_timing::CommitTimerStateUserData;
use smithay::wayland::compositor::{
    CompositorHandler, CompositorState, add_pre_commit_hook, get_parent, is_sync_subsurface,
    with_states,
};

impl<BackendData: Backend + 'static> CompositorHandler for State<BackendData> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a smithay::wayland::compositor::CompositorClientState {
        &client
            .get_data::<crate::state::ClientState>()
            .unwrap()
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        // A commit-timer timestamp asks us to hold the commit until then, so arm
        // a wakeup for the earliest pending deadline (this one or an older one).
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let timestamp = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CommitTimerStateUserData>()
                    .and_then(|timer| timer.borrow().timestamp)
            });
            if let Some(timestamp) = timestamp {
                let earliest = state
                    .next_commit_deadline()
                    .map_or(timestamp, |pending| pending.min(timestamp));
                state.wake_at(earliest);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.backend_data.early_import(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.toplevels.get(&root).map(|ws| ws.window.clone()) {
                window.on_commit();
            }
        }

        self.popups.commit(surface);
        self.ensure_initial_configure(surface);
        // A commit is new damage; render on demand instead of polling.
        self.schedule_render();
    }
}
