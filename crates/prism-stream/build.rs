//! Two things decided before the crate compiles: an identity for the binary on macOS, and the
//! condition the stream window is compiled under everywhere.
//!
//! # An identity to be signed under
//!
//! On macOS the process that draws the stream is a bare executable inside the shell's bundle,
//! with no bundle and no `Info.plist` of its own. `codesign` then derives its identifier from
//! the file name, so the binary ships signed as `prism-stream` — a name that says nothing about
//! which application it belongs to, and that the system reads as a different piece of software
//! from the one beside it.
//!
//! Linking an `Info.plist` into a `__TEXT,__info_plist` section is what an executable has in
//! place of a bundle. `codesign` prefers it over the file name, and Tauri re-signs this file on
//! its way into the bundle without passing an identifier — so the section is the only thing
//! that makes the identity survive. Nothing else needs it: every other platform signs nothing
//! and reads no such section.
//!
//! # Whether there is a window
//!
//! On macOS and Windows that is the `window` feature. On Linux it is that and `linux-desktop`
//! as well, because the window there has nothing to show without the software decoder the
//! second one brings. Written once here as `cfg(windowed)` rather than spelled out at every gate.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=Info.plist");
    println!("cargo::rustc-check-cfg=cfg(windowed)");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let window = std::env::var_os("CARGO_FEATURE_WINDOW").is_some();
    let desktop = std::env::var_os("CARGO_FEATURE_LINUX_DESKTOP").is_some();

    let windowed = window
        && match os.as_str() {
            "macos" | "windows" => true,
            "linux" => desktop,
            _ => false,
        };

    if windowed {
        println!("cargo::rustc-cfg=windowed");
    }

    if os == "macos" {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");

        println!(
            "cargo::rustc-link-arg-bin=prism-stream=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
