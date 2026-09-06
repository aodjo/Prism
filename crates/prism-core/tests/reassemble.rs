//! Tests for frame reassembly.
//!
//! Real networks deliver packets late, twice, or never. These tests drive the
//! reassembler through each of those and check that a frame is only ever handed on when
//! it is genuinely whole and byte-identical to what the encoder produced.

use prism_core::net::packet::{FLAG_IDR, FLAG_LAST_OF_FRAME, MAX_VIDEO_PAYLOAD, VideoPacket};
use prism_core::net::packetize::SlicePacketizer;
use prism_core::net::reassemble::{FrameReassembler, MAX_SLICES_PER_FRAME, PushOutcome};

/// Builds a deterministic pseudo-random bitstream of the requested length.
fn bitstream(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(31).wrapping_add(usize::from(seed)) & 0xff) as u8)
        .collect()
}

/// Packetises a whole frame, marking the final slice so the receiver can tell where the
/// frame ends.
fn frame_packets<'a>(
    frame_id: u32,
    capture_ts_us: u64,
    slices: &'a [Vec<u8>],
    idr: bool,
) -> Vec<VideoPacket<'a>> {
    let last = slices.len() - 1;

    slices
        .iter()
        .enumerate()
        .flat_map(|(slice_id, data)| {
            let mut flags = if idr { FLAG_IDR } else { 0 };
            if slice_id == last {
                flags |= FLAG_LAST_OF_FRAME;
            }
            SlicePacketizer::new(frame_id, slice_id as u16, flags, capture_ts_us, data).unwrap()
        })
        .collect()
}

/// Reorders a slice deterministically so tests exercise arrival orders without a
/// random number generator.
fn shuffled<T>(items: Vec<T>, stride: usize) -> Vec<T> {
    let len = items.len();
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(len);

    let mut idx = 0;
    for _ in 0..len {
        while slots[idx].is_none() {
            idx = (idx + 1) % len;
        }
        out.push(slots[idx].take().unwrap());
        idx = (idx + stride) % len;
    }

    out
}

