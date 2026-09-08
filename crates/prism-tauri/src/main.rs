//! The Prism shell.
//!
//! What Electron used to be: the windows a person clicks, and nothing else. The stream itself
//! is drawn by a native window this process never touches, and the rule that made the napi
//! surface small applies here unchanged — **a video frame never reaches the webview**. What
//! crosses is control calls and statistics, and statistics no faster than ten a second.
//!
//! The difference from the Electron shell is that there is no boundary left to cross. This is
//! the same Rust that runs the data plane, so a command calls [`prism_core`] directly rather
//! than marshalling through Node. The `.node` addon, and the 538 lines that existed only to
//! describe these calls to JavaScript, are not replaced by anything.

// A second console behind the window on Windows is a developer's tool, not a product's.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account;
mod harness;
mod permissions;
mod sessions;
mod settings;
mod sharing;
mod stream;

use std::sync::Mutex;

use prism_core::identity;
use settings::Settings;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// The settings as they stand, read once at launch and written when somebody changes something.
struct Held(Mutex<Settings>);

/// Turns a failure into the sentence a window should show.
///
/// Commands return `Result<_, String>` rather than a typed error because what reaches the
/// webview is text somebody reads. A structured error would be a shape the frontend then has to
/// translate, and there is nothing on the other side that would do anything different with one
/// kind than another.
fn say(error: &dyn std::error::Error) -> String {
    error.to_string()
}

/// Returns the Prism version string.
///
/// The first thing a window asks for, and the proof that the shell and the core were built from
/// one commit.
#[tauri::command]
#[must_use]
fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Returns the wire format revision this build speaks.
///
/// Compared against `FORMAT_VERSION` from `@prism/protocol` when a session opens: a mismatch
/// means the two halves came from different commits, and a refused session is better than a
/// stream parsed against the wrong layout.
#[tauri::command]
#[must_use]
fn wire_format_version() -> u32 {
    prism_core::net::packet::FORMAT_VERSION
}

/// Returns this machine's public key as hex, creating its long-term key on first use.
///
/// The private half has no call that returns it. What a window needs is the public half: the
/// thing another machine pins, and the thing a support conversation refers to.
///
/// # Errors
///
/// Fails if the key cannot be read or written, which on a machine with a home directory means a
/// permissions problem worth showing rather than working around.
#[tauri::command]
fn identity_public_key() -> Result<String, String> {
    let path = identity::default_path().map_err(|error| say(&error))?;
    let identity = identity::load_or_create(&path).map_err(|error| say(&error))?;

    Ok(identity::to_hex(identity.public()))
}

/// Returns the public keys of every machine this one has paired with.
///
/// # Errors
///
/// Fails if the store exists but cannot be read. A machine that has never paired returns an
/// empty list: that is a state, not a fault.
#[tauri::command]
fn paired_peers() -> Result<Vec<String>, String> {
    let path = identity::default_peers_path().map_err(|error| say(&error))?;

    Ok(identity::known_peers(&path)
        .map_err(|error| say(&error))?
        .iter()
        .map(identity::to_hex)
        .collect())
}

/// Returns everything a person has chosen.
///
/// # Errors
///
/// Never fails: a settings file that cannot be read is an absent one, and absent means defaults.
#[tauri::command]
fn get_settings(held: tauri::State<'_, Held>) -> Result<Settings, String> {
    held.0
        .lock()
        .map(|settings| settings.clone())
        .map_err(|_| "the settings lock was poisoned".to_owned())
}

/// Writes what changed and returns the settings as they now stand.
///
/// Takes the whole object rather than one field, because the window already holds a copy and
/// sending back the part it changed would mean two places deciding what the rest still is.
///
/// # Errors
///
/// Fails if there is no home directory, or the file cannot be written.
#[tauri::command]
fn set_settings(next: Settings, held: tauri::State<'_, Held>) -> Result<Settings, String> {
    let mut settings = held
        .0
        .lock()
        .map_err(|_| "the settings lock was poisoned".to_owned())?;

    settings::save(&next)?;
    *settings = next;

    Ok(settings.clone())
}

/// The page a launch opens.
///
/// Being signed in is the answer to what setup asks, so it is the whole of the question here.
/// The harness may override it, which is how a picture gets taken of a window this machine's own
/// state would not otherwise show.
fn opening_page() -> String {
    harness::forced_page().unwrap_or_else(|| "home.html".to_owned())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(Held(Mutex::new(settings::load())));

            let window =
                WebviewWindowBuilder::new(app, "home", WebviewUrl::App(opening_page().into()))
                    .title("Prism")
                    .inner_size(1280.0, 800.0)
                    .min_inner_size(1040.0, 720.0)
                    .center()
                    // The design puts its own content where a title bar would be, and carries the
                    // traffic lights over the top left of it.
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    // The window is named in the markup, and a second name printed over it by the
                    // system is the design's own header with a title bar drawn on top of it.
                    .hidden_title(true)
                    .background_color(tauri::window::Color(8, 8, 11, 255))
                    .build()?;

            harness::run(&window);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            version,
            wire_format_version,
            identity_public_key,
            paired_peers,
            get_settings,
            set_settings,
            harness::drive_result
        ])
        .run(tauri::generate_context!())
        .expect("the shell could not start");
}
