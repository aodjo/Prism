//! Headless Prism host and client.
//!
//! This binary is how M1 through M4 are developed and measured: it runs the data plane
//! with no Electron and no window, so the latency numbers describe the pipeline rather
//! than a compositor. CI drives it for protocol regression runs.

#[cfg(target_os = "macos")]
mod audio;
mod client;
#[cfg(target_os = "macos")]
mod display;
#[cfg(target_os = "macos")]
mod encode;
mod host;
mod pattern;
/// Replaying a dump is only useful where there is a decoder to replay it through.
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod replay;

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use prism_core::identity;
use prism_core::net::handshake::Identity;
use prism_core::net::negotiate::{Codecs, H264, Offer};

/// Which codec a recording holds.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DecodeCodec {
    /// Read it out of the recording, which is what it is written in.
    Auto,
    /// H.264.
    H264,
    /// HEVC.
    Hevc,
}

/// How the client trades latency against even presentation.
///
/// Pictures do not arrive at a steady rate — encoding, the network and decoding each vary
/// a little — so showing each one the moment it is ready reproduces that unevenness on
/// screen. Holding them to a common age removes it, at the cost of the delay needed to
/// cover the spread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PacingMode {
    /// Show every picture as soon as it decodes: the lowest latency the pipeline can give.
    Performance,
    /// Even out arrival jitter, which on a loopback path costs about six milliseconds.
    Smooth,
}

impl PacingMode {
    /// Returns the longest a picture may be held under this mode, in microseconds.
    ///
    /// A ceiling rather than a target: the pacer measures what the path actually needs and
    /// stays under this, so a steady path is barely delayed even in smooth mode.
    fn ceiling_us(self) -> u32 {
        match self {
            PacingMode::Performance => 0,
            PacingMode::Smooth => 16_000,
        }
    }
}

/// Command line for the headless host and client.
#[derive(Debug, Parser)]
#[command(name = "prism-cli", version, about = "Headless Prism host and client")]
struct Cli {
    /// Which side of the session to run.
    #[command(subcommand)]
    command: Command,
}

/// The sides of a session, plus the encoder probe.
#[derive(Debug, Subcommand)]
enum Command {
    /// Wait for a paired client to connect, then produce frames and send them.
    Host {
        /// Address to listen on.
        ///
        /// The host cannot dial: over the internet it has no way to learn a client's address
        /// until that client speaks. Once one does, the socket is connected to it and the
        /// kernel discards datagrams from anywhere else.
        #[arg(long, default_value = "0.0.0.0:47200")]
        bind: SocketAddr,

        /// Give up after this long with no client, in seconds.
        #[arg(long, default_value_t = 300)]
        wait_secs: u64,

        /// Rendezvous server to register with, so clients can find this machine behind NAT.
        ///
        /// Without one the host is reachable only from a network the client can already
        /// address: the same LAN, a VPN, or a forwarded port.
        #[arg(long)]
        rendezvous: Option<SocketAddr>,

        /// Frames per second.
        #[arg(long, default_value_t = 60)]
        fps: u32,

        /// Encoded bytes per frame, for the synthetic source.
        #[arg(long, default_value_t = 40_000)]
        frame_bytes: usize,

        /// Slices per frame, for the synthetic source.
        #[arg(long, default_value_t = 4)]
        slices: usize,

        /// Frames to send before stopping.
        #[arg(long, default_value_t = 300)]
        frames: u32,

        /// Encode real H.264 instead of sending synthetic bytes.
        #[arg(long)]
        encode: bool,

        /// Capture the screen instead of painting a test pattern. Implies --encode.
        #[arg(long)]
        capture: bool,

        /// Frame width when encoding.
        #[arg(long, default_value_t = 1920)]
        width: u32,

        /// Frame height when encoding.
        #[arg(long, default_value_t = 1080)]
        height: u32,

        /// Target bitrate in bits per second when encoding.
        #[arg(long, default_value_t = 24_000_000)]
        bitrate: u32,

        /// Drop this fraction of outgoing video packets, as a percentage.
        ///
        /// The M4 gate is judged at 5. Deterministic and seeded rather than an operating
        /// system traffic shaper, so a failure reproduces and CI can run it.
        #[arg(long, default_value_t = 0.0)]
        loss: f64,

        /// Seed for the loss injector, so a failing run repeats exactly.
        #[arg(long, default_value_t = 1)]
        loss_seed: u64,

        /// Send Reed-Solomon parity sized for this much loss, as a percentage.
        ///
        /// Omitted means no parity at all. The codec clamps the ratio to ten to twenty
        /// percent of the block, so a wild figure cannot spend the whole bitrate on repair.
        #[arg(long)]
        parity: Option<f64>,

        /// Spread packets over the frame interval at this rate, in megabits per second.
        ///
        /// Omitted sends every packet as fast as the socket accepts it, which is what
        /// causes the queueing a congestion controller then measures and reacts to.
        #[arg(long)]
        pace: Option<f64>,

        /// Let the congestion controller drive the pacing rate from client feedback.
        ///
        /// Requires --pace, which supplies the rate it starts from.
        #[arg(long)]
        adaptive: bool,

        /// Also capture and send this machine's audio, at this many bits per second.
        ///
        /// Off by default: the command line exists to measure the video path, and a second
        /// stream would be in every number without being what any of them are about. Naming a
        /// rate opts in, which is what makes the audio path testable at all — the client
        /// offers audio only when it has a window to play it through.
        #[arg(long)]
        audio: Option<u32>,

        /// Where this machine's long-term key is kept, generated on first use.
        #[arg(long)]
        identity: Option<PathBuf>,

        /// Restrict the session to this one client key in hex.
        ///
        /// Every paired client is admitted by default, because a host serves whichever of its
        /// machines connects. A host that had paired with nothing refuses everyone: there is
        /// no way to run open, and a session that skipped this would be one where anyone who
        /// can reach the port can watch the screen and type on it.
        #[arg(long)]
        peer_key: Option<String>,
    },

