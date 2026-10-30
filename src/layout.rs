//! Comet layout: where windows go, which are shown, and the stacking order
//! rendering and hit-testing share.

use std::collections::{HashMap, HashSet};
use std::sync::MutexGuard;

use smithay::desktop::{
    LayerMap, LayerSurface, Space, Window, WindowSurfaceType, layer_map_for_output,
};
use smithay::output::{Mode, Output, Scale};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Point;
use smithay::utils::{Logical, Rectangle, Size};
use smithay::wayland::compositor::{SurfaceAttributes, with_states};
use smithay::wayland::session_lock::LockSurface;
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::backend::Backend;
use crate::state::State;
use crate::state::{WindowMode, WindowState};

pub fn is_dialog(toplevel: &ToplevelSurface) -> bool {
    toplevel.parent().is_some()
}

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
}

impl<BackendData: Backend + 'static> State<BackendData> {
    /// Set a toplevel's pending size/bounds/states for `mode` over `area`.
    pub(crate) fn set_toplevel_placement(
        &self,
        toplevel: &ToplevelSurface,
        area: Rectangle<i32, Logical>,
        mode: WindowMode,
    ) {
        toplevel.with_pending_state(|pending| {
            pending.states.unset(xdg_toplevel::State::Maximized);
            pending.states.unset(xdg_toplevel::State::Fullscreen);
            match mode {
                WindowMode::Fullscreen => {
                    pending.size = Some(area.size);
                    pending.bounds = None;
                    pending.states.set(xdg_toplevel::State::Fullscreen);
                }
                WindowMode::Floating => {
                    pending.size = None;
                    pending.bounds = Some(area.size);
                }
                WindowMode::Maximized => {
                    pending.size = Some(area.size);
                    pending.bounds = None;
                    pending.states.set(xdg_toplevel::State::Maximized);
                }
            }
        });
    }

    /// The placement a toplevel's mode implies: fullscreen, dialog, or maximized.
    pub(crate) fn placement_mode(
        &self,
        surface: &WlSurface,
        toplevel: &ToplevelSurface,
    ) -> WindowMode {
        if self
            .toplevels
            .get(surface)
            .is_some_and(|ws| ws.mode == WindowMode::Fullscreen)
        {
            WindowMode::Fullscreen
        } else if is_dialog(toplevel) {
            WindowMode::Floating
        } else {
            WindowMode::Maximized
        }
    }
}

impl<BackendData: Backend + 'static> State<BackendData> {
    fn parent_of(&self, surface: &WlSurface) -> Option<WlSurface> {
        self.toplevels
            .get(surface)
            .and_then(|ws| ws.window.toplevel())
            .and_then(|t| t.parent())
    }

    fn window_for(&self, surface: &WlSurface) -> Option<Window> {
        self.toplevels.get(surface).map(|ws| ws.window.clone())
    }

    fn layout_mut(&mut self, output: &Output) -> &mut Layout {
        self.layouts.entry(output.clone()).or_default()
    }

    /// The single output of the space, if any.
    pub fn primary_output(&self) -> Option<Output> {
        self.space.outputs().next().cloned()
    }

    /// Give `output` `mode`, if any, and its scale, then refit everything on it.
    pub fn reconfigure_output(&mut self, output: &Output, mode: Option<Mode>) {
        let scale = self.output_scale(output);
        let scale = (scale != output.current_scale().fractional_scale())
            .then_some(Scale::Fractional(scale));
        if mode.is_none() && scale.is_none() {
            return;
        }
        output.change_current_state(mode, None, scale, None);
        layer_map_for_output(output).arrange();
        self.apply_layout(output);
        self.update_capture_constraints(output);
        if !self.is_locked() {
            return;
        }
        let geometry = self.space.output_geometry(output);
        let size = geometry.map(|geo| Size::from((geo.size.w as u32, geo.size.h as u32)));
        let on_output = |surface: &&LockSurface| {
            self.lock_surface_outputs.get(&surface.wl_surface().id()) == Some(output)
        };
        for surface in self.lock_surfaces.iter().filter(on_output) {
            surface.with_pending_state(|pending| pending.size = size);
            surface.send_configure();
        }
    }

    /// Size, position, xdg state, and Space z-order from the Comet model.
    pub fn apply_layout(&mut self, output: &Output) {
        let zone = layer_map_for_output(output).non_exclusive_zone();
        if let Some(focused) = self.active_window.clone() {
            let group = ancestor_chain(&focused, |s| self.parent_of(s));
            self.layout_mut(output).activate_group(group);
        }
        self.layout_mut(output).set_zone(zone);

        let stack: Vec<WlSurface> = self.layout_mut(output).stack().to_vec();
        let output_geo = self.space.output_geometry(output);
        for surface in &stack {
            let Some(window) = self.window_for(surface) else {
                continue;
            };
            let Some(toplevel) = window.toplevel() else {
                continue;
            };

            // Fullscreen covers the output; dialogs center over their parent; else the work zone.
            let mode = self.placement_mode(surface, toplevel);
            let (area, loc) = match mode {
                WindowMode::Fullscreen => {
                    let Some(geo) = output_geo else {
                        continue;
                    };
                    (geo, geo.loc)
                }
                WindowMode::Floating => {
                    let loc = toplevel
                        .parent()
                        .and_then(|parent| self.window_for(&parent))
                        .and_then(|parent| self.space.element_geometry(&parent))
                        .map(|parent_geo| {
                            Point::from((
                                parent_geo.loc.x
                                    + (parent_geo.size.w - window.geometry().size.w) / 2,
                                parent_geo.loc.y
                                    + (parent_geo.size.h - window.geometry().size.h) / 2,
                            ))
                        })
                        .unwrap_or_else(|| centered_loc(zone, window.geometry()));
                    (zone, loc)
                }
                WindowMode::Maximized => (zone, zone.loc),
            };

            if let Some(ws) = self.toplevels.get_mut(surface) {
                ws.mode = mode;
                ws.layout_size = window.geometry().size;
            }
            self.set_toplevel_placement(toplevel, area, mode);
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
            self.space.relocate_element(&window, loc);
        }

        // Space z-order is the stack, back → front.
        for surface in &stack {
            if let Some(window) = self.window_for(surface) {
                self.space.raise_element(&window, false);
            }
        }
        self.schedule_render();
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

    /// Whether a shown modal dialog has `window` as a parent, blocking its input.
    pub fn modal_blocks(&self, visible: &HashSet<WlSurface>, window: &WlSurface) -> bool {
        visible
            .iter()
            .filter(|s| *s != window && self.toplevels.get(*s).is_some_and(|ws| ws.modal))
            .any(|modal| ancestor_chain(modal, |s| self.parent_of(s)).contains(window))
    }

    /// The active group's surfaces that are stacked on `output`.
    pub fn visible_surfaces(&self, output: &Output) -> HashSet<WlSurface> {
        let Some(layout) = self.layouts.get(output) else {
            return HashSet::new();
        };
        let mut visible = self.active_group();
        visible.retain(|s| layout.contains(s));
        visible
    }
}

