//! Tests for the audio path.
//!
//! Two things are being guarded, and they fail in different ways. The codec has to turn sound
//! into bytes and back into recognisable sound — a codec that runs without error while
//! producing silence would pass any test that only checks for errors. And the jitter buffer has
//! to hand playback a frame every five milliseconds no matter what order the network delivers
//! in, because the one thing it must never do is run dry.

use prism_core::audio::codec::{AudioDecoder, AudioEncoder, CodecError};
use prism_core::audio::jitter::{JitterBuffer, Pull};
use prism_core::audio::{CHANNELS, FRAME_INTERLEAVED, FRAME_US, SAMPLE_RATE};
use prism_core::net::packet::{AUDIO_HEADER_LEN, AudioPacket, MAX_AUDIO_PAYLOAD, ProtocolError};

/// Builds one frame of a tone, interleaved across both channels.
///
/// A tone rather than noise: noise is the worst case for any transform codec and says nothing
/// about how a desktop's audio will fare.
fn tone(frame: usize, hz: f64, amplitude: f32) -> Vec<f32> {
    (0..FRAME_INTERLEAVED)
        .map(|i| {
            let sample = frame * (FRAME_INTERLEAVED / CHANNELS) + i / CHANNELS;
            let t = sample as f64 / f64::from(SAMPLE_RATE);

            (t * hz * std::f64::consts::TAU).sin() as f32 * amplitude
        })
        .collect()
}

/// Returns the mean square of a frame, which is what says whether anything is there at all.
fn energy(samples: &[f32]) -> f64 {
    samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len() as f64
}

#[test]
fn sound_survives_a_round_trip() {
    // The check a codec integration actually needs. One that ran without error while emitting
    // silence would pass every test that only looks for errors, and this is the one it fails.
    let mut encoder = AudioEncoder::new(128_000).expect("an encoder");
    let mut decoder = AudioDecoder::new().expect("a decoder");

    let mut heard = 0f64;
    let mut sent = 0f64;

    for frame in 0..200 {
        let samples = tone(frame, 440.0, 0.5);
        let packet = encoder.encode(&samples).expect("encodes");
        let decoded = decoder.decode(packet).expect("decodes");

        // The codec has a couple of milliseconds of delay, so the first frames out are the
        // silence it emits before the input has worked through.
        if frame > 10 {
            sent += energy(&samples);
            heard += energy(decoded);
        }
    }

    let ratio = heard / sent;
    assert!(
        (0.5..2.0).contains(&ratio),
        "decoded audio carries {ratio:.3} of the energy that went in, so it is not the same \
         sound"
    );
}

#[test]
fn a_frame_is_small_enough_that_audio_never_fragments() {
    // The property the whole audio design rests on: one frame is one packet. If a frame ever
    // needed two, audio would need reassembly, and a lost packet would cost a stall rather than
    // a concealed gap.
    let mut encoder = AudioEncoder::new(256_000).expect("an encoder");

    let mut largest = 0;
    for frame in 0..100 {
        // Three tones at full scale: far more to encode than a desktop usually produces.
        let samples: Vec<f32> = tone(frame, 440.0, 0.4)
            .iter()
            .zip(tone(frame, 3_000.0, 0.3))
            .zip(tone(frame, 97.0, 0.3))
            .map(|((a, b), c)| a + b + c)
            .collect();

        largest = largest.max(encoder.encode(&samples).expect("encodes").len());
    }

    assert!(
        largest < MAX_AUDIO_PAYLOAD,
        "a frame reached {largest} bytes against a {MAX_AUDIO_PAYLOAD} byte budget"
    );
    println!("largest frame at 256 kbps: {largest} bytes");
}

#[test]
fn a_frame_of_the_wrong_length_is_refused() {
    let mut encoder = AudioEncoder::new(128_000).expect("an encoder");

    for length in [0, 1, FRAME_INTERLEAVED - 1, FRAME_INTERLEAVED + 1] {
        assert_eq!(
            encoder.encode(&vec![0.0; length]).map(<[u8]>::len),
            Err(CodecError::WrongFrameSize { actual: length })
        );
    }
}

