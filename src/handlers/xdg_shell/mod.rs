pub mod decoration;

use std::collections::HashSet;

use crate::backend::Backend;
use crate::layout::is_dialog;
use crate::state::{State, WindowMode, WindowState};
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window, WindowSurfaceType,
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
};
use smithay::input::{Seat, pointer::Focus};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::{wl_output, wl_seat, wl_surface::WlSurface};
use smithay::utils::{Serial, Size};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};

impl<BackendData: Backend + 'static> XdgShellHandler for State<BackendData> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());

        self.toplevels.insert(
            surface.wl_surface().clone(),
            WindowState {
                window: window.clone(),
                mode: WindowMode::Floating,
                output: None,
                modal: false,
                layout_size: Size::default(),
            },
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // The foreign-toplevel `closed` event is sent by
        // `foreign_toplevel_refresh` on the next idle callback.
        self.forget_toplevel(surface.wl_surface());
        self.forget_thumbnail_surface(surface.wl_surface());
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        if self.is_mapped(surface.wl_surface()) {
            self.relayout(surface.wl_surface());
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Layer popups get their parent patched in later; they're tracked by
        // `WlrLayerShellHandler::new_popup`, so skip them here.
        if surface.get_parent_surface().is_none() {
            return;
        }
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.popups.cleanup();
        // A closed popup damages the window it belonged to.
        self.schedule_render();
        if let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(surface)) {
            self.capture_commit(&root);
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        if self.is_locked() {
            surface.send_popup_done();
            return;
        }
        let seat = Seat::from_resource(&seat).unwrap();
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };

        let ret = self.popups.grab_popup(root, kind, &seat, serial);

        if let Ok(mut grab) = ret {
            if let Some(keyboard) = seat.get_keyboard() {
                if keyboard.is_grabbed()
                    && !(keyboard.has_grab(serial)
                        || grab.previous_serial().is_none_or(|s| keyboard.has_grab(s)))
                {
                    grab.ungrab(PopupUngrabStrategy::All);
                    return;
                }
                keyboard.set_focus(self, grab.current_grab(), serial);
                keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
            }
            if let Some(pointer) = seat.get_pointer() {
                if pointer.is_grabbed()
                    && !(pointer.has_grab(serial)
                        || grab.previous_serial().is_none_or(|s| pointer.has_grab(s)))
                {
                    grab.ungrab(PopupUngrabStrategy::All);
                    return;
                }
                pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
            }
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // Dialogs are never maximized.
        if is_dialog(&surface) {
            return;
        }
        // A fullscreen window keeps its mode.
        if matches!(
            self.toplevels.get(surface.wl_surface()).map(|ws| ws.mode),
            Some(WindowMode::Fullscreen)
        ) {
            return;
        }
        self.relayout(surface.wl_surface());
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        // Dialogs are never maximized, so ignore restore requests.
        if is_dialog(&surface) {
            return;
        }
        // Not maximized (e.g. the spurious restore GTK sends on a floating
        // window): nothing to restore.
        if !matches!(
            self.toplevels.get(surface.wl_surface()).map(|ws| ws.mode),
            Some(WindowMode::Maximized)
        ) {
            return;
        }
        self.relayout(surface.wl_surface());
        surface.send_configure();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        // Dialogs are never fullscreen.
        if is_dialog(&surface) {
            return;
        }
        if matches!(
            self.toplevels.get(surface.wl_surface()).map(|ws| ws.mode),
            Some(WindowMode::Fullscreen)
        ) {
            return;
        }
        if let Some(window) = self.apply_mode_request(surface.wl_surface(), WindowMode::Fullscreen)
        {
            self.focus_window(&window);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        // Not fullscreen (e.g. the `unset_fullscreen` GTK sends on every
        // headerbar drag): nothing to restore.
        if !matches!(
            self.toplevels.get(surface.wl_surface()).map(|ws| ws.mode),
            Some(WindowMode::Fullscreen)
        ) {
            return;
        }
        self.apply_mode_request(surface.wl_surface(), WindowMode::Maximized);
    }
}

impl<BackendData: Backend + 'static> State<BackendData> {
    /// Make `window` the active one, raised with its family. The keyboard
    /// follows unless an `Exclusive` layer holds it, until that layer goes.
    pub fn focus_window(&mut self, window: &Window) {
        let focused_surface = window.toplevel().unwrap().wl_surface().clone();
        let already = self.active_window.as_ref() == Some(&focused_surface);
        self.active_window = Some(focused_surface.clone());
        self.layer_shell_on_demand_focus = None;

        if !already {
            self.relayout(&focused_surface);
            let group = self.active_group();
            self.set_activated_group(&group);
        }
        self.update_keyboard_focus();
    }

    /// Set the xdg `activated` state on every toplevel in `group` and clear it
    /// on the others, then flush the pending states to clients.
    fn set_activated_group(&mut self, group: &HashSet<WlSurface>) {
        for element in self.space.elements() {
            let toplevel = element.toplevel().unwrap();
            let is_active = group.contains(toplevel.wl_surface());
            toplevel.with_pending_state(|state| {
                if is_active {
                    state.states.set(xdg_toplevel::State::Activated);
                } else {
                    state.states.unset(xdg_toplevel::State::Activated);
                }
            });
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
    }

    pub fn focus_topmost(&mut self) {
        let topmost = self.space.elements().last().cloned();
        if let Some(window) = topmost {
            self.focus_window(&window);
        } else {
            self.active_window = None;
            self.layer_shell_on_demand_focus = None;
            self.update_keyboard_focus();
        }
    }

    fn map_toplevel(&mut self, surface: &WlSurface, window: &Window) {
        let output = self.output_of(surface);
        let loc = output
            .as_ref()
            .and_then(|o| self.output_areas(o))
            .map(|areas| areas.zone.loc)
            .unwrap_or_default();

        self.toplevels.get_mut(surface).unwrap().output = output.clone();
        self.space.map_element(window.clone(), loc, false);
        // A new window takes focus; a parent mapped after its focused dialog
        // leaves the focus on the dialog.
        if !self.active_group().contains(surface) {
            self.focus_window(window);
        } else if let Some(output) = &output {
            self.apply_layout(output);
        }
    }

    fn unmap_toplevel(&mut self, surface: &WlSurface, window: &Window) {
        // A remap is a fresh mapping: reset the mode so it is re-evaluated.
        self.toplevels.get_mut(surface).unwrap().mode = WindowMode::Floating;
        self.space.unmap_elem(window);
        self.stop_captures_for_toplevel(surface);
        self.focus_topmost();
        self.schedule_render();
    }

    /// Drop a toplevel once its xdg_toplevel or wl_surface is gone.
    pub fn forget_toplevel(&mut self, surface: &WlSurface) {
        let Some(ws) = self.toplevels.remove(surface) else {
            return;
        };
        self.stop_captures_for_toplevel(surface);
        if self.space.elements().any(|window| window == &ws.window) {
            self.space.unmap_elem(&ws.window);
            self.focus_topmost();
            self.schedule_render();
        }
    }

    /// Apply a mode change: reposition a mapped window, or reply to an unmapped one.
    fn apply_mode_request(&mut self, surface: &WlSurface, mode: WindowMode) -> Option<Window> {
        self.toplevels.get_mut(surface).unwrap().mode = mode;
        let window = self.toplevels.get(surface).map(|ws| ws.window.clone())?;
        self.reply_placement(surface, &window);
        Some(window)
    }

    /// Reposition a mapped window, or reply to one that can't be laid out yet.
    fn reply_placement(&mut self, surface: &WlSurface, window: &Window) {
        if self.is_mapped(surface) {
            self.relayout(surface);
        } else if window
            .toplevel()
            .is_some_and(|toplevel| toplevel.is_initial_configure_sent())
        {
            self.configure_toplevel(surface, window);
        }
    }

    /// Send a configure carrying the toplevel's current placement.
    fn configure_toplevel(&mut self, surface: &WlSurface, window: &Window) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let mode = self.placement_mode(surface, toplevel);
        self.toplevels.get_mut(surface).unwrap().mode = mode;
        if let Some(area) = self.placement_area_of(surface, mode) {
            self.set_toplevel_placement(toplevel, area, mode);
        }
        toplevel.send_pending_configure();
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.toplevels.get(&root).map(|ws| &ws.window) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        // Popups stay within their window's area, clear of the bar and keyboard.
        let mode = self.placement_mode(&root, toplevel);
        let Some(mut target) = self.placement_area_of(&root, mode) else {
            return;
        };
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    pub fn unconstrain_layer_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(output) = self.space.outputs().find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                .is_some()
        }) else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };

        let map = layer_map_for_output(output);
        let Some(layer) = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL) else {
            return;
        };
        let Some(layer_geo) = map.layer_geometry(layer) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= layer_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

pub fn handle_commit<BackendData: Backend + 'static>(
    state: &mut State<BackendData>,
    surface: &WlSurface,
) -> bool {
    if !state.toplevels.contains_key(surface) {
        return false;
    }
    {
        let window = state.toplevels[surface].window.clone();
        let mapped = state.is_mapped(surface);
        let buffered = with_renderer_surface_state(surface, |states| states.buffer().is_some())
            .unwrap_or(false);

        match (buffered, mapped) {
            (true, false) => state.map_toplevel(surface, &window),
            (false, true) => state.unmap_toplevel(surface, &window),
            (false, false) => state.configure_toplevel(surface, &window),
            (true, true) => {
                // Keep floating windows centred.
                let ws = &state.toplevels[surface];
                if ws.mode == WindowMode::Floating && ws.layout_size != window.geometry().size {
                    state.relayout(surface);
                }
            }
        }
    }
    true
}
