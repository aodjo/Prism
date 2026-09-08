//! Generates the context Tauri needs at compile time.
//!
//! Reads `tauri.conf.json` and the capability files beside it and turns them into the code
//! `tauri::generate_context!` expands to, so that the window definitions and the permission set
//! are settled when the binary is built rather than read off disk on a machine we do not
//! control.

fn main() {
    tauri_build::build();
}
