use compositor::backend;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Backend selection: an explicit `MECHA_BACKEND` wins; otherwise we assume
    // we're nested (winit) when a parent display server is present, and drive
    // KMS/DRM directly (udev) when running from a bare VT.
    let use_winit = match std::env::var("MECHA_BACKEND").ok().as_deref() {
        Some("winit") => true,
        Some("udev") => false,
        Some(other) => {
            eprintln!("unknown MECHA_BACKEND={other:?}, falling back to auto-detection");
            parent_display_present()
        }
        None => parent_display_present(),
    };

    if use_winit {
        backend::winit::run()
    } else {
        backend::udev::run()
    }
}

/// True when a parent Wayland or X11 display is already running.
fn parent_display_present() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}
