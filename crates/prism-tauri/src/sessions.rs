//! What has been watched lately, kept between runs.
//!
//! Beside the settings rather than in them, because it grows on its own and settings do not: a
//! file a person edits should not be one an application appends to every few minutes. It sits in
//! the directory the settings do, under the name the Electron shell wrote, so somebody moving
//! between the two shells keeps the list they had rather than being told they have watched
//! nothing.
//!
//! Nothing here is on the frame path. One entry is written when a stream ends, and the whole list
//! is read when a window asks for it.

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::settings;

/// How many sessions are kept.
///
/// Enough that a week of ordinary use is still there, few enough that the file stays something
/// that can be read in one go without thinking about it.
const KEEP: usize = 50;

/// One stream that ran, kept after it ended.
///
/// Written only for a session that actually established. A connection that failed is a failure to
/// show at the time, not a session to look back on, and a list of them would bury the ones
/// somebody is looking for.
///
/// Field names are camelCase on the wire for the reason the settings' are: the windows reading
/// them are the same TypeScript that read them from Electron, and the file on disk is the one
/// that shell already wrote.
///
/// `host` and `endedAt` are the two an entry is nothing without, so they have no default and an
/// entry missing either is dropped on read. The rest describe a session that is known to have
/// happened, and zero is a truthful answer for any of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The host that was watched, as hex.
    pub host: String,
    /// When the two sides finished the handshake, in milliseconds since the epoch.
    #[serde(default)]
    pub started_at: i64,
    /// When the stream ended.
    pub ended_at: i64,
    /// The mean of every round trip reported while it ran, in milliseconds.
    #[serde(default)]
    pub rtt_ms: f64,
    /// How many frames arrived over the whole session.
    #[serde(default)]
    pub frames: u64,
}

/// The history as it stands.
///
/// Read once at launch and held, rather than read from disk on every call: a window asks for the
/// whole list each time it comes forward, and the only thing that ever changes it is this process
/// finishing a stream.
pub struct Held(Mutex<Vec<Session>>);

impl Held {
    /// Reads the history from disk.
    ///
    /// # Panics
    ///
    /// Never. A history that cannot be read is an empty one.
    #[must_use]
    pub fn new() -> Self {
        Self(Mutex::new(load()))
    }
}

impl Default for Held {
    /// What is already on disk, which is the only sensible state to start from.
    fn default() -> Self {
        Self::new()
    }
}

/// Returns where the history is kept.
///
/// # Errors
///
/// Returns `None` if the system has no home directory to put it under.
#[must_use]
pub fn path() -> Option<PathBuf> {
    Some(settings::profile_dir()?.join("sessions.json"))
}

/// Reads the history, newest first.
///
/// A file that cannot be read is treated as an empty one. Losing the record of what was watched
/// is not worth refusing to start over.
#[must_use]
pub fn load() -> Vec<Session> {
    path()
        .and_then(|path| fs::read_to_string(path).ok())
        .map_or_else(Vec::new, |text| parse(&text))
}

/// Returns the streams that have run on this machine, most recent first.
///
/// # Errors
///
/// Fails only if a thread panicked while holding the history, which is not a state this has a
/// better answer for than saying so.
#[tauri::command]
pub fn get_sessions(held: tauri::State<'_, Held>) -> Result<Vec<Session>, String> {
    held.0
        .lock()
        .map(|history| history.clone())
        .map_err(|_| "the session history lock was poisoned".to_owned())
}

/// Adds one session to the front of the history and writes it back.
///
/// Returns the history as it now stands, because the part of the shell that noticed the stream
/// end is also the part that tells the open windows about it — the same division the Electron
/// main process drew between writing the file and broadcasting the list.
///
/// Not a command, and deliberately: no window records a session. A session is recorded because a
/// stream ended, which is something this process observes rather than something a page asks for.
///
/// # Panics
///
/// Never, including on a poisoned lock. What the lock guards is a list of finished sessions
/// replaced whole, so a panic elsewhere cannot have left it half-written, and refusing to keep a
/// session over it would lose the session for nothing.
pub fn record(held: &Held, session: Session) -> Vec<Session> {
    let mut history = held.0.lock().unwrap_or_else(PoisonError::into_inner);
    let next = newest_first(session, &history);

    // A history that could not be written is a history that will be shorter next launch, which is
    // not a reason to fail at somebody who has just closed a stream.
    let _ = save(&next);

    *history = next;

    history.clone()
}

