//! Watching another machine, in a window this process does not draw.
//!
//! The stream runs as a process of its own. Under Electron that was forced — a window has to be
//! driven from the process's main thread on macOS, and Electron already owned that thread — but
//! the arrangement was the better one for reasons that have nothing to do with Node, so it
//! survives the move. The capture, decode and present path runs where the interface cannot
//! stall it, in a process that can crash without taking the window with it, and the rule that a
//! frame never reaches the shell holds by construction rather than by discipline.
//!
//! What crosses back is the child's own output, read a line at a time on threads of its own: a
//! phase, the terms the two sides settled on, and a line of counters once a second. Nothing
//! here is on the frame path, and the only thing this module and the stream share is a pipe.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::State;

use crate::sessions::Session;
use crate::settings::Settings;

// The settings state that main.rs manages, under a name that says what it holds: this module
// keeps a `Held` of its own, and two of them in one file would be a coin toss at every call.
use crate::Held as Chosen;

/// How many lines of the child's output to keep, which is what explains a failure.
const LOG_LINES: usize = 40;

/// How long the client waits without a packet before deciding the host has gone.
///
/// The stream ends when the host stops sending rather than after a fixed number of frames, and
/// a person switching windows is not a reason to give up on it.
const IDLE_TIMEOUT_MS: u64 = 10_000;

/// The shortest gap between two reports to the shell.
///
/// The child prints its counters once a second, so this drops nothing a person reads; what it
/// is here for is the burst of lines a failing session writes on its way out, which would
/// otherwise cross the boundary as fast as a pipe can carry them. A phase change goes through
/// whatever this says, because there are only a handful in a session and losing one leaves a
/// window claiming a stream that has already ended.
const REPORT_EVERY: Duration = Duration::from_millis(100);

/// What the stream is doing.
///
/// The names on the wire are the ones the windows already switch on, because the markup reading
/// them is the same TypeScript that read them from Electron.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Nothing has been watched since this shell started.
    Idle,
    /// A client is running and the two sides have not finished the handshake.
    Connecting,
    /// Somebody is watching another machine's screen.
    Streaming,
    /// The stream ended, either because it was asked to or because it ran out of host to watch.
    Stopped,
    /// The client exited on its own, with something in the log about why.
    Failed,
}

/// What the stream agreed to carry, said once when the session opens.
///
/// Fixed for the life of a session: both sides negotiated it and neither can change it without
/// opening a new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Terms {
    /// The video codec, as the two sides named it.
    pub codec: String,
    /// Pixels across, or zero when this side asked for whatever the host's screen is.
    pub width: u32,
    /// Pixels down, or zero for the same reason.
    pub height: u32,
    /// Frames a second the host will send.
    pub fps: u32,
}

/// What the stream is doing right now, as of the last line of counters.
///
/// Numbers only. This is the whole of what a running stream tells the interface, and it is why
/// the interface can show a figure without a frame ever reaching it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// The round trip the client last measured, in milliseconds.
    pub rtt_ms: f64,
    /// Frames arriving a second, over the last second.
    pub fps: f64,
    /// What is actually arriving, in megabits a second.
    pub mbps: f64,
    /// Frames reassembled since the stream opened.
    pub frames: u64,
}

/// Everything the interface knows about the stream.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// What the stream is doing.
    pub phase: Phase,
    /// The host being watched, as hex, while a stream is running.
    pub host: Option<String>,
    /// What the two sides agreed to, once they have.
    pub terms: Option<Terms>,
    /// What is happening, as of the last second.
    pub stats: Option<Stats>,
    /// The last few lines the client wrote, which is what explains a failure.
    pub log: Vec<String>,
}

/// Called whenever anything about the stream changes.
///
/// The shell wires this to a window event. It is called from the threads reading the client's
/// output and never with the state locked, so it may call back into this module.
pub type Watcher = Box<dyn Fn(&Snapshot) + Send + Sync + 'static>;

