//! Our zwlr-foreign-toplevel-management-v1 + smithay's ext-foreign-toplevel-list-v1, driven by [`State::foreign_toplevel_refresh`].

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use smithay::backend::renderer::utils::with_renderer_surface_state;
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
use smithay::wayland::compositor::with_states;
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
};
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::backend::Backend;
use crate::state::{State, WindowMode};

const WLR_MANAGEMENT_VERSION: u32 = 3;

trait WlrHandle: Eq + Hash + Clone {
    type Client: PartialEq;

    fn client_id(&self) -> Option<Self::Client>;
    fn wlr_version(&self) -> u32;
    fn send_parent(&self, parent: Option<&Self>);
    fn send_done(&self);
    fn send_closed(&self);
}

impl WlrHandle for ZwlrForeignToplevelHandleV1 {
    type Client = ClientId;

    fn client_id(&self) -> Option<ClientId> {
        self.client().map(|client| client.id())
    }

    fn wlr_version(&self) -> u32 {
        Resource::version(self)
    }

    fn send_parent(&self, parent: Option<&Self>) {
        self.parent(parent);
    }

    fn send_done(&self) {
        self.done();
    }

    fn send_closed(&self) {
        self.closed();
    }
}

/// Per-client handle bookkeeping stored alongside the entered outputs.
#[derive(Debug)]
struct WlrInstance<S, O> {
    outputs: Vec<O>,
    /// Last non-null parent announced on this handle (`v3+` only), so `None`
    /// is sent when a parent clears without spamming parentless opens.
    announced_parent: Option<S>,
    needs_done: bool,
}

trait WlrEntry<S, H: WlrHandle, O> {
    fn parent(&self) -> Option<&S>;
    fn set_parent(&mut self, parent: Option<S>);
    fn instances(&self) -> &HashMap<H, WlrInstance<S, O>>;
    fn instances_mut(&mut self) -> &mut HashMap<H, WlrInstance<S, O>>;
}

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
    /// Per-client wlr handles, each with entered outputs and parent sync state.
    wlr_management_instances: HashMap<ZwlrForeignToplevelHandleV1, WlrInstance<WlSurface, WlOutput>>,
    /// smithay's ext-list handle; owns all ext instance bookkeeping.
    ext_handle: Option<ForeignToplevelHandle>,
}

impl WlrEntry<WlSurface, ZwlrForeignToplevelHandleV1, WlOutput> for ToplevelData {
    fn parent(&self) -> Option<&WlSurface> {
        self.parent.as_ref()
    }

    fn set_parent(&mut self, parent: Option<WlSurface>) {
        self.parent = parent;
    }

    fn instances(&self) -> &HashMap<ZwlrForeignToplevelHandleV1, WlrInstance<WlSurface, WlOutput>> {
        &self.wlr_management_instances
    }

    fn instances_mut(
        &mut self,
    ) -> &mut HashMap<ZwlrForeignToplevelHandleV1, WlrInstance<WlSurface, WlOutput>> {
        &mut self.wlr_management_instances
    }
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

        self.wlr_management_instances.insert(
            toplevel.clone(),
            WlrInstance {
                outputs,
                announced_parent: None,
                needs_done: true,
            },
        );
        Some(toplevel)
    }

    /// Read a newly observed toplevel's full announcement state.
    fn new(update: &ToplevelUpdate) -> Self {
        let (title, app_id) = with_xdg_title_app_id(&update.surface, |title, app_id| {
            (
                title.unwrap_or_default().to_owned(),
                app_id.unwrap_or_default().to_owned(),
            )
        });
        ToplevelData {
            title,
            app_id,
            states: update.states.clone(),
            output: update.output.clone(),
            parent: update.parent.clone(),
            wlr_management_instances: HashMap::new(),
            ext_handle: None,
        }
    }

    /// Title/app_id present only where they differ from the announced ones.
    fn changed_title_app_id(&self, surface: &WlSurface) -> (Option<String>, Option<String>) {
        with_xdg_title_app_id(surface, |title, app_id| {
            let title = title.unwrap_or("");
            let app_id = app_id.unwrap_or("");
            (
                (self.title != title).then(|| title.to_owned()),
                (self.app_id != app_id).then(|| app_id.to_owned()),
            )
        })
    }
}

/// One toplevel's freshly observed state, applied to the announced set.
struct ToplevelUpdate {
    surface: WlSurface,
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

