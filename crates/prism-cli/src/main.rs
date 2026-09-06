//! Headless Prism host and client.
//!
//! This binary is how M1 through M4 are developed and measured: it runs the data plane
//! with no Electron and no window, so the latency numbers describe the pipeline rather
//! than a compositor. CI drives it for protocol regression runs.

mod client;
#[cfg(target_os = "macos")]
mod display;
#[cfg(target_os = "macos")]
mod encode;
mod host;
mod identity;
mod pair;
mod pattern;
mod rendezvous;
mod session;
mod wire;

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use prism_core::net::handshake::Identity;

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

    /// Exchange long-term keys with another machine using a six digit code.
    ///
    /// Run once per pair of machines. After it, neither side ever needs a code again, and a
    /// peer that cannot prove it holds the matching private key is refused before it can send
    /// a single byte the session acts on.
    Pair {
        /// Which side of the exchange to run.
        #[command(subcommand)]
        side: PairSide,
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
    },
}

/// The two halves of a pairing exchange.
///
/// The client dials, because that is where the person who typed the code is waiting. This is
/// the opposite of a running session, where the host dials — pairing and streaming are
/// separate exchanges and neither constrains the other.
#[derive(Debug, Subcommand)]
enum PairSide {
    /// Show a code and wait for one client to use it.
    Host {
        /// Address to listen on.
        #[arg(long, default_value = "0.0.0.0:47100")]
        bind: SocketAddr,

        /// Where this machine's long-term key is kept, generated on first use.
        #[arg(long)]
        identity: Option<PathBuf>,
    },

    /// Type a code the host is showing and pair with it.
    Client {
        /// Address the host is waiting on.
        #[arg(long)]
        host: SocketAddr,

        /// The six digits the host printed.
        #[arg(long)]
        pin: String,

        /// Where this machine's long-term key is kept, generated on first use.
        #[arg(long)]
        identity: Option<PathBuf>,
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

/// Works out which client keys a host session will admit.
///
/// Named on the command line, or every machine pairing has recorded. Never everyone: a host
/// that has paired with nothing admits nobody, which is the right answer rather than an
/// inconvenience.
///
/// # Errors
///
/// Returns [`std::io::ErrorKind::NotFound`] with an instruction to pair when nothing has
/// been, and [`std::io::ErrorKind::InvalidInput`] for a key that is not one.
fn admitted_clients(named: Option<&str>) -> Result<Vec<[u8; 32]>, Box<dyn Error>> {
    let peers = identity::default_peers_path()?;

    if let Some(text) = named {
        return Ok(vec![identity::parse_peer_key(text)?]);
    }

    let known = identity::known_peers(&peers)?;
    if known.is_empty() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no client has been paired; run `prism-cli pair host` and pair one first",
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
            identity,
            peer_key,
        } => {
            let keys = host::HostKeys {
                identity: open_identity(identity.as_deref())?,
                allowed: admitted_clients(peer_key.as_deref())?,
            };
            let config = host::HostConfig {
                bind,
                rendezvous,
                patience: Duration::from_secs(wait_secs),
                fps,
                frame_bytes,
                slices,
                frames,
                loss_ppm: percent_to_ppm(loss),
                loss_seed,
                parity_loss: parity.map(|percent| (percent.clamp(0.0, 100.0) / 100.0) as f32),
                pace_bps: pace.map(|mbps| (mbps.clamp(0.0, 10_000.0) * 1e6) as u32),
                adaptive,
            };

            if !encode && !capture {
                return Ok(host::run(config, &keys)?);
            }

            #[cfg(target_os = "macos")]
            if capture {
                return host::run_captured(config, &keys, bitrate, width, height);
            }

            #[cfg(target_os = "macos")]
            {
                host::run_encoded(
                    config,
                    &keys,
                    prism_core::encode::EncoderConfig {
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
                    host::run_windows(config, &keys, encoder_config, capture)
                }

                #[cfg(not(target_os = "windows"))]
                Err("encoding is not implemented on this platform yet".into())
            }
        }

        Command::Client {
            host,
            rendezvous,
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
            synthetic_input,
            identity,
            peer_key,
        } => {
            let pacing_us = pacing_ms.map_or_else(|| mode.ceiling_us(), |ms| ms * 1_000);
            let config = client::ClientConfig {
                host,
                rendezvous,
                frames,
                idle_timeout: Duration::from_millis(idle_timeout_ms),
                report_every,
                in_flight,
                decode: decode || display,
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

        Command::Pair { side } => {
            let peers = identity::default_peers_path()?;

            match side {
                PairSide::Host {
                    bind,
                    identity: path,
                } => {
                    let identity = open_identity(path.as_deref())?;
                    pair::host(bind, &identity, &peers)?;
                }
                PairSide::Client {
                    host,
                    pin,
                    identity: path,
                } => {
                    let identity = open_identity(path.as_deref())?;
                    pair::client(host, &pin, &identity, &peers)?;
                }
            }

            Ok(())
        }

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

        Command::Encode {
            out,
            frames,
            width,
            height,
            fps,
            bitrate,
            slice_bytes,
        } => {
            #[cfg(target_os = "macos")]
            {
                encode::run(encode::EncodeConfig {
                    out,
                    frames,
                    encoder: prism_core::encode::EncoderConfig {
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
                let _ = (out, frames, width, height, fps, bitrate, slice_bytes);
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
