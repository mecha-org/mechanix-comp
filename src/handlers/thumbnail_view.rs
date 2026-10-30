//! Phosh's `phosh_private` v8: thumbnail views of toplevels drawn onto a
//! layer surface. Only `add_thumbnail_view` is served; every other request is
//! a protocol error. A thumbnail whose toplevel handle or layer surface is
//! already gone is inert: never stored, never drawn, its requests no-ops.

use std::collections::HashSet;
use std::hash::Hash;

use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::backend::Backend;
use crate::protocols::phosh_private::phosh_private::{self, PhoshPrivate};
use crate::protocols::phosh_private::phosh_private_thumbnail::{self, PhoshPrivateThumbnail};
use crate::state::State;

const PHOSH_PRIVATE_VERSION: u32 = 8;

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

    fn remove(&mut self, id: &T) {
        self.entries.retain(|(entry, _)| entry != id);
    }

    /// Record a `set_properties`; unknown ids (inert from creation, or dropped
    /// with their layer) are ignored.
    fn set_properties(&mut self, id: &T, x: i32, y: i32, scale: f64, alpha: f32) {
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

    fn shown_toplevels(&self) -> HashSet<S> {
        self.entries
            .iter()
            .map(|(_, thumbnail)| thumbnail)
            .filter(|thumbnail| thumbnail.drawable())
            .map(|thumbnail| thumbnail.toplevel.clone())
            .collect()
    }

    /// Drop thumbnails on layer `surface`; make those of toplevel `surface` inert.
    fn forget_surface(&mut self, surface: &S) {
        self.entries
            .retain(|(_, thumbnail)| &thumbnail.layer != surface);
        for (_, thumbnail) in &mut self.entries {
            if &thumbnail.toplevel == surface {
                thumbnail.toplevel_alive = false;
            }
        }
    }
}

/// The `phosh_private` module state, held by [`State`].
#[derive(Debug, Default)]
pub struct ThumbnailViewState {
    thumbnails: Thumbnails<PhoshPrivateThumbnail, WlSurface>,
}

impl ThumbnailViewState {
    /// Advertise `phosh_private` v8.
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<PhoshPrivate, PhoshPrivateUdata>,
        D: Dispatch<PhoshPrivate, PhoshPrivateUdata>,
        D: Dispatch<PhoshPrivateThumbnail, ThumbnailUdata>,
        D: 'static,
    {
        dh.create_global::<D, PhoshPrivate, _>(PHOSH_PRIVATE_VERSION, PhoshPrivateUdata);
        Self::default()
    }

    /// Apply every pending `set_properties` of thumbnails on `layer`.
    pub fn on_layer_commit(&mut self, layer: &WlSurface) {
        self.thumbnails.on_layer_commit(layer);
    }

    /// Thumbnails to draw on `layer`, in creation order.
    #[allow(dead_code, reason = "consumed by the layer-surface renderer")]
    pub fn for_layer<'a>(
        &'a self,
        layer: &'a WlSurface,
    ) -> impl Iterator<Item = &'a Thumbnail> + 'a {
        self.thumbnails.for_layer(layer)
    }

    /// Toplevels shown through at least one drawable thumbnail.
    #[allow(dead_code, reason = "consumed by toplevel liveness checks")]
    pub fn shown_toplevels(&self) -> HashSet<WlSurface> {
        self.thumbnails.shown_toplevels()
    }

    /// `s` is gone: drop thumbnails on it as a layer; those of it as a
    /// toplevel stay inert until the client destroys them.
    pub fn forget_surface(&mut self, s: &WlSurface) {
        self.thumbnails.forget_surface(s);
    }
}

/// Marker so the `phosh_private` global and resources dispatch here.
pub struct PhoshPrivateUdata;

/// Marker for `phosh_private_thumbnail` resources.
pub struct ThumbnailUdata;

/// Objects created by unsupported requests; their requests are ignored.
pub struct UnsupportedUdata;

impl<BackendData: Backend + 'static> GlobalDispatch2<PhoshPrivate, State<BackendData>>
    for PhoshPrivateUdata
{
    fn can_view(&self, client: &Client) -> bool {
        crate::state::client_is_unrestricted(client)
    }

    fn bind(
        &self,
        _state: &mut State<BackendData>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<PhoshPrivate>,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        data_init.init(resource, PhoshPrivateUdata);
    }
}

impl<BackendData: Backend + 'static> Dispatch2<PhoshPrivate, State<BackendData>>
    for PhoshPrivateUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &PhoshPrivate,
        request: phosh_private::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        let unsupported = match request {
            phosh_private::Request::AddThumbnailView {
                id,
                toplevel,
                layer_surface,
            } => {
                let thumbnail = data_init.init(id, ThumbnailUdata);
                let toplevel = state.foreign_toplevel.surface_for_wlr_handle(&toplevel);
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
                return;
            }
            phosh_private::Request::RotateDisplay { .. } => "rotate_display",
            phosh_private::Request::GetXdgSwitcher { id } => {
                data_init.init(id, UnsupportedUdata);
                "get_xdg_switcher"
            }
            phosh_private::Request::GetThumbnail { id, .. } => {
                data_init.init(id, UnsupportedUdata);
                "get_thumbnail"
            }
            phosh_private::Request::GetKeyboardEvent { id } => {
                data_init.init(id, UnsupportedUdata);
                "get_keyboard_event"
            }
            phosh_private::Request::GetStartupTracker { id } => {
                data_init.init(id, UnsupportedUdata);
                "get_startup_tracker"
            }
            phosh_private::Request::SetShellState { .. } => "set_shell_state",
        };
        resource.post_error(
            phosh_private::Error::InvalidArgument,
            format!("{unsupported} is not supported"),
        );
    }
}

impl<BackendData: Backend + 'static> Dispatch2<PhoshPrivateThumbnail, State<BackendData>>
    for ThumbnailUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &PhoshPrivateThumbnail,
        request: phosh_private_thumbnail::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            phosh_private_thumbnail::Request::SetProperties { x, y, scale, alpha } => {
                state
                    .thumbnail_view
                    .thumbnails
                    .set_properties(resource, x, y, scale, alpha as f32);
            }
            phosh_private_thumbnail::Request::Destroy => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut State<BackendData>,
        _client: ClientId,
        resource: &PhoshPrivateThumbnail,
    ) {
        state.thumbnail_view.thumbnails.remove(resource);
    }
}

impl<I: Resource, BackendData: Backend + 'static> Dispatch2<I, State<BackendData>>
    for UnsupportedUdata
{
    fn request(
        &self,
        _state: &mut State<BackendData>,
        _client: &Client,
        _resource: &I,
        _request: I::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
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
    fn inert_handle_makes_inert_thumbnail() {
        // `add_thumbnail_view` with a closed handle or dead layer surface
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

    #[test]
    fn add_thumbnail_view_opcode_matches_upstream() {
        let xml = include_str!("../../protocols/phosh-private.xml");
        let start = xml
            .find(r#"<interface name="phosh_private" "#)
            .expect("phosh_private interface");
        let end = start + xml[start..].find("</interface>").expect("interface end");
        let index = xml[start..end]
            .split("<request ")
            .skip(1)
            .position(|request| request.starts_with(r#"name="add_thumbnail_view""#))
            .expect("add_thumbnail_view request");
        assert_eq!(
            usize::from(phosh_private::REQ_ADD_THUMBNAIL_VIEW_OPCODE),
            index
        );
    }
}
