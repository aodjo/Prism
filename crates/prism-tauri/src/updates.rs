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
use tauri::{AppHandle, Manager, State};
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

/// Downloads and installs the newer version, if there is one.
///
/// Returns what was installed, or `None` when there was nothing to install. The application has
/// to be restarted for it to take effect; on Windows the installer does that itself.
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

    let version = update.version.clone();

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| err.to_string())?;

    Ok(Some(version))
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

/// Looks once, shortly after the window opens, and installs anything it finds.
///
/// Spawned rather than awaited: a check that has to finish before a window appears is a window
/// that does not appear when the network is slow, and the reason for updating is not urgent
/// enough to be worth that.
pub fn check_in_background(app: &AppHandle) {
    let app = app.clone();

    tauri::async_runtime::spawn(async move {
        let wanted = {
            let held: State<'_, crate::Held> = app.state();
            let Ok(settings) = held.0.lock() else {
                return;
            };

            settings.auto_update
        };

        if !wanted {
            return;
        }

        let held: State<'_, crate::Held> = app.state();
        let Ok(channel) = following(&held) else {
            return;
        };

        // Failures are not reported. Nothing is wrong with this machine because a server was
        // unreachable, and a window that says so on every launch without a network would be
        // reporting the network rather than the application.
        if let Ok(Some(update)) = look(&app, &channel).await {
            let _ = update.download_and_install(|_, _| {}, || {}).await;
        }
    });
}