/// Called once when a session that established has ended, with what it came to.
///
/// Separate from [`Watcher`] because it fires once rather than continuously, and because what it
/// carries is written down rather than drawn. It takes the session by value, since the one thing
/// the shell does with it is hand it to [`crate::sessions::record`].
pub type Recorder = Box<dyn Fn(Session) + Send + Sync + 'static>;

/// The stream as it stands, and the client that is producing it.
///
/// One mutex over both, so that the process and what the interface is told about it cannot
/// disagree: whoever ends the stream and whoever notices it ending are different threads.
struct Inner {
    phase: Phase,
    host: Option<String>,
    terms: Option<Terms>,
    stats: Option<Stats>,
    log: VecDeque<String>,
    child: Option<Child>,
    /// Which run the fields above describe.
    ///
    /// A client that is being killed goes on writing for as long as it takes to die, and a
    /// person who reconnects immediately has a second one running by then. Everything the
    /// threads of a run do is refused once this has moved on, which is what stops a dead
    /// stream's last line from being written over a live one's state.
    run: u64,
    started_at: Option<i64>,
    /// Whether this end asked the stream to stop.
    ///
    /// A client that is killed exits on a signal rather than with a status, which is
    /// indistinguishable from a crash unless somebody remembers having asked. Ending a session
    /// on purpose is not a failure and must not be reported to a person as one.
    stopping: bool,
    /// Every round trip reported this session, summed, so the mean survives the session.
    rtt_sum: f64,
    /// How many were reported.
    rtt_count: u32,
    /// When the shell was last told anything, which is what holds the reports to ten a second.
    reported: Instant,
}

impl Inner {
    /// The state of a machine that has never streamed.
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            host: None,
            terms: None,
            stats: None,
            log: VecDeque::new(),
            child: None,
            run: 0,
            started_at: None,
            stopping: false,
            rtt_sum: 0.0,
            rtt_count: 0,
            reported: Instant::now(),
        }
    }

    /// Copies out what the interface is allowed to see, which is all of it.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            phase: self.phase,
            host: self.host.clone(),
            terms: self.terms.clone(),
            stats: self.stats.clone(),
            log: self.log.iter().cloned().collect(),
        }
    }

    /// Keeps a line of the client's output, dropping the oldest once there are enough.
    fn remember(&mut self, line: String) {
        self.log.push_back(line);

        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// Turns what just ended into a session, if it was one.
    ///
    /// A run that never got past the handshake is not history: nothing was watched, and the
    /// failure has already been reported as a failure. The second call returns `None`, because
    /// the mark it reads is cleared as it goes.
    fn take_session(&mut self) -> Option<Session> {
        let (Some(started_at), Some(host)) = (self.started_at, self.host.clone()) else {
            return None;
        };

        self.started_at = None;

        Some(Session {
            host,
            started_at,
            ended_at: now_ms(),
            rtt_ms: if self.rtt_count > 0 {
                self.rtt_sum / f64::from(self.rtt_count)
            } else {
                0.0
            },
            frames: self.stats.as_ref().map_or(0, |stats| stats.frames),
        })
    }
}

/// What the threads of a run share with the shell.
struct Shared {
    inner: Mutex<Inner>,
    watch: Watcher,
    record: Recorder,
}

/// The one stream this shell runs, and everything known about it.
///
/// Held by the shell for its whole life rather than created per connection, so that the window
/// that asks what is happening and the window that started it are answered from one place.
pub struct Held(Arc<Shared>);

impl Held {
    /// Creates the holder, reporting to `watch` and handing finished sessions to `record`.
    ///
    /// Both are called from the threads reading the client, not from the shell's own, so
    /// anything they touch has to be safe to touch from either.
    #[must_use]
    pub fn new(watch: Watcher, record: Recorder) -> Self {
        Self(Arc::new(Shared {
            inner: Mutex::new(Inner::new()),
            watch,
            record,
        }))
    }

    /// Returns what the stream is doing.
    ///
    /// # Errors
    ///
    /// Fails only if a thread died holding the state, which nothing here does.
    pub fn state(&self) -> Result<Snapshot, String> {
        Ok(self.locked()?.snapshot())
    }

