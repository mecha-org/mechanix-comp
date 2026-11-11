//! Text input and virtual keyboards: how an on-screen keyboard's keys reach
//! clients.

use smithay::backend::input::InputEvent;
use smithay::wayland::text_input::TextInputActivation;
use smithay::wayland::virtual_keyboard::{
    VirtualKeyboardBackend, VirtualKeyboardHandler, VirtualKeyboardSpecialEvent,
};
use tracing::{debug, warn};

use crate::backend::Backend;
use crate::input::KeyAction;
use crate::state::State;

impl<BackendData: Backend + 'static> TextInputActivation for State<BackendData> {}

impl<BackendData: Backend + 'static> VirtualKeyboardHandler for State<BackendData> {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        let keyboard = self.seat.get_keyboard().unwrap();
        match event {
            // Its keycodes only mean something under its own keymap.
            InputEvent::DeviceAdded { device }
            | InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { device }) => {
                if let Some(keymap) = device.keymap()
                    && let Err(err) = keyboard.set_keymap_from_keymap_file(self, keymap)
                {
                    warn!(?err, "failed to apply a virtual keyboard's keymap");
                }
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            }) => {
                keyboard.with_xkb_state(self, |mut context| {
                    context.set_modifier_mask(mods_depressed, mods_latched, mods_locked, group)
                });
            }
            InputEvent::Keyboard { event } => {
                let dark = self.note_input();
                match self.keyboard_key_to_action::<VirtualKeyboardBackend>(event, dark) {
                    // A client must not move the session off its VT.
                    KeyAction::VtSwitch(vt) => {
                        debug!(vt, "ignoring a VT switch from a virtual keyboard")
                    }
                    action => self.process_common_key_action(action),
                }
            }
            _ => {}
        }
    }
}
