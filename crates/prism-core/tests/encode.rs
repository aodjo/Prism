//! Tests for video encoding.
//!
//! The platform-agnostic half checks the Annex B framing. The VideoToolbox half runs the
//! real hardware encoder, because the properties that make a session low latency are
//! silently ignored by a mock and the only way to know they were accepted is to ask the
//! encoder.

use prism_core::encode::{EncodedFrame, EncoderConfig};
use prism_core::net::negotiate::Codec;

#[test]
fn a_frame_byte_budget_is_one_frame_at_the_target_rate() {
    let config = EncoderConfig {
        codec: Codec::H264,
        width: 2560,
        height: 1440,
        fps: 120,
        bitrate_bps: 40_000_000,
        max_slice_bytes: 0,
    };

    assert_eq!(config.frame_byte_budget(), 40_000_000 / 8 / 120);
}

#[test]
fn a_zero_frame_rate_has_no_budget_rather_than_dividing_by_zero() {
    let config = EncoderConfig {
        codec: Codec::H264,
        width: 1920,
        height: 1080,
        fps: 0,
        bitrate_bps: 24_000_000,
        max_slice_bytes: 0,
    };

    assert_eq!(config.frame_byte_budget(), 0);
}

#[test]
fn nal_units_are_framed_with_start_codes_and_indexed_in_order() {
    let mut frame = EncodedFrame::default();
    frame.push_nal(&[0x67, 0x42, 0x00]);
    frame.push_nal(&[0x68, 0xce]);
    frame.push_nal(&[0x65, 0x88, 0x84, 0x00]);

    assert_eq!(frame.slices.len(), 3);
    assert_eq!(frame.slice(0).unwrap(), &[0, 0, 0, 1, 0x67, 0x42, 0x00]);
    assert_eq!(frame.slice(1).unwrap(), &[0, 0, 0, 1, 0x68, 0xce]);
    assert_eq!(
        frame.slice(2).unwrap(),
        &[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00]
    );
    assert!(frame.slice(3).is_none());

    let concatenated: Vec<u8> = (0..frame.slices.len())
        .flat_map(|i| frame.slice(i).unwrap().to_vec())
        .collect();
    assert_eq!(
        concatenated, frame.data,
        "the slices must reproduce the frame exactly"
    );
}

#[test]
fn resetting_a_frame_keeps_its_buffers() {
    let mut frame = EncodedFrame::default();
    frame.push_nal(&[0u8; 512]);
    let capacity = frame.data.capacity();

    frame.pts_us = 99;
    frame.is_idr = true;
    frame.reset();

    assert_eq!(frame.pts_us, 0);
    assert!(!frame.is_idr);
    assert!(frame.data.is_empty());
    assert!(frame.slices.is_empty());
    assert_eq!(
        frame.data.capacity(),
        capacity,
        "resetting must not release memory"
    );
}

#[cfg(target_os = "macos")]
mod videotoolbox {
    use std::time::Duration;

    use prism_core::encode::videotoolbox::{Nv12Frame, VideoToolboxEncoder};
    use prism_core::encode::{EncoderConfig, START_CODE};
    use prism_core::net::negotiate::Codec;

    /// A small session, kept modest so the test runs quickly on a shared CI machine.
    fn config() -> EncoderConfig {
        EncoderConfig {
            codec: Codec::H264,
            width: 640,
            height: 360,
            fps: 30,
            bitrate_bps: 2_000_000,
            max_slice_bytes: 4_000,
        }
    }

    /// Paints a moving pattern so successive frames actually differ.
    ///
    /// A static image encodes to almost nothing, which would make the test pass without
    /// the encoder ever producing a real slice.
    fn paint(frame: &mut Nv12Frame, phase: usize) {
        let width = frame.width() as usize;
        let height = frame.height() as usize;

        frame
            .fill(|luma, luma_stride, chroma, chroma_stride| {
                for y in 0..height {
                    for x in 0..width {
                        luma[y * luma_stride + x] = ((x + y + phase * 5) % 256) as u8;
                    }
                }
                for y in 0..height / 2 {
                    for x in 0..width {
                        chroma[y * chroma_stride + x] = ((x + phase) % 256) as u8;
                    }
                }
            })
            .expect("a freshly created buffer can be locked");
    }

