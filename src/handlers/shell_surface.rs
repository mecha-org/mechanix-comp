//! `mx_shell_surface_manager_v1` and `mx_shell_surface_v1`
//! (`protocols/mx-shell-surface-v1.xml`).
//!
//! Lets privileged shell clients give a `zwlr_layer_surface_v1` shell state:
//! a [`ShellRole`], chosen before its initial configure, and whether it is
//! hidden. Both are double-buffered on the surface ([`ShellState`]) and
//! apply on its commit. [`crate::layout`] reads [`shell_role`] to place the
//! layer surface in the corresponding stacking [`Slot`](crate::layout::Slot),
//! [`crate::input`] uses it to recognize edge gesture surfaces, and every
//! place that shows a layer surface, routes input or focus to it or reserves
//! its zone skips one that is [`hidden`].

use std::sync::Mutex;

use smithay::{
    desktop::LayerSurface,
    reexports::{
        wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, WEnum,
            backend::{ClientId, GlobalId},
            protocol::wl_surface::WlSurface,
        },
    },
    wayland::{
        Dispatch2, GlobalDispatch2,
        compositor::{Cacheable, with_states},
        shell::wlr_layer::LayerSurfaceData,
    },
};

use crate::{
    backend::Backend,
    protocols::shell_surface::{
        mx_shell_surface_manager_v1::{self, Error, MxShellSurfaceManagerV1},
        mx_shell_surface_v1::{self, MxShellSurfaceV1},
    },
    state::{State, client_is_unrestricted},
};

pub use crate::protocols::shell_surface::mx_shell_surface_manager_v1::Role as ShellRole;

const VERSION: u32 = 1;

/// A layer surface's shell state, double-buffered on its `wl_surface`:
/// pending as `mx_shell_surface_v1` sets it, current from the next commit.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShellState {
    /// `None` for no role, `none` included.
    pub role: Option<ShellRole>,
    pub hidden: bool,
}

impl Cacheable for ShellState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Whether a layer surface has a shell surface now: at most one at a time.
#[derive(Default)]
struct Constructed(Mutex<bool>);

/// The current shell state of the layer surface `surface`.
fn state_of(surface: &WlSurface) -> ShellState {
    with_states(surface, |states| {
        *states.cached_state.get::<ShellState>().current()
    })
}

/// Returns the [`ShellRole`] `layer` has now, if any.
pub fn shell_role(layer: &LayerSurface) -> Option<ShellRole> {
    state_of(layer.wl_surface()).role
}

/// Whether `layer` is hidden now: not shown, no input or focus, and no
/// reserved zone.
pub fn hidden(layer: &LayerSurface) -> bool {
    state_of(layer.wl_surface()).hidden
}

/// Marker so the `mx_shell_surface_manager_v1` global and resources dispatch here.
pub struct ShellSurfaceManagerUdata;

/// The layer surface a `mx_shell_surface_v1` extends; `None` for one made
/// inert because its layer surface was already gone.
pub struct ShellSurfaceUdata(Option<WlSurface>);

/// Registers the `mx_shell_surface_manager_v1` global, restricted to
/// unrestricted (host/shell) clients.
pub fn init_shell_surface_manager<D>(dh: &DisplayHandle) -> GlobalId
where
    D: smithay::reexports::wayland_server::GlobalDispatch<
            MxShellSurfaceManagerV1,
            ShellSurfaceManagerUdata,
        > + 'static,
{
    dh.create_global::<D, MxShellSurfaceManagerV1, _>(VERSION, ShellSurfaceManagerUdata)
}

impl<BackendData: Backend + 'static> GlobalDispatch2<MxShellSurfaceManagerV1, State<BackendData>>
    for ShellSurfaceManagerUdata
{
    fn can_view(&self, client: &Client) -> bool {
        client_is_unrestricted(client)
    }

    fn bind(
        &self,
        _state: &mut State<BackendData>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<MxShellSurfaceManagerV1>,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        data_init.init(resource, ShellSurfaceManagerUdata);
    }
}

