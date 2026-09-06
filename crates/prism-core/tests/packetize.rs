//! Tests for slice packetisation.
//!
//! The property that matters is that a slice survives the full trip: cut into packets,
//! serialised onto the wire, parsed back, and concatenated in order, it must equal the
//! bitstream the encoder produced. Everything else here guards a boundary around that.

use prism_core::net::packet::{
    FLAG_IDR, FLAG_LAST_OF_FRAME, MAX_PACKET_SIZE, MAX_VIDEO_PAYLOAD, VideoPacket,
};
use prism_core::net::packetize::{MAX_SLICE_LEN, PacketizeError, SlicePacketizer};

/// Builds a deterministic pseudo-random bitstream of the requested length.
///
/// A counter pattern would hide index errors that happen to line up with packet
/// boundaries, so the bytes are spread with a multiplier and an offset instead.
fn bitstream(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(37).wrapping_add(11) & 0xff) as u8)
        .collect()
}

/// Sends a slice through packetisation, serialisation, and parsing, returning the
/// bytes the receiver would reassemble.
///
/// # Panics
///
/// Panics if packetisation, encoding, or decoding fails, since every input reaching
/// this helper is expected to be valid.
fn round_trip(frame_id: u32, slice_id: u16, flags: u8, ts: u64, data: &[u8]) -> Vec<u8> {
    let packetizer = SlicePacketizer::new(frame_id, slice_id, flags, ts, data).unwrap();
    let expected_count = packetizer.packet_count();

    let wire: Vec<Vec<u8>> = packetizer
        .map(|packet| {
            let mut buf = [0u8; MAX_PACKET_SIZE];
            let written = packet.encode_into(&mut buf).unwrap();
            buf[..written].to_vec()
        })
        .collect();

    assert_eq!(
        wire.len(),
        usize::from(expected_count),
        "packet_count must be exact"
    );

    let mut reassembled = Vec::with_capacity(data.len());
    for (idx, bytes) in wire.iter().enumerate() {
        let packet = VideoPacket::decode(bytes).unwrap();

        assert_eq!(packet.frame_id, frame_id);
        assert_eq!(packet.slice_id, slice_id);
        assert_eq!(packet.flags, flags);
        assert_eq!(packet.capture_ts_us, ts);
        assert_eq!(
            packet.pkt_idx, idx as u16,
            "packets must be numbered in order"
        );
        assert_eq!(packet.pkt_count, expected_count);
        assert!(packet.payload.len() <= MAX_VIDEO_PAYLOAD);
        assert!(!packet.payload.is_empty(), "no packet may be empty");

        reassembled.extend_from_slice(packet.payload);
    }

    reassembled
}

#[test]
fn a_slice_survives_the_round_trip_at_every_boundary() {
    let sizes = [
        1,
        2,
        MAX_VIDEO_PAYLOAD - 1,
        MAX_VIDEO_PAYLOAD,
        MAX_VIDEO_PAYLOAD + 1,
        MAX_VIDEO_PAYLOAD * 2 - 1,
        MAX_VIDEO_PAYLOAD * 2,
        MAX_VIDEO_PAYLOAD * 2 + 1,
        MAX_VIDEO_PAYLOAD * 7 + 391,
        200_000,
    ];

    for len in sizes {
        let data = bitstream(len);
        let reassembled = round_trip(42, 3, FLAG_IDR, 1_108_152_157_446, &data);
        assert_eq!(reassembled, data, "slice of {len} bytes did not survive");
    }
}

#[test]
fn packet_count_is_the_ceiling_of_the_division() {
    let cases = [
        (1usize, 1u16),
        (MAX_VIDEO_PAYLOAD, 1),
        (MAX_VIDEO_PAYLOAD + 1, 2),
        (MAX_VIDEO_PAYLOAD * 2, 2),
        (MAX_VIDEO_PAYLOAD * 2 + 1, 3),
    ];

    for (len, expected) in cases {
        let data = bitstream(len);
        let packetizer = SlicePacketizer::new(1, 0, 0, 0, &data).unwrap();
        assert_eq!(packetizer.packet_count(), expected, "{len} bytes");
        assert_eq!(packetizer.len(), usize::from(expected));
    }
}

#[test]
fn only_the_final_packet_is_short() {
    let data = bitstream(MAX_VIDEO_PAYLOAD * 3 + 17);
    let packets: Vec<_> = SlicePacketizer::new(1, 0, 0, 0, &data).unwrap().collect();

    let (last, full) = packets.split_last().unwrap();
    assert!(full.iter().all(|p| p.payload.len() == MAX_VIDEO_PAYLOAD));
    assert_eq!(last.payload.len(), 17);
}

#[test]
fn a_full_packet_fills_the_mtu_budget_exactly() {
    let data = bitstream(MAX_VIDEO_PAYLOAD);
    let packet = SlicePacketizer::new(1, 0, 0, 0, &data)
        .unwrap()
        .next()
        .unwrap();

    let mut buf = [0u8; MAX_PACKET_SIZE];
    assert_eq!(packet.encode_into(&mut buf).unwrap(), MAX_PACKET_SIZE);
}

#[test]
fn flags_reach_every_packet_of_the_slice() {
    let data = bitstream(MAX_VIDEO_PAYLOAD * 3);
    let flags = FLAG_IDR | FLAG_LAST_OF_FRAME;
    let packets: Vec<_> = SlicePacketizer::new(1, 0, flags, 0, &data)
        .unwrap()
        .collect();

    assert_eq!(packets.len(), 3);
    assert!(packets.iter().all(|p| p.flags == flags));
}

#[test]
fn an_empty_slice_is_rejected() {
    assert_eq!(
        SlicePacketizer::new(1, 0, 0, 0, &[]).unwrap_err(),
        PacketizeError::EmptySlice
    );
}

#[test]
fn a_slice_needing_more_than_u16_packets_is_rejected() {
    let data = vec![0u8; MAX_SLICE_LEN + 1];
    assert_eq!(
        SlicePacketizer::new(1, 0, 0, 0, &data).unwrap_err(),
        PacketizeError::SliceTooLarge {
            actual: MAX_SLICE_LEN + 1
        }
    );
}

#[test]
fn the_largest_representable_slice_is_accepted() {
    let data = vec![0u8; MAX_SLICE_LEN];
    let packetizer = SlicePacketizer::new(1, 0, 0, 0, &data).unwrap();
    assert_eq!(packetizer.packet_count(), u16::MAX);
}
