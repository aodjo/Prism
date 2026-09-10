//! Handing this machine's screen out, and saying what is happening while it is.
//!
//! The other half of the application. One machine both watches and is watched, so the session
//! that hands this screen out lives beside the one that opens somebody else's — one identity,
//! one account, one window.
//!
//! The session itself runs on threads of its own inside [`prism_core`]. What crosses into the
//! webview is a handful of counters and an address, asked for ten times a second at the very
//! most, and nothing else ever will: no frame, no texture and no packet is reachable from
//! anything in this file.
//!
//! Under Electron this was three pieces — a class holding the addon's session, a timer pushing
//! snapshots at every open window, and the addon class the timer read through. Only the first
//! said anything about sharing. The addon class existed to cross into Node, and the timer
//! existed because a main process that wanted a window to know something had to send it. Both
//! are gone: a window here asks when it is ready to draw, and what it asks is
//! [`prism_core::control::host`] with nothing in between.

use std::sync::Mutex;

use prism_core::control::host::{self, HostConfig, HostKeys, HostService, Phase};
use prism_core::identity;
use serde::Serialize;
use tauri::State;

use crate::say;
use crate::settings::{self, Settings};

// The settings state that main.rs manages, under a name that says what it holds: this module
// keeps a `Held` of its own, and two of them in one file would be a coin toss at every call.
use crate::Held as Chosen;

/// The slowest and the fastest a session may be started at, in frames per second.
const FPS: (u32, u32) = (1, 480);

/// The least and the most a session may spend, in bits per second.
const BITRATE_BPS: (u32, u32) = (500_000, 200_000_000);

/// One machine's willingness to be watched.
///
/// One session at a time. A second session on the same machine would mean two capture streams
/// and two encoders competing for one GPU, which is slower than either alone and gives both
/// viewers a worse picture than one would have had.
pub struct Held(Mutex<Option<HostService>>);

impl Held {
    /// Builds the sharing state of a machine that is not shared.
    #[must_use]
    pub fn new() -> Self {
        Self(Mutex::new(None))
    }
}

impl Default for Held {
    /// A machine starts unshared, whatever it was doing last time.
    ///
    /// Whether it is meant to be shared is remembered in the settings and acted on at launch;
    /// that is a decision, and this is only the absence of a running session.
    fn default() -> Self {
        Self::new()
    }
}

/// What a host session is doing, as of a moment ago.
///
/// Every field is cheap to produce, which is what lets a window ask for it repeatedly. Nothing
/// here is derived from a frame: these are counters the session thread increments as it goes,
/// read here and turned into text.
///
/// The field names are camelCase because the window reading them is the same TypeScript that
/// read them from Electron, and `HostSnapshot` in `api.d.ts` is the shape it expects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// `opening`, `waiting`, `streaming`, `stopped` or `failed`.
    pub phase: String,
    /// Where the rendezvous server sees this machine, once it has said.
    pub observed: Option<String>,
    /// The address this machine is actually listening on.
    ///
    /// What somebody on the same network has to be told, and the only way to reach this
    /// machine when no rendezvous server is configured.
    pub local: Option<String>,
    /// The watching machine's public key as hex, once one has connected.
    pub peer: Option<String>,
    /// Frames captured, encoded and sent.
    pub frames: String,
    /// Packets put on the wire, parity included.
    pub packets: String,
    /// Bytes put on the wire, headers included.
    pub bytes: String,
    /// What sending has worked out to so far, in bits per second.
    pub bitrate_bps: String,
    /// What went wrong, when the phase is `failed`.
    pub error: Option<String>,
}

/// Renders a counter as decimal text.
///
/// `api.d.ts` declares these counters `bigint`, because the Node-API surface handed them over
/// as one. What crosses here is JSON, which has no such thing: a JSON number becomes a double
/// the moment `JSON.parse` reads it, so a counter past nine quadrillion would arrive rounded
/// with nothing on either side saying it had been. Text crosses exactly, and `BigInt` on the
/// window's side rebuilds the type its code is already written against.
fn counter(value: u64) -> String {
    value.to_string()
}

/// Turns what the session thread recorded into what a window reads.
fn describe(snapshot: &host::Snapshot) -> Snapshot {
    Snapshot {
        phase: match snapshot.phase {
            Phase::Opening => "opening",
            Phase::Waiting => "waiting",
            Phase::Streaming => "streaming",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
        }
        .to_owned(),
        observed: snapshot.observed.map(|address| address.to_string()),
        local: snapshot.local.map(|address| address.to_string()),
        peer: snapshot.peer.as_ref().map(identity::to_hex),
        frames: counter(snapshot.frames),
        packets: counter(snapshot.packets),
        bytes: counter(snapshot.bytes),
        bitrate_bps: counter(snapshot.bitrate_bps),
        error: snapshot.error.clone(),
    }
}

