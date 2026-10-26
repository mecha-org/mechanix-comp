//! Screencopy (`ext-image-copy-capture`) for outputs and toplevels.

use std::cell::RefCell;
use std::time::Duration;

use smithay::backend::allocator::gbm::GbmBuffer;
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::drm::compositor::RenderFrameResult;
use smithay::backend::drm::gbm::GbmFramebuffer;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::{
    AsRenderElements, Element as _, Id, Kind, RenderElement, RenderElementPresentationState,
};
use smithay::backend::renderer::gles::{Capability, GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{
    Bind, BufferType, Color32F, ExportMem, Frame as _, Offscreen, Renderer, RendererSuper,
    buffer_type,
};
use smithay::desktop::Window;
use smithay::desktop::utils::surface_primary_scanout_output;
use smithay::output::{Output, WeakOutput};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{
    Buffer as BufferCoords, IsAlive, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};
use smithay::wayland::compositor::with_states;
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle;
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
    OutputCaptureSourceState, ToplevelCaptureSourceHandler, ToplevelCaptureSourceState,
};
use smithay::wayland::image_copy_capture::{
    BufferConstraints, CaptureFailureReason, DmabufConstraints, Frame, FrameRef,
    ImageCopyCaptureHandler, ImageCopyCaptureState, Session, SessionRef,
};
use smithay::wayland::shm::{shm_format_to_fourcc, with_buffer_contents, with_buffer_contents_mut};
use tracing::warn;

use crate::backend::{Backend, egl_render_node, output_refresh};
use crate::render::Element;
use crate::state::State;

type Fb<'a> = <GlesRenderer as RendererSuper>::Framebuffer<'a>;

/// Errors from filling a capture buffer: GL, shm access, or a blit.
type CaptureError = Box<dyn std::error::Error + Send + Sync>;

/// Disjoint damage rectangles in the capture's render space.
type Damage = Vec<Rectangle<i32, Physical>>;

/// A frame composited by the DRM backend, copied by output captures.
pub type DrmFrame<'a> = RenderFrameResult<'a, GbmBuffer, GbmFramebuffer, Element>;

/// Formats offered for client shm capture buffers.
const SHM_FORMATS: &[wl_shm::Format] = &[wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888];
/// dma-buf formats accepted: 32-bit RGBA, the same set wl_shm offers.
const DMA_FORMATS: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Xrgb8888];
/// Background of toplevel captures, so ARGB buffers keep the window's alpha.
const TRANSPARENT: Color32F = Color32F::new(0.0, 0.0, 0.0, 0.0);
/// Frame interval for recorded hidden windows when the output has none.
const FALLBACK_REFRESH: Duration = Duration::from_nanos(16_666_667);
/// Damage rectangles a session keeps before collapsing them to their bounds.
const MAX_DAMAGE_RECTS: usize = 16;

/// Where a capture source reads from.
#[derive(Debug, Clone)]
enum CaptureSource {
    Output(WeakOutput),
    Toplevel(WlSurface),
}

/// A toplevel capture target: window, geometry, buffer size and scale.
struct ToplevelTarget {
    window: Window,
    /// The window's output (or the first one); it sets the scale.
    output: Option<Output>,
    geometry: Rectangle<i32, Logical>,
    size: Size<i32, BufferCoords>,
    scale: f64,
}

/// How a source is drawn into its capture buffers.
#[derive(Debug, Clone, Copy)]
struct Canvas {
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    /// Applied while drawing, and reported to the client.
    transform: Transform,
    clear: Color32F,
}

/// What a damage tracker was built for; a change invalidates it.
type TrackerKey = (Size<i32, Physical>, Scale<f64>, Transform);

