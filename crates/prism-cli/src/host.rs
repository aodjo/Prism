//! Host side: produces frames and sends them.
//!
//! Two sources exist. The synthetic one emits bytes shaped like video and exercises the
//! transport alone. The encoded one runs the real hardware encoder, which is what makes
//! the client's latency figure describe the pipeline rather than just the network.

use std::io;
use std::net::SocketAddr;
use std::thread::sleep;
use std::time::{Duration, Instant};

use prism_core::clock::now_us;

use crate::wire::SliceSender;

/// How the host should shape its traffic.
#[derive(Debug, Clone, Copy)]
pub struct HostConfig {
    /// Address of the receiving client.
    pub peer: SocketAddr,
    /// Frames to send per second.
    pub fps: u32,
    /// Encoded bytes per frame, for the synthetic source only.
    pub frame_bytes: usize,
    /// Slices per frame, for the synthetic source only.
    pub slices: usize,
    /// Frames to send before stopping.
    pub frames: u32,
    /// Video packets to drop on the way out, in parts per million.
    ///
    /// Zero sends everything. This is how a run is made lossy without an operating system
    /// traffic shaper, so the recovery machinery can be judged reproducibly and in CI.
    pub loss_ppm: u32,
    /// Seed for the loss injector, so a failing run repeats exactly.
    pub loss_seed: u64,
    /// Loss estimate to size Reed-Solomon parity against, or `None` to send none.
    pub parity_loss: Option<f32>,
    /// Bitrate to pace outgoing packets at, or `None` to send them at line rate.
    pub pace_bps: Option<u32>,
    /// Whether the congestion controller may drive the pacing rate from feedback.
    pub adaptive: bool,
}

/// Sends `config.frames` synthetic frames and reports what was transmitted.
///
/// Frames are paced to the requested rate by sleeping to each frame's deadline. Packets
/// within a frame go out back to back; spreading them across the interval is the send
/// pacer's job and arrives in M4.
///
/// # Errors
///
/// Returns an [`io::Error`] if the socket cannot be bound, connected, or written to.
///
/// # Panics
///
/// Panics if `config.slices` is zero or `config.frame_bytes` is smaller than
/// `config.slices`, since neither describes a frame an encoder could produce.
pub fn run(config: HostConfig) -> io::Result<()> {
    assert!(config.slices > 0, "a frame needs at least one slice");
    assert!(
        config.frame_bytes >= config.slices,
        "every slice needs at least one byte"
    );

    let mut sender = SliceSender::connect(config.peer)?;
    if let Some(bitrate) = config.pace_bps {
        sender.enable_pacing(bitrate, config.adaptive);
    }
    sender.serve_return_path(true)?;
    if let Some(loss) = config.parity_loss {
        sender.enable_parity(loss);
    }
    if config.loss_ppm > 0 {
        sender.inject_loss(config.loss_ppm, config.loss_seed);
    }
    let slices = build_slices(config.frame_bytes, config.slices);
    let interval = frame_interval(config.fps);

    println!(
        "host: sending {} synthetic frames of {} bytes in {} slices at {} fps to {}",
        config.frames, config.frame_bytes, config.slices, config.fps, config.peer
    );

    let start = Instant::now();

    for frame_id in 0..config.frames {
        pace(start, interval, frame_id);
        // Ahead of the frame's own packets, so eighteen bytes the cursor depends on are
        // not queued behind a whole frame of video.
        sender.send_cursor()?;
        let capture_ts_us = now_us();
        sender.note_capture(frame_id, capture_ts_us);

        // The synthetic source follows the controller the way a real encoder would, by
        // producing less. Sending a shorter prefix of each slice rather than rebuilding it
        // keeps the frame path free of allocation.
        let budget = frame_budget(sender.target_bps(), config.fps, config.frame_bytes);

        for (slice_id, slice) in slices.iter().enumerate() {
            let bytes = slice_prefix(slice, slice_id, slices.len(), budget);
            sender.send_slice(
                frame_id,
                slice_id as u16,
                bytes,
                capture_ts_us,
                frame_id == 0,
                slice_id + 1 == slices.len(),
            )?;
        }
    }

    report(&sender, start.elapsed());
    Ok(())
}

