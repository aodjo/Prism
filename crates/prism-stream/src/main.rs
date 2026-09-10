//! The process the shell runs to show another machine.
//!
//! It takes no arguments. One [`ipc::Start`] arrives on standard input, the window opens, and
//! everything the run has to say leaves on standard output as [`ipc::Event`]s until it ends.
//!
//! Taking no arguments is the point. This was a command with a dozen flags, and the shell built
//! a command line and read English sentences back to work out what was happening — so the set
//! of things the shell could ask for was the set of flags that happened to exist, and the words
//! could not be changed without breaking it. Now the two ends share types.

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use prism_core::control::client::{Report, Reporter};
use prism_stream::ipc;

/// What a run came to, and whether the shell heard why on the channel it reads messages on.
///
/// The distinction exists because the shell keeps both channels in one log. A reason sent as an
/// [`ipc::Event::Ended`] and printed to standard error as well arrives twice, and somebody
/// looking at a failed stream reads the same sentence under itself.
enum Outcome {
    /// It ran and ended.
    Fine,
    /// It failed, and the message carrying the reason was written.
    Told,
    /// It failed with nowhere to say so but standard error.
    Untold(String),
}

fn main() -> ExitCode {
    match serve() {
        Outcome::Fine => ExitCode::SUCCESS,
        Outcome::Told => ExitCode::FAILURE,
        Outcome::Untold(error) => {
            // Standard output carries messages, so this cannot go there. The shell keeps what
            // arrives here as the log that explains a failure.
            let _ = writeln!(io::stderr(), "{error}");

            ExitCode::FAILURE
        }
    }
}

/// Reads the one message that says what to do, and does it.
fn serve() -> Outcome {
    let start: ipc::Start = match ipc::read(&mut io::stdin().lock()) {
        Ok(Some(start)) => start,
        Ok(None) => return Outcome::Untold("nothing said what to stream".to_owned()),
        Err(err) => return Outcome::Untold(format!("could not read what to stream: {err}")),
    };

    // Behind a lock because the receive thread, the decode thread and this one all report, and
    // a message half-written by one and half by another is a message neither side can read.
    let out = Arc::new(Mutex::new(io::stdout()));
    let say = {
        let out = Arc::clone(&out);
        Reporter::new(move |report| {
            let Ok(mut out) = out.lock() else {
                return;
            };
            let _ = ipc::write(&mut *out, &translate(report));
        })
    };

    let outcome = watch(&start, &say);

    let told = match out.lock() {
        Ok(mut out) => ipc::write(
            &mut *out,
            &ipc::Event::Ended {
                error: outcome.as_ref().err().cloned(),
            },
        )
        .is_ok(),
        Err(_) => false,
    };

    match outcome {
        Ok(()) => Outcome::Fine,
        Err(_) if told => Outcome::Told,
        Err(error) => Outcome::Untold(error),
    }
}

/// Opens the window and shows the host until the stream ends.
#[cfg(all(feature = "window", any(target_os = "macos", target_os = "windows")))]
fn watch(start: &ipc::Start, say: &Reporter) -> Result<(), String> {
    use std::time::Duration;

    use prism_core::control::client::{ClientConfig, decodable};
    use prism_core::identity;
    use prism_core::net::negotiate::Offer;
    use prism_stream::display;

    /// The ceiling on how long a picture may be held back to even out arrival jitter.
    ///
    /// Zero is the other mode: show every picture the moment it decodes.
    const SMOOTH_PACING_US: u32 = 8_000;

    let address = match start.address.as_deref() {
        Some(address) => Some(
            address
                .parse()
                .map_err(|_| format!("{address} is not an address"))?,
        ),
        None => None,
    };

    if address.is_none() && start.rendezvous.is_none() {
        return Err("give either an address or a rendezvous server".to_owned());
    }

    let peers = identity::default_peers_path().map_err(|err| err.to_string())?;
    let config = ClientConfig {
        host: address,
        rendezvous: start.rendezvous.clone(),
        force_relay: false,
        frames: None,
        idle_timeout: Duration::from_millis(start.idle_timeout_ms),
        // Nothing periodic: the summaries are for a measurement run being read in a terminal,
        // and what a window shows is the counters, which arrive on their own timer.
        report_every: 0,
        in_flight: 4,
        decode: true,
        offer: Offer {
            codecs: decodable(),
            // The window the stream is shown in. A host sending more pixels than that is
            // spending bitrate on pixels thrown away before anybody sees them.
            max_width: u16::try_from(start.width).unwrap_or(u16::MAX),
            max_height: u16::try_from(start.height).unwrap_or(u16::MAX),
            max_fps: u16::MAX,
            audio: true,
        },
        identity: identity::load_or_create(
            &identity::default_path().map_err(|err| err.to_string())?,
        )
        .map_err(|err| err.to_string())?,
        peer_key: identity::resolve_peer(Some(&start.host), &peers)
            .map_err(|err| err.to_string())?,
    };

    display::run(
        config,
        start.width,
        start.height,
        if start.smooth { SMOOTH_PACING_US } else { 0 },
        start.control,
        false,
        say,
    )
    .map_err(|err| err.to_string())
}

/// Refuses, on a build with no window to show a stream in.
///
/// Said plainly rather than crashing: this binary exists on every platform the workspace builds,
/// and a platform whose client is not written yet should say so.
#[cfg(not(all(feature = "window", any(target_os = "macos", target_os = "windows"))))]
fn watch(start: &ipc::Start, say: &Reporter) -> Result<(), String> {
    let _ = (start, say);

    Err("this build has no window to show a stream in".to_owned())
}

/// Turns what the session says into what the shell reads.
fn translate(report: Report) -> ipc::Event {
    match report {
        Report::Note(line) => ipc::Event::Note { line },
        Report::Established(address) => ipc::Event::Established {
            address: address.to_string(),
        },
        Report::Terms(agreed) => ipc::Event::Terms {
            codec: format!("{:?}", agreed.codec),
            // A client that will take whatever the host's screen is says so with the largest
            // number the field holds. That is not a size anybody wants shown to them.
            width: capped(agreed.width),
            height: capped(agreed.height),
            fps: u32::from(agreed.fps),
            audio: agreed.audio,
        },
        Report::Counters(counters) => ipc::Event::Counters {
            round_trip_us: counters.round_trip_us,
            fps: counters.fps,
            kbps: counters.kbps,
            frames: counters.frames,
        },
    }
}

/// Returns a dimension, or zero when it means "whatever the host has".
fn capped(size: u16) -> u32 {
    if size < 65_534 { u32::from(size) } else { 0 }
}
