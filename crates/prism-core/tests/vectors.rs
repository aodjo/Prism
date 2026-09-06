//! Conformance tests against the shared wire format vectors.
//!
//! `packages/protocol/vectors.json` is the single source of truth for the protocol.
//! The TypeScript suite in `packages/protocol/test/packet.test.ts` asserts against the
//! same file, so a layout change that is applied to only one implementation fails here.

use prism_core::net::packet::{
    Channel, FEEDBACK_PACKET_LEN, FORMAT_VERSION, FeedbackPacket, MAX_PACKET_SIZE,
    MAX_VIDEO_PAYLOAD, VIDEO_FLAGS_RESERVED_MASK, VIDEO_HEADER_LEN, VideoPacket, channel_of,
};
use serde_json::Value;

/// Loads and parses the shared vector fixtures.
///
/// The file is embedded at compile time so the test binary does not depend on the
/// working directory it is run from.
///
/// # Panics
///
/// Panics if the embedded JSON does not parse, which means the fixtures are corrupt.
fn vectors() -> Value {
    serde_json::from_str(include_str!("../../../packages/protocol/vectors.json"))
        .expect("vectors.json must parse")
}

/// Converts a lowercase hex string into the bytes it represents.
///
/// # Panics
///
/// Panics if `hex` has odd length or contains a non-hex digit.
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("valid hex"))
        .collect()
}

/// Renders bytes as a lowercase hex string for comparison against the fixtures.
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads a `u64` field that the fixtures carry as a string to survive JSON's number range.
///
/// # Panics
///
/// Panics if the field is missing or does not parse as a `u64`.
fn u64_field(value: &Value, key: &str) -> u64 {
    value[key]
        .as_str()
        .expect("u64 fields are encoded as strings")
        .parse()
        .expect("valid u64")
}

#[test]
fn constants_match_the_shared_vectors() {
    let v = vectors();

    assert_eq!(
        u64::from(FORMAT_VERSION),
        v["formatVersion"].as_u64().unwrap()
    );
    assert_eq!(
        MAX_PACKET_SIZE as u64,
        v["constants"]["maxPacketSize"].as_u64().unwrap()
    );
    assert_eq!(
        VIDEO_HEADER_LEN as u64,
        v["constants"]["videoHeaderLen"].as_u64().unwrap()
    );
    assert_eq!(
        MAX_VIDEO_PAYLOAD as u64,
        v["constants"]["maxVideoPayload"].as_u64().unwrap()
    );
    assert_eq!(
        FEEDBACK_PACKET_LEN as u64,
        v["constants"]["feedbackPacketLen"].as_u64().unwrap()
    );
    assert_eq!(
        u64::from(VIDEO_FLAGS_RESERVED_MASK),
        v["videoFlags"]["reservedMask"].as_u64().unwrap()
    );

    assert_eq!(
        Channel::Control as u64,
        v["channels"]["control"].as_u64().unwrap()
    );
    assert_eq!(
        Channel::Video as u64,
        v["channels"]["video"].as_u64().unwrap()
    );
    assert_eq!(
        Channel::Audio as u64,
        v["channels"]["audio"].as_u64().unwrap()
    );
    assert_eq!(
        Channel::Input as u64,
        v["channels"]["input"].as_u64().unwrap()
    );
    assert_eq!(
        Channel::Feedback as u64,
        v["channels"]["feedback"].as_u64().unwrap()
    );
}

