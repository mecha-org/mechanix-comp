use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::{GbmFramebufferExporter, NodeFilter};
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, NodeType,
};
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu};
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Scale};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{Device as ControlDevice, ModeTypeFlags, connector, crtc};
use smithay::reexports::input::AccelProfile;
use smithay::reexports::input::{DeviceCapability, Libinput};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{DeviceFd, IsAlive, Monotonic, Time};
use smithay::wayland::compositor::with_states;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{error, info, warn};

use crate::backend::{Backend, Wakeups, output_refresh};
use crate::drawing::{PointerElement, cached_pointer_buffer};
use crate::render::{Element, OutputElements, output_elements};
use crate::state::State;

// Scanout framebuffer formats to try, most preferred first. 8-bit only keeps
// things simple and is universally supported.
const SUPPORTED_FORMATS: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Xrgb8888];

/// Concrete `DrmOutput` type: GBM allocator + framebuffer exporter, presentation
/// feedback as per-frame user data, backed by a `DrmDeviceFd`.
type GbmDrmOutput = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    OutputPresentationFeedback,
    DrmDeviceFd,
>;
type GbmDrmOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    OutputPresentationFeedback,
    DrmDeviceFd,
>;

/// Identifies which physical output a smithay `Output` belongs to, stored in the
/// output's user data.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct OutputKey {
    node: DrmNode,
    crtc: crtc::Handle,
}

/// Per-CRTC scanout state.
struct CrtcOutput {
    global: GlobalId,
    drm_output: GbmDrmOutput,
}

/// Per-DRM-device state.
struct DeviceData {
    drm_output_manager: GbmDrmOutputManager,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, CrtcOutput>,
    registration_token: RegistrationToken,
}

#[derive(Default)]
enum RepaintState {
    #[default]
    Idle,
    RenderQueued,
    AwaitingVblank {
        /// New damage arrived while the frame was scanning out.
        damage_pending: bool,
    },
}

#[derive(Default)]
struct Repaint {
    state: RepaintState,
    /// When frame callbacks were last sent (pacing).
    last_ack: Option<Time<Monotonic>>,
    /// Pending paced redraw, if any.
    render_wake: Option<RegistrationToken>,
}

pub struct UdevData {
    login: LibSeatSession,
    loop_handle: LoopHandle<'static, State<UdevData>>,
    /// Shared GLES renderer, created on the first KMS card.
    renderer: Option<GlesRenderer>,
    /// Card `renderer` was created on; its removal clears the renderer.
    renderer_node: Option<DrmNode>,
    devices: HashMap<DrmNode, DeviceData>,
    keyboards: Vec<smithay::reexports::input::Device>,
    /// Connected pointer devices; the software cursor renders only while non-empty.
    pointers: Vec<smithay::reexports::input::Device>,
    /// True once a pointer device has actually moved; a phantom pointer (e.g.
    /// the HDMI controller) must not summon a cursor stuck at (0,0).
    pointer_moved: bool,
    /// Loaded xcursor theme used to pick the current cursor frame.
    pointer_image: crate::cursor::Cursor,
    /// Cache of imported cursor frames, keyed by the raw xcursor image.
    pointer_images: Vec<(xcursor::parser::Image, MemoryRenderBuffer)>,
    pointer_element: PointerElement,
    /// Per-output repaint state, keyed by (device, CRTC).
    repaints: HashMap<(DrmNode, crtc::Handle), Repaint>,
    /// Pending `wp_commit_timing` wakeups.
    wakeups: Wakeups<UdevData>,
    /// True while the session is paused; rendering is skipped.
    paused: bool,
}

impl Backend for UdevData {
    fn renderer(&mut self) -> &mut GlesRenderer {
        self.renderer
            .as_mut()
            .expect("KMS renderer not initialized")
    }

    fn seat_name(&self) -> String {
        self.login.seat()
    }

    fn reset_buffers(&mut self, output: &Output) {
        if let Some(id) = output.user_data().get::<OutputKey>()
            && let Some(device) = self.devices.get_mut(&id.node)
            && let Some(surface) = device.surfaces.get_mut(&id.crtc)
        {
            surface.drm_output.reset_buffers();
        }
    }

