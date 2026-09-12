pub mod decoration;

use std::collections::HashSet;

use crate::backend::Backend;
use crate::layout::is_dialog;
use crate::state::{State, WindowMode, WindowState};
use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window, WindowSurfaceType,
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
};
use smithay::input::{Seat, pointer::Focus};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::{wl_output, wl_seat, wl_surface::WlSurface};
use smithay::utils::{SERIAL_COUNTER, Serial};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};

impl<BackendData: Backend + 'static> XdgShellHandler for State<BackendData> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());

        #[cfg(feature = "session")]
        if let Some(output) = self.space.outputs().next().cloned() {
            // `set_parent` hasn't arrived yet, so only set bounds here; the first
            // commit decides sizing/mapping in `handle_commit`.
            let zone = layer_map_for_output(&output).non_exclusive_zone();
            surface.with_pending_state(|state| {
                state.bounds = Some(zone.size);
            });
        }

        self.toplevels.insert(
            surface.wl_surface().clone(),
            WindowState {
                window: window.clone(),
                mode: WindowMode::Floating,
                mapped: false,
                modal: false,
            },
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // The foreign-toplevel `closed` event is sent by
        // `foreign_toplevel_refresh` on the next idle callback.
        let wl = surface.wl_surface();
        for layout in self.layouts.values_mut() {
            layout.remove(wl);
        }
        self.toplevels.remove(wl);
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned();
        if let Some(window) = window {
            self.space.unmap_elem(&window);
            self.focus_topmost();
        }
    }

    #[cfg_attr(not(feature = "session"), allow(unused_variables))]
    fn parent_changed(&mut self, surface: ToplevelSurface) {
        #[cfg(feature = "session")]
        if self
            .toplevels
            .get(surface.wl_surface())
            .is_some_and(|ws| ws.mapped)
            && let Some(output) = self.primary_output()
        {
            self.apply_layout(&output);
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

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
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
        #[cfg(feature = "session")]
        if let Some(output) = self.primary_output() {
            self.apply_layout(&output);
        }
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
        #[cfg(feature = "session")]
        if let Some(output) = self.primary_output() {
            self.apply_layout(&output);
        }
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
        let output_geo = self
            .space
            .outputs()
            .next()
            .and_then(|o| self.space.output_geometry(o));
        if let Some(geo) = output_geo {
            surface.with_pending_state(|state| {
                state.size = Some(geo.size);
                state.states.set(xdg_toplevel::State::Fullscreen);
            });
            self.toplevels.get_mut(surface.wl_surface()).unwrap().mode = WindowMode::Fullscreen;
        }
        surface.send_configure();

        let window = self
            .toplevels
            .get(surface.wl_surface())
            .map(|ws| ws.window.clone());
        if let Some(window) = window {
            self.focus_window(&window, SERIAL_COUNTER.next_serial());
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
        self.toplevels.get_mut(surface.wl_surface()).unwrap().mode = WindowMode::Maximized;
        #[cfg(feature = "session")]
        if let Some(output) = self.primary_output() {
            self.apply_layout(&output);
        }
        surface.send_configure();
    }
}

impl<BackendData: Backend + 'static> State<BackendData> {
    pub fn focus_window(&mut self, window: &Window, serial: Serial) {
        // A fullscreen window keeps keyboard focus; other windows can't steal it.
        if let Some(active) = self.active_fullscreen_window()
            && &active != window
        {
            return;
        }

        let focused_surface = window.toplevel().unwrap().wl_surface().clone();
        self.active_window = Some(focused_surface.clone());
        self.layer_shell_on_demand_focus = None;

        #[cfg(feature = "session")]
        if let Some(output) = self.primary_output() {
            self.apply_layout(&output);
        }

        let group = self.active_group();
        self.set_activated_group(&group);

        self.seat
            .get_keyboard()
            .unwrap()
            .set_focus(self, Some(focused_surface), serial);
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
        let topmost = self
            .space
            .outputs()
            .next()
            .and_then(|o| self.layouts.get(o))
            .and_then(|layout| layout.top().cloned())
            .and_then(|s| self.toplevels.get(&s).map(|ws| ws.window.clone()));
        if let Some(window) = topmost {
            self.focus_window(&window, SERIAL_COUNTER.next_serial());
        } else {
            self.active_window = None;
            self.layer_shell_on_demand_focus = None;
            let keyboard = self.seat.get_keyboard().unwrap();
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }
    }

    /// Map a toplevel on its first commit and insert it into Comet.
    #[cfg(feature = "session")]
    fn handle_toplevel_first_commit(&mut self, surface: &WlSurface, window: &Window) {
        let toplevel = window.toplevel().unwrap();
        let output = self.space.outputs().next().cloned();
        let loc = output
            .as_ref()
            .map(|o| layer_map_for_output(o).non_exclusive_zone().loc)
            .unwrap_or_default();

        if let Some(output) = &output {
            self.layouts
                .entry(output.clone())
                .or_default()
                .insert(surface.clone());
        }

        self.space.map_element(window.clone(), loc, false);
        self.toplevels.get_mut(surface).unwrap().mapped = true;
        if self.active_fullscreen_window().is_none() {
            self.focus_window(window, SERIAL_COUNTER.next_serial());
        } else if let Some(output) = &output {
            self.apply_layout(output);
        }
        if !toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &root)
        else {
            return;
        };

        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        let mut target = output_geo;
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

/// Toplevel commit handler: first-commit map, keeps dialogs centered. Returns true for toplevels (skip popup path).
pub fn handle_commit<BackendData: Backend + 'static>(
    state: &mut State<BackendData>,
    surface: &WlSurface,
) -> bool {
    let Some(ws) = state.toplevels.get(surface) else {
        return false;
    };
    let mapped = ws.mapped;

    if !mapped {
        #[cfg(feature = "session")]
        let window = ws.window.clone();
        #[cfg(feature = "session")]
        state.handle_toplevel_first_commit(surface, &window);
        return true;
    }

    #[cfg(feature = "session")]
    if let Some(output) = state.primary_output() {
        state.apply_layout(&output);
    }
    true
}
