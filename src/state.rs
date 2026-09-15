use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use smithay::backend::renderer::element::{
    RenderElementStates, default_primary_scanout_output_compare,
};
use smithay::desktop::utils::{
    OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
    surface_primary_scanout_output, take_presentation_feedback_surface_tree,
    update_surface_primary_scanout_output, with_surfaces_surface_tree,
};
use smithay::desktop::{PopupManager, Space, Window, layer_map_for_output};
use smithay::input::keyboard::Keysym;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic,
};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, DisconnectReason, ObjectId,
};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Clock, Logical, Monotonic, Point, SERIAL_COUNTER, Time};
use smithay::wayland::commit_timing::{
    CommitTimerBarrierStateUserData, CommitTimingManagerState, Timestamp,
};
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, SurfaceData, with_states,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::fifo::{FifoBarrierCachedState, FifoManagerState};
use smithay::wayland::fractional_scale::{FractionalScaleManagerState, with_fractional_scale};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::presentation::{PresentationState, Refresh};
use smithay::wayland::session_lock::{LockSurface, SessionLocker};
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::viewporter::ViewporterState;

use smithay::wayland::foreign_toplevel_list::ForeignToplevelListState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::input_method::InputMethodManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::wlr_data_control::DataControlState;
use smithay::wayland::session_lock::SessionLockManagerState;
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::backend::Backend;
use crate::handlers::foreign_toplevel::ForeignToplevelManagerState;
use crate::handlers::output_power::OutputPowerManagerState;
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

/// Whether this output has queued or presented a lock frame for the current lock.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum LockFrame {
    #[default]
    None,
    Queued,
    Presented,
}

/// Session lock lifecycle. `is_locked()` is true for both `Locking` and `Locked`.
#[derive(Default)]
pub enum LockPhase {
    #[default]
    Unlocked,
    Locking(SessionLocker),
    Locked,
}

impl LockPhase {
    pub fn is_locked(&self) -> bool {
        !matches!(self, Self::Unlocked)
    }
}

fn output_lock_frame(output: &Output) -> LockFrame {
    output
        .user_data()
        .insert_if_missing(Cell::<LockFrame>::default);
    output.user_data().get::<Cell<LockFrame>>().unwrap().get()
}

fn set_output_lock_frame(output: &Output, frame: LockFrame) {
    output
        .user_data()
        .insert_if_missing(Cell::<LockFrame>::default);
    output
        .user_data()
        .get::<Cell<LockFrame>>()
        .unwrap()
        .set(frame);
}

pub struct State<BackendData: Backend + 'static> {
    pub socket_name: OsString,
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
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<State<BackendData>>,
    pub popups: PopupManager,
    pub xdg_activation_state: XdgActivationState,
    pub data_device_state: DataDeviceState,
    pub data_control_state: DataControlState,
    pub session_lock_state: SessionLockManagerState,
    pub foreign_toplevel: ForeignToplevelManagerState,
    pub foreign_toplevel_list: ForeignToplevelListState,
    pub idle_notifier_state: IdleNotifierState<State<BackendData>>,
    pub idle_inhibiting_surfaces: HashSet<WlSurface>,
    pub output_power: OutputPowerManagerState,

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
    pub lock_phase: LockPhase,
    pub lock_surfaces: Vec<LockSurface>,
    pub lock_surface_outputs: HashMap<ObjectId, Output>,
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