#[test]
fn a_concealed_frame_is_sound_rather_than_silence() {
    // Concealment is the whole reason audio needs no parity. A decoder that returned silence
    // for a missing frame would produce exactly the click that concealment exists to avoid.
    let mut encoder = AudioEncoder::new(128_000).expect("an encoder");
    let mut decoder = AudioDecoder::new().expect("a decoder");

    for frame in 0..40 {
        let packet = encoder.encode(&tone(frame, 440.0, 0.5)).expect("encodes");
        decoder.decode(packet).expect("decodes");
    }

    let invented = decoder.conceal().expect("conceals");

    assert!(
        energy(invented) > 1e-6,
        "the concealed frame is silence, which is the click it exists to avoid"
    );
    assert_eq!(decoder.concealed(), 1);
}

#[test]
fn frames_come_out_in_order_however_they_arrive() {
    // UDP reorders. A buffer that handed playback whatever arrived last would play a session
    // back shuffled, which is worse than any amount of delay.
    let mut buffer = JitterBuffer::new();

    for (index, sequence) in [3u32, 0, 4, 1, 2].into_iter().enumerate() {
        buffer.push(sequence, &[sequence as u8], index as u64 * 5_000);
    }

    let mut order = Vec::new();
    while let Pull::Frame { sequence, .. } = buffer.pull() {
        order.push(sequence);
    }

    assert_eq!(order, vec![0, 1, 2, 3, 4]);
}

#[test]
fn nothing_plays_until_there_is_enough_held_to_keep_playing() {
    // Starting on the first packet would mean the output runs dry on the first uneven arrival,
    // and running dry is the click the buffer exists to prevent.
    let mut buffer = JitterBuffer::new();

    buffer.push(0, &[0], 0);
    assert_eq!(buffer.pull(), Pull::Empty, "playback started on one frame");

    buffer.push(1, &[1], 5_000);
    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 0, .. }));
}

#[test]
fn a_missing_frame_is_reported_once_something_later_arrives() {
    // The judgement the buffer has to make: a gap is either a packet still in flight or one
    // that is gone. A later frame in hand settles it, because the path does not reorder by
    // more than a frame or two and waiting longer turns a concealed gap into a stall.
    let mut buffer = JitterBuffer::new();

    buffer.push(0, &[0], 0);
    buffer.push(1, &[1], 5_000);
    buffer.push(3, &[3], 15_000);

    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 0, .. }));
    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 1, .. }));
    assert_eq!(buffer.pull(), Pull::Missing { sequence: 2 });
    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 3, .. }));
    assert_eq!(buffer.lost(), 1);
}

#[test]
fn a_gap_with_nothing_behind_it_waits_rather_than_concealing() {
    // The other half of that judgement. Concealing a frame that is merely late would mean
    // playing an invented frame and then throwing away the real one.
    let mut buffer = JitterBuffer::new();

    buffer.push(0, &[0], 0);
    buffer.push(1, &[1], 5_000);

    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 0, .. }));
    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 1, .. }));
    assert_eq!(buffer.pull(), Pull::Empty, "a late frame was given up on");
    assert_eq!(buffer.lost(), 0);

    buffer.push(2, &[2], 12_000);
    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 2, .. }));
}

#[test]
fn a_frame_that_arrives_after_its_moment_is_dropped_rather_than_played_late() {
    // Playing it would mean going backwards, which is a worse artefact than the gap that was
    // already concealed in its place.
    let mut buffer = JitterBuffer::new();

    for sequence in 0..4u32 {
        buffer.push(sequence, &[sequence as u8], u64::from(sequence) * 5_000);
    }
    for _ in 0..4 {
        buffer.pull();
    }

    buffer.push(1, &[1], 30_000);

    assert_eq!(buffer.late(), 1);
    assert_eq!(buffer.pull(), Pull::Empty);
}

#[test]
fn a_duplicate_is_counted_and_played_once() {
    let mut buffer = JitterBuffer::new();

    buffer.push(0, &[0], 0);
    buffer.push(0, &[0], 1_000);
    buffer.push(1, &[1], 5_000);

    assert_eq!(buffer.duplicated(), 1);

    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 0, .. }));
    assert!(matches!(buffer.pull(), Pull::Frame { sequence: 1, .. }));
    assert_eq!(buffer.pull(), Pull::Empty);
}