/// Builds a host configuration out of what a person has chosen.
///
/// Anything the settings do not speak to keeps the core's default, so a session started from a
/// window is configured the way the core thinks a session should be.
///
/// The two numbers are clamped rather than taken as given. They come from fields somebody
/// types into, and a rate of zero is a session that sends nothing while a bitrate no network
/// carries is a session that stalls — both fail somewhere a long way from the person who typed
/// them. Running at the nearest sensible value is a better answer than failing at the first
/// frame with a number nobody can connect to what they did.
///
/// # Errors
///
/// Fails if the address to listen on is not one.
fn configure(settings: &Settings) -> Result<HostConfig, String> {
    Ok(HostConfig {
        bind: settings
            .bind
            .parse()
            .map_err(|_| format!("{} is not an address to listen on", settings.bind))?,
        // Left as a name. Whether it resolves is a question for the session, which asks it as
        // it opens so that a region added since this machine started sharing is one this
        // session already knows about. Empty means reachable only directly.
        rendezvous: Some(settings.rendezvous.clone()).filter(|name| !name.is_empty()),
        fps: settings.fps.clamp(FPS.0, FPS.1),
        bitrate_bps: u32::try_from(settings.bitrate_bps)
            .unwrap_or(u32::MAX)
            .clamp(BITRATE_BPS.0, BITRATE_BPS.1),
        inject_input: settings.control,
        ..HostConfig::default()
    })
}

/// Loads this machine's identity and every machine that may watch it.
///
/// An empty list is not refused. Sharing is this machine saying its own screen may be watched
/// by the account it belongs to — a statement about itself, true whether or not a second
/// machine exists yet. Somebody who turns it on before installing Prism anywhere else has done
/// nothing wrong, and a switch that refused to move until some other machine appeared would be
/// answering a question nobody asked.
///
/// # Errors
///
/// Fails if there is no home directory, or if the identity or the list of machines is there
/// and cannot be read.
fn keys() -> Result<HostKeys, String> {
    let path = identity::default_path().map_err(|error| say(&error))?;
    let identity = identity::load_or_create(&path).map_err(|error| say(&error))?;
    let peers = identity::default_peers_path().map_err(|error| say(&error))?;
    let allowed = identity::known_peers(&peers).map_err(|error| say(&error))?;

    Ok(HostKeys { identity, allowed })
}

/// Writes down whether this machine is meant to be shared.
///
/// Separate from whether it is shared this second: a session can end for reasons nobody chose
/// — a network that went away, a machine that slept — and the next launch should put this
/// machine back the way it was left rather than the way it happened to fail.
///
/// Written through the settings the shell already holds rather than straight to the file, so
/// that a window asking what the settings are gets the answer this just made true. The file is
/// written first, as everywhere else here: settings in memory that disagree with settings on
/// disk are settings that will disagree with themselves at the next launch.
///
/// # Errors
///
/// Fails if the settings cannot be written.
fn remember(chosen: &State<'_, Chosen>, on: bool) -> Result<(), String> {
    let mut settings = chosen
        .0
        .lock()
        .map_err(|_| "the settings lock was poisoned".to_owned())?;

    if settings.sharing == on {
        return Ok(());
    }

    let next = Settings {
        sharing: on,
        ..settings.clone()
    };

    settings::save(&next)?;
    *settings = next;

    Ok(())
}

/// Starts sharing this machine and returns what the session is doing a moment later.
///
/// Returns as soon as the session's thread exists. Binding a socket, registering with a
/// rendezvous server and waiting for somebody to connect all happen on that thread, which is
/// what makes this safe to call from a window: the wait can be minutes long, and none of it
/// happens here.
///
/// That it was turned on is written down only once the session is running. Somebody who turned
/// this machine on for another one of theirs meant it to stay on — a switch that quietly went
/// back to off at every restart would be a machine reachable only while somebody has a window
/// open on it, which is the opposite of the point — but a start that failed is not a state
/// worth restoring.
///
/// # Errors
///
/// Fails if this machine may not record its screen, if the address to listen on cannot be
/// parsed, if this machine's identity cannot be read, or if the session's thread cannot be
/// spawned. All four are worth showing rather than swallowing: each one leaves a machine that
/// looks shared and is not.
/// Puts a machine back to sharing, if that is how it was left.
///
/// Somebody who turned this machine on for another one of theirs meant it to stay on. Without
/// this the switch went quietly back to off at every launch, which makes a machine reachable
/// only while somebody has a window open on it — the opposite of what sharing is for. The
/// setting has recorded the answer since the beginning; nothing acted on it.
///
/// Failures are not reported and not written down. There is no window listening yet, and a
/// machine that could not start sharing this time is one that should still try next time: the
/// usual reason is a screen recording grant given while Prism was running, which the next launch
/// is exactly what fixes.
pub fn resume(app: &tauri::AppHandle) {
    use tauri::Manager as _;

    let chosen: State<'_, Chosen> = app.state();

    let wanted = chosen
        .0
        .lock()
        .map(|settings| settings.sharing)
        .unwrap_or(false);

    if !wanted {
        return;
    }

    let held: State<'_, Held> = app.state();
    let _ = start_sharing(held, chosen);
}