/// How a frame is produced: copied from the composite, or drawn again.
enum Paint<'a, 'f, E> {
    /// Blit the DRM composite, leaving out the plane elements in `filter`.
    Blit(&'a DrmFrame<'f>, &'a [Id]),
    Draw(&'a [E]),
}

/// A live client capture session.
#[derive(Debug)]
pub struct CaptureSession {
    session: Session,
    source: CaptureSource,
    /// Buffer size last advertised to the client.
    size: Size<i32, BufferCoords>,
    /// The client's frame waiting for damage.
    pending: Option<Frame>,
    /// `None` until first tracked or resized, so the next damage is full.
    tracker: Option<(TrackerKey, OutputDamageTracker)>,
    /// Source damage not yet delivered to the client.
    damage: Damage,
}

/// Frame clock of a recorded hidden toplevel, shared by its recorders.
#[derive(Debug, Default)]
struct RecordedFrameClock {
    /// When the last frame callback was sent.
    last: Option<Duration>,
    /// Whether the next one is already scheduled.
    armed: bool,
}

impl CaptureSession {
    fn new(session: Session, source: CaptureSource) -> Self {
        let size = session
            .current_constraints()
            .map_or_else(Size::default, |constraints| constraints.size);
        Self {
            session,
            source,
            size,
            pending: None,
            tracker: None,
            damage: Vec::new(),
        }
    }

    /// Whether this session reads from `output`.
    fn is_for(&self, output: &Output) -> bool {
        matches!(&self.source, CaptureSource::Output(source) if source.upgrade().as_ref() == Some(output))
    }

    fn is_for_toplevel(&self, surface: &WlSurface) -> bool {
        matches!(&self.source, CaptureSource::Toplevel(source) if source == surface)
    }

    /// On a size change: re-advertise it, fail the frame, return `false`.
    fn ensure_size(&mut self, size: Size<i32, BufferCoords>, renderer: &GlesRenderer) -> bool {
        if self.size == size {
            return true;
        }
        self.size = size;
        self.session
            .update_constraints(constraints(size, dmabuf_constraints(renderer)));
        self.tracker = None;
        self.damage.clear();
        if let Some(frame) = self.pending.take() {
            frame.fail(CaptureFailureReason::BufferConstraints);
        }
        false
    }

    /// Track damage; fill the pending frame once there is unseen damage.
    fn update<B: Backend + 'static, E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        loop_handle: &LoopHandle<'static, State<B>>,
        canvas: Canvas,
        paint: Paint<'_, '_, E>,
        presented: Duration,
    ) {
        let key = (canvas.size, canvas.scale, canvas.transform);
        let tracker = match &mut self.tracker {
            Some((current, tracker)) if *current == key => tracker,
            slot => {
                let (size, scale, transform) = key;
                let tracker = OutputDamageTracker::new(size, scale, transform);
                &mut slot.insert((key, tracker)).1
            }
        };
        let new = match paint {
            Paint::Blit(composited, filter) => {
                composited.damage_from_age(tracker, 1, filter.iter().cloned())
            }
            Paint::Draw(elements) => tracker.damage_output(1, elements),
        };
        if let Ok((Some(new), _)) = new {
            add_damage(&mut self.damage, new.iter().copied());
        }
        if self.damage.is_empty() {
            return;
        }
        let Some(frame) = self.pending.take() else {
            return;
        };
        let region = copy_region(&self.damage, &frame.damage(), canvas);
        let painted = copy_to_buffer(
            renderer,
            &frame.buffer(),
            canvas,
            &region,
            |renderer, fb, region| match paint {
                Paint::Blit(composited, filter) => Ok(composited.blit_frame_result(
                    canvas.size,
                    canvas.transform,
                    canvas.scale,
                    renderer,
                    fb,
                    region.iter().copied(),
                    filter.iter().cloned(),
                )?),
                Paint::Draw(elements) => draw(renderer, fb, canvas, elements, region),
            },
        );
        match painted {
            Ok(sync) => {
                let damage = buffer_damage(&self.damage, canvas);
                self.damage.clear();
                ready_after(
                    loop_handle,
                    sync,
                    frame,
                    damage,
                    canvas.transform,
                    presented,
                );
            }
            Err(err) => {
                // Damage is kept so a retried frame still gets it.
                warn!("capture failed: {err}");
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }
}

impl<BackendData: Backend + 'static> ImageCaptureSourceHandler for State<BackendData> {
    // Sessions outlive their source object, so there is nothing to clean up.
}

impl<BackendData: Backend + 'static> OutputCaptureSourceHandler for State<BackendData> {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source
            .user_data()
            .insert_if_missing(|| CaptureSource::Output(output.downgrade()));
    }
}

