//! `mx_thumbnail_view_v1`: thumbnails of toplevels drawn onto a shell's
//! layer surfaces. A thumbnail whose toplevel handle or layer surface is
//! already gone is inert: never stored, never drawn, its requests no-ops.

#[cfg(test)]
use std::collections::HashSet;
use std::hash::Hash;

use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::backend::Backend;
use crate::protocols::thumbnail_view::mx_thumbnail_view_manager_v1::{
    self, MxThumbnailViewManagerV1,
};
use crate::protocols::thumbnail_view::mx_thumbnail_view_v1::{self, MxThumbnailViewV1};
use crate::state::State;

const VERSION: u32 = 1;

/// One thumbnail: `toplevel`'s contents drawn onto `layer` with the last
/// applied `set_properties`.
#[derive(Debug)]
pub struct Thumbnail<S = WlSurface> {
    pub toplevel: S,
    pub layer: S,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub alpha: f32,
    /// `set_properties` waiting for the next commit of `layer`.
    pending: Option<(i32, i32, f64, f32)>,
    /// Set by the first applied `set_properties`; never drawn before that.
    applied: bool,
    /// Cleared when the toplevel goes; the thumbnail then stays inert until
    /// the client destroys it.
    toplevel_alive: bool,
}

impl<S> Thumbnail<S> {
    fn drawable(&self) -> bool {
        self.applied && self.toplevel_alive
    }
}

/// The thumbnail bookkeeping, keyed by thumbnail id `T` over surfaces `S` so
/// it can be exercised without a Wayland display. Kept in creation order,
/// which is the draw order.
#[derive(Debug)]
struct Thumbnails<T, S> {
    entries: Vec<(T, Thumbnail<S>)>,
}

impl<T, S> Default for Thumbnails<T, S> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T: PartialEq, S: Eq + Hash + Clone> Thumbnails<T, S> {
    fn add(&mut self, id: T, toplevel: S, layer: S) {
        self.entries.push((
            id,
            Thumbnail {
                toplevel,
                layer,
                x: 0,
                y: 0,
                scale: 1.0,
                alpha: 1.0,
                pending: None,
                applied: false,
                toplevel_alive: true,
            },
        ));
    }

    /// Drop thumbnail `id`; the layer it was drawn on, if it was drawn.
    fn remove(&mut self, id: &T) -> Option<S> {
        let index = self.entries.iter().position(|(entry, _)| entry == id)?;
        let (_, thumbnail) = self.entries.remove(index);
        thumbnail.drawable().then_some(thumbnail.layer)
    }

    /// Record a `set_properties`; unknown ids (inert from creation, or dropped
    /// with their layer) are ignored. A non-finite or non-positive scale, or a
    /// NaN alpha, is ignored (the previous pending stays); alpha is clamped to
    /// [0, 1].
    fn set_properties(&mut self, id: &T, x: i32, y: i32, scale: f64, alpha: f32) {
        if !(scale.is_finite() && scale > 0.0) || alpha.is_nan() {
            return;
        }
        let alpha = alpha.clamp(0.0, 1.0);
        if let Some((_, thumbnail)) = self.entries.iter_mut().find(|(entry, _)| entry == id) {
            thumbnail.pending = Some((x, y, scale, alpha));
        }
    }

    fn on_layer_commit(&mut self, layer: &S) {
        for (_, thumbnail) in &mut self.entries {
            if &thumbnail.layer != layer {
                continue;
            }
            if let Some((x, y, scale, alpha)) = thumbnail.pending.take() {
                thumbnail.x = x;
                thumbnail.y = y;
                thumbnail.scale = scale;
                thumbnail.alpha = alpha;
                thumbnail.applied = true;
            }
        }
    }

    fn for_layer<'a>(&'a self, layer: &'a S) -> impl Iterator<Item = &'a Thumbnail<S>> + 'a {
        self.entries
            .iter()
            .map(|(_, thumbnail)| thumbnail)
            .filter(move |thumbnail| &thumbnail.layer == layer && thumbnail.drawable())
    }

    #[cfg(test)]
    fn shown_toplevels(&self) -> HashSet<S> {
        self.entries
            .iter()
            .map(|(_, thumbnail)| thumbnail)
            .filter(|thumbnail| thumbnail.drawable())
            .map(|thumbnail| thumbnail.toplevel.clone())
            .collect()
    }

    /// Drop thumbnails on layer `surface`; make those of toplevel `surface`
    /// inert. Returns the layers that lost a drawn thumbnail.
    fn forget_surface(&mut self, surface: &S) -> Vec<S> {
        let mut redraw = Vec::new();
        let mut note = |layer: &S| {
            if !redraw.contains(layer) {
                redraw.push(layer.clone());
            }
        };
        self.entries.retain(|(_, thumbnail)| {
            let dropped = &thumbnail.layer == surface;
            if dropped && thumbnail.drawable() {
                note(&thumbnail.layer);
            }
            !dropped
        });
        for (_, thumbnail) in &mut self.entries {
            if &thumbnail.toplevel == surface {
                if thumbnail.drawable() {
                    note(&thumbnail.layer);
                }
                thumbnail.toplevel_alive = false;
            }
        }
        redraw
    }
}

