use crate::backend::Backend;
use crate::state::{State, root_surface};
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::reexports::calloop::Interest;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::commit_timing::CommitTimerStateUserData;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorHandler, CompositorState, SurfaceAttributes, add_blocker,
    add_pre_commit_hook, is_sync_subsurface, with_states,
};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::drm_syncobj::DrmSyncobjCachedState;

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
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let (commit_deadline, acquire_point, dmabuf) = with_states(surface, |states| {
                let commit_deadline = states
                    .data_map
                    .get::<CommitTimerStateUserData>()
                    .and_then(|timer| timer.borrow().timestamp);
                let acquire_point = states
                    .cached_state
                    .get::<DrmSyncobjCachedState>()
                    .pending()
                    .acquire_point
                    .clone();
                let dmabuf = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    });
                (commit_deadline, acquire_point, dmabuf)
            });

            // wp_commit_timing: arm the earliest pending deadline.
            if let Some(commit_deadline) = commit_deadline {
                let earliest = state
                    .next_commit_deadline()
                    .map_or(commit_deadline, |pending| pending.min(commit_deadline));
                state.wake_at(earliest);
            }
            if let Some(dmabuf) = dmabuf
                && let Some(client) = surface.client()
            {
                // Explicit sync when the client provided an acquire point.
                if let Some(acquire_point) = acquire_point
                    && let Ok((blocker, source)) = acquire_point.generate_blocker()
                    && state
                        .loop_handle
                        .insert_source(source, {
                            let client = client.clone();
                            move |_, _, data| {
                                let dh = data.display_handle.clone();
                                data.client_compositor_state(&client)
                                    .blocker_cleared(data, &dh);
                                Ok(())
                            }
                        })
                        .is_ok()
                {
                    add_blocker(surface, blocker);
                    tracing::debug!(surface = ?surface.id(), "armed explicit acquire-point blocker");
                    return;
                }

                // Otherwise (or if the explicit blocker could not be armed) wait
                // on the buffer's implicit fences.
                if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ)
                    && state
                        .loop_handle
                        .insert_source(source, move |_, _, data| {
                            let dh = data.display_handle.clone();
                            data.client_compositor_state(&client)
                                .blocker_cleared(data, &dh);
                            Ok(())
                        })
                        .is_ok()
                {
                    add_blocker(surface, blocker);
                    tracing::debug!(surface = ?surface.id(), "blocked commit on dmabuf implicit fence");
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        let root = (!is_sync_subsurface(surface)).then(|| root_surface(surface));
        if let Some(window) = root
            .as_ref()
            .and_then(|root| self.toplevels.get(root))
            .map(|ws| ws.window.clone())
        {
            window.on_commit();
        }

        self.popups.commit(surface);
        if let Some(root) = &root {
            self.capture_commit(root);
        }

        self.ensure_initial_configure(surface);
        // A commit is new damage; render on demand instead of polling.
        if !self.in_hidden_window(surface) {
            self.schedule_render();
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.popups.cleanup();
        self.forget_toplevel(surface);
        if self.idle_inhibiting_surfaces.remove(surface) {
            self.update_idle_inhibit();
        }
    }
}
