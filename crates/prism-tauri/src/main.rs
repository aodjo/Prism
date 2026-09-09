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
mod updates;
mod windows;

use std::sync::Mutex;

use prism_core::identity;
use settings::Settings;
use tauri::{Emitter, Manager};

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
/// The version the bundle carries, which is the one the updater compares — not the crate's,
/// which is the workspace's `0.0.0` in every build there has ever been.
#[tauri::command]
#[must_use]
fn version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
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

/// Ends the session and undoes what signing in set up.
///
/// Signing out is more than forgetting a token. What setup asked for was an account; without one
/// there is nothing for the home window to draw, and the name this machine goes by came from the
/// account it has just left. Leaving either behind is what makes a signed-out application look
/// like a signed-in one with the names rubbed out.
///
/// Wrapped here rather than done in [`account`], because which windows exist is this file's
/// business and not that module's.
///
/// # Errors
///
/// Fails if the account cannot be reached for long enough to say so, or the settings cannot be
/// written. The token is gone from this machine either way.
#[tauri::command(async)]
fn sign_out(
    app: tauri::AppHandle,
    account: tauri::State<'_, account::Held>,
    chosen: tauri::State<'_, Held>,
    sharing: tauri::State<'_, sharing::Held>,
) -> Result<account::AccountState, String> {
    let state = account::account_sign_out(account, chosen.clone())?;

    // Stopped rather than left running. A machine goes on offering its screen to whoever the
    // account last said may watch it, and somebody who has signed out has said they are done.
    sharing::stop_sharing(sharing, chosen.clone())?;

    {
        let mut settings = chosen
            .0
            .lock()
            .map_err(|_| "the settings lock was poisoned".to_owned())?;

        settings.nickname = String::new();
        // Whoever signs in next starts at the beginning, which includes being shown what this
        // machine still has to allow. Leaving it set would carry one person's answer over to
        // somebody else's first run.
        settings.setup_finished = false;
        settings::save(&settings)?;
    }

    windows::open_setup(&app).map_err(|error| error.to_string())?;

    Ok(state)
}

/// The page a launch opens.
///
/// Both halves have to be true to skip setup: there has to be an account, and somebody has to
/// have been all the way through. Neither answers for the other.
///
/// The account alone is not enough because setup cannot be finished in one sitting. Its
/// permissions step ends by sending somebody to System Settings, and macOS reads a new grant
/// only when the application starts again — so a launch in the middle of setup looks exactly
/// like an ordinary one by somebody signed in, and used to land on the home window, skipping
/// the step that had just sent them away.
///
/// The record alone is not enough either, and that was the earlier bug: consulted on its own it
/// outlived a sign-out, and the application kept opening on a home window built out of an
/// account it was no longer on. Reading both is what makes each one's failure harmless.
///
/// The harness may override it, which is how a picture gets taken of a window this machine's own
/// state would not otherwise show.
fn opening() -> (&'static str, &'static str) {
    match harness::forced_page().as_deref() {
        Some("setup.html") => ("setup", "setup.html"),
        Some("index.html") => ("settings", "index.html"),
        Some(_) => ("home", "home.html"),
        None if account::signed_in_before() && settings::load().setup_finished => {
            ("home", "home.html")
        }
        None => ("setup", "setup.html"),
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            app.manage(Held(Mutex::new(settings::load())));
            app.manage(account::Held::new());
            app.manage(sharing::Held::new());
            // Before the stream, whose recorder looks this up when a session ends.
            app.manage(sessions::Held::new());

            // What the stream reports as it runs, and what it leaves behind when it stops. Both
            // are called from the threads reading the client's output rather than from this
            // one, which is why neither may block: an event is queued and a session is a file
            // append, and nothing here waits for a window to be listening.
            let reporting = app.handle().clone();
            let recording = app.handle().clone();

            app.manage(stream::Held::new(
                Box::new(move |snapshot| {
                    let _ = reporting.emit("stream:state", snapshot);
                }),
                Box::new(move |session| {
                    let held = recording.state::<sessions::Held>();
                    let history = sessions::record(&held, session);

                    let _ = recording.emit("sessions:changed", history);
                }),
            ));

            // Once, on its own, after everything a window needs is managed. It reads the
            // setting, so it has to come after the settings are; it does not block a window,
            // so it comes before one is staged rather than after.
            updates::check_in_background(app.handle());

            let (label, page) = opening();
            let window = windows::stage(app.handle(), label, page)?;

            // Whenever the window comes forward. That is the moment somebody is about to look at
            // the list of their machines, and the moment they are most likely to have just signed
            // in on another one. The list is not this machine's to decide, so it goes stale as
            // soon as anything happens anywhere else.
            let asking = app.handle().clone();

            window.on_window_event(move |event| {
                if !matches!(event, tauri::WindowEvent::Focused(true)) {
                    return;
                }

                let asking = asking.clone();

                // On its own thread: this runs on the one drawing the window, and asking a
                // server across the internet from here would freeze the window it is redrawing.
                std::thread::spawn(move || {
                    let account = asking.state::<account::Held>();
                    let chosen = asking.state::<Held>();

                    if account::refresh(&account, &chosen).unwrap_or(false)
                        && let Ok(state) = account::account_state(account, chosen)
                    {
                        let _ = asking.emit("account:state", state);
                    }
                });
            });

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
            permissions::permissions,
            permissions::request_permission,
            sharing::start_sharing,
            sharing::stop_sharing,
            sharing::sharing_state,
            stream::stream_connect,
            stream::stream_disconnect,
            stream::stream_state,
            sessions::get_sessions,
            account::account_state,
            account::account_challenge,
            account::account_register,
            account::account_sign_in,
            account::account_rename,
            account::account_forget_device,
            sign_out,
            windows::finish_setup,
            windows::open_settings,
            windows::fit,
            updates::build_info,
            updates::check_for_update,
            updates::install_update,
            harness::drive_result
        ])
        .run(tauri::generate_context!())
        .expect("the shell could not start");
}
