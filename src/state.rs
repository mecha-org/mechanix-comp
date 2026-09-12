use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::renderer::element::{
    RenderElementStates, default_primary_scanout_output_compare,
};
use smithay::desktop::utils::{
    surface_primary_scanout_output, update_surface_primary_scanout_output,
    with_surfaces_surface_tree,
};
use smithay::desktop::{PopupManager, Space, Window, layer_map_for_output};
use smithay::input::keyboard::Keysym;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{BindError, Display, DisplayHandle};
use smithay::utils::{Clock, Logical, Monotonic, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::{CompositorClientState, CompositorState, with_states};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::{FractionalScaleManagerState, with_fractional_scale};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::session_lock::LockSurface;
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::viewporter::ViewporterState;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::backend::Backend;
use crate::layout::Layout;

/// How a toplevel is arranged right now.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WindowMode {
    /// Fills the work zone (default; re-applied on zone changes).
    Maximized,
    /// Dialogs: never sized or maximized, kept centered over their parent.
    Floating,
    /// Fullscreen request honored: rendered above the top layer.
    Fullscreen,
}

/// A toplevel's mapping state, keyed in `State::toplevels` by its `wl_surface`.
pub struct WindowState {
    pub window: Window,
    pub mode: WindowMode,
    /// True once the window joined the `Space`.
    pub mapped: bool,
    /// The xdg-dialog modal hint, cached so input handling needn't lock
    /// surface data on every click.
    pub modal: bool,
}

/// The icon surface a client sets for an active drag-and-drop, drawn next to
/// the cursor while the grab is running.
#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

pub struct State<BackendData: Backend + 'static> {
    pub start_time: Instant,
    pub socket_name: OsString,
    #[cfg(feature = "session")]
    pub session: crate::session::Session<BackendData>,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    /// All xdg toplevels ever created, keyed by `wl_surface`, whether or not
    /// mapped into `space` yet (mapping happens on the first commit). Dead
    /// entries are pruned by `cleanup_toplevels`.
    pub toplevels: HashMap<WlSurface, WindowState>,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State<BackendData>>,
    pub popups: PopupManager,

    pub seat: Seat<Self>,
    pub suppressed_keys: Vec<Keysym>,
    pub cursor_status: CursorImageStatus,
    pub clock: Clock<Monotonic>,
    pub pointer: PointerHandle<State<BackendData>>,
    pub cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,

    /// The drag-and-drop icon, set on `dnd_requested` and cleared on drop.
    pub dnd_icon: Option<DndIcon>,

    // Rendering backend + dmabuf import. The dmabuf global is created lazily by
    // each backend once its renderer (and thus its format list) exists.
    pub backend_data: BackendData,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,

    pub is_locked: bool,
    pub lock_surfaces: Vec<LockSurface>,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    /// One layout model per output; the source of truth for window stacking.
    #[allow(clippy::mutable_key_type)] // `Output` is interior-mutable, but stable as a key.
    pub layouts: HashMap<Output, Layout>,
    /// The `OnDemand` layer surface last opened or clicked. `update_keyboard_focus`
    /// focuses it while it stays a mapped OnDemand layer, so launchers and
    /// panels take keyboard focus on open.
    pub layer_shell_on_demand_focus: Option<WlSurface>,
    /// The toplevel surface last focused; the fallback keyboard focus when no
    /// layer-shell surface holds it.
    pub active_window: Option<WlSurface>,
}

/// Which well-known name to bind. Does not change layout or protocols.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketName {
    /// `wayland-1` … `wayland-32`.
    Session,
    /// `wayland-nest-0` … `wayland-nest-7`.
    Nest,
}

