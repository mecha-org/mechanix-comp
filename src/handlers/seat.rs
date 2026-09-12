use crate::backend::Backend;
use crate::state::State;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::pointer_constraints::{
    ConstraintRemove, PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
};
use smithay::wayland::seat::WaylandFocus;

#[cfg(feature = "session")]
use smithay::desktop::{PopupKind, PopupManager};
#[cfg(feature = "session")]
use smithay::reexports::wayland_server::Resource;
#[cfg(feature = "session")]
use smithay::utils::Rectangle;
#[cfg(feature = "session")]
use smithay::wayland::input_method::{InputMethodHandler, PopupSurface};
#[cfg(feature = "session")]
use smithay::wayland::selection::data_device::set_data_device_focus;
#[cfg(feature = "session")]
use tracing::warn;

impl<BackendData: Backend + 'static> SeatHandler for State<BackendData> {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State<BackendData>> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    #[cfg_attr(not(feature = "session"), allow(unused_variables))]
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        #[cfg(feature = "session")]
        set_data_device_focus(
            &self.display_handle,
            seat,
            focused.and_then(|s| self.display_handle.get_client(s.id()).ok()),
        );
    }
}

impl<BackendData: Backend + 'static> PointerConstraintsHandler for State<BackendData> {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // XXX region
        let Some(current_focus) = pointer.current_focus() else {
            return;
        };
        if current_focus.wl_surface().as_deref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                constraint.unwrap().activate();
            });
        }
    }

    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        constraint_remove: ConstraintRemove,
    ) {
        // Clear cursor_position_hint to prevent a oneshot PointerLocked constraint
        // from causing this function to be called again during PointerLeave and
        // unexpectedly changing the cursor position.
        let Some((hint_surface, hint_location)) = self.cursor_position_hint.take() else {
            return;
        };

        match constraint_remove {
            ConstraintRemove::Destroyed(pointer_constraint) => match pointer_constraint {
                PointerConstraint::Confined(_confined_pointer) => return,
                PointerConstraint::Locked(locked_pointer) => {
                    let origin = self
                        .space
                        .elements()
                        .find_map(|window| {
                            (window.wl_surface().as_deref() == Some(&hint_surface))
                                .then(|| window.geometry())
                        })
                        .unwrap_or_default()
                        .loc
                        .to_f64();

                    let surface_location = origin + hint_location;
                    if let Some(region) = locked_pointer.region()
                        && region.contains(hint_location.to_i32_floor())
                    {
                        pointer.set_location(surface_location);
                    } else {
                        pointer.set_location(surface_location);
                    }
                }
            },
            ConstraintRemove::PointerLeave(_region) => return,
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        if with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|c| c.is_active())
        }) {
            self.cursor_position_hint = Some((surface.clone(), location));
        }
    }
}

#[cfg(feature = "session")]
impl<BackendData: Backend + 'static> InputMethodHandler for State<BackendData> {
    fn new_popup(&mut self, surface: PopupSurface) {
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!("Failed to track popup: {}", err);
        }
    }

    fn popup_repositioned(&mut self, _: PopupSurface) {}

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
    }

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, smithay::utils::Logical> {
        self.space
            .elements()
            .find_map(|window| {
                (window.wl_surface().as_deref() == Some(parent)).then(|| window.geometry())
            })
            .unwrap_or_default()
    }
}

impl<BackendData: Backend + 'static> TabletSeatHandler for State<BackendData> {
    type ToolFocus = WlSurface;
}
