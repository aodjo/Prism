//! What this machine still has to allow before it can be shared.
//!
//! Both grants fail quietly when they are missing — capture delivers black frames and silence,
//! injection posts events that go nowhere — so a window that never asked would show a host that
//! looks like it is working and is not. The asking happens here, before a session rather than
//! during one.
//!
//! The system prompts at most once in the life of an application. After that a request returns
//! the standing refusal and shows nothing, because the decision has moved to Privacy settings
//! where only a person can change it. So [`request_permission`] is prepared to send somebody to
//! that pane instead, which is the one thing the shell does here that the core cannot: the core
//! has no business starting processes, and the shell already knows how to open a link.
//!
//! Nothing here is on the frame path. These are control calls, made when a window is drawn and
//! when somebody presses a button in it.

use prism_core::control::permissions::{Grant, Permissions};
use serde::Serialize;

/// What the window calls the local network permission when it asks for it.
///
/// Kept out of [`Grant`] deliberately. Everything in that enum can be checked and reported as
/// held or missing; this one cannot be checked at all, and putting it there would make every
/// answer about it a guess.
const LOCAL_NETWORK: &str = "network";

/// Where the system keeps the switch for it.
const LOCAL_NETWORK_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork";

/// One thing the system still has to allow.
///
/// Field names are camelCase on the wire because the window reading them is the same TypeScript
/// that read them from Electron, and `MissingGrant` in `api.d.ts` is what it expects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingGrant {
    /// Which grant this is: `screen` or `input`, and what to pass back to ask for it.
    pub id: String,
    /// What the system's own settings call it.
    pub name: String,
    /// Why a host needs it, in one sentence.
    pub purpose: String,
    /// A link that opens the settings pane holding it.
    pub settings_url: String,
}

/// What this machine currently allows a host to do.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPermissions {
    /// Whether this platform has a local-network privacy settings pane.
    pub network_settings: bool,
    /// Whether the screen may be recorded, which also covers capturing its audio.
    pub screen: bool,
    /// Whether this machine may be controlled.
    pub input: bool,
    /// What is still missing, ready to show.
    pub missing: Vec<MissingGrant>,
}

/// Returns what the system allows, without prompting for anything.
///
/// Safe to call as often as a window redraws: nothing here raises a dialog.
///
/// Whether the session will accept the client's input is read from the settings rather than
/// passed by the window, because the window asks this question with no arguments and the shell
/// is where that answer already lives. A machine set to show its screen and nothing else is
/// never asked to justify wanting control of itself.
///
/// # Errors
///
/// Fails only if the settings lock was poisoned, which means another command panicked while
/// holding it.
#[tauri::command]
pub fn permissions(held: tauri::State<'_, crate::Held>) -> Result<HostPermissions, String> {
    Ok(look(controlling(&held)?))
}

/// Asks the system for one grant, opens its settings pane if the answer is already no, and
/// returns what is held afterwards.
///
/// A refusal here is not a failure to report. The prompt appears once and never again, so the
/// only way forward from `false` is the pane that holds the standing answer, and opening it is
/// more use to somebody than an error message naming it.
///
/// The pane is opened only for a grant that is actually missing for this session. Asking for
/// Accessibility on a machine that is sharing its screen without accepting input would be
/// sending somebody to hand over control they did not want to give.
///
/// # Errors
///
/// Fails if `id` is not a grant this build knows, or if the settings lock was poisoned. A grant
/// the system refuses is not an error: it comes back in `missing`.
#[tauri::command]
pub fn request_permission(
    id: String,
    held: tauri::State<'_, crate::Held>,
) -> Result<HostPermissions, String> {
    // Read the setting and let the lock go before asking, because the system's dialog stands
    // there until somebody answers it and every other command would be waiting behind it.
    let controlling = controlling(&held)?;

    // The local network is not a [`Grant`], because the system offers nothing to ask it with:
    // there is no preflight, no request, and no answer afterwards. So it is not checked, is
    // never in `missing`, and never refuses a share — and the one thing that can be done about
    // it is to put somebody in front of the switch.
    //
    // Saying it was held, which the first-run screen did, is the one answer that must not be
    // given. It is the grant that stops two machines on one network reaching each other while
    // everything through the rendezvous goes on working, so a screen that ticks it green sends
    // the one person who could fix it looking somewhere else.
    if id == LOCAL_NETWORK {
        if !cfg!(target_os = "macos") {
            return Err("Local network privacy settings are only available on macOS.".to_owned());
        }
        open_settings_pane(LOCAL_NETWORK_PANE);

        return Ok(look(controlling));
    }

    let grant = named(&id).ok_or_else(|| format!("no such grant: {id}"))?;

    Ok(request_missing(
        grant,
        controlling,
        prism_core::control::permissions::check,
        prism_core::control::permissions::request,
        open_settings_pane,
    ))
}