impl<BackendData: Backend + 'static> ToplevelCaptureSourceHandler for State<BackendData> {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.toplevel_capture_source_state
    }

    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: ForeignToplevelHandle,
    ) {
        if let Some(surface) = self.foreign_toplevel.surface_for_ext_handle(&toplevel) {
            source
                .user_data()
                .insert_if_missing(|| CaptureSource::Toplevel(surface));
        }
    }
}

impl<BackendData: Backend + 'static> ImageCopyCaptureHandler for State<BackendData> {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let size = match source.user_data().get::<CaptureSource>()? {
            CaptureSource::Output(output) => output_size(&output.upgrade()?)?,
            CaptureSource::Toplevel(surface) => self.toplevel_target(surface)?.size,
        };
        let dma = dmabuf_constraints(self.backend_data.renderer());
        Some(constraints(size, dma))
    }

    fn new_session(&mut self, session: Session) {
        // Only sources carrying a `CaptureSource` were offered constraints.
        if let Some(source) = session.source().user_data().get::<CaptureSource>().cloned() {
            self.capture_sessions
                .push(CaptureSession::new(session, source));
        }
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let Some(index) = self
            .capture_sessions
            .iter()
            .position(|capture| capture.session == *session)
        else {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        };
        let capture = &mut self.capture_sessions[index];
        capture.pending = Some(frame);
        match &capture.source {
            CaptureSource::Output(output) => {
                // Render if damage is waiting; otherwise wait for a change.
                if (capture.tracker.is_none() || !capture.damage.is_empty())
                    && let Some(output) = output.upgrade()
                {
                    self.backend_data.schedule_render(&output);
                }
            }
            CaptureSource::Toplevel(surface) => {
                let surface = surface.clone();
                self.capture_toplevel(index);
                self.wake_recorded_toplevel(&surface);
            }
        }
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        for capture in &mut self.capture_sessions {
            capture.pending.take_if(|pending| *pending == frame);
        }
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.capture_sessions
            .retain(|capture| capture.session != session);
        self.image_copy_capture_state.cleanup();
    }
}