    /// Starts a stream onto a host.
    ///
    /// `address` is where the host can be reached directly, or empty to be introduced by the
    /// rendezvous server. `settings` supplies that server and whether this end may type on the
    /// other machine.
    ///
    /// Returns a moment after starting rather than once anything is on screen: the handshake
    /// happens in the client, and the phase reaching `streaming` is what says it worked.
    ///
    /// # Errors
    ///
    /// Fails if a stream is already running, if there is neither an address nor a rendezvous
    /// server to find the host with, or if the client cannot be found or started.
    pub fn start(
        &self,
        host: &str,
        address: &str,
        settings: &Settings,
    ) -> Result<Snapshot, String> {
        if address.is_empty() && settings.rendezvous.is_empty() {
            return Err("set a rendezvous server, or give the host address directly".to_owned());
        }

        let binary = find_client()?;
        let mut inner = self.locked()?;

        if inner.child.is_some() {
            return Err("a stream is already running".to_owned());
        }

        let mut child = Command::new(&binary)
            .args(arguments(host, address, settings))
            // Nothing is ever written to it, and a client that inherited this process's input
            // would be reading the same terminal a developer is typing into.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", binary.display()))?;

        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();

            return Err("the stream started with no output to read it through".to_owned());
        };

        inner.run = inner.run.wrapping_add(1);
        inner.phase = Phase::Connecting;
        inner.host = Some(host.to_owned());
        inner.terms = None;
        inner.stats = None;
        inner.log.clear();
        inner.started_at = None;
        inner.stopping = false;
        inner.rtt_sum = 0.0;
        inner.rtt_count = 0;
        inner.reported = Instant::now();
        inner.child = Some(child);

        let run = inner.run;
        let snapshot = inner.snapshot();
        drop(inner);

        // Two threads because two pipes: the client writes its counters to one and whatever
        // went wrong to the other, and a single thread reading them in turn would sit on the
        // quiet one while the loud one filled its buffer.
        let readers = [
            self.spawn_reader(run, stdout),
            self.spawn_reader(run, stderr),
        ];

        let shared = Arc::clone(&self.0);
        std::thread::spawn(move || {
            for reader in readers {
                let _ = reader.join();
            }

            conclude(&shared, run);
        });

        (self.0.watch)(&snapshot);

        Ok(snapshot)
    }

    /// Ends the stream, if one is running.
    ///
    /// Returns as soon as the client has been told to go, not once it has: the phase becomes
    /// `stopped` when the process actually ends, and that arrives through the watcher like
    /// every other change.
    ///
    /// The Electron shell asked politely first and insisted after a second and a bit, because
    /// SDL turns a terminate signal into a quit event the stream only reads between frames —
    /// so a stream whose host had gone quiet did not notice for as long as its idle timeout.
    /// The standard library has only the insistent kind of kill, so that is what this sends;
    /// what it costs is the client's own tidy shutdown, not anything the person watching sees.
    ///
    /// # Errors
    ///
    /// Fails only if a thread died holding the state, which nothing here does.
    pub fn stop(&self) -> Result<Snapshot, String> {
        let mut inner = self.locked()?;

        if let Some(child) = inner.child.as_mut() {
            let _ = child.kill();
            inner.stopping = true;
        }

        Ok(inner.snapshot())
    }

    /// Takes the state, turning a poisoned lock into the sentence a window shows.
    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Inner>, String> {
        self.0
            .inner
            .lock()
            .map_err(|_| "the stream lock was poisoned".to_owned())
    }

    /// Starts a thread that reads one of the client's pipes until it closes.
    fn spawn_reader(&self, run: u64, source: impl Read + Send + 'static) -> JoinHandle<()> {
        let shared = Arc::clone(&self.0);

        std::thread::spawn(move || absorb(&shared, run, source))
    }
}

