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

use prism_core::control::permissions::Grant;
use serde::Serialize;

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
    let grant = named(&id).ok_or_else(|| format!("no such grant: {id}"))?;

    // Read the setting and let the lock go before asking, because the system's dialog stands
    // there until somebody answers it and every other command would be waiting behind it.
    let controlling = controlling(&held)?;

    if !prism_core::control::permissions::request(grant) {
        let now = look(controlling);

        if let Some(missing) = now.missing.iter().find(|one| one.id == id) {
            open_settings_pane(&missing.settings_url);
        }

        return Ok(now);
    }

    Ok(look(controlling))
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
    let held = prism_core::control::permissions::check();

    HostPermissions {
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
    fn the_wire_names_are_the_ones_the_window_reads() {
        let written = serde_json::to_string(&describe(Grant::Screen)).expect("writes");

        assert!(written.contains("\"settingsUrl\""));
        assert!(!written.contains("\"settings_url\""));
    }

    #[test]
    fn a_grant_is_named_by_the_id_a_window_passes_back() {
        for grant in [Grant::Screen, Grant::Input] {
            assert_eq!(named(&describe(grant).id), Some(grant));
        }

        assert_eq!(named("clipboard"), None);
    }
}
