//! Keeping the application current.
//!
//! A remote desktop is two programs that have to agree about a wire format, and the two are on
//! different machines belonging to the same person. When one is behind, the session fails in a
//! way neither end can explain — so the version somebody is running is not a preference, it is
//! part of whether the product works at all. That is why this is on by default.
//!
//! # Two lines
//!
//! `production` is what a release is tagged as; `development` is every build between releases.
//! An installation follows the line it was built from until somebody moves it in Settings, and
//! the two are ordered so that moving is rarely needed: a development build is a prerelease of
//! the version being worked toward, so the day that version is released it supersedes every
//! development build of it without anybody choosing anything. See `scripts/version.mjs`.
//!
//! # What this file does not do
//!
//! Verify anything. The signature on an update is checked by the plugin against the public key
//! compiled into this binary, before a byte of it is run. Nothing here can weaken that, and
//! nothing here should try to help.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

/// What this build was made as, written in by the build and read back here.
///
/// `development` when nothing set it, which is what a build from somebody's own machine is.
const CHANNEL: &str = match option_env!("PRISM_CHANNEL") {
    Some(channel) => channel,
    None => "development",
};

/// Which copy of this version this is, counted by whatever produced it.
///
/// Zero for a build nobody counted. Shown beside the version because the version alone does not
/// say which build somebody is reporting a fault in.
const BUILD: &str = match option_env!("PRISM_BUILD") {
    Some(build) => build,
    None => "0",
};

/// What the application is, in the two numbers that answer different questions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Build {
    /// The version, as the updater compares it.
    pub version: String,
    /// Which copy of it this is.
    pub build: String,
    /// The line this build came from.
    pub channel: String,
    /// The line this machine is being offered, which is the one above unless it was changed.
    pub following: String,
}

/// A version that is available and is not the one running.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Available {
    /// What it calls itself.
    pub version: String,
    /// What the release said about it, if anything.
    pub notes: String,
}

/// Returns what this build is.
///
/// The version is the one the bundle carries, which is the number the updater compares. It is
/// not `CARGO_PKG_VERSION`: the crate is versioned with the workspace and stays at `0.0.0` in
/// every build ever made, so a window told that number showed `0.0.0` before an update and
/// `0.0.0` afterwards — which is indistinguishable from an update that never happened.
///
/// # Errors
///
/// Fails only if the settings lock was poisoned.
#[tauri::command]
pub fn build_info(app: AppHandle, held: State<'_, crate::Held>) -> Result<Build, String> {
    Ok(Build {
        version: app.package_info().version.to_string(),
        build: BUILD.to_owned(),
        channel: CHANNEL.to_owned(),
        following: following(&held)?,
    })
}

/// Which line this machine is being offered.
///
/// The setting when it names one, and otherwise the line this build came from — so an
/// installation stays where it started without anybody having chosen.
fn following(held: &State<'_, crate::Held>) -> Result<String, String> {
    let chosen = held
        .0
        .lock()
        .map(|settings| settings.update_channel.clone())
        .map_err(|_| "the settings lock was poisoned".to_owned())?;

    Ok(if chosen.is_empty() {
        CHANNEL.to_owned()
    } else {
        chosen
    })
}

/// Asks whether there is a newer version, without installing it.
///
/// # Errors
///
/// Returns what the updater said if the check could not be made — no network, an endpoint that
/// answered with something else. Finding nothing is not a failure: it answers `None`.
#[tauri::command]
pub async fn check_for_update(
    app: AppHandle,
    held: State<'_, crate::Held>,
) -> Result<Option<Available>, String> {
    let channel = following(&held)?;

    Ok(look(&app, &channel).await?.map(|update| Available {
        version: update.version.clone(),
        notes: update.body.clone().unwrap_or_default(),
    }))
}

