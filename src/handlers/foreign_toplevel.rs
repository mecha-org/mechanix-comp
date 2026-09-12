//! Our zwlr-foreign-toplevel-management-v1 + smithay's ext-foreign-toplevel-list-v1, driven by [`State::foreign_toplevel_refresh`].

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::foreign_toplevel_list::v1::server::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::compositor::with_states;
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
};
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::backend::Backend;
use crate::state::{State, WindowMode};

const WLR_MANAGEMENT_VERSION: u32 = 3;

/// Marker so the wlr global and resources dispatch here.
#[derive(Default)]
pub struct ForeignToplevelUdata;

/// What the foreign-toplevel protocols need to know about one toplevel.
#[derive(Debug)]
pub struct ToplevelData {
    title: String,
    app_id: String,
    /// The wlr `state` event payload: maximized/fullscreen/activated flags.
    states: Vec<u8>,
    /// The output the toplevel is currently on, for `output_enter`/`leave`.
    output: Option<Output>,
    /// xdg parent, if any; the wlr `parent` event is sent once handles exist.
    parent: Option<WlSurface>,
    /// Per-client wlr handles, each with the outputs entered so far.
    wlr_management_instances: HashMap<ZwlrForeignToplevelHandleV1, Vec<WlOutput>>,
    /// smithay's ext-list handle; owns all ext instance bookkeeping.
    ext_handle: Option<ForeignToplevelHandle>,
}

impl ToplevelData {
    fn add_wlr_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
    ) -> Option<ZwlrForeignToplevelHandleV1>
    where
        D: Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelUdata> + 'static,
    {
        let Ok(toplevel) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, D>(
            handle,
            manager.version(),
            ForeignToplevelUdata,
        ) else {
            return None;
        };
        manager.toplevel(&toplevel);

        toplevel.title(self.title.clone());
        toplevel.app_id(self.app_id.clone());
        toplevel.state(state_for_version(&self.states, toplevel.version()));

        let mut outputs = Vec::new();
        if let Some(output) = &self.output {
            for wl_output in output.client_outputs(client) {
                toplevel.output_enter(&wl_output);
                outputs.push(wl_output);
            }
        }

        self.wlr_management_instances
            .insert(toplevel.clone(), outputs);
        Some(toplevel)
    }
}

/// A toplevel snapshot at refresh time, for diffing against announced data.
struct ToplevelSnapshot {
    surface: WlSurface,
    title: String,
    app_id: String,
    states: Vec<u8>,
    output: Option<Output>,
    parent: Option<WlSurface>,
}

/// The foreign-toplevel module state, held by [`State`].
#[derive(Default)]
pub struct ForeignToplevelManagerState {
    wlr_management_instances: HashSet<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<WlSurface, ToplevelData>,
}

impl ForeignToplevelManagerState {
    /// Register the wlr management global; ext-list lives in `State::foreign_toplevel_list`.
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelUdata>,
        D: Dispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelUdata>,
        D: Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelUdata>,
        D: 'static,
    {
        dh.create_global::<D, ZwlrForeignToplevelManagerV1, _>(
            WLR_MANAGEMENT_VERSION,
            ForeignToplevelUdata,
        );
        Self::default()
    }

