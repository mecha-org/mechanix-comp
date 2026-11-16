//! `mx_shell_surface_manager_v1` (`protocols/mx-shell-surface-v1.xml`).
//!
//! Lets privileged shell clients assign a [`ShellRole`] to a
//! `zwlr_layer_surface_v1` before its initial configure. [`crate::layout`]
//! reads [`shell_role`] to place the layer surface in the corresponding
//! stacking [`Slot`](crate::layout::Slot) and [`crate::input`] uses it to
//! recognize edge gesture surfaces.

use smithay::{
    desktop::LayerSurface,
    reexports::{
        wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        wayland_server::{Client, DataInit, DisplayHandle, New, Resource, WEnum, backend::GlobalId},
    },
    wayland::{
        Dispatch2, GlobalDispatch2, compositor::with_states, shell::wlr_layer::LayerSurfaceData,
    },
};

use crate::{
    backend::Backend,
    protocols::shell_surface::mx_shell_surface_manager_v1::{
        Error, MxShellSurfaceManagerV1, Request,
    },
    state::{State, client_is_unrestricted},
};

pub use crate::protocols::shell_surface::mx_shell_surface_manager_v1::Role as ShellRole;

const VERSION: u32 = 1;

/// Returns the [`ShellRole`] assigned to `layer`, if any.
pub fn shell_role(layer: &LayerSurface) -> Option<ShellRole> {
    with_states(layer.wl_surface(), |states| {
        states.data_map.get::<ShellRole>().copied()
    })
}

/// Marker so the `mx_shell_surface_manager_v1` global and resources dispatch here.
pub struct ShellSurfaceManagerUdata;

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
        request: Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            Request::Destroy => {}
            Request::SetRole {
                layer_surface,
                role,
            } => {
                let WEnum::Value(role) = role else {
                    resource.post_error(Error::InvalidRole, "invalid shell surface role");
                    return;
                };
                if let Err(err) = assign_role(state, &layer_surface, role) {
                    resource.post_error(
                        err,
                        "layer_surface already has a shell role or was already configured",
                    );
                }
            }
        }
    }
}

fn assign_role<BackendData: Backend>(
    state: &mut State<BackendData>,
    layer_surface: &ZwlrLayerSurfaceV1,
    role: ShellRole,
) -> Result<(), Error> {
    // On the surface, not the mapped layer: a surface that is not on an
    // output yet keeps its role for when it is.
    let Some(layer) = state
        .layer_shell_state
        .layer_surfaces()
        .find(|l| l.shell_surface() == layer_surface)
    else {
        // Its surface is gone.
        return Ok(());
    };
    with_states(layer.wl_surface(), |states| {
        let configured = states
            .data_map
            .get::<LayerSurfaceData>()
            .is_some_and(|d| d.lock().unwrap().initial_configure_sent);
        if configured || !states.data_map.insert_if_missing_threadsafe(|| role) {
            return Err(Error::AlreadySet);
        }
        Ok(())
    })
}
