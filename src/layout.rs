//! Comet layout: where windows go, which are shown, and the stacking order
//! rendering and hit-testing share.

use std::collections::HashSet;
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
use crate::state::WindowMode;

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

/// An output's work zone (minus exclusive zones) and whole area, global.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OutputAreas {
    pub zone: Rectangle<i32, Logical>,
    pub full: Rectangle<i32, Logical>,
}

impl OutputAreas {
    /// The area `mode` places a window in: the whole output when fullscreen,
    /// else the work zone. Dialogs use the work zone even over a fullscreen
    /// window, so the bar and keyboard never cover them.
    pub(crate) fn for_mode(&self, mode: WindowMode) -> Rectangle<i32, Logical> {
        match mode {
            WindowMode::Fullscreen => self.full,
            WindowMode::Maximized | WindowMode::Floating => self.zone,
        }
    }
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

    /// `output`'s work zone and whole area, both in global coordinates.
    pub(crate) fn output_areas(&self, output: &Output) -> Option<OutputAreas> {
        let full = self.space.output_geometry(output)?;
        let mut zone = layer_map_for_output(output).non_exclusive_zone();
        zone.loc += full.loc;
        Some(OutputAreas { zone, full })
    }

    /// The area `mode` places `surface` in on its window's output.
    pub(crate) fn placement_area_of(
        &self,
        surface: &WlSurface,
        mode: WindowMode,
    ) -> Option<Rectangle<i32, Logical>> {
        Some(self.output_areas(&self.output_of(surface)?)?.for_mode(mode))
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

    /// The output `surface`'s window is on; for one not yet placed, its
    /// nearest placed ancestor's, else the first output.
    pub(crate) fn output_of(&self, surface: &WlSurface) -> Option<Output> {
        std::iter::successors(Some(surface.clone()), |s| self.parent_of(s))
            .find_map(|s| self.toplevels.get(&s)?.output.clone())
            .or_else(|| self.space.outputs().next().cloned())
    }

    /// Lay out the output `surface`'s window is on.
    pub(crate) fn relayout(&mut self, surface: &WlSurface) {
        if let Some(output) = self.output_of(surface) {
            self.apply_layout(&output);
        }
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

    /// Raise the active group, then size, position and configure `output`'s windows.
    pub fn apply_layout(&mut self, output: &Output) {
        if let Some(focused) = self.active_window.clone() {
            for surface in ancestor_chain(&focused, |s| self.parent_of(s)) {
                if let Some(window) = self.window_for(&surface) {
                    self.space.raise_element(&window, false);
                }
            }
        }

        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|window| {
                window
                    .toplevel()
                    .is_some_and(|t| self.is_on(t.wl_surface(), output))
            })
            .cloned()
            .collect();
        let Some(areas) = self.output_areas(output) else {
            self.schedule_render();
            return;
        };
        for window in &windows {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let surface = toplevel.wl_surface();

            // Dialogs are centered in their area, pinned to its top-left when too big.
            let mode = self.placement_mode(surface, toplevel);
            let area = areas.for_mode(mode);
            let loc = match mode {
                WindowMode::Floating => centered_loc(area, window.geometry()),
                WindowMode::Fullscreen | WindowMode::Maximized => area.loc,
            };

            if let Some(ws) = self.toplevels.get_mut(surface) {
                ws.mode = mode;
                ws.layout_size = window.geometry().size;
            }
            self.set_toplevel_placement(toplevel, area, mode);
            if toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
            self.space.relocate_element(window, loc);
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

    /// The active group's windows mapped on `output`, the ones it shows.
    pub fn visible_surfaces(&self, output: &Output) -> HashSet<WlSurface> {
        let mut visible = self.active_group();
        visible.retain(|s| self.is_on(s, output));
        visible
    }

    fn root_of(&self, surface: &WlSurface) -> WlSurface {
        std::iter::successors(Some(surface.clone()), |s| self.parent_of(s))
            .last()
            .unwrap()
    }

    /// How `output` stacks: whether its shown windows are a fullscreen family,
    /// and whether a prompt has the keyboard focus.
    pub fn stack_mode(&self, output: &Output) -> StackMode {
        let fullscreen = self.active_window.as_ref().is_some_and(|focused| {
            let root = self.root_of(focused);
            self.is_on(&root, output)
                && self
                    .toplevels
                    .get(&root)
                    .is_some_and(|ws| ws.mode == WindowMode::Fullscreen)
        });
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        let prompt_focused = focus.is_some_and(|focus| {
            layer_map_for_output(output)
                .layer_for_surface(&focus, WindowSurfaceType::TOPLEVEL)
                .is_some_and(|layer| role_slot(layer) == Some(Slot::Prompt))
        });
        StackMode {
            fullscreen,
            prompt_focused,
        }
    }

    /// Whether `surface` is a toplevel mapped on `output`.
    fn is_on(&self, surface: &WlSurface, output: &Output) -> bool {
        self.toplevels.get(surface).is_some_and(|ws| {
            ws.output.as_ref() == Some(output) && self.space.elements().any(|w| w == &ws.window)
        })
    }

    /// Whether `surface` is a toplevel mapped into the space.
    pub fn is_mapped(&self, surface: &WlSurface) -> bool {
        let Some(ws) = self.toplevels.get(surface) else {
            return false;
        };
        self.space.elements().any(|window| window == &ws.window)
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

/// How an output stacks, beyond its layer surfaces' own roles.
#[derive(Debug, Clone, Copy, Default)]
pub struct StackMode {
    /// The shown windows are a fullscreen family: they go above Top layers.
    pub fullscreen: bool,
    /// A prompt has the keyboard focus: the keyboard goes above prompts.
    pub prompt_focused: bool,
}

/// Where surfaces stack, front to back. Shell roles come from layer-shell
/// namespaces and only count in Top and Overlay; everything else keeps the
/// protocol's layer order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    KeyboardOverPrompt,
    Prompt,
    Chrome,
    Osd,
    Keyboard,
    View,
    Overlay,
    FullscreenWindows,
    Top,
    BottomPopups,
    BackgroundPopups,
    Windows,
    Bottom,
    Background,
}

/// A Top or Overlay layer surface's shell role, from its namespace.
fn role_slot(layer: &LayerSurface) -> Option<Slot> {
    if !matches!(layer.layer(), Layer::Top | Layer::Overlay) {
        return None;
    }
    let namespace = layer.namespace();
    let has = |prefix: &str| namespace.starts_with(prefix);
    if namespace == "osk" {
        Some(Slot::Keyboard)
    } else if has("mx-prompt-") {
        Some(Slot::Prompt)
    } else if has("mx-edge-") || has("mx-bar-") {
        Some(Slot::Chrome)
    } else if has("mx-osd-") {
        Some(Slot::Osd)
    } else if has("mx-view-") {
        Some(Slot::View)
    } else {
        None
    }
}

/// An output's shown surfaces, holding its layer map while in use.
pub struct StackingOrder<'a> {
    space: &'a Space<Window>,
    map: MutexGuard<'a, LayerMap>,
    origin: Point<i32, Logical>,
    visible: &'a HashSet<WlSurface>,
    mode: StackMode,
}

impl<'a> StackingOrder<'a> {
    pub fn new(
        space: &'a Space<Window>,
        output: &'a Output,
        visible: &'a HashSet<WlSurface>,
        mode: StackMode,
    ) -> Self {
        Self {
            space,
            map: layer_map_for_output(output),
            origin: space.output_geometry(output).unwrap_or_default().loc,
            visible,
            mode,
        }
    }

    /// Front to back, by [`Slot`]: the keyboard (above prompts while one has
    /// focus), prompts, edges and bars, OSDs, the keyboard otherwise, views,
    /// other Overlay layers, a fullscreen family, other Top layers,
    /// Bottom/Background popups, other windows, Bottom/Background surfaces.
    /// Within a slot, newest first.
    pub fn iter(&self) -> impl Iterator<Item = Stacked<'_>> {
        let popups = WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE;
        let own = WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE;
        let mut layers = Vec::new();
        for surface in self.map.layers().rev() {
            let Some(geo) = self.map.layer_geometry(surface) else {
                continue;
            };
            let loc = geo.loc + self.origin;
            let mut push = |slot, kind| {
                let entry = StackEntry::Layer(surface, kind);
                layers.push((slot, Stacked { entry, loc }));
            };
            match (surface.layer(), role_slot(surface)) {
                (_, Some(Slot::Keyboard)) if self.mode.prompt_focused => {
                    push(Slot::KeyboardOverPrompt, WindowSurfaceType::ALL)
                }
                (_, Some(slot)) => push(slot, WindowSurfaceType::ALL),
                (Layer::Overlay, None) => push(Slot::Overlay, WindowSurfaceType::ALL),
                (Layer::Top, None) => push(Slot::Top, WindowSurfaceType::ALL),
                (Layer::Bottom, None) => {
                    push(Slot::BottomPopups, popups);
                    push(Slot::Bottom, own);
                }
                (Layer::Background, None) => {
                    push(Slot::BackgroundPopups, popups);
                    push(Slot::Background, own);
                }
            }
        }
        layers.sort_by_key(|(slot, _)| *slot);
        let windows_slot = if self.mode.fullscreen {
            Slot::FullscreenWindows
        } else {
            Slot::Windows
        };
        let below = layers.split_off(layers.partition_point(|(slot, _)| *slot < windows_slot));
        layers
            .into_iter()
            .map(|(_, stacked)| stacked)
            .chain(self.windows())
            .chain(below.into_iter().map(|(_, stacked)| stacked))
    }

    /// The shown windows, topmost first.
    fn windows(&self) -> impl Iterator<Item = Stacked<'_>> {
        self.space.elements().rev().filter_map(move |window| {
            if !self.visible.contains(window.toplevel()?.wl_surface()) {
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