/// Encodes synthetic pictures with the hardware encoder and sends the result.
///
/// The capture timestamp is taken before the frame enters the encoder, so the latency
/// the client measures covers encode, network, reassembly, and decode together.
///
/// # Errors
///
/// Returns an error if the encoder cannot be created, a frame cannot be encoded, or the
/// socket cannot be written to.
#[cfg(target_os = "macos")]
pub fn run_encoded(
    config: HostConfig,
    encoder_config: prism_core::encode::EncoderConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    use prism_core::encode::videotoolbox::{Nv12Frame, VideoToolboxEncoder};

    let mut sender = SliceSender::connect(config.peer)?;
    if let Some(bitrate) = config.pace_bps {
        sender.enable_pacing(bitrate, config.adaptive);
    }
    sender.serve_return_path(true)?;
    if let Some(loss) = config.parity_loss {
        sender.enable_parity(loss);
    }
    if config.loss_ppm > 0 {
        sender.inject_loss(config.loss_ppm, config.loss_seed);
    }
    let mut encoder = VideoToolboxEncoder::new(encoder_config)?;
    let mut picture = Nv12Frame::new(encoder_config.width, encoder_config.height)?;
    let interval = frame_interval(config.fps);

    println!(
        "host: encoding {} frames at {}x{} {} fps, {} kbps, to {}",
        config.frames,
        encoder_config.width,
        encoder_config.height,
        config.fps,
        encoder_config.bitrate_bps / 1000,
        config.peer
    );
    if !encoder.slicing_supported() {
        println!(
            "host: this encoder emits one slice per frame, so transmission cannot start early"
        );
    }

    let start = Instant::now();
    let mut dropped = 0u32;

    for frame_id in 0..config.frames {
        pace(start, interval, frame_id);
        sender.send_cursor()?;

        crate::pattern::paint(&mut picture, frame_id as usize)?;
        let capture_ts_us = now_us();
        encoder.encode(picture.pixel_buffer(), capture_ts_us, frame_id == 0)?;

        let Some(frame) = encoder.poll(Duration::from_millis(200)) else {
            dropped += 1;
            continue;
        };

        let capture_ts_us = frame.pts_us;
        sender.note_capture(frame_id, capture_ts_us);
        let is_idr = frame.is_idr;
        let last = frame.slices.len() - 1;

        for slice_id in 0..frame.slices.len() {
            let data = frame.slice(slice_id).expect("slice index is in range");
            sender.send_slice(
                frame_id,
                slice_id as u16,
                data,
                capture_ts_us,
                is_idr,
                slice_id == last,
            )?;
        }
    }

    report(&sender, start.elapsed());
    if dropped > 0 {
        println!("host: {dropped} frames produced nothing within the encode deadline");
    }

    Ok(())
}

/// Captures the screen, encodes it, and sends the result.
///
/// The capture timestamp is taken when the compositor hands the frame over, so the
/// latency the client measures covers everything from that moment onward.
///
/// # Errors
///
/// Returns an error if capture cannot start — most often because Screen Recording has not
/// been granted — or if the encoder or socket fails.
#[cfg(target_os = "macos")]
pub fn run_captured(
    config: HostConfig,
    bitrate_bps: u32,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    use prism_core::capture::CaptureConfig;
    use prism_core::capture::screencapturekit::ScreenCapture;
    use prism_core::encode::videotoolbox::VideoToolboxEncoder;

    let mut capture = ScreenCapture::start(CaptureConfig {
        fps: config.fps,
        width,
        height,
        ..CaptureConfig::default()
    })?;

    let (width, height) = (capture.width(), capture.height());
    let encoder_config = prism_core::encode::EncoderConfig {
        width,
        height,
        fps: config.fps,
        bitrate_bps,
        max_slice_bytes: bitrate_bps / 8 / config.fps.max(1) / 4,
    };

    let mut encoder = VideoToolboxEncoder::new(encoder_config)?;
    let mut sender = SliceSender::connect(config.peer)?;
    if let Some(bitrate) = config.pace_bps {
        sender.enable_pacing(bitrate, config.adaptive);
    }
    sender.serve_return_path(true)?;
    if let Some(loss) = config.parity_loss {
        sender.enable_parity(loss);
    }
    if config.loss_ppm > 0 {
        sender.inject_loss(config.loss_ppm, config.loss_seed);
    }

    println!(
        "host: capturing the screen at {width}x{height} {} fps, {} kbps, to {}",
        config.fps,
        bitrate_bps / 1000,
        config.peer
    );

    let start = Instant::now();
    let mut sent_frames = 0u32;
    let mut idle = 0u32;

    while sent_frames < config.frames {
        let Some(captured) = capture.poll(Duration::from_millis(500)) else {
            idle += 1;
            if idle > 20 {
                return Err("the compositor stopped delivering frames".into());
            }
            continue;
        };
        idle = 0;
        sender.send_cursor()?;

        let capture_ts_us = captured.capture_ts_us;
        encoder.encode(captured.pixel_buffer(), capture_ts_us, sent_frames == 0)?;

        let Some(frame) = encoder.poll(Duration::from_millis(200)) else {
            continue;
        };

        sender.note_capture(sent_frames, capture_ts_us);
        let is_idr = frame.is_idr;
        let last = frame.slices.len() - 1;
        for slice_id in 0..frame.slices.len() {
            let data = frame.slice(slice_id).expect("slice index is in range");
            sender.send_slice(
                sent_frames,
                slice_id as u16,
                data,
                frame.pts_us,
                is_idr,
                slice_id == last,
            )?;
        }

        sent_frames += 1;
    }

    report(&sender, start.elapsed());
    Ok(())
}

