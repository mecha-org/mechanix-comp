//! Comet layout: one stateful model per output, owning the window stack and the work zone.

use std::collections::HashSet;

#[cfg(feature = "session")]
use smithay::desktop::{Window, layer_map_for_output};
use smithay::output::Output;
#[cfg(feature = "session")]
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
#[cfg(feature = "session")]
use smithay::utils::{Point, Size};
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::backend::Backend;
use crate::state::State;
#[cfg(feature = "session")]
use crate::state::WindowMode;

pub fn is_dialog(toplevel: &ToplevelSurface) -> bool {
    toplevel.parent().is_some()
}

#[cfg(feature = "session")]
fn centered_loc(
    zone: Rectangle<i32, Logical>,
    geo: Rectangle<i32, Logical>,
) -> Point<i32, Logical> {
    Point::from((
        zone.loc.x + (zone.size.w - geo.size.w).max(0) / 2,
        zone.loc.y + (zone.size.h - geo.size.h).max(0) / 2,
    ))
}

fn ancestor_chain(
    focused: &WlSurface,
    parent_of: impl Fn(&WlSurface) -> Option<WlSurface>,
) -> Vec<WlSurface> {
    let mut chain = vec![focused.clone()];
    let mut current = focused.clone();
    while let Some(parent) = parent_of(&current) {
        chain.push(parent.clone());
        current = parent;
    }
    chain.reverse();
    chain
}

/// Per-output model: the window stack (back → front) and the work zone.
#[derive(Debug, Default)]
pub struct Layout {
    stack: Vec<WlSurface>,
    zone: Rectangle<i32, Logical>,
}

impl Layout {
    /// Whether `surface` is stacked on this output.
    pub fn contains(&self, surface: &WlSurface) -> bool {
        self.stack.contains(surface)
    }

    pub fn insert(&mut self, surface: WlSurface) {
        if !self.stack.iter().any(|s| s == &surface) {
            self.stack.push(surface);
        }
    }

    pub fn remove(&mut self, surface: &WlSurface) {
        self.stack.retain(|s| s != surface);
    }

    pub fn retain(&mut self, f: impl Fn(&WlSurface) -> bool) {
        self.stack.retain(f);
    }

    pub fn set_zone(&mut self, zone: Rectangle<i32, Logical>) {
        self.zone = zone;
    }

    /// Move this ancestor chain (root → leaf) to the front of the stack.
    pub fn activate_group(&mut self, group: Vec<WlSurface>) {
        let group_set: HashSet<_> = group.iter().cloned().collect();
        self.stack.retain(|s| !group_set.contains(s));
        self.stack.extend(group);
    }

    pub fn stack(&self) -> &[WlSurface] {
        &self.stack
    }

    pub fn top(&self) -> Option<&WlSurface> {
        self.stack.last()
    }

    /// What is on screen when `focused` leads the group: the ancestor chain.
    pub fn visible(
        &self,
        focused: Option<&WlSurface>,
        parent_of: impl Fn(&WlSurface) -> Option<WlSurface>,
    ) -> HashSet<WlSurface> {
        match focused {
            Some(focused) => ancestor_chain(focused, parent_of).into_iter().collect(),
            None => HashSet::new(),
        }
    }
}

#[cfg(feature = "session")]
struct Placement {
    loc: Point<i32, Logical>,
    size: Option<Size<i32, Logical>>,
    bounds: Option<Size<i32, Logical>>,
    maximized: bool,
    reported: WindowMode,
}

impl<BackendData: Backend + 'static> State<BackendData> {
    fn parent_of(&self, surface: &WlSurface) -> Option<WlSurface> {
        self.toplevels
            .get(surface)
            .and_then(|ws| ws.window.toplevel())
            .and_then(|t| t.parent())
    }

    #[cfg(feature = "session")]
    fn window_for(&self, surface: &WlSurface) -> Option<Window> {
        self.toplevels.get(surface).map(|ws| ws.window.clone())
    }

    #[cfg(feature = "session")]
    fn layout_mut(&mut self, output: &Output) -> &mut Layout {
        self.layouts.entry(output.clone()).or_default()
    }

    /// The single output of the space, if any.
    pub fn primary_output(&self) -> Option<Output> {
        self.space.outputs().next().cloned()
    }

    /// Size, position, xdg state, and Space z-order from the Comet model.
    #[cfg(feature = "session")]
    pub fn apply_layout(&mut self, output: &Output) {
        let zone = layer_map_for_output(output).non_exclusive_zone();
        if let Some(focused) = self.active_window.clone() {
            let group = ancestor_chain(&focused, |s| self.parent_of(s));
            self.layout_mut(output).activate_group(group);
        }
        self.layout_mut(output).set_zone(zone);

        let stack: Vec<WlSurface> = self.layout_mut(output).stack().to_vec();
        for surface in &stack {
            let Some(window) = self.window_for(surface) else {
                continue;
            };
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            if self
                .toplevels
                .get(surface)
                .is_some_and(|ws| ws.mode == WindowMode::Fullscreen)
            {
                continue;
            }

            let dialog = is_dialog(toplevel);
            let placement = if dialog {
                let loc = toplevel
                    .parent()
                    .and_then(|parent| self.window_for(&parent))
                    .and_then(|parent| self.space.element_geometry(&parent))
                    .map(|parent_geo| {
                        Point::from((
                            parent_geo.loc.x + (parent_geo.size.w - window.geometry().size.w) / 2,
                            parent_geo.loc.y + (parent_geo.size.h - window.geometry().size.h) / 2,
                        ))
                    })
                    .unwrap_or_else(|| centered_loc(zone, window.geometry()));
                Placement {
                    loc,
                    size: None,
                    bounds: Some(zone.size),
                    maximized: false,
                    reported: WindowMode::Floating,
                }
            } else {
                Placement {
                    loc: zone.loc,
                    size: Some(zone.size),
                    bounds: None,
                    maximized: true,
                    reported: WindowMode::Maximized,
                }
            };

            if let Some(ws) = self.toplevels.get_mut(surface) {
                ws.mode = placement.reported;
            }
            toplevel.with_pending_state(|pending| {
                pending.size = placement.size;
                pending.bounds = placement.bounds;
                if placement.maximized {
                    pending.states.set(xdg_toplevel::State::Maximized);
                } else {
                    pending.states.unset(xdg_toplevel::State::Maximized);
                }
            });
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
            self.space.relocate_element(&window, placement.loc);
        }

        // Space z-order is the stack, back → front.
        for surface in &stack {
            if let Some(window) = self.window_for(surface) {
                self.space.raise_element(&window, false);
            }
        }
    }
}

impl<BackendData: Backend + 'static> State<BackendData> {
    /// The focused window plus its ancestor chain.
    pub fn active_group(&self) -> HashSet<WlSurface> {
        let Some(focused) = &self.active_window else {
            return HashSet::new();
        };
        ancestor_chain(focused, |s| self.parent_of(s))
            .into_iter()
            .collect()
    }

    /// The active group's surfaces that are stacked on `output`.
    pub fn visible_surfaces(&self, output: &Output) -> HashSet<WlSurface> {
        let Some(layout) = self.layouts.get(output) else {
            return HashSet::new();
        };
        layout
            .visible(self.active_window.as_ref(), |s| self.parent_of(s))
            .into_iter()
            .filter(|s| layout.contains(s))
            .collect()
    }
}