    fn change_vt(&mut self, vt: i32) {
        info!(to = vt, "Trying to switch vt");
        if let Err(err) = self.login.change_vt(vt) {
            error!(vt, "Error switching vt: {}", err);
        }
    }

    fn output_power_supported(&self, output: &Output) -> bool {
        output.user_data().get::<OutputKey>().is_some()
    }

    fn set_output_dpms(&mut self, output: &Output, on: bool) -> bool {
        let Some(id) = output.user_data().get::<OutputKey>().copied() else {
            return false;
        };
        let Some(surface) = self
            .devices
            .get_mut(&id.node)
            .and_then(|d| d.surfaces.get_mut(&id.crtc))
        else {
            return false;
        };
        if on {
            surface.drm_output.reset_buffers();
        } else {
            if let Err(err) = surface.drm_output.with_compositor(|c| c.clear()) {
                warn!("DPMS off failed on {}: {err}", output.name());
                return false;
            }
        }
        // Turning the CRTC off/on cancels any queued frame; nothing will vblank.
        let key = (id.node, id.crtc);
        if let Some(frame) = self.repaints.get_mut(&key) {
            frame.state = RepaintState::Idle;
        }
        true
    }

    fn prepare_resume(&mut self) {
        for (node, device) in &mut self.devices {
            if let Err(err) = device.drm_output_manager.lock().activate(false) {
                warn!("Failed to activate DRM device {node} after resume: {err}");
            }
        }
        for frame in self.repaints.values_mut() {
            frame.state = RepaintState::Idle;
        }
    }

    fn schedule_render(&mut self, output: &Output) {
        let Some(id) = output.user_data().get::<OutputKey>().copied() else {
            return;
        };
        let key = (id.node, id.crtc);
        let frame = self.repaints.entry(key).or_default();
        // A real render supersedes any pending paced redraw.
        let paced_redraw = if matches!(frame.state, RepaintState::Idle) {
            frame.render_wake.take()
        } else {
            None
        };
        if let Some(token) = paced_redraw {
            self.loop_handle.remove(token);
        }
        match &mut frame.state {
            state @ RepaintState::Idle => {
                *state = RepaintState::RenderQueued;
                let (node, crtc) = key;
                self.loop_handle.insert_idle(move |state| {
                    if let Some(frame) = state.backend_data.repaints.get_mut(&key) {
                        frame.state = RepaintState::Idle;
                    }
                    state.render_surface(node, crtc);
                });
            }
            RepaintState::AwaitingVblank { damage_pending } => {
                *damage_pending = true;
            }
            RepaintState::RenderQueued => {}
        }
    }

    fn arm_commit_timer(&mut self, delay: Duration) {
        self.wakeups.arm_commit(delay);
    }

    fn schedule_render_after(&mut self, output: &Output, delay: Duration) {
        let Some(id) = output.user_data().get::<OutputKey>().copied() else {
            return;
        };
        let key = (id.node, id.crtc);
        let frame = self.repaints.entry(key).or_default();
        if let Some(token) = frame.render_wake.take() {
            self.loop_handle.remove(token);
        }
        let token =
            self.loop_handle
                .insert_source(Timer::from_duration(delay), move |_, _, state| {
                    if let Some(frame) = state.backend_data.repaints.get_mut(&key) {
                        frame.render_wake = None;
                    }
                    if let Some(output) = state.output_for_crtc(id.node, id.crtc) {
                        state.backend_data.schedule_render(&output);
                    }
                    TimeoutAction::Drop
                });
        if let Ok(token) = token {
            frame.render_wake = Some(token);
        }
    }
}

impl UdevData {
    fn cancel_queued_frames(&mut self) {
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                if let Err(err) = surface.drm_output.with_compositor(|c| c.clear()) {
                    warn!("Cancelling queued frame failed: {err}");
                }
            }
        }
    }
}

