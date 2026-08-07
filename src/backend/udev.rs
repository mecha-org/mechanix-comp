use std::collections::HashMap;
use std::path::Path;
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
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent, primary_gpu};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{ModeTypeFlags, connector, crtc};
use smithay::reexports::input::{DeviceCapability, Libinput};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::DeviceFd;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{error, info, warn};

use crate::backend::Backend;
use crate::render::{Element, output_elements};
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
    session: LibSeatSession,
    loop_handle: LoopHandle<'static, State<UdevData>>,
    primary_gpu: DrmNode,
    /// Single GLES renderer bound to the primary GPU. Created when the primary
    /// device is added.
    renderer: Option<GlesRenderer>,
    devices: HashMap<DrmNode, DeviceData>,
    keyboards: Vec<smithay::reexports::input::Device>,
}

impl Backend for UdevData {
    fn renderer(&mut self) -> &mut GlesRenderer {
        self.renderer
            .as_mut()
            .expect("primary GPU renderer not initialized")
    }

    fn seat_name(&self) -> String {
        self.session.seat()
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
        if let Err(err) = self.session.change_vt(vt) {
            error!(vt, "Error switching vt: {}", err);
        }
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
        session,
        loop_handle: loop_handle.clone(),
        primary_gpu,
        renderer: None,
        devices: HashMap::new(),
        keyboards: Vec::new(),
    };

    let mut state = State::new(&mut event_loop, display, udev_data);

    let udev_backend = UdevBackend::new(state.seat.name())?;
    // Initialize libinput backend
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        state.backend_data.session.clone().into(),
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
            } else if let InputEvent::DeviceRemoved { ref device } = event {
                if device.has_capability(DeviceCapability::Keyboard) {
                    data.backend_data.keyboards.retain(|item| item != device);
                }
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
                let nodes: Vec<DrmNode> = state.backend_data.devices.keys().copied().collect();
                for node in nodes {
                    let crtcs: Vec<crtc::Handle> = match state.backend_data.devices.get_mut(&node) {
                        Some(device) => {
                            if let Err(err) = device.drm_output_manager.lock().activate(false) {
                                warn!("Failed to activate DRM device {node}: {err}");
                            }
                            device.surfaces.keys().copied().collect()
                        }
                        None => continue,
                    };

                    // The previous swapchain buffers are stale after a VT switch;
                    // discard them so the next frame is a clean full render.
                    let outputs: Vec<Output> = state
                        .space
                        .outputs()
                        .filter(|o| {
                            o.user_data()
                                .get::<UdevOutputId>()
                                .is_some_and(|id| id.device_id == node)
                        })
                        .cloned()
                        .collect();
                    for output in &outputs {
                        state.backend_data.reset_buffers(output);
                    }

                    for crtc in crtcs {
                        state.backend_data.loop_handle.insert_idle(move |state| {
                            state.render_surface(node, crtc);
                        });
                    }
                }
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
        // Per-frame upkeep: refresh the space, clean up dead popups/toplevels,
        // re-derive keyboard focus, and flush client events.
        state.space.refresh();
        state.popups.cleanup();
        state.cleanup_toplevels();
        state.update_keyboard_focus();
        state.foreign_toplevel_refresh();
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

        let fd = self.backend_data.session.open(
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
        output.change_current_state(Some(wl_mode), None, None, Some((0, 0).into()));
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

    /// Render one CRTC. On damage, queue a pageflip and wait for the vblank
    /// (`frame_finish` re-arms). On no damage, re-arm a poll ~one frame later.
    fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| {
                o.user_data().get::<UdevOutputId>()
                    == Some(&UdevOutputId {
                        device_id: node,
                        crtc,
                    })
            })
            .cloned()
        else {
            return;
        };

        let frame_duration = frame_duration(&output);

        let queued = {
            let Some(renderer) = self.backend_data.renderer.as_mut() else {
                return;
            };
            let Some(device) = self.backend_data.devices.get_mut(&node) else {
                return;
            };
            let Some(surface) = device.surfaces.get_mut(&crtc) else {
                return;
            };

            let (elements, clear_color) = output_elements(
                renderer,
                &self.space,
                &output,
                self.is_locked,
                &self.lock_surfaces,
                &self.toplevels,
            );

            match surface.drm_output.render_frame(
                renderer,
                &elements,
                clear_color,
                FrameFlags::DEFAULT,
            ) {
                Ok(result) if !result.is_empty => match surface.drm_output.queue_frame(()) {
                    Ok(()) => true,
                    Err(err) => {
                        warn!("Failed to queue frame: {err}");
                        false
                    }
                },
                Ok(_) => false,
                Err(err) => {
                    warn!("Rendering failed: {err}");
                    false
                }
            }
        };

        if !queued {
            // No pageflip pending; poll again after one refresh interval.
            let timer = Timer::from_duration(frame_duration);
            let _ = self
                .backend_data
                .loop_handle
                .insert_source(timer, move |_, _, state| {
                    state.render_surface(node, crtc);
                    TimeoutAction::Drop
                });
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
            self.send_frame_callbacks(&output);
            self.backend_data.loop_handle.insert_idle(move |state| {
                state.render_surface(node, crtc);
            });
        }
    }
}

fn frame_duration(output: &Output) -> Duration {
    output
        .current_mode()
        .filter(|mode| mode.refresh > 0)
        .map(|mode| Duration::from_secs_f64(1000.0 / mode.refresh as f64))
        .unwrap_or_else(|| Duration::from_millis(16))
}