    /// Connect to a paired host, receive frames, and report latency.
    Client {
        /// Address the host is listening on, when it is directly reachable.
        #[arg(long)]
        host: Option<SocketAddr>,

        /// Rendezvous server to find the host through, when it is not.
        #[arg(long)]
        rendezvous: Option<SocketAddr>,

        /// Go through the relay without trying a direct path first.
        ///
        /// For measuring what relaying costs against the same session run directly.
        #[arg(long)]
        force_relay: bool,

        /// Stop after this many frames; runs until idle when omitted.
        #[arg(long)]
        frames: Option<u32>,

        /// Give up after this long with no packets.
        #[arg(long, default_value_t = 2000)]
        idle_timeout_ms: u64,

        /// Print a running summary every this many frames; zero disables it.
        #[arg(long, default_value_t = 60)]
        report_every: u32,

        /// Frames allowed in flight before the oldest is abandoned.
        #[arg(long, default_value_t = 4)]
        in_flight: usize,

        /// Decode the reassembled frames and report end-to-end latency.
        #[arg(long)]
        decode: bool,

        /// Show the decoded stream in a window. Implies --decode.
        #[arg(long)]
        display: bool,

        /// Window width when showing the stream.
        #[arg(long, default_value_t = 1280)]
        window_width: u32,

        /// Window height when showing the stream.
        #[arg(long, default_value_t = 720)]
        window_height: u32,

        /// Whether to favour latency or even presentation.
        #[arg(long, value_enum, default_value_t = PacingMode::Performance)]
        mode: PacingMode,

        /// Override the mode's hold ceiling, in milliseconds. For measurement.
        #[arg(long)]
        pacing_ms: Option<u32>,

        /// Watch the stream without controlling the host.
        #[arg(long)]
        no_input: bool,

        /// Ask for the host's audio even without a window to play it through.
        ///
        /// A session with a window asks for audio anyway. This is for measuring the audio
        /// path: the frames arrive, are counted and reported, and are not played, because a
        /// measurement run that suddenly made noise would be a surprise.
        #[arg(long)]
        audio: bool,

        /// Send a steady stream of fabricated pointer motion, so the input path can be
        /// measured without a hand on the mouse.
        #[arg(long)]
        synthetic_input: bool,

        /// Where this machine's long-term key is kept, generated on first use.
        #[arg(long)]
        identity: Option<PathBuf>,

        /// The host's public key in hex.
        ///
        /// Defaults to the one `prism-cli pair` recorded.
        #[arg(long)]
        peer_key: Option<String>,
    },

