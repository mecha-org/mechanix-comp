//! `mx_accelerators_v1`: the shell's accelerator grabs, and the repeat of
//! a held grabbed key. The bookkeeping is display-free: generic over the
//! owner type (the protocol resource in production, `u32` in tests).

use std::time::Duration;

use smithay::input::keyboard::{Keysym, ModifiersState, xkb};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{Client, DataInit, DisplayHandle, GlobalDispatch, New};
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use tracing::warn;

use crate::backend::Backend;
use crate::protocols::accelerators::mx_accelerators_v1::{
    self, GrabError as ProtoError, MxAcceleratorsV1,
};
use crate::state::State;

const VERSION: u32 = 1;

/// Delay before the first repeat of a held grabbed key.
pub const REPEAT_DELAY: Duration = Duration::from_millis(400);
/// Interval between subsequent repeats.
pub const REPEAT_INTERVAL: Duration = Duration::from_millis(120);
/// Longest continuous repeat; a key held longer (stuck, or pressed in a
/// pocket) stops repeating until it is pressed again.
pub const REPEAT_MAX: Duration = Duration::from_secs(10);

/// Whether a repeat that started (first repeat sent) at `first` should
/// stop at `now` instead of sending another activation.
fn repeat_expired(first: Duration, now: Duration) -> bool {
    now.saturating_sub(first) >= REPEAT_MAX
}

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
    AlreadyGrabbed,
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
            return Err(GrabError::AlreadyGrabbed);
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

/// A held grabbed key, repeated from the compositor until its release or
/// [`REPEAT_MAX`].
struct Repeat {
    keysym: Keysym,
    owner: MxAcceleratorsV1,
    action_id: u32,
    /// The repeat timer; `None` once it stopped at [`REPEAT_MAX`], with the
    /// key still down and its release still to report.
    token: Option<RegistrationToken>,
}

/// Accelerator grabs of all `mx_accelerators_v1` objects plus the one held
/// key.
#[derive(Default)]
pub struct KeyboardGrabState {
    pub grabs: Grabs<MxAcceleratorsV1>,
    repeat: Option<Repeat>,
}

impl KeyboardGrabState {
    /// Advertise `mx_accelerators_v1`, to unrestricted clients only.
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<MxAcceleratorsV1, AcceleratorsUdata> + 'static,
    {
        dh.create_global::<D, MxAcceleratorsV1, _>(VERSION, AcceleratorsUdata);
        Self::default()
    }
}

/// Marker so the `mx_accelerators_v1` global and objects dispatch here.
pub struct AcceleratorsUdata;

impl<BackendData: Backend + 'static> State<BackendData> {
    fn cancel_repeat(&mut self) -> Option<(MxAcceleratorsV1, u32)> {
        let r = self.keyboard_grabs.repeat.take()?;
        if let Some(token) = r.token {
            self.loop_handle.remove(token);
        }
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
        // A new grabbed press ends the held one, before it starts: an owner
        // that ignores presses until a release sees this one.
        if let Some((old, old_id)) = self.cancel_repeat() {
            old.released(old_id, time);
        }
        owner.pressed(action_id, time);
        let timer_owner = owner.clone();
        let mut first = None;
        let token = self.loop_handle.insert_source(
            Timer::from_duration(REPEAT_DELAY),
            move |_, _, state| {
                let now = Duration::from(state.clock.now());
                let first = *first.get_or_insert(now);
                if repeat_expired(first, now) {
                    // Stop quietly: the key is still down, and its release
                    // is still reported. A replaced or cancelled repeat
                    // removed this timer, so the held key is this one;
                    // forget the timer without removing the source twice.
                    if let Some(r) = state.keyboard_grabs.repeat.as_mut() {
                        r.token = None;
                    }
                    return TimeoutAction::Drop;
                }
                timer_owner.pressed(action_id, now.as_millis() as u32);
                TimeoutAction::ToDuration(REPEAT_INTERVAL)
            },
        );
        match token {
            Ok(token) => {
                self.keyboard_grabs.repeat = Some(Repeat {
                    keysym,
                    owner,
                    action_id,
                    token: Some(token),
                });
            }
            Err(e) => {
                warn!(err = %e, "Failed to start accelerator repeat timer");
                // No repeat, but the release is still reported.
                self.keyboard_grabs.repeat = Some(Repeat {
                    keysym,
                    owner,
                    action_id,
                    token: None,
                });
            }
        }
        true
    }

    /// A key was released: end its repeat, if any, and report the release.
    pub fn grab_release(&mut self, keysym: Keysym, time: u32) {
        if self
            .keyboard_grabs
            .repeat
            .as_ref()
            .is_some_and(|r| r.keysym == keysym)
            && let Some((owner, action_id)) = self.cancel_repeat()
        {
            owner.released(action_id, time);
        }
    }

    fn drop_keyboard_owner(&mut self, owner: &MxAcceleratorsV1) {
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

impl<BackendData: Backend + 'static> GlobalDispatch2<MxAcceleratorsV1, State<BackendData>>
    for AcceleratorsUdata
{
    fn can_view(&self, client: &Client) -> bool {
        crate::state::client_is_unrestricted(client)
    }

    fn bind(
        &self,
        _state: &mut State<BackendData>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<MxAcceleratorsV1>,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        data_init.init(resource, AcceleratorsUdata);
    }
}

impl<BackendData: Backend + 'static> Dispatch2<MxAcceleratorsV1, State<BackendData>>
    for AcceleratorsUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &MxAcceleratorsV1,
        request: mx_accelerators_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        use mx_accelerators_v1::Request;
        match request {
            Request::Grab { accelerator } => {
                let Some(acc) = parse_accelerator(&accelerator) else {
                    resource.grab_failed(accelerator, ProtoError::InvalidAccelerator);
                    return;
                };
                match state.keyboard_grabs.grabs.grab(resource.clone(), acc) {
                    Ok(id) => resource.grabbed(accelerator, id),
                    Err(GrabError::AlreadyGrabbed) => {
                        resource.grab_failed(accelerator, ProtoError::AlreadyGrabbed)
                    }
                }
            }
            Request::Ungrab { action_id } => {
                if state.keyboard_grabs.grabs.ungrab(resource, action_id)
                    && state
                        .keyboard_grabs
                        .repeat
                        .as_ref()
                        .is_some_and(|r| r.owner == *resource && r.action_id == action_id)
                {
                    state.cancel_repeat();
                }
            }
            Request::Destroy => state.drop_keyboard_owner(resource),
        }
    }

    fn destroyed(
        &self,
        state: &mut State<BackendData>,
        _client: ClientId,
        resource: &MxAcceleratorsV1,
    ) {
        state.drop_keyboard_owner(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_stops_after_max() {
        let first = Duration::from_secs(100);
        assert!(!repeat_expired(first, first));
        assert!(!repeat_expired(first, first + REPEAT_MAX - REPEAT_INTERVAL));
        assert!(repeat_expired(first, first + REPEAT_MAX));
        assert!(repeat_expired(first, first + REPEAT_MAX + REPEAT_INTERVAL));
        // A clock that steps back never ends the repeat early.
        assert!(!repeat_expired(first, first - REPEAT_INTERVAL));
    }

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
    fn same_accelerator_twice_is_already_grabbed() {
        let mut g = Grabs::<u32>::default();
        g.grab(1, vol()).unwrap();
        assert_eq!(g.grab(1, vol()), Err(GrabError::AlreadyGrabbed));
        assert_eq!(g.grab(2, vol()), Err(GrabError::AlreadyGrabbed));
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