impl<BackendData: Backend + 'static> State<BackendData> {
    /// Re-send buffer constraints after an output's mode changed.
    pub fn update_capture_constraints(&mut self, output: &Output) {
        let Some(size) = output_size(output) else {
            return;
        };
        let renderer = self.backend_data.renderer();
        for capture in &mut self.capture_sessions {
            if capture.is_for(output) {
                capture.ensure_size(size, renderer);
            }
        }
    }

    /// Stop every session reading from `output` (e.g. on disconnect).
    pub fn stop_captures_for_output(&mut self, output: &Output) {
        self.capture_sessions
            .retain(|capture| !capture.is_for(output));
    }

    /// Whether something is recording the toplevel `surface`.
    pub fn is_toplevel_recorded(&self, surface: &WlSurface) -> bool {
        self.capture_sessions
            .iter()
            .any(|capture| capture.is_for_toplevel(surface))
    }

    /// Stop every session reading from the toplevel `surface`.
    pub fn stop_captures_for_toplevel(&mut self, surface: &WlSurface) {
        self.capture_sessions
            .retain(|capture| !capture.is_for_toplevel(surface));
    }

    /// Refresh captures of the toplevel that `root` (or its popup) belongs to.
    pub fn capture_commit(&mut self, root: &WlSurface) {
        if !self
            .capture_sessions
            .iter()
            .any(|capture| matches!(capture.source, CaptureSource::Toplevel(_)))
        {
            return;
        }
        let Some(toplevel) = self.toplevel_of(root) else {
            return;
        };
        let mut recorded = false;
        for index in 0..self.capture_sessions.len() {
            if self.capture_sessions[index].is_for_toplevel(&toplevel) {
                recorded = true;
                self.capture_toplevel(index);
            }
        }
        if recorded {
            self.wake_recorded_toplevel(&toplevel);
        }
    }

    /// Give a recorded hidden toplevel one frame callback per refresh.
    fn wake_recorded_toplevel(&mut self, surface: &WlSurface) {
        let Some(ToplevelTarget {
            window,
            output: Some(output),
            ..
        }) = self.toplevel_target(surface)
        else {
            return;
        };
        let refresh = output_refresh(&output).unwrap_or(FALLBACK_REFRESH);
        let now: Duration = self.clock.now().into();
        let delay = with_states(surface, |states| {
            if surface_primary_scanout_output(surface, states).is_some() {
                return None;
            }
            states
                .data_map
                .insert_if_missing(|| RefCell::new(RecordedFrameClock::default()));
            let mut clock = states
                .data_map
                .get::<RefCell<RecordedFrameClock>>()?
                .borrow_mut();
            if clock.armed {
                return None;
            }
            clock.armed = true;
            Some(
                clock
                    .last
                    .map_or(Duration::ZERO, |last| (last + refresh).saturating_sub(now)),
            )
        });
        let Some(delay) = delay else {
            return;
        };
        let target = surface.clone();
        let timer = Timer::from_duration(delay);
        let inserted = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.send_recorded_frame_callback(&target, &window, &output);
            TimeoutAction::Drop
        });
        if let Err(err) = inserted {
            warn!(
                "failed to schedule frame callback for a recorded window: {}",
                err.error
            );
            // Let a later commit arm it again.
            with_states(surface, |states| {
                if let Some(clock) = states.data_map.get::<RefCell<RecordedFrameClock>>() {
                    clock.borrow_mut().armed = false;
                }
            });
        }
    }

    fn send_recorded_frame_callback(
        &mut self,
        surface: &WlSurface,
        window: &Window,
        output: &Output,
    ) {
        if !surface.alive() {
            return;
        }
        // Recording ended meanwhile: the window gets suspended again.
        let recorded = self.is_toplevel_recorded(surface);
        let now: Duration = self.clock.now().into();
        with_states(surface, |states| {
            if let Some(clock) = states.data_map.get::<RefCell<RecordedFrameClock>>() {
                let mut clock = clock.borrow_mut();
                clock.armed = false;
                if recorded {
                    clock.last = Some(now);
                }
            }
        });
        if recorded {
            window.send_frame(output, now, None, |_, _| Some(output.clone()));
        }
    }

    /// The window behind a toplevel source with its capture size and scale.
    fn toplevel_target(&self, surface: &WlSurface) -> Option<ToplevelTarget> {
        let window = self.toplevels.get(surface)?.window.clone();
        let geometry = window.geometry();
        if geometry.size.w <= 0 || geometry.size.h <= 0 {
            return None;
        }
        let output = self
            .space
            .outputs_for_element(&window)
            .into_iter()
            .next()
            .or_else(|| self.space.outputs().next().cloned());
        let scale = output
            .as_ref()
            .map_or(1.0, |output| output.current_scale().fractional_scale());
        Some(ToplevelTarget {
            size: capture_size(geometry.size, scale),
            window,
            output,
            geometry,
            scale,
        })
    }

    /// Re-check toplevel captures that paint the cursor; it may have moved.
    pub fn refresh_cursor_captures(&mut self) {
        for index in 0..self.capture_sessions.len() {
            let capture = &self.capture_sessions[index];
            if matches!(capture.source, CaptureSource::Toplevel(_))
                && capture.pending.is_some()
                && capture.session.draw_cursor()
            {
                self.capture_toplevel(index);
            }
        }
    }

    /// Track a toplevel session's damage; draw the window into its buffer.
    fn capture_toplevel(&mut self, index: usize) {
        let CaptureSource::Toplevel(surface) = &self.capture_sessions[index].source else {
            return;
        };
        // Unmapped or not yet sized: the frame waits for a commit.
        let Some(target) = self.toplevel_target(surface) else {
            return;
        };
        let focus = self.pointer.current_focus();
        let has_pointer =
            focus.and_then(|focus| self.toplevel_of(&focus)).as_ref() == Some(surface);
        let capture = &mut self.capture_sessions[index];
        // Track damage only while a frame waits, i.e. since the last delivery.
        if !capture.ensure_size(target.size, self.backend_data.renderer())
            || capture.pending.is_none()
        {
            return;
        }
        let draw_cursor = capture.session.draw_cursor();

        let scale = Scale::from(target.scale);
        let pointer = self.pointer.current_location();
        let mut elements: Vec<Element> = Vec::new();
        if draw_cursor
            && has_pointer
            && let Some(origin) = self.space.element_location(&target.window)
            // Focus can outlast a drag out of the window; require the pointer inside it.
            && Rectangle::from_size(target.geometry.size)
                .to_f64()
                .contains(pointer - origin.to_f64())
        {
            let cursor = pointer.to_i32_round() - origin - self.cursor_hotspot();
            let location = cursor.to_physical_precise_round(scale);
            elements = self.backend_data.cursor_elements(location, scale);
        }
        let renderer = self.backend_data.renderer();
        // The window's geometry origin is the capture's top left.
        let offset = (Point::default() - target.geometry.loc).to_physical_precise_round(scale);
        elements.extend(target.window.render_elements(renderer, offset, scale, 1.0));
        let canvas = Canvas {
            size: (target.size.w, target.size.h).into(),
            scale,
            transform: Transform::Normal,
            clear: TRANSPARENT,
        };
        let presented = self.clock.now().into();
        self.capture_sessions[index].update(
            renderer,
            &self.loop_handle,
            canvas,
            Paint::Draw(&elements),
            presented,
        );
    }
}