/// KMS-capable = has modeset resources; render-only cards (etnaviv) don't.
fn is_kms_card(fd: &DrmDeviceFd) -> bool {
    fd.resource_handles().is_ok_and(|res| {
        !res.crtcs().is_empty() && !res.connectors().is_empty() && !res.encoders().is_empty()
    })
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, State<UdevData>> = EventLoop::try_new()?;
    let display: Display<State<UdevData>> = Display::new()?;

    let (session, notifier) = LibSeatSession::new()?;
    let udev_backend = UdevBackend::new(session.seat())?;

    let loop_handle = event_loop.handle();
    let udev_data = UdevData {
        login: session,
        loop_handle: loop_handle.clone(),
        renderer: None,
        renderer_node: None,
        devices: HashMap::new(),
        keyboards: Vec::new(),
        pointers: Vec::new(),
        pointer_moved: false,
        pointer_image: crate::cursor::Cursor::load(),
        pointer_images: Vec::new(),
        pointer_element: PointerElement::default(),
        repaints: HashMap::new(),
        wakeups: Wakeups::new(loop_handle.clone()),
        paused: false,
    };

    let mut state = State::new(&mut event_loop, display, udev_data);

    // Initialize libinput backend
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        state.backend_data.login.clone().into(),
    );
    libinput_context
        .udev_assign_seat(state.seat.name())
        .unwrap();
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    // Bind all our objects that get driven by the event loop
    event_loop
        .handle()
        .insert_source(libinput_backend, move |mut event, _, data| {
            if let InputEvent::DeviceAdded { device } = &mut event {
                if device.has_capability(DeviceCapability::Keyboard) {
                    if let Some(led_state) = data
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.led_state())
                    {
                        device.led_update(led_state.into());
                    }
                    data.backend_data.keyboards.push(device.clone());
                }
                if device.has_capability(DeviceCapability::Pointer) {
                    // Mice report raw 1:1 deltas by default; a high-DPI gaming
                    // mouse then outruns the screen in milliseconds. Adaptive
                    // acceleration (resolution-aware) keeps any pointer usable.
                    if device.config_accel_is_available() {
                        let _ = device.config_accel_set_profile(AccelProfile::Adaptive);
                        let _ = device.config_accel_set_speed(0.0);
                    }
                    data.backend_data.pointers.push(device.clone());
                    data.backend_data.pointer_moved = false;
                }
            } else if let InputEvent::DeviceRemoved { ref device } = event {
                if device.has_capability(DeviceCapability::Keyboard) {
                    data.backend_data.keyboards.retain(|item| item != device);
                }
                if device.has_capability(DeviceCapability::Pointer) {
                    data.backend_data.pointers.retain(|item| item != device);
                    if data.backend_data.pointers.is_empty() {
                        data.backend_data.pointer_moved = false;
                    }
                }
            }

            // A real pointer moving is what summons the cursor; touch and the
            // phantom HDMI "pointer" never move it.
            if matches!(
                event,
                InputEvent::PointerMotion { .. } | InputEvent::PointerMotionAbsolute { .. }
            ) {
                data.backend_data.pointer_moved = true;
            }

            data.process_input_event(event)
        })
        .unwrap();

    // Enumerate cards with smithay's discovery, preferring its primary GPU
    // (boot_vga, else a render node). The first KMS-capable card becomes primary.
    let seat = state.backend_data.login.seat();
    let mut candidates = all_gpus(&seat)?;
    if let Some(primary) = primary_gpu(&seat).ok().flatten() {
        candidates.sort_by_key(|path| path != &primary);
    }
    for path in candidates {
        let Ok(node) = DrmNode::from_path(&path) else {
            continue;
        };
        if let Err(err) = state.device_added(node, &path) {
            error!("Failed to add DRM device {}: {err}", path.display());
        }
    }
    if state.backend_data.renderer.is_none() {
        return Err("no KMS-capable DRM device found".into());
    }

    // Session pause/resume across VT switches.
    event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), state| match event {
            SessionEvent::PauseSession => {
                info!("session paused");
                state.backend_data.paused = true;
                state.backend_data.cancel_queued_frames();
                for device in state.backend_data.devices.values_mut() {
                    device.drm_output_manager.pause();
                }
                for frame in state.backend_data.repaints.values_mut() {
                    frame.state = RepaintState::default();
                }
            }
            SessionEvent::ActivateSession => {
                info!("session resumed");
                state.backend_data.paused = false;
                state.resume_drm_session();
            }
        })?;

    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Added { device_id, path } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id)
                    && let Err(err) = state.device_added(node, &path)
                {
                    error!("Failed to add device {device_id}: {err}");
                }
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    state.device_changed(node);
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    state.device_removed(node);
                }
            }
        })?;

    println!(
        "Compositor listening on Wayland socket: {:?}",
        state.socket_name
    );

    event_loop.run(None, &mut state, move |state| {
        state.refresh_space();
        state.foreign_toplevel_refresh();
        state.update_idle_inhibit();
        let _ = state.display_handle.flush_clients();
    })?;

    Ok(())
}