/// Returns the interval between frames at the requested rate.
fn frame_interval(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))
}

/// Sleeps until the deadline for `frame_id`.
///
/// Pacing against a fixed origin rather than the previous frame keeps the average rate
/// exact, so a late frame does not push every frame after it.
fn pace(start: Instant, interval: Duration, frame_id: u32) {
    let deadline = start + interval * frame_id;
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        sleep(remaining);
    }
}

/// Prints what was transmitted.
fn report(sender: &SliceSender, elapsed: Duration) {
    println!(
        "host: sent {} packets, {:.1} MB in {:.2}s ({:.1} Mbps)",
        sender.packets(),
        sender.bytes() as f64 / 1e6,
        elapsed.as_secs_f64(),
        sender.bytes() as f64 * 8.0 / elapsed.as_secs_f64() / 1e6
    );
    sender.report_pacing();
    if sender.parity_sent() > 0 {
        println!("parity  : {} shards sent", sender.parity_sent());
    }
    sender.report_loss();
    sender.report_feedback();
}

/// Returns how many bytes this frame may carry, given what the controller wants.
///
/// A real encoder is told a bitrate and produces frames that average it. The synthetic
/// source does the same arithmetic directly. Without this the controller has nothing to
/// actuate: pacing alone slows the wire while the source keeps producing at full rate, and
/// the queue simply moves inside the host.
fn frame_budget(target_bps: Option<u32>, fps: u32, configured: usize) -> usize {
    let Some(bps) = target_bps else {
        return configured;
    };

    let wanted = bps as usize / 8 / fps.max(1) as usize;

    // Never more than the source was built to produce, and never so little that a slice
    // ends up empty — an encoder cannot emit a frame of nothing either.
    wanted.clamp(MIN_FRAME_BYTES, configured)
}

/// Returns the prefix of one slice that fits inside a frame budget.
///
/// The budget is split evenly, and the remainder goes to the last slice so the frame's
/// total is exactly the budget rather than a rounding error below it.
fn slice_prefix(slice: &[u8], slice_id: usize, slices: usize, budget: usize) -> &[u8] {
    let base = budget / slices;
    let wanted = if slice_id + 1 == slices {
        base + budget % slices
    } else {
        base
    };

    &slice[..wanted.clamp(1, slice.len())]
}

/// Smallest frame the synthetic source will produce, in bytes.
///
/// One packet per slice at the eight slices the pipeline's encoders use at most. Below this
/// the shape stops resembling a frame and the measurement stops meaning anything.
const MIN_FRAME_BYTES: usize = 8 * 1200;

/// Splits a frame budget into slice bitstreams with a distinguishable byte pattern.
///
/// The pattern differs per slice so a reassembly bug that swaps or repeats a slice shows
/// up as wrong bytes rather than passing unnoticed.
fn build_slices(frame_bytes: usize, slices: usize) -> Vec<Vec<u8>> {
    let base = frame_bytes / slices;
    let remainder = frame_bytes % slices;

    (0..slices)
        .map(|slice_id| {
            let len = base + usize::from(slice_id < remainder);
            (0..len)
                .map(|i| (i.wrapping_mul(31).wrapping_add(slice_id) & 0xff) as u8)
                .collect()
        })
        .collect()
}