/// Feeds every packet in order and returns the outcome of the last one.
fn push_all(reassembler: &mut FrameReassembler, packets: &[VideoPacket<'_>]) -> PushOutcome {
    let mut outcome = PushOutcome::Accepted;
    for packet in packets {
        outcome = reassembler.push(packet);
    }
    outcome
}

#[test]
fn a_single_slice_frame_is_delivered_intact() {
    let slices = vec![bitstream(MAX_VIDEO_PAYLOAD * 2 + 5, 1)];
    let packets = frame_packets(1, 12_345, &slices, true);
    let mut reassembler = FrameReassembler::new(4);

    assert_eq!(
        push_all(&mut reassembler, &packets),
        PushOutcome::FrameComplete
    );

    let frame = reassembler.take_completed().unwrap();
    assert_eq!(frame.frame_id, 1);
    assert_eq!(frame.capture_ts_us, 12_345);
    assert!(frame.is_idr);
    assert_eq!(frame.data, &slices[0][..]);
}

#[test]
fn a_multi_slice_frame_is_concatenated_in_slice_order() {
    let slices = vec![
        bitstream(MAX_VIDEO_PAYLOAD + 1, 1),
        bitstream(MAX_VIDEO_PAYLOAD * 3, 2),
        bitstream(17, 3),
    ];
    let expected: Vec<u8> = slices.concat();
    let packets = frame_packets(9, 1, &slices, false);
    let mut reassembler = FrameReassembler::new(4);

    assert_eq!(
        push_all(&mut reassembler, &packets),
        PushOutcome::FrameComplete
    );

    let frame = reassembler.take_completed().unwrap();
    assert!(!frame.is_idr);
    assert_eq!(frame.data, &expected[..]);
}

#[test]
fn arrival_order_does_not_change_the_result() {
    let slices = vec![
        bitstream(MAX_VIDEO_PAYLOAD * 2 + 3, 1),
        bitstream(MAX_VIDEO_PAYLOAD * 2 + 7, 2),
    ];
    let expected: Vec<u8> = slices.concat();

    for stride in [1, 3, 5, 7] {
        let packets = shuffled(frame_packets(4, 8, &slices, false), stride);
        let mut reassembler = FrameReassembler::new(4);

        assert_eq!(
            push_all(&mut reassembler, &packets),
            PushOutcome::FrameComplete
        );
        assert_eq!(reassembler.take_completed().unwrap().data, &expected[..]);
    }
}

#[test]
fn a_repeated_packet_is_counted_but_not_stored_twice() {
    let slices = vec![bitstream(MAX_VIDEO_PAYLOAD * 3, 1)];
    let packets = frame_packets(1, 0, &slices, false);
    let mut reassembler = FrameReassembler::new(4);

    assert_eq!(reassembler.push(&packets[0]), PushOutcome::Accepted);
    assert_eq!(reassembler.push(&packets[0]), PushOutcome::Duplicate);
    assert_eq!(reassembler.push(&packets[1]), PushOutcome::Accepted);
    assert_eq!(reassembler.push(&packets[1]), PushOutcome::Duplicate);
    assert_eq!(reassembler.push(&packets[2]), PushOutcome::FrameComplete);

    assert_eq!(reassembler.take_completed().unwrap().data, &slices[0][..]);
    assert_eq!(reassembler.stats().duplicates, 2);
}

#[test]
fn a_frame_missing_a_packet_is_never_delivered() {
    let slices = vec![bitstream(MAX_VIDEO_PAYLOAD * 4, 1)];
    let packets = frame_packets(1, 0, &slices, false);
    let mut reassembler = FrameReassembler::new(4);

    for packet in packets.iter().skip(1) {
        assert_eq!(reassembler.push(packet), PushOutcome::Accepted);
    }

    assert!(reassembler.take_completed().is_none());
    assert_eq!(reassembler.stats().completed, 0);
}

#[test]
fn a_frame_missing_its_final_slice_marker_is_never_delivered() {
    let slices = [bitstream(100, 1)];
    let data = &slices[0];
    let packets: Vec<_> = SlicePacketizer::new(1, 0, 0, 0, data).unwrap().collect();
    let mut reassembler = FrameReassembler::new(4);

    assert_eq!(push_all(&mut reassembler, &packets), PushOutcome::Accepted);
    assert!(reassembler.take_completed().is_none());
}

#[test]
fn packets_for_an_already_delivered_frame_are_rejected() {
    let slices = vec![bitstream(500, 1)];
    let packets = frame_packets(5, 0, &slices, false);
    let mut reassembler = FrameReassembler::new(4);

    assert_eq!(
        push_all(&mut reassembler, &packets),
        PushOutcome::FrameComplete
    );
    assert!(reassembler.take_completed().is_some());

    assert_eq!(reassembler.push(&packets[0]), PushOutcome::Stale);
    assert_eq!(reassembler.stats().stale, 1);
}

#[test]
fn several_frames_flow_through_in_order() {
    let mut reassembler = FrameReassembler::new(4);

    for frame_id in 1..=20u32 {
        let slices = vec![
            bitstream(MAX_VIDEO_PAYLOAD + frame_id as usize, frame_id as u8),
            bitstream(64, frame_id as u8),
        ];
        let expected: Vec<u8> = slices.concat();
        let packets = shuffled(
            frame_packets(frame_id, u64::from(frame_id) * 100, &slices, false),
            3,
        );

        assert_eq!(
            push_all(&mut reassembler, &packets),
            PushOutcome::FrameComplete
        );

        let frame = reassembler.take_completed().unwrap();
        assert_eq!(frame.frame_id, frame_id);
        assert_eq!(frame.capture_ts_us, u64::from(frame_id) * 100);
        assert_eq!(frame.data, &expected[..]);
    }

    let stats = reassembler.stats();
    assert_eq!(stats.completed, 20);
    assert_eq!(stats.dropped_incomplete, 0);
}

#[test]
fn delivering_a_frame_abandons_the_older_ones_still_in_flight() {
    let mut reassembler = FrameReassembler::new(4);

    let stalled = vec![bitstream(MAX_VIDEO_PAYLOAD * 3, 1)];
    let stalled_packets = frame_packets(1, 0, &stalled, false);
    assert_eq!(reassembler.push(&stalled_packets[0]), PushOutcome::Accepted);

    let newer = vec![bitstream(200, 2)];
    let newer_packets = frame_packets(2, 0, &newer, false);
    assert_eq!(
        push_all(&mut reassembler, &newer_packets),
        PushOutcome::FrameComplete
    );
    assert_eq!(
        reassembler.take_completed().unwrap().data,
        &newer[..].concat()[..]
    );

    assert_eq!(
        reassembler.stats().dropped_incomplete,
        1,
        "frame 1 cannot help a decoder that already has frame 2"
    );

    for packet in stalled_packets.iter().skip(1) {
        assert_eq!(reassembler.push(packet), PushOutcome::Stale);
    }
}

#[test]
fn the_oldest_frame_is_evicted_when_every_slot_is_in_use() {
    let mut reassembler = FrameReassembler::new(2);

    let bodies: Vec<Vec<Vec<u8>>> = (1..=3)
        .map(|id| vec![bitstream(MAX_VIDEO_PAYLOAD * 3, id as u8)])
        .collect();
    let per_frame: Vec<Vec<VideoPacket<'_>>> = bodies
        .iter()
        .enumerate()
        .map(|(i, slices)| frame_packets(i as u32 + 1, 0, slices, false))
        .collect();

    assert_eq!(reassembler.push(&per_frame[0][0]), PushOutcome::Accepted);
    assert_eq!(reassembler.push(&per_frame[1][0]), PushOutcome::Accepted);
    assert_eq!(reassembler.stats().dropped_incomplete, 0);

    assert_eq!(reassembler.push(&per_frame[2][0]), PushOutcome::Accepted);
    assert_eq!(
        reassembler.stats().dropped_incomplete,
        1,
        "frame 1 makes way for frame 3"
    );

    assert_eq!(
        push_all(&mut reassembler, &per_frame[1][1..]),
        PushOutcome::FrameComplete
    );

    let frame = reassembler.take_completed().unwrap();
    assert_eq!(frame.frame_id, 2, "frame 2 kept its slot and completed");
    assert_eq!(frame.data, &bodies[1].concat()[..]);
}

#[test]
fn a_packet_older_than_everything_in_flight_is_refused() {
    let mut reassembler = FrameReassembler::new(1);

    let recent = vec![bitstream(MAX_VIDEO_PAYLOAD * 2, 9)];
    let recent_packets = frame_packets(10, 0, &recent, false);
    assert_eq!(reassembler.push(&recent_packets[0]), PushOutcome::Accepted);

    let ancient = vec![bitstream(MAX_VIDEO_PAYLOAD * 2, 1)];
    let ancient_packets = frame_packets(2, 0, &ancient, false);
    assert_eq!(reassembler.push(&ancient_packets[0]), PushOutcome::Stale);
    assert_eq!(
        reassembler.stats().dropped_incomplete,
        0,
        "frame 10 keeps its slot"
    );
}

#[test]
fn an_untaken_frame_is_dropped_when_the_next_frame_starts() {
    let mut reassembler = FrameReassembler::new(4);

    let first = vec![bitstream(200, 1)];
    assert_eq!(
        push_all(&mut reassembler, &frame_packets(1, 0, &first, false)),
        PushOutcome::FrameComplete
    );

    let second = vec![bitstream(200, 2)];
    let second_packets = frame_packets(2, 0, &second, false);
    assert_eq!(
        push_all(&mut reassembler, &second_packets),
        PushOutcome::FrameComplete
    );

    let frame = reassembler.take_completed().unwrap();
    assert_eq!(
        frame.frame_id, 2,
        "the newer frame wins when the older was never taken"
    );
    assert_eq!(reassembler.stats().dropped_incomplete, 1);
}

#[test]
fn malformed_packets_are_rejected() {
    let mut reassembler = FrameReassembler::new(4);
    let full = vec![0u8; MAX_VIDEO_PAYLOAD];

    let cases: [(&str, VideoPacket<'_>); 5] = [
        (
            "pkt_count of zero",
            VideoPacket {
                frame_id: 1,
                slice_id: 0,
                pkt_idx: 0,
                pkt_count: 0,
                flags: 0,
                capture_ts_us: 0,
                payload: &full,
            },
        ),
        (
            "index past the count",
            VideoPacket {
                frame_id: 1,
                slice_id: 0,
                pkt_idx: 3,
                pkt_count: 3,
                flags: 0,
                capture_ts_us: 0,
                payload: &full,
            },
        ),
        (
            "empty payload",
            VideoPacket {
                frame_id: 1,
                slice_id: 0,
                pkt_idx: 0,
                pkt_count: 1,
                flags: 0,
                capture_ts_us: 0,
                payload: &[],
            },
        ),
        (
            "short packet in the middle of a slice",
            VideoPacket {
                frame_id: 1,
                slice_id: 0,
                pkt_idx: 0,
                pkt_count: 2,
                flags: 0,
                capture_ts_us: 0,
                payload: &[1, 2, 3],
            },
        ),
        (
            "slice_id beyond the ceiling",
            VideoPacket {
                frame_id: 1,
                slice_id: MAX_SLICES_PER_FRAME as u16,
                pkt_idx: 0,
                pkt_count: 1,
                flags: 0,
                capture_ts_us: 0,
                payload: &[1],
            },
        ),
    ];

    for (name, packet) in cases {
        assert_eq!(reassembler.push(&packet), PushOutcome::Invalid, "{name}");
    }

    assert_eq!(reassembler.stats().invalid, 5);
}

#[test]
fn a_slice_whose_packet_count_changes_is_rejected() {
    let mut reassembler = FrameReassembler::new(4);
    let full = vec![0u8; MAX_VIDEO_PAYLOAD];

    let first = VideoPacket {
        frame_id: 1,
        slice_id: 0,
        pkt_idx: 0,
        pkt_count: 3,
        flags: 0,
        capture_ts_us: 0,
        payload: &full,
    };
    let contradicting = VideoPacket {
        frame_id: 1,
        slice_id: 0,
        pkt_idx: 1,
        pkt_count: 5,
        flags: 0,
        capture_ts_us: 0,
        payload: &full,
    };

    assert_eq!(reassembler.push(&first), PushOutcome::Accepted);
    assert_eq!(reassembler.push(&contradicting), PushOutcome::Invalid);
}

#[test]
fn frame_numbering_survives_wrapping() {
    let mut reassembler = FrameReassembler::new(4);

    for offset in 0..4u32 {
        let frame_id = u32::MAX.wrapping_sub(1).wrapping_add(offset);
        let slices = vec![bitstream(300, offset as u8)];
        let packets = frame_packets(frame_id, 0, &slices, false);

        assert_eq!(
            push_all(&mut reassembler, &packets),
            PushOutcome::FrameComplete
        );

        let frame = reassembler.take_completed().unwrap();
        assert_eq!(frame.frame_id, frame_id);
        assert_eq!(frame.data, &slices[0][..]);
    }

    assert_eq!(reassembler.stats().completed, 4);
    assert_eq!(reassembler.stats().stale, 0);
}