/// Reads a pipe to its end, keeping the state up with what the client says.
///
/// Blocking reads on a thread of their own, which is the whole of what this needs: the control
/// plane is allowed to wait, and a runtime here would buy nothing that a pipe and a thread do
/// not already do.
fn absorb(shared: &Arc<Shared>, run: u64, source: impl Read) {
    for line in BufReader::new(source).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(mut inner) = shared.inner.lock() else {
            return;
        };

        if inner.run != run {
            return;
        }

        let was = inner.phase;

        // The client says this exactly once, when the handshake completes. Reading it is what
        // turns "a process is running" into "a person is watching a screen".
        if line.contains("session established") {
            inner.phase = Phase::Streaming;

            if inner.started_at.is_none() {
                inner.started_at = Some(now_ms());
            }
        }

        if let Some(terms) = read_terms(&line) {
            inner.terms = Some(terms);
        }

        if let Some(stats) = read_stats(&line) {
            inner.rtt_sum += stats.rtt_ms;
            inner.rtt_count = inner.rtt_count.saturating_add(1);
            inner.stats = Some(stats);
        }

        inner.remember(line);

        if inner.phase == was && inner.reported.elapsed() < REPORT_EVERY {
            continue;
        }

        inner.reported = Instant::now();
        let snapshot = inner.snapshot();
        drop(inner);

        (shared.watch)(&snapshot);
    }
}

/// Settles what the run came to, once the client has nothing left to say.
///
/// Both pipes being at their end means the process has closed them, which it does when it
/// exits — so the wait under the lock is a formality that returns at once, and holding the lock
/// across it is what keeps the phase and the process from disagreeing for a moment in between.
fn conclude(shared: &Arc<Shared>, run: u64) {
    let Ok(mut inner) = shared.inner.lock() else {
        return;
    };

    if inner.run != run {
        return;
    }

    let ended_well = inner
        .child
        .take()
        .and_then(|mut child| child.wait().ok())
        .is_some_and(|status| status.success());

    inner.phase = if ended_well || inner.stopping {
        Phase::Stopped
    } else {
        Phase::Failed
    };

    let session = inner.take_session();
    inner.host = None;
    let snapshot = inner.snapshot();
    drop(inner);

    (shared.watch)(&snapshot);

    if let Some(session) = session {
        (shared.record)(session);
    }
}

/// Builds the command line that watches one host.
///
/// Every decision here is one the client would otherwise have to guess at: which machine to
/// trust, how to reach it, and whether this end is watching or working.
fn arguments(host: &str, address: &str, settings: &Settings) -> Vec<String> {
    let mut args = vec![
        "client".to_owned(),
        "--display".to_owned(),
        "--peer-key".to_owned(),
        host.to_owned(),
    ];

    if address.is_empty() {
        args.push("--rendezvous".to_owned());
        args.push(settings.rendezvous.clone());
    } else {
        args.push("--host".to_owned());
        args.push(address.to_owned());
    }

    if !settings.control {
        args.push("--no-input".to_owned());
    }

    if settings.smooth {
        args.push("--mode".to_owned());
        args.push("smooth".to_owned());
    }

    args.push("--idle-timeout-ms".to_owned());
    args.push(IDLE_TIMEOUT_MS.to_string());

    args
}