    /// Diff snapshots against announced toplevels, emitting events; also drives ext-list in the same pass.
    fn apply<D>(
        &mut self,
        dh: &DisplayHandle,
        list: &mut ForeignToplevelListState,
        snapshots: Vec<ToplevelSnapshot>,
    ) where
        D: ForeignToplevelListHandler
            + Dispatch<ExtForeignToplevelHandleV1, ForeignToplevelHandle>
            + Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelUdata>
            + 'static,
    {
        // 1. Close toplevels that are gone (destroyed or crashed).
        self.toplevels.retain(|surface, data| {
            let keep = snapshots.iter().any(|snap| snap.surface == *surface);
            if !keep {
                if let Some(handle) = data.ext_handle.take() {
                    list.remove_toplevel(&handle);
                }
                for instance in data.wlr_management_instances.keys() {
                    instance.closed();
                }
            }
            keep
        });

        // 2. Create or update the remaining toplevels.
        // Surfaces whose parent changed or that are new: their own handle and
        // the handles of their children must re-send the wlr `parent` event.
        let mut parent_targets: HashSet<WlSurface> = HashSet::new();
        for snap in &snapshots {
            match self.toplevels.entry(snap.surface.clone()) {
                Entry::Occupied(mut entry) => {
                    let data = entry.get_mut();

                    let mut new_title = None;
                    if data.title != snap.title {
                        data.title = snap.title.clone();
                        new_title = Some(snap.title.clone());
                    }

                    let mut new_app_id = None;
                    if data.app_id != snap.app_id {
                        data.app_id = snap.app_id.clone();
                        new_app_id = Some(snap.app_id.clone());
                    }

                    // ext handle stays in sync with the diff above, so only touch it on changes.
                    if (new_title.is_some() || new_app_id.is_some())
                        && let Some(handle) = &data.ext_handle
                    {
                        if let Some(title) = &new_title {
                            handle.send_title(title);
                        }
                        if let Some(app_id) = &new_app_id {
                            handle.send_app_id(app_id);
                        }
                        handle.send_done();
                    }

                    let mut states_changed = false;
                    if data.states != snap.states {
                        data.states = snap.states.clone();
                        states_changed = true;
                    }

                    let mut output_changed = false;
                    if data.output.as_ref() != snap.output.as_ref() {
                        data.output = snap.output.clone();
                        output_changed = true;
                    }

                    if data.parent != snap.parent {
                        data.parent = snap.parent.clone();
                        parent_targets.insert(snap.surface.clone());
                    }

                    if new_title.is_some()
                        || new_app_id.is_some()
                        || states_changed
                        || output_changed
                    {
                        for (instance, outputs) in &mut data.wlr_management_instances {
                            if let Some(title) = &new_title {
                                instance.title(title.clone());
                            }
                            if let Some(app_id) = &new_app_id {
                                instance.app_id(app_id.clone());
                            }
                            if states_changed {
                                instance.state(state_for_version(&snap.states, instance.version()));
                            }
                            if output_changed {
                                for wl_output in outputs.drain(..) {
                                    instance.output_leave(&wl_output);
                                }
                                if let (Some(output), Some(client)) =
                                    (&data.output, instance.client())
                                {
                                    for wl_output in output.client_outputs(&client) {
                                        instance.output_enter(&wl_output);
                                        outputs.push(wl_output);
                                    }
                                }
                            }
                            instance.done();
                        }
                    }

                    // Clean up dead wl_outputs.
                    for outputs in data.wlr_management_instances.values_mut() {
                        outputs.retain(|wl_output| wl_output.is_alive());
                    }
                }
                Entry::Vacant(entry) => {
                    let mut data = ToplevelData {
                        title: snap.title.clone(),
                        app_id: snap.app_id.clone(),
                        states: snap.states.clone(),
                        output: snap.output.clone(),
                        parent: snap.parent.clone(),
                        wlr_management_instances: HashMap::new(),
                        ext_handle: None,
                    };
                    data.ext_handle =
                        Some(list.new_toplevel::<D>(data.title.clone(), data.app_id.clone()));
                    parent_targets.insert(snap.surface.clone());
                    for manager in &self.wlr_management_instances {
                        if let Some(client) = manager.client() {
                            data.add_wlr_instance::<D>(dh, &client, manager);
                        }
                    }

                    entry.insert(data);
                }
            }
        }

        // Children of targeted surfaces change alongside them.
        for snap in &snapshots {
            if let Some(parent) = &snap.parent
                && parent_targets.contains(parent)
            {
                parent_targets.insert(snap.surface.clone());
            }
        }

        for surface in parent_targets {
            self.send_wlr_parent(&surface);
        }
    }

