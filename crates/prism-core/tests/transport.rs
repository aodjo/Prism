//! Tests for the UDP transport.
//!
//! These run a real socket pair on the loopback interface so the packetiser, the wire
//! format, the socket, and the reassembler are exercised together. Each frame is sent
//! and then drained before the next begins, which keeps the test deterministic rather
//! than dependent on socket buffer sizing.

use std::time::Duration;

use prism_core::net::packet::{
    FLAG_IDR, FLAG_LAST_OF_FRAME, MAX_PACKET_SIZE, MAX_VIDEO_PAYLOAD, VideoPacket,
};
use prism_core::net::packetize::SlicePacketizer;
use prism_core::net::reassemble::{FrameReassembler, PushOutcome};
use prism_core::net::transport::UdpTransport;

/// Builds a deterministic bitstream so a swapped or repeated slice shows up as wrong bytes.
fn bitstream(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(31).wrapping_add(seed) & 0xff) as u8)
        .collect()
}

/// Creates a connected sender and a bound receiver on the loopback interface.
///
/// Both use an ephemeral port so concurrent test runs cannot collide.
fn socket_pair() -> (UdpTransport, UdpTransport) {
    let local = "127.0.0.1:0".parse().expect("valid loopback address");

    let receiver = UdpTransport::bind(local).expect("receiver binds");
    receiver
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout is settable");

    let sender = UdpTransport::bind(local).expect("sender binds");
    sender
        .connect(receiver.local_addr().expect("receiver has an address"))
        .expect("connects");

    (sender, receiver)
}

#[test]
fn frames_survive_a_round_trip_over_a_real_socket() {
    let (sender, receiver) = socket_pair();
    let mut reassembler = FrameReassembler::new(4);
    let mut send_buf = [0u8; MAX_PACKET_SIZE];
    let mut recv_buf = [0u8; MAX_PACKET_SIZE];

    for frame_id in 0..5u32 {
        let slices = [
            bitstream(MAX_VIDEO_PAYLOAD * 2 + 13, frame_id as usize),
            bitstream(MAX_VIDEO_PAYLOAD + 7, frame_id as usize + 100),
        ];
        let expected: Vec<u8> = slices.concat();
        let capture_ts_us = 1_000_000 + u64::from(frame_id);

        let mut sent = 0;
        for (slice_id, slice) in slices.iter().enumerate() {
            let mut flags = if frame_id == 0 { FLAG_IDR } else { 0 };
            if slice_id + 1 == slices.len() {
                flags |= FLAG_LAST_OF_FRAME;
            }

            let packetizer =
                SlicePacketizer::new(frame_id, slice_id as u16, flags, capture_ts_us, slice)
                    .expect("slice is packetisable");

            for packet in packetizer {
                let len = packet.encode_into(&mut send_buf).expect("packet fits");
                sender.send(&send_buf[..len]).expect("send succeeds");
                sent += 1;
            }
        }

        let mut completed = false;
        for _ in 0..sent {
            let bytes = receiver.recv_into(&mut recv_buf).expect("packet arrives");
            let packet = VideoPacket::decode(bytes).expect("packet parses");

            if reassembler.push(&packet) == PushOutcome::FrameComplete {
                let frame = reassembler
                    .take_completed()
                    .expect("completed frame is available");
                assert_eq!(frame.frame_id, frame_id);
                assert_eq!(frame.capture_ts_us, capture_ts_us);
                assert_eq!(frame.is_idr, frame_id == 0);
                assert_eq!(
                    frame.data,
                    &expected[..],
                    "frame {frame_id} came back wrong"
                );
                completed = true;
            }
        }

        assert!(completed, "frame {frame_id} never completed");
    }

    let stats = reassembler.stats();
    assert_eq!(stats.completed, 5);
    assert_eq!(stats.duplicates, 0);
    assert_eq!(stats.invalid, 0);
    assert_eq!(stats.dropped_incomplete, 0);
}

#[test]
fn a_receiver_with_no_sender_times_out_rather_than_hanging() {
    let local = "127.0.0.1:0".parse().expect("valid loopback address");
    let receiver = UdpTransport::bind(local).expect("receiver binds");
    receiver
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("timeout is settable");

    let mut recv_buf = [0u8; MAX_PACKET_SIZE];
    let err = receiver
        .recv_into(&mut recv_buf)
        .expect_err("nothing was sent");

    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "expected a timeout, got {:?}",
        err.kind()
    );
}
