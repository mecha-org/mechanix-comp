use crate::backend::Backend;
use crate::state::State;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::wayland::output::OutputHandler;

impl<BackendData: Backend + 'static> OutputHandler for State<BackendData> {
    fn output_bound(&mut self, output: Output, wl_output: WlOutput) {
        self.foreign_toplevel.output_bound(&output, &wl_output);
    }
}
