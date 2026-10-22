use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::element::{AsRenderElements, Wrap};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::desktop::space::SpaceRenderElements;
use smithay::desktop::{LayerSurface, PopupManager, Space, Window, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Scale;
use smithay::wayland::session_lock::LockSurface;
use smithay::wayland::shell::wlr_layer::Layer;

use std::collections::HashMap;
use std::collections::HashSet;

use crate::drawing::PointerRenderElement;
use crate::state::{WindowMode, WindowState};

// Background shown behind normal desktop contents.
pub const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.1, 1.0];
// Behind the lock surface until it attaches a buffer.
pub const CLEAR_COLOR_LOCKED: [f32; 4] = [0.08, 0.08, 0.08, 1.0];

smithay::backend::renderer::element::render_elements! {
    pub OutputElements<R, E> where R: ImportAll + ImportMem;
    Space = SpaceRenderElements<R, E>,
    Surface = WaylandSurfaceRenderElement<R>,
    Pointer = PointerRenderElement<R>,
}

/// Concrete element type used by both backends: the compositor always renders
/// with a `GlesRenderer`, and every element ultimately resolves to a
/// `WaylandSurfaceRenderElement`.
pub type Element = OutputElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Build the full list of render elements for `output` plus the clear color,
/// shared by every backend so the "what to draw" policy lives in one place.
/// Each backend owns "how to present" (winit binds a framebuffer, udev drives a
/// DRM swapchain).
///
/// The renderer draws the list back-to-front, so the front of the list is the
/// top of the stack. The layer-shell stack is built by hand (mirroring the
/// positions smithay's `space_render_elements` would use) so that popups of
/// Background/Bottom layers render *above* the windows, while the bars
/// themselves stay below them.
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
    if is_locked {
        // Only render live lock surfaces; dead ones fall back to the clear color.
        for lock_surface in lock_surfaces.iter().filter(|s| s.alive()) {
            elements.extend(
                render_elements_from_surface_tree::<
                    GlesRenderer,
                    WaylandSurfaceRenderElement<GlesRenderer>,
                >(
                    renderer,
                    lock_surface.wl_surface(),
                    (0, 0),
                    output.current_scale().fractional_scale(),
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(OutputElements::Surface),
            );
        }
        (elements, CLEAR_COLOR_LOCKED)
    } else {
        let scale = output.current_scale().fractional_scale();

        // Split the layer map exactly like smithay's `space_render_elements`:
        // upper = Top/Overlay, lower = Background/Bottom (insertion order,
        // reversed). The guard must outlive the borrowed layer surfaces.
        let map = layer_map_for_output(output);
        let (lower, upper): (Vec<&LayerSurface>, Vec<&LayerSurface>) = map
            .layers()
            .rev()
            .partition(|s| matches!(s.layer(), Layer::Background | Layer::Bottom));

        // Fullscreen windows render above the top layer but below Overlay, so
        // Overlay wins the stack and Top stays below.
        let (overlay, top): (Vec<&LayerSurface>, Vec<&LayerSurface>) =
            upper.into_iter().partition(|s| s.layer() == Layer::Overlay);

        // Overlay layers (and their popups) above everything.
        for surface in &overlay {
            let Some(geo) = map.layer_geometry(surface) else {
                continue;
            };
            elements.extend(
                surface
                    .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                        renderer,
                        geo.loc.to_physical_precise_round(scale),
                        Scale::from(scale),
                        1.0,
                    )
                    .into_iter()
                    .map(OutputElements::Surface),
            );
        }

        let windows = |renderer: &mut GlesRenderer, fullscreen: bool| -> Vec<Element> {
            let Some(output_geo) = space.output_geometry(output) else {
                return Vec::new();
            };
            let wanted = |window: &Window| {
                window.toplevel().is_some_and(|toplevel| {
                    let surface = toplevel.wl_surface();
                    visible.contains(surface)
                        && toplevels
                            .get(surface)
                            .is_some_and(|ws| (ws.mode == WindowMode::Fullscreen) == fullscreen)
                })
            };
            space
                .elements()
                .rev()
                .filter(|window| wanted(window))
                .flat_map(|window| {
                    let loc = space.element_location(window).unwrap()
                        - window.geometry().loc
                        - output_geo.loc;
                    window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                        renderer,
                        loc.to_physical_precise_round(scale),
                        Scale::from(scale),
                        1.0,
                    )
                })
                .map(|e| OutputElements::Space(SpaceRenderElements::Element(Wrap::from(e))))
                .collect()
        };

        // Fullscreen windows above the Top layer.
        elements.extend(windows(renderer, true));

        // Top layers (and their popups) above the non-fullscreen windows.
        for surface in &top {
            let Some(geo) = map.layer_geometry(surface) else {
                continue;
            };
            elements.extend(
                surface
                    .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                        renderer,
                        geo.loc.to_physical_precise_round(scale),
                        Scale::from(scale),
                        1.0,
                    )
                    .into_iter()
                    .map(OutputElements::Surface),
            );
        }

        // Background/Bottom layer popups, hoisted above the windows.
        for surface in &lower {
            let Some(geo) = map.layer_geometry(surface) else {
                continue;
            };
            for (popup, popup_offset) in PopupManager::popups_for_surface(surface.wl_surface()) {
                let offset = (popup_offset - popup.geometry().loc)
                    .to_f64()
                    .to_physical(scale)
                    .to_i32_round();
                elements.extend(
                    render_elements_from_surface_tree::<
                        GlesRenderer,
                        WaylandSurfaceRenderElement<GlesRenderer>,
                    >(
                        renderer,
                        popup.wl_surface(),
                        geo.loc.to_physical_precise_round(scale) + offset,
                        Scale::from(scale),
                        1.0,
                        Kind::Unspecified,
                    )
                    .into_iter()
                    .map(OutputElements::Surface),
                );
            }
        }

        // The other windows, below the Top layer.
        elements.extend(windows(renderer, false));

        // Background/Bottom layer surfaces (the bars), below the windows.
        // Their popups were hoisted above the windows already.
        for surface in &lower {
            let Some(geo) = map.layer_geometry(surface) else {
                continue;
            };
            elements.extend(
                render_elements_from_surface_tree::<
                    GlesRenderer,
                    WaylandSurfaceRenderElement<GlesRenderer>,
                >(
                    renderer,
                    surface.wl_surface(),
                    geo.loc.to_physical_precise_round(scale),
                    Scale::from(scale),
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(OutputElements::Surface),
            );
        }
        (elements, CLEAR_COLOR)
    }
}