    /// Announce or update one toplevel, emitting events; drives the ext-list in the same pass.
    fn apply_one<D>(
        &mut self,
        dh: &DisplayHandle,
        list: &mut ForeignToplevelListState,
        update: &ToplevelUpdate,
        parent_targets: &mut HashSet<WlSurface>,
    ) where
        D: ForeignToplevelListHandler
            + Dispatch<ExtForeignToplevelHandleV1, ForeignToplevelHandle>
            + Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelUdata>
            + 'static,
    {
        match self.toplevels.entry(update.surface.clone()) {
            Entry::Occupied(mut entry) => {
                let data = entry.get_mut();
                let (title, app_id) = data.changed_title_app_id(&update.surface);

                let mut new_title = None;
                if let Some(title) = title {
                    data.title = title.clone();
                    new_title = Some(title);
                }

                let mut new_app_id = None;
                if let Some(app_id) = app_id {
                    data.app_id = app_id.clone();
                    new_app_id = Some(app_id);
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
                if data.states != update.states {
                    data.states = update.states.clone();
                    states_changed = true;
                }

                let mut output_changed = false;
                if data.output.as_ref() != update.output.as_ref() {
                    data.output = update.output.clone();
                    output_changed = true;
                }

                if data.parent != update.parent {
                    data.parent = update.parent.clone();
                    parent_targets.insert(update.surface.clone());
                }

                if new_title.is_some() || new_app_id.is_some() || states_changed || output_changed {
                    for (instance, inst) in &mut data.wlr_management_instances {
                        if let Some(title) = &new_title {
                            instance.title(title.clone());
                        }
                        if let Some(app_id) = &new_app_id {
                            instance.app_id(app_id.clone());
                        }
                        if states_changed {
                            instance.state(state_for_version(&update.states, instance.version()));
                        }
                        if output_changed {
                            for wl_output in inst.outputs.drain(..) {
                                instance.output_leave(&wl_output);
                            }
                            if let (Some(output), Some(client)) = (&data.output, instance.client())
                            {
                                for wl_output in output.client_outputs(&client) {
                                    instance.output_enter(&wl_output);
                                    inst.outputs.push(wl_output);
                                }
                            }
                        }
                        inst.needs_done = true;
                    }
                }

                // Clean up dead wl_outputs.
                for inst in data.wlr_management_instances.values_mut() {
                    inst.outputs.retain(|wl_output| wl_output.is_alive());
                }
            }
            Entry::Vacant(entry) => {
                let mut data = ToplevelData::new(update);
                data.ext_handle =
                    Some(list.new_toplevel::<D>(data.title.clone(), data.app_id.clone()));
                parent_targets.insert(update.surface.clone());
                for manager in &self.wlr_management_instances {
                    if let Some(client) = manager.client() {
                        data.add_wlr_instance::<D>(dh, &client, manager);
                    }
                }

                entry.insert(data);
            }
        }
    }

    /// Close and drop announced toplevels that are no longer active.
    fn retain_active(&mut self, list: &mut ForeignToplevelListState, seen: &HashSet<WlSurface>) {
        retain_active_entries(&mut self.toplevels, seen, |data| {
            if let Some(handle) = data.ext_handle.take() {
                list.remove_toplevel(&handle);
            }
        });
    }

    /// The `wl_surface` behind a wlr handle, if its toplevel is still announced.
    pub fn surface_for_wlr_handle(
        &self,
        resource: &ZwlrForeignToplevelHandleV1,
    ) -> Option<WlSurface> {
        self.toplevels
            .iter()
            .find(|(_, data)| data.wlr_management_instances.contains_key(resource))
            .map(|(surface, _)| surface.clone())
    }

    /// The `wl_surface` behind an ext-foreign-toplevel handle, if tracked.
    pub fn surface_for_ext_handle(&self, handle: &ForeignToplevelHandle) -> Option<WlSurface> {
        self.toplevels
            .iter()
            .find(|(_, data)| {
                data.ext_handle
                    .as_ref()
                    .is_some_and(|ext| ext.matches(handle))
            })
            .map(|(surface, _)| surface.clone())
    }

    fn remove_wlr_instance(&mut self, resource: &ZwlrForeignToplevelHandleV1) {
        for data in self.toplevels.values_mut() {
            data.wlr_management_instances.remove(resource);
        }
    }
}

/// Emit `done` on every handle of `data` with pending changes.
fn flush_done_entry<S, H, O, E>(data: &mut E)
where
    H: WlrHandle,
    E: WlrEntry<S, H, O>,
{
    for (handle, inst) in data.instances_mut() {
        if inst.needs_done {
            handle.send_done();
            inst.needs_done = false;
        }
    }
}

/// Emit `done` on every handle across `entries` that has pending changes.
fn flush_all_done<S, H, O, E>(entries: &mut HashMap<S, E>)
where
    S: Eq + Hash,
    H: WlrHandle,
    E: WlrEntry<S, H, O>,
{
    for data in entries.values_mut() {
        flush_done_entry(data);
    }
}

/// Propagate parent updates across `targets` and their children, emitting
/// `parent` on v3+ handles whenever a handle's resolved parent changed.
fn sync_parents<S, H, O, E>(entries: &mut HashMap<S, E>, targets: &mut HashSet<S>)
where
    S: Eq + Hash + Clone,
    H: WlrHandle,
    E: WlrEntry<S, H, O>,
{
    if targets.is_empty() {
        return;
    }
    loop {
        let before = targets.len();
        for (surface, data) in entries.iter() {
            if let Some(parent) = data.parent()
                && targets.contains(parent)
            {
                targets.insert(surface.clone());
            }
        }
        if targets.len() == before {
            break;
        }
    }

    let surfaces: Vec<S> = targets.iter().cloned().collect();
    for surface in surfaces {
        let desired_parent = entries
            .get(&surface)
            .and_then(|data| data.parent().cloned());
        let Some(handles) = entries
            .get(&surface)
            .map(|data| data.instances().keys().cloned().collect::<Vec<_>>())
        else {
            continue;
        };

        let updates: Vec<(H, Option<H>, Option<S>)> = handles
            .into_iter()
            .filter(|handle| handle.wlr_version() >= 3)
            .filter_map(|handle| {
                let client = handle.client_id()?;
                let resolved = desired_parent.as_ref().and_then(|parent| {
                    entries.get(parent).and_then(|data| {
                        data.instances()
                            .keys()
                            .find(|h| h.client_id().as_ref() == Some(&client))
                            .cloned()
                            .map(|ph| (ph, parent.clone()))
                    })
                });
                let (parent_handle, parent_surface) = match resolved {
                    Some((ph, ps)) => (Some(ph), Some(ps)),
                    None => (None, None),
                };
                Some((handle, parent_handle, parent_surface))
            })
            .collect();

        if let Some(data) = entries.get_mut(&surface) {
            for (handle, parent_handle, parent_surface) in updates {
                if let Some(inst) = data.instances_mut().get_mut(&handle)
                    && inst.announced_parent != parent_surface
                {
                    handle.send_parent(parent_handle.as_ref());
                    inst.announced_parent = parent_surface;
                    inst.needs_done = true;
                }
            }
        }
    }
}

/// Drop entries missing from `seen`. Active children referencing a dropped parent
/// have their parent cleared and announced as `parent(None)` before the parent's
/// `closed` event goes out.
fn retain_active_entries<S, H, O, E>(
    entries: &mut HashMap<S, E>,
    seen: &HashSet<S>,
    mut on_remove: impl FnMut(&mut E),
) where
    S: Eq + Hash + Clone,
    H: WlrHandle,
    E: WlrEntry<S, H, O>,
{
    let dropped: HashSet<S> = entries
        .keys()
        .filter(|surface| !seen.contains(*surface))
        .cloned()
        .collect();
    if dropped.is_empty() {
        return;
    }

    let mut cleared = HashSet::new();
    for (surface, data) in entries.iter_mut() {
        if !dropped.contains(surface)
            && data.parent().is_some_and(|parent| dropped.contains(parent))
        {
            data.set_parent(None);
            cleared.insert(surface.clone());
        }
    }
    sync_parents(entries, &mut cleared);
    for surface in &cleared {
        if let Some(data) = entries.get_mut(surface) {
            flush_done_entry(data);
        }
    }

    entries.retain(|surface, data| {
        let keep = seen.contains(surface);
        if !keep {
            on_remove(data);
            for handle in data.instances().keys() {
                handle.send_closed();
            }
        }
        keep
    });
}

/// The wlr `state` payload for a toplevel in `mode`, `active` or not.
fn to_state_vec(mode: WindowMode, active: bool) -> Vec<u8> {
    let mut states = Vec::new();
    if matches!(mode, WindowMode::Maximized) {
        states.extend((zwlr_foreign_toplevel_handle_v1::State::Maximized as u32).to_ne_bytes());
    }
    if matches!(mode, WindowMode::Fullscreen) {
        states.extend((zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32).to_ne_bytes());
    }
    if active {
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
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|chunk| **chunk != fullscreen)
        .flatten()
        .copied()
        .collect()
}

/// True while the surface has a buffer (xdg mapped): a null buffer unmaps it.
fn has_buffer(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

/// Runs `f` with the toplevel's (title, app_id) under the role lock.
fn with_xdg_title_app_id<T>(
    surface: &WlSurface,
    f: impl FnOnce(Option<&str>, Option<&str>) -> T,
) -> T {
    with_states(surface, |states| {
        match states.data_map.get::<XdgToplevelSurfaceData>() {
            Some(attrs) => {
                let attrs = attrs.lock().unwrap();
                f(attrs.title.as_deref(), attrs.app_id.as_deref())
            }
            None => f(None, None),
        }
    })
}

impl<BackendData: Backend + 'static> State<BackendData> {
    /// Reconcile both protocols with the current windows; active last so old windows deactivate first.
    pub fn foreign_toplevel_refresh(&mut self) {
        // Activated is what xdg `activated` is: the active window and its
        // parents, whoever holds the keyboard (an open menu, an `Exclusive`
        // layer).
        let active = self.active_group();
        let Self {
            toplevels,
            space,
            foreign_toplevel: manager,
            foreign_toplevel_list: list,
            display_handle,
            ..
        } = self;

        let active_surfaces: HashSet<WlSurface> = toplevels
            .iter()
            .filter(|(surface, _)| surface.is_alive() && has_buffer(surface))
            .map(|(surface, _)| surface.clone())
            .collect();

        let mut seen: HashSet<WlSurface> = HashSet::new();
        let mut parent_targets: HashSet<WlSurface> = HashSet::new();
        // Two passes so the active windows are announced last.
        for active_pass in [false, true] {
            for (surface, ws) in toplevels.iter() {
                let is_active = active.contains(surface);
                if is_active != active_pass || !active_surfaces.contains(surface) {
                    continue;
                }
                seen.insert(surface.clone());
                let output = space.outputs_for_element(&ws.window).into_iter().next();
                let parent = ws
                    .window
                    .toplevel()
                    .and_then(|toplevel| toplevel.parent())
                    .filter(|p| active_surfaces.contains(p));
                let states = to_state_vec(ws.mode, is_active);
                let update = ToplevelUpdate {
                    surface: surface.clone(),
                    states,
                    output,
                    parent,
                };
                manager.apply_one::<State<BackendData>>(
                    display_handle,
                    list,
                    &update,
                    &mut parent_targets,
                );
            }
        }

        sync_parents(&mut manager.toplevels, &mut parent_targets);
        flush_all_done(&mut manager.toplevels);
        manager.retain_active(list, &seen);
    }

    /// wlr `activate` request: raise the toplevel and make it the active one.
    /// A toplevel with dialogs gets the one [`activation_target`] picks, so
    /// switching back to it does not hide them.
    fn foreign_activate(&mut self, surface: &WlSurface) {
        if !self.toplevels.contains_key(surface) {
            return;
        }
        let stack: Vec<Stacked<WlSurface>> = self
            .space
            .elements()
            .filter_map(|window| {
                let toplevel = window.toplevel()?;
                let window = toplevel.wl_surface().clone();
                let modal = self.toplevels.get(&window).is_some_and(|ws| ws.modal);
                Some(Stacked {
                    parent: toplevel.parent(),
                    window,
                    modal,
                })
            })
            .collect();
        let target = activation_target(surface, &stack);
        let Some(ws) = self.toplevels.get(&target) else {
            return;
        };
        let window = ws.window.clone();
        self.focus_window(&window);
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
        &mut self.foreign_toplevel_list
    }
}

impl<BackendData: Backend + 'static>
    GlobalDispatch2<ZwlrForeignToplevelManagerV1, State<BackendData>> for ForeignToplevelUdata
{
    fn can_view(&self, client: &Client) -> bool {
        crate::state::client_is_unrestricted(client)
    }

    fn bind(
        &self,
        state: &mut State<BackendData>,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        let manager = data_init.init(resource, ForeignToplevelUdata);

        // Handles need initial parent events (v3+) and a `done` per surface.
        let mut new_surfaces = HashSet::new();
        for (surface, data) in state.foreign_toplevel.toplevels.iter_mut() {
            if data
                .add_wlr_instance::<State<BackendData>>(dh, client, &manager)
                .is_some()
            {
                new_surfaces.insert(surface.clone());
            }
        }
        sync_parents(&mut state.foreign_toplevel.toplevels, &mut new_surfaces);
        flush_all_done(&mut state.foreign_toplevel.toplevels);

        state
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
        let surface = state.foreign_toplevel.surface_for_wlr_handle(resource);
        let Some(surface) = surface else {
            return;
        };

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized
            | zwlr_foreign_toplevel_handle_v1::Request::SetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. }
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                // State is compositor-driven and reflected back on the next refresh.
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { width, height, .. } => {
                // Unused, but still checked.
                if width < 0 || height < 0 {
                    resource.post_error(
                        zwlr_foreign_toplevel_handle_v1::Error::InvalidRectangle,
                        "negative rectangle size",
                    );
                }
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
        state.foreign_toplevel.remove_wlr_instance(resource);
    }
}

/// One mapped window as [`activation_target`] sees it.
struct Stacked<T> {
    window: T,
    parent: Option<T>,
    modal: bool,
}

/// The window a shell's activate of `target` focuses, given every mapped
/// window back to front: `target`'s topmost modal dialog, else `target`.
/// Plain dialogs stay on screen above it and are focused by a tap.
fn activation_target<T: Clone + PartialEq>(target: &T, stack: &[Stacked<T>]) -> T {
    let parent_of = |w: &T| {
        stack
            .iter()
            .find(|s| &s.window == w)
            .and_then(|s| s.parent.as_ref())
    };
    // Bounded by the stack's length, in case parents ever form a cycle.
    let below_target = |s: &Stacked<T>| {
        let mut parent = s.parent.as_ref();
        for _ in 0..stack.len() {
            match parent {
                Some(p) if p == target => return true,
                Some(p) => parent = parent_of(p),
                None => return false,
            }
        }
        false
    };
    stack
        .iter()
        .filter(|s| s.modal && below_target(s))
        .last()
        .map_or_else(|| target.clone(), |s| s.window.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(window: u32, parent: Option<u32>, modal: bool) -> Stacked<u32> {
        Stacked {
            window,
            parent,
            modal,
        }
    }

    #[test]
    fn a_window_without_dialogs_is_itself() {
        assert_eq!(
            activation_target(&1, &[w(1, None, false), w(2, None, false)]),
            1
        );
    }

    #[test]
    fn a_plain_dialog_focused_last_does_not_take_it() {
        let stack = [w(1, None, false), w(2, Some(1), false), w(3, None, false)];
        assert_eq!(activation_target(&1, &stack), 1);
    }

    #[test]
    fn the_parent_focused_after_its_dialog_stays_focused() {
        let stack = [w(2, Some(1), false), w(1, None, false)];
        assert_eq!(activation_target(&1, &stack), 1);
    }

    #[test]
    fn a_modal_dialog_wins_over_anything_focused_later() {
        let stack = [w(3, Some(1), true), w(2, Some(1), false), w(1, None, false)];
        assert_eq!(activation_target(&1, &stack), 3);
    }

    #[test]
    fn the_topmost_of_several_modals_wins() {
        let stack = [w(1, None, false), w(2, Some(1), true), w(3, Some(1), true)];
        assert_eq!(activation_target(&1, &stack), 3);
    }

    #[test]
    fn a_dialog_of_a_dialog_counts() {
        let stack = [w(1, None, false), w(2, Some(1), false), w(3, Some(2), true)];
        assert_eq!(activation_target(&1, &stack), 3);
    }

    #[test]
    fn other_apps_dialogs_are_ignored() {
        let stack = [w(1, None, false), w(5, None, false), w(6, Some(5), true)];
        assert_eq!(activation_target(&1, &stack), 1);
    }

    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum HandleEvent {
        Parent(u32, Option<u32>),
        Done(u32),
        Closed(u32),
        ExtRemoved(u32),
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct TestHandle {
        id: u32,
        client: u32,
        version: u32,
        log: LogRef,
    }

    #[derive(Debug, Default)]
    struct EventLog(Rc<RefCell<Vec<HandleEvent>>>);

    impl EventLog {
        fn take(&self) -> Vec<HandleEvent> {
            std::mem::take(&mut *self.0.borrow_mut())
        }
    }

    #[derive(Debug, Clone)]
    struct LogRef(Rc<RefCell<Vec<HandleEvent>>>);

    impl PartialEq for LogRef {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.0, &other.0)
        }
    }
    impl Eq for LogRef {}
    impl Hash for LogRef {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            (Rc::as_ptr(&self.0) as usize).hash(state);
        }
    }

