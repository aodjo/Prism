//! Conformance tests against the shared wire format vectors.
//!
//! `packages/protocol/vectors.json` is the single source of truth for the protocol.
//! The TypeScript suite in `packages/protocol/test/packet.test.ts` asserts against the
//! same file, so a layout change that is applied to only one implementation fails here.

use prism_core::net::packet::{
    CLOCK_PING_LEN, CLOCK_PONG_LEN, CONTROL_HEADER_LEN, CURSOR_POSITION_LEN, Channel, ClockPing,
    ClockPong, ControlType, CursorPosition, FEC_HEADER_LEN, FEEDBACK_PACKET_LEN, FORMAT_VERSION,
    FecPacket, FeedbackPacket, INPUT_PACKET_LEN, InputEvent, InputKind, InputPacket,
    MAX_PACKET_SIZE, MAX_VIDEO_PAYLOAD, MouseButton, VIDEO_FLAGS_RESERVED_MASK, VIDEO_HEADER_LEN,
    VideoPacket, channel_of, control_type_of,
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

    assert_eq!(
        CONTROL_HEADER_LEN as u64,
        v["constants"]["controlHeaderLen"].as_u64().unwrap()
    );
    assert_eq!(
        CLOCK_PING_LEN as u64,
        v["constants"]["clockPingLen"].as_u64().unwrap()
    );
    assert_eq!(
        CLOCK_PONG_LEN as u64,
        v["constants"]["clockPongLen"].as_u64().unwrap()
    );
    assert_eq!(
        ControlType::ClockPing as u64,
        v["controlTypes"]["clockPing"].as_u64().unwrap()
    );
    assert_eq!(
        ControlType::ClockPong as u64,
        v["controlTypes"]["clockPong"].as_u64().unwrap()
    );
}

#[test]
fn input_packets_round_trip_through_the_vectors() {
    let v = vectors();

    assert_eq!(
        INPUT_PACKET_LEN as u64,
        v["constants"]["inputPacketLen"].as_u64().unwrap()
    );
    assert_eq!(
        InputKind::MouseMove as u64,
        v["inputKinds"]["mouseMove"].as_u64().unwrap()
    );
    assert_eq!(
        InputKind::MouseButton as u64,
        v["inputKinds"]["mouseButton"].as_u64().unwrap()
    );
    assert_eq!(
        InputKind::MouseScroll as u64,
        v["inputKinds"]["mouseScroll"].as_u64().unwrap()
    );
    assert_eq!(
        InputKind::Key as u64,
        v["inputKinds"]["key"].as_u64().unwrap()
    );
    assert_eq!(
        MouseButton::Left as u64,
        v["mouseButtons"]["left"].as_u64().unwrap()
    );

    for vector in v["inputPackets"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let expected_hex = vector["hex"].as_str().unwrap();
        let fields = &vector["fields"];

        let x = fields["x"].as_i64().unwrap() as i16;
        let pressed = fields["flags"].as_u64().unwrap() & 1 != 0;
        let event = match fields["kind"].as_u64().unwrap() {
            0 => InputEvent::MouseMove {
                dx: x,
                dy: fields["y"].as_i64().unwrap() as i16,
            },
            1 => InputEvent::MouseButton {
                button: MouseButton::try_from(x).unwrap(),
                pressed,
            },
            2 => InputEvent::MouseScroll {
                dx: x,
                dy: fields["y"].as_i64().unwrap() as i16,
            },
            _ => InputEvent::Key {
                usage: x as u16,
                pressed,
            },
        };

        let packet = InputPacket {
            origin_ts_us: u64_field(fields, "originTsUs"),
            event,
        };

        let mut buf = [0u8; INPUT_PACKET_LEN];
        let written = packet.encode_into(&mut buf).unwrap();
        assert_eq!(written, INPUT_PACKET_LEN);
        assert_eq!(bytes_to_hex(&buf[..written]), expected_hex, "encode {name}");
        assert_eq!(
            InputPacket::decode(&hex_to_bytes(expected_hex)).unwrap(),
            packet,
            "decode {name}"
        );
    }
}

