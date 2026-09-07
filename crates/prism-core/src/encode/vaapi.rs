//! Encoding on Linux through VAAPI.
//!
//! One interface over every GPU a Linux machine is likely to have: Intel through the media
//! driver, AMD through Mesa, NVIDIA through NVDEC's VAAPI shim. That breadth is the reason to
//! use it rather than each vendor's own SDK, which is the arrangement Windows is stuck with.
//!
//! # What this file decides, and what the driver decides
//!
//! VAAPI is a thin layer. It does not choose a profile, a rate control mode, or a buffer size;
//! it carries whatever it is given to the hardware, and a driver handed a bad configuration
//! produces a stream that decodes into something nobody wants rather than an error. So the
//! decisions the plan makes about latency have to be made here, explicitly, and they are the
//! part of this file worth reading:
//!
//! - **Constant bitrate against a one-frame buffer.** The single most important setting in the
//!   whole pipeline. A larger buffer lets the encoder answer a hard frame with a huge one, and
//!   a huge frame takes several frame times to transmit — which is a latency spike with no
//!   name on it. Sized in [`RateControl::for_frames`].
//! - **No frame reordering and no B-frames.** A B-frame cannot be encoded until the frame
//!   after it exists, so it costs a whole frame of latency before anything else happens.
//! - **The intra period is effectively infinite.** A periodic keyframe is a periodic bitrate
//!   spike; recovery happens on request instead, which the client asks for when it can no
//!   longer decode.

#![cfg(all(target_os = "linux", feature = "vaapi"))]

use crate::encode::EncoderConfig;
use crate::net::negotiate::Codec;

/// The VAAPI profile a codec is encoded under.
///
/// Named rather than taken from the bindings so the choice is visible and testable without a
/// GPU: the numbers are stable ABI values from `va.h`, and getting one wrong configures the
/// hardware for a different codec than the session agreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// H.264 High. Every decoder this project targets handles it, and it is what the fallback
    /// codec means in practice.
    H264High,
    /// HEVC Main. Roughly half the bitrate of H.264 for the same picture.
    HevcMain,
    /// AV1 Profile 0. Only on hardware new enough to encode it, which the negotiation checks
    /// before it is ever asked for.
    Av1Profile0,
}

impl Profile {
    /// Returns the profile to encode a codec under.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedCodec`] for a codec this backend cannot encode, which is the right
    /// outcome rather than a silent substitution: a host that agreed one codec and encoded
    /// another produces a client showing a black window with nothing reporting a fault.
    pub fn for_codec(codec: Codec) -> Result<Self, UnsupportedCodec> {
        match codec {
            Codec::H264 => Ok(Profile::H264High),
            Codec::Hevc => Ok(Profile::HevcMain),
            Codec::Av1 => Ok(Profile::Av1Profile0),
        }
    }

    /// Returns the `VAProfile` value this is, as `va.h` defines it.
    ///
    /// These are wire-stable constants in the VAAPI ABI rather than an enumeration the
    /// bindings happen to order a particular way. Checked against an Intel TigerLake UHD with
    /// the iHD driver, which reports 7 and 17 among the profiles it can encode — and does not
    /// report 32, so a Linux host must ask the driver what it has rather than advertise AV1
    /// because the code knows the number.
    #[must_use]
    pub fn as_va_profile(self) -> i32 {
        match self {
            // VAProfileH264High
            Profile::H264High => 7,
            // VAProfileHEVCMain
            Profile::HevcMain => 17,
            // VAProfileAV1Profile0
            Profile::Av1Profile0 => 32,
        }
    }
}

/// Which of VAAPI's encode paths a driver offers.
///
/// Two exist and hardware does not always have both. Measured on an Intel TigerLake UHD with
/// the iHD driver: it advertises **only** [`Entrypoint::LowPower`], and asking for the other
/// one fails with `VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT` — which is worth knowing because
/// the ordinary path is the one every example uses, and a host that assumed it would refuse to
/// start on that machine for a reason it could not explain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entrypoint {
    /// The general encode path.
    Slice,
    /// The fixed-function low power path.
    ///
    /// Lower latency and lower power for the same bitrate, at some quality. On the hardware
    /// measured here it is not an optimisation but the only option.
    LowPower,
}