    impl WlrHandle for TestHandle {
        type Client = u32;

        fn client_id(&self) -> Option<u32> {
            Some(self.client)
        }

        fn wlr_version(&self) -> u32 {
            self.version
        }

        fn send_parent(&self, parent: Option<&Self>) {
            self.log
                .0
                .borrow_mut()
                .push(HandleEvent::Parent(self.id, parent.map(|p| p.id)));
        }

        fn send_done(&self) {
            self.log.0.borrow_mut().push(HandleEvent::Done(self.id));
        }

        fn send_closed(&self) {
            self.log.0.borrow_mut().push(HandleEvent::Closed(self.id));
        }
    }

    struct TestEntry {
        parent: Option<u32>,
        ext: u32,
        instances: HashMap<TestHandle, WlrInstance<u32, ()>>,
    }

    impl WlrEntry<u32, TestHandle, ()> for TestEntry {
        fn parent(&self) -> Option<&u32> {
            self.parent.as_ref()
        }

        fn set_parent(&mut self, parent: Option<u32>) {
            self.parent = parent;
        }

        fn instances(&self) -> &HashMap<TestHandle, WlrInstance<u32, ()>> {
            &self.instances
        }

        fn instances_mut(&mut self) -> &mut HashMap<TestHandle, WlrInstance<u32, ()>> {
            &mut self.instances
        }
    }