    /// Send the wlr v3 `parent` event on every handle of `surface` and its children.
    fn send_wlr_parent(&mut self, surface: &WlSurface) {
        let instances: Vec<ZwlrForeignToplevelHandleV1> = self
            .toplevels
            .get(surface)
            .map(|data| data.wlr_management_instances.keys().cloned().collect())
            .unwrap_or_default();
        for instance in &instances {
            self.send_parent_to(surface, instance);
            instance.done();
        }

        let children: Vec<(WlSurface, ZwlrForeignToplevelHandleV1)> = self
            .toplevels
            .iter()
            .filter_map(|(child, data)| {
                if data.parent.as_ref() != Some(surface) {
                    return None;
                }
                Some(
                    data.wlr_management_instances
                        .keys()
                        .cloned()
                        .map(|instance| (child.clone(), instance))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect();
        for (child, instance) in children {
            self.send_parent_to(&child, &instance);
            instance.done();
        }
    }

    /// Send the wlr v3 `parent` event on `instance`, matching the parent's
    /// handle in the same client; pre-v3 handles keep their own version.
    fn send_parent_to(&self, surface: &WlSurface, instance: &ZwlrForeignToplevelHandleV1) {
        let Some(parent) = self
            .toplevels
            .get(surface)
            .and_then(|data| data.parent.as_ref())
        else {
            return;
        };
        let Some(client) = instance.client() else {
            return;
        };
        let parent = self.toplevels.get(parent).and_then(|data| {
            data.wlr_management_instances
                .keys()
                .find(|handle| handle.client().as_ref() == Some(&client))
        });
        instance.parent(parent);
    }

    fn surface_for_wlr_handle(&self, resource: &ZwlrForeignToplevelHandleV1) -> Option<WlSurface> {
        self.toplevels
            .iter()
            .find(|(_, data)| data.wlr_management_instances.contains_key(resource))
            .map(|(surface, _)| surface.clone())
    }

    fn remove_wlr_instance(&mut self, resource: &ZwlrForeignToplevelHandleV1) {
        for data in self.toplevels.values_mut() {
            data.wlr_management_instances.remove(resource);
        }
    }
}

/// The wlr `state` payload for a toplevel in `mode` with `focused` focus.
fn to_state_vec(mode: WindowMode, focused: bool) -> Vec<u8> {
    let mut states = Vec::new();
    if matches!(mode, WindowMode::Maximized) {
        states.extend((zwlr_foreign_toplevel_handle_v1::State::Maximized as u32).to_ne_bytes());
    }
    if matches!(mode, WindowMode::Fullscreen) {
        states.extend((zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32).to_ne_bytes());
    }
    if focused {
        states.extend((zwlr_foreign_toplevel_handle_v1::State::Activated as u32).to_ne_bytes());
    }
    states
}

/// Drop `fullscreen` from the state payload for pre-v2 handles.
fn state_for_version(states: &[u8], version: u32) -> Vec<u8> {
    if version >= 2 {
        return states.to_vec();
    }
    let fullscreen = (zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32).to_ne_bytes();
    states
        .chunks_exact(4)
        .filter(|chunk| *chunk != fullscreen)
        .flatten()
        .copied()
        .collect()
}

/// True once the surface has committed a buffer (xdg mapped).
fn has_buffer(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states
            .data_map
            .get::<RendererSurfaceStateUserData>()
            .is_some()
    })
}

/// The toplevel's current title and app_id.
fn foreign_toplevel_title_app_id(surface: &WlSurface) -> (String, String) {
    with_states(surface, |states| {
        let Some(attrs) = states.data_map.get::<XdgToplevelSurfaceData>() else {
            return (String::new(), String::new());
        };
        let attrs = attrs.lock().unwrap();
        (
            attrs.title.clone().unwrap_or_default(),
            attrs.app_id.clone().unwrap_or_default(),
        )
    })
}

impl<BackendData: Backend + 'static> State<BackendData> {
    /// Reconcile both protocols with the current windows; focused last so old windows deactivate first.
    pub fn foreign_toplevel_refresh(&mut self) {
        let focused = self.seat.get_keyboard().unwrap().current_focus();

        let mut snapshots: Vec<ToplevelSnapshot> = Vec::new();
        for (surface, ws) in &self.toplevels {
            if !surface.is_alive() || !has_buffer(surface) {
                continue;
            }
            let (title, app_id) = foreign_toplevel_title_app_id(surface);
            let states = to_state_vec(ws.mode, focused.as_ref() == Some(surface));
            let output = self
                .space
                .outputs_for_element(&ws.window)
                .into_iter()
                .next();
            snapshots.push(ToplevelSnapshot {
                surface: surface.clone(),
                title,
                app_id,
                states,
                output,
                parent: ws.window.toplevel().and_then(|toplevel| toplevel.parent()),
            });
        }

        // Focused last.
        if let Some(focused) = focused {
            snapshots.sort_by_key(|snap| (snap.surface == focused) as u8);
        }

        self.session.foreign_toplevel.apply::<State<BackendData>>(
            &self.display_handle,
            &mut self.session.foreign_toplevel_list,
            snapshots,
        );
    }

