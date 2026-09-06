//! Client side: reassembles frames and, when asked, decodes them.
//!
//! Receiving and decoding run on separate threads, and that separation is not an
//! optimisation. A single-threaded client stops reading the socket while it decodes, so
//! packets queue in the kernel buffer and every frame behind the one being decoded is
//! measured as late. Collapsing the two inflated p99 from 5.9 ms to 51.8 ms in an early
//! version of this file while the pipeline itself was unchanged.
//!
//! The receive thread therefore does nothing but read, reassemble, and hand off. When the
//! decoder falls behind, frames are dropped rather than queued: a frame that has waited
//! behind another is already too late to be worth showing.

use std::io;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread;
use std::time::Duration;

use prism_core::clock::now_us;
use prism_core::net::packet::{MAX_PACKET_SIZE, VideoPacket};
use prism_core::net::reassemble::{FrameReassembler, PushOutcome};
use prism_core::net::transport::UdpTransport;
use prism_core::stats::{LatencyRecorder, LatencySummary};

/// Where decoded pictures go when the client is showing them.
///
/// The payload type differs by platform because only macOS has a decoder so far; the
/// alias keeps the signatures below identical everywhere.
#[cfg(target_os = "macos")]
pub type PictureSink = SyncSender<prism_core::decode::videotoolbox::DecodedFrame>;

/// Where decoded pictures would go on a platform with no decoder yet.
#[cfg(not(target_os = "macos"))]
pub type PictureSink = SyncSender<()>;

/// How many frames may wait for the decoder before the newest is dropped.
///
/// Two, because a frame that has queued behind another has already missed its moment.
const DECODE_QUEUE_DEPTH: usize = 2;

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
    /// Decode the reassembled frames rather than only counting them.
    pub decode: bool,
}

/// A reassembled frame on its way from the receive thread to the decode thread.
///
/// The buffer is recycled back to the receive thread after decoding, so a running
/// session does not allocate.
#[derive(Debug, Default)]
struct FrameBuf {
    capture_ts_us: u64,
    data: Vec<u8>,
}

/// What the decode thread reports when it finishes.
#[derive(Debug, Default)]
struct DecodeReport {
    decoded: u32,
    /// Submissions that produced no picture before the poll timeout expired.
    starved: u32,
    /// End to end, from just before the encoder to just after the decoder.
    summary: Option<LatencySummary>,
    /// The decode stage alone, from submitting a frame to holding its picture.
    stage: Option<LatencySummary>,
    /// How far the picture that came out lags the frame that went in.
    lag: Option<LatencySummary>,
    errors: Vec<i32>,
}

/// Receives packets until the frame budget or the idle timeout is reached.
///
/// # Errors
///
/// Returns an [`io::Error`] if the socket cannot be bound or read, other than the
/// timeout that ends the run normally.
pub fn run(config: ClientConfig, pictures: Option<PictureSink>) -> io::Result<()> {
    let transport = UdpTransport::bind(config.bind)?;
    transport.set_read_timeout(Some(config.idle_timeout))?;

    println!(
        "client: listening on {} ({} frames in flight, decode {})",
        transport.local_addr()?,
        config.in_flight,
        if config.decode { "on" } else { "off" }
    );

    let (frames_tx, frames_rx) = sync_channel::<FrameBuf>(DECODE_QUEUE_DEPTH);
    let (recycle_tx, recycle_rx) = channel::<FrameBuf>();
    let decoder = config
        .decode
        .then(|| spawn_decoder(frames_rx, recycle_tx, pictures));

    let mut reassembler = FrameReassembler::new(config.in_flight);
    let mut arrival = LatencyRecorder::new(4096);
    let mut recv_buf = [0u8; MAX_PACKET_SIZE];
    let mut malformed = 0u64;
    let mut frames = 0u32;
    let mut behind = 0u32;

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

        arrival.record(elapsed_us(frame.capture_ts_us));
        frames += 1;

        if config.decode {
            let mut buf = recycle_rx.try_recv().unwrap_or_default();
            buf.capture_ts_us = frame.capture_ts_us;
            buf.data.clear();
            buf.data.extend_from_slice(frame.data);

            if frames_tx.try_send(buf).is_err() {
                behind += 1;
            }
        }

        if config.report_every > 0 && frames % config.report_every == 0 {
            report("arrival ", &mut arrival);
        }
        if config.frames.is_some_and(|target| frames >= target) {
            break;
        }
    }

    drop(frames_tx);
    let decode_report = decoder.map(|handle| handle.join().unwrap_or_default());

    println!("\nclient: {frames} frames reassembled, {malformed} packets unparseable");
    report("arrival ", &mut arrival);

    if let Some(decode_report) = decode_report {
        println!(
            "client: {} frames decoded, {behind} dropped at the decoder, {} produced no picture in time",
            decode_report.decoded, decode_report.starved
        );
        print_summary("decode  ", decode_report.stage);
        print_summary("outlag  ", decode_report.lag);
        print_summary("pipeline", decode_report.summary);
        if !decode_report.errors.is_empty() {
            println!(
                "client: decoder reported {} failures: {:?}",
                decode_report.errors.len(),
                decode_report.errors
            );
        }
    }

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