impl<BackendData: Backend + 'static> State<BackendData> {
    pub fn new_with_socket(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        backend_data: BackendData,
        socket: SocketName,
    ) -> Self {
        let start_time = Instant::now();
        let dh = display.handle();
        let clock = Clock::new();

        // The zwp_linux_dmabuf_v1 global is created lazily by each backend once
        // its renderer's format list is available. Dispatch is handled by the
        // blanket `delegate_dispatch2!`.
        let dmabuf_state = DmabufState::new();

        let seat_name = backend_data.seat_name();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [
                xdg_toplevel::WmCapabilities::Maximize,
                xdg_toplevel::WmCapabilities::Fullscreen,
            ],
        );
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let space = Space::default();
        let popups = PopupManager::default();
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, seat_name);
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        let pointer = seat.add_pointer();

        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        #[allow(clippy::mutable_key_type)] // `Output` is interior-mutable, but stable as a key.
        let layouts: HashMap<Output, Layout> = HashMap::new();
        CursorShapeManagerState::new::<Self>(&dh);
        #[cfg(feature = "session")]
        let session = crate::session::Session::new(&dh, event_loop);

        let socket_name = Self::init_wayland_listener(display, event_loop, socket);
        let loop_signal = event_loop.get_signal();

        Self {
            start_time,
            socket_name,
            #[cfg(feature = "session")]
            session,
            display_handle: dh,
            space,
            toplevels: HashMap::new(),
            loop_signal,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            layer_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            popups,
            seat,
            suppressed_keys: Vec::new(),
            cursor_status: CursorImageStatus::default_named(),
            pointer,
            cursor_position_hint: None,
            dnd_icon: None,
            clock,
            backend_data,
            dmabuf_state,
            dmabuf_global: None,
            is_locked: false,
            lock_surfaces: Vec::new(),
            viewporter_state,
            fractional_scale_manager_state,
            layouts,
            layer_shell_on_demand_focus: None,
            active_window: None,
        }
    }

    fn bind_socket(socket: SocketName) -> ListeningSocketSource {
        match socket {
            SocketName::Session => ListeningSocketSource::new_auto().unwrap(),
            SocketName::Nest => {
                for i in 0..8 {
                    match ListeningSocketSource::with_name(&format!("wayland-nest-{i}")) {
                        Ok(source) => return source,
                        Err(BindError::AlreadyInUse) => {}
                        Err(err) => panic!("failed to bind nest socket: {err}"),
                    }
                }
                panic!("wayland-nest-0..7 all in use");
            }
        }
    }

    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<Self>,
        socket: SocketName,
    ) -> OsString {
        let listening_socket = Self::bind_socket(socket);
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("Failed to init the wayland event source.");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    unsafe {
                        display.get_mut().dispatch_clients(state).unwrap();
                    }
                    let _ = state.display_handle.flush_clients();
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// Refresh the space, popups, dead toplevels, and keyboard focus.
    /// Callers flush after any work that still queues client events.
    pub fn refresh_space(&mut self) {
        self.space.refresh();
        self.popups.cleanup();
        self.cleanup_toplevels();
        self.update_keyboard_focus();
    }

    /// Queue a redraw on every output; the backend skips ones already pending.
    pub fn schedule_render(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in &outputs {
            self.backend_data.schedule_render(output);
        }
    }

    /// Send frame callbacks to every visible surface on `output`, once per
    /// presented frame. Lifecycle bookkeeping happens in the backends' idle
    /// callbacks instead, so client I/O isn't blocked on frame presentation.
    ///
    /// Surfaces are acked every presented frame; hidden ones (cleared scan-out
    /// records) fall back to a 1Hz throttle so their frame clocks keep running.
    pub fn send_frame_callbacks(&mut self, output: &Output) {
        let now = self.start_time.elapsed();
        #[cfg(feature = "session")]
        if self.is_locked {
            // Send frame callbacks only to live surfaces.
            for lock_surface in self.lock_surfaces.iter().filter(|s| s.alive()) {
                smithay::desktop::utils::send_frames_surface_tree(
                    lock_surface.wl_surface(),
                    output,
                    now,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }

            return;
        }
        let scale = output.current_scale().fractional_scale();
        // Visible surfaces are acked every presented frame; hidden ones get
        // one ack per second so their frame clocks keep running (1Hz).
        let throttle = Some(Duration::from_secs(1));
        for window in self.space.elements() {
            window.send_frame(output, now, throttle, |surface, data| {
                surface_primary_scanout_output(surface, data)
            });
            self.push_fractional_scale(window.toplevel().unwrap().wl_surface(), scale);
        }
        for layer_surface in layer_map_for_output(output).layers() {
            layer_surface.send_frame(output, now, throttle, |surface, data| {
                surface_primary_scanout_output(surface, data)
            });
            self.push_fractional_scale(layer_surface.wl_surface(), scale);
        }
    }

    /// Record the output each surface was presented on from the last render
    /// report; surfaces not presented (hidden windows) lose their record.
    pub fn update_surface_scanout(&mut self, output: &Output, states: &RenderElementStates) {
        for window in self.space.elements() {
            window.with_surfaces(|surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    data,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
        for layer_surface in layer_map_for_output(output).layers() {
            layer_surface.with_surfaces(|surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    data,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            with_surfaces_surface_tree(surface, |surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    data,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
        if let Some(icon) = &self.dnd_icon {
            with_surfaces_surface_tree(&icon.surface, |surface, data| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    data,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
    }

    /// Push the preferred fractional scale to `surface` (no-op for surfaces
    /// without the object); the module only sends the event when it changes.
    fn push_fractional_scale(&self, surface: &WlSurface, scale: f64) {
        if !surface.is_alive() {
            return;
        }
        with_states(surface, |states| {
            with_fractional_scale(states, |fractional_scale| {
                fractional_scale.set_preferred_scale(scale);
            });
        });
    }

    /// The topmost window currently in `Fullscreen` mode, if any. While one is
    /// active it is rendered above the top layer and keeps keyboard focus.
    pub fn active_fullscreen_window(&self) -> Option<Window> {
        self.space
            .elements()
            .rev()
            .find(|window| {
                window.toplevel().is_some_and(|toplevel| {
                    self.toplevels
                        .get(toplevel.wl_surface())
                        .is_some_and(|ws| ws.mode == WindowMode::Fullscreen)
                })
            })
            .cloned()
    }

    /// Prune bookkeeping for toplevels whose client went away without the
    /// `toplevel_destroyed` path (e.g. a crash); dropping the entry also drops
    /// its foreign-toplevel handle.
    pub fn cleanup_toplevels(&mut self) {
        self.toplevels
            .retain(|_, ws| ws.window.toplevel().unwrap().wl_surface().is_alive());
        for layout in self.layouts.values_mut() {
            layout.retain(|s| s.is_alive());
        }
    }

    /// The topmost window currently marked modal, if any. While open, input to
    /// every other window is blocked. Without the session xdg-dialog global the
    /// flag stays false.
    pub fn active_modal_window(&self) -> Option<Window> {
        self.space
            .elements()
            .rev()
            .find(|w| {
                w.toplevel().is_some_and(|toplevel| {
                    self.toplevels
                        .get(toplevel.wl_surface())
                        .is_some_and(|ws| ws.modal)
                })
            })
            .cloned()
    }

    /// Recompute keyboard focus from the layer-shell priority list and apply it
    /// if it changed. Called each frame from the backends' idle callbacks.
    pub fn update_keyboard_focus(&mut self) {
        if self.is_locked {
            return;
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        if keyboard.is_grabbed() {
            return;
        }
        let focus = self.compute_keyboard_focus();
        if keyboard.current_focus().as_ref() != focus.as_ref() {
            keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
        }
    }

    /// The keyboard focus target: the top-most `Exclusive` layer (Overlay/Top),
    /// else the opened/clicked `OnDemand` layer, else the active window.
    fn compute_keyboard_focus(&self) -> Option<WlSurface> {
        for kind in [Layer::Overlay, Layer::Top] {
            if let Some(surface) = self.topmost_exclusive_layer(kind) {
                return Some(surface);
            }
        }
        if let Some(surface) = self
            .layer_shell_on_demand_focus
            .clone()
            .filter(|s| self.is_mapped_on_demand_layer(s))
        {
            return Some(surface);
        }
        self.active_window
            .clone()
            .filter(|surface| surface.is_alive())
    }

    /// The top-most mapped layer on `kind` with `Exclusive` keyboard interactivity.
    fn topmost_exclusive_layer(&self, kind: Layer) -> Option<WlSurface> {
        self.space.outputs().find_map(|output| {
            layer_map_for_output(output)
                .layers_on(kind)
                .rev()
                .find(|layer| {
                    layer.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                })
                .map(|layer| layer.wl_surface().clone())
        })
    }

    /// Whether `surface` is still a mapped layer with `OnDemand` interactivity.
    fn is_mapped_on_demand_layer(&self, surface: &WlSurface) -> bool {
        if !surface.is_alive() {
            return false;
        }
        self.space.outputs().any(|output| {
            layer_map_for_output(output).layers().any(|layer| {
                layer.wl_surface() == surface
                    && layer.cached_state().keyboard_interactivity
                        == KeyboardInteractivity::OnDemand
            })
        })
    }
}

#[cfg(feature = "session")]
impl<B: Backend + 'static> State<B> {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        backend_data: B,
    ) -> Self {
        Self::new_with_socket(event_loop, display, backend_data, SocketName::Session)
    }

    pub fn update_idle_inhibit(&mut self) {
        self.session
            .idle_inhibiting_surfaces
            .retain(|surface| surface.is_alive());
        let inhibited = !self.session.idle_inhibiting_surfaces.is_empty();
        self.session.idle_notifier_state.set_is_inhibited(inhibited);
    }

    pub fn confirm_pending_lock(&mut self) {
        if self.lock_surfaces.iter().any(|s| s.alive())
            && let Some(locker) = self.session.pending_lock.take()
        {
            locker.lock();
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