    /// Decode a recorded bitstream, with no socket and no window.
    ///
    /// The client writes one with `PRISM_DUMP_BITSTREAM`. Replaying it is how a decoder is
    /// judged on its own, before there is a renderer to show whether it worked.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Decode {
        /// The recorded bitstream to read.
        #[arg(long)]
        file: PathBuf,

        /// Which codec it was encoded with, or `auto` to read that out of the recording.
        #[arg(long, default_value = "auto")]
        codec: DecodeCodec,

        /// Read each picture back and say whether it is a flat colour.
        ///
        /// Costs a copy out of GPU memory per frame, which is exactly what the live path
        /// exists to avoid — so it is off unless asked for.
        #[arg(long)]
        verify: bool,
    },

    /// Print this machine's public key, creating its long-term key if there is none.
    ///
    /// The two sides exchange these once. Each pins the other's, and from then on a peer
    /// that cannot prove it holds the matching private key is refused before it can send a
    /// single byte the session acts on.
    Keygen {
        /// Where to keep the key. Defaults to `~/.prism/identity.key`.
        #[arg(long)]
        identity: Option<PathBuf>,
    },

    /// Report what the system still has to allow before this machine can host.
    ///
    /// Both grants fail quietly when they are missing: capture delivers black frames and
    /// silence, injection posts events that go nowhere, and nothing raises an error. So the
    /// answer is worth having before a session rather than after one that looked fine.
    Permissions {
        /// Ask the system for anything missing, rather than only reporting it.
        ///
        /// The prompt appears once in the life of an application. After that only a person
        /// can change the answer, in the settings pane this prints.
        #[arg(long)]
        request: bool,
    },

    /// Listen to what this machine is playing and report what was heard.
    ///
    /// Everything that stops a Mac's sound reaching the wire produces one symptom:
    /// ScreenCaptureKit starts, delivers buffers on schedule, and fills every one with
    /// zeroes. A denied Screen Recording grant does that, a misconfigured capture does that,
    /// and so does a quiet room. Nothing downstream can tell them apart, because a five byte
    /// Opus frame is the correct encoding of silence — no error is raised anywhere along the
    /// way. This reads the samples, which is the only place they differ.
    Audio {
        /// How long to listen, in seconds.
        #[arg(long, default_value_t = 3)]
        secs: u64,

        /// Bitrate to encode at while listening, in bits per second.
        #[arg(long, default_value_t = 128_000)]
        bitrate: u32,
    },

    /// Encode synthetic frames to an Annex B file to verify the encoder.
    Encode {
        /// Where to write the elementary stream.
        #[arg(long, default_value = "prism-probe.h264")]
        out: PathBuf,

        /// Frames to encode.
        #[arg(long, default_value_t = 120)]
        frames: u32,

        /// Frame width in pixels.
        #[arg(long, default_value_t = 1920)]
        width: u32,

        /// Frame height in pixels.
        #[arg(long, default_value_t = 1080)]
        height: u32,

        /// Frames per second.
        #[arg(long, default_value_t = 60)]
        fps: u32,

        /// Target bitrate in bits per second.
        #[arg(long, default_value_t = 24_000_000)]
        bitrate: u32,

        /// Soft ceiling on bytes per slice; zero leaves slicing to the encoder.
        #[arg(long, default_value_t = 12_000)]
        slice_bytes: u32,

        /// Also write the frames that went in, as raw NV12.
        ///
        /// What makes a quality comparison possible: two codecs at one bitrate can only be
        /// told apart against the thing they were both trying to reproduce.
        #[arg(long)]
        source_out: Option<PathBuf>,

        /// Encode HEVC rather than H.264.
        ///
        /// Worth about half the bitrate for the same picture, which is what makes it the
        /// codec to want over the internet. This is how the claim gets checked against a
        /// file rather than asserted.
        #[arg(long)]
        hevc: bool,

        /// How many frames may be inside the encoder at once.
        ///
        /// One waits for each frame before painting the next, so the rate it measures is
        /// paint and encode added together rather than overlapped. Raising it is what the
        /// plan means by encoding asynchronously, and is how to tell a frame rate bounded by
        /// the encoder's latency from one bounded by its throughput.
        #[arg(long, default_value_t = 1)]
        in_flight: usize,
    },
}

