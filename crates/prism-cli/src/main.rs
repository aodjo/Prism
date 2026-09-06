//! Headless Prism host and client.
//!
//! The CLI is how milestones M1 through M4 are developed and measured: it runs the
//! full data plane with no Electron and no UI, so latency numbers reflect the pipeline
//! rather than a window manager. CI also drives it for protocol regression runs.

use prism_core::net::packet::FORMAT_VERSION;

/// Entry point. Prints build identity until the capture pipeline lands in M1.
fn main() {
    println!(
        "prism-cli {} (wire format v{FORMAT_VERSION})",
        env!("CARGO_PKG_VERSION")
    );
}