/// Puts one session at the front and drops whatever falls off the end.
fn newest_first(session: Session, history: &[Session]) -> Vec<Session> {
    let mut next = Vec::with_capacity(KEEP.min(history.len() + 1));

    next.push(session);
    next.extend(history.iter().take(KEEP - 1).cloned());

    next
}

/// Turns the file's text into sessions, newest first.
///
/// Entry by entry rather than the array in one piece, because one record somebody's disk mangled
/// is not a reason to forget the forty-nine good ones around it. Anything that is not an array at
/// all is no history: a file that shape was not written by this.
///
/// The cap applies here as well as on the way in, so a longer file — written by a version that
/// kept more, or by hand — is read as its newest [`KEEP`] rather than growing without end.
fn parse(text: &str) -> Vec<Session> {
    let Ok(stored) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        return Vec::new();
    };

    stored
        .into_iter()
        .filter_map(|one| serde_json::from_value(one).ok())
        .take(KEEP)
        .collect()
}

/// Writes the history, creating the directory if this is the first run.
///
/// # Errors
///
/// Fails if there is no home directory, or the file cannot be written.
fn save(history: &[Session]) -> Result<(), String> {
    let path = path().ok_or_else(|| "no home directory to keep the history in".to_owned())?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    let text = serde_json::to_string_pretty(history).map_err(|error| error.to_string())?;

    fs::write(path, format!("{text}\n")).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session told apart from the others only by when it ended.
    fn ended(at: i64) -> Session {
        Session {
            host: "aa".to_owned(),
            started_at: at - 60_000,
            ended_at: at,
            rtt_ms: 8.5,
            frames: 3_600,
        }
    }

    /// The history a machine that has watched [`KEEP`] streams has, newest first.
    fn full() -> Vec<Session> {
        (0..KEEP)
            .map(|nth| ended(1_000 - i64::try_from(nth).expect("fits")))
            .collect()
    }

    #[test]
    fn the_cap_drops_the_oldest() {
        let next = newest_first(ended(1_001), &full());

        assert_eq!(next.len(), KEEP);
        assert_eq!(next.first().map(|one| one.ended_at), Some(1_001));
        assert_eq!(next.last().map(|one| one.ended_at), Some(952));
    }

    #[test]
    fn a_file_longer_than_the_cap_reads_as_its_newest() {
        let mut stored = full();
        stored.extend((0..10).map(|nth| ended(900 - nth)));

        let history = parse(&serde_json::to_string(&stored).expect("writes"));

        assert_eq!(history.len(), KEEP);
        assert_eq!(history.first().map(|one| one.ended_at), Some(1_000));
    }

    #[test]
    fn a_corrupt_file_reads_as_no_history() {
        // A write that stopped halfway, which is what a machine losing power during one leaves.
        assert!(parse(r#"[{"host":"aa","endedAt":17"#).is_empty());
        assert!(parse("").is_empty());

        // An object where an array should be: whatever wrote this, it was not this.
        assert!(parse(r#"{"host":"aa","endedAt":17}"#).is_empty());
    }

    #[test]
    fn one_bad_entry_does_not_take_the_others_with_it() {
        let stored = r#"[
            {"host":"aa","endedAt":2},
            {"endedAt":3},
            {"host":"bb","endedAt":1,"rttMs":9.5}
        ]"#;

        let history = parse(stored);

        assert_eq!(history.len(), 2);
        assert_eq!(history[0].host, "aa");
        assert_eq!(history[1].rtt_ms, 9.5);
        assert_eq!(history[0].frames, 0);
    }

    #[test]
    fn the_wire_names_are_the_ones_the_windows_read() {
        let written = serde_json::to_string(&ended(17)).expect("writes");

        assert!(written.contains("\"startedAt\""));
        assert!(written.contains("\"endedAt\""));
        assert!(written.contains("\"rttMs\""));
        assert!(!written.contains("\"started_at\""));
    }
}
