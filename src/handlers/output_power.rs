//! `zwlr_output_power_manager_v1`: clients (`wlopm`, a future shell) set DPMS.

use std::collections::{HashMap, HashSet};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};
use smithay::reexports::wayland_server::WEnum;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use tracing::warn;

use crate::backend::Backend;
use crate::state::State;

const VERSION: u32 = 1;

#[derive(Default)]
pub struct OutputPowerManagerUdata;

/// Per-handle output; `None` if the `wl_output` was already gone.
pub struct OutputPowerUdata {
    output: Option<Output>,
}

#[derive(Default)]
pub struct OutputPowerManagerState {
    /// Outputs currently DPMS-off. Missing means on.
    off: HashSet<Output>,
    handles: HashMap<Output, HashSet<ZwlrOutputPowerV1>>,
}

impl OutputPowerManagerState {
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrOutputPowerManagerV1, OutputPowerManagerUdata>
            + Dispatch<ZwlrOutputPowerManagerV1, OutputPowerManagerUdata>
            + Dispatch<ZwlrOutputPowerV1, OutputPowerUdata>
            + 'static,
    {
        dh.create_global::<D, ZwlrOutputPowerManagerV1, _>(VERSION, OutputPowerManagerUdata);
        Self::default()
    }

    pub fn is_off(&self, output: &Output) -> bool {
        self.off.contains(output)
    }

    fn send_mode(&self, output: &Output, on: bool) {
        let mode = if on {
            zwlr_output_power_v1::Mode::On
        } else {
            zwlr_output_power_v1::Mode::Off
        };
        if let Some(handles) = self.handles.get(output) {
            for handle in handles {
                handle.mode(mode);
            }
        }
    }

    fn remove_handle(&mut self, resource: &ZwlrOutputPowerV1) {
        for handles in self.handles.values_mut() {
            handles.remove(resource);
        }
        self.handles.retain(|_, handles| !handles.is_empty());
    }

    /// Output left the space: fail every handle and drop the off-bit.
    pub fn output_removed(&mut self, output: &Output) {
        if let Some(handles) = self.handles.remove(output) {
            for handle in handles {
                handle.failed();
            }
        }
        self.off.remove(output);
    }
}

impl<BackendData: Backend + 'static> State<BackendData> {
    /// Apply DPMS for `output` and notify protocol clients; backend refusal sends `failed`.
    pub fn set_output_power(&mut self, output: &Output, on: bool) {
        if self.output_power.is_off(output) == !on {
            return;
        }

        if !self.backend_data.set_output_dpms(output, on) {
            warn!(output = %output.name(), on, "output power change failed");
            if let Some(handles) = self.output_power.handles.get(output) {
                for handle in handles {
                    handle.failed();
                }
            }
            return;
        }

        if on {
            self.output_power.off.remove(output);
            self.backend_data.schedule_render(output);
        } else {
            self.output_power.off.insert(output.clone());
            self.release_fifo_barriers(output);
            self.maybe_send_locked();
        }
        self.output_power.send_mode(output, on);
    }

    /// Wake every blanked output. Returns true if anything was off, so the
    /// caller can consume the input that woke the panel.
    pub fn wake_outputs_if_off(&mut self) -> bool {
        if self.output_power.off.is_empty() {
            return false;
        }
        let outputs: Vec<Output> = self.output_power.off.iter().cloned().collect();
        for output in &outputs {
            self.set_output_power(output, true);
        }
        true
    }

    /// Re-activate DRM after a VT switch, keeping DPMS-off outputs blanked.
    pub fn resume_drm_session(&mut self) {
        self.backend_data.prepare_resume();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in &outputs {
            self.backend_data.reset_buffers(output);
            if self.output_power.is_off(output) {
                let _ = self.backend_data.set_output_dpms(output, false);
            } else {
                self.backend_data.schedule_render(output);
            }
        }
    }
}

impl<BackendData: Backend + 'static> GlobalDispatch2<ZwlrOutputPowerManagerV1, State<BackendData>>
    for OutputPowerManagerUdata
{
    fn bind(
        &self,
        _state: &mut State<BackendData>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        data_init.init(resource, OutputPowerManagerUdata);
    }
}

impl<BackendData: Backend + 'static> Dispatch2<ZwlrOutputPowerManagerV1, State<BackendData>>
    for OutputPowerManagerUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } => {
                create_output_power(state, id, &output, data_init);
            }
            zwlr_output_power_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        _state: &mut State<BackendData>,
        _client: ClientId,
        _resource: &ZwlrOutputPowerManagerV1,
    ) {
    }
}

fn create_output_power<BackendData: Backend + 'static>(
    state: &mut State<BackendData>,
    id: New<ZwlrOutputPowerV1>,
    wl_output: &WlOutput,
    data_init: &mut DataInit<'_, State<BackendData>>,
) {
    let Some(output) = Output::from_resource(wl_output) else {
        let handle = data_init.init(id, OutputPowerUdata { output: None });
        handle.failed();
        return;
    };

    if !state.backend_data.output_power_supported(&output) {
        let handle = data_init.init(
            id,
            OutputPowerUdata {
                output: Some(output),
            },
        );
        handle.failed();
        return;
    }

    let on = !state.output_power.is_off(&output);
    let handle = data_init.init(
        id,
        OutputPowerUdata {
            output: Some(output.clone()),
        },
    );
    handle.mode(if on {
        zwlr_output_power_v1::Mode::On
    } else {
        zwlr_output_power_v1::Mode::Off
    });
    state
        .output_power
        .handles
        .entry(output)
        .or_default()
        .insert(handle);
}

impl<BackendData: Backend + 'static> Dispatch2<ZwlrOutputPowerV1, State<BackendData>>
    for OutputPowerUdata
{
    fn request(
        &self,
        state: &mut State<BackendData>,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<BackendData>>,
    ) {
        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => {
                let on = match mode {
                    WEnum::Value(zwlr_output_power_v1::Mode::On) => true,
                    WEnum::Value(zwlr_output_power_v1::Mode::Off) => false,
                    WEnum::Value(_) | WEnum::Unknown(_) => {
                        resource.post_error(
                            zwlr_output_power_v1::Error::InvalidMode,
                            "invalid output power mode",
                        );
                        return;
                    }
                };
                let Some(output) = &self.output else {
                    resource.failed();
                    return;
                };
                state.set_output_power(output, on);
            }
            zwlr_output_power_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        state: &mut State<BackendData>,
        _client: ClientId,
        resource: &ZwlrOutputPowerV1,
    ) {
        state.output_power.remove_handle(resource);
    }
}