/// Finds the headless client.
///
/// Four places, in the order they should win: an explicit override for somebody testing a
/// build, the copy shipped beside the shell, the one a macOS bundle keeps in its resources, and
/// whichever build profile a development run left it in. Everything but the override is
/// relative to this executable rather than to the working directory, because a shell launched
/// from a dock icon has no working directory worth the name.
///
/// # Errors
///
/// Fails if none of them is there, naming everywhere it looked — a missing client is a
/// packaging mistake, and the paths are what say which one.
fn find_client() -> Result<PathBuf, String> {
    let name = format!("prism-cli{}", std::env::consts::EXE_SUFFIX);
    let mut looked = Vec::new();

    if let Some(named) = std::env::var_os("PRISM_CLI").filter(|path| !path.is_empty()) {
        looked.push(PathBuf::from(named));
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(beside) = exe.parent()
    {
        looked.push(beside.join(&name));
        // A macOS bundle runs from `Contents/MacOS` and keeps what it ships in
        // `Contents/Resources`.
        looked.push(beside.join("..").join("Resources").join(&name));
        // A development run has the shell in one of the two profile directories and the client
        // in either, since the two are built by separate commands and nothing makes them agree.
        looked.push(beside.join("..").join("release").join(&name));
        looked.push(beside.join("..").join("debug").join(&name));
    }

    if let Some(found) = looked.iter().find(|candidate| candidate.is_file()) {
        return Ok(found.clone());
    }

    let paths: Vec<String> = looked
        .iter()
        .map(|candidate| candidate.display().to_string())
        .collect();

    Err(format!(
        "could not find {name}; looked in {}",
        paths.join(", ")
    ))
}

/// Reads the line the client prints once, naming what the two sides settled on.
///
/// Parsed from the client's own output rather than passed back some other way, because that is
/// where the negotiation happens and the output is already being read. A line that does not
/// match is not an error: most of them are something else.
fn read_terms(line: &str) -> Option<Terms> {
    let (_, rest) = line.split_once("client: terms ")?;

    let width: u32 = field(rest, "width")?.parse().ok()?;
    let height: u32 = field(rest, "height")?.parse().ok()?;

    // A client that will take whatever the host's screen is says so with the largest number the
    // field holds. That is not a size anybody wants shown to them.
    let capped = width < 65_534 && height < 65_534;

    Some(Terms {
        codec: field(rest, "codec")?.to_owned(),
        width: if capped { width } else { 0 },
        height: if capped { height } else { 0 },
        fps: field(rest, "fps")?.parse().ok()?,
    })
}

/// Reads the line the client prints once a second while it is running.
fn read_stats(line: &str) -> Option<Stats> {
    let (_, rest) = line.split_once("client: stats ")?;

    let rtt_us: f64 = field(rest, "rtt_us")?.parse().ok()?;
    let kbps: f64 = field(rest, "kbps")?.parse().ok()?;

    Some(Stats {
        rtt_ms: rtt_us / 1000.0,
        fps: field(rest, "fps")?.parse().ok()?,
        mbps: kbps / 1000.0,
        frames: field(rest, "frames")?.parse().ok()?,
    })
}

/// Reads one `key=value` out of the machine-readable half of a line.
///
/// The client prints each of these lines twice, once for a person and once in this shape,
/// precisely so that nothing has to match the wording of a sentence somebody may reword.
fn field<'a>(rest: &'a str, key: &str) -> Option<&'a str> {
    rest.split_ascii_whitespace()
        .find_map(|token| token.strip_prefix(key)?.strip_prefix('='))
}

