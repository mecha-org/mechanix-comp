use std::sync::Mutex;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{WinitEvent, WinitGraphicsBackend};
use smithay::desktop::layer_map_for_output;
use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{IsAlive, Rectangle, Transform};
use smithay::wayland::compositor::with_states;

use crate::backend::Backend;
use crate::drawing::PointerElement;
use crate::render::{Element, OutputElements, output_elements};
use crate::state::State;

/// Backend data for the nested winit window. The output and damage tracker are
/// captured by the redraw closure, so this only owns the graphics backend plus
/// the cursor state (mirroring the udev backend).
pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    /// Loaded xcursor theme used to pick the current cursor frame.
    pub pointer_image: crate::cursor::Cursor,
    /// Cache of imported cursor frames, keyed by the raw xcursor image.
    pub pointer_images: Vec<(xcursor::parser::Image, MemoryRenderBuffer)>,
    pub pointer_element: PointerElement,
}

impl Backend for WinitData {
    fn renderer(&mut self) -> &mut GlesRenderer {
        self.backend.renderer()
    }

    fn seat_name(&self) -> String {
        "winit".to_string()
    }

    fn reset_buffers(&mut self, _output: &Output) {
        // The winit backend re-renders a full frame every time; there are no
        // scanout buffers to reset.
    }

    fn touch_transform(&self, _output: &Output) -> Transform {
        // Nested winit windows report positions in window space already, so
        // absolute input needs no output-transform correction.
        Transform::Normal
    }
}

/// Create the nested winit window, wire up the compositor state, and run the
/// event loop to completion.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<State<WinitData>> = EventLoop::try_new()?;
    let display: Display<State<WinitData>> = Display::new()?;

    let (backend, winit) = smithay::backend::winit::init::<GlesRenderer>()?;
    let mut state = State::new(
        &mut event_loop,
        display,
        WinitData {
            backend,
            pointer_image: crate::cursor::Cursor::load(),
            pointer_images: Vec::new(),
            pointer_element: PointerElement::default(),
        },
    );

    // Disable the host compositor's cursor.
    state
        .backend_data
        .backend
        .window()
        .set_cursor_visible(false);

    // Advertise zwp_linux_dmabuf_v1 with the formats the GLES renderer can
    // import, now that the renderer exists.
    let dmabuf_formats = state.backend_data.renderer().dmabuf_formats();
    let dmabuf_global = state
        .dmabuf_state
        .create_global::<State<WinitData>>(&state.display_handle, dmabuf_formats);
    state.dmabuf_global = Some(dmabuf_global);

    let mode = Mode {
        size: state.backend_data.backend.window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "0".into(),
        },
    );
    let _global = output.create_global::<State<WinitData>>(&state.display_handle);
    // Follow the host's scale factor unless `MECHA_SCALE` overrides it.
    let scale = crate::backend::env_scale()
        .unwrap_or_else(|| crate::backend::snap_scale(state.backend_data.backend.scale_factor()));
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(Scale::Fractional(scale)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, scale_factor } => {
                // Follow the host's scale factor (unless overridden) when it changes.
                let scale = (crate::backend::env_scale().is_none()
                    && scale_factor != output.current_scale().fractional_scale())
                .then(|| Scale::Fractional(crate::backend::snap_scale(scale_factor)));
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    scale,
                    None,
                );

                // Re-arrange layers for the new size, keeping toplevels filling
                // the (possibly changed) non-exclusive zone.
                layer_map_for_output(&output).arrange();
                state.apply_layout(&output);

                if state.is_locked {
                    let logical_size = state.space.output_geometry(&output).map(|geo| geo.size);
                    for surface in &state.lock_surfaces {
                        surface.with_pending_state(|pending| {
                            pending.size =
                                logical_size.map(|size| (size.w as u32, size.h as u32).into());
                        });
                        surface.send_configure();
                    }
                }
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                let size = state.backend_data.backend.window_size();
                let damage = Rectangle::from_size(size);

                // Reset to the default named shape if the client-provided
                // cursor surface went away, then mirror the status into the
                // pointer element.
                {
                    let mut reset = false;
                    if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
                        reset = !surface.alive();
                    }
                    if reset {
                        state.cursor_status = CursorImageStatus::default_named();
                    }
                    state
                        .backend_data
                        .pointer_element
                        .set_status(state.cursor_status.clone());
                }

                let visible = state.visible_surfaces(&output);
                let result = {
                    let (renderer, mut framebuffer) = state.backend_data.backend.bind().unwrap();
                    let scale =
                        smithay::utils::Scale::from(output.current_scale().fractional_scale());

                    let cursor_pos = state.pointer.current_location();
                    let cursor_hotspot =
                        if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
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

                    // Pick the current cursor frame, importing it as a render
                    // buffer once (cached in `pointer_images`). Load it at the
                    // output's scale so the named cursor renders at the correct
                    // physical size on scaled outputs.
                    let cursor_scale =
                        output.current_scale().fractional_scale().round().max(1.0) as u32;
                    let frame = state
                        .backend_data
                        .pointer_image
                        .get_image(cursor_scale, state.clock.now().into());
                    let pointer_images = &mut state.backend_data.pointer_images;
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
                    state.backend_data.pointer_element.set_buffer(pointer_image);

                    // Queue the cursor above everything else.
                    let mut custom_elements: Vec<Element> = Vec::new();
                    custom_elements.extend(
                        state
                            .backend_data
                            .pointer_element
                            .render_elements::<Element>(
                                renderer,
                                (cursor_pos - cursor_hotspot.to_f64())
                                    .to_physical(scale)
                                    .to_i32_round(),
                                scale,
                                1.0,
                            ),
                    );

                    // Draw the dnd icon if any.
                    if let Some(icon) = state.dnd_icon.as_ref() {
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

                    let (elements, clear_color) = output_elements(
                        renderer,
                        &state.space,
                        &output,
                        state.is_locked,
                        &state.lock_surfaces,
                        &state.toplevels,
                        &visible,
                        custom_elements,
                    );
                    damage_tracker
                        .render_output(renderer, &mut framebuffer, 0, &elements, clear_color)
                        .unwrap()
                };
                state.update_surface_scanout(&output, &result.states);
                state.backend_data.backend.submit(Some(&[damage])).unwrap();

                state.send_frame_callbacks(&output);
                state.confirm_pending_lock();

                state.backend_data.backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => (),
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