/// Requests a grant only when the current session still needs it, preserving existing consent.
fn request_missing(
    grant: Grant,
    controlling: bool,
    mut read: impl FnMut() -> Permissions,
    ask: impl FnOnce(Grant) -> bool,
    open: impl FnOnce(&str),
) -> HostPermissions {
    let before = read();
    if !before.missing(controlling).contains(&grant) {
        return present(before, controlling);
    }

    let granted = ask(grant);
    let now = present(read(), controlling);
    if !granted && let Some(missing) = now.missing.iter().find(|one| one.id == describe(grant).id) {
        open(&missing.settings_url);
    }
    now
}

/// Starts Prism again, for a grant the system will not report until it does.
///
/// Screen recording is read once per process on macOS: a grant given while the application is
/// running is a grant it goes on saying it does not have. Nothing in a window can work around
/// that, so this exists to do the one thing that does.
///
/// Does not return when it works. A caller reaches the line after it only where the platform
/// refused, which is why it is not `-> !`.
#[tauri::command]
pub fn restart(app: tauri::AppHandle) {
    relaunch(&app);
}

/// Starts Prism again the way the system starts it, and lets this copy go.
///
/// Every restart goes through here — the one a missing grant asks for, and the one after an
/// update is installed. The second used Tauri's own restart until a copy started that way was
/// found unable to reach the machine it was trying to watch on the local network, for as long
/// as it ran, while the same bundle opened from the Dock reached it at once.
///
/// Returns once the relaunch has been handed over and this copy told to exit, or not at all
/// where there is no such hand-over and Tauri's own restart is the only one left.
pub(crate) fn relaunch(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    if reopen() {
        app.exit(0);

        return;
    }

    app.restart();
}

/// Asks the system to open this application again, once this copy of it has gone.
///
/// Tauri's own restart runs the binary inside the bundle directly. That produces a process which
/// happens to live in an application rather than a running application: the system launched
/// nothing, so it has nothing recorded against it, and the privacy grants that belong to the
/// bundle are not offered to it. Which is exactly what this restart exists to collect — so it
/// would restart, ask again, and be told no a second time.
///
/// The wait is a shell holding on until this process is gone. `open` on a bundle that is still
/// running brings the old copy forward instead of starting a new one, and the old copy is the
/// one on its way out.
///
/// # Returns
///
/// Whether the relaunch was handed over. False leaves the caller to restart the other way, which
/// is better than not restarting.
#[cfg(target_os = "macos")]
fn reopen() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };

    // `…/Prism.app/Contents/MacOS/prism-tauri` — three steps up is the bundle.
    let Some(bundle) = exe.ancestors().nth(3) else {
        return false;
    };

    if bundle.extension().is_none_or(|kind| kind != "app") {
        return false;
    }

    let waiting = format!(
        "while kill -0 {} 2>/dev/null; do sleep 0.2; done; open {}",
        std::process::id(),
        shell_quoted(&bundle.display().to_string()),
    );

    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(waiting)
        .spawn()
        .is_ok()
}

