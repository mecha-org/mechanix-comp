use std::process::Command;
use std::time::Duration;

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
        InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent, TouchEvent,
    },
    desktop::{WindowSurfaceType, layer_map_for_output},
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState, keysyms, xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
        touch::{
            DownEvent, GrabStartData, MotionEvent as TouchMotion, OrientationEvent, ShapeEvent,
            TouchDownGrab, TouchGrab, TouchInnerHandle, UpEvent,
        },
    },
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::shell::wlr_layer::{Anchor, KeyboardInteractivity},
};
use tracing::{debug, error, info};

use crate::backend::Backend;
use crate::layout::{Slot, StackEntry, StackingOrder, role_slot};
use crate::state::State;

/// Distance in logical pixels a touch on an edge gesture strip must travel
/// before it commits either to the edge strip (inward swipe) or to the surface
/// underneath (any other direction).
const EDGE_SWIPE_THRESHOLD: f64 = 8.0;

/// Maximum time a touch on an edge gesture strip stays undecided before being
/// delivered to the surface underneath as a press/hold. Touchscreens report
/// the first motion as late as 33 ms after the touch-down, so this leaves room
/// for a few reports.
const EDGE_SLOP_DURATION: Duration = Duration::from_millis(150);

impl<BackendData: Backend + 'static> State<BackendData> {
    /// The surface under `pos` and its location, topmost first in the order
    /// the output is drawn. `Slot::Edge` gesture strips are skipped so pointer
    /// events and non-swipe touches pass through to surfaces beneath them.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.hit_under(pos).0
    }

    /// Single front-to-back hit-test pass returning `(under, edge)`:
    /// - `under`: topmost non-`Slot::Edge` surface at `pos`.
    /// - `edge`: topmost `Slot::Edge` layer at `pos` if nothing stacked above
    ///   it (such as a system prompt) already hit `pos`.
    #[allow(clippy::type_complexity)]
    fn hit_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> (
        Option<(WlSurface, Point<f64, Logical>)>,
        Option<(WlSurface, Point<f64, Logical>, Anchor)>,
    ) {
        let Some(output) = self
            .space
            .output_under(pos)
            .next()
            .or_else(|| self.space.outputs().next())
        else {
            return (None, None);
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return (None, None);
        };

        if self.is_locked() {
            // Find if the pos is within any lock surface.
            for lock_surface in self.lock_surfaces.iter().filter(|s| s.alive()) {
                let surface = lock_surface.wl_surface();
                if let Some((s, p)) = smithay::desktop::utils::under_from_surface_tree(
                    surface,
                    pos - output_geo.loc.to_f64(),
                    (0, 0),
                    WindowSurfaceType::ALL,
                ) {
                    return (
                        Some((s.clone(), p.to_f64() + output_geo.loc.to_f64())),
                        None,
                    );
                }
            }
            // If locked, don't fall through to other surfaces.
            return (None, None);
        }

        let visible = self.visible_surfaces(output);
        let mode = self.stack_mode(output);
        let order = StackingOrder::new(&self.space, output, &visible, mode);
        let mut edge = None;
        for stacked in order.iter() {
            if let StackEntry::Layer(layer, _) = stacked.entry
                && role_slot(layer) == Some(Slot::Edge)
            {
                if edge.is_none()
                    && let Some((surface, loc)) = stacked.surface_under(pos)
                {
                    edge = Some((surface, loc, layer.cached_state().anchor));
                }
                continue;
            }
            let Some(found) = stacked.surface_under(pos) else {
                continue;
            };
            // A modal dialog blocks input to its parents.
            if let StackEntry::Window(window) = stacked.entry
                && window
                    .toplevel()
                    .is_some_and(|t| self.modal_blocks(&visible, t.wl_surface()))
            {
                return (None, edge);
            }
            return (Some(found), edge);
        }
        (None, edge)
    }

    pub(crate) fn process_common_key_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::None => (),

            KeyAction::VtSwitch(vt) => {
                self.backend_data.change_vt(vt);
            }

            KeyAction::Quit => {
                info!("Quitting.");
                self.loop_signal.stop();
            }

            KeyAction::Run(cmd) => {
                info!(cmd, "Starting program");

                if let Err(e) = Command::new(&cmd)
                    .envs(self.socket_name.to_str().map(|v| ("WAYLAND_DISPLAY", v)))
                    .spawn()
                {
                    error!(cmd, err = %e, "Failed to start program");
                }
            }
        }
    }

    /// `dark`: every output is off (see [`Self::note_input`]).
    pub(crate) fn keyboard_key_to_action<B: InputBackend>(
        &mut self,
        evt: B::KeyboardKeyEvent,
        dark: bool,
    ) -> KeyAction {
        let keycode = evt.key_code();
        let state = evt.state();
        debug!(?keycode, ?state, "key");
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time(&evt);
        let mut suppressed_keys = self.suppressed_keys.clone();
        let keyboard = self.seat.get_keyboard().unwrap();

        let action = keyboard
            .input(
                self,
                keycode,
                state,
                serial,
                time,
                |state_, modifiers, handle| {
                    // `modified_sym` as-is: shifted-letter grabs like
                    // `<Shift>a` may not match; only volume keys matter now.
                    let keysym = handle.modified_sym();

                    debug!(
                        ?state,
                        mods = ?modifiers,
                        keysym = xkb::keysym_get_name(keysym),
                        "keysym"
                    );

                    // If the key is pressed and triggered a action
                    // we will not forward the key to the client.
                    // Additionally add the key to the suppressed keys
                    // so that we can decide on a release if the key
                    // should be forwarded to the client or not.
                    if let KeyState::Pressed = state {
                        // Accelerator grabs win over shortcuts, shortcut
                        // inhibitors, the lock screen and the dark.
                        // Anything else pressed in the dark is dropped,
                        // its release too.
                        if state_.grab_press(keysym, modifiers.into(), time.millis()) || dark {
                            if !suppressed_keys.contains(&keysym) {
                                suppressed_keys.push(keysym);
                            }
                            return FilterResult::Intercept(KeyAction::None);
                        }
                        let action = process_keyboard_shortcut(*modifiers, keysym);

                        if action.is_some() {
                            suppressed_keys.push(keysym);
                        }

                        action
                            .map(FilterResult::Intercept)
                            .unwrap_or(FilterResult::Forward)
                    } else {
                        state_.grab_release(keysym, time.millis());
                        let suppressed = suppressed_keys.contains(&keysym);
                        if suppressed {
                            suppressed_keys.retain(|k| *k != keysym);
                            FilterResult::Intercept(KeyAction::None)
                        } else {
                            FilterResult::Forward
                        }
                    }
                },
            )
            .unwrap_or(KeyAction::None);

        self.suppressed_keys = suppressed_keys;
        action
    }

    fn touch_location_transformed<B: InputBackend, E: AbsolutePositionEvent<B>>(
        &self,
        evt: &E,
    ) -> Option<Point<f64, Logical>> {
        let output = self
            .space
            .outputs()
            .find(|output| output.name().starts_with("eDP"))
            .or_else(|| self.space.outputs().next());

        let output = output?;
        let output_geometry = self.space.output_geometry(output)?;

        let transform = self.backend_data.touch_transform(output);
        let size = transform.invert().transform_size(output_geometry.size);
        Some(
            transform.transform_point_in(evt.position_transformed(size), &size.to_f64())
                + output_geometry.loc.to_f64(),
        )
    }

    fn on_touch_down<B: InputBackend>(&mut self, evt: B::TouchDownEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };

        let Some(touch_location) = self.touch_location_transformed(&evt) else {
            return;
        };

        let serial = SERIAL_COUNTER.next_serial();
        let down_event = DownEvent {
            slot: evt.slot(),
            location: touch_location,
            serial,
            time: evt.time(),
        };
        let (under, edge) = self.hit_under(touch_location);

        if !handle.is_grabbed() {
            if let Some((edge_surface, edge_loc, anchor)) = edge {
                let timer = self
                    .loop_handle
                    .insert_source(
                        Timer::from_duration(EDGE_SLOP_DURATION),
                        move |_, _, state| {
                            state.expire_edge_pending_grab(serial);
                            TimeoutAction::Drop
                        },
                    )
                    .ok();
                let grab = EdgePendingGrab {
                    start_data: GrabStartData {
                        focus: under,
                        slot: down_event.slot,
                        location: down_event.location,
                    },
                    down_event,
                    edge_focus: (edge_surface, edge_loc),
                    anchor,
                    timer,
                };
                handle.set_grab(self, grab, serial);
                return;
            }
            self.assign_pointer_click_focus(touch_location, serial);
            self.touch_went_down(under.as_ref(), serial);
        }

        handle.down(self, under, &down_event);
    }

    fn expire_edge_pending_grab(&mut self, serial: smithay::utils::Serial) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        let pending = handle
            .with_grab(|active_serial, grab| {
                (active_serial == serial)
                    .then(|| grab.downcast_ref::<EdgePendingGrab<BackendData>>())
                    .flatten()
                    .map(|g| (g.down_event.clone(), g.start_data.focus.clone()))
            })
            .flatten();
        let Some((down_event, focus)) = pending else {
            return;
        };
        handle.unset_grab(self);
        self.assign_pointer_click_focus(down_event.location, down_event.serial);
        self.touch_went_down(focus.as_ref(), down_event.serial);
        handle.down(self, focus, &down_event);
        handle.frame(self);
    }

    fn on_touch_up<B: InputBackend>(&mut self, evt: B::TouchUpEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        handle.up(
            self,
            &UpEvent {
                slot: evt.slot(),
                serial,
                time: evt.time(),
            },
        )
    }

    fn on_touch_motion<B: InputBackend>(&mut self, evt: B::TouchMotionEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        let Some(touch_location) = self.touch_location_transformed(&evt) else {
            return;
        };

        // The touch-down and edge-pending grabs keep the surface(s) first
        // touched and ignore `focus`, so only other grabs (drag-and-drop) need
        // the hit test.
        let keeps_focus = handle
            .with_grab(|_, grab| {
                grab.is::<TouchDownGrab<Self>>() || grab.is::<EdgePendingGrab<BackendData>>()
            })
            .unwrap_or(false);
        let under = (!keeps_focus)
            .then(|| self.surface_under(touch_location))
            .flatten();
        handle.motion(
            self,
            under,
            &TouchMotion {
                slot: evt.slot(),
                location: touch_location,
                time: evt.time(),
            },
        );
    }

    fn on_touch_frame<B: InputBackend>(&mut self, _evt: B::TouchFrameEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        handle.frame(self);
    }

    fn on_touch_cancel<B: InputBackend>(&mut self, _evt: B::TouchCancelEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        handle.cancel(self);
    }

    fn on_device_added<B: InputBackend>(&mut self, device: B::Device) {
        if device.has_capability(DeviceCapability::Touch) && self.seat.get_touch().is_none() {
            self.seat.add_touch();
        }
    }

    fn on_device_removed<B: InputBackend>(&mut self, _device: B::Device) {}

    /// A first touch went down on `focus`: remembered for the activation
    /// tokens its serial may be asked for with.
    fn touch_went_down(
        &mut self,
        focus: Option<&(WlSurface, Point<f64, Logical>)>,
        serial: smithay::utils::Serial,
    ) {
        self.last_touch_down = focus.map(|(surface, _)| (surface.clone(), serial));
    }

    /// Same focus policy for pointer press and touch down: the surface under
    /// the point takes focus, OnDemand layers only through `layer_shell_on_demand_focus`.
    fn assign_pointer_click_focus(
        &mut self,
        pos: Point<f64, Logical>,
        serial: smithay::utils::Serial,
    ) {
        let Some((surface, _)) = self.surface_under(pos) else {
            return;
        };
        if self.is_locked() {
            self.seat
                .get_keyboard()
                .unwrap()
                .set_focus(self, Some(surface), serial);
            return;
        }

        let layer = self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map.layer_for_surface(&surface, WindowSurfaceType::ALL)?;
            let interactivity = layer.cached_state().keyboard_interactivity;
            Some((layer.wl_surface().clone(), interactivity))
        });
        if let Some((layer, interactivity)) = layer {
            if interactivity == KeyboardInteractivity::OnDemand {
                self.layer_shell_on_demand_focus = Some(layer);
            }
        } else if let Some(ws) = self
            .toplevel_of(&surface)
            .and_then(|s| self.toplevels.get(&s))
        {
            self.focus_window(&ws.window.clone());
        }
    }

    /// Note an input event as user activity, resetting the idle-notify
    /// timers (the notifier itself keeps inhibited seats from idling).
    /// Returns whether every output is off, the dark: there input reaches
    /// no client and is not activity, so a touch in a pocket neither lands
    /// nor keeps the session awake. Grabbed accelerators still reach their
    /// owner, so the shell can turn the outputs back on with the power key.
    pub(crate) fn note_input(&mut self) -> bool {
        let dark = self.is_dark();
        if !dark {
            self.idle_notifier_state.notify_activity(&self.seat);
        }
        dark
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        let dark = self.note_input();
        match event {
            InputEvent::Keyboard { event, .. } => {
                match self.keyboard_key_to_action::<I>(event, dark) {
                    // TODO Separate for different backends e.g. VtSwitch
                    action => match action {
                        KeyAction::VtSwitch(_)
                        | KeyAction::None
                        | KeyAction::Quit
                        | KeyAction::Run(_) => self.process_common_key_action(action),
                    },
                }
            }
            // In the dark, nothing new starts. A touch whose down was
            // dropped stays dropped after the outputs are back: smithay
            // ignores the motion and up of a touch it never saw. Its frames
            // go too, or smithay warns of a frame with nothing in it.
            InputEvent::PointerMotion { .. }
            | InputEvent::PointerMotionAbsolute { .. }
            | InputEvent::PointerAxis { .. }
            | InputEvent::TouchDown { .. }
            | InputEvent::TouchFrame { .. }
                if dark => {}
            InputEvent::PointerButton { event, .. }
                if dark && event.state() == ButtonState::Pressed => {}
            InputEvent::PointerMotion { event } => {
                let pointer = self.seat.get_pointer().unwrap();
                let mut pos = pointer.current_location() + event.delta();
                // Clamp the pointer to the outputs' bounds.
                if let Some(bounds) = self
                    .space
                    .outputs()
                    .filter_map(|output| self.space.output_geometry(output))
                    .reduce(|a, b| a.merge(b))
                {
                    let (x, y) = (bounds.loc.x as f64, bounds.loc.y as f64);
                    pos.x = pos.x.clamp(x, x + bounds.size.w as f64 - 1.0);
                    pos.y = pos.y.clamp(y, y + bounds.size.h as f64 - 1.0);
                }
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(pos);
                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(self);
                self.schedule_render();
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let output = self.space.outputs().next().unwrap();

                let output_geo = self.space.output_geometry(output).unwrap();

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.seat.get_pointer().unwrap();

                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(self);
                self.schedule_render();
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    self.assign_pointer_click_focus(pointer.current_location(), serial);
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            InputEvent::TouchDown { event } => self.on_touch_down::<I>(event),
            InputEvent::TouchUp { event } => self.on_touch_up::<I>(event),
            InputEvent::TouchMotion { event } => self.on_touch_motion::<I>(event),
            InputEvent::TouchFrame { event } => self.on_touch_frame::<I>(event),
            InputEvent::TouchCancel { event } => self.on_touch_cancel::<I>(event),
            InputEvent::DeviceAdded { device } => self.on_device_added::<I>(device),
            InputEvent::DeviceRemoved { device } => self.on_device_removed::<I>(device),
            _ => {}
        }
    }
}