#[test]
fn clock_pings_round_trip_through_the_vectors() {
    let v = vectors();

    for vector in v["clockPings"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let expected_hex = vector["hex"].as_str().unwrap();
        let ping = ClockPing {
            t1_us: u64_field(&vector["fields"], "t1Us"),
        };

        let mut buf = [0u8; CLOCK_PING_LEN];
        let written = ping.encode_into(&mut buf).unwrap();
        assert_eq!(written, CLOCK_PING_LEN);
        assert_eq!(bytes_to_hex(&buf[..written]), expected_hex, "encode {name}");
        assert_eq!(
            ClockPing::decode(&hex_to_bytes(expected_hex)).unwrap(),
            ping,
            "decode {name}"
        );
    }
}

#[test]
fn clock_pongs_round_trip_through_the_vectors() {
    let v = vectors();

    for vector in v["clockPongs"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let expected_hex = vector["hex"].as_str().unwrap();
        let fields = &vector["fields"];
        let pong = ClockPong {
            t1_us: u64_field(fields, "t1Us"),
            t2_us: u64_field(fields, "t2Us"),
            t3_us: u64_field(fields, "t3Us"),
        };

        let mut buf = [0u8; CLOCK_PONG_LEN];
        let written = pong.encode_into(&mut buf).unwrap();
        assert_eq!(written, CLOCK_PONG_LEN);
        assert_eq!(bytes_to_hex(&buf[..written]), expected_hex, "encode {name}");
        assert_eq!(
            ClockPong::decode(&hex_to_bytes(expected_hex)).unwrap(),
            pong,
            "decode {name}"
        );
    }
}

#[test]
fn fec_packets_round_trip_through_the_vectors() {
    let v = vectors();

    for vector in v["fecPackets"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let expected_hex = vector["hex"].as_str().unwrap();
        let fields = &vector["fields"];
        let payload = hex_to_bytes(vector["payloadHex"].as_str().unwrap());

        let packet = FecPacket {
            frame_id: fields["frameId"].as_u64().unwrap() as u32,
            slice_id: fields["sliceId"].as_u64().unwrap() as u16,
            data_count: fields["dataCount"].as_u64().unwrap() as u8,
            parity_count: fields["parityCount"].as_u64().unwrap() as u8,
            shard_index: fields["shardIndex"].as_u64().unwrap() as u8,
            tail_len: fields["tailLen"].as_u64().unwrap() as u16,
            capture_ts_us: u64_field(fields, "captureTsUs"),
            payload: &payload,
        };

        let mut buf = [0u8; MAX_PACKET_SIZE];
        let written = packet.encode_into(&mut buf).unwrap();
        assert_eq!(written, FEC_HEADER_LEN + payload.len());
        assert_eq!(bytes_to_hex(&buf[..written]), expected_hex, "encode {name}");

        let bytes = hex_to_bytes(expected_hex);
        assert_eq!(FecPacket::decode(&bytes).unwrap(), packet, "decode {name}");
    }
}

#[test]
fn cursor_positions_round_trip_through_the_vectors() {
    let v = vectors();

    for vector in v["cursorPositions"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let expected_hex = vector["hex"].as_str().unwrap();
        let fields = &vector["fields"];
        let cursor = CursorPosition {
            sample_ts_us: u64_field(fields, "sampleTsUs"),
            x: fields["x"].as_u64().unwrap() as u16,
            y: fields["y"].as_u64().unwrap() as u16,
            screen_width: fields["screenWidth"].as_u64().unwrap() as u16,
            screen_height: fields["screenHeight"].as_u64().unwrap() as u16,
        };

        let mut buf = [0u8; CURSOR_POSITION_LEN];
        let written = cursor.encode_into(&mut buf).unwrap();
        assert_eq!(written, CURSOR_POSITION_LEN);
        assert_eq!(bytes_to_hex(&buf[..written]), expected_hex, "encode {name}");
        assert_eq!(
            CursorPosition::decode(&hex_to_bytes(expected_hex)).unwrap(),
            cursor,
            "decode {name}"
        );
    }
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
            Ok(Channel::Control) => match control_type_of(&bytes) {
                Err(_) => true,
                Ok(ControlType::ClockPing) => ClockPing::decode(&bytes).is_err(),
                Ok(ControlType::ClockPong) => ClockPong::decode(&bytes).is_err(),
                Ok(ControlType::CursorPosition) => CursorPosition::decode(&bytes).is_err(),
            },
            Ok(Channel::Input) => InputPacket::decode(&bytes).is_err(),
            Ok(Channel::Fec) => FecPacket::decode(&bytes).is_err(),
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