/// The current time in milliseconds since the epoch.
///
/// The unit the history is written in, which the Electron shell wrote with `Date.now()` into the
/// same file. A session stamped in anything else would sort into the wrong place.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Starts a stream onto a paired host.
///
/// `address` is what the window was told to use; empty means fall back to what was stored for
/// this host, and then to the rendezvous server.
///
/// # Errors
///
/// Fails if a stream is already running, if there is no way to reach the host, or if the client
/// cannot be found or started.
#[tauri::command]
pub fn stream_connect(
    host: String,
    address: String,
    stream: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<Snapshot, String> {
    let chosen = settings
        .0
        .lock()
        .map_err(|_| "the settings lock was poisoned".to_owned())?
        .clone();

    // What the window passed, or what was stored for this host last time. The rendezvous server
    // is the fallback after both, and starting refuses when there is neither.
    let direct = match address.trim() {
        "" => chosen.addresses.get(&host).map_or("", String::as_str),
        given => given,
    };

    stream.start(&host, direct, &chosen)
}

/// Ends the stream, if one is running.
///
/// # Errors
///
/// Fails only if a thread died holding the state, which nothing here does.
#[tauri::command]
pub fn stream_disconnect(stream: State<'_, Held>) -> Result<Snapshot, String> {
    stream.stop()
}

/// Returns what the stream is doing.
///
/// Asked once when a window opens; everything after that arrives through the watcher, so this
/// is not what keeps a window up to date.
///
/// # Errors
///
/// Fails only if a thread died holding the state, which nothing here does.
#[tauri::command]
pub fn stream_state(stream: State<'_, Held>) -> Result<Snapshot, String> {
    stream.state()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_are_read_out_of_the_line_written_for_a_program() {
        let terms = read_terms("client: terms codec=H264 width=1920 height=1080 fps=60 audio=1")
            .expect("the line names terms");

        assert_eq!(terms.codec, "H264");
        assert_eq!(terms.width, 1920);
        assert_eq!(terms.height, 1080);
        assert_eq!(terms.fps, 60);
    }

    #[test]
    fn asking_for_whatever_the_host_has_is_not_a_size() {
        let terms = read_terms("client: terms codec=H264 width=65535 height=65535 fps=60 audio=0")
            .expect("the line names terms");

        assert_eq!(terms.width, 0);
        assert_eq!(terms.height, 0);
    }

    #[test]
    fn the_sentence_written_for_a_person_is_not_parsed() {
        assert!(
            read_terms("client: agreed H264, 1920x1080, 60 fps, 24.0 Mbps, audio on").is_none()
        );
        assert!(read_stats("client: session established with 1.2.3.4:47200 (ab)").is_none());
    }

    #[test]
    fn counters_come_back_in_the_units_a_window_shows() {
        let stats = read_stats("client: stats rtt_us=8200 fps=59.9 kbps=18400 frames=1234")
            .expect("the line names counters");

        assert!((stats.rtt_ms - 8.2).abs() < 1e-9);
        assert!((stats.fps - 59.9).abs() < 1e-9);
        assert!((stats.mbps - 18.4).abs() < 1e-9);
        assert_eq!(stats.frames, 1234);
    }

    #[test]
    fn an_address_is_used_instead_of_the_rendezvous_and_not_beside_it() {
        let settings = Settings::default();
        let args = arguments("ab12", "10.0.0.4:47200", &settings);

        assert!(args.contains(&"--host".to_owned()));
        assert!(!args.contains(&"--rendezvous".to_owned()));
        assert!(!args.contains(&"--no-input".to_owned()));
    }

    #[test]
    fn watching_without_typing_is_asked_for_by_name() {
        let settings = Settings {
            control: false,
            smooth: true,
            ..Settings::default()
        };
        let args = arguments("ab12", "", &settings);

        assert!(args.contains(&"--no-input".to_owned()));
        assert_eq!(
            args.iter().position(|arg| arg == "--mode").map(|at| at + 1),
            args.iter().position(|arg| arg == "smooth")
        );
        assert!(args.contains(&"--rendezvous".to_owned()));
    }

    #[test]
    fn the_wire_names_are_the_ones_the_windows_read() {
        let written = serde_json::to_string(&Snapshot {
            phase: Phase::Streaming,
            host: None,
            terms: None,
            stats: Some(Stats {
                rtt_ms: 8.2,
                fps: 60.0,
                mbps: 18.4,
                frames: 12,
            }),
            log: Vec::new(),
        })
        .expect("writes");

        assert!(written.contains("\"phase\":\"streaming\""));
        assert!(written.contains("\"rttMs\""));
        assert!(!written.contains("\"rtt_ms\""));
    }

    #[test]
    fn a_run_that_never_established_is_not_history() {
        let mut inner = Inner::new();
        inner.host = Some("ab12".to_owned());

        assert!(inner.take_session().is_none());

        inner.started_at = Some(1);
        inner.rtt_sum = 30.0;
        inner.rtt_count = 3;

        let session = inner.take_session().expect("it established");

        assert_eq!(session.host, "ab12");
        assert!((session.rtt_ms - 10.0).abs() < 1e-9);
        assert!(inner.take_session().is_none());
    }

    #[test]
    fn the_log_keeps_the_end_of_what_was_said() {
        let mut inner = Inner::new();

        for line in 0..LOG_LINES + 5 {
            inner.remember(line.to_string());
        }

        assert_eq!(inner.log.len(), LOG_LINES);
        assert_eq!(inner.log.front().map(String::as_str), Some("5"));
    }
}