    struct Harness {
        log: EventLog,
        clients: Vec<(u32, u32)>,
        entries: HashMap<u32, TestEntry>,
        next_handle: u32,
    }

    impl Harness {
        fn new(clients: &[(u32, u32)]) -> Self {
            Self {
                log: EventLog::default(),
                clients: clients.to_vec(),
                entries: HashMap::new(),
                next_handle: 100,
            }
        }

        fn refresh(&mut self, active: &[(u32, Option<u32>)]) -> HashMap<u32, Vec<TestHandle>> {
            let active_set: HashSet<u32> = active.iter().map(|&(s, _)| s).collect();
            let mut targets = HashSet::new();
            let mut created: HashMap<u32, Vec<TestHandle>> = HashMap::new();

            for &(surface, parent) in active {
                let parent = parent.filter(|p| active_set.contains(p));
                match self.entries.entry(surface) {
                    Entry::Occupied(mut e) => {
                        let data = e.get_mut();
                        if data.parent != parent {
                            data.parent = parent;
                            targets.insert(surface);
                        }
                    }
                    Entry::Vacant(e) => {
                        let mut instances = HashMap::new();
                        for &(client, version) in &self.clients {
                            let h = TestHandle {
                                id: self.next_handle,
                                client,
                                version,
                                log: LogRef(self.log.0.clone()),
                            };
                            self.next_handle += 1;
                            created.entry(surface).or_default().push(h.clone());
                            instances.insert(
                                h,
                                WlrInstance {
                                    outputs: Vec::new(),
                                    announced_parent: None,
                                    needs_done: true,
                                },
                            );
                        }
                        targets.insert(surface);
                        e.insert(TestEntry {
                            parent,
                            ext: surface * 10,
                            instances,
                        });
                    }
                }
            }

            sync_parents(&mut self.entries, &mut targets);
            flush_all_done(&mut self.entries);
            let log = self.log.0.clone();
            retain_active_entries(&mut self.entries, &active_set, |data| {
                log.borrow_mut().push(HandleEvent::ExtRemoved(data.ext));
            });
            created
        }
    }