#[test]
fn a_steady_path_pays_the_smallest_delay_the_buffer_has() {
    // Delay is pure cost. A path that delivers evenly should be charged the minimum, and a
    // buffer that always sat deep would add tens of milliseconds to every session for the
    // benefit of the worst one.
    let mut buffer = JitterBuffer::new();

    for sequence in 0..50u32 {
        buffer.push(sequence, &[0], u64::from(sequence) * u64::from(FRAME_US));
        buffer.pull();
    }

    assert_eq!(buffer.depth(), 2, "an even path was charged for jitter");
}

#[test]
fn an_uneven_path_is_held_deeper() {
    // And the other direction: a path that stalls for thirty milliseconds has to be covered,
    // or the output runs dry every time it does.
    let mut buffer = JitterBuffer::new();

    let mut clock = 0u64;
    for sequence in 0..50u32 {
        // Every fifth frame arrives thirty milliseconds late, which is what a path with a
        // busy router looks like.
        clock += if sequence % 5 == 0 { 35_000 } else { 5_000 };
        buffer.push(sequence, &[0], clock);
        buffer.pull();
    }

    assert!(
        buffer.depth() > 2,
        "a stalling path was charged the minimum, so playback will run dry"
    );
    println!("depth on a stalling path: {} frames", buffer.depth());
}

#[test]
fn the_depth_comes_back_down_when_the_path_settles() {
    // A single stall must not hold the buffer deep for the rest of a session. Delay that was
    // paid for one bad moment and never given back is delay a person feels for hours.
    let mut buffer = JitterBuffer::new();

    let mut clock = 0u64;
    for sequence in 0..20u32 {
        clock += if sequence == 5 { 60_000 } else { 5_000 };
        buffer.push(sequence, &[0], clock);
        buffer.pull();
    }
    let after_stall = buffer.depth();

    for sequence in 20..600u32 {
        clock += u64::from(FRAME_US);
        buffer.push(sequence, &[0], clock);
        buffer.pull();
    }

    assert!(
        buffer.depth() < after_stall,
        "the buffer stayed at {after_stall} frames after the path settled"
    );
    println!(
        "depth after the stall: {after_stall}, once settled: {}",
        buffer.depth()
    );
}

#[test]
fn a_stream_that_restarts_does_not_play_half_a_second_of_stale_sound() {
    let mut buffer = JitterBuffer::new();

    for sequence in 0..200u32 {
        buffer.push(sequence, &[0], u64::from(sequence) * 5_000);
    }

    assert!(
        buffer.held() < 200,
        "the buffer held {} frames, which is a second of stale audio",
        buffer.held()
    );
}

#[test]
fn an_audio_packet_survives_a_round_trip() {
    let payload = [0x9au8; 83];
    let packet = AudioPacket {
        sequence: 0xdead_beef,
        capture_ts_us: 0x0123_4567_89ab_cdef,
        payload: &payload,
    };

    let mut buf = [0u8; 1200];
    let len = packet.encode_into(&mut buf).expect("encodes");

    assert_eq!(len, AUDIO_HEADER_LEN + payload.len());
    assert_eq!(AudioPacket::decode(&buf[..len]).expect("decodes"), packet);
}

#[test]
fn an_audio_packet_over_the_budget_is_refused() {
    let payload = vec![0u8; MAX_AUDIO_PAYLOAD + 1];
    let packet = AudioPacket {
        sequence: 0,
        capture_ts_us: 0,
        payload: &payload,
    };

    assert_eq!(
        packet.encode_into(&mut [0u8; 1400]),
        Err(ProtocolError::PayloadTooLarge {
            actual: MAX_AUDIO_PAYLOAD + 1
        })
    );
}

#[test]
fn a_packet_from_another_channel_is_refused() {
    let mut buf = [0u8; 32];
    buf[0] = 1;

    assert!(AudioPacket::decode(&buf).is_err());
}

#[test]
fn a_truncated_audio_header_is_refused() {
    let mut buf = [0u8; AUDIO_HEADER_LEN];
    buf[0] = 2;

    for length in 1..AUDIO_HEADER_LEN {
        assert!(
            AudioPacket::decode(&buf[..length]).is_err(),
            "a {length} byte header was accepted"
        );
    }

    assert!(
        AudioPacket::decode(&buf).is_ok(),
        "an empty frame is a frame"
    );
}