#[test]
fn video_packets_round_trip_through_the_vectors() {
    let v = vectors();

    for vector in v["videoPackets"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let fields = &vector["fields"];
        let expected_hex = vector["hex"].as_str().unwrap();
        let payload = hex_to_bytes(fields["payloadHex"].as_str().unwrap());

        let packet = VideoPacket {
            frame_id: fields["frameId"].as_u64().unwrap() as u32,
            slice_id: fields["sliceId"].as_u64().unwrap() as u16,
            pkt_idx: fields["pktIdx"].as_u64().unwrap() as u16,
            pkt_count: fields["pktCount"].as_u64().unwrap() as u16,
            flags: fields["flags"].as_u64().unwrap() as u8,
            capture_ts_us: u64_field(fields, "captureTsUs"),
            payload: &payload,
        };

        let mut buf = [0u8; MAX_PACKET_SIZE];
        let written = packet.encode_into(&mut buf).unwrap();
        assert_eq!(bytes_to_hex(&buf[..written]), expected_hex, "encode {name}");

        let encoded = hex_to_bytes(expected_hex);
        let decoded = VideoPacket::decode(&encoded).unwrap();
        assert_eq!(decoded.frame_id, packet.frame_id, "decode {name} frame_id");
        assert_eq!(decoded.slice_id, packet.slice_id, "decode {name} slice_id");
        assert_eq!(decoded.pkt_idx, packet.pkt_idx, "decode {name} pkt_idx");
        assert_eq!(
            decoded.pkt_count, packet.pkt_count,
            "decode {name} pkt_count"
        );
        assert_eq!(decoded.flags, packet.flags, "decode {name} flags");
        assert_eq!(
            decoded.capture_ts_us, packet.capture_ts_us,
            "decode {name} capture_ts_us"
        );
        assert_eq!(decoded.payload, packet.payload, "decode {name} payload");
    }
}

#[test]
fn feedback_packets_round_trip_through_the_vectors() {
    let v = vectors();

    for vector in v["feedbackPackets"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let fields = &vector["fields"];
        let expected_hex = vector["hex"].as_str().unwrap();

        let report = FeedbackPacket {
            last_frame_id: fields["lastFrameId"].as_u64().unwrap() as u32,
            recv_bitmap: fields["recvBitmap"].as_u64().unwrap() as u32,
            client_ts_us: u64_field(fields, "clientTsUs"),
        };

        let mut buf = [0u8; FEEDBACK_PACKET_LEN];
        let written = report.encode_into(&mut buf).unwrap();
        assert_eq!(written, FEEDBACK_PACKET_LEN);
        assert_eq!(bytes_to_hex(&buf[..written]), expected_hex, "encode {name}");

        let decoded = FeedbackPacket::decode(&hex_to_bytes(expected_hex)).unwrap();
        assert_eq!(decoded, report, "decode {name}");
    }
}

#[test]
fn malformed_packets_are_rejected() {
    let v = vectors();

    for vector in v["rejects"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let reason = vector["reason"].as_str().unwrap();
        let bytes = hex_to_bytes(vector["hex"].as_str().unwrap());

        let rejected = match channel_of(&bytes) {
            Err(_) => true,
            Ok(Channel::Video) => VideoPacket::decode(&bytes).is_err(),
            Ok(Channel::Feedback) => FeedbackPacket::decode(&bytes).is_err(),
            Ok(_) => false,
        };

        assert!(rejected, "{name} should have been rejected: {reason}");
    }
}

#[test]
fn a_full_size_payload_exactly_fills_a_packet() {
    let payload = [0u8; MAX_VIDEO_PAYLOAD];
    let packet = VideoPacket {
        frame_id: 1,
        slice_id: 0,
        pkt_idx: 0,
        pkt_count: 1,
        flags: 0,
        capture_ts_us: 0,
        payload: &payload,
    };

    let mut buf = [0u8; MAX_PACKET_SIZE];
    assert_eq!(packet.encode_into(&mut buf).unwrap(), MAX_PACKET_SIZE);
}

#[test]
fn an_oversized_payload_is_rejected() {
    let payload = [0u8; MAX_VIDEO_PAYLOAD + 1];
    let packet = VideoPacket {
        frame_id: 1,
        slice_id: 0,
        pkt_idx: 0,
        pkt_count: 1,
        flags: 0,
        capture_ts_us: 0,
        payload: &payload,
    };

    let mut buf = [0u8; MAX_PACKET_SIZE + 1];
    assert!(packet.encode_into(&mut buf).is_err());
}
