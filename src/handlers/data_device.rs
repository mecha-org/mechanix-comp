use crate::backend::Backend;
use crate::state::{DndIcon, State};
use smithay::input::Seat;
use smithay::input::dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};

impl<BackendData: Backend + 'static> SelectionHandler for State<BackendData> {
    type SelectionUserData = ();
}

impl<BackendData: Backend + 'static> DndGrabHandler for State<BackendData> {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
        self.schedule_render();
    }
}

impl<BackendData: Backend + 'static> WaylandDndGrabHandler for State<BackendData> {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        self.dnd_icon = icon.map(|surface| DndIcon {
            surface,
            offset: (0, 0).into(),
        });
        self.schedule_render();

        match type_ {
            GrabType::Pointer => {
                let ptr = seat.get_pointer().unwrap();
                let start_data = ptr.grab_start_data().unwrap();

                // create a dnd grab to start the operation
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => {
                // TODO touch handling
                source.cancel();
            }
        }
    }
}

impl<BackendData: Backend + 'static> DataDeviceHandler for State<BackendData> {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}
