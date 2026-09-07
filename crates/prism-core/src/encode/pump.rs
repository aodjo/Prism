//! The screen's path from the compositor to the wire, in one place.
//!
//! Capture a frame, encode it, send the slices. Three lines to describe and five copies of it
//! had grown across this repository — one per host mode in the command line, one per platform
//! in the session service — which is how the window application came to be missing both of
//! M6's fixes while the command line had them. It was capped at the frame rate a serial
//! encoder gives, and it could not recover from a single lost frame, and nothing said so
//! because each copy looked correct on its own.
//!
//! This is that loop, once. What callers still differ about is what to do around it: how long
//! to run, what to count, what to print. So this owns the pipeline and hands back one frame at
//! a time, rather than owning the loop as well.

#![cfg(target_os = "macos")]

use std::collections::VecDeque;
use std::time::Duration;

use crate::capture::screencapturekit::{CapturedFrame, ScreenCapture};
use crate::capture::{CaptureConfig, CaptureError};
use crate::encode::EncoderConfig;
use crate::encode::videotoolbox::VideoToolboxEncoder;
use crate::net::sender::SliceSender;

/// How many frames may be inside the encoder at once.
///
/// Two, and the second one is worth a great deal. Submitting a frame and waiting for it before
/// handling the next leaves the hardware idle through everything else the loop does, so the
/// achievable rate is the sum rather than the larger of the two. Measured at 1440p with HEVC
/// on Apple Silicon: one in flight encodes 600 frames in 9.05 s (66 fps), two in 3.05 s
/// (197 fps), and the per-frame latency does not move (p50 6.14 → 6.17 ms).
///
/// It stops at two because frames beyond that are queued rather than overlapped, and a queue
/// inside the encoder is latency with nobody's name on it: three in flight costs 3.83 ms of
/// p50, and six costs 15 ms. That is the trade the one-frame VBV exists to refuse.
pub const IN_FLIGHT: usize = 2;

/// How long to wait for the compositor before counting the frame as missing.
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(500);

/// How long to wait for the encoder before giving up on a frame.
///
/// Thirty times the measured encode latency at 1440p. Reaching it means the encoder has
/// stopped rather than fallen behind.
const ENCODE_TIMEOUT: Duration = Duration::from_millis(200);

/// What one turn of the pipeline produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pumped {
    /// A frame was captured, encoded and sent.
    Sent,
    /// A frame went in and the pipeline is still filling, so none came out yet.
    Filling,
    /// The compositor delivered nothing within the timeout.
    ///
    /// Ordinary on a still screen, which ScreenCaptureKit does not send frames for. A caller
    /// that sees many in a row is looking at a compositor that has stopped.
    Idle,
    /// A frame went in and nothing came out of the encoder in time.
    Dropped,
    /// The client is no longer there.
    ///
    /// A connected UDP socket learns this from the port-unreachable the far machine's kernel
    /// sends when nothing is listening any more, which arrives as a refused connection on the
    /// next write. It is how an ordinary disconnection looks from here, so it ends the session
    /// rather than failing it — a host that showed an error every time somebody closed their
    /// client would be showing an error after most sessions.
    PeerGone,
}

/// Whether a socket error means the client has gone rather than that something broke.
fn peer_gone(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
    )
}

/// Turns a failed send into either an ordinary ending or a real error.
fn after_send(err: &std::io::Error) -> Result<Pumped, String> {
    if peer_gone(err) {
        Ok(Pumped::PeerGone)
    } else {
        Err(err.to_string())
    }
}

/// What a pipeline is being asked to produce.
#[derive(Debug, Clone, Copy)]
pub struct PumpConfig {
    /// Frames per second to capture at.
    pub fps: u32,
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Width to scale frames to, or zero for the display's native width.
    ///
    /// A Retina display is far larger than anything worth streaming at frame rate, and the
    /// compositor scales for free while it is already touching the pixels.
    pub width: u32,
    /// Height to scale frames to, or zero for the display's native height.
    pub height: u32,
    /// The codec the two machines agreed on.
    ///
    /// Taken from [`SliceSender::agreed`] rather than from anything decided earlier: a host
    /// that encodes one thing having agreed another produces a client that decodes nothing
    /// and reports no error, because a decoder waiting for parameter sets it will never see
    /// has nothing to complain about.
    pub codec: crate::net::negotiate::Codec,
}

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
