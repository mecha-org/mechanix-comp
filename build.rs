//! Prepares `protocols/phosh-private.xml` for wayland-scanner.
//!
//! The vendored file stays byte-for-byte upstream. Upstream's
//! `phosh_private_startup_tracker` events reference their bitfield as
//! `enum="flags"` while it is declared as `flag`, which the scanner turns into
//! a missing type; the scanner reads a copy with that reference fixed.

use std::env;
use std::fs;
use std::path::PathBuf;

const PHOSH_PRIVATE: &str = "protocols/phosh-private.xml";

fn main() {
    println!("cargo:rerun-if-changed={PHOSH_PRIVATE}");
    let xml = fs::read_to_string(PHOSH_PRIVATE).expect("read phosh-private.xml");
    let fixed = xml.replace(r#"enum="flags""#, r#"enum="flag""#);

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let xml_path = out_dir.join("phosh-private.xml");
    fs::write(&xml_path, fixed).expect("write phosh-private.xml");

    // The scanner macros take a literal path; hand them one through `include!`.
    let xml_path = xml_path.to_str().expect("UTF-8 OUT_DIR");
    fs::write(
        out_dir.join("phosh_private_interfaces.rs"),
        format!("wayland_scanner::generate_interfaces!({xml_path:?});\n"),
    )
    .expect("write phosh_private_interfaces.rs");
    fs::write(
        out_dir.join("phosh_private_server.rs"),
        format!("wayland_scanner::generate_server_code!({xml_path:?});\n"),
    )
    .expect("write phosh_private_server.rs");
}
