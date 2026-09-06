//! Headless Prism host and client.
//!
//! This binary is how M1 through M4 are developed and measured: it runs the data plane
//! with no Electron and no window, so the latency numbers describe the pipeline rather
//! than a compositor. CI drives it for protocol regression runs.

mod client;
mod host;

use std::net::SocketAddr;
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
}

/// Parses the command line and runs the requested side.
fn main() -> ExitCode {
    let cli = Cli::parse();

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
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("prism-cli: {err}");
            ExitCode::FAILURE
        }
    }
}