    #[test]
    fn explicit_parent_cleared_to_none_notifies_v3() {
        let mut h = Harness::new(&[(1, 3)]);
        let created = h.refresh(&[(1, None), (2, Some(1))]);
        let p_h = created[&1][0].id;
        let c_h = created[&2][0].id;
        let events = h.log.take();
        // Parentless window receives no spurious Parent(None); child receives Parent(Some(p)).
        assert!(!events.contains(&HandleEvent::Parent(p_h, None)));
        assert!(events.contains(&HandleEvent::Parent(c_h, Some(p_h))));
        assert!(events.contains(&HandleEvent::Done(c_h)));

        // Clearing child's parent to None sends Parent(None) + Done.
        h.refresh(&[(1, None), (2, None)]);
        assert_eq!(
            h.log.take(),
            vec![HandleEvent::Parent(c_h, None), HandleEvent::Done(c_h)]
        );
    }

    #[test]
    fn parent_removed_clears_child_parent_before_closing_parent() {
        let mut h = Harness::new(&[(1, 3)]);
        let created = h.refresh(&[(1, None), (2, Some(1))]);
        let p_h = created[&1][0].id;
        let c_h = created[&2][0].id;
        h.log.take();

        // Parent closes/unmaps while child stays mapped.
        h.refresh(&[(2, Some(1))]);
        assert_eq!(
            h.log.take(),
            vec![
                HandleEvent::Parent(c_h, None),
                HandleEvent::Done(c_h),
                HandleEvent::ExtRemoved(10),
                HandleEvent::Closed(p_h),
            ]
        );

        // Even if stale xdg parent still mentions 1 on later ticks, nothing re-emits.
        h.refresh(&[(2, Some(1))]);
        assert!(h.log.take().is_empty());

        // Direct retain_active_entries drop also clears children before closing parent.
        let mut h2 = Harness::new(&[(1, 3)]);
        let created2 = h2.refresh(&[(1, None), (2, Some(1))]);
        let p2 = created2[&1][0].id;
        let c2 = created2[&2][0].id;
        h2.log.take();
        let keep = HashSet::from([2]);
        let log2 = h2.log.0.clone();
        retain_active_entries(&mut h2.entries, &keep, |data| {
            log2.borrow_mut().push(HandleEvent::ExtRemoved(data.ext));
        });
        assert_eq!(
            h2.log.take(),
            vec![
                HandleEvent::Parent(c2, None),
                HandleEvent::Done(c2),
                HandleEvent::ExtRemoved(10),
                HandleEvent::Closed(p2),
            ]
        );
    }