impl Entrypoint {
    /// Returns the `VAEntrypoint` value this is, as `va.h` defines it.
    #[must_use]
    pub fn as_va_entrypoint(self) -> u32 {
        match self {
            // VAEntrypointEncSlice
            Entrypoint::Slice => 6,
            // VAEntrypointEncSliceLP
            Entrypoint::LowPower => 8,
        }
    }

    /// Returns the entrypoints to try, best first.
    ///
    /// Low power first. It is what this project wants anyway — the whole design trades picture
    /// for latency — and it is the one some hardware has instead of the other rather than as
    /// well as it.
    #[must_use]
    pub fn preference() -> [Self; 2] {
        [Entrypoint::LowPower, Entrypoint::Slice]
    }
}

/// A codec this backend cannot encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("VAAPI here cannot encode {codec:?}")]
pub struct UnsupportedCodec {
    /// What was asked for.
    pub codec: Codec,
}

/// How much the encoder may spend, and how far ahead it may spend it.
///
/// The second number is the one that matters. Rate control is not about the average — every
/// encoder hits its average — it is about what happens to a frame the encoder finds hard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateControl {
    /// Target bits per second.
    pub bits_per_second: u32,
    /// The buffer the encoder is allowed to think in, in bits.
    pub buffer_bits: u32,
    /// How full that buffer is considered at the start, in bits.
    ///
    /// The same as the buffer, so the encoder begins with its whole allowance rather than
    /// spending the first second earning it.
    pub initial_buffer_bits: u32,
}

impl RateControl {
    /// Sizes the buffer to hold this many frames.
    ///
    /// One is the setting the latency target is built on, and the argument exists so the
    /// reasoning can be tested rather than only asserted: at one frame the encoder cannot
    /// produce a picture that takes longer than a frame time to send, which is exactly the
    /// property the whole pipeline depends on.
    ///
    /// # Panics
    ///
    /// Never. A frame rate of zero is treated as one, because a session that negotiated zero
    /// frames a second has a bigger problem than its buffer size and dividing by it here would
    /// only hide that.
    #[must_use]
    pub fn for_frames(bitrate_bps: u32, fps: u32, frames: u32) -> Self {
        let per_frame = bitrate_bps / fps.max(1);
        // Saturating rather than wrapping: a wrapped buffer size is a small buffer, which
        // looks like a working encoder producing terrible pictures.
        let buffer_bits = per_frame.saturating_mul(frames.max(1));

        Self {
            bits_per_second: bitrate_bps,
            buffer_bits,
            initial_buffer_bits: buffer_bits,
        }
    }

    /// The rate control a session should run at.
    ///
    /// One frame of buffer, from the plan's first design decision.
    #[must_use]
    pub fn for_session(config: &EncoderConfig) -> Self {
        Self::for_frames(config.bitrate_bps, config.fps, 1)
    }
}

/// How many slices a frame is cut into.
///
/// Transmission of a frame can begin as soon as its first slice is finished rather than when
/// the whole picture is, which is worth about half a frame time. More slices means starting
/// sooner and compressing slightly worse, because a slice cannot predict across its own
/// boundary.
///
/// Rounded down to whole macroblock rows, since a slice is a run of them: asking for more
/// slices than the picture has rows would have the driver silently choose its own number.
#[must_use]
pub fn slices_for(height: u32, wanted: u32) -> u32 {
    /// Macroblock height in pixels, for H.264 and the smallest coding unit HEVC will use.
    const MACROBLOCK: u32 = 16;

    let rows = height.div_ceil(MACROBLOCK).max(1);

    wanted.clamp(1, rows)
}

