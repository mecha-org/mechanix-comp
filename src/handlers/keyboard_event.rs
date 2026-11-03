//! Accelerator-grab bookkeeping for Phosh's `phosh_private_keyboard_event`.
//! Display-free: generic over the owner type (the protocol resource in
//! production, `u32` in tests).

use std::time::Duration;

use smithay::input::keyboard::{Keysym, ModifiersState, xkb};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, Resource};
use smithay::wayland::Dispatch2;
use tracing::warn;

use crate::backend::Backend;
use crate::protocols::phosh_private::phosh_private_keyboard_event::{
    self, Error as ProtoError, PhoshPrivateKeyboardEvent,
};
use crate::state::State;

/// Delay before the first repeat of a held grabbed key.
pub const REPEAT_DELAY: Duration = Duration::from_millis(400);
/// Interval between subsequent repeats.
pub const REPEAT_INTERVAL: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccelMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl From<&ModifiersState> for AccelMods {
    fn from(m: &ModifiersState) -> Self {
        Self {
            ctrl: m.ctrl,
            alt: m.alt,
            shift: m.shift,
            logo: m.logo,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accelerator {
    pub keysym: Keysym,
    pub mods: AccelMods,
}

/// Parse a GTK-style accelerator such as `<Super><Shift>a`.
pub fn parse_accelerator(s: &str) -> Option<Accelerator> {
    let mut rest = s;
    let mut mods = AccelMods::default();
    while let Some(after) = rest.strip_prefix('<') {
        let end = after.find('>')?;
        match after[..end].to_ascii_lowercase().as_str() {
            "shift" => mods.shift = true,
            "control" | "ctrl" | "primary" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "super" | "logo" | "meta" => mods.logo = true,
            _ => return None,
        }
        rest = &after[end + 1..];
    }
    if rest.is_empty() {
        return None;
    }
    let keysym = xkb::keysym_from_name(rest, xkb::KEYSYM_NO_FLAGS);
    if keysym == Keysym::NoSymbol {
        return None;
    }
    Some(Accelerator { keysym, mods })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabError {
    #[allow(dead_code)] // parsing rejects bad keysyms before `grab`
    InvalidKeysym,
    AlreadySubscribed,
}

/// All live grabs, keyed by owner.
#[derive(Debug)]
pub struct Grabs<C> {
    entries: Vec<(C, u32, Accelerator)>,
    next_id: u32,
}

impl<C> Default for Grabs<C> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }
}

impl<C: PartialEq> Grabs<C> {
    pub fn grab(&mut self, owner: C, acc: Accelerator) -> Result<u32, GrabError> {
        if self.entries.iter().any(|(_, _, a)| *a == acc) {
            return Err(GrabError::AlreadySubscribed);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push((owner, id, acc));
        Ok(id)
    }

    /// False if `action_id` is not one of `owner`'s grabs.
    pub fn ungrab(&mut self, owner: &C, action_id: u32) -> bool {
        match self
            .entries
            .iter()
            .position(|(o, id, _)| o == owner && *id == action_id)
        {
            Some(i) => {
                self.entries.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn drop_owner(&mut self, owner: &C) {
        self.entries.retain(|(o, _, _)| o != owner);
    }

    pub fn lookup(&self, keysym: Keysym, mods: AccelMods) -> Option<(&C, u32)> {
        self.entries
            .iter()
            .find(|(_, _, a)| a.keysym == keysym && a.mods == mods)
            .map(|(o, id, _)| (o, *id))
    }
}

/// A held grabbed key being repeated from the compositor.
struct Repeat {
    keysym: Keysym,
    owner: PhoshPrivateKeyboardEvent,
    action_id: u32,
    token: RegistrationToken,
}

/// Accelerator grabs of all `phosh_private_keyboard_event` resources plus the
/// one active repeat.
#[derive(Default)]
pub struct KeyboardGrabState {
    pub grabs: Grabs<PhoshPrivateKeyboardEvent>,
    repeat: Option<Repeat>,
}

/// Marker for `phosh_private_keyboard_event` resources.
pub struct KeyboardEventUdata;

impl<BackendData: Backend + 'static> State<BackendData> {
    fn cancel_repeat(&mut self) -> Option<(PhoshPrivateKeyboardEvent, u32)> {
        let r = self.keyboard_grabs.repeat.take()?;
        self.loop_handle.remove(r.token);
        Some((r.owner, r.action_id))
    }

    /// A key was pressed: if it is a grabbed accelerator, notify the owner,
    /// start the repeat and return true (the key must not reach clients).
    pub fn grab_press(&mut self, keysym: Keysym, mods: AccelMods, time: u32) -> bool {
        let Some((owner, action_id)) = self
            .keyboard_grabs
            .grabs
            .lookup(keysym, mods)
            .map(|(o, id)| (o.clone(), id))
        else {
            return false;
        };
        owner.accelerator_activated_event(action_id, time);
        // A new grabbed press replaces a running repeat; that key is done.
        if let Some((old, old_id)) = self.cancel_repeat()
            && old.version() >= 7
        {
            old.accelerator_released_event(old_id, time);
        }
        let timer_owner = owner.clone();
        let token = self.loop_handle.insert_source(
            Timer::from_duration(REPEAT_DELAY),
            move |_, _, state| {
                let now = Duration::from(state.clock.now()).as_millis() as u32;
                timer_owner.accelerator_activated_event(action_id, now);
                TimeoutAction::ToDuration(REPEAT_INTERVAL)
            },
        );
        match token {
            Ok(token) => {
                self.keyboard_grabs.repeat = Some(Repeat {
                    keysym,
                    owner,
                    action_id,
                    token,
                });
            }
            Err(e) => warn!(err = %e, "Failed to start accelerator repeat timer"),
        }
        true
    }

    /// A key was released: end its repeat and report the release.
    pub fn grab_release(&mut self, keysym: Keysym, time: u32) {
        if self
            .keyboard_grabs
            .repeat
            .as_ref()
            .is_some_and(|r| r.keysym == keysym)
            && let Some((owner, action_id)) = self.cancel_repeat()
            && owner.version() >= 7
        {
            owner.accelerator_released_event(action_id, time);
        }
    }

    /// Outputs blanked: the wake-on-input path eats the next event, possibly
    /// the key's release, so end any repeat now (reporting the release) and
    /// forget its suppressed keysym.
    pub fn end_grab_repeat(&mut self) {
        let Some(keysym) = self.keyboard_grabs.repeat.as_ref().map(|r| r.keysym) else {
            return;
        };
        let now = Duration::from(self.clock.now()).as_millis() as u32;
        self.grab_release(keysym, now);
        self.suppressed_keys.retain(|k| *k != keysym);
    }

    fn drop_keyboard_owner(&mut self, owner: &PhoshPrivateKeyboardEvent) {
        self.keyboard_grabs.grabs.drop_owner(owner);
        if self
            .keyboard_grabs
            .repeat
            .as_ref()
            .is_some_and(|r| r.owner == *owner)
        {
            self.cancel_repeat();
        }
    }
}

impl<BackendData: Backend + 'static> Dispatch2<PhoshPrivateKeyboardEvent, State<BackendData>>
    for KeyboardEventUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &PhoshPrivateKeyboardEvent,
        request: phosh_private_keyboard_event::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        use phosh_private_keyboard_event::Request;
        match request {
            Request::GrabAcceleratorRequest { accelerator } => {
                let Some(acc) = parse_accelerator(&accelerator) else {
                    resource.grab_failed_event(accelerator, ProtoError::InvalidKeysym as u32);
                    return;
                };
                match state.keyboard_grabs.grabs.grab(resource.clone(), acc) {
                    Ok(id) => resource.grab_success_event(accelerator, id),
                    Err(GrabError::AlreadySubscribed) => resource
                        .grab_failed_event(accelerator, ProtoError::AlreadySubscribed as u32),
                    Err(GrabError::InvalidKeysym) => {
                        resource.grab_failed_event(accelerator, ProtoError::InvalidKeysym as u32)
                    }
                }
            }
            Request::UngrabAcceleratorRequest { action_id } => {
                if state.keyboard_grabs.grabs.ungrab(resource, action_id) {
                    if state
                        .keyboard_grabs
                        .repeat
                        .as_ref()
                        .is_some_and(|r| r.owner == *resource && r.action_id == action_id)
                    {
                        state.cancel_repeat();
                    }
                    resource.ungrab_success_event(action_id);
                } else {
                    resource.ungrab_failed_event(action_id, ProtoError::InvalidArgument as u32);
                }
            }
            Request::Destroy => state.drop_keyboard_owner(resource),
        }
    }

    fn destroyed(
        &self,
        state: &mut State<BackendData>,
        _client: ClientId,
        resource: &PhoshPrivateKeyboardEvent,
    ) {
        state.drop_keyboard_owner(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol() -> Accelerator {
        parse_accelerator("XF86AudioRaiseVolume").unwrap()
    }

    #[test]
    fn parses_bare_keysym() {
        let a = parse_accelerator("XF86AudioRaiseVolume").unwrap();
        assert_eq!(a.keysym, Keysym::XF86_AudioRaiseVolume);
        assert_eq!(a.mods, AccelMods::default());
    }

    #[test]
    fn parses_modifiers() {
        let a = parse_accelerator("<Super><Shift>a").unwrap();
        assert_eq!(a.keysym, Keysym::a);
        assert!(a.mods.logo && a.mods.shift && !a.mods.ctrl && !a.mods.alt);
        assert!(parse_accelerator("<ctrl>q").unwrap().mods.ctrl);
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_accelerator("").is_none());
        assert!(parse_accelerator("NotAKey").is_none());
        assert!(parse_accelerator("<Super>").is_none());
    }

    #[test]
    fn grab_assigns_increasing_ids() {
        let mut g = Grabs::<u32>::default();
        assert_eq!(g.grab(1, vol()), Ok(1));
        assert_eq!(
            g.grab(1, parse_accelerator("XF86AudioLowerVolume").unwrap()),
            Ok(2)
        );
    }

    #[test]
    fn same_accelerator_twice_is_already_subscribed() {
        let mut g = Grabs::<u32>::default();
        g.grab(1, vol()).unwrap();
        assert_eq!(g.grab(1, vol()), Err(GrabError::AlreadySubscribed));
        assert_eq!(g.grab(2, vol()), Err(GrabError::AlreadySubscribed));
    }

    #[test]
    fn ungrab_only_by_owner() {
        let mut g = Grabs::<u32>::default();
        let id = g.grab(1, vol()).unwrap();
        assert!(!g.ungrab(&2, id));
        assert!(g.ungrab(&1, id));
        assert!(!g.ungrab(&1, id));
        assert!(g.grab(2, vol()).is_ok());
    }

    #[test]
    fn drop_owner_frees_its_grabs() {
        let mut g = Grabs::<u32>::default();
        g.grab(1, vol()).unwrap();
        g.drop_owner(&1);
        assert!(
            g.lookup(Keysym::XF86_AudioRaiseVolume, AccelMods::default())
                .is_none()
        );
        assert!(g.grab(2, vol()).is_ok());
    }

    #[test]
    fn lookup_requires_exact_mods() {
        let mut g = Grabs::<u32>::default();
        let id = g.grab(7, vol()).unwrap();
        assert_eq!(
            g.lookup(Keysym::XF86_AudioRaiseVolume, AccelMods::default()),
            Some((&7, id))
        );
        let shift = AccelMods {
            shift: true,
            ..Default::default()
        };
        assert!(g.lookup(Keysym::XF86_AudioRaiseVolume, shift).is_none());
    }
}
