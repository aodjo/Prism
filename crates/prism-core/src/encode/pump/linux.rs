//! The Linux half of the pipeline: the desktop portal's PipeWire stream into OpenH264.
//!
//! Everything here is on the processor, which the other two platforms go out of their way never
//! to be. A frame arrives from PipeWire as bytes, is converted to I420 as it is scaled to the
//! session's size, and is encoded in software. That is two passes over every pixel the macOS and
//! Windows pipelines never make, and it is what lets any Linux machine be a host — a board, a
//! virtual machine, a laptop whose GPU driver encodes nothing.
//!
//! The encoder is synchronous, so like the Windows pipeline there is only ever one frame inside
//! it and a frame that went in is a frame that came out.

#![cfg(linux_desktop)]

use crate::capture::CaptureError;
use crate::capture::fit_within;
use crate::capture::pipewire::{Frame, Frames};
use crate::capture::portal::{self, Granted};
use crate::encode::EncoderConfig;
use crate::encode::openh264::SoftwareEncoder;
use crate::encode::pump::{PumpConfig, Pumped, REPEAT_INTERVAL, after_send};
use crate::net::sender::SliceSender;
use crate::yuv::I420;

/// The screen, encoded, ready to send.
///
/// Owns the portal session, the stream and the encoder. Dropping it stops all three — the stream
/// first, because it reads through the session the portal granted.
pub struct ScreenPump {
    frames: Frames,
    /// Held for as long as the stream is; dropping it closes the portal session.
    _granted: Granted,
    encoder: SoftwareEncoder,
    /// The frame most recently taken from the stream, and the buffer the next is swapped into.
    frame: Frame,
    picture: I420,
    submitted: u32,
    emitted: u32,
}

impl ScreenPump {
    /// Asks the desktop for its screen, and starts encoding it for the session that has been
    /// agreed.
    ///
    /// Opens without asking anybody when an earlier session left a restore token, and otherwise
    /// waits for the person at the machine to answer the portal's dialog.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::PermissionDenied`] if the desktop refused, and
    /// [`CaptureError::Start`] if the portal, the stream or the encoder will not start.
    pub fn start(config: PumpConfig) -> Result<Self, CaptureError> {
        let mut granted = portal::open()?;
        let remote = granted.take_remote();

        let frames = Frames::start(remote, granted.node, config.fps)?;
        let screen = frames.agreed(REPEAT_INTERVAL)?;

        // The screen's shape, inside whatever the client said it can show. Scaled here, on the
        // way into the encoder, because nothing earlier on this path will do it.
        let (width, height) = fit_within(screen, (config.width, config.height));
        let picture = I420::new(width, height);

        let encoder = SoftwareEncoder::new(EncoderConfig {
            codec: config.codec,
            width: picture.width(),
            height: picture.height(),
            fps: config.fps,
            bitrate_bps: config.bitrate_bps,
            max_slice_bytes: 0,
        })
        .map_err(|err| CaptureError::Start {
            reason: err.to_string(),
        })?;

        if !granted.controls() {
            eprintln!(
                "host: this desktop shares its screen but not its input, so it can be watched \
                 and not controlled"
            );
        }

        Ok(Self {
            frames,
            _granted: granted,
            encoder,
            frame: Frame::default(),
            picture,
            submitted: 0,
            emitted: 0,
        })
    }

