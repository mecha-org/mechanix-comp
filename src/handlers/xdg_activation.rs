use crate::backend::Backend;
use crate::state::State;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

impl<BackendData: Backend + 'static> XdgActivationHandler for State<BackendData> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if token_data.timestamp.elapsed().as_secs() < 10 {
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|tl| tl.wl_surface() == &surface))
                .cloned();
            if let Some(window) = window {
                self.focus_window(&window);
            }
        }
    }
}
