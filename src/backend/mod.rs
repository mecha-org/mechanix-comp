use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Physical, Size, Transform};

#[cfg(feature = "session")]
pub mod udev;
#[cfg(feature = "session")]
pub mod winit;

/// 2x only for clearly HiDPI panels: tall enough, real physical size, >192 DPI on both axes.
pub fn guess_default_scale(size_mm: Option<(u32, u32)>, mode: Size<i32, Physical>) -> f64 {
    let Some((w_mm, h_mm)) = size_mm else {
        return 1.0;
    };
    // Ignore bogus EDID sizes reported as a bare aspect ratio.
    if w_mm == 0 || h_mm == 0 || mode.h < 1200 {
        return 1.0;
    }
    if matches!(
        (w_mm, h_mm),
        (1600, 900) | (1600, 1000) | (160, 90) | (160, 100) | (16, 9) | (16, 10)
    ) {
        return 1.0;
    }
    let dpi_x = mode.w as f64 / (w_mm as f64 / 25.4);
    let dpi_y = mode.h as f64 / (h_mm as f64 / 25.4);
    if dpi_x > 192.0 && dpi_y > 192.0 {
        2.0
    } else {
        1.0
    }
}

/// Snap to the fractional-scale protocol's representable values (N/120).
pub fn snap_scale(scale: f64) -> f64 {
    (scale * 120.0).round() / 120.0
}

/// The `MECHA_SCALE` override, if set.
pub fn env_scale() -> Option<f64> {
    std::env::var("MECHA_SCALE")
        .ok()?
        .parse::<f64>()
        .ok()
        .map(|scale| snap_scale(scale.clamp(0.1, 10.0)))
}

/// Abstraction over the concrete rendering/presentation backend the compositor
/// runs on. Both backends render with the same `GlesRenderer`, so the render
/// element type stays uniform (`WaylandSurfaceRenderElement<GlesRenderer>`);
/// only the presentation path differs.
pub trait Backend {
    /// The renderer used to import client buffers and draw frames.
    fn renderer(&mut self) -> &mut GlesRenderer;

    /// The seat name advertised for this backend.
    fn seat_name(&self) -> String;

    /// Discard any cached swapchain/damage buffers for `output`, e.g. after a
    /// session resume (VT switch back) where the previous framebuffers are stale.
    fn reset_buffers(&mut self, output: &Output);

    /// Opportunity to pre-import a client buffer onto the render GPU before it is
    /// sampled. A no-op with a single-GPU `GlesRenderer`.
    fn early_import(&mut self, surface: &WlSurface) {
        let _ = surface;
    }

    fn change_vt(&mut self, _vt: i32) {} // no-op by default

    /// Whether this output can be DPMS-blanked (`zwlr_output_power_v1`).
    fn output_power_supported(&self, _output: &Output) -> bool {
        false
    }

    /// Enable or disable the CRTC. After `on`, the caller must `schedule_render`.
    fn set_output_dpms(&mut self, _output: &Output, _on: bool) -> bool {
        false
    }

    /// Re-activate DRM after a VT switch or resume. Default is a no-op.
    fn prepare_resume(&mut self) {}

    /// Queue a redraw of `output`; the backend skips ones already pending.
    fn schedule_render(&mut self, _output: &Output) {}

    /// Transform to apply to absolute (touch) input positions for `output`.
    /// Nested winit windows already report positions in window space, so they
    /// want identity; udev/DRM (and trait default) reports them in the output's transformed space.
    fn touch_transform(&self, output: &Output) -> Transform {
        output.current_transform()
    }
}
