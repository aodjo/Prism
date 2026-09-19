//! Gives the stream binary an identity of its own to be signed under.
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
//! that makes the identity survive.
//!
//! Nothing else needs it. Every other platform signs nothing and reads no such section.

fn main() {
    println!("cargo:rerun-if-changed=Info.plist");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");

    println!(
        "cargo:rustc-link-arg-bin=prism-stream=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