    /// Returns the NAL unit type of a slice, which follows its start code.
    fn nal_type(slice: &[u8]) -> u8 {
        slice[START_CODE.len()] & 0x1f
    }

    #[test]
    fn the_first_frame_carries_parameter_sets_and_later_frames_do_not() {
        let mut encoder = VideoToolboxEncoder::new(config()).expect("session is creatable");
        let mut source = Nv12Frame::new(640, 360).expect("buffer is allocatable");

        paint(&mut source, 0);
        encoder
            .encode(source.pixel_buffer(), 0, true)
            .expect("first frame encodes");

        let first = encoder
            .poll(Duration::from_secs(5))
            .expect("first frame comes back");
        assert!(first.is_idr, "a forced keyframe must be reported as an IDR");

        let types: Vec<u8> = (0..first.slices.len())
            .map(|i| nal_type(first.slice(i).unwrap()))
            .collect();
        assert!(
            types.contains(&7),
            "an IDR frame must carry an SPS, got {types:?}"
        );
        assert!(
            types.contains(&8),
            "an IDR frame must carry a PPS, got {types:?}"
        );
        assert!(
            types.contains(&5),
            "an IDR frame must carry an IDR slice, got {types:?}"
        );

        for phase in 1..5 {
            paint(&mut source, phase);
            encoder
                .encode(source.pixel_buffer(), phase as u64 * 33_333, false)
                .expect("frame encodes");

            let frame = encoder
                .poll(Duration::from_secs(5))
                .expect("frame comes back");
            assert!(!frame.is_idr, "frame {phase} should not be an IDR");

            let types: Vec<u8> = (0..frame.slices.len())
                .map(|i| nal_type(frame.slice(i).unwrap()))
                .collect();
            assert!(
                !types.contains(&7) && !types.contains(&8),
                "a non-IDR frame must not repeat parameter sets, got {types:?}"
            );
        }
    }

    #[test]
    fn every_frame_is_the_concatenation_of_its_slices() {
        let mut encoder = VideoToolboxEncoder::new(config()).expect("session is creatable");
        let mut source = Nv12Frame::new(640, 360).expect("buffer is allocatable");

        for phase in 0..4 {
            paint(&mut source, phase);
            encoder
                .encode(source.pixel_buffer(), phase as u64 * 33_333, phase == 0)
                .expect("frame encodes");

            let frame = encoder
                .poll(Duration::from_secs(5))
                .expect("frame comes back");
            assert!(
                !frame.slices.is_empty(),
                "an encoded frame has at least one NAL unit"
            );

            let concatenated: Vec<u8> = (0..frame.slices.len())
                .flat_map(|i| frame.slice(i).unwrap().to_vec())
                .collect();
            assert_eq!(concatenated, frame.data);

            for i in 0..frame.slices.len() {
                let slice = frame.slice(i).unwrap();
                assert_eq!(
                    &slice[..START_CODE.len()],
                    &START_CODE,
                    "slice {i} is unframed"
                );
                assert!(slice.len() > START_CODE.len(), "slice {i} has no payload");
            }
        }
    }

    #[test]
    fn a_keyframe_can_be_forced_partway_through_a_stream() {
        let mut encoder = VideoToolboxEncoder::new(config()).expect("session is creatable");
        let mut source = Nv12Frame::new(640, 360).expect("buffer is allocatable");

        for phase in 0..3 {
            paint(&mut source, phase);
            encoder
                .encode(source.pixel_buffer(), phase as u64 * 33_333, phase == 0)
                .expect("frame encodes");
            encoder
                .poll(Duration::from_secs(5))
                .expect("frame comes back");
        }

        paint(&mut source, 3);
        encoder
            .encode(source.pixel_buffer(), 100_000, true)
            .expect("forced keyframe encodes");

        let frame = encoder
            .poll(Duration::from_secs(5))
            .expect("frame comes back");
        assert!(
            frame.is_idr,
            "the encoder must honour a forced keyframe request"
        );
    }
}