/// Possible results of a keyboard action
#[derive(Debug)]
pub(crate) enum KeyAction {
    /// Quit the compositor
    Quit,
    /// Trigger a vt-switch
    VtSwitch(i32),
    /// run a command
    Run(String),
    /// Do nothing more
    None,
}

fn process_keyboard_shortcut(modifiers: ModifiersState, keysym: Keysym) -> Option<KeyAction> {
    if modifiers.ctrl && modifiers.alt && keysym == Keysym::BackSpace
        || modifiers.logo && keysym == Keysym::q
    {
        // ctrl+alt+backspace = quit
        // logo + q = quit
        Some(KeyAction::Quit)
    } else if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&keysym.raw())
    {
        // VTSwitch
        Some(KeyAction::VtSwitch(
            (keysym.raw() - keysyms::KEY_XF86Switch_VT_1 + 1) as i32,
        ))
    } else if modifiers.logo && keysym == Keysym::Return {
        // run terminal
        Some(KeyAction::Run("weston-terminal".into()))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeDecision {
    Pending,
    EdgeSwipe,
    PassThrough,
}

fn classify_edge_motion(
    anchor: Anchor,
    start: Point<f64, Logical>,
    pos: Point<f64, Logical>,
) -> EdgeDecision {
    let dx = pos.x - start.x;
    let dy = pos.y - start.y;
    let (inward, cross) = match (
        anchor.contains(Anchor::TOP),
        anchor.contains(Anchor::BOTTOM),
        anchor.contains(Anchor::LEFT),
        anchor.contains(Anchor::RIGHT),
    ) {
        (true, false, _, _) => (dy, dx.abs()),
        (false, true, _, _) => (-dy, dx.abs()),
        (_, _, true, false) => (dx, dy.abs()),
        (_, _, false, true) => (-dx, dy.abs()),
        _ => (0.0, dx.hypot(dy)),
    };

    if inward >= EDGE_SWIPE_THRESHOLD && inward > cross {
        EdgeDecision::EdgeSwipe
    } else if dx.hypot(dy) >= EDGE_SWIPE_THRESHOLD {
        EdgeDecision::PassThrough
    } else {
        EdgeDecision::Pending
    }
}

/// Holds a touch that landed on a `Slot::Edge` layer surface until it either
/// swipes inward (>= 8 px), moves in another direction (>= 8 px), lifts, or
/// exceeds [`EDGE_SLOP_DURATION`] (150 ms).
struct EdgePendingGrab<BackendData: Backend + 'static> {
    start_data: GrabStartData<State<BackendData>>,
    down_event: DownEvent,
    edge_focus: (WlSurface, Point<f64, Logical>),
    anchor: Anchor,
    timer: Option<RegistrationToken>,
}

