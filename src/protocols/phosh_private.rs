//! Server bindings for Phosh's private protocol (`protocols/phosh-private.xml`,
//! read through the copy `build.rs` prepares).
//!
//! The XML references wlr interfaces; they come from smithay's
//! `wayland_protocols_wlr` re-export so the resource types match smithay's.

#![allow(dead_code, non_camel_case_types, unused_imports, unused_variables)]
#![allow(non_upper_case_globals, non_snake_case, missing_docs, clippy::all)]

use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::*;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::server::*;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::*;
use wayland_server;
use wayland_server::protocol::*;

pub mod __interfaces {
    use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::__interfaces::*;
    use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::server::__interfaces::*;
    use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::__interfaces::*;
    use wayland_server::protocol::__interfaces::*;
    include!(concat!(env!("OUT_DIR"), "/phosh_private_interfaces.rs"));
}
use self::__interfaces::*;

include!(concat!(env!("OUT_DIR"), "/phosh_private_server.rs"));
