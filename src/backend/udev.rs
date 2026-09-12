use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::{GbmFramebufferExporter, NodeFilter};
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
};
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent, primary_gpu};
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Scale};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{ModeTypeFlags, connector, crtc};
use smithay::reexports::input::AccelProfile;
use smithay::reexports::input::{DeviceCapability, Libinput};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{DeviceFd, IsAlive, Transform};
use smithay::wayland::compositor::with_states;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{error, info, warn};

use crate::backend::Backend;
use crate::drawing::PointerElement;
use crate::render::{Element, OutputElements, output_elements};
use crate::state::State;

// Scanout framebuffer formats to try, most preferred first. 8-bit only keeps
// things simple and is universally supported.
const SUPPORTED_FORMATS: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Xrgb8888];

/// Concrete `DrmOutput` type: GBM allocator + framebuffer exporter, no per-frame
/// user data (`()`), backed by a `DrmDeviceFd`.
type GbmDrmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;
type GbmDrmOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// Identifies which physical output a smithay `Output` belongs to, stored in the
/// output's user data.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct UdevOutputId {
    device_id: DrmNode,
    crtc: crtc::Handle,
}

/// Per-CRTC scanout state.
struct SurfaceData {
    global: Option<GlobalId>,
    drm_output: GbmDrmOutput,
}

/// Per-DRM-device state.
struct DeviceData {
    drm_output_manager: GbmDrmOutputManager,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    registration_token: RegistrationToken,
}

pub struct UdevData {
    login: LibSeatSession,
    loop_handle: LoopHandle<'static, State<UdevData>>,
    primary_gpu: DrmNode,
    /// Single GLES renderer bound to the primary GPU. Created when the primary
    /// device is added.
    renderer: Option<GlesRenderer>,
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
    /// CRTCs with a render already queued, so `schedule_render` on them is a no-op.
    pending_render: HashSet<(DrmNode, crtc::Handle)>,
}

impl Backend for UdevData {
    fn renderer(&mut self) -> &mut GlesRenderer {
        self.renderer
            .as_mut()
            .expect("primary GPU renderer not initialized")
    }

    fn seat_name(&self) -> String {
        self.login.seat()
    }

    fn reset_buffers(&mut self, output: &Output) {
        if let Some(id) = output.user_data().get::<UdevOutputId>()
            && let Some(device) = self.devices.get_mut(&id.device_id)
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
        output.user_data().get::<UdevOutputId>().is_some()
    }

    fn set_output_dpms(&mut self, output: &Output, on: bool) -> bool {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return false;
        };
        let Some(device) = self.devices.get_mut(&id.device_id) else {
            return false;
        };
        let Some(surface) = device.surfaces.get_mut(&id.crtc) else {
            return false;
        };
        if on {
            // clear() left the CRTC inactive; the next queue_frame must modeset.
            surface.drm_output.reset_buffers();
            return true;
        }
        match surface.drm_output.with_compositor(|c| c.clear()) {
            Ok(()) => true,
            Err(err) => {
                warn!("DPMS off failed on {}: {err}", output.name());
                false
            }
        }
    }

    fn prepare_resume(&mut self) {
        for (node, device) in &mut self.devices {
            if let Err(err) = device.drm_output_manager.lock().activate(false) {
                warn!("Failed to activate DRM device {node} after resume: {err}");
            }
        }
    }

    fn schedule_render(&mut self, output: &Output) {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return;
        };
        if !self.pending_render.insert((id.device_id, id.crtc)) {
            return;
        }
        self.loop_handle.insert_idle(move |state| {
            state
                .backend_data
                .pending_render
                .remove(&(id.device_id, id.crtc));
            state.render_surface(id.device_id, id.crtc);
        });
    }
}