#[cfg(test)]
mod tests {
    use super::{Profile, RateControl, slices_for};
    use crate::encode::EncoderConfig;
    use crate::net::negotiate::Codec;

    #[test]
    fn every_negotiable_codec_has_a_profile() {
        // The negotiation will only agree a codec this machine advertised, so a codec arriving
        // here without a profile means the two lists have drifted apart — and the symptom
        // would be a session that opens and never shows a picture.
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            let profile = Profile::for_codec(codec).expect("a profile");

            assert!(
                profile.as_va_profile() > 0,
                "{codec:?} maps to a profile of {}",
                profile.as_va_profile()
            );
        }
    }

    #[test]
    fn no_two_codecs_share_a_profile() {
        // A transposed pair would configure the hardware for the wrong codec and produce a
        // bitstream the client cannot parse, with nothing between here and there to notice.
        let profiles: Vec<i32> = [Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .map(|codec| {
                Profile::for_codec(codec)
                    .expect("a profile")
                    .as_va_profile()
            })
            .collect();

        let mut sorted = profiles.clone();
        sorted.sort_unstable();
        sorted.dedup();

        assert_eq!(sorted.len(), profiles.len(), "two codecs share a profile");
    }

    #[test]
    fn the_buffer_holds_exactly_one_frame_of_bits() {
        // The single most important number in the pipeline. A buffer of several frames lets
        // the encoder answer a hard frame with a picture that takes several frame times to
        // transmit, and that is a latency spike nothing downstream can undo.
        let rate = RateControl::for_session(&EncoderConfig {
            codec: Codec::H264,
            width: 2560,
            height: 1440,
            fps: 120,
            bitrate_bps: 40_000_000,
            max_slice_bytes: 0,
        });

        assert_eq!(rate.bits_per_second, 40_000_000);
        assert_eq!(rate.buffer_bits, 40_000_000 / 120);
        assert_eq!(
            rate.initial_buffer_bits, rate.buffer_bits,
            "the encoder should start with its whole allowance"
        );
    }

    #[test]
    fn a_bigger_buffer_is_bigger_by_exactly_the_frames_asked_for() {
        let one = RateControl::for_frames(24_000_000, 60, 1);
        let four = RateControl::for_frames(24_000_000, 60, 4);

        assert_eq!(four.buffer_bits, one.buffer_bits * 4);
    }

    #[test]
    fn a_frame_rate_of_zero_does_not_divide_by_it() {
        let rate = RateControl::for_frames(24_000_000, 0, 1);

        assert_eq!(rate.buffer_bits, 24_000_000);
    }

    #[test]
    fn the_low_power_path_is_tried_first() {
        // Not a preference so much as a fact about the hardware: the Intel part this was
        // measured on has the low power entrypoint and not the other, so a host that tried
        // them the other way round would fail on its first attempt every time.
        assert_eq!(
            super::Entrypoint::preference()[0],
            super::Entrypoint::LowPower
        );
    }

    #[test]
    fn the_entrypoints_are_the_numbers_va_h_gives_them() {
        // Transposing these asks the driver for a decode path and gets an error whose text
        // says nothing about which of the two was meant.
        assert_eq!(super::Entrypoint::Slice.as_va_entrypoint(), 6);
        assert_eq!(super::Entrypoint::LowPower.as_va_entrypoint(), 8);
    }

    #[test]
    fn slices_never_exceed_the_rows_there_are_to_cut() {
        // Asking for more slices than the picture has macroblock rows would have the driver
        // quietly pick its own number, and a frame cut differently from what the sender
        // believes is a frame the reassembler is told the wrong shape of.
        assert_eq!(slices_for(1080, 4), 4);
        assert_eq!(slices_for(64, 100), 4, "1080p has 68 rows, 64 pixels has 4");
        assert_eq!(slices_for(16, 8), 1);
    }

    #[test]
    fn there_is_always_at_least_one_slice() {
        assert_eq!(slices_for(0, 0), 1);
        assert_eq!(slices_for(1080, 0), 1);
    }
}
