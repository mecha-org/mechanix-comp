use std::time::Duration;

use crate::backend::Backend;
use crate::state::State;
use smithay::input::Seat;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

/// How long a token stays usable.
const TOKEN_LIFETIME: Duration = Duration::from_secs(30);

impl<BackendData: Backend + 'static> XdgActivationHandler for State<BackendData> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    /// A token without a serial is taken as is; one with a serial must
    /// come from input the client got while it held the keyboard or the
    /// pointer (the serial no older than that focus).
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Smithay never drops a token itself.
        self.xdg_activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() < TOKEN_LIFETIME);
        let Some((serial, seat)) = &data.serial else {
            return true;
        };
        if Seat::from_resource(seat).as_ref() != Some(&self.seat) {
            return false;
        }
        let since_focus = |focus: Option<WlSurface>, enter: Option<Serial>| {
            focus.zip(enter).is_some_and(|(surface, enter)| {
                serial.is_no_older_than(&enter)
                    && surface.client().map(|c| c.id()) == data.client_id
            })
        };
        let keyboard = self.seat.get_keyboard().unwrap();
        let pointer = self.seat.get_pointer().unwrap();
        since_focus(keyboard.current_focus(), keyboard.last_enter())
            || since_focus(pointer.current_focus(), pointer.last_enter())
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if token_data.timestamp.elapsed() >= TOKEN_LIFETIME {
            return;
        }
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