/// One entry of the stacking order, at its location in global coordinates.
pub struct Stacked<'a> {
    pub entry: StackEntry<'a>,
    pub loc: Point<i32, Logical>,
}

/// What a stacking order entry draws and takes input for.
pub enum StackEntry<'a> {
    /// A layer surface's popups and/or its own surface tree, as `kind` selects.
    Layer(&'a LayerSurface, WindowSurfaceType),
    /// A shown window with its popups.
    Window(&'a Window),
}

/// An output's shown surfaces, holding its layer map while in use.
pub struct StackingOrder<'a> {
    space: &'a Space<Window>,
    map: MutexGuard<'a, LayerMap>,
    origin: Point<i32, Logical>,
    toplevels: &'a HashMap<WlSurface, WindowState>,
    visible: &'a HashSet<WlSurface>,
}

impl<'a> StackingOrder<'a> {
    pub fn new(
        space: &'a Space<Window>,
        output: &'a Output,
        toplevels: &'a HashMap<WlSurface, WindowState>,
        visible: &'a HashSet<WlSurface>,
    ) -> Self {
        Self {
            space,
            map: layer_map_for_output(output),
            origin: space.output_geometry(output).unwrap_or_default().loc,
            toplevels,
            visible,
        }
    }

    /// Front to back: Overlay layers, fullscreen windows, Top layers,
    /// Bottom/Background popups, the other windows, then the Bottom/Background surfaces.
    pub fn iter(&self) -> impl Iterator<Item = Stacked<'_>> {
        let popups = WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE;
        let own = WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE;
        self.layer(Layer::Overlay, WindowSurfaceType::ALL)
            .chain(self.windows(true))
            .chain(self.layer(Layer::Top, WindowSurfaceType::ALL))
            .chain(self.layer(Layer::Bottom, popups))
            .chain(self.layer(Layer::Background, popups))
            .chain(self.windows(false))
            .chain(self.layer(Layer::Bottom, own))
            .chain(self.layer(Layer::Background, own))
    }

    /// `layer`'s surfaces, newest first as the order within a layer is ours to pick.
    fn layer(&self, layer: Layer, kind: WindowSurfaceType) -> impl Iterator<Item = Stacked<'_>> {
        self.map.layers_on(layer).rev().filter_map(move |surface| {
            let loc = self.map.layer_geometry(surface)?.loc + self.origin;
            Some(Stacked {
                entry: StackEntry::Layer(surface, kind),
                loc,
            })
        })
    }

    /// The shown windows that are fullscreen, or the ones that aren't, topmost first.
    fn windows(&self, fullscreen: bool) -> impl Iterator<Item = Stacked<'_>> {
        self.space.elements().rev().filter_map(move |window| {
            let surface = window.toplevel()?.wl_surface();
            let mode = self.toplevels.get(surface)?.mode;
            if !self.visible.contains(surface) || (mode == WindowMode::Fullscreen) != fullscreen {
                return None;
            }
            let loc = self.space.element_location(window)? - window.geometry().loc;
            Some(Stacked {
                entry: StackEntry::Window(window),
                loc,
            })
        })
    }
}

impl Stacked<'_> {
    /// The surface of this entry under `pos`, and that surface's location.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let local = pos - self.loc.to_f64();
        let hit = match self.entry {
            StackEntry::Layer(layer, kind) => layer_under(layer, local, kind),
            StackEntry::Window(window) => window.surface_under(local, WindowSurfaceType::ALL),
        };
        hit.map(|(surface, loc)| (surface, (loc + self.loc).to_f64()))
    }
}

/// The surface of `layer` under `local`, its main surface only within its
/// viewport-scaled rectangle and input region so click-through layers pass the point on.
fn layer_under(
    layer: &LayerSurface,
    local: Point<f64, Logical>,
    kind: WindowSurfaceType,
) -> Option<(WlSurface, Point<i32, Logical>)> {
    let main = layer.wl_surface();
    if let Some(hit) = layer.surface_under(local, kind).filter(|(s, _)| s != main) {
        return Some(hit);
    }
    let hits = kind.contains(WindowSurfaceType::TOPLEVEL)
        && layer.geometry().to_f64().contains(local)
        && with_states(main, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let region = attributes.current().input_region.as_ref();
            region.is_none_or(|region| region.contains(local.to_i32_round()))
        });
    hits.then(|| (main.clone(), Point::default()))
}
