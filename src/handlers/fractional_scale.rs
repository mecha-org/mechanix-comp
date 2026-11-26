use crate::backend::Backend;
use crate::state::State;
use smithay::wayland::fractional_scale::FractionalScaleHandler;

/// Smithay sends the scale set before the client asked for it.
impl<BackendData: Backend + 'static> FractionalScaleHandler for State<BackendData> {}
