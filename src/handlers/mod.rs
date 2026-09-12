pub mod compositor;
#[cfg(feature = "session")]
pub mod data_control;
#[cfg(feature = "session")]
pub mod data_device;
pub mod dmabuf;
#[cfg(feature = "session")]
pub mod foreign_toplevel;
pub mod fractional_scale;
#[cfg(feature = "session")]
pub mod idle;
pub mod layer_shell;
pub mod output;
#[cfg(feature = "session")]
pub mod output_power;
pub mod seat;
#[cfg(feature = "session")]
pub mod session_lock;
pub mod shm;
#[cfg(feature = "session")]
pub mod xdg_activation;
#[cfg(feature = "session")]
pub mod xdg_dialog;
pub mod xdg_shell;
#[cfg(feature = "session")]
pub mod xdg_toplevel_icon;

use crate::backend::Backend;
use crate::state::State;
use smithay::desktop::PopupKind;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

smithay::delegate_dispatch2!(@<BackendData: Backend + 'static> State<BackendData>);

impl<BackendData: Backend + 'static> State<BackendData> {
    /// One commit-time entry point from `CompositorHandler::commit`: routes the
    /// committed surface to its protocol's configure handling, keeping the
    /// commit handler protocol-agnostic.
    pub fn ensure_initial_configure(&mut self, surface: &WlSurface) {
        if layer_shell::handle_commit(self, surface) {
            return;
        }
        if xdg_shell::handle_commit(self, surface) {
            return;
        }
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            popup.send_configure().expect("initial configure failed");
        }
    }
}
