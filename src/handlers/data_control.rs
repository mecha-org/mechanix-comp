use crate::backend::Backend;
use crate::state::State;
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};

impl<BackendData: Backend + 'static> DataControlHandler for State<BackendData> {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}