/// Starts the decode thread.
///
/// The decoder is created inside the thread and never leaves it, which keeps every
/// platform session object on the one thread that owns it.
#[cfg(target_os = "macos")]
fn spawn_decoder(
    frames: Receiver<FrameBuf>,
    recycle: Sender<FrameBuf>,
    pictures: Option<PictureSink>,
) -> thread::JoinHandle<DecodeReport> {
    use prism_core::decode::DecodeError;
    use prism_core::decode::videotoolbox::VideoToolboxDecoder;

    /// How long the decode thread waits for a picture before moving on.
    ///
    /// Short on purpose. Blocking here stalls the whole decode thread, so every frame
    /// behind the one being waited for is measured as late and may be dropped. Roughly
    /// two frame intervals is long enough to absorb the decoder's own pipelining and
    /// short enough that a stall cannot cascade.
    const POLL_TIMEOUT: Duration = Duration::from_millis(8);

    thread::spawn(move || {
        let mut decoder = VideoToolboxDecoder::new();
        let mut latency = LatencyRecorder::new(4096);
        let mut stage = LatencyRecorder::new(4096);
        let mut decoded = 0u32;
        let mut starved = 0u32;
        let mut lag = LatencyRecorder::new(4096);

        while let Ok(buf) = frames.recv() {
            let started = std::time::Instant::now();

            match decoder.decode(&buf.data, buf.capture_ts_us) {
                Ok(()) | Err(DecodeError::NoParameterSets) => {}
                Err(err) => eprintln!("client: {err}"),
            }

            if let Some(picture) = decoder.poll(POLL_TIMEOUT) {
                lag.record(
                    buf.capture_ts_us
                        .saturating_sub(picture.pts_us)
                        .min(u64::from(u32::MAX)) as u32,
                );
                latency.record(elapsed_us(picture.pts_us));
                stage.record(started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
                decoded += 1;

                if let Some(sink) = pictures.as_ref() {
                    // Dropped rather than queued: a picture that waits its turn is
                    // already too late to be worth showing.
                    let _ = sink.try_send(picture);
                }
            } else {
                starved += 1;
            }

            let _ = recycle.send(buf);
        }

        DecodeReport {
            decoded,
            starved,
            summary: latency.summarize(),
            stage: stage.summarize(),
            lag: lag.summarize(),
            errors: decoder.take_errors(),
        }
    })
}

/// Starts a decode thread on a platform with no decoder yet.
///
/// Drains the channel so the receive thread never blocks handing frames over.
#[cfg(not(target_os = "macos"))]
fn spawn_decoder(
    frames: Receiver<FrameBuf>,
    recycle: Sender<FrameBuf>,
    pictures: Option<PictureSink>,
) -> thread::JoinHandle<DecodeReport> {
    let _ = pictures;
    thread::spawn(move || {
        while let Ok(buf) = frames.recv() {
            let _ = recycle.send(buf);
        }
        DecodeReport::default()
    })
}

/// Returns how long ago a host timestamp was, clamped to what a sample can hold.
///
/// Host and client share a clock here because both run on one machine. Comparing across
/// machines needs the clock synchronisation that arrives in M2.
fn elapsed_us(timestamp_us: u64) -> u32 {
    now_us()
        .saturating_sub(timestamp_us)
        .min(u64::from(u32::MAX)) as u32
}

/// Prints a latency summary under the given label.
fn report(label: &str, recorder: &mut LatencyRecorder) {
    print_summary(label, recorder.summarize());
}

/// Prints an already computed summary, or a placeholder when there is none.
fn print_summary(label: &str, summary: Option<LatencySummary>) {
    let Some(summary) = summary else {
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
