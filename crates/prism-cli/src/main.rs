//! Headless Prism host and client.
//!
//! This binary is how M1 through M4 are developed and measured: it runs the data plane
//! with no Electron and no window, so the latency numbers describe the pipeline rather
//! than a compositor. CI drives it for protocol regression runs.

mod client;
#[cfg(target_os = "macos")]
mod encode;
mod host;

use std::net::SocketAddr;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// Command line for the headless host and client.
#[derive(Debug, Parser)]
#[command(name = "prism-cli", version, about = "Headless Prism host and client")]
struct Cli {
    /// Which side of the session to run.
    #[command(subcommand)]
    command: Command,
}

/// The two sides of a session.
#[derive(Debug, Subcommand)]
enum Command {
    /// Generate synthetic frames and send them to a client.
    Host {
        /// Address of the receiving client.
        #[arg(long)]
        peer: SocketAddr,

        /// Frames per second.
        #[arg(long, default_value_t = 60)]
        fps: u32,

        /// Encoded bytes per frame.
        #[arg(long, default_value_t = 40_000)]
        frame_bytes: usize,

        /// Slices per frame.
        #[arg(long, default_value_t = 4)]
        slices: usize,

        /// Frames to send before stopping.
        #[arg(long, default_value_t = 300)]
        frames: u32,
    },

    /// Receive frames and report reassembly latency.
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
    },

    /// Encode synthetic frames to an Annex B file to verify the encoder.
    #[cfg(target_os = "macos")]
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
    let cli = Cli::parse();

    #[cfg(target_os = "macos")]
    if let Command::Encode {
        out,
        frames,
        width,
        height,
        fps,
        bitrate,
        slice_bytes,
    } = &cli.command
    {
        let config = encode::EncodeConfig {
            out: out.clone(),
            frames: *frames,
            encoder: prism_core::encode::EncoderConfig {
                width: *width,
                height: *height,
                fps: *fps,
                bitrate_bps: *bitrate,
                max_slice_bytes: *slice_bytes,
            },
        };
        return match encode::run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("prism-cli: {err}");
                ExitCode::FAILURE
            }
        };
    }

    let result = match cli.command {
        Command::Host {
            peer,
            fps,
            frame_bytes,
            slices,
            frames,
        } => host::run(host::HostConfig {
            peer,
            fps,
            frame_bytes,
            slices,
            frames,
        }),
        Command::Client {
            bind,
            frames,
            idle_timeout_ms,
            report_every,
            in_flight,
        } => client::run(client::ClientConfig {
            bind,
            frames,
            idle_timeout: Duration::from_millis(idle_timeout_ms),
            report_every,
            in_flight,
        }),
        #[cfg(target_os = "macos")]
        Command::Encode { .. } => unreachable!("handled before the match"),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("prism-cli: {err}");
            ExitCode::FAILURE
        }
    }
}