impl<BackendData: Backend + 'static> State<BackendData> {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        backend_data: BackendData,
    ) -> Self {
        let dh = display.handle();
        let clock = Clock::new();

        // The zwp_linux_dmabuf_v1 global is created lazily by each backend once
        // its renderer's format list is available. Dispatch is handled by the
        // blanket `delegate_dispatch2!`.
        let dmabuf_state = DmabufState::new();
        // `wp_commit_timing`: holds client commits until their requested time.
        CommitTimingManagerState::new::<Self>(&dh);
        // `wp_fifo`: holds commits until the previous frame was presented.
        FifoManagerState::new::<Self>(&dh);
        // `wp_presentation`: reports when a frame was actually presented.
        PresentationState::new::<Self>(&dh, clock.id() as u32);

        let seat_name = backend_data.seat_name();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [
                xdg_toplevel::WmCapabilities::Maximize,
                xdg_toplevel::WmCapabilities::Fullscreen,
            ],
        );
        XdgDecorationState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let space = Space::default();
        let popups = PopupManager::default();
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, seat_name);
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        let pointer = seat.add_pointer();

        ViewporterState::new::<Self>(&dh);
        FractionalScaleManagerState::new::<Self>(&dh);
        #[allow(clippy::mutable_key_type)] // `Output` is interior-mutable, but stable as a key.
        let layouts: HashMap<Output, Layout> = HashMap::new();
        CursorShapeManagerState::new::<Self>(&dh);
        let mut xdg_toplevel_icon = XdgToplevelIconManager::new::<Self>(&dh);
        xdg_toplevel_icon.add_icon_size(64);
        TextInputManagerState::new::<Self>(&dh);
        InputMethodManagerState::new::<Self, _>(&dh, |_client| true);
        VirtualKeyboardManagerState::new::<Self, _>(&dh, |_client| true);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, |_| true);
        let foreign_toplevel = ForeignToplevelManagerState::new::<Self>(&dh);
        let foreign_toplevel_list = ForeignToplevelListState::new::<Self>(&dh);
        XdgDialogState::new::<Self>(&dh);
        let idle_notifier_state = IdleNotifierState::new(&dh, event_loop.handle());
        IdleInhibitManagerState::new::<Self>(&dh);
        let data_control_state = DataControlState::new::<Self, _>(&dh, None, |_| true);
        let output_power = OutputPowerManagerState::new::<Self>(&dh);

        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();

        Self {
            socket_name,
            xdg_activation_state,
            data_device_state,
            session_lock_state,
            foreign_toplevel,
            foreign_toplevel_list,
            idle_notifier_state,
            data_control_state,
            output_power,
            idle_inhibiting_surfaces: HashSet::new(),
            display_handle: dh,
            space,
            toplevels: HashMap::new(),
            loop_signal,
            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            shm_state,
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
            lock_phase: LockPhase::Unlocked,
            lock_surfaces: Vec::new(),
            lock_surface_outputs: HashMap::new(),
            layouts,
            layer_shell_on_demand_focus: None,
            active_window: None,
        }
    }

    fn init_wayland_listener(display: Display<Self>, event_loop: &mut EventLoop<Self>) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().unwrap();
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

    pub fn is_locked(&self) -> bool {
        self.lock_phase.is_locked()
    }

    /// Queue a redraw on every output; the backend skips ones already pending.
    pub fn schedule_render(&mut self) {
        for output in self.space.outputs() {
            self.backend_data.schedule_render(output);
        }
    }

    /// Send frame callbacks for a presented frame; `now` is its presentation time.
    pub fn send_frame_callbacks(&mut self, output: &Output, now: Duration) {
        if self.is_locked() {
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

    pub fn lock_frame_queued(&mut self, output: &Output) {
        if self.is_locked() {
            set_output_lock_frame(output, LockFrame::Queued);
        }
    }

    pub fn lock_frame_presented(&mut self, output: &Output) {
        if output_lock_frame(output) != LockFrame::Queued {
            return;
        }
        set_output_lock_frame(output, LockFrame::Presented);
        self.maybe_send_locked();
    }

    /// Send `locked` if every on output has already presented.
    pub fn maybe_send_locked(&mut self) {
        let LockPhase::Locking(_) = self.lock_phase else {
            return;
        };
        let done = self
            .space
            .outputs()
            .filter(|output| !self.output_power.is_off(output))
            .all(|output| output_lock_frame(output) == LockFrame::Presented);
        if done
            && let LockPhase::Locking(locker) =
                std::mem::replace(&mut self.lock_phase, LockPhase::Locked)
        {
            locker.lock();
        }
    }

    pub fn reset_lock_frames(&self) {
        for output in self.space.outputs() {
            set_output_lock_frame(output, LockFrame::None);
        }
    }

    /// Visit every surface the compositor knows about (windows, layers, cursor,
    /// DnD icon, and lock surfaces).
    fn for_each_surface(&self, mut f: impl FnMut(&WlSurface, &SurfaceData)) {
        for window in self.space.elements() {
            window.with_surfaces(&mut f);
        }
        for output in self.space.outputs() {
            for layer in layer_map_for_output(output).layers() {
                layer.with_surfaces(&mut f);
            }
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            with_surfaces_surface_tree(surface, &mut f);
        }
        if let Some(icon) = &self.dnd_icon {
            with_surfaces_surface_tree(&icon.surface, &mut f);
        }
        for lock_surface in &self.lock_surfaces {
            with_surfaces_surface_tree(lock_surface.wl_surface(), &mut f);
        }
    }

    /// Visit the surfaces on `output`: its windows, its layers, and lock
    /// surfaces while locked.
    fn for_each_surface_on(&self, output: &Output, mut f: impl FnMut(&WlSurface, &SurfaceData)) {
        for window in self.space.elements_for_output(output) {
            window.with_surfaces(&mut f);
        }
        for layer in layer_map_for_output(output).layers() {
            layer.with_surfaces(&mut f);
        }
        if self.is_locked() {
            for lock_surface in self.lock_surfaces.iter().filter(|s| s.alive()) {
                with_surfaces_surface_tree(lock_surface.wl_surface(), &mut f);
            }
        }
    }

    /// Wake the backend after `delay` to release commit-timing barriers that are due.
    pub fn wake_at(&mut self, deadline: Timestamp) {
        let now = self.clock.now();
        self.backend_data
            .arm_commit_timer(Time::elapsed(&now, deadline.into()));
    }

    /// Arm a wakeup at the earliest pending commit-timing deadline, if any.
    pub fn reschedule_commit_timer(&mut self) {
        if let Some(deadline) = self.next_commit_deadline() {
            self.wake_at(deadline);
        }
    }

    /// Signal per-surface barriers, then resume the clients they were holding back.
    /// `signal` returns whether the surface had a barrier worth reporting. When
    /// `output` is set, only that output's surfaces are visited.
    fn release_blockers(
        &mut self,
        output: Option<&Output>,
        mut signal: impl FnMut(&WlSurface, &SurfaceData) -> bool,
    ) {
        let mut clients: HashMap<ClientId, Client> = HashMap::new();
        {
            let mut visit = |surface: &WlSurface, states: &SurfaceData| {
                if signal(surface, states)
                    && let Some(client) = surface.client()
                {
                    clients.insert(client.id(), client);
                }
            };
            match output {
                Some(output) => self.for_each_surface_on(output, &mut visit),
                None => self.for_each_surface(&mut visit),
            }
        }
        let dh = self.display_handle.clone();
        for client in clients.into_values() {
            self.client_compositor_state(&client)
                .blocker_cleared(self, &dh);
        }
    }

    /// Release commit-timing barriers whose requested time has passed.
    pub fn release_commit_timers(&mut self, deadline: impl Into<Timestamp>) {
        let deadline = deadline.into();
        self.release_blockers(None, |_surface, states| match states
            .data_map
            .get::<CommitTimerBarrierStateUserData>(
        ) {
            Some(barrier) => barrier.lock().unwrap().signal_until(deadline),
            None => false,
        });
    }

    /// The earliest commit-timing deadline still pending across all surfaces.
    pub fn next_commit_deadline(&self) -> Option<Timestamp> {
        let mut earliest: Option<Timestamp> = None;
        {
            let mut visit = |_surface: &WlSurface, states: &SurfaceData| {
                if let Some(barrier) = states.data_map.get::<CommitTimerBarrierStateUserData>()
                    && let Some(deadline) = barrier.lock().unwrap().next_deadline()
                {
                    earliest = Some(earliest.map_or(deadline, |earliest| earliest.min(deadline)));
                }
            };
            self.for_each_surface(&mut visit);
        }
        earliest
    }

    /// Signal pending `wp_fifo` barriers for surfaces on `output`.
    pub fn release_fifo_barriers(&mut self, output: &Output) {
        self.release_blockers(Some(output), |_surface, states| {
            states
                .cached_state
                .get::<FifoBarrierCachedState>()
                .current()
                .barrier
                .take()
                .map(|barrier| barrier.signal())
                .is_some()
        });
    }

    /// Drain the `wp_presentation` feedback committed for the frame just rendered.
    pub fn take_presentation_feedback(
        &self,
        output: &Output,
        states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);
        for window in self.space.elements_for_output(output) {
            window.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
        }
        for layer in layer_map_for_output(output).layers() {
            layer.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
        }
        for lock_surface in self.lock_surfaces.iter().filter(|s| s.alive()) {
            if self
                .lock_surface_outputs
                .get(&lock_surface.wl_surface().id())
                == Some(output)
            {
                take_presentation_feedback_surface_tree(
                    lock_surface.wl_surface(),
                    &mut feedback,
                    |_, _| Some(output.clone()),
                    |surface, _| {
                        surface_presentation_feedback_flags_from_states(surface, None, states)
                    },
                );
            }
        }
        feedback
    }

    /// Report a presented frame to its clients, at the real vblank time.
    pub fn send_presentation_feedback(
        &self,
        output: &Output,
        mut feedback: OutputPresentationFeedback,
        vblank: Option<Time<Monotonic>>,
        now: Time<Monotonic>,
        seq: u64,
    ) {
        let (presented, flags) = match vblank {
            Some(vblank) => (
                vblank,
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            ),
            None => (now, wp_presentation_feedback::Kind::Vsync),
        };
        let refresh = crate::backend::output_refresh(output);
        feedback.presented(
            presented,
            refresh.map(Refresh::fixed).unwrap_or(Refresh::Unknown),
            seq,
            flags,
        );
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
        let before = self.toplevels.len();
        self.toplevels
            .retain(|_, ws| ws.window.toplevel().unwrap().wl_surface().is_alive());
        for layout in self.layouts.values_mut() {
            layout.retain(|s| s.is_alive());
        }
        if self.toplevels.len() != before {
            self.schedule_render();
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
        if self.is_locked() {
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

    pub fn update_idle_inhibit(&mut self) {
        self.idle_inhibiting_surfaces
            .retain(|surface| surface.is_alive());
        let inhibited = !self.idle_inhibiting_surfaces.is_empty();
        self.idle_notifier_state.set_is_inhibited(inhibited);
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