/// One rendered output frame, offered to that output's capture sessions.
pub struct OutputFrame<'a, 'f> {
    pub output: &'a Output,
    /// The scene the frame was rendered from, front to back.
    pub elements: &'a [Element],
    pub clear_color: [f32; 4],
    /// The DRM composite to copy from; the nested backend has none.
    pub composited: Option<&'a DrmFrame<'f>>,
    pub presented: Duration,
}

/// Track damage for the output's sessions; fill frames that have some.
pub fn capture_output<B: Backend + 'static>(
    sessions: &mut [CaptureSession],
    renderer: &mut GlesRenderer,
    loop_handle: &LoopHandle<'static, State<B>>,
    frame: OutputFrame<'_, '_>,
) {
    let OutputFrame {
        output,
        elements,
        clear_color,
        composited,
        presented,
    } = frame;
    if !sessions.iter().any(|capture| capture.is_for(output)) {
        return;
    }
    let Some(buffer_size) = output_size(output) else {
        return;
    };
    let canvas = Canvas {
        size: (buffer_size.w, buffer_size.h).into(),
        scale: Scale::from(output.current_scale().fractional_scale()),
        // Swapchain layout; winit's `Flipped180` only undoes GL's y-flip.
        transform: composited.map_or(Transform::Normal, |_| output.current_transform()),
        clear: clear_color.into(),
    };
    let composited = composited.filter(|_| renderer.capabilities().contains(&Capability::Blit));
    let cursors: Vec<Id> = elements
        .iter()
        .filter(|element| element.kind() == Kind::Cursor)
        .map(|element| element.id().clone())
        .collect();
    // The blit filter skips plane elements only; a composited cursor stays.
    let cursor_in_swapchain = composited.is_some_and(|composited| {
        cursors.iter().any(|id| {
            composited
                .states
                .element_render_state(id.clone())
                .is_some_and(|state| {
                    matches!(
                        state.presentation_state,
                        RenderElementPresentationState::Rendering { .. }
                    )
                })
        })
    });
    let mut without_cursor: Option<Vec<&Element>> = None;

    for capture in sessions.iter_mut().filter(|capture| capture.is_for(output)) {
        if !capture.ensure_size(buffer_size, renderer) {
            continue;
        }
        let draw_cursor = capture.session.draw_cursor();
        match composited {
            Some(composited) if draw_cursor || !cursor_in_swapchain => {
                let filter: &[Id] = if draw_cursor { &[] } else { &cursors };
                let paint = Paint::<Element>::Blit(composited, filter);
                capture.update(renderer, loop_handle, canvas, paint, presented);
            }
            _ if draw_cursor => {
                capture.update(
                    renderer,
                    loop_handle,
                    canvas,
                    Paint::Draw(elements),
                    presented,
                );
            }
            _ => {
                let scene = without_cursor.get_or_insert_with(|| {
                    elements
                        .iter()
                        .filter(|element| element.kind() != Kind::Cursor)
                        .collect()
                });
                capture.update(renderer, loop_handle, canvas, Paint::Draw(scene), presented);
            }
        }
    }
}

