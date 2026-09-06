//! Receiving client: reassembles frames from UDP and reports latency.
//!
//! There is no decoder or display here yet. The client measures how long a frame takes
//! to arrive whole, which is the part of the budget the transport is responsible for,
//! and reports the packet-level counters that explain any frame that never arrived.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use prism_core::clock::now_us;
use prism_core::net::packet::{MAX_PACKET_SIZE, VideoPacket};
use prism_core::net::reassemble::{FrameReassembler, PushOutcome};
use prism_core::net::transport::UdpTransport;
use prism_core::stats::LatencyRecorder;

/// How the receiving client should behave.
#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    /// Address to receive packets on.
    pub bind: SocketAddr,
    /// Stop after this many frames, or run until idle if `None`.
    pub frames: Option<u32>,
    /// Give up after this long with no packets.
    pub idle_timeout: Duration,
    /// Print a running summary every this many frames.
    pub report_every: u32,
    /// How many frames may be in flight before the oldest is abandoned.
    pub in_flight: usize,
}

/// Receives packets until the frame budget or the idle timeout is reached.
///
/// Returns once no packet has arrived for `config.idle_timeout`, which is how a run ends
/// when the sender simply stops.
///
/// # Errors
///
/// Returns an [`io::Error`] if the socket cannot be bound or read, other than the
/// timeout that ends the run normally.
pub fn run(config: ClientConfig) -> io::Result<()> {
    let transport = UdpTransport::bind(config.bind)?;
    transport.set_read_timeout(Some(config.idle_timeout))?;

    println!(
        "client: listening on {} ({} frames in flight, {:?} idle timeout)",
        transport.local_addr()?,
        config.in_flight,
        config.idle_timeout
    );

    let mut reassembler = FrameReassembler::new(config.in_flight);
    let mut recorder = LatencyRecorder::new(4096);
    let mut recv_buf = [0u8; MAX_PACKET_SIZE];
    let mut malformed = 0u64;
    let mut frames = 0u32;

    loop {
        let bytes = match transport.recv_into(&mut recv_buf) {
            Ok(bytes) => bytes,
            Err(err) if is_timeout(&err) => break,
            Err(err) => return Err(err),
        };

        let Ok(packet) = VideoPacket::decode(bytes) else {
            malformed += 1;
            continue;
        };

        if reassembler.push(&packet) != PushOutcome::FrameComplete {
            continue;
        }

        let Some(frame) = reassembler.take_completed() else {
            continue;
        };
        let latency_us = now_us().saturating_sub(frame.capture_ts_us);
        recorder.record(latency_us.min(u64::from(u32::MAX)) as u32);

        frames += 1;
        if config.report_every > 0 && frames % config.report_every == 0 {
            report("client", &mut recorder);
        }
        if config.frames.is_some_and(|target| frames >= target) {
            break;
        }
    }

    println!("\nclient: {frames} frames reassembled, {malformed} packets unparseable");
    report("final", &mut recorder);

    let stats = reassembler.stats();
    println!(
        "packets  accepted {}  duplicate {}  stale {}  invalid {}",
        stats.accepted, stats.duplicates, stats.stale, stats.invalid
    );
    println!(
        "frames   completed {}  dropped incomplete {}",
        stats.completed, stats.dropped_incomplete
    );

    Ok(())
}

/// Prints a latency summary under the given label.
///
/// p99 is the figure every milestone is judged against, so it is reported alongside the
/// median rather than buried.
fn report(label: &str, recorder: &mut LatencyRecorder) {
    let Some(summary) = recorder.summarize() else {
        println!("{label}: no frames measured");
        return;
    };

    println!(
        "{label}: n={} min {:.2} p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms",
        summary.count,
        ms(summary.min_us),
        ms(summary.p50_us),
        ms(summary.p95_us),
        ms(summary.p99_us),
        ms(summary.max_us)
    );
}

/// Converts microseconds to milliseconds for display.
fn ms(micros: u32) -> f64 {
    f64::from(micros) / 1000.0
}

/// Returns whether an error is the read timeout that ends a run normally.
///
/// The kind differs by platform: Unix reports `WouldBlock` and Windows `TimedOut`.
fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
