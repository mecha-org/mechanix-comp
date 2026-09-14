use crate::backend::Backend;
use crate::state::{State, WindowMode};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::get_parent;
use smithay::wayland::dmabuf::{
    DmabufFeedback, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};

impl<BackendData: Backend + 'static> DmabufHandler for State<BackendData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Import eagerly to validate the buffer at attach time. The resulting
        // texture is discarded here — `GlesRenderer` weak-ref-caches dmabuf
        // imports, so the render pass reuses it when sampling the surface.
        match self.backend_data.renderer().import_dmabuf(&dmabuf, None) {
            Ok(_texture) => {
                let _ = notifier.successful::<State<BackendData>>();
            }
            Err(_) => notifier.failed(),
        }
    }

    fn new_surface_feedback(
        &mut self,
        surface: &WlSurface,
        _global: &DmabufGlobal,
    ) -> Option<DmabufFeedback> {
        // Fullscreen surfaces get scanout feedback before their first attempt.
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let output = {
            let state = self.toplevels.get(&root)?;
            if state.mode != WindowMode::Fullscreen {
                return None;
            }
            self.space
                .outputs_for_element(&state.window)
                .into_iter()
                .next()
                .or_else(|| self.space.outputs().next().cloned())?
        };
        self.backend_data.scanout_dmabuf_feedback(&output)
    }
}