/// Send `ready` once the GPU fence signals, without blocking rendering.
fn ready_after<B: Backend + 'static>(
    loop_handle: &LoopHandle<'static, State<B>>,
    sync: SyncPoint,
    frame: Frame,
    damage: Vec<Rectangle<i32, BufferCoords>>,
    transform: Transform,
    presented: Duration,
) {
    let ready = move |frame: Frame| frame.success(transform, Some(damage), presented);
    if sync.is_reached() {
        return ready(frame);
    }
    let Some(fence) = sync.export() else {
        // No exportable fence: block on it instead.
        if sync.wait().is_err() {
            warn!("interrupted waiting for capture fence");
        }
        return ready(frame);
    };
    let mut pending = Some((frame, ready));
    let source = Generic::new(fence, Interest::READ, Mode::OneShot);
    let inserted = loop_handle.insert_source(source, move |_, _, _| {
        if let Some((frame, ready)) = pending.take() {
            ready(frame);
        }
        Ok(PostAction::Remove)
    });
    if let Err(err) = inserted {
        // The frame is dropped with the source, which fails it.
        warn!("failed to watch capture fence: {}", err.error);
    }
}

/// Paint `region` into a client buffer (dma-buf directly, shm via readback).
fn copy_to_buffer(
    renderer: &mut GlesRenderer,
    buffer: &WlBuffer,
    canvas: Canvas,
    region: &[Rectangle<i32, Physical>],
    paint: impl FnOnce(
        &mut GlesRenderer,
        &mut Fb<'_>,
        &[Rectangle<i32, Physical>],
    ) -> Result<SyncPoint, CaptureError>,
) -> Result<SyncPoint, CaptureError> {
    match buffer_type(buffer) {
        Some(BufferType::Dma) => {
            let mut dmabuf = get_dmabuf(buffer)?.clone();
            let mut fb = renderer.bind(&mut dmabuf)?;
            paint(renderer, &mut fb, region)
        }
        Some(BufferType::Shm) => {
            // Rare (clients use dma-bufs): paint it all, read it all back.
            let fourcc = shm_fourcc(buffer)?;
            let size = Size::from((canvas.size.w, canvas.size.h));
            let mut texture = Offscreen::<GlesTexture>::create_buffer(renderer, fourcc, size)?;
            let mut fb = renderer.bind(&mut texture)?;
            // The readback is queued after the paint on the same context.
            let _ = paint(renderer, &mut fb, &[full_region(canvas)])?;
            write_shm(renderer, &fb, size, fourcc, buffer)?;
            Ok(SyncPoint::signaled())
        }
        _ => Err("unsupported capture buffer type".into()),
    }
}

/// Clear `region` of the bound framebuffer and draw `elements` over it.
fn draw<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    fb: &mut Fb<'_>,
    canvas: Canvas,
    elements: &[E],
    region: &[Rectangle<i32, Physical>],
) -> Result<SyncPoint, CaptureError> {
    let mut frame = renderer.render(fb, canvas.size, canvas.transform)?;
    frame.clear(canvas.clear, region)?;
    draw_render_elements::<GlesRenderer, _, _>(&mut frame, canvas.scale, elements, region)?;
    Ok(frame.finish()?)
}