/// The thumbnail views, held by [`State`].
#[derive(Debug, Default)]
pub struct ThumbnailViewState {
    thumbnails: Thumbnails<MxThumbnailViewV1, WlSurface>,
}

impl ThumbnailViewState {
    /// Advertise `mx_thumbnail_view_manager_v1`, to unrestricted clients
    /// only.
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<MxThumbnailViewManagerV1, ThumbnailUdata> + 'static,
    {
        dh.create_global::<D, MxThumbnailViewManagerV1, _>(VERSION, ThumbnailUdata);
        Self::default()
    }

    /// Apply every pending `set_properties` of thumbnails on `layer`.
    pub fn on_layer_commit(&mut self, layer: &WlSurface) {
        self.thumbnails.on_layer_commit(layer);
    }

    /// Thumbnails to draw on `layer`, in creation order.
    pub fn for_layer<'a>(
        &'a self,
        layer: &'a WlSurface,
    ) -> impl Iterator<Item = &'a Thumbnail> + 'a {
        // Belt and braces: `forget_surface` already makes dead toplevels inert.
        self.thumbnails
            .for_layer(layer)
            .filter(|thumbnail| thumbnail.toplevel.is_alive())
    }

    /// `s` is gone: drop thumbnails on it as a layer; those of it as a
    /// toplevel stay inert until the client destroys them. Returns the layer
    /// surfaces that lost a drawn thumbnail and need a redraw.
    #[must_use = "redraw the returned layers' outputs"]
    pub fn forget_surface(&mut self, s: &WlSurface) -> Vec<WlSurface> {
        self.thumbnails.forget_surface(s)
    }
}

/// Marker so the manager global and the thumbnails dispatch here.
pub struct ThumbnailUdata;

impl<BackendData: Backend + 'static> GlobalDispatch2<MxThumbnailViewManagerV1, State<BackendData>>
    for ThumbnailUdata
{
    fn can_view(&self, client: &Client) -> bool {
        crate::state::client_is_unrestricted(client)
    }

    fn bind(
        &self,
        _state: &mut State<BackendData>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<MxThumbnailViewManagerV1>,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        data_init.init(resource, ThumbnailUdata);
    }
}

impl<BackendData: Backend + 'static> Dispatch2<MxThumbnailViewManagerV1, State<BackendData>>
    for ThumbnailUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        _resource: &MxThumbnailViewManagerV1,
        request: mx_thumbnail_view_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            mx_thumbnail_view_manager_v1::Request::Destroy => {}
            mx_thumbnail_view_manager_v1::Request::GetThumbnail {
                id,
                toplevel,
                layer_surface,
            } => {
                let thumbnail = data_init.init(id, ThumbnailUdata);
                // The foreign-toplevel map is pruned only on the next refresh,
                // so a handle can still resolve to a window already forgotten.
                let toplevel = state
                    .foreign_toplevel
                    .surface_for_wlr_handle(&toplevel)
                    .filter(|surface| surface.is_alive() && state.toplevels.contains_key(surface));
                let layer = state
                    .layer_shell_state
                    .layer_surfaces()
                    .find(|layer| layer.shell_surface() == &layer_surface)
                    .map(|layer| layer.wl_surface().clone());
                // A closed handle or dead layer surface can race with this
                // request, so it is not an error: the thumbnail stays out of
                // the store, inert, and its requests are no-ops.
                if let (Some(toplevel), Some(layer)) = (toplevel, layer) {
                    state
                        .thumbnail_view
                        .thumbnails
                        .add(thumbnail, toplevel, layer);
                }
            }
        }
    }
}

