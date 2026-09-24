//! Names the one condition the Linux desktop code is compiled under.
//!
//! That code needs two things at once — a Linux target, and the `linux-desktop` feature that
//! brings in the libraries it builds against — and spelling both out on every item behind them
//! is a condition written forty times and got wrong in one. So it is written here, once, as
//! `cfg(linux_desktop)`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(linux_desktop)");

    let linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    let wanted = std::env::var_os("CARGO_FEATURE_LINUX_DESKTOP").is_some();

    if linux && wanted {
        println!("cargo::rustc-cfg=linux_desktop");
    }
}