fn shm_fourcc(buffer: &WlBuffer) -> Result<Fourcc, CaptureError> {
    with_buffer_contents(buffer, |_, _, data| shm_format_to_fourcc(data.format))?
        .ok_or_else(|| "unsupported shm format".into())
}

/// Read the whole framebuffer back into an shm buffer.
fn write_shm(
    renderer: &mut GlesRenderer,
    src: &Fb<'_>,
    size: Size<i32, BufferCoords>,
    fourcc: Fourcc,
    buffer: &WlBuffer,
) -> Result<(), CaptureError> {
    let mapping = renderer.copy_framebuffer(src, Rectangle::from_size(size), fourcc)?;
    let pixels = renderer.map_texture(&mapping)?;

    with_buffer_contents_mut(buffer, |ptr, len, data| {
        // SAFETY: smithay maps the whole pool (`len` bytes) for this closure.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        let stride = data.stride as usize;
        let row_len = size.w as usize * 4;
        for (row, src) in pixels.chunks_exact(row_len).enumerate() {
            let at = data.offset as usize + row * stride;
            dst.get_mut(at..at + row_len)
                .ok_or("shm buffer smaller than its constraints")?
                .copy_from_slice(src);
        }
        Ok(())
    })?
}

/// Merge `new` into `damage`, kept disjoint (overlaps would blend twice).
fn add_damage(damage: &mut Damage, new: impl IntoIterator<Item = Rectangle<i32, Physical>>) {
    for mut rect in new {
        while let Some(index) = damage.iter().position(|other| other.overlaps(rect)) {
            rect = rect.merge(damage.swap_remove(index));
        }
        damage.push(rect);
    }
    if damage.len() > MAX_DAMAGE_RECTS
        && let Some(bounds) = damage.drain(..).reduce(Rectangle::merge)
    {
        damage.push(bounds);
    }
}

/// The region to repaint: the session damage plus the client's buffer damage.
fn copy_region(
    damage: &[Rectangle<i32, Physical>],
    client: &[Rectangle<i32, BufferCoords>],
    canvas: Canvas,
) -> Damage {
    if canvas.transform != Transform::Normal {
        // A blit uses one damage list in two spaces; only full damage fits.
        return vec![full_region(canvas)];
    }
    let bounds = Rectangle::from_size(canvas.size);
    let mut region = damage.to_vec();
    add_damage(
        &mut region,
        client
            .iter()
            .filter_map(|&rect| from_buffer(rect).intersection(bounds)),
    );
    region
}

/// A rectangle covering the canvas in either orientation.
fn full_region(canvas: Canvas) -> Rectangle<i32, Physical> {
    let side = canvas.size.w.max(canvas.size.h);
    let size = if canvas.transform == Transform::Normal {
        canvas.size
    } else {
        (side, side).into()
    };
    Rectangle::from_size(size)
}

/// Session damage in buffer coordinates, as reported to the client.
fn buffer_damage(
    damage: &[Rectangle<i32, Physical>],
    canvas: Canvas,
) -> Vec<Rectangle<i32, BufferCoords>> {
    if canvas.transform == Transform::Normal {
        damage.iter().copied().map(to_buffer).collect()
    } else {
        vec![to_buffer(Rectangle::from_size(canvas.size))]
    }
}

/// Identity between render space and the buffer of an untransformed capture.
fn to_buffer(rect: Rectangle<i32, Physical>) -> Rectangle<i32, BufferCoords> {
    Rectangle::new(
        (rect.loc.x, rect.loc.y).into(),
        (rect.size.w, rect.size.h).into(),
    )
}

fn from_buffer(rect: Rectangle<i32, BufferCoords>) -> Rectangle<i32, Physical> {
    Rectangle::new(
        (rect.loc.x, rect.loc.y).into(),
        (rect.size.w, rect.size.h).into(),
    )
}

fn constraints(size: Size<i32, BufferCoords>, dma: Option<DmabufConstraints>) -> BufferConstraints {
    BufferConstraints {
        size,
        shm: SHM_FORMATS.to_vec(),
        dma,
    }
}