impl<BackendData: Backend + 'static> Dispatch2<MxShellSurfaceManagerV1, State<BackendData>>
    for ShellSurfaceManagerUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &MxShellSurfaceManagerV1,
        request: mx_shell_surface_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            mx_shell_surface_manager_v1::Request::Destroy => {}
            mx_shell_surface_manager_v1::Request::GetShellSurface {
                id,
                layer_surface,
                role,
            } => {
                let WEnum::Value(role) = role else {
                    resource.post_error(Error::InvalidRole, "invalid shell surface role");
                    return;
                };
                let role = (role != ShellRole::None).then_some(role);
                match construct(state, &layer_surface, role) {
                    Ok(surface) => {
                        data_init.init(id, ShellSurfaceUdata(surface));
                    }
                    Err(Error::AlreadyConfigured) => resource.post_error(
                        Error::AlreadyConfigured,
                        "layer_surface was already configured",
                    ),
                    Err(err) => {
                        resource.post_error(err, "layer_surface already has a shell surface")
                    }
                }
            }
        }
    }
}

/// Give the layer surface its shell state, pending its next commit. `None`
/// when its surface is gone: the shell surface is then inert.
fn construct<BackendData: Backend>(
    state: &mut State<BackendData>,
    layer_surface: &ZwlrLayerSurfaceV1,
    role: Option<ShellRole>,
) -> Result<Option<WlSurface>, Error> {
    // On the surface, not the mapped layer: a surface that is not on an
    // output yet keeps its state for when it is.
    let Some(layer) = state
        .layer_shell_state
        .layer_surfaces()
        .find(|l| l.shell_surface() == layer_surface)
    else {
        return Ok(None);
    };
    let surface = layer.wl_surface().clone();
    with_states(&surface, |states| {
        let configured = states
            .data_map
            .get::<LayerSurfaceData>()
            .is_some_and(|d| d.lock().unwrap().initial_configure_sent);
        if configured {
            return Err(Error::AlreadyConfigured);
        }
        states
            .data_map
            .insert_if_missing_threadsafe(Constructed::default);
        let mut constructed = states
            .data_map
            .get::<Constructed>()
            .unwrap()
            .0
            .lock()
            .unwrap();
        if *constructed {
            return Err(Error::AlreadyConstructed);
        }
        *constructed = true;
        *states.cached_state.get::<ShellState>().pending() = ShellState {
            role,
            hidden: false,
        };
        Ok(())
    })?;
    Ok(Some(surface))
}

impl<BackendData: Backend + 'static> Dispatch2<MxShellSurfaceV1, State<BackendData>>
    for ShellSurfaceUdata
{
    fn request(
        &self,
        _state: &mut State<BackendData>,
        _client: &Client,
        _resource: &MxShellSurfaceV1,
        request: mx_shell_surface_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            mx_shell_surface_v1::Request::SetHidden { hidden } => {
                let Some(surface) = self.0.as_ref().filter(|s| s.is_alive()) else {
                    return;
                };
                with_states(surface, |states| {
                    states.cached_state.get::<ShellState>().pending().hidden = hidden != 0;
                });
            }
            mx_shell_surface_v1::Request::Destroy => {}
        }
    }

    /// From the next commit, shown and without a role; a new shell surface
    /// may then be made, before the first configure only.
    fn destroyed(
        &self,
        _state: &mut State<BackendData>,
        _client: ClientId,
        _resource: &MxShellSurfaceV1,
    ) {
        let Some(surface) = self.0.as_ref().filter(|s| s.is_alive()) else {
            return;
        };
        with_states(surface, |states| {
            *states.cached_state.get::<ShellState>().pending() = ShellState::default();
            if let Some(constructed) = states.data_map.get::<Constructed>() {
                *constructed.0.lock().unwrap() = false;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_starts_shown_without_a_role() {
        let state = ShellState::default();
        assert_eq!(state.role, None);
        assert!(!state.hidden);
    }
}
