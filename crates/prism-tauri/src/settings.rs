//! What a person has chosen, and where it is kept.
//!
//! The same file the Electron shell reads and writes, in the same place, deliberately. Somebody
//! who has been using this application has an account signed in, machines listed and a name for
//! this computer; a shell that kept its settings somewhere new would open as a fresh install and
//! ask them to do all of it again. The two shells share one profile for as long as both exist,
//! and when the Electron one goes the file stays where it is.
//!
//! Nothing here is on the frame path. It is read once at launch and written when somebody
//! changes something.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where this project's rendezvous is, which is what a fresh installation uses.
const PRISM_RENDEZVOUS: &str = "rv.presm.kr:47300";

/// Where this project's account server is.
///
/// A different name from the rendezvous, and one that resolves to a single machine. The
/// rendezvous name is several — one record per region — because a signalling server is a
/// stateless introducer and any of them will do. Accounts are a file on one disk that no server
/// tells another about, so the same arrangement would sign somebody in against whichever region
/// answered and deny their account existed on the next call.
const PRISM_ACCOUNT_SERVER: &str = "https://accounts.presm.kr";

/// Everything a person has chosen.
///
/// Field names are camelCase on the wire because the windows reading them are the same
/// TypeScript that read them from Electron, and a rename would be a change to the file on disk
/// as well as to the markup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Rendezvous server to find hosts through, or empty to connect directly.
    pub rendezvous: String,
    /// Account server to sign in to, which is what makes a machine's own machines find it.
    pub account_server: String,
    /// What the person at this machine calls it, or empty for none.
    pub nickname: String,
    /// Address to listen on while shared.
    pub bind: String,
    /// Frames per second to capture at while shared.
    pub fps: u32,
    /// What to spend while shared, in bits per second.
    pub bitrate_bps: u64,
    /// Whether this machine is meant to be shared, read at launch to put it back as it was left.
    pub sharing: bool,
    /// Whether to send input to the host, or only watch.
    pub control: bool,
    /// Whether to even out arrival jitter at the cost of a little latency.
    pub smooth: bool,
    /// Where each paired host can be reached directly, keyed by its public key.
    pub addresses: BTreeMap<String, String>,
    /// The machines somebody wants at the front of the list, by public key.
    pub pinned: Vec<String>,
    /// Whether to look for a new version at all.
    ///
    /// On, because a remote desktop that is out of date on one of the two machines is a session
    /// that fails for a reason neither end can see, and because the person who would otherwise
    /// have to notice is the same person who would have to fix it.
    ///
    /// Finding one is where this stops. Installing is asked for, because replacing the bundle
    /// under somebody in the middle of watching another machine is not a thing to do quietly.
    pub auto_update: bool,
    /// Which builds this machine is offered: `production` or `development`.
    ///
    /// Empty means the line this build came from, which is what an installation follows until
    /// somebody moves it. Setting it is how a machine joins or leaves the development line
    /// without being reinstalled.
    pub update_channel: String,
    /// Which language the windows are in: `en`, `ko`, or empty to follow the machine.
    ///
    /// Empty by default, because the machine already knows what language its owner reads and
    /// asking again is asking a question that has been answered.
    #[serde(default)]
    pub language: String,
    /// Whether somebody has been all the way through setup on this machine.
    ///
    /// Needed because the permissions step cannot be finished in one sitting: macOS only reads a
    /// new grant when the application starts again, so setup ends with a restart in the middle
    /// of it. Without a record of having reached the end, that restart looks exactly like an
    /// ordinary launch by somebody signed in, and the window that opens is the home one.
    ///
    /// This is not a second answer to "is there an account" — it is only ever consulted
    /// alongside that one, never instead of it. An earlier version of this flag was consulted
    /// on its own and outlived a sign-out, which opened a home window built on an account the
    /// machine was no longer on.
    #[serde(default)]
    pub setup_finished: bool,
    /// The blocks on the home board, in the arrangement somebody left them.
    ///
    /// Empty until somebody moves something, and empty is what a fresh machine wants: the window
    /// lays out a default board from whatever machines the account has, which is a better first
    /// screen than one saved before those machines existed. Once anything here is set it is
    /// taken as the whole arrangement, so a block missing from this list is a block that was
    /// deliberately removed rather than one that has yet to be placed.
    #[serde(default)]
    pub board: Vec<Block>,
}

/// One block on the home board.
///
/// Positions and sizes are in holes rather than pixels. The board is a lattice and a block can
/// only ever sit on it, so storing pixels would be storing a number that has to be divided back
/// on every read and would go wrong the first time the lattice changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Block {
    /// What tells this block from the others on the board.
    pub id: String,
    /// What it draws: `machine`, `machines`, `mine`, `sessions`, `terms` or `link`.
    pub kind: String,
    /// Which machine it is about, as hex, for the kinds that are about one.
    pub host: String,
    /// How many holes from the left edge of the board.
    pub x: u32,
    /// How many holes from the top.
    pub y: u32,
    /// How many holes across.
    pub w: u32,
    /// How many holes down.
    pub h: u32,
    /// What it is called, or empty to use the name the machine already has.
    pub label: String,
    /// Which figures it shows, by the names the window knows them by.
    pub fields: Vec<String>,
    /// Its colour: one entry for a flat colour, several for a gradient across them.
    pub accent: Vec<String>,
}

impl Default for Block {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: String::new(),
            host: String::new(),
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            label: String::new(),
            fields: Vec::new(),
            accent: Vec::new(),
        }
    }
}

