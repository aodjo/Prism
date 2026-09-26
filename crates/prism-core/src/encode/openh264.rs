//! Software H.264 encoding, for a Linux machine with nothing better.
//!
//! Cisco's OpenH264, compiled into this binary. It runs on the processor, so it is the one
//! encoder here that is never waiting on a GPU and never absent: a virtual machine, an ARM board,
//! a laptop whose driver exposes no VAAPI encoder all have it.
//!
//! What it is asked for is what every other backend is asked for, in its own vocabulary:
//!
//! - **Screen content, in real time.** OpenH264 has a mode for exactly this, tuned for text
//!   and flat regions rather than camera noise, and it is the one used.
//! - **Rate control by bitrate, never by skipping.** Its default skips frames to hold a rate,
//!   and a skipped frame on a screen is a click that did nothing. It is told not to.
//! - **No keyframes nobody asked for.** No periodic intra frames and no scene-change detection:
//!   a desktop changes scene every time a window opens, and each of those would be a frame
//!   several times the size of its neighbours. A keyframe is sent when the client asks.
//! - **BT.709, video range, said in the stream.** The colour every renderer in this project
//!   undoes, and written into the parameter sets so a decoder elsewhere is not left to guess.
//!
//! OpenH264 produces the Constrained Baseline profile, which has no B-frames at all — so there
//! is no reordering to turn off, and every frame is finished when `encode` returns.

#![cfg(linux_desktop)]

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig as Settings, FrameRate, FrameType,
    IntraFramePeriod, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;

use crate::decode::nal_units;
use crate::encode::{EncodeError, EncodedFrame, EncoderConfig};
use crate::net::negotiate::Codec;
use crate::yuv::I420;

/// What OpenH264 reports a failure with, which carries a message rather than a code.
const FAILED: i32 = -1;

/// Lets OpenH264 read a picture's planes where they are.
impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width() as usize, self.height() as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        let across = self.width() as usize;

        (across, across / 2, across / 2)
    }

    fn y(&self) -> &[u8] {
        I420::y(self)
    }

    fn u(&self) -> &[u8] {
        I420::u(self)
    }

    fn v(&self) -> &[u8] {
        I420::v(self)
    }
}

/// An H.264 encoder on the processor.
pub struct SoftwareEncoder {
    encoder: Encoder,
    config: EncoderConfig,
    frame: EncodedFrame,
}

impl SoftwareEncoder {
    /// Opens an encoder for pictures of the configured size.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] if the codec is not H.264, which is the only one
    /// OpenH264 makes, or if the encoder will not start.
    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        if config.codec != Codec::H264 {
            return Err(EncodeError::SessionCreate {
                reason: "only H.264 is encoded in software",
                status: FAILED,
            });
        }

        let settings = Settings::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(config.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(config.fps.max(1) as f32))
            .skip_frames(false)
            .scene_change_detect(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .complexity(Complexity::Low)
            .vui(VuiConfig::bt709());

        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), settings).map_err(|_| {
                EncodeError::SessionCreate {
                    reason: "OpenH264 would not start an encoder",
                    status: FAILED,
                }
            })?;

        Ok(Self {
            encoder,
            config,
            frame: EncodedFrame::default(),
        })
    }

    /// The configuration the encoder was opened with, and the bitrate it has been moved to since.
    #[must_use]
    pub fn config(&self) -> EncoderConfig {
        self.config
    }

    /// Changes the target bitrate of the running encoder.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Property`] if OpenH264 refuses the new rate. The session carries
    /// on at the old one.
    pub fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        if bitrate_bps == 0 || bitrate_bps == self.config.bitrate_bps {
            return Ok(());
        }

        let refused = |status: i32| EncodeError::Property {
            property: "bitrate",
            status,
        };

        let mut rate = openh264_sys2::SBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: i32::try_from(bitrate_bps).map_err(|_| refused(FAILED))?,
        };

        // SAFETY: the encoder has been initialised by the time a session is asking to change
        // its rate — the first frame does that — and `rate` is a live `SBitrateInfo`, which is
        // what this option is documented to take. OpenH264 copies it before returning.
        let status = unsafe {
            self.encoder.raw_api().set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                (&raw mut rate).cast(),
            )
        };

        if status != 0 {
            return Err(refused(status));
        }

        self.config.bitrate_bps = bitrate_bps;

        Ok(())
    }

    /// Encodes one picture, a keyframe if `force_idr` says so.
    ///
    /// The frame that comes back holds every NAL unit OpenH264 produced for the picture, in
    /// order, with their start codes rewritten to this project's four-byte form.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if the picture is not the size the encoder was opened
    /// for, and [`EncodeError::Encode`] if OpenH264 refuses it.
    pub fn encode(
        &mut self,
        picture: &I420,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<&EncodedFrame, EncodeError> {
        if (picture.width(), picture.height()) != (self.config.width, self.config.height) {
            return Err(EncodeError::InputBuffer {
                reason: "the picture is not the size the encoder was opened for",
            });
        }

        if force_idr {
            self.encoder.force_intra_frame();
        }

        let stream = self
            .encoder
            .encode(picture)
            .map_err(|_| EncodeError::Encode { status: FAILED })?;

        self.frame.reset();
        self.frame.pts_us = pts_us;
        self.frame.is_idr = matches!(stream.frame_type(), FrameType::IDR);

        for index in 0..stream.num_layers() {
            let Some(layer) = stream.layer(index) else {
                continue;
            };

            for unit in 0..layer.nal_count() {
                // Each unit arrives with a start code of whichever length OpenH264 chose, and
                // this is what strips it.
                for nal in layer.nal_unit(unit).into_iter().flat_map(nal_units) {
                    self.frame.push_nal(nal);
                }
            }
        }

        Ok(&self.frame)
    }
}

impl core::fmt::Debug for SoftwareEncoder {
    /// Describes the encoder without reaching into OpenH264.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SoftwareEncoder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}