#[tauri::command]
pub fn start_sharing(held: State<'_, Held>, chosen: State<'_, Chosen>) -> Result<Snapshot, String> {
    // Before anything else, because a machine that may not record its screen shares perfectly:
    // it registers, it is found, it accepts a session, it agrees a codec, and it sends nothing
    // at all. What somebody sees at the other end is a black window and no reason for it — and
    // the reason was knowable here, one call, before any of it started.
    //
    // Screen recording only. Accessibility missing means a session nobody can type into, which
    // is a screen share and a thing somebody may well want; this one means there is nothing to
    // share.
    if !prism_core::control::permissions::check().screen {
        return Err(
            "Prism may not record this screen yet. Allow Screen Recording for Prism in \
             System Settings, then start Prism again."
                .to_owned(),
        );
    }

    let mut session = held
        .0
        .lock()
        .map_err(|_| "the sharing lock was poisoned".to_owned())?;

    // A window that asks twice — two of them are open, or one was reloaded — is told what the
    // running session is doing rather than given a second one.
    if let Some(service) = session.as_ref() {
        return Ok(describe(&service.snapshot()));
    }

    let settings = chosen
        .0
        .lock()
        .map_err(|_| "the settings lock was poisoned".to_owned())?
        .clone();

    let service =
        HostService::start(configure(&settings)?, keys()?).map_err(|error| say(&error))?;
    let snapshot = describe(&service.snapshot());

    *session = Some(service);

    // Let go of the session before the settings are written, so a window polling for a
    // snapshot in that moment is answered rather than held behind a disk write.
    drop(session);

    remember(&chosen, true)?;

    Ok(snapshot)
}

/// Stops sharing, if this machine was.
///
/// Waits for the session's thread rather than only asking it to end, so that by the time a
/// window is told sharing stopped, the capture and the socket are actually gone. The thread
/// notices between frames and while waiting for a client, so this is a frame interval rather
/// than a wait somebody would sit through.
///
/// Safe to call when nothing is running: a window that stops sharing and then closes would
/// otherwise have to remember which it did first.
///
/// # Errors
///
/// Fails if the settings cannot be written. The session has ended by then either way.
#[tauri::command]
pub fn stop_sharing(held: State<'_, Held>, chosen: State<'_, Chosen>) -> Result<(), String> {
    let running = held
        .0
        .lock()
        .map_err(|_| "the sharing lock was poisoned".to_owned())?
        .take();

    // Taken out of the lock before being joined, so that a window polling for a snapshot
    // during the join is answered instead of waiting behind it.
    if let Some(service) = running {
        service.join();
    }

    remember(&chosen, false)
}

/// Returns what this machine's own session is doing, or nothing when it is not shared.
///
/// This is what a window polls, ten times a second at the very most: a panel that changes
/// faster than somebody can read it costs frames to produce and tells them nothing.
///
/// # Errors
///
/// Fails only if a thread panicked while holding the session, which is a broken shell rather
/// than a machine that is not shared.
#[tauri::command]
pub fn sharing_state(held: State<'_, Held>) -> Result<Option<Snapshot>, String> {
    let session = held
        .0
        .lock()
        .map_err(|_| "the sharing lock was poisoned".to_owned())?;

    Ok(session
        .as_ref()
        .map(|service| describe(&service.snapshot())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_nobody_could_capture_at_is_brought_back_in_range() {
        let settings = Settings {
            fps: 100_000,
            bitrate_bps: 1,
            ..Settings::default()
        };
        let config = configure(&settings).expect("the default address parses");

        assert_eq!(config.fps, FPS.1);
        assert_eq!(config.bitrate_bps, BITRATE_BPS.0);
    }

    #[test]
    fn a_bitrate_past_what_a_u32_holds_lands_on_the_ceiling() {
        let settings = Settings {
            bitrate_bps: u64::MAX,
            ..Settings::default()
        };
        let config = configure(&settings).expect("the default address parses");

        assert_eq!(config.bitrate_bps, BITRATE_BPS.1);
    }

    #[test]
    fn no_rendezvous_means_reachable_only_directly() {
        let settings = Settings {
            rendezvous: String::new(),
            ..Settings::default()
        };
        let config = configure(&settings).expect("the default address parses");

        assert!(config.rendezvous.is_none());
    }

    #[test]
    fn an_address_that_is_not_one_is_said_rather_than_started_with() {
        let settings = Settings {
            bind: "the usual port".to_owned(),
            ..Settings::default()
        };

        assert!(configure(&settings).is_err());
    }

    #[test]
    fn the_counters_cross_as_text_the_window_can_rebuild_a_bigint_from() {
        let snapshot = describe(&host::Snapshot {
            phase: Phase::Streaming,
            observed: None,
            local: None,
            peer: None,
            frames: u64::MAX,
            packets: 0,
            bytes: 0,
            bitrate_bps: 0,
            audio_frames: 0,
            error: None,
        });

        let written = serde_json::to_string(&snapshot).expect("writes");

        assert!(written.contains("\"frames\":\"18446744073709551615\""));
        assert!(written.contains("\"bitrateBps\""));
        assert!(written.contains("\"phase\":\"streaming\""));
    }
}
