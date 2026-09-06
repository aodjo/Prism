//! Build script for the Node-API surface.
//!
//! `napi_build::setup` emits the linker flags Node addons need on each platform.

/// Configures the crate as a Node-API addon.
fn main() {
    napi_build::setup();
}
