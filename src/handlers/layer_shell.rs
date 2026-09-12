use crate::backend::Backend;
use crate::state::State;
use smithay::desktop::{LayerSurface, PopupKind, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
    WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::PopupSurface;

impl<BackendData: Backend + 'static> WlrLayerShellHandler for State<BackendData> {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // Map into the output's `LayerMap`; keyboard focus and the initial
        // configure are deferred to `handle_commit`, once the double-buffered
        // interactivity state is applied.
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());
        if let Some(output) = output {
            let mut map = layer_map_for_output(&output);
            map.map_layer(&LayerSurface::new(surface, namespace))
                .unwrap();
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| {
                layer_map_for_output(o)
                    .layers()
                    .any(|layer| layer.layer_surface() == &surface)
            })
            .cloned()
        else {
            if self.layer_shell_on_demand_focus.as_ref() == Some(surface.wl_surface()) {
                self.layer_shell_on_demand_focus = None;
            }
            return;
        };

        #[cfg(feature = "session")]
        let zone_before = layer_map_for_output(&output).non_exclusive_zone();
        {
            let mut map = layer_map_for_output(&output);
            let layer = map
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = layer {
                map.unmap_layer(&layer);
            }
        }
        if self.layer_shell_on_demand_focus.as_ref() == Some(surface.wl_surface()) {
            self.layer_shell_on_demand_focus = None;
        }
        #[cfg(feature = "session")]
        if zone_before != layer_map_for_output(&output).non_exclusive_zone() {
            self.apply_layout(&output);
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_layer_popup(&popup);
        let _ = self.popups.track_popup(PopupKind::Xdg(popup));
    }
}

/// Layer-surface side of a commit: arrange the map, send the first configure,
/// and reflow toplevels if the work zone changed. Returns `true` if `surface`
/// was a layer surface (so the caller can skip the toplevel path).
pub fn handle_commit<BackendData: Backend + 'static>(
    state: &mut State<BackendData>,
    surface: &WlSurface,
) -> bool {
    let Some(output) = state
        .space
        .outputs()
        .find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
        .cloned()
    else {
        return false;
    };

    let initial_configure_sent = with_states(surface, |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .unwrap()
            .lock()
            .unwrap()
            .initial_configure_sent
    });

    #[cfg(feature = "session")]
    let zone_before = layer_map_for_output(&output).non_exclusive_zone();
    let (needs_configure, on_demand) = {
        let mut map = layer_map_for_output(&output);
        map.arrange();
        let layer = map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .unwrap();
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        }
        // Overlay/Top OnDemand layers take keyboard focus on first map; Bottom/Background are click-to-focus.
        let cached = layer.cached_state();
        let on_demand = matches!(cached.layer, Layer::Overlay | Layer::Top)
            && cached.keyboard_interactivity == KeyboardInteractivity::OnDemand;
        (!initial_configure_sent, on_demand)
    };

    #[cfg(feature = "session")]
    if zone_before != layer_map_for_output(&output).non_exclusive_zone() {
        state.apply_layout(&output);
    }
    if needs_configure && on_demand {
        state.layer_shell_on_demand_focus = Some(surface.clone());
    }
    true
}
