//! Node-API surface over the Prism core.
//!
//! This crate exposes **control calls and low-rate stats only**. Video frames, GPU
//! textures, and packets never cross this boundary: a frame that enters V8 has already
//! cost more than the entire latency budget. Anything added here that takes or returns
//! frame data is a bug.
//!
//! The resulting `.node` is a Node-API binary, so one build loads unchanged in both
//! Node and Electron.

use napi_derive::napi;

/// Returns the Prism version string.
///
/// Used by the Electron shells to confirm the native addon loaded and to report the
/// core version alongside the UI version in diagnostics.
#[napi]
#[must_use]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Returns the wire format revision this build speaks.
///
/// The control plane compares this against `FORMAT_VERSION` from `@prism/protocol`
/// during session setup; a mismatch means the addon and the TypeScript bundle were
/// built from different commits and the session is refused rather than risking a
/// silently misparsed stream.
#[napi]
#[must_use]
pub fn wire_format_version() -> u32 {
    prism_core::net::packet::FORMAT_VERSION
}