    /// The picture size the session is actually sending.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.picture.width(), self.picture.height())
    }

    /// How many frames have been sent.
    #[must_use]
    pub fn sent(&self) -> u32 {
        self.emitted
    }

    /// Whether the encoder cuts frames into slices that can be sent before the frame is done.
    ///
    /// No: OpenH264 encodes a frame into one slice here, and it has finished the whole of it by
    /// the time any of it could be sent.
    #[must_use]
    pub fn slicing_supported(&self) -> bool {
        false
    }

    /// Captures one frame, encodes it, and sends it.
    ///
    /// A turn where the compositor delivered nothing sends the screen as it last was, so a still
    /// machine is a still picture rather than no picture.
    ///
    /// # Errors
    ///
    /// Returns a message if the stream stopped or the encoder failed. A socket that has lost its
    /// client is reported as [`Pumped::PeerGone`] rather than as an error.
    pub fn pump(&mut self, sender: &mut SliceSender, adaptive: bool) -> Result<Pumped, String> {
        let fresh = self
            .frames
            .take(&mut self.frame, REPEAT_INTERVAL)
            .map_err(|err| err.to_string())?;

        if !fresh && self.submitted == 0 {
            return Ok(Pumped::Idle);
        }

        let capture_ts_us = crate::clock::now_us();

        if let Err(err) = sender.send_cursor() {
            return after_send(&err);
        }

        if adaptive && let Some(target) = sender.target_bps() {
            // Refused is ignored: a session at the old rate is a worse picture, not no picture.
            let _ = self.encoder.set_bitrate_bps(target);
        }

        let force_idr = self.submitted == 0 || sender.take_keyframe_request();

        if fresh && let Some(order) = self.frame.order {
            self.picture.fill_from(
                &self.frame.bytes,
                self.frame.stride,
                self.frame.width,
                self.frame.height,
                order,
            );
        }

        let frame = self
            .encoder
            .encode(&self.picture, capture_ts_us, force_idr)
            .map_err(|err| err.to_string())?;

        self.submitted += 1;

        if frame.slices.is_empty() {
            return Ok(Pumped::Dropped);
        }

        let frame_id = self.emitted;
        self.emitted += 1;

        sender.note_capture(frame_id, capture_ts_us);

        let last = frame.slices.len().saturating_sub(1);
        for index in 0..frame.slices.len() {
            let data = frame.slice(index).expect("slice index is in range");
            if let Err(err) =
                sender.send_slice(frame_id, data, capture_ts_us, frame.is_idr, index == last)
            {
                return after_send(&err);
            }
        }

        Ok(Pumped::Sent)
    }
}

impl std::fmt::Debug for ScreenPump {
    /// Names the type and what it has done, without reaching into the system objects.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenPump")
            .field("size", &self.size())
            .field("emitted", &self.emitted)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::capture::pipewire::{Frame, Frames};
    use crate::decode::openh264::SoftwareDecoder;
    use crate::encode::EncoderConfig;
    use crate::encode::openh264::SoftwareEncoder;
    use crate::net::negotiate::Codec;
    use crate::yuv::I420;

    /// The pipeline this file drives, minus the portal and the socket: frames from a PipeWire
    /// video source, scaled into I420, encoded, and decoded again.
    ///
    /// The portal is the one part no test can stand in for — it asks a person — so the stream
    /// comes from a source made for the purpose instead. See the capture's own test for how.
    #[test]
    #[ignore = "needs a PipeWire daemon with a video source in PRISM_TEST_PIPEWIRE_NODE"]
    fn a_captured_frame_survives_encoding_and_decoding() {
        let node = std::env::var("PRISM_TEST_PIPEWIRE_NODE")
            .expect("PRISM_TEST_PIPEWIRE_NODE")
            .parse()
            .expect("a node id");

        let frames = Frames::start(None, node, 30).expect("the stream starts");
        let screen = frames.agreed(Duration::from_secs(1)).expect("a format");

        // Half the size, so the scaling on the way in is exercised as well.
        let mut picture = I420::new(screen.0 / 2, screen.1 / 2);
        let mut encoder = SoftwareEncoder::new(EncoderConfig {
            codec: Codec::H264,
            width: picture.width(),
            height: picture.height(),
            fps: 30,
            bitrate_bps: 2_000_000,
            max_slice_bytes: 0,
        })
        .expect("an encoder");
        let mut decoder = SoftwareDecoder::new().expect("a decoder");

        let mut frame = Frame::default();
        let mut shown = None;

        for index in 0..10u64 {
            assert!(
                frames
                    .take(&mut frame, Duration::from_secs(1))
                    .expect("no failure")
            );

            let order = frame.order.expect("a known pixel order");
            picture.fill_from(&frame.bytes, frame.stride, frame.width, frame.height, order);

            let encoded = encoder
                .encode(&picture, index, index == 0)
                .expect("encodes");
            decoder.decode(&encoded.data, index).expect("decodes");

            if let Some(decoded) = decoder.poll(Duration::ZERO) {
                shown = Some(decoded);
            }
        }

        let decoded = shown.expect("a picture came out");

        assert_eq!(
            (decoded.width(), decoded.height()),
            (picture.width(), picture.height())
        );

        // A test pattern is bars of different brightness, so a picture that came through is not
        // one flat grey.
        let luma = decoded.planes().y();
        let (darkest, brightest) = luma
            .iter()
            .fold((u8::MAX, u8::MIN), |(lo, hi), &y| (lo.min(y), hi.max(y)));
        assert!(brightest - darkest > 100, "{darkest}..{brightest}");
    }
}
