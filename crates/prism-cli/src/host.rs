//! Synthetic host: generates frames, packetises them, and sends them over UDP.
//!
//! There is no capture or encoder here yet. The point is to exercise the transport with
//! traffic shaped like real video — a frame every interval, cut into slices, each slice
//! cut into MTU-sized packets — so the receive path can be measured before any
//! platform-specific code exists.

use std::io;
use std::net::SocketAddr;
use std::thread::sleep;
use std::time::{Duration, Instant};

use prism_core::clock::now_us;
use prism_core::net::packet::{FLAG_IDR, FLAG_LAST_OF_FRAME, MAX_PACKET_SIZE};
use prism_core::net::packetize::SlicePacketizer;
use prism_core::net::transport::UdpTransport;

/// How the synthetic host should shape its traffic.
#[derive(Debug, Clone, Copy)]
pub struct HostConfig {
    /// Address to send packets to.
    pub peer: SocketAddr,
    /// Frames to send per second.
    pub fps: u32,
    /// Total encoded bytes per frame, split evenly across slices.
    pub frame_bytes: usize,
    /// Slices per frame, mirroring what a low-latency encoder would emit.
    pub slices: usize,
    /// How many frames to send before stopping.
    pub frames: u32,
}

/// Sends `config.frames` synthetic frames and reports what was transmitted.
///
/// Frames are paced to the requested rate by sleeping to each frame's deadline. Packets
/// within a frame go out back to back; spreading them across the frame interval is the
/// send pacer's job and arrives in M4.
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

    let transport = UdpTransport::bind("0.0.0.0:0".parse().expect("valid bind address"))?;
    transport.connect(config.peer)?;

    let slices = build_slices(config.frame_bytes, config.slices);
    let mut send_buf = [0u8; MAX_PACKET_SIZE];
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps));

    println!(
        "host: sending {} frames of {} bytes in {} slices at {} fps to {}",
        config.frames, config.frame_bytes, config.slices, config.fps, config.peer
    );

    let start = Instant::now();
    let mut packets_sent = 0u64;
    let mut bytes_sent = 0u64;

    for frame_id in 0..config.frames {
        let deadline = start + frame_interval * frame_id;
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            sleep(remaining);
        }

        let capture_ts_us = now_us();

        for (slice_id, slice) in slices.iter().enumerate() {
            let mut flags = 0;
            if frame_id == 0 {
                flags |= FLAG_IDR;
            }
            if slice_id + 1 == slices.len() {
                flags |= FLAG_LAST_OF_FRAME;
            }

            let packetizer =
                SlicePacketizer::new(frame_id, slice_id as u16, flags, capture_ts_us, slice)
                    .expect("synthetic slices are always packetisable");

            for packet in packetizer {
                let len = packet
                    .encode_into(&mut send_buf)
                    .expect("packet fits the send buffer");
                transport.send(&send_buf[..len])?;
                packets_sent += 1;
                bytes_sent += len as u64;
            }
        }
    }

    let elapsed = start.elapsed();
    println!(
        "host: sent {packets_sent} packets, {:.1} MB in {:.2}s ({:.1} Mbps)",
        bytes_sent as f64 / 1e6,
        elapsed.as_secs_f64(),
        bytes_sent as f64 * 8.0 / elapsed.as_secs_f64() / 1e6
    );

    Ok(())
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
