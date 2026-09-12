//! Session-only protocol state. Compiled out of the nest.

use std::collections::HashSet;

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelListState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::input_method::InputMethodManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::wlr_data_control::DataControlState;
use smithay::wayland::session_lock::{SessionLockManagerState, SessionLocker};
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager;

use crate::backend::Backend;
use crate::handlers::foreign_toplevel::ForeignToplevelManagerState;
use crate::handlers::output_power::OutputPowerManagerState;
use crate::state::State;

pub struct Session<B: Backend + 'static> {
    pub xdg_activation_state: XdgActivationState,
    pub data_device_state: DataDeviceState,
    pub session_lock_state: SessionLockManagerState,
    pub foreign_toplevel: ForeignToplevelManagerState,
    pub foreign_toplevel_list: ForeignToplevelListState,
    pub xdg_toplevel_icon: XdgToplevelIconManager,
    pub xdg_dialog_state: XdgDialogState,
    pub idle_notifier_state: IdleNotifierState<State<B>>,
    pub idle_inhibit_manager_state: IdleInhibitManagerState,
    pub data_control_state: DataControlState,
    pub output_power: OutputPowerManagerState,
    pub idle_inhibiting_surfaces: HashSet<WlSurface>,
    pub pending_lock: Option<SessionLocker>,
}

impl<B: Backend + 'static> Session<B> {
    pub fn new(dh: &DisplayHandle, event_loop: &EventLoop<'static, State<B>>) -> Self {
        type S<B> = State<B>;
        let mut xdg_toplevel_icon = XdgToplevelIconManager::new::<S<B>>(dh);
        xdg_toplevel_icon.add_icon_size(64);
        TextInputManagerState::new::<S<B>>(dh);
        InputMethodManagerState::new::<S<B>, _>(dh, |_client| true);
        VirtualKeyboardManagerState::new::<S<B>, _>(dh, |_client| true);
        Self {
            xdg_activation_state: XdgActivationState::new::<S<B>>(dh),
            data_device_state: DataDeviceState::new::<S<B>>(dh),
            session_lock_state: SessionLockManagerState::new::<S<B>, _>(dh, |_| true),
            foreign_toplevel: ForeignToplevelManagerState::new::<S<B>>(dh),
            foreign_toplevel_list: ForeignToplevelListState::new::<S<B>>(dh),
            xdg_toplevel_icon,
            xdg_dialog_state: XdgDialogState::new::<S<B>>(dh),
            idle_notifier_state: IdleNotifierState::new(dh, event_loop.handle()),
            idle_inhibit_manager_state: IdleInhibitManagerState::new::<S<B>>(dh),
            data_control_state: DataControlState::new::<S<B>, _>(dh, None, |_| true),
            output_power: OutputPowerManagerState::new::<S<B>>(dh),
            idle_inhibiting_surfaces: HashSet::new(),
            pending_lock: None,
        }
    }
}