impl<BackendData: Backend + 'static> Dispatch2<MxThumbnailViewV1, State<BackendData>>
    for ThumbnailUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &MxThumbnailViewV1,
        request: mx_thumbnail_view_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            mx_thumbnail_view_v1::Request::SetProperties { x, y, scale, alpha } => {
                state
                    .thumbnail_view
                    .thumbnails
                    .set_properties(resource, x, y, scale, alpha as f32);
            }
            mx_thumbnail_view_v1::Request::Destroy => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut State<BackendData>,
        _client: ClientId,
        resource: &MxThumbnailViewV1,
    ) {
        if let Some(layer) = state.thumbnail_view.thumbnails.remove(resource) {
            state.schedule_render_for_layers(&[layer]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYER: u32 = 1;
    const OTHER_LAYER: u32 = 2;
    const TOPLEVEL: u32 = 10;

    fn one_thumbnail() -> Thumbnails<u32, u32> {
        let mut thumbnails = Thumbnails::default();
        thumbnails.add(100, TOPLEVEL, LAYER);
        thumbnails
    }

    fn drawn(thumbnails: &Thumbnails<u32, u32>, layer: u32) -> Vec<(i32, i32, f64, f32)> {
        thumbnails
            .for_layer(&layer)
            .map(|t| (t.x, t.y, t.scale, t.alpha))
            .collect()
    }

    #[test]
    fn set_properties_waits_for_commit() {
        let mut thumbnails = one_thumbnail();
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        assert!(drawn(&thumbnails, LAYER).is_empty());
        assert!(thumbnails.shown_toplevels().is_empty());

        thumbnails.on_layer_commit(&LAYER);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(10, 20, 0.5, 0.8)]);
        assert_eq!(thumbnails.shown_toplevels(), HashSet::from([TOPLEVEL]));
    }

    #[test]
    fn commit_of_other_layer_does_not_apply() {
        let mut thumbnails = one_thumbnail();
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        thumbnails.on_layer_commit(&OTHER_LAYER);
        assert!(drawn(&thumbnails, LAYER).is_empty());
        assert!(drawn(&thumbnails, OTHER_LAYER).is_empty());
    }

    #[test]
    fn layer_death_drops_thumbnails() {
        let mut thumbnails = one_thumbnail();
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        thumbnails.on_layer_commit(&LAYER);

        thumbnails.forget_surface(&LAYER);
        assert!(drawn(&thumbnails, LAYER).is_empty());
        assert!(thumbnails.shown_toplevels().is_empty());
        // A pending update for the dropped thumbnail is a harmless no-op.
        thumbnails.set_properties(&100, 1, 2, 1.0, 1.0);
        thumbnails.on_layer_commit(&LAYER);
        assert!(drawn(&thumbnails, LAYER).is_empty());
    }

    #[test]
    fn toplevel_death_stops_drawing() {
        let mut thumbnails = one_thumbnail();
        thumbnails.add(101, 11, LAYER);
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        thumbnails.set_properties(&101, 30, 40, 0.25, 1.0);
        thumbnails.on_layer_commit(&LAYER);

        thumbnails.forget_surface(&TOPLEVEL);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(30, 40, 0.25, 1.0)]);
        assert_eq!(thumbnails.shown_toplevels(), HashSet::from([11]));

        thumbnails.set_properties(&100, 0, 0, 1.0, 1.0);
        thumbnails.on_layer_commit(&LAYER);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(30, 40, 0.25, 1.0)]);
        thumbnails.remove(&100);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(30, 40, 0.25, 1.0)]);
    }

    #[test]
    fn removing_drawn_thumbnails_reports_layers_to_redraw() {
        let mut thumbnails = one_thumbnail();
        thumbnails.add(101, 11, OTHER_LAYER);
        // Not yet applied: nothing was drawn, nothing to redraw.
        assert_eq!(thumbnails.remove(&101), None);
        thumbnails.add(102, 11, OTHER_LAYER);
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        thumbnails.set_properties(&102, 10, 20, 0.5, 0.8);
        thumbnails.on_layer_commit(&LAYER);
        thumbnails.on_layer_commit(&OTHER_LAYER);

        assert_eq!(thumbnails.forget_surface(&TOPLEVEL), vec![LAYER]);
        // Already inert: no second redraw.
        assert_eq!(thumbnails.forget_surface(&TOPLEVEL), Vec::<u32>::new());
        assert_eq!(thumbnails.remove(&100), None);
        assert_eq!(thumbnails.remove(&102), Some(OTHER_LAYER));

        let mut thumbnails = one_thumbnail();
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        thumbnails.on_layer_commit(&LAYER);
        assert_eq!(thumbnails.forget_surface(&LAYER), vec![LAYER]);
    }

    #[test]
    fn invalid_properties_are_sanitised() {
        let mut thumbnails = one_thumbnail();
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            thumbnails.set_properties(&100, 1, 2, scale, 1.0);
        }
        thumbnails.set_properties(&100, 3, 4, 1.0, f32::NAN);
        thumbnails.on_layer_commit(&LAYER);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(10, 20, 0.5, 0.8)]);

        thumbnails.set_properties(&100, 0, 0, 1.0, 1.5);
        thumbnails.on_layer_commit(&LAYER);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(0, 0, 1.0, 1.0)]);
        thumbnails.set_properties(&100, 0, 0, 1.0, -0.5);
        thumbnails.on_layer_commit(&LAYER);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(0, 0, 1.0, 0.0)]);
    }

    #[test]
    fn inert_handle_makes_inert_thumbnail() {
        // `get_thumbnail` with a closed handle or dead layer surface
        // stores nothing; the thumbnail's later requests must be harmless.
        let mut thumbnails = one_thumbnail();
        thumbnails.set_properties(&100, 10, 20, 0.5, 0.8);
        thumbnails.set_properties(&200, 1, 2, 1.0, 1.0);
        thumbnails.on_layer_commit(&LAYER);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(10, 20, 0.5, 0.8)]);
        assert_eq!(thumbnails.shown_toplevels(), HashSet::from([TOPLEVEL]));

        thumbnails.remove(&200);
        assert_eq!(drawn(&thumbnails, LAYER), vec![(10, 20, 0.5, 0.8)]);
    }
}