/// Take over the session, open the primary GPU, light up its first connected
/// connector, and run the event loop to completion.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, State<UdevData>> = EventLoop::try_new()?;
    let display: Display<State<UdevData>> = Display::new()?;

    let (session, notifier) = LibSeatSession::new()?;

    // Pick the primary GPU and normalize to its primary (card) node, which is
    // the one that carries KMS/modesetting.
    let primary_path = if let Ok(custom_dev) =
        std::env::var("MECHA_DRM_DEVICE").or_else(|_| std::env::var("WLR_DRM_DEVICES"))
    {
        let p = std::path::PathBuf::from(&custom_dev);
        if p.exists() {
            p
        } else {
            let candidate = std::path::PathBuf::from(format!("/dev/dri/{custom_dev}"));
            if candidate.exists() { candidate } else { p }
        }
    } else {
        primary_gpu(&session.seat())?.ok_or("no GPU found for seat")?
    };
    let primary_node = DrmNode::from_path(&primary_path)?;
    let primary_gpu = primary_node
        .node_with_type(NodeType::Primary)
        .and_then(|n| n.ok())
        .unwrap_or(primary_node);
    info!("Using {primary_gpu} ({primary_path:?}) as primary GPU");

    let loop_handle = event_loop.handle();
    let udev_data = UdevData {
        login: session,
        loop_handle: loop_handle.clone(),
        primary_gpu,
        renderer: None,
        devices: HashMap::new(),
        keyboards: Vec::new(),
        pointers: Vec::new(),
        pointer_moved: false,
        pointer_image: crate::cursor::Cursor::load(),
        pointer_images: Vec::new(),
        pointer_element: PointerElement::default(),
        pending_render: HashSet::new(),
    };

    let mut state = State::new(&mut event_loop, display, udev_data);

    let udev_backend = UdevBackend::new(state.seat.name())?;
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

    // Enumerate DRM devices; add the primary one (single-GPU: others ignored).
    let primary_dev_id = primary_gpu.dev_id();
    if let Some((_, path)) = udev_backend
        .device_list()
        .find(|(dev_id, _)| *dev_id == primary_dev_id)
    {
        if let Err(err) = state.device_added(primary_gpu, path) {
            error!("Failed to initialize primary GPU: {err}");
            return Err(err);
        }
    } else {
        return Err(format!("primary GPU {primary_gpu} not found in udev device list").into());
    }

    // Session pause/resume across VT switches.
    event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), state| match event {
            SessionEvent::PauseSession => {
                info!("session paused");
                for device in state.backend_data.devices.values_mut() {
                    device.drm_output_manager.pause();
                }
            }
            SessionEvent::ActivateSession => {
                info!("session resumed");
                state.resume_drm_session();
            }
        })?;

    // GPU / connector hotplug. New GPUs are ignored (single-GPU); connector
    // changes on the primary are honored, primary removal is handled.
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Added { device_id, path } => {
                if device_id == primary_dev_id
                    && let Ok(node) = DrmNode::from_dev_id(device_id)
                    && !state.backend_data.devices.contains_key(&node)
                {
                    if let Err(err) = state.device_added(node, &path) {
                        error!("Failed to add device {device_id}: {err}");
                    }
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
        // Single-GPU: we only ever render on and scan out from the primary GPU.
        if node != self.backend_data.primary_gpu {
            info!("Ignoring non-primary GPU {node}");
            return Ok(());
        }

        let fd = self.backend_data.login.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;

        // Route vblank events for this device to frame_finish.
        let registration_token = self.backend_data.loop_handle.insert_source(
            drm_notifier,
            move |event, meta, state: &mut State<UdevData>| match event {
                DrmEvent::VBlank(crtc) => state.frame_finish(node, crtc, meta),
                DrmEvent::Error(err) => error!("DRM error: {err}"),
            },
        )?;

        // Build the single GLES renderer on the primary GPU.
        let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
        let egl_context = EGLContext::new(&egl_display)?;
        let render_formats = egl_context.dmabuf_render_formats().clone();
        let renderer = unsafe { GlesRenderer::new(egl_context)? };

        // Advertise zwp_linux_dmabuf_v1 now that the primary renderer exists.
        //
        // We must tell clients *which* render node to use for buffer sharing. The
        // correct way to find this is to ask EGL itself — `EGLDevice::device_for_display`
        // returns the device EGL actually opened (on kmsro/lcdif boards like REV7 this
        // is the etnaviv renderD128, not the display controller card2). Falling back to
        // `node` (the card node) would give clients an fd of -1 and break EGL init.
        if self.dmabuf_global.is_none() {
            let dmabuf_formats = renderer.dmabuf_formats();

            // Ask EGL which render node it is actually using. This handles kmsro
            // transparently: even though `node` is a display-only controller (imx-lcdif),
            // EGL internally uses the paired etnaviv render node.
            let render_node = EGLDevice::device_for_display(&egl_display)
                .ok()
                .and_then(|dev| dev.try_get_render_node().ok().flatten())
                // Final fallback: use the card node's own render peer (works on
                // real GPU cards like etnaviv card0 that do have a render node).
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
        self.backend_data.renderer = Some(renderer);

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
        // Single-output: only the first connected connector gets lit.
        if self.space.outputs().next().is_some() {
            info!("Ignoring extra connector (single-output mode)");
            return;
        }

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
        let global = output.create_global::<State<UdevData>>(&self.display_handle);
        output.set_preferred(wl_mode);
        // Auto-detect the scale from the panel's physical size unless overridden.
        let scale = crate::backend::env_scale().unwrap_or_else(|| {
            crate::backend::snap_scale(crate::backend::guess_default_scale(
                connector.size(),
                wl_mode.size,
            ))
        });
        output.change_current_state(
            Some(wl_mode),
            None,
            Some(Scale::Fractional(scale)),
            Some((0, 0).into()),
        );
        output.user_data().insert_if_missing(|| UdevOutputId {
            device_id: node,
            crtc,
        });
        self.space.map_output(&output, (0, 0));

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
                self.space.unmap_output(&output);
                return;
            }
        };

        device.surfaces.insert(
            crtc,
            SurfaceData {
                global: Some(global),
                drm_output,
            },
        );

        // Kick off the first render.
        self.backend_data.loop_handle.insert_idle(move |state| {
            state.render_surface(node, crtc);
        });
    }

    fn connector_disconnected(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(device) = self.backend_data.devices.get_mut(&node) else {
            return;
        };
        let Some(mut surface) = device.surfaces.remove(&crtc) else {
            return;
        };
        if let Some(global) = surface.global.take() {
            self.display_handle.remove_global::<State<UdevData>>(global);
        }
        let output = self
            .space
            .outputs()
            .find(|o| {
                o.user_data().get::<UdevOutputId>()
                    == Some(&UdevOutputId {
                        device_id: node,
                        crtc,
                    })
            })
            .cloned();
        if let Some(output) = output {
            self.session.output_power.output_removed(&output);
            self.space.unmap_output(&output);
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
    }

    /// Render one CRTC: queue a pageflip on damage, deliver frame callbacks otherwise.
    fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(output) = self.output_for_crtc(node, crtc) else {
            return;
        };

        if self.session.output_power.is_off(&output) {
            return;
        }

        let visible = self.visible_surfaces(&output);

        let mut queued = false;
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

                    // Pick the current animation frame, importing it as a
                    // render buffer once (cached in `pointer_images`).
                    //
                    // Load the cursor at the output's scale so the named cursor
                    // renders at the correct physical size (and sharp) on scaled
                    // outputs, matching client-provided (surface) cursors.
                    let cursor_scale =
                        output.current_scale().fractional_scale().round().max(1.0) as u32;
                    let frame = self
                        .backend_data
                        .pointer_image
                        .get_image(cursor_scale, self.clock.now().into());
                    let pointer_images = &mut self.backend_data.pointer_images;
                    let pointer_image = pointer_images
                        .iter()
                        .find_map(|(image, texture)| {
                            if image == &frame {
                                Some(texture.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(|| {
                            let buffer = MemoryRenderBuffer::from_slice(
                                &frame.pixels_rgba,
                                Fourcc::Argb8888,
                                (frame.width as i32, frame.height as i32),
                                cursor_scale as i32,
                                Transform::Normal,
                                None,
                            );
                            pointer_images.push((frame, buffer.clone()));
                            buffer
                        });

                    self.backend_data.pointer_element.set_buffer(pointer_image);

                    // Reset to the default named shape if the
                    // client-provided cursor surface went away.
                    {
                        let mut reset = false;
                        if let CursorImageStatus::Surface(ref surface) = self.cursor_status {
                            reset = !surface.is_alive();
                        }
                        if reset {
                            self.cursor_status = CursorImageStatus::default_named();
                        }
                        self.backend_data
                            .pointer_element
                            .set_status(self.cursor_status.clone());
                    }

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
                    self.is_locked,
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
                    None
                }
            };

            if let Some(states) = result {
                match surface.drm_output.queue_frame(()) {
                    Ok(()) => {
                        queued = true;
                        self.update_surface_scanout(&output, &states);
                    }
                    Err(err) => {
                        warn!("Failed to queue frame: {err}");
                    }
                }
            }
        }

        if !queued {
            // No pageflip; still deliver frame callbacks for the commit that woke us.
            self.send_frame_callbacks(&output);
            self.confirm_pending_lock();
            // Keep repainting so surface removals get re-rendered.
            self.schedule_repaint(&output);
        }
    }

    /// Vblank handler: the queued frame scanned out. Retire it, notify clients,
    /// and schedule the next repaint.
    fn frame_finish(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        _meta: &mut Option<DrmEventMetadata>,
    ) {
        {
            let Some(device) = self.backend_data.devices.get_mut(&node) else {
                return;
            };
            let Some(surface) = device.surfaces.get_mut(&crtc) else {
                return;
            };
            if let Err(err) = surface.drm_output.frame_submitted() {
                warn!("frame_submitted failed: {err}");
            }
        }

        if let Some(output) = self.output_for_crtc(node, crtc) {
            self.send_frame_callbacks(&output);
            self.confirm_pending_lock();
            self.schedule_repaint(&output);
        }
    }

    /// Schedule the next repaint after a short delay: frame-callback-driven
    /// clients repaint during it, and late changes (e.g. surface removal) are
    /// picked up instead of leaving a stale frame on the CRTC.
    fn schedule_repaint(&mut self, output: &Output) {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return;
        };
        if !self
            .backend_data
            .pending_render
            .insert((id.device_id, id.crtc))
        {
            return;
        }
        let frame_duration = output
            .current_mode()
            .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
            .unwrap_or(Duration::from_millis(16));
        let repaint_delay = Duration::from_secs_f64(frame_duration.as_secs_f64() * 0.6);
        if self
            .backend_data
            .loop_handle
            .insert_source(Timer::from_duration(repaint_delay), move |_, _, state| {
                state
                    .backend_data
                    .pending_render
                    .remove(&(id.device_id, id.crtc));
                state.render_surface(id.device_id, id.crtc);
                TimeoutAction::Drop
            })
            .is_err()
        {
            warn!("failed to schedule repaint");
        }
    }

    fn output_for_crtc(&self, node: DrmNode, crtc: crtc::Handle) -> Option<Output> {
        let id = UdevOutputId {
            device_id: node,
            crtc,
        };
        self.space
            .outputs()
            .find(|o| o.user_data().get() == Some(&id))
            .cloned()
    }
}
