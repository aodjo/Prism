//! Tests for what makes an encoded stream recoverable.
//!
//! A stream that only the first client to connect can decode is a stream with a bug that
//! looks like a black window and reports nothing. These check the two things that decide
//! whether a decoder can ever start, and the one knob congestion control needs to actuate.

#![cfg(target_os = "macos")]

use std::time::Duration;

use prism_core::encode::EncoderConfig;
use prism_core::encode::videotoolbox::{Nv12Frame, VideoToolboxEncoder};
use prism_core::net::negotiate::Codec;

/// Frame size for the probes. Small so the tests are quick; the properties do not depend
/// on resolution.
const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

/// Builds an encoder and a picture to feed it.
fn session(bitrate_bps: u32) -> (VideoToolboxEncoder, Nv12Frame) {
    let config = EncoderConfig {
        codec: Codec::H264,
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        bitrate_bps,
        max_slice_bytes: 0,
    };

    (
        VideoToolboxEncoder::new(config).expect("a compression session starts"),
        Nv12Frame::new(WIDTH, HEIGHT).expect("a buffer is allocatable"),
    )
}

/// Paints a frame that differs from its neighbours, so the encoder has real work to do.
fn paint(picture: &mut Nv12Frame, seed: u32) {
    picture
        .fill(|luma, luma_stride, chroma, chroma_stride| {
            for row in 0..HEIGHT as usize {
                for column in 0..WIDTH as usize {
                    luma[row * luma_stride + column] =
                        (seed as u8).wrapping_mul(3).wrapping_add(column as u8);
                }
            }
            for row in 0..HEIGHT as usize / 2 {
                for column in 0..WIDTH as usize / 2 {
                    chroma[row * chroma_stride + column * 2] = 128;
                    chroma[row * chroma_stride + column * 2 + 1] = 128;
                }
            }
        })
        .expect("a freshly created buffer can be locked");
}

/// Encodes `count` frames and returns, per frame, whether it carried parameter sets and
/// whether it was an IDR.
fn encode_run(count: u32) -> Vec<(u32, bool, bool)> {
    let (mut encoder, mut picture) = session(6_000_000);
    let mut seen = Vec::new();

    for index in 0..count {
        paint(&mut picture, index);
        encoder
            .encode(picture.pixel_buffer(), u64::from(index), index == 0)
            .expect("the frame is encodable");

        if let Some(frame) = encoder.poll(Duration::from_millis(500)) {
            let carries_parameter_sets = frame.slices.iter().any(|range| {
                frame
                    .data
                    .get(range.start + prism_core::encode::START_CODE.len())
                    .is_some_and(|&byte| byte & 0x1f == 7)
            });
            seen.push((index, carries_parameter_sets, frame.is_idr));
        }
    }

    seen
}

#[test]
fn parameter_sets_come_back_round_so_a_late_client_can_start() {
    // Without this the stream has exactly one moment where it can be understood. A client
    // that connects a second later, or loses the first frame, waits on a black window
    // forever with nothing anywhere reporting an error.
    let run = encode_run(150);
    let carrying: Vec<u32> = run
        .iter()
        .filter(|(_, parameter_sets, _)| *parameter_sets)
        .map(|(index, _, _)| *index)
        .collect();

    assert!(
        carrying.len() >= 3,
        "parameter sets should repeat across 150 frames, saw them at {carrying:?}"
    );

    let first_gap = carrying[1] - carrying[0];
    assert!(
        first_gap <= 60,
        "a client should wait at most a second to be able to start, waited {first_gap} frames"
    );
}

#[test]
fn repeating_the_parameter_sets_does_not_cost_a_keyframe() {
    // The cheap fix and the expensive one look the same from the outside. Forcing an IDR
    // every second would also let a client join, at the price of a bitrate spike every
    // second — which is exactly what the plan wants removed.
    let run = encode_run(150);
    let idrs: Vec<u32> = run
        .iter()
        .filter(|(_, _, is_idr)| *is_idr)
        .map(|(index, _, _)| *index)
        .collect();

    assert_eq!(
        idrs,
        vec![0],
        "only the frame that was asked to be an IDR should be one"
    );
}

#[test]
fn the_bitrate_can_be_changed_on_a_running_session() {
    // The actuator congestion control needs. Pacing alone slows the wire while the encoder
    // keeps producing the same bytes, which moves the queue into the host rather than
    // removing it.
    let (mut encoder, mut picture) = session(12_000_000);

    paint(&mut picture, 0);
    encoder
        .encode(picture.pixel_buffer(), 0, true)
        .expect("the first frame is encodable");

    encoder
        .set_bitrate_bps(4_000_000)
        .expect("the encoder accepts a lower rate mid-session");
    assert_eq!(encoder.config().bitrate_bps, 4_000_000);

    encoder
        .set_bitrate_bps(24_000_000)
        .expect("and a higher one");
    assert_eq!(encoder.config().bitrate_bps, 24_000_000);

    paint(&mut picture, 1);
    encoder
        .encode(picture.pixel_buffer(), 1, false)
        .expect("the session still works after the changes");
}

#[test]
fn long_term_references_are_treated_as_a_capability_and_not_assumed() {
    // This machine answers false: Apple Silicon's hardware H.264 encoder refuses EnableLTR,
    // the same way it refuses MaxH264SliceBytes. The test asserts only that asking is
    // survivable, because the answer is a property of the silicon and CI may run on other
    // hardware — what must never happen is a host that assumes the answer and breaks.
    let (encoder, _) = session(6_000_000);

    let supported = encoder.ltr_supported();
    println!("long-term references supported here: {supported}");

    assert_eq!(
        encoder.config().bitrate_bps,
        6_000_000,
        "probing for a property the encoder may refuse must leave the session usable"
    );
}