/// Wraps a path so a shell reads it as one word.
///
/// Single quotes, with any single quote in the path closed and reopened around an escaped one.
/// The path comes from the running executable rather than from anybody's typing, but a command
/// line assembled without quoting is a command line that breaks on the first space.
#[cfg(target_os = "macos")]
fn shell_quoted(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Returns whether the session will accept the client's input.
///
/// # Errors
///
/// Fails if the settings lock was poisoned.
fn controlling(held: &tauri::State<'_, crate::Held>) -> Result<bool, String> {
    held.0
        .lock()
        .map(|settings| settings.control)
        .map_err(|_| "the settings lock was poisoned".to_owned())
}

/// Reads the system's answer and dresses it up for a window.
fn look(controlling: bool) -> HostPermissions {
    present(prism_core::control::permissions::check(), controlling)
}

/// Describes a permission snapshot without requesting or resetting any system permission.
fn present(held: Permissions, controlling: bool) -> HostPermissions {
    HostPermissions {
        network_settings: cfg!(target_os = "macos"),
        screen: held.screen,
        input: held.input,
        missing: held
            .missing(controlling)
            .into_iter()
            .map(describe)
            .collect(),
    }
}

/// Returns the grant a window named, or `None` if this build has no such grant.
fn named(id: &str) -> Option<Grant> {
    match id {
        "screen" => Some(Grant::Screen),
        "input" => Some(Grant::Input),
        _ => None,
    }
}

/// Returns everything a window needs to show one missing grant and ask for it again.
fn describe(grant: Grant) -> MissingGrant {
    MissingGrant {
        id: match grant {
            Grant::Screen => "screen",
            Grant::Input => "input",
        }
        .to_owned(),
        name: grant.name().to_owned(),
        purpose: grant.purpose().to_owned(),
        settings_url: grant.settings_url().to_owned(),
    }
}

/// Opens the settings pane holding a grant.
///
/// Handed to `open`, which is what resolves an `x-apple.systempreferences:` link to the pane it
/// names. Waiting on it waits for `open` itself and not for System Settings, so this returns as
/// soon as the request has been handed over — and reaping it there is what keeps a child around
/// only for as long as it takes to launch the thing.
///
/// A failure is dropped rather than returned. The caller is on its way to telling somebody what
/// is still missing and where to find it; a pane that did not open changes nothing about that
/// answer, and turning it into an error would replace the useful half of the reply with the
/// useless half.
#[cfg(target_os = "macos")]
fn open_settings_pane(url: &str) {
    let _ = std::process::Command::new("open").arg(url).status();
}

/// Opens the settings pane holding a grant.
///
/// There is no pane, on a system that gates none of this. Nothing reaches here anyway: with
/// both grants reported as held, nothing is ever missing to open a pane for.
#[cfg(not(target_os = "macos"))]
fn open_settings_pane(url: &str) {
    let _ = url;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granted_permissions_are_returned_without_prompting_or_opening_settings() {
        for grant in [Grant::Screen, Grant::Input] {
            let held = request_missing(
                grant,
                true,
                || Permissions {
                    screen: true,
                    input: true,
                },
                |_| panic!("an existing grant must not be requested again"),
                |_| panic!("an existing grant must not open settings"),
            );
            assert!(held.missing.is_empty());
        }
    }

    #[test]
    fn a_session_without_control_does_not_request_accessibility() {
        let held = request_missing(
            Grant::Input,
            false,
            || Permissions {
                screen: true,
                input: false,
            },
            |_| panic!("view-only sharing must not request control"),
            |_| panic!("view-only sharing must not open Accessibility settings"),
        );
        assert!(held.missing.is_empty());
    }

    #[test]
    fn a_new_grant_is_reflected_in_the_returned_snapshot() {
        let mut reads = 0;
        let held = request_missing(
            Grant::Input,
            true,
            || {
                reads += 1;
                Permissions {
                    screen: true,
                    input: reads > 1,
                }
            },
            |grant| {
                assert_eq!(grant, Grant::Input);
                true
            },
            |_| panic!("a newly granted permission must not open settings"),
        );
        assert!(held.input);
        assert!(held.missing.is_empty());
        assert_eq!(reads, 2);
    }

    #[test]
    fn a_standing_refusal_only_opens_the_requested_permission_pane() {
        let mut opened = None;
        let held = request_missing(
            Grant::Screen,
            true,
            || Permissions {
                screen: false,
                input: true,
            },
            |grant| {
                assert_eq!(grant, Grant::Screen);
                false
            },
            |url| opened = Some(url.to_owned()),
        );
        assert_eq!(opened.as_deref(), Some(Grant::Screen.settings_url()));
        assert_eq!(held.missing.len(), 1);
        assert!(held.input);
    }

    #[test]
    fn a_grant_observed_after_a_refused_request_does_not_reopen_settings() {
        let mut reads = 0;
        let held = request_missing(
            Grant::Input,
            true,
            || {
                reads += 1;
                Permissions {
                    screen: true,
                    input: reads > 1,
                }
            },
            |_| false,
            |_| panic!("a concurrent grant must not reopen settings"),
        );
        assert!(held.input);
    }

    #[test]
    fn the_wire_names_are_the_ones_the_window_reads() {
        let written = serde_json::to_string(&describe(Grant::Screen)).expect("writes");

        assert!(written.contains("\"settingsUrl\""));
        assert!(!written.contains("\"settings_url\""));
    }

    #[test]
    fn the_platform_reports_whether_network_privacy_settings_exist() {
        let held = look(false);
        assert_eq!(held.network_settings, cfg!(target_os = "macos"));
        let value = serde_json::to_value(held).unwrap();
        assert_eq!(value["networkSettings"], cfg!(target_os = "macos"));
    }

    #[test]
    fn a_grant_is_named_by_the_id_a_window_passes_back() {
        for grant in [Grant::Screen, Grant::Input] {
            assert_eq!(named(&describe(grant).id), Some(grant));
        }

        assert_eq!(named("clipboard"), None);
    }
}
