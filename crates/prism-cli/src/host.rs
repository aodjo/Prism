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
    let slices = build_slices(config.frame_bytes, config.slices);
    let interval = frame_interval(config.fps);

    println!(
        "host: sending {} synthetic frames of {} bytes in {} slices at {} fps to {}",
        config.frames, config.frame_bytes, config.slices, config.fps, config.peer
    );

    let start = Instant::now();

    for frame_id in 0..config.frames {
        pace(start, interval, frame_id);
        let capture_ts_us = now_us();

        for (slice_id, slice) in slices.iter().enumerate() {
            sender.send_slice(
                frame_id,
                slice_id as u16,
                slice,
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

        crate::pattern::paint(&mut picture, frame_id as usize)?;
        let capture_ts_us = now_us();
        encoder.encode(&picture, capture_ts_us, frame_id == 0)?;

        let Some(frame) = encoder.poll(Duration::from_millis(200)) else {
            dropped += 1;
            continue;
        };

        let capture_ts_us = frame.pts_us;
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
}

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