impl Default for Settings {
    /// The settings a machine uses before anybody has changed anything.
    ///
    /// Control on, because a remote desktop nobody can type on is a screen share. Smoothing off,
    /// because it costs latency and latency is what this is for.
    ///
    /// The listening port is fixed rather than left to the operating system. With a rendezvous
    /// it makes no difference, since the port is discovered either way — but without one, a
    /// machine on an operating-system-chosen port is a machine nobody can reach: the other end
    /// has no way to learn a number nothing told it.
    ///
    /// The bitrate is where a session starts rather than what it is held to: congestion control
    /// takes it as its opening figure and moves from there, down as fast as a path needs and up
    /// again when it will carry more. So the number to choose is what a link that can afford it
    /// should open at, not the least any link might manage — and a whole desktop at sixty frames
    /// is soft at twenty-four megabits in a way a person reads as a bad picture rather than as a
    /// setting.
    fn default() -> Self {
        Self {
            rendezvous: PRISM_RENDEZVOUS.to_owned(),
            account_server: PRISM_ACCOUNT_SERVER.to_owned(),
            nickname: String::new(),
            bind: "0.0.0.0:47200".to_owned(),
            fps: 60,
            bitrate_bps: 40_000_000,
            sharing: false,
            control: true,
            smooth: false,
            addresses: BTreeMap::new(),
            pinned: Vec::new(),
            auto_update: true,
            update_channel: String::new(),
            language: String::new(),
            setup_finished: false,
            board: Vec::new(),
        }
    }
}

/// Returns the directory the two shells share.
///
/// Built to match what Electron's `app.getPath('userData')` produced for a package named
/// `@prism/client`, because that is where the file already is on every machine this has ever
/// run on.
///
/// # Errors
///
/// Returns `None` if the system has no home directory to put it under.
#[must_use]
pub fn profile_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library")
            .join("Application Support")
    } else if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(
            || std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")),
            |config| Some(PathBuf::from(config)),
        )?
    };

    Some(base.join("@prism").join("client"))
}

/// Returns where the settings file is.
///
/// # Errors
///
/// Returns `None` if there is no home directory to put it under.
#[must_use]
pub fn path() -> Option<PathBuf> {
    Some(profile_dir()?.join("settings.json"))
}

/// Reads the settings, falling back to the defaults for anything missing or unreadable.
///
/// A corrupt file is treated as an absent one: refusing to start because a settings file was
/// truncated is a worse failure than starting with defaults. `serde`'s `default` on every field
/// is what drops a key this version no longer has a name for, which is the same rule the
/// TypeScript reader applies.
#[must_use]
pub fn load() -> Settings {
    let Some(path) = path() else {
        return Settings::default();
    };

    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes the settings, creating the directory if this is the first run.
///
/// # Errors
///
/// Fails if there is no home directory, or if the file cannot be written.
pub fn save(settings: &Settings) -> Result<(), String> {
    let path = path().ok_or_else(|| "no home directory to keep settings in".to_owned())?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;

    // Replaced rather than written over. `load` cannot tell a file a power cut left half
    // written from one that was never there, so a truncated write does not read as a bad
    // settings file — it reads as a machine that has never been set up, and comes back with
    // the setup window, no nickname, and sharing off.
    prism_core::store::replace(&path, format!("{text}\n").as_bytes())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_do_not_survive_a_read() {
        // `setupDone` was taken out of the product; a file written before that still has it, and
        // carrying it forward is how a setting nothing reads outlives the code that read it.
        let stored = r#"{"nickname":"mac","setupDone":true,"fps":120}"#;
        let settings: Settings = serde_json::from_str(stored).expect("parses");

        assert_eq!(settings.nickname, "mac");
        assert_eq!(settings.fps, 120);

        let written = serde_json::to_string(&settings).expect("writes");
        assert!(!written.contains("setupDone"));
    }

    #[test]
    fn a_missing_key_falls_back_to_its_default() {
        let settings: Settings = serde_json::from_str("{}").expect("parses");

        assert_eq!(settings.bind, "0.0.0.0:47200");
        assert!(settings.control);
        assert!(!settings.sharing);
    }

    #[test]
    fn the_wire_names_are_the_ones_the_windows_read() {
        let written = serde_json::to_string(&Settings::default()).expect("writes");

        assert!(written.contains("\"accountServer\""));
        assert!(written.contains("\"bitrateBps\""));
        assert!(!written.contains("\"account_server\""));
    }

    #[test]
    fn a_board_survives_the_trip_the_window_sends_it_on() {
        // Quoted with two hashes: a colour is written `"#35d6ff"`, and `"#` would close a
        // raw string opened with one.
        let stored = r##"{"board":[{"id":"b1","kind":"machine","host":"ab","x":1,"y":1,"w":17,
            "h":9,"label":"","fields":["fps","rtt"],"accent":["#35d6ff","#7c5cff"]}]}"##;
        let settings: Settings = serde_json::from_str(stored).expect("parses");
        let block = settings.board.first().expect("one block");

        assert_eq!(block.kind, "machine");
        assert_eq!((block.x, block.y, block.w, block.h), (1, 1, 17, 9));
        assert_eq!(block.accent.len(), 2);

        // Sent back under the names the window reads, not the ones Rust writes them in.
        let written = serde_json::to_string(&settings).expect("writes");

        assert!(written.contains("\"board\""));
        assert!(written.contains("\"accent\""));
    }

    #[test]
    fn a_board_written_before_blocks_existed_reads_as_an_empty_one() {
        // Which is what makes the window lay out a default rather than open on nothing.
        let settings: Settings = serde_json::from_str(r#"{"nickname":"mac"}"#).expect("parses");

        assert!(settings.board.is_empty());
    }
}