/// Parses the command line and runs the requested side.
fn main() -> ExitCode {
    match dispatch(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("prism-cli: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Returns what this machine can decode.
///
/// A statement about the hardware, not a wish: naming a codec that is not implemented would
/// agree a session that never shows a frame, and the symptom is a black window with nothing
/// reporting an error.
fn client_codecs() -> Codecs {
    #[cfg(target_os = "macos")]
    {
        Codecs::none()
            .with(H264)
            .with(prism_core::net::negotiate::HEVC)
    }

    // Every other platform decodes nothing yet, so it offers the floor and gets a session it
    // can at least reassemble and measure.
    #[cfg(not(target_os = "macos"))]
    {
        Codecs::none().with(H264)
    }
}

/// Prints what the system allows, and what to do about anything it does not.
///
/// # Errors
///
/// Returns an error when something is still missing, so that a script can tell a machine
/// that is ready to host from one that is not.
fn report_permissions(request: bool) -> Result<(), Box<dyn Error>> {
    use prism_core::control::permissions::{check, request as ask};

    let mut held = check();

    if request {
        for grant in held.missing(true) {
            let granted = ask(grant);
            println!(
                "{}: {}",
                grant.name(),
                if granted {
                    "granted".to_string()
                } else {
                    format!("still refused — open {}", grant.settings_url())
                }
            );
        }

        held = check();
    }

    println!(
        "screen recording: {}\naccessibility   : {}",
        if held.screen { "granted" } else { "missing" },
        if held.input { "granted" } else { "missing" },
    );

    // Judged as a host that controls the machine, because that is what the command line runs
    // by default and the stricter answer is the useful one to fail on.
    let missing = held.missing(true);
    if missing.is_empty() {
        println!("this machine can host");
        return Ok(());
    }

    for grant in &missing {
        eprintln!(
            "missing {} — needed {}. Grant it at {}",
            grant.name(),
            grant.purpose(),
            grant.settings_url()
        );
    }

    Err(format!(
        "{} of 2 grants missing; this machine would host with a black screen or no control",
        missing.len()
    )
    .into())
}

/// Loudest sample still counted as nothing at all.
///
/// Far below anything anyone would call quiet, because it is not separating quiet from loud:
/// a capture that has come adrift from the mix returns samples of exactly zero, and a machine
/// playing at the lowest volume it offers still returns thousands of times this.
#[cfg(target_os = "macos")]
const SILENCE_FLOOR: f32 = 1e-6;

/// Listens to the machine's own output and reports what arrived.
///
/// Prints the peak sample and the size of the Opus packets it produced, which is what
/// separates the two failures that look identical from a distance: a capture that is not
/// attached to the mix delivers frames of zeroes at exactly the right rate, and those encode
/// to the same five-byte packets a genuinely silent machine produces.
///
/// # Errors
///
/// Returns an error if the system will not open its audio at all, or if the encoder will not
/// start.
#[cfg(target_os = "macos")]
fn listen(secs: u64, bitrate_bps: u32) -> Result<(), Box<dyn Error>> {
    use prism_core::audio::codec::AudioEncoder;
    use prism_core::audio::screencapturekit::SystemAudioCapture;
    use prism_core::audio::{Pulled, SystemAudio};

    let mut capture = SystemAudioCapture::start()?;
    let mut encoder = AudioEncoder::new(bitrate_bps)?;

    println!(
        "audio: listening for {secs}s at {} kbps",
        bitrate_bps / 1000
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut frames = 0u64;
    let mut quiet = 0u64;
    let mut peak = 0.0f32;
    let mut bytes = 0usize;
    let mut largest = 0usize;

    while std::time::Instant::now() < deadline {
        let Pulled::Frame(frame) = capture.poll(Duration::from_millis(50)) else {
            quiet += 1;
            continue;
        };

        for &sample in frame {
            peak = peak.max(sample.abs());
        }

        let packet = encoder.encode(frame)?;
        bytes += packet.len();
        largest = largest.max(packet.len());
        frames += 1;
    }

    // Printed with room below the audible, because the thing this has to tell apart is a
    // quiet room from a dead tap and those differ by orders of magnitude rather than by a
    // decimal place. A wedged capture returns samples of exactly zero; anything a machine is
    // really playing, at any volume somebody would choose, is far above that.
    println!(
        "audio: {frames} frames, {quiet} timed out, peak {peak:.3e} ({}), mean {} bytes, largest {largest}",
        if peak > 0.0 {
            format!("{:.1} dBFS", 20.0 * peak.log10())
        } else {
            "digital silence".to_owned()
        },
        bytes / usize::try_from(frames).unwrap_or(1).max(1)
    );

    if frames == 0 {
        return Err("no audio arrived at all; the stream opened but delivered nothing".into());
    }

    if peak <= SILENCE_FLOOR {
        return Err(
            "audio arrived on schedule but every sample was zero, which is either a quiet \
             machine or a capture that has come adrift from the mix. Play something and try \
             again first. If it stays silent, check that the application which launched this \
             holds the Screen Recording grant that system audio is behind — a refused grant \
             delivers empty buffers rather than failing, and the grant belongs to the \
             launching application rather than to this binary, so a command line tool \
             inherits the terminal's."
                .into(),
        );
    }

    println!("audio: this machine's output is being captured");

    Ok(())
}

/// Reports that this platform has no system audio capture.
///
/// # Errors
///
/// Always. There is nothing to probe.
#[cfg(not(target_os = "macos"))]
fn listen(secs: u64, bitrate_bps: u32) -> Result<(), Box<dyn Error>> {
    let _ = (secs, bitrate_bps);
    Err("system audio can only be probed on macOS so far".into())
}

/// Turns the encoder probe's flag into a codec.
///
/// Only the platform with an encoder to probe has a caller for it.
#[cfg(target_os = "macos")]
fn codec_of(hevc: bool) -> prism_core::net::negotiate::Codec {
    if hevc {
        prism_core::net::negotiate::Codec::Hevc
    } else {
        prism_core::net::negotiate::Codec::H264
    }
}

/// Works out which client keys a host session will admit.
///
/// Named on the command line, or every machine the account has said is its own. Never
/// everyone: a host that trusts nothing admits nobody, which is the right answer rather than
/// an inconvenience.
///
/// # Errors
///
/// Returns [`std::io::ErrorKind::NotFound`] when nothing is trusted, and
/// [`std::io::ErrorKind::InvalidInput`] for a key that is not one.
fn admitted_clients(named: Option<&str>) -> Result<Vec<[u8; 32]>, Box<dyn Error>> {
    let peers = identity::default_peers_path()?;

    if let Some(text) = named {
        return Ok(vec![identity::parse_peer_key(text)?]);
    }

    let known = identity::known_peers(&peers)?;
    if known.is_empty() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no machine may watch this one yet. Sign in to an account from the application \
             on both machines, or name a key with --peer-key",
        )));
    }

    Ok(known)
}

