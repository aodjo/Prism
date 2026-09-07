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
//! - **A bitrate this encoder may not be able to hold.** VideoToolbox and NVENC take a target
//!   and a one-frame buffer; measured on an Intel TigerLake UHD, VAAPI's low power entrypoint
//!   offers *only* constant quantiser — `VAConfigAttribRateControl` reports `VA_RC_CQP` and
//!   nothing else. So on that hardware the bitrate is not something to ask for but something
//!   to steer, one frame at a time, through the quantiser. See [`RateControl`].
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

/// How the encoder is kept near its bitrate.
///
/// What a driver offers is not a given. `VAConfigAttribRateControl` on an Intel TigerLake UHD,
/// low power entrypoint, reports `VA_RC_CQP` and nothing else — no constant bitrate, no
/// buffer to size, no target to name. On hardware like that the only lever is the quantiser,
/// and holding a rate means moving it: the frame came out too big, so the next one is coded
/// more coarsely.
///
/// This is the arithmetic for that, kept separate from the encoder so it can be reasoned about
/// and tested without a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateControl {
    /// The bitrate being aimed at.
    pub bits_per_second: u32,
    /// Bytes a frame may take, on average, to hit that rate.
    pub bytes_per_frame: u32,
    /// The quantiser the next frame will be coded at.
    pub qp: u8,
}

/// Coarsest quantiser the controller will use.
///
/// H.264 allows 51, which is a picture nobody would watch. Stopping short of it means a
/// session on a hopeless link produces a bad picture rather than an unrecognisable one, and
/// the congestion controller is the thing that should be lowering the target instead.
pub const MAX_QP: u8 = 42;

/// Finest quantiser the controller will use.
///
/// Below about eighteen the encoder spends bits on detail no viewer of a moving screen will
/// notice, and spending them is what makes a frame too large to send in its own frame time.
pub const MIN_QP: u8 = 18;

impl RateControl {
    /// Starts at a quantiser in the middle of the useful range.
    ///
    /// Twenty-six is H.264's own default and a reasonable guess for a desktop; the first few
    /// frames correct it either way.
    #[must_use]
    pub fn new(bitrate_bps: u32, fps: u32) -> Self {
        Self {
            bits_per_second: bitrate_bps,
            bytes_per_frame: bitrate_bps / 8 / fps.max(1),
            qp: 26,
        }
    }

    /// The controller a session should start with.
    #[must_use]
    pub fn for_session(config: &EncoderConfig) -> Self {
        Self::new(config.bitrate_bps, config.fps)
    }

    /// Moves the quantiser after seeing what a frame actually cost.
    ///
    /// One step at a time rather than jumping to what the last frame implies. A screen that
    /// changes suddenly produces one large frame, and an encoder that answered it by coarsening
    /// several steps would follow one hard frame with several ugly ones — which is more visible
    /// than the frame that caused it.
    pub fn observe(&mut self, frame_bytes: u32) {
        /// Fraction over budget before the quantiser is coarsened, and under before it is
        /// refined. A band rather than a point, so a frame that lands near the target does not
        /// have the controller oscillating around it.
        const TOLERANCE: u32 = 8;

        let over = self.bytes_per_frame + self.bytes_per_frame / TOLERANCE;
        let under = self.bytes_per_frame - self.bytes_per_frame / TOLERANCE;

        if frame_bytes > over {
            self.qp = (self.qp + 1).min(MAX_QP);
        } else if frame_bytes < under {
            self.qp = self.qp.saturating_sub(1).max(MIN_QP);
        }
    }

    /// Changes the rate being aimed at, as the congestion controller decides it.
    ///
    /// The quantiser is left where it is: it is already close to right for the picture, and
    /// the next frame will move it toward the new budget the same way as any other.
    pub fn retarget(&mut self, bitrate_bps: u32, fps: u32) {
        self.bits_per_second = bitrate_bps;
        self.bytes_per_frame = bitrate_bps / 8 / fps.max(1);
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
    fn the_budget_is_the_bitrate_divided_by_the_frame_rate() {
        let rate = RateControl::for_session(&EncoderConfig {
            codec: Codec::H264,
            width: 2560,
            height: 1440,
            fps: 120,
            bitrate_bps: 40_000_000,
            max_slice_bytes: 0,
        });

        assert_eq!(rate.bytes_per_frame, 40_000_000 / 8 / 120);
    }

    #[test]
    fn a_frame_rate_of_zero_does_not_divide_by_it() {
        assert_eq!(RateControl::new(24_000_000, 0).bytes_per_frame, 3_000_000);
    }

    #[test]
    fn a_frame_over_budget_coarsens_the_quantiser_and_one_under_refines_it() {
        let mut rate = RateControl::new(24_000_000, 60);
        let budget = rate.bytes_per_frame;
        let started = rate.qp;

        rate.observe(budget * 4);
        assert_eq!(
            rate.qp,
            started + 1,
            "a frame four times over changed nothing"
        );

        rate.observe(budget / 4);
        rate.observe(budget / 4);
        assert_eq!(
            rate.qp,
            started - 1,
            "two frames well under did not refine it"
        );
    }

    #[test]
    fn a_frame_near_the_budget_leaves_the_quantiser_alone() {
        // Without a band the controller oscillates around the target, and every oscillation is
        // a visible change in how the picture is coded.
        let mut rate = RateControl::new(24_000_000, 60);
        let budget = rate.bytes_per_frame;
        let started = rate.qp;

        rate.observe(budget);
        rate.observe(budget + budget / 100);
        rate.observe(budget - budget / 100);

        assert_eq!(rate.qp, started);
    }

    #[test]
    fn the_quantiser_stays_inside_the_useful_range() {
        // Fifty-one is legal and unwatchable. A hopeless link should produce a bad picture
        // rather than an unrecognisable one, and lowering the target is the congestion
        // controller's job rather than this one's.
        let mut rate = RateControl::new(1_000_000, 60);
        for _ in 0..200 {
            rate.observe(u32::MAX);
        }
        assert_eq!(rate.qp, super::MAX_QP);

        for _ in 0..200 {
            rate.observe(0);
        }
        assert_eq!(rate.qp, super::MIN_QP);
    }

    #[test]
    fn retargeting_moves_the_budget_and_leaves_the_quantiser() {
        let mut rate = RateControl::new(24_000_000, 60);
        rate.observe(rate.bytes_per_frame * 4);
        let qp = rate.qp;

        rate.retarget(12_000_000, 60);

        assert_eq!(rate.bytes_per_frame, 12_000_000 / 8 / 60);
        assert_eq!(rate.qp, qp, "the quantiser is already close to right");
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