/// Downloads the newer version, installs it, and restarts into it.
///
/// The three together, because separately they are three ways to end up somewhere confusing.
/// Installing without restarting replaces the bundle under a process that goes on running the
/// old code, so nothing appears to have happened and the next launch changes version for no
/// reason anybody can see. Asking again after the download is asking a second time about a
/// decision already made.
///
/// Returns the version that was installed once the new copy has been asked for, and this one is
/// on its way out. `Ok(None)` means there was nothing to install after all, which happens when a
/// newer version was found and then withdrawn.
///
/// # Errors
///
/// Returns what the updater said if the download or the install failed. A signature that does
/// not verify fails here, which is the point of it.
#[tauri::command]
pub async fn install_update(
    app: AppHandle,
    held: State<'_, crate::Held>,
) -> Result<Option<String>, String> {
    let channel = following(&held)?;

    let Some(update) = look(&app, &channel).await? else {
        return Ok(None);
    };

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| err.to_string())?;

    // Through the system rather than by running the new binary directly: a copy started the
    // direct way could not reach a machine on the local network for as long as it ran.
    crate::permissions::relaunch(&app);

    Ok(Some(update.version))
}

/// Asks the endpoint for this channel what it has.
///
/// The channel is a header rather than part of the path, because the path is a template the
/// bundler fills in and adding to it would mean the built application could only ever ask about
/// the line it was made on.
async fn look(
    app: &AppHandle,
    channel: &str,
) -> Result<Option<tauri_plugin_updater::Update>, String> {
    app.updater_builder()
        .header("x-prism-channel", channel)
        .map_err(|err| err.to_string())?
        .build()
        .map_err(|err| err.to_string())?
        .check()
        .await
        .map_err(|err| err.to_string())
}

/// How long between one look for a new version and the next.
///
/// Half an hour, because this application is left open. A check made only at launch is a check
/// a machine that has been running since breakfast never makes again — and the builds it is
/// looking for arrive several times a day.
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Looks shortly after the window opens, and goes on looking while the application runs.
///
/// Emits `update:available` carrying an [`Available`] when it finds a version that is newer than
/// the one running and is not the one it last said this about. Installing is a separate
/// decision, made by whoever is looking at the window.
///
/// Spawned rather than awaited: a check that has to finish before a window appears is a window
/// that does not appear when the network is slow, and the reason for updating is not urgent
/// enough to be worth that.
///
/// The setting is read on each pass rather than once. Somebody who turns automatic updates off
/// has turned them off now, not from the next launch.
pub fn check_in_background(app: &AppHandle) {
    let app = app.clone();

    // A thread of its own rather than an asynchronous task, because what this does between
    // checks is wait for half an hour. Sleeping that on a task would want a timer this crate
    // does not otherwise have a runtime dependency for, and a parked thread costs nothing.
    std::thread::spawn(move || {
        // What was last announced, so that a version somebody has already been offered is not
        // offered again every half hour. A newer one still is: this remembers the answer, not
        // the fact of having asked.
        let mut announced: Option<String> = None;

        loop {
            look_once(&app, &mut announced);

            std::thread::sleep(LOOK_EVERY);
        }
    });
}

/// One pass: read the setting, ask, and say what came back if it is worth saying.
///
/// The setting is read here rather than by the caller, so that somebody who turns automatic
/// updates off has turned them off now rather than from the next launch.
fn look_once(app: &AppHandle, announced: &mut Option<String>) {
    let held: State<'_, crate::Held> = app.state();

    let wanted = {
        let Ok(settings) = held.0.lock() else {
            return;
        };

        settings.auto_update
    };

    if !wanted {
        return;
    }

    let Ok(channel) = following(&held) else {
        return;
    };

    // Found, and then said rather than acted on. Installing here would replace the bundle under
    // a running process — which changes nothing a person can see until they next launch, and
    // would do it while they were in the middle of watching another machine. What happens next
    // is their decision, and `install_update` carries it out.
    //
    // Failures are not reported. Nothing is wrong with this machine because a server was
    // unreachable, and a window that says so on every launch without a network would be
    // reporting the network rather than the application.
    let found = tauri::async_runtime::block_on(look(app, &channel));

    if let Ok(Some(update)) = found
        && announced.as_deref() != Some(update.version.as_str())
    {
        *announced = Some(update.version.clone());

        let _ = app.emit(
            "update:available",
            Available {
                version: update.version.clone(),
                notes: update.body.clone().unwrap_or_default(),
            },
        );
    }
}