/// Loads the identity at `path`, or at the default location when none was given.
///
/// Generated on first use rather than demanded up front: a machine that has never run has
/// nothing to lose by making a key, and demanding one before the first run would put a setup
/// step in front of every install.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the key cannot be read or written.
fn open_identity(path: Option<&std::path::Path>) -> Result<Identity, Box<dyn Error>> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => identity::default_path()?,
    };

    Ok(identity::load_or_create(&path)?)
}

/// Runs the selected subcommand.
///
/// # Errors
///
/// Returns whatever the selected side failed with, including the platform errors raised
/// when a codec-backed mode is requested where no codec backend exists yet.
fn dispatch(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Host {
            bind,
            wait_secs,
            rendezvous,
            fps,
            frame_bytes,
            slices,
            frames,
            encode,
            capture,
            width,
            height,
            bitrate,
            loss,
            loss_seed,
            parity,
            pace,
            adaptive,
            audio,
            identity,
            peer_key,
        } => {
            let keys = host::HostKeys {
                identity: open_identity(identity.as_deref())?,
                allowed: admitted_clients(peer_key.as_deref())?,
            };
            let run = host::HostRun {
                session: host::HostConfig {
                    bind,
                    rendezvous,
                    patience: Duration::from_secs(wait_secs),
                    fps,
                    bitrate_bps: bitrate,
                    frames: Some(frames),
                    parity_loss: parity.map(|percent| (percent.clamp(0.0, 100.0) / 100.0) as f32),
                    pace_bps: pace.map(|mbps| (mbps.clamp(0.0, 10_000.0) * 1e6) as u32),
                    adaptive,
                    inject_input: true,
                    // Off unless --audio names a rate. The command line measures the video
                    // path, and a second stream would be in every number without being what
                    // any of them are about.
                    audio_bitrate_bps: audio,
                    // What this machine's encoder can actually produce, from the core rather
                    // than restated here — a second answer to that question is a second answer
                    // that drifts.
                    codecs: prism_core::control::host::host_codecs(),
                },
                frame_bytes,
                slices,
                loss_ppm: percent_to_ppm(loss),
                loss_seed,
            };

            if !encode && !capture {
                return Ok(host::run(run, &keys)?);
            }

            #[cfg(target_os = "macos")]
            if capture {
                return host::run_captured(run, &keys, bitrate, width, height);
            }

            #[cfg(target_os = "macos")]
            {
                host::run_encoded(
                    run,
                    &keys,
                    prism_core::encode::EncoderConfig {
                        // The measurement paths configure the encoder before a session
                        // exists, so there is nothing agreed yet. H.264 is what both sides
                        // advertise anyway.
                        codec: prism_core::net::negotiate::Codec::H264,
                        width,
                        height,
                        fps,
                        bitrate_bps: bitrate,
                        max_slice_bytes: bitrate / 8 / fps.max(1) / 4,
                    },
                )
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = (width, height, bitrate);
                #[cfg(target_os = "windows")]
                {
                    let encoder_config = prism_core::encode::EncoderConfig {
                        // Replaced with whatever the session agreed, once it has opened.
                        codec: prism_core::net::negotiate::Codec::H264,
                        width,
                        height,
                        fps,
                        bitrate_bps: bitrate,
                        // NVENC counts slices rather than bytes, so the host's slice count
                        // is what it is told. Apple's encoder takes a byte ceiling and
                        // refuses it anyway, which is why the two paths read this field
                        // differently.
                        max_slice_bytes: slices as u32,
                    };
                    host::run_windows(run, &keys, encoder_config, capture)
                }

                #[cfg(not(target_os = "windows"))]
                Err("encoding is not implemented on this platform yet".into())
            }
        }

        Command::Client {
            host,
            rendezvous,
            force_relay,
            frames,
            idle_timeout_ms,
            report_every,
            in_flight,
            decode,
            display,
            window_width,
            window_height,
            mode,
            pacing_ms,
            no_input,
            audio,
            synthetic_input,
            identity,
            peer_key,
        } => {
            let pacing_us = pacing_ms.map_or_else(|| mode.ceiling_us(), |ms| ms * 1_000);
            let config = client::ClientConfig {
                host,
                rendezvous,
                force_relay,
                frames,
                idle_timeout: Duration::from_millis(idle_timeout_ms),
                report_every,
                in_flight,
                decode: decode || display,
                // What this machine can decode. H.264 alone until the VideoToolbox path is
                // taught the others; naming a codec that is not implemented would agree a
                // session that never shows a frame.
                offer: Offer {
                    // Both, on macOS: VideoToolbox decodes each in hardware, and the codec
                    // that ends up being used is whichever the host can also produce.
                    codecs: client_codecs(),
                    // The window the stream will be shown in, when there is one. A host
                    // sending more pixels than that is spending bitrate on pixels thrown
                    // away before anybody sees them. A run with no window is measuring the
                    // pipeline rather than watching it, and constrains nothing.
                    max_width: if display {
                        u16::try_from(window_width).unwrap_or(u16::MAX)
                    } else {
                        u16::MAX
                    },
                    max_height: if display {
                        u16::try_from(window_height).unwrap_or(u16::MAX)
                    } else {
                        u16::MAX
                    },
                    max_fps: u16::MAX,
                    // Asked for whenever there is a window, and on demand without one so the
                    // path can be measured. Offering it is not the same as playing it.
                    audio: display || audio,
                },
                identity: open_identity(identity.as_deref())?,
                peer_key: identity::resolve_peer(
                    peer_key.as_deref(),
                    &identity::default_peers_path()?,
                )?,
            };

            let offset =
                std::sync::Arc::new(std::sync::atomic::AtomicI64::new(client::OFFSET_UNKNOWN));

            #[cfg(not(target_os = "macos"))]
            {
                let _ = (window_width, window_height, pacing_us, no_input);
                if config.decode {
                    return Err("decoding is not implemented on this platform yet".into());
                }
                Ok(client::run(
                    config,
                    client::ClientHooks {
                        offset: Some(offset),
                        input: synthetic_input.then(windowless_input),
                        ..client::ClientHooks::default()
                    },
                )?)
            }

            #[cfg(target_os = "macos")]
            {
                if display {
                    display::run(
                        config,
                        window_width,
                        window_height,
                        pacing_us,
                        !no_input,
                        synthetic_input,
                    )
                } else {
                    Ok(client::run(
                        config,
                        client::ClientHooks {
                            offset: Some(offset),
                            input: synthetic_input.then(windowless_input),
                            ..client::ClientHooks::default()
                        },
                    )?)
                }
            }
        }

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        Command::Decode {
            file,
            codec,
            verify,
        } => replay::run(
            &file,
            match codec {
                DecodeCodec::Auto => None,
                DecodeCodec::H264 => Some(prism_core::net::negotiate::Codec::H264),
                DecodeCodec::Hevc => Some(prism_core::net::negotiate::Codec::Hevc),
            },
            verify,
        ),

        Command::Keygen { identity: path } => {
            let path = match path {
                Some(path) => path,
                None => identity::default_path()?,
            };
            let identity = identity::load_or_create(&path)?;

            println!("{}", identity::to_hex(identity.public()));
            eprintln!("prism-cli: key kept at {}", path.display());

            Ok(())
        }

        Command::Permissions { request } => report_permissions(request),

        Command::Audio { secs, bitrate } => listen(secs, bitrate),

        Command::Encode {
            out,
            frames,
            width,
            height,
            fps,
            bitrate,
            slice_bytes,
            source_out,
            hevc,
            in_flight,
        } => {
            #[cfg(target_os = "macos")]
            {
                encode::run(encode::EncodeConfig {
                    out,
                    source_out,
                    frames,
                    in_flight,
                    encoder: prism_core::encode::EncoderConfig {
                        codec: codec_of(hevc),
                        width,
                        height,
                        fps,
                        bitrate_bps: bitrate,
                        max_slice_bytes: slice_bytes,
                    },
                })
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = (
                    out,
                    frames,
                    width,
                    height,
                    fps,
                    bitrate,
                    slice_bytes,
                    source_out,
                    hevc,
                    in_flight,
                );
                Err("encoding is not implemented on this platform yet".into())
            }
        }
    }
}

/// Builds the input slot for a client with no window, and starts fabricating motion into it.
///
/// A client that shows the stream captures real input from its window. One that does not
/// has nothing to capture, so the only way it can exercise the return path is to make the
/// events up. That is worth having because it is the only way to measure input against a
/// host whose video this client cannot decode.
fn windowless_input() -> std::sync::Arc<std::sync::OnceLock<client::InputSender>> {
    let slot = std::sync::Arc::new(std::sync::OnceLock::new());
    client::spawn_synthetic_input(std::sync::Arc::clone(&slot));
    slot
}

/// Converts a loss percentage from the command line into parts per million.
///
/// Parts per million on the inside because the decision is an integer comparison on the
/// send path; a percentage on the outside because that is how the requirement is written
/// and how anyone reading the output will think about it.
///
/// Values outside zero to a hundred are clamped rather than refused: a run that quietly did
/// something other than what was asked is worse than one that says what it did, and the
/// injector reports the rate it actually achieved.
fn percent_to_ppm(percent: f64) -> u32 {
    (percent.clamp(0.0, 100.0) * 10_000.0).round() as u32
}
