//! Tests for video decoding.
//!
//! The Annex B parser is checked directly. The VideoToolbox half runs a real round trip:
//! a painted frame goes through the hardware encoder, the wire format, and the hardware
//! decoder, and the picture that comes out is compared against the picture that went in.
//! Nothing short of that proves the two sessions agree on the bitstream.

use prism_core::decode::{NAL_IDR, NAL_PPS, NAL_SPS, nal_type, nal_units};

#[test]
fn nal_units_are_split_on_either_start_code_length() {
    let stream = [
        0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
    ];
    let nals: Vec<&[u8]> = nal_units(&stream).collect();

    assert_eq!(nals.len(), 3);
    assert_eq!(nals[0], &[0x67, 0x42]);
    assert_eq!(nals[1], &[0x68, 0xce]);
    assert_eq!(nals[2], &[0x65, 0x88]);
}

#[test]
fn nal_types_are_read_from_the_header_byte() {
    assert_eq!(nal_type(&[0x67]), Some(NAL_SPS));
    assert_eq!(nal_type(&[0x68]), Some(NAL_PPS));
    assert_eq!(nal_type(&[0x65]), Some(NAL_IDR));
    assert_eq!(nal_type(&[]), None);
}

#[test]
fn a_stream_without_start_codes_yields_nothing() {
    assert_eq!(nal_units(&[0x67, 0x42, 0x00]).count(), 0);
    assert_eq!(nal_units(&[]).count(), 0);
}

#[test]
fn trailing_start_codes_do_not_produce_empty_units() {
    let stream = [0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1];
    let nals: Vec<&[u8]> = nal_units(&stream).collect();

    assert_eq!(nals.len(), 1);
    assert_eq!(nals[0], &[0x65, 0x88]);
}

#[cfg(target_os = "macos")]
mod round_trip {
    use std::time::Duration;

    use prism_core::decode::DecodeError;
    use prism_core::decode::videotoolbox::VideoToolboxDecoder;
    use prism_core::encode::EncoderConfig;
    use prism_core::encode::videotoolbox::{Nv12Frame, VideoToolboxEncoder};

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;

    /// Returns a modest encoder configuration that runs quickly on a shared machine.
    fn config() -> EncoderConfig {
        EncoderConfig {
            width: WIDTH,
            height: HEIGHT,
            fps: 30,
            bitrate_bps: 8_000_000,
            max_slice_bytes: 0,
        }
    }

    /// Returns the luma value this test paints at a given position and phase.
    ///
    /// A smooth gradient rather than sharp edges, so the comparison is not dominated by
    /// ringing around hard transitions that any lossy codec will produce.
    fn luma_at(x: usize, y: usize, phase: usize) -> u8 {
        (((x + y * 2 + phase * 9) / 3) % 200 + 28) as u8
    }

    /// Paints the reference pattern into a frame.
    fn paint(frame: &mut Nv12Frame, phase: usize) {
        frame
            .fill(|luma, luma_stride, chroma, chroma_stride| {
                for y in 0..HEIGHT as usize {
                    for x in 0..WIDTH as usize {
                        luma[y * luma_stride + x] = luma_at(x, y, phase);
                    }
                }
                for y in 0..HEIGHT as usize / 2 {
                    for x in 0..WIDTH as usize {
                        chroma[y * chroma_stride + x] = 128;
                    }
                }
            })
            .expect("a freshly created buffer can be locked");
    }

    #[test]
    fn a_decoder_refuses_to_start_before_it_has_parameter_sets() {
        let mut decoder = VideoToolboxDecoder::new();

        assert!(!decoder.is_ready());
        assert_eq!(
            decoder.decode(&[0, 0, 0, 1, 0x41, 0x9a], 0).unwrap_err(),
            DecodeError::NoParameterSets
        );
    }

    #[test]
    fn a_painted_frame_survives_the_encoder_and_the_decoder() {
        let mut encoder = VideoToolboxEncoder::new(config()).expect("session is creatable");
        let mut decoder = VideoToolboxDecoder::new();
        let mut source = Nv12Frame::new(WIDTH, HEIGHT).expect("buffer is allocatable");
        let mut luma = Vec::new();

        let mut compared = 0;

        for phase in 0..6usize {
            paint(&mut source, phase);
            encoder
                .encode(&source, phase as u64 * 33_333, phase == 0)
                .expect("frame encodes");

            let encoded = encoder
                .poll(Duration::from_secs(5))
                .expect("frame comes back");
            let pts_us = encoded.pts_us;
            let bitstream = encoded.data.clone();

            decoder.decode(&bitstream, pts_us).expect("frame decodes");
            assert!(
                decoder.is_ready(),
                "parameter sets arrived with the first frame"
            );

            let Some(picture) = decoder.poll(Duration::from_secs(5)) else {
                continue;
            };

            assert_eq!(picture.width, WIDTH);
            assert_eq!(picture.height, HEIGHT);
            assert_eq!(
                picture.pts_us, pts_us,
                "timestamps must survive the round trip"
            );

            picture
                .copy_luma(&mut luma)
                .expect("the picture can be read back");
            assert_eq!(luma.len(), (WIDTH * HEIGHT) as usize);

            let error = mean_absolute_error(&luma, phase);
            assert!(
                error < 12.0,
                "phase {phase} decoded to a different picture, mean luma error {error:.1}"
            );

            compared += 1;
        }

        assert!(compared >= 4, "only {compared} frames came back out of 6");
        assert!(
            decoder.take_errors().is_empty(),
            "the decoder reported failures"
        );
    }

    /// Returns the mean absolute luma difference between a decoded picture and the
    /// pattern that was painted for that phase.
    ///
    /// H.264 is lossy, so an exact match is not expected; what matters is that the
    /// picture is recognisably the one that went in rather than noise or a stale frame.
    fn mean_absolute_error(decoded: &[u8], phase: usize) -> f64 {
        let total: u64 = (0..HEIGHT as usize)
            .flat_map(|y| (0..WIDTH as usize).map(move |x| (x, y)))
            .map(|(x, y)| {
                let expected = i32::from(luma_at(x, y, phase));
                let actual = i32::from(decoded[y * WIDTH as usize + x]);
                expected.abs_diff(actual) as u64
            })
            .sum();

        total as f64 / f64::from(WIDTH * HEIGHT)
    }
}
