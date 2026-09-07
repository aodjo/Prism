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
    use prism_core::net::negotiate::Codec;

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;

    /// Returns a modest encoder configuration that runs quickly on a shared machine.
    fn config() -> EncoderConfig {
        EncoderConfig {
            codec: Codec::H264,
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
    pub fn paint(frame: &mut Nv12Frame, phase: usize) {
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
        let mut decoder = VideoToolboxDecoder::new(Codec::H264);

        assert!(!decoder.is_ready());
        assert_eq!(
            decoder.decode(&[0, 0, 0, 1, 0x41, 0x9a], 0).unwrap_err(),
            DecodeError::NoParameterSets
        );
    }

    #[test]
    fn a_painted_frame_survives_the_encoder_and_the_decoder() {
        round_trip(Codec::H264);
    }

    #[test]
    fn a_painted_frame_survives_hevc_too() {
        // The reason the codec is negotiated at all. HEVC numbers its NAL units differently
        // and carries a third parameter set, so a path that quietly assumed H.264 produces a
        // black window with nothing reporting an error — which is exactly what the first
        // HEVC stream out of this encoder did, until an independent decoder refused it.
        round_trip(Codec::Hevc);
    }

    /// How many frames a round trip encodes.
    ///
    /// More than the sixty at which the encoder repeats its parameter sets, because a decoder
    /// that only works on the frames carrying them looks perfect over a handful and fails on
    /// every real session. That is exactly what the first HEVC stream did.
    const FRAMES: usize = 90;

    /// Encodes painted frames and checks each one comes back recognisable.
    fn round_trip(codec: Codec) {
        let mut settings = config();
        settings.codec = codec;

        let mut encoder = VideoToolboxEncoder::new(settings).expect("session is creatable");
        let mut decoder = VideoToolboxDecoder::new(codec);
        let mut source = Nv12Frame::new(WIDTH, HEIGHT).expect("buffer is allocatable");
        let mut luma = Vec::new();

        let mut compared = 0;

        for phase in 0..FRAMES {
            paint(&mut source, phase);
            encoder
                .encode(source.pixel_buffer(), phase as u64 * 33_333, phase == 0)
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
                "{codec:?} phase {phase} decoded to a different picture, mean luma error \
                 {error:.1}"
            );

            compared += 1;
        }

        assert!(
            compared * 4 >= FRAMES * 3,
            "only {compared} of {FRAMES} {codec:?} frames came back"
        );
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

#[cfg(target_os = "macos")]
mod through_the_wire {
    use std::time::Duration;

    use prism_core::decode::videotoolbox::VideoToolboxDecoder;
    use prism_core::encode::EncoderConfig;
    use prism_core::encode::videotoolbox::{Nv12Frame, VideoToolboxEncoder};
    use prism_core::net::negotiate::Codec;
    use prism_core::net::packet::{FLAG_IDR, FLAG_LAST_OF_FRAME, MAX_PACKET_SIZE, VideoPacket};
    use prism_core::net::packetize::SlicePacketizer;
    use prism_core::net::reassemble::{FrameReassembler, PushOutcome};

    use crate::round_trip::paint;

    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;

    /// How many frames to push through. More than the sixty at which parameter sets repeat,
    /// because a path that only carries those looks perfect over a handful.
    const FRAMES: usize = 90;

    /// Encodes frames, packetises every slice, reassembles them, and decodes the result.
    ///
    /// The in-process round trip already proves the encoder and decoder agree. This proves
    /// that what the network puts back together is the same thing — which is a separate
    /// claim, and the one that was false: HEVC decoded perfectly frame to frame and failed on
    /// every frame that crossed the wire.
    fn wire_round_trip(codec: Codec) -> (usize, usize) {
        let settings = EncoderConfig {
            codec,
            width: WIDTH,
            height: HEIGHT,
            fps: 60,
            bitrate_bps: 8_000_000,
            max_slice_bytes: 0,
        };

        let mut encoder = VideoToolboxEncoder::new(settings).expect("session is creatable");
        let mut decoder = VideoToolboxDecoder::new(codec);
        let mut source = Nv12Frame::new(WIDTH, HEIGHT).expect("buffer is allocatable");
        let mut reassembler = FrameReassembler::new(4);
        let mut buf = [0u8; MAX_PACKET_SIZE];

        let mut decoded = 0;
        let mut submitted = 0;

        for phase in 0..FRAMES {
            paint(&mut source, phase);
            encoder
                .encode(source.pixel_buffer(), phase as u64 * 16_667, phase == 0)
                .expect("frame encodes");

            let Some(frame) = encoder.poll(Duration::from_secs(5)) else {
                continue;
            };

            let last = frame.slices.len() - 1;
            let mut complete = None;

            for index in 0..frame.slices.len() {
                let data = frame.slice(index).expect("slice index is in range");
                let mut flags = 0;
                if frame.is_idr {
                    flags |= FLAG_IDR;
                }
                if index == last {
                    flags |= FLAG_LAST_OF_FRAME;
                }

                let packets =
                    SlicePacketizer::new(phase as u32, index as u16, flags, frame.pts_us, data)
                        .expect("an encoded slice is packetisable");

                for packet in packets {
                    let len = packet.encode_into(&mut buf).expect("packet fits");
                    let parsed = VideoPacket::decode(&buf[..len]).expect("packet parses");

                    if reassembler.push(&parsed) == PushOutcome::FrameComplete {
                        complete = reassembler
                            .take_completed()
                            .map(|frame| (frame.data.to_vec(), frame.capture_ts_us));
                    }
                }
            }

            let Some((bitstream, pts_us)) = complete else {
                continue;
            };

            // The bytes that came off the wire have to be the bytes that went on it.
            assert_eq!(
                bitstream, frame.data,
                "{codec:?} frame {phase} was reassembled into something else"
            );

            submitted += 1;
            if decoder.decode(&bitstream, pts_us).is_ok()
                && decoder.poll(Duration::from_secs(2)).is_some()
            {
                decoded += 1;
            }
        }

        (decoded, submitted)
    }

    #[test]
    fn h264_survives_the_wire() {
        let (decoded, submitted) = wire_round_trip(Codec::H264);

        assert!(
            decoded * 4 >= submitted * 3,
            "only {decoded} of {submitted} frames decoded after crossing the wire"
        );
    }

    #[test]
    fn hevc_survives_the_wire() {
        let (decoded, submitted) = wire_round_trip(Codec::Hevc);

        assert!(
            decoded * 4 >= submitted * 3,
            "only {decoded} of {submitted} frames decoded after crossing the wire"
        );
    }
}