    /// wlr `activate` request: raise the toplevel and give it keyboard focus.
    fn foreign_activate(&mut self, surface: &WlSurface) {
        let Some(ws) = self.toplevels.get(surface) else {
            return;
        };
        let window = ws.window.clone();
        self.focus_window(&window, SERIAL_COUNTER.next_serial());
    }

    /// wlr `close` request: ask the toplevel to close.
    fn foreign_close(&mut self, surface: &WlSurface) {
        let Some(ws) = self.toplevels.get(surface) else {
            return;
        };
        if let Some(toplevel) = ws.window.toplevel() {
            toplevel.send_close();
        }
    }
}

impl<BackendData: Backend + 'static> ForeignToplevelListHandler for State<BackendData> {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.session.foreign_toplevel_list
    }
}

impl<BackendData: Backend + 'static>
    GlobalDispatch2<ZwlrForeignToplevelManagerV1, State<BackendData>> for ForeignToplevelUdata
{
    fn bind(
        &self,
        state: &mut State<BackendData>,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        let manager = data_init.init(resource, ForeignToplevelUdata);

        // Handles need a `done` per surface; `send_wlr_parent` sends it.
        let mut new_surfaces = Vec::new();
        for (surface, data) in state.session.foreign_toplevel.toplevels.iter_mut() {
            if data
                .add_wlr_instance::<State<BackendData>>(dh, client, &manager)
                .is_some()
            {
                new_surfaces.push(surface.clone());
            }
        }
        for surface in new_surfaces {
            state.session.foreign_toplevel.send_wlr_parent(&surface);
        }

        state
            .session
            .foreign_toplevel
            .wlr_management_instances
            .insert(manager);
    }
}

impl<BackendData: Backend + 'static> Dispatch2<ZwlrForeignToplevelManagerV1, State<BackendData>>
    for ForeignToplevelUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                resource.finished();
                state
                    .session
                    .foreign_toplevel
                    .wlr_management_instances
                    .remove(resource);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        state: &mut State<BackendData>,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
    ) {
        state
            .session
            .foreign_toplevel
            .wlr_management_instances
            .remove(resource);
    }
}

impl<BackendData: Backend + 'static> Dispatch2<ZwlrForeignToplevelHandleV1, State<BackendData>>
    for ForeignToplevelUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        let surface = state
            .session
            .foreign_toplevel
            .surface_for_wlr_handle(resource);
        let Some(surface) = surface else {
            return;
        };

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized
            | zwlr_foreign_toplevel_handle_v1::Request::SetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. }
            | zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. }
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                // State is compositor-driven and reflected back on the next refresh.
            }
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.foreign_activate(&surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.foreign_close(&surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        state: &mut State<BackendData>,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
    ) {
        state.session.foreign_toplevel.remove_wlr_instance(resource);
    }
}