impl State<UdevData> {
    fn device_added(
        &mut self,
        node: DrmNode,
        path: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if node.ty() != NodeType::Primary {
            return Ok(());
        }
        if self.backend_data.devices.contains_key(&node) {
            return Ok(());
        }

        let fd = self.backend_data.login.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        if !is_kms_card(&fd) {
            info!("Ignoring {node}: not a KMS device");
            return Ok(());
        }

        let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;

        // Build the shared renderer on the first usable card. If EGL fails or is
        // software, skip this card so a later KMS card can take over.
        let render_formats = if let Some(renderer) = self.backend_data.renderer.as_mut() {
            renderer.egl_context().dmabuf_render_formats().clone()
        } else {
            let egl_display = match unsafe { EGLDisplay::new(gbm.clone()) } {
                Ok(display) => display,
                Err(err) => {
                    warn!("Ignoring {node}: EGL init failed: {err}");
                    return Ok(());
                }
            };
            let software = EGLDevice::device_for_display(&egl_display)
                .ok()
                .is_some_and(|device| device.is_software());
            if software {
                warn!("Ignoring {node}: software EGL renderer");
                return Ok(());
            }
            let egl_context = match EGLContext::new(&egl_display) {
                Ok(context) => context,
                Err(err) => {
                    warn!("Ignoring {node}: EGL context failed: {err}");
                    return Ok(());
                }
            };
            let render_formats = egl_context.dmabuf_render_formats().clone();
            let renderer = match unsafe { GlesRenderer::new(egl_context) } {
                Ok(renderer) => renderer,
                Err(err) => {
                    warn!("Ignoring {node}: renderer init failed: {err}");
                    return Ok(());
                }
            };

            // Ask EGL for the render node to advertise (kmsro: not the card node).
            if self.dmabuf_global.is_none() {
                let dmabuf_formats = renderer.dmabuf_formats();

                let render_node = super::egl_render_node(&egl_display)
                    .or_else(|| node.node_with_type(NodeType::Render).and_then(|r| r.ok()));

                let main_device_id = render_node
                    .map(|n| n.dev_id())
                    .unwrap_or_else(|| node.dev_id());

                let default_feedback = smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(
                    main_device_id,
                    dmabuf_formats,
                )
                .build()
                .unwrap();

                let global = self
                    .dmabuf_state
                    .create_global_with_default_feedback::<State<UdevData>>(
                        &self.display_handle,
                        &default_feedback,
                    );
                self.dmabuf_global = Some(global);
            }
            info!("Using {node} as the display device");
            self.backend_data.renderer = Some(renderer);
            self.backend_data.renderer_node = Some(node);
            render_formats
        };

        // Route vblank events for this device to frame_finish.
        let registration_token = self.backend_data.loop_handle.insert_source(
            drm_notifier,
            move |event, meta, state: &mut State<UdevData>| match event {
                DrmEvent::VBlank(crtc) => state.frame_finish(node, crtc, meta),
                DrmEvent::Error(err) => error!("DRM error: {err}"),
            },
        )?;

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), NodeFilter::All);

        let drm_output_manager = GbmDrmOutputManager::new(
            drm,
            allocator,
            exporter,
            Some(gbm),
            SUPPORTED_FORMATS.iter().copied(),
            render_formats.iter().copied(),
        );

        self.backend_data.devices.insert(
            node,
            DeviceData {
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                surfaces: HashMap::new(),
                registration_token,
            },
        );

        self.device_changed(node);
        Ok(())
    }

    fn device_changed(&mut self, node: DrmNode) {
        let Some(device) = self.backend_data.devices.get_mut(&node) else {
            return;
        };

        let scan_result = match device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device())
        {
            Ok(result) => result,
            Err(err) => {
                warn!("Failed to scan connectors on {node}: {err}");
                return;
            }
        };

        for event in scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(node, connector, crtc),
                DrmScanEvent::Disconnected {
                    crtc: Some(crtc), ..
                } => self.connector_disconnected(node, crtc),
                _ => {}
            }
        }
    }

    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.backend_data.devices.get_mut(&node) else {
            return;
        };
        let Some(renderer) = self.backend_data.renderer.as_mut() else {
            return;
        };

        let mode_id = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);
        let Some(&drm_mode) = connector.modes().get(mode_id) else {
            warn!("Connector has no modes");
            return;
        };
        let wl_mode = WlMode::from(drm_mode);

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            output_name,
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make: "Unknown".into(),
                model: "Unknown".into(),
                serial_number: "Unknown".into(),
            },
        );
        output.set_preferred(wl_mode);
        // Auto-detect the scale from the panel's physical size unless overridden.
        let scale = crate::backend::env_scale().unwrap_or_else(|| {
            crate::backend::snap_scale(crate::backend::guess_default_scale(
                connector.size(),
                wl_mode.size,
            ))
        });
        // Extend: place each output to the right of the existing ones.
        let x = self
            .space
            .outputs()
            .map(|output| self.space.output_geometry(output).unwrap().size.w)
            .sum::<i32>();
        let position = (x, 0).into();
        output.change_current_state(
            Some(wl_mode),
            None,
            Some(Scale::Fractional(scale)),
            Some(position),
        );
        output
            .user_data()
            .insert_if_missing(|| OutputKey { node, crtc });

        let global = output.create_global::<State<UdevData>>(&self.display_handle);
        self.space.map_output(&output, position);

        let drm_output = match device
            .drm_output_manager
            .lock()
            .initialize_output::<GlesRenderer, Element>(
                crtc,
                drm_mode,
                &[connector.handle()],
                &output,
                None,
                renderer,
                &DrmOutputRenderElements::default(),
            ) {
            Ok(drm_output) => drm_output,
            Err(err) => {
                warn!("Failed to initialize DRM output: {err}");
                self.display_handle.remove_global::<State<UdevData>>(global);
                self.space.unmap_output(&output);
                return;
            }
        };

        device
            .surfaces
            .insert(crtc, CrtcOutput { global, drm_output });

        // Kick off the first render.
        self.backend_data.schedule_render(&output);
    }

    fn connector_disconnected(&mut self, node: DrmNode, crtc: crtc::Handle) {
        if let Some(frame) = self.backend_data.repaints.remove(&(node, crtc))
            && let Some(token) = frame.render_wake
        {
            self.backend_data.loop_handle.remove(token);
        }
        let Some(device) = self.backend_data.devices.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.remove(&crtc) else {
            return;
        };
        self.display_handle
            .remove_global::<State<UdevData>>(surface.global);
        if let Some(output) = self.output_for_crtc(node, crtc) {
            self.release_fifo_barriers(&output);
            self.session.output_power.output_removed(&output);
            self.space.unmap_output(&output);
            self.maybe_send_locked();
        }
    }

    fn device_removed(&mut self, node: DrmNode) {
        let crtcs: Vec<crtc::Handle> = match self.backend_data.devices.get(&node) {
            Some(device) => device.surfaces.keys().copied().collect(),
            None => return,
        };
        for crtc in crtcs {
            self.connector_disconnected(node, crtc);
        }
        if let Some(device) = self.backend_data.devices.remove(&node) {
            self.backend_data
                .loop_handle
                .remove(device.registration_token);
        }
        // The renderer's EGL device is gone; drop it so a later KMS card recreates it.
        if self.backend_data.renderer_node == Some(node) {
            info!("Renderer card {node} removed");
            self.backend_data.renderer = None;
            self.backend_data.renderer_node = None;
            if let Some(global) = self.dmabuf_global.take() {
                self.dmabuf_state
                    .destroy_global::<State<UdevData>>(&self.display_handle, global);
            }
        }
    }

    /// Render one CRTC: queue a pageflip on damage, deliver frame callbacks otherwise.
    fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(output) = self.output_for_crtc(node, crtc) else {
            return;
        };

        if self.session.output_power.is_off(&output) {
            return;
        }
        if self.backend_data.paused {
            return;
        }

        let Some(refresh) = output_refresh(&output) else {
            return;
        };

        let visible = self.visible_surfaces(&output);
        let locked = self.is_locked();

        let mut queued = false;
        let mut render_failed = false;
        let mut cursor_animated = false;
        let mut scanout_states: Option<RenderElementStates> = None;
        {
            let Some(renderer) = self.backend_data.renderer.as_mut() else {
                return;
            };
            let Some(device) = self.backend_data.devices.get_mut(&node) else {
                return;
            };
            let Some(surface) = device.surfaces.get_mut(&crtc) else {
                return;
            };

            let (elements, clear_color) = {
                // Build the pointer cursor element (anvil-style), rendering it
                // above everything else when the pointer is over this output.
                let output_geometry = self.space.output_geometry(&output).unwrap();
                let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
                let pointer_location = self.pointer.current_location();

                let mut custom_elements: Vec<Element> = Vec::new();
                // Render the cursor only after a real pointer has moved; the
                // touch-only panel (and phantom devices) never summon it.
                let pointer_present =
                    self.backend_data.pointer_moved && !self.backend_data.pointers.is_empty();
                if pointer_present && output_geometry.to_f64().contains(pointer_location) {
                    let cursor_hotspot =
                        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
                            with_states(surface, |states| {
                                states
                                    .data_map
                                    .get::<Mutex<CursorImageAttributes>>()
                                    .unwrap()
                                    .lock()
                                    .unwrap()
                                    .hotspot
                            })
                        } else {
                            (0, 0).into()
                        };
                    let cursor_pos = pointer_location - output_geometry.loc.to_f64();

                    let cursor_scale =
                        output.current_scale().fractional_scale().round().max(1.0) as u32;
                    let frame = self
                        .backend_data
                        .pointer_image
                        .get_image(cursor_scale, self.clock.now().into());
                    let pointer_image = cached_pointer_buffer(
                        &mut self.backend_data.pointer_images,
                        frame,
                        cursor_scale as i32,
                    );
                    self.backend_data.pointer_element.set_buffer(pointer_image);
                    if matches!(&self.cursor_status, CursorImageStatus::Surface(s) if !s.is_alive())
                    {
                        self.cursor_status = CursorImageStatus::default_named();
                    }
                    self.backend_data
                        .pointer_element
                        .set_status(self.cursor_status.clone());
                    cursor_animated = matches!(self.cursor_status, CursorImageStatus::Named(_))
                        && self.backend_data.pointer_image.is_animated(cursor_scale);

                    custom_elements.extend(
                        self.backend_data.pointer_element.render_elements(
                            renderer,
                            (cursor_pos - cursor_hotspot.to_f64())
                                .to_physical(scale)
                                .to_i32_round(),
                            scale,
                            1.0,
                        ),
                    );

                    // Draw the dnd icon if applicable.
                    if let Some(icon) = self.dnd_icon.as_ref() {
                        let dnd_icon_pos = (cursor_pos + icon.offset.to_f64())
                            .to_physical(scale)
                            .to_i32_round();
                        if icon.surface.alive() {
                            custom_elements.extend(
                                render_elements_from_surface_tree::<
                                    GlesRenderer,
                                    WaylandSurfaceRenderElement<GlesRenderer>,
                                >(
                                    renderer,
                                    &icon.surface,
                                    dnd_icon_pos,
                                    scale,
                                    1.0,
                                    Kind::Unspecified,
                                )
                                .into_iter()
                                .map(OutputElements::Surface),
                            );
                        }
                    }
                }

                output_elements(
                    renderer,
                    &self.space,
                    &output,
                    locked,
                    &self.lock_surfaces,
                    &self.toplevels,
                    &visible,
                    custom_elements,
                )
            };

            let result = match surface.drm_output.render_frame(
                renderer,
                &elements,
                clear_color,
                FrameFlags::DEFAULT,
            ) {
                Ok(result) if !result.is_empty => Some(result.states),
                Ok(_) => None,
                Err(err) => {
                    warn!("Rendering failed: {err}");
                    render_failed = true;
                    None
                }
            };

            if let Some(states) = result {
                scanout_states = Some(states);
            }
        }

        if let Some(states) = scanout_states.take() {
            self.update_surface_scanout(&output, &states);
            let feedback = self.take_presentation_feedback(&output);
            let queue_result = self
                .backend_data
                .devices
                .get_mut(&node)
                .and_then(|device| device.surfaces.get_mut(&crtc))
                .map(|surface| surface.drm_output.queue_frame(feedback));
            match queue_result {
                Some(Ok(())) => queued = true,
                Some(Err(err)) => {
                    warn!("Failed to queue frame: {err}");
                    render_failed = true;
                }
                None => return,
            }
        }

        if queued {
            self.lock_frame_queued(&output);
            // The frame is on the CRTC; damage arriving before its vblank re-renders.
            if let Some(frame) = self.backend_data.repaints.get_mut(&(node, crtc)) {
                frame.state = RepaintState::AwaitingVblank {
                    damage_pending: false,
                };
            }
        } else if render_failed {
            // Retry a failed render at the next refresh.
            self.backend_data.schedule_render_after(&output, refresh);
        } else {
            // Nothing to present. Deliver frame callbacks at most once per
            // refresh so a client cannot redraw faster than the output.
            let now = self.clock.now();
            let deadline = self
                .backend_data
                .repaints
                .get(&(node, crtc))
                .and_then(|frame| frame.last_ack)
                .map(|last| last + refresh);
            match deadline {
                Some(deadline) if now < deadline => {
                    self.backend_data.schedule_render_after(
                        &output,
                        Duration::from(deadline) - Duration::from(now),
                    );
                }
                _ => {
                    if let Some(frame) = self.backend_data.repaints.get_mut(&(node, crtc)) {
                        frame.last_ack = Some(now);
                    }
                    self.send_frame_callbacks(&output, Duration::from(now));
                }
            }
        }

        if cursor_animated && !self.is_locked() {
            self.backend_data.schedule_render_after(&output, refresh);
        }
    }

    /// Vblank: retire the frame, notify clients, re-render if damage landed meanwhile.
    fn frame_finish(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        meta: &mut Option<DrmEventMetadata>,
    ) {
        let submitted = {
            let Some(device) = self.backend_data.devices.get_mut(&node) else {
                return;
            };
            let Some(surface) = device.surfaces.get_mut(&crtc) else {
                return;
            };
            match surface.drm_output.frame_submitted() {
                Ok(Some(feedback)) => Some(feedback),
                Ok(None) => None,
                Err(err) => {
                    warn!("frame_submitted failed: {err}");
                    None
                }
            }
        };
        let Some(submitted) = submitted else {
            // No frame was retired; let the next damage re-render.
            if let Some(frame) = self.backend_data.repaints.get_mut(&(node, crtc)) {
                frame.state = RepaintState::Idle;
            }
            return;
        };

        let now = self.clock.now();
        let vblank = meta.as_ref().and_then(|meta| match meta.time {
            DrmEventTime::Monotonic(time) if !time.is_zero() => Some(Time::from(time)),
            _ => None,
        });
        let seq = meta.as_ref().map(|meta| meta.sequence as u64).unwrap_or(0);

        let render_again = self
            .backend_data
            .repaints
            .get_mut(&(node, crtc))
            .is_some_and(|frame| {
                frame.last_ack = Some(vblank.unwrap_or(now));
                let damage = matches!(
                    frame.state,
                    RepaintState::AwaitingVblank {
                        damage_pending: true
                    }
                );
                frame.state = RepaintState::Idle;
                damage
            });

        if let Some(output) = self.output_for_crtc(node, crtc) {
            self.lock_frame_presented(&output);
            // Ack with the vblank time so client frame-callback clocks match it.
            self.send_frame_callbacks(&output, Duration::from(vblank.unwrap_or(now)));
            self.send_presentation_feedback(&output, submitted, vblank, now, seq);
            // Render before releasing FIFO waiters so a resumed commit only marks damage.
            if render_again {
                self.render_surface(node, crtc);
            }
            self.release_fifo_barriers(&output);
        }
    }

    fn output_for_crtc(&self, node: DrmNode, crtc: crtc::Handle) -> Option<Output> {
        let key = OutputKey { node, crtc };
        self.space
            .outputs()
            .find(|output| output.user_data().get() == Some(&key))
            .cloned()
    }
}
