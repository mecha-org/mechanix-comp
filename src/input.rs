use std::process::Command;

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
        touch::{DownEvent, UpEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::shell::wlr_layer::KeyboardInteractivity,
};
use tracing::{debug, error, info};

use crate::backend::Backend;
use crate::layout::{StackEntry, StackingOrder};
use crate::state::State;

impl<BackendData: Backend + 'static> State<BackendData> {
    /// The surface under `pos` and its location, topmost first in the order
    /// the output is drawn.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.space.outputs().next()?;
        let output_geo = self.space.output_geometry(output)?;

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
                    return Some((s.clone(), p.to_f64() + output_geo.loc.to_f64()));
                }
            }
            // If locked, don't fall through to other surfaces.
            return None;
        }

        let visible = self.visible_surfaces(output);
        let order = StackingOrder::new(&self.space, output, &self.toplevels, &visible);
        for stacked in order.iter() {
            let Some(found) = stacked.surface_under(pos) else {
                continue;
            };
            // A modal dialog blocks input to its parents.
            if let StackEntry::Window(window) = stacked.entry
                && window
                    .toplevel()
                    .is_some_and(|t| self.modal_blocks(&visible, t.wl_surface()))
            {
                return None;
            }
            return Some(found);
        }
        None
    }

    fn process_common_key_action(&mut self, action: KeyAction) {
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

    fn keyboard_key_to_action<B: InputBackend>(&mut self, evt: B::KeyboardKeyEvent) -> KeyAction {
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
                |_, modifiers, handle| {
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
                        let action = process_keyboard_shortcut(*modifiers, keysym);

                        if action.is_some() {
                            suppressed_keys.push(keysym);
                        }

                        action
                            .map(FilterResult::Intercept)
                            .unwrap_or(FilterResult::Forward)
                    } else {
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
        if !handle.is_grabbed() {
            self.assign_pointer_click_focus(touch_location, serial);
        }

        let under = self.surface_under(touch_location);
        handle.down(
            self,
            under,
            &DownEvent {
                slot: evt.slot(),
                location: touch_location,
                serial,
                time: evt.time(),
            },
        );
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

        let under = self.surface_under(touch_location);
        handle.motion(
            self,
            under,
            &smithay::input::touch::MotionEvent {
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
            self.focus_window(&ws.window.clone(), serial);
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Wake blanked outputs on any input; consume the event that woke them.
        if self.wake_outputs_if_off() {
            return;
        }
        // Any input event counts as user activity: reset the idle-notify
        // timers so `swayidle`-style clients don't go idle while the user is
        // using the compositor. The notifier itself keeps inhibited seats
        // (zwp-idle-inhibit-v1) from idling.
        self.idle_notifier_state.notify_activity(&self.seat);
        match event {
            InputEvent::Keyboard { event, .. } => match self.keyboard_key_to_action::<I>(event) {
                // TODO Separate for different backends e.g. VtSwitch
                action => match action {
                    KeyAction::VtSwitch(_)
                    | KeyAction::None
                    | KeyAction::Quit
                    | KeyAction::Run(_) => self.process_common_key_action(action),
                },
            },
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
enum KeyAction {
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