impl<BackendData: Backend + 'static> EdgePendingGrab<BackendData> {
    /// Flush the buffered touch-down to either the edge surface (`to_edge`) or
    /// the surface beneath it, and hand the gesture to `TouchDownGrab` (or
    /// unset the grab when `touch_points == 0`).
    fn commit(
        &mut self,
        data: &mut State<BackendData>,
        handle: &mut TouchInnerHandle<'_, State<BackendData>>,
        to_edge: bool,
        touch_points: usize,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let serial = self.down_event.serial;
        let focus = if to_edge {
            Some(self.edge_focus.clone())
        } else {
            data.assign_pointer_click_focus(self.down_event.location, serial);
            self.start_data.focus.clone()
        };
        data.touch_went_down(focus.as_ref(), serial);
        handle.down(data, focus.clone(), &self.down_event);
        if touch_points > 0 {
            let grab = TouchDownGrab {
                start_data: GrabStartData {
                    focus: focus.clone(),
                    ..self.start_data.clone()
                },
                touch_points,
            };
            handle.set_grab(self, data, serial, grab);
        } else {
            handle.unset_grab(self, data);
        }
        focus
    }
}

impl<BackendData: Backend + 'static> TouchGrab<State<BackendData>>
    for EdgePendingGrab<BackendData>
{
    fn down(
        &mut self,
        data: &mut State<BackendData>,
        handle: &mut TouchInnerHandle<'_, State<BackendData>>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &DownEvent,
    ) {
        let focus = self.commit(data, handle, false, 2);
        handle.down(data, focus, event);
    }

    fn up(
        &mut self,
        data: &mut State<BackendData>,
        handle: &mut TouchInnerHandle<'_, State<BackendData>>,
        event: &UpEvent,
    ) {
        if event.slot == self.down_event.slot {
            self.commit(data, handle, false, 0);
            handle.frame(data);
            handle.up(data, event);
        }
    }

    fn motion(
        &mut self,
        data: &mut State<BackendData>,
        handle: &mut TouchInnerHandle<'_, State<BackendData>>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &TouchMotion,
    ) {
        if event.slot != self.down_event.slot {
            return;
        }
        let to_edge =
            match classify_edge_motion(self.anchor, self.down_event.location, event.location) {
                EdgeDecision::Pending => return,
                EdgeDecision::EdgeSwipe => true,
                EdgeDecision::PassThrough => false,
            };
        let focus = self.commit(data, handle, to_edge, 1);
        handle.motion(data, focus, event);
    }

    fn frame(
        &mut self,
        _data: &mut State<BackendData>,
        _handle: &mut TouchInnerHandle<'_, State<BackendData>>,
    ) {
    }

    fn cancel(
        &mut self,
        data: &mut State<BackendData>,
        handle: &mut TouchInnerHandle<'_, State<BackendData>>,
    ) {
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        _data: &mut State<BackendData>,
        _handle: &mut TouchInnerHandle<'_, State<BackendData>>,
        _event: &ShapeEvent,
    ) {
    }

    fn orientation(
        &mut self,
        _data: &mut State<BackendData>,
        _handle: &mut TouchInnerHandle<'_, State<BackendData>>,
        _event: &OrientationEvent,
    ) {
    }

    fn start_data(&self) -> &GrabStartData<State<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut State<BackendData>) {
        if let Some(token) = self.timer.take() {
            data.loop_handle.remove(token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_edge_classifies_upward_swipe_vs_horizontal_or_tap() {
        let bottom = Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT;
        let start = Point::from((200.0, 718.0));

        assert_eq!(
            classify_edge_motion(bottom, start, Point::from((200.0, 714.0))),
            EdgeDecision::Pending
        );
        assert_eq!(
            classify_edge_motion(bottom, start, Point::from((202.0, 709.0))),
            EdgeDecision::EdgeSwipe
        );
        assert_eq!(
            classify_edge_motion(bottom, start, Point::from((212.0, 716.0))),
            EdgeDecision::PassThrough
        );
        assert_eq!(
            classify_edge_motion(bottom, start, Point::from((200.0, 728.0))),
            EdgeDecision::PassThrough
        );
    }

    #[test]
    fn top_edge_classifies_downward_swipe_vs_horizontal_or_upward() {
        let top = Anchor::TOP | Anchor::LEFT | Anchor::RIGHT;
        let start = Point::from((120.0, 10.0));

        assert_eq!(
            classify_edge_motion(top, start, Point::from((120.0, 15.0))),
            EdgeDecision::Pending
        );
        assert_eq!(
            classify_edge_motion(top, start, Point::from((122.0, 19.0))),
            EdgeDecision::EdgeSwipe
        );
        assert_eq!(
            classify_edge_motion(top, start, Point::from((132.0, 12.0))),
            EdgeDecision::PassThrough
        );
        assert_eq!(
            classify_edge_motion(top, start, Point::from((120.0, 1.0))),
            EdgeDecision::PassThrough
        );
    }
}