/// The buffer size for an output's current mode, as the swapchain lays it out.
fn output_size(output: &Output) -> Option<Size<i32, BufferCoords>> {
    let size = output.current_mode()?.size;
    Some(Size::from((size.w, size.h)))
}

/// The buffer size for a logical size at `scale`.
fn capture_size(logical: Size<i32, Logical>, scale: f64) -> Size<i32, BufferCoords> {
    Size::from((
        (logical.w as f64 * scale).round() as i32,
        (logical.h as f64 * scale).round() as i32,
    ))
}

/// dma-buf constraints: accepted formats with their renderable modifiers.
fn dmabuf_constraints(renderer: &GlesRenderer) -> Option<DmabufConstraints> {
    let node = egl_render_node(renderer.egl_context().display())?;
    let mut formats: Vec<(Fourcc, Vec<Modifier>)> =
        DMA_FORMATS.iter().map(|&code| (code, Vec::new())).collect();
    for format in renderer.egl_context().dmabuf_render_formats().iter() {
        if let Some((_, modifiers)) = formats.iter_mut().find(|(code, _)| *code == format.code) {
            modifiers.push(format.modifier);
        }
    }
    formats.retain(|(_, modifiers)| !modifiers.is_empty());
    (!formats.is_empty()).then_some(DmabufConstraints { node, formats })
}

#[cfg(test)]
mod tests {
    use super::{add_damage, capture_size, copy_region};
    use smithay::utils::{Rectangle, Size, Transform};

    #[test]
    fn capture_size_scales_and_rounds() {
        assert_eq!(
            capture_size(Size::from((800, 600)), 1.0),
            Size::from((800, 600))
        );
        assert_eq!(
            capture_size(Size::from((800, 600)), 2.0),
            Size::from((1600, 1200))
        );
        assert_eq!(
            capture_size(Size::from((801, 601)), 1.5),
            Size::from((1202, 902))
        );
    }

    #[test]
    fn damage_stays_disjoint() {
        let mut damage = vec![Rectangle::new((0, 0).into(), (10, 10).into())];
        add_damage(
            &mut damage,
            [
                Rectangle::new((5, 5).into(), (10, 10).into()),
                Rectangle::new((100, 100).into(), (5, 5).into()),
            ],
        );
        assert_eq!(
            damage,
            vec![
                Rectangle::new((0, 0).into(), (15, 15).into()),
                Rectangle::new((100, 100).into(), (5, 5).into()),
            ]
        );
        // A bridging rect merges all three; an exact duplicate collapses.
        add_damage(
            &mut damage,
            [Rectangle::new((10, 10).into(), (95, 95).into())],
        );
        add_damage(
            &mut damage,
            [Rectangle::new((0, 0).into(), (105, 105).into())],
        );
        assert_eq!(
            damage,
            vec![Rectangle::new((0, 0).into(), (105, 105).into())]
        );
    }

    #[test]
    fn damage_collapses_past_limit() {
        let mut damage = Vec::new();
        add_damage(
            &mut damage,
            (0..=super::MAX_DAMAGE_RECTS as i32)
                .map(|i| Rectangle::new((i * 10, 0).into(), (5, 5).into())),
        );
        assert_eq!(
            damage,
            vec![Rectangle::new(
                (0, 0).into(),
                (super::MAX_DAMAGE_RECTS as i32 * 10 + 5, 5).into()
            )]
        );
    }

    #[test]
    fn client_damage_is_clipped_to_the_buffer() {
        let canvas = super::Canvas {
            size: Size::from((100, 100)),
            scale: 1.0.into(),
            transform: Transform::Normal,
            clear: super::TRANSPARENT,
        };
        let region = copy_region(
            &[Rectangle::new((0, 0).into(), (10, 10).into())],
            &[Rectangle::new((90, 90).into(), (50, 50).into())],
            canvas,
        );
        assert_eq!(
            region,
            vec![
                Rectangle::new((0, 0).into(), (10, 10).into()),
                Rectangle::new((90, 90).into(), (10, 10).into()),
            ]
        );
    }
}
