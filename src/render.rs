use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Physical, Point, Scale};
use smithay::wayland::session_lock::LockSurface;

use std::collections::HashMap;
use std::collections::HashSet;

use crate::drawing::PointerRenderElement;
use crate::layout::{StackEntry, Stacked, StackingOrder};
use crate::state::WindowState;

// Background shown behind normal desktop contents.
pub const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.1, 1.0];
// Behind the lock surface until it attaches a buffer.
pub const CLEAR_COLOR_LOCKED: [f32; 4] = [0.08, 0.08, 0.08, 1.0];

smithay::backend::renderer::element::render_elements! {
    pub OutputElements<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Pointer = PointerRenderElement<R>,
}

/// Concrete element type used by both backends: the compositor always renders
/// with a `GlesRenderer`, and every element ultimately resolves to a
/// `WaylandSurfaceRenderElement`.
pub type Element = OutputElements<GlesRenderer>;

/// Build the full list of render elements for `output` plus the clear color,
/// shared by every backend so the "what to draw" policy lives in one place.
/// Each backend owns "how to present" (winit binds a framebuffer, udev drives a
/// DRM swapchain).
///
/// The renderer draws the list back-to-front, so the front of the list is the
/// top of the stacking order, which comes from [`StackingOrder`] like hit-testing does.
#[allow(clippy::too_many_arguments)]
pub fn output_elements(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    output: &Output,
    is_locked: bool,
    lock_surfaces: &[LockSurface],
    toplevels: &HashMap<WlSurface, WindowState>,
    visible: &HashSet<WlSurface>,
    custom_elements: Vec<Element>,
) -> (Vec<Element>, [f32; 4]) {
    // Queue cursor elements first
    let mut elements = custom_elements;
    let scale = output.current_scale().fractional_scale();
    if is_locked {
        // Only render live lock surfaces; dead ones fall back to the clear color.
        for lock_surface in lock_surfaces.iter().filter(|s| s.alive()) {
            elements.extend(surface_tree(
                renderer,
                lock_surface.wl_surface(),
                (0, 0).into(),
                scale,
            ));
        }
        (elements, CLEAR_COLOR_LOCKED)
    } else {
        let output_loc = space.output_geometry(output).unwrap_or_default().loc;
        let order = StackingOrder::new(space, output, toplevels, visible);
        for Stacked { entry, loc } in order.iter() {
            let loc = (loc - output_loc).to_physical_precise_round(scale);
            match entry {
                StackEntry::Layer(layer, kind) => {
                    if kind.contains(WindowSurfaceType::POPUP) {
                        for (popup, popup_offset) in
                            PopupManager::popups_for_surface(layer.wl_surface())
                        {
                            let offset = (popup_offset - popup.geometry().loc)
                                .to_f64()
                                .to_physical(scale)
                                .to_i32_round();
                            elements.extend(surface_tree(
                                renderer,
                                popup.wl_surface(),
                                loc + offset,
                                scale,
                            ));
                        }
                    }
                    if kind.contains(WindowSurfaceType::TOPLEVEL) {
                        elements.extend(surface_tree(renderer, layer.wl_surface(), loc, scale));
                    }
                }
                StackEntry::Window(window) => elements.extend(window.render_elements::<Element>(
                    renderer,
                    loc,
                    Scale::from(scale),
                    1.0,
                )),
            }
        }
        (elements, CLEAR_COLOR)
    }
}

/// The render elements of `surface` and its subsurfaces at `loc`.
fn surface_tree(
    renderer: &mut GlesRenderer,
    surface: &WlSurface,
    loc: Point<i32, Physical>,
    scale: f64,
) -> Vec<Element> {
    render_elements_from_surface_tree(renderer, surface, loc, scale, 1.0, Kind::Unspecified)
}