    #[test]
    fn pre_v3_handles_receive_no_parent_events() {
        let mut h = Harness::new(&[(1, 1), (2, 2), (3, 3)]);
        let created = h.refresh(&[(1, None), (2, Some(1))]);
        let v1_c = created[&2][0].id;
        let v2_c = created[&2][1].id;
        let v3_c = created[&2][2].id;
        let v3_p = created[&1][2].id;
        let events = h.log.take();
        assert!(!events.iter().any(|e| matches!(e, HandleEvent::Parent(id, _) if *id == v1_c || *id == v2_c)));
        assert!(events.contains(&HandleEvent::Parent(v3_c, Some(v3_p))));

        h.refresh(&[(1, None), (2, None)]);
        assert_eq!(
            h.log.take(),
            vec![HandleEvent::Parent(v3_c, None), HandleEvent::Done(v3_c)]
        );
    }

    #[test]
    fn unmap_closes_wlr_and_ext_and_remap_announces_fresh_handles() {
        let mut h = Harness::new(&[(1, 3)]);
        let first = h.refresh(&[(1, None)]);
        let first_id = first[&1][0].id;
        assert_eq!(h.log.take(), vec![HandleEvent::Done(first_id)]);

        // Unmap: closed on wlr and removed from ext.
        h.refresh(&[]);
        assert_eq!(
            h.log.take(),
            vec![HandleEvent::ExtRemoved(10), HandleEvent::Closed(first_id)]
        );

        // Remap: fresh wlr handle is created; old handle receives nothing.
        let second = h.refresh(&[(1, None)]);
        let second_id = second[&1][0].id;
        assert_ne!(first_id, second_id);
        assert_eq!(h.log.take(), vec![HandleEvent::Done(second_id)]);
    }

    #[test]
    fn child_announced_before_parent_in_same_refresh_resolves_parent_before_done() {
        let mut h = Harness::new(&[(1, 3)]);
        let created = h.refresh(&[(2, Some(1)), (1, None)]);
        let c_h = created[&2][0].id;
        let p_h = created[&1][0].id;
        let events = h.log.take();
        let parent_idx = events
            .iter()
            .position(|e| *e == HandleEvent::Parent(c_h, Some(p_h)))
            .expect("child received parent");
        let done_idx = events
            .iter()
            .position(|e| *e == HandleEvent::Done(c_h))
            .expect("child received done");
        assert!(parent_idx < done_idx);
    }
}
