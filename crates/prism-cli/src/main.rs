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
mod pattern;
mod wire;

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

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
    /// Produce frames and send them to a client.
    Host {
        /// Address of the receiving client.
        #[arg(long)]
        peer: SocketAddr,

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
    },

    /// Receive frames and report latency.
    Client {
        /// Address to listen on.
        #[arg(long)]
        bind: SocketAddr,

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

/// Runs the selected subcommand.
///
/// # Errors
///
/// Returns whatever the selected side failed with, including the platform errors raised
/// when a codec-backed mode is requested where no codec backend exists yet.
fn dispatch(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::Host {
            peer,
            fps,
            frame_bytes,
            slices,
            frames,
            encode,
            capture,
            width,
            height,
            bitrate,
        } => {
            let config = host::HostConfig {
                peer,
                fps,
                frame_bytes,
                slices,
                frames,
            };

            if !encode && !capture {
                return Ok(host::run(config)?);
            }

            #[cfg(target_os = "macos")]
            if capture {
                return host::run_captured(config, bitrate, width, height);
            }

            #[cfg(target_os = "macos")]
            {
                host::run_encoded(
                    config,
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
                Err("encoding is not implemented on this platform yet".into())
            }
        }

        Command::Client {
            bind,
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
        } => {
            let pacing_us = pacing_ms.map_or_else(|| mode.ceiling_us(), |ms| ms * 1_000);
            let config = client::ClientConfig {
                bind,
                frames,
                idle_timeout: Duration::from_millis(idle_timeout_ms),
                report_every,
                in_flight,
                decode: decode || display,
            };

            let offset =
                std::sync::Arc::new(std::sync::atomic::AtomicI64::new(client::OFFSET_UNKNOWN));

            #[cfg(not(target_os = "macos"))]
            {
                let _ = (window_width, window_height, pacing_us);
                if config.decode {
                    return Err("decoding is not implemented on this platform yet".into());
                }
                Ok(client::run(config, None, offset)?)
            }

            #[cfg(target_os = "macos")]
            {
                if display {
                    display::run(config, window_width, window_height, pacing_us)
                } else {
                    Ok(client::run(config, None, offset)?)
                }
            }
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
