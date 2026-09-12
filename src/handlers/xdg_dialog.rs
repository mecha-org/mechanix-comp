use crate::backend::Backend;
use crate::state::State;
use smithay::wayland::shell::xdg::dialog::{ToplevelDialogHint, XdgDialogHandler};

impl<BackendData: Backend + 'static> XdgDialogHandler for State<BackendData> {
    fn dialog_hint_changed(
        &mut self,
        toplevel: smithay::wayland::shell::xdg::ToplevelSurface,
        hint: ToplevelDialogHint,
    ) {
        // Cache the hint so input handling doesn't lock surface data per click.
        if let Some(ws) = self.toplevels.get_mut(toplevel.wl_surface()) {
            ws.modal = hint == ToplevelDialogHint::Modal;
        }
    }
}
