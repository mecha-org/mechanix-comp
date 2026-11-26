//! The touch side of an xdg_popup grab, which smithay leaves out: it has
//! keyboard and pointer popup grabs only.
//!
//! Touches on the grabbing client's own surfaces go through as normal (an
//! "owner-events" grab), and so do touches on layer surfaces, the shell,
//! which leave the popups open. A touch down on another app's surface
//! dismisses the client's grabbing popups and is not delivered. Once the
//! popups are gone, touches are back to normal.

use smithay::desktop::{PopupGrab, PopupUngrabStrategy};
use smithay::input::SeatHandler;
use smithay::input::touch::{
    DefaultGrab, DownEvent, GrabStartData, MotionEvent, OrientationEvent, ShapeEvent, TouchGrab,
    TouchInnerHandle, UpEvent,
};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::get_role;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::LAYER_SURFACE_ROLE;

use crate::state::root_surface;

pub struct PopupTouchGrab<D>
where
    D: SeatHandler<KeyboardFocus = WlSurface, PointerFocus = WlSurface, TouchFocus = WlSurface>
        + 'static,
{
    popup_grab: PopupGrab<D>,
    /// The grabbed popup: smithay drops the grab if it is destroyed.
    start_data: GrabStartData<D>,
}

impl<D> PopupTouchGrab<D>
where
    D: SeatHandler<KeyboardFocus = WlSurface, PointerFocus = WlSurface, TouchFocus = WlSurface>
        + 'static,
{
    pub fn new(popup_grab: &PopupGrab<D>) -> Self {
        Self {
            popup_grab: popup_grab.clone(),
            start_data: GrabStartData {
                focus: popup_grab
                    .current_grab()
                    .map(|popup| (popup, Point::default())),
                slot: None.into(),
                location: Point::default(),
            },
        }
    }

    /// Every popup of the grab is gone.
    pub fn has_ended(&self) -> bool {
        self.popup_grab.has_ended()
    }

    /// Whether a touch on `surface` goes through: the shell's, or the
    /// grabbing client's own.
    fn passes(&self, surface: &WlSurface) -> bool {
        get_role(&root_surface(surface)) == Some(LAYER_SURFACE_ROLE)
            || self
                .popup_grab
                .current_grab()
                .is_some_and(|popup| surface.same_client_as(&popup.id()))
    }
}

impl<D> TouchGrab<D> for PopupTouchGrab<D>
where
    D: SeatHandler<KeyboardFocus = WlSurface, PointerFocus = WlSurface, TouchFocus = WlSurface>
        + 'static,
{
    fn down(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &DownEvent,
    ) {
        if self.has_ended() {
            // Back to normal, this touch included.
            return DefaultGrab.down(data, handle, focus, event);
        }
        if !focus
            .as_ref()
            .is_some_and(|(surface, _)| self.passes(surface))
        {
            self.popup_grab.ungrab(PopupUngrabStrategy::All);
            handle.unset_grab(self, data);
            return;
        }
        handle.down(data, focus, event);
    }

    fn up(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, event: &UpEvent) {
        handle.up(data, event);
    }

    fn motion(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, focus, event);
    }

    fn frame(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>) {
        handle.frame(data);
    }

    fn cancel(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>) {
        handle.cancel(data);
    }

    fn shape(&mut self, data: &mut D, handle: &mut TouchInnerHandle<'_, D>, event: &ShapeEvent) {
        handle.shape(data, event);
    }

    fn orientation(
        &mut self,
        data: &mut D,
        handle: &mut TouchInnerHandle<'_, D>,
        event: &OrientationEvent,
    ) {
        handle.orientation(data, event);
    }

    fn start_data(&self) -> &GrabStartData<D> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut D) {}
}
