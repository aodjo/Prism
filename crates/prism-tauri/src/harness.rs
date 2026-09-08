//! Opening a window, photographing it, and driving it from outside.
//!
//! A developer affordance, and the reason every visual claim in this project can be checked.
//! The Electron shell had this: `PRISM_WINDOW_PAGE` chose what to open, `PRISM_WINDOW_DRIVE` ran
//! a script inside the page and printed what it returned, and `PRISM_WINDOW_SCREENSHOT` wrote a
//! picture and quit. Without an equivalent here, moving shells would mean every statement about
//! what a window looks like becoming an assertion nobody checked.
//!
//! Two things are harder than they were. Electron could photograph its own window; a webview
//! cannot, so the picture is taken by asking the window system for that one window — which is
//! also better, because it never catches the desktop behind it. And `eval` hands nothing back,
//! so a driven script returns its value by calling a command, and this waits for that call.
//!
//! Enabled only by an environment variable naming a file on this machine.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tauri::{Manager, WebviewWindow};

/// Where a driven script's answer arrives.
///
/// A script runs in the page, so the only way back is a call into the shell. This is the other
/// end of that call, set before the script is evaluated and taken by the command that receives
/// the result.
static ANSWER: OnceLock<Mutex<Option<Sender<String>>>> = OnceLock::new();

/// Long enough for the page to have asked who this machine is and drawn the answer.
const SETTLE: Duration = Duration::from_millis(1200);

/// How long a driven script may take before this gives up on it.
const PATIENCE: Duration = Duration::from_secs(30);

/// Receives what a driven script returned.
///
/// Called by the harness's own wrapper rather than by anything a person wrote, and registered
/// only when the shell was started with `PRISM_WINDOW_DRIVE`.
///
/// # Errors
///
/// Never fails. A result that arrives when nothing is waiting is dropped, which is what happens
/// if a script answers twice.
#[tauri::command]
pub fn drive_result(value: String) {
    if let Some(slot) = ANSWER.get()
        && let Ok(mut held) = slot.lock()
        && let Some(sender) = held.take()
    {
        let _ = sender.send(value);
    }
}

/// Which page the harness was told to open, if any.
///
/// A developer affordance: the page to open, so that a picture can be taken of a window this
/// machine's own state would not otherwise show.
#[must_use]
pub fn forced_page() -> Option<String> {
    std::env::var("PRISM_WINDOW_PAGE")
        .ok()
        .filter(|page| !page.is_empty())
}

/// Runs whatever the environment asked for, then quits if it asked for anything.
///
/// Returns without doing anything when no harness variable is set, which is every real run.
pub fn run(window: &WebviewWindow) {
    let drive = std::env::var("PRISM_WINDOW_DRIVE")
        .ok()
        .filter(|path| !path.is_empty());
    let shot = std::env::var("PRISM_WINDOW_SCREENSHOT")
        .ok()
        .filter(|path| !path.is_empty());

    if drive.is_none() && shot.is_none() {
        return;
    }

    let window = window.clone();

    std::thread::spawn(move || {
        std::thread::sleep(SETTLE);

        if let Some(path) = drive {
            match script(&window, &path) {
                Ok(answer) => println!("{answer}"),
                Err(trouble) => {
                    println!("drive failed: {trouble}");
                    std::process::exit(1);
                }
            }
        }

        if let Some(path) = shot
            && let Err(trouble) = photograph(&window, &path)
        {
            println!("screenshot failed: {trouble}");
            std::process::exit(1);
        }

        window.app_handle().exit(0);
    });
}

/// Runs a file of JavaScript inside the page and returns what it evaluated to.
///
/// The script reaches the application exactly the way a person does — through the markup and
/// the bridge the page already has — with no privileged access of its own. It is wrapped so
/// that its value, or the message of whatever it threw, comes back through a command.
///
/// # Errors
///
/// Fails if the file cannot be read, the page will not accept the script, or nothing answers
/// within [`PATIENCE`].
fn script(window: &WebviewWindow, path: &str) -> Result<String, String> {
    let source = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let (sender, receiver) = channel();

    ANSWER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "the harness lock was poisoned".to_owned())?
        .replace(sender);

    // `JSON.stringify` rather than the value itself, because what comes back has to be one
    // string and a script is as likely to return an object as a sentence.
    let wrapped = format!(
        r"(async () => {{
            const answer = (value) =>
                window.__TAURI_INTERNALS__.invoke('drive_result', {{ value }});
            try {{
                const value = await (async () => {{ return ({source}); }})();
                await answer(JSON.stringify(value, null, 2) ?? 'undefined');
            }} catch (error) {{
                await answer('drive threw: ' + (error && error.message ? error.message : String(error)));
            }}
        }})();"
    );

    window.eval(&wrapped).map_err(|error| error.to_string())?;

    receiver
        .recv_timeout(PATIENCE)
        .map_err(|_| format!("nothing came back within {}s", PATIENCE.as_secs()))
}

/// Writes a picture of the window.
///
/// Asks the window system for this one window rather than for a rectangle of the screen, so the
/// picture contains the application and nothing that happened to be behind it.
///
/// # Errors
///
/// Fails if the window cannot be identified, or the capture does not run.
#[cfg(target_os = "macos")]
fn photograph(window: &WebviewWindow, path: &str) -> Result<(), String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let handle = window.ns_window().map_err(|error| error.to_string())?;
    let ns_window = handle.cast::<AnyObject>();

    if ns_window.is_null() {
        return Err("the window has no handle to photograph".to_owned());
    }

    // SAFETY: `ns_window()` returns the window's own `NSWindow` for as long as the window is
    // alive, and this runs while it is on screen. `windowNumber` takes no arguments and returns
    // an integer, which is the identifier the window server knows it by.
    let number: isize = unsafe { msg_send![&*ns_window, windowNumber] };

    let status = std::process::Command::new("screencapture")
        .args(["-x", "-o", "-l", &number.to_string(), path])
        .status()
        .map_err(|error| error.to_string())?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("screencapture exited with {status}"))
    }
}

/// Writes a picture of the window.
///
/// # Errors
///
/// Always, on a platform where this has not been written yet. Said plainly rather than written
/// as an empty file, because a screenshot nobody took is worse than no screenshot at all.
#[cfg(not(target_os = "macos"))]
fn photograph(_window: &WebviewWindow, _path: &str) -> Result<(), String> {
    Err("photographing a window is only implemented on macOS so far".to_owned())
}
