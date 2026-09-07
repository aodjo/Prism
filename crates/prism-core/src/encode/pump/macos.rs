//! The macOS half of the pipeline: ScreenCaptureKit into VideoToolbox.
//!
//! What is peculiar to this platform: frames arrive as IOSurface-backed pixel buffers the
//! encoder takes directly, and VideoToolbox is asynchronous, so the pipeline is genuinely two
//! frames deep and the second one is worth a great deal.

#![cfg(target_os = "macos")]

use std::collections::VecDeque;

use crate::capture::screencapturekit::{CapturedFrame, ScreenCapture};
use crate::capture::{CaptureConfig, CaptureError};
use crate::encode::EncoderConfig;
use crate::encode::pump::{
    CAPTURE_TIMEOUT, ENCODE_TIMEOUT, IN_FLIGHT, PumpConfig, Pumped, after_send,
};
use crate::encode::videotoolbox::VideoToolboxEncoder;
use crate::net::sender::SliceSender;

/// The screen, encoded, ready to send.
///
/// Owns the capture, the encoder, and the frames currently inside it. Dropping it stops both.
pub struct ScreenPump {
    capture: ScreenCapture,
    encoder: VideoToolboxEncoder,
    /// Captured frames the encoder may still be reading.
    ///
    /// Holding them is the whole reason this is a queue: the compositor's buffer belongs to
    /// the frame it arrived with, and releasing it while the encoder is mid-frame is a
    /// use-after-free with a picture in it.
    in_flight: VecDeque<CapturedFrame>,
    submitted: u32,
    emitted: u32,
}

impl ScreenPump {
    /// Starts capturing the screen and encoding it for the session that has been agreed.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::PermissionDenied`] when Screen Recording has not been granted,
    /// and [`CaptureError::Start`] if the stream or the encoder will not start.
    pub fn start(config: PumpConfig) -> Result<Self, CaptureError> {
        let capture = ScreenCapture::start(CaptureConfig {
            fps: config.fps,
            width: config.width,
            height: config.height,
            ..CaptureConfig::default()
        })?;

        // The encoder is built for what the capture actually produces, not for what was
        // asked: a compositor that rounded a requested size would otherwise be encoded at
        // dimensions its frames do not have.
        let encoder = VideoToolboxEncoder::new(EncoderConfig {
            codec: config.codec,
            width: capture.width(),
            height: capture.height(),
            fps: config.fps,
            bitrate_bps: config.bitrate_bps,
            max_slice_bytes: config.bitrate_bps / 8 / config.fps.max(1) / 4,
        })
        .map_err(|err| CaptureError::Start {
            reason: err.to_string(),
        })?;

        Ok(Self {
            capture,
            encoder,
            in_flight: VecDeque::with_capacity(IN_FLIGHT),
            submitted: 0,
            emitted: 0,
        })
    }

    /// The picture size the session is actually sending.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.capture.width(), self.capture.height())
    }

    /// How many frames have been sent.
    #[must_use]
    pub fn sent(&self) -> u32 {
        self.emitted
    }

    /// Whether the encoder cuts frames into slices that can be sent before the frame is done.
    #[must_use]
    pub fn slicing_supported(&self) -> bool {
        self.encoder.slicing_supported()
    }

    /// Captures one frame, submits it, and sends whichever finished frame is now ready.
    ///
    /// Call this in a loop. Each turn puts one frame into the encoder and takes at most one
    /// out, which is what keeps the encoder busy through the capture and the send.
    ///
    /// `adaptive` lets the congestion controller set the encoder's bitrate. Pacing alone is
    /// not an actuator: slowing the wire while the encoder produces the same bytes moves the
    /// queue inside the host rather than removing it.
    ///
    /// # Errors
    ///
    /// Returns a message if the encoder rejects a frame or the socket cannot be written to.
    pub fn pump(&mut self, sender: &mut SliceSender, adaptive: bool) -> Result<Pumped, String> {
        let Some(captured) = self.capture.poll(CAPTURE_TIMEOUT) else {
            return Ok(Pumped::Idle);
        };

        // Ahead of the frame's own packets, so the bytes the cursor depends on are not queued
        // behind a whole frame of video.
        if let Err(err) = sender.send_cursor() {
            return after_send(&err);
        }

        if adaptive && let Some(target) = sender.target_bps() {
            // A refusal is ignored rather than reported. A session running at the old rate is
            // a worse picture than asked for; a session that stops is no picture at all.
            let _ = self.encoder.set_bitrate_bps(target);
        }

        // The first frame must be a keyframe or nothing decodes at all. After that, only when
        // the client says it has lost its place: every frame here is a reference, so one gap
        // makes every later frame undecodable and the client cannot fix it alone.
        let force_idr = self.submitted == 0 || sender.take_keyframe_request();
        let capture_ts_us = captured.capture_ts_us;

        self.encoder
            .encode(captured.pixel_buffer(), capture_ts_us, force_idr)
            .map_err(|err| err.to_string())?;

        self.in_flight.push_back(captured);
        self.submitted += 1;

        if self.in_flight.len() < IN_FLIGHT {
            return Ok(Pumped::Filling);
        }

        let drained = self.drain(sender);

        // Released only after its encoded output has been collected.
        self.in_flight.pop_front();

        drained
    }

    /// Sends one finished frame, and says whether there was one.
    ///
    /// The session forbids frame reordering and emits no B-frames, so the nth frame out is the
    /// nth frame in and the count is the right identifier for it.
    fn drain(&mut self, sender: &mut SliceSender) -> Result<Pumped, String> {
        let frame_id = self.emitted;
        self.emitted += 1;

        let Some(frame) = self.encoder.poll(ENCODE_TIMEOUT) else {
            return Ok(Pumped::Dropped);
        };

        let capture_ts_us = frame.pts_us;
        let is_idr = frame.is_idr;
        let last = frame.slices.len().saturating_sub(1);

        sender.note_capture(frame_id, capture_ts_us);

        for slice_id in 0..frame.slices.len() {
            let data = frame.slice(slice_id).expect("slice index is in range");
            if let Err(err) =
                sender.send_slice(frame_id, data, capture_ts_us, is_idr, slice_id == last)
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
            .field("submitted", &self.submitted)
            .field("emitted", &self.emitted)
            .finish_non_exhaustive()
    }
}
