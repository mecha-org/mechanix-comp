//! Accelerator-grab bookkeeping for Phosh's `phosh_private_keyboard_event`.
//! Display-free: generic over the owner type (the protocol resource in
//! production, `u32` in tests).

use std::time::Duration;

use smithay::input::keyboard::{Keysym, ModifiersState, xkb};

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
