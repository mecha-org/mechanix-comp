use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};
use smithay::wayland::session_lock::LockSurface;

use std::collections::HashMap;
use std::collections::HashSet;

use crate::drawing::PointerRenderElement;
use crate::handlers::thumbnail_view::{Thumbnail, ThumbnailViewState};
use crate::layout::{StackEntry, StackMode, Stacked, StackingOrder};
use crate::state::WindowState;

// Background shown behind normal desktop contents.
pub const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.1, 1.0];
// Behind the lock surface until it attaches a buffer.
pub const CLEAR_COLOR_LOCKED: [f32; 4] = [0.08, 0.08, 0.08, 1.0];

smithay::backend::renderer::element::render_elements! {
    pub OutputElements<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Pointer = PointerRenderElement<R>,
    Thumbnail = ThumbnailElement<R>,
}

/// A toplevel's surface tree drawn into a layer surface by `phosh_private`:
/// scaled, moved to its place in the layer, and clipped to the layer.
pub type ThumbnailElement<R> =
    CropRenderElement<RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>>;

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
    stack_mode: StackMode,
    thumbnails: &ThumbnailViewState,
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
        let order = StackingOrder::new(space, output, visible, stack_mode);
        for Stacked { entry, loc } in order.iter() {
            let loc_logical = loc;
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
                        // Thumbnails sit above the layer's own buffer.
                        let mut rect = layer.geometry();
                        rect.loc = loc_logical - output_loc;
                        for thumbnail in thumbnails.for_layer(layer.wl_surface()) {
                            if let Some(ws) = toplevels.get(&thumbnail.toplevel) {
                                elements.extend(thumbnail_elements(
                                    renderer, &ws.window, thumbnail, rect, scale,
                                ));
                            }
                        }
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

/// Where a thumbnail lands on the output, and the rect it is clipped to, both
/// in output-local physical pixels. `layer` is the layer surface's
/// output-local logical rect, `window` the toplevel's logical size, and
/// `(x, y, scale)` the thumbnail's layer-local placement.
fn thumbnail_geometry(
    layer: Rectangle<i32, Logical>,
    window: Size<i32, Logical>,
    (x, y, scale): (i32, i32, f64),
    output_scale: f64,
) -> (Rectangle<i32, Physical>, Rectangle<i32, Physical>) {
    let loc = (layer.loc + Point::from((x, y))).to_physical_precise_round(output_scale);
    let size = window
        .to_f64()
        .upscale(scale)
        .to_physical(output_scale)
        .to_i32_round();
    let crop = layer.to_physical_precise_round(output_scale);
    (Rectangle::new(loc, size), crop)
}

/// The elements of `window` (surface tree, subsurfaces and popups) drawn as
/// `thumbnail` into the layer surface at output-local logical `layer`.
/// `(x, y)` addresses the window geometry's top-left, so CSD shadows fall
/// outside it.
fn thumbnail_elements(
    renderer: &mut GlesRenderer,
    window: &Window,
    thumbnail: &Thumbnail,
    layer: Rectangle<i32, Logical>,
    output_scale: f64,
) -> Vec<Element> {
    let geometry = window.geometry();
    if geometry.is_empty() {
        return Vec::new();
    }
    let (dst, crop) = thumbnail_geometry(
        layer,
        geometry.size,
        (thumbnail.x, thumbnail.y, thumbnail.scale),
        output_scale,
    );
    // Lay the window out with its geometry's top-left at the origin, so
    // rescaling around the origin and relocating to `dst` places it.
    let origin = Point::<i32, Physical>::default();
    let location = origin - geometry.loc.to_physical_precise_round(output_scale);
    window
        .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            location,
            Scale::from(output_scale),
            thumbnail.alpha,
        )
        .into_iter()
        .filter_map(|element| {
            let element = RescaleRenderElement::from_element(element, origin, thumbnail.scale);
            let element = RelocateRenderElement::from_element(element, dst.loc, Relocate::Relative);
            CropRenderElement::from_element(element, output_scale, crop)
        })
        .map(OutputElements::Thumbnail)
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_elements_are_cropped_and_scaled() {
        let layer = Rectangle::new((0, 0).into(), (544, 1209).into());
        let window = Size::from((544, 1209));

        let (dst, crop) = thumbnail_geometry(layer, window, (40, 100, 0.5), 2.25);
        assert_eq!(dst, Rectangle::new((90, 225).into(), (612, 1360).into()));
        assert_eq!(crop, Rectangle::new((0, 0).into(), (1224, 2720).into()));

        // Placed past the right edge: what is drawn ends at the layer's edge.
        let (dst, crop) = thumbnail_geometry(layer, window, (500, 100, 0.5), 2.25);
        let drawn = dst.intersection(crop).unwrap();
        assert!(dst.loc.x + dst.size.w > crop.loc.x + crop.size.w);
        assert_eq!(drawn.loc.x + drawn.size.w, crop.loc.x + crop.size.w);
        assert_eq!(drawn.loc.x, 1125);
    }

    #[test]
    fn thumbnail_geometry_follows_the_layer_position() {
        let layer = Rectangle::new((10, 20).into(), (100, 200).into());
        let (dst, crop) = thumbnail_geometry(layer, Size::from((100, 200)), (4, 8, 1.0), 2.0);
        assert_eq!(dst, Rectangle::new((28, 56).into(), (200, 400).into()));
        assert_eq!(crop, Rectangle::new((20, 40).into(), (200, 400).into()));
    }
}
