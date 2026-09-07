//! The Windows half of the pipeline: Windows.Graphics.Capture into NVENC.
//!
//! What is peculiar to this platform: a captured frame is a BGRA texture on the GPU, and NVENC
//! wants NV12, so a shader converts between them on the device the frame already lives on —
//! the pixels never touch the CPU. And this encoder wrapper is synchronous, handing back the
//! finished frame from the call that submitted it, so there is nothing to overlap and only one
//! frame is ever inside it.
//!
//! That last point is the one thing this pipeline does worse than the macOS one, and it is not
//! a property of NVENC. NVENC has `enableEncodeAsync` and a completion event, which is what the
//! plan means by encoding asynchronously; the wrapper here simply does not use them yet.
//! Turning that on is worth what it was worth on the other platform — 66 frames a second to
//! 197 at 1440p — and it needs the hardware in front of it to be worth writing.

#![cfg(target_os = "windows")]

use windows::core::Interface;

use crate::capture::wgc::ScreenCapture;
use crate::capture::{CaptureConfig, CaptureError};
use crate::encode::EncoderConfig;
use crate::encode::nv12::{Bgra2Nv12, Nv12Texture};
use crate::encode::nvenc::NvencEncoder;
use crate::encode::pump::{CAPTURE_TIMEOUT, PumpConfig, Pumped, after_send};
use crate::net::sender::SliceSender;

/// How many slices NVENC cuts each frame into.
///
/// Counted rather than sized: NVENC takes a slice count where VideoToolbox takes a byte limit.
/// Four, so transmission of a frame can begin about a quarter of the way through encoding it
/// rather than at the end.
const SLICES_PER_FRAME: u32 = 4;

/// The screen, encoded, ready to send.
///
/// Owns the capture, the conversion target and the encoder. Dropping it stops all three.
pub struct ScreenPump {
    capture: ScreenCapture,
    encoder: NvencEncoder,
    converter: Bgra2Nv12,
    target: Nv12Texture,
    width: u32,
    height: u32,
    submitted: u32,
    emitted: u32,
}

impl ScreenPump {
    /// Starts capturing the screen and encoding it for the session that has been agreed.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Start`] if the capture, the conversion or the encoder will not
    /// start, and whatever the capture reports if the display cannot be opened.
    pub fn start(config: PumpConfig) -> Result<Self, CaptureError> {
        let capture = ScreenCapture::start(CaptureConfig {
            fps: config.fps,
            width: config.width,
            height: config.height,
            ..CaptureConfig::default()
        })?;

        // Even dimensions, because NV12 subsamples chroma by two in both directions.
        let (width, height) = (capture.width() & !1, capture.height() & !1);
        let device = capture.device();

        let failed = |err: crate::encode::EncodeError| CaptureError::Start {
            reason: err.to_string(),
        };

        let target = Nv12Texture::new(device, width, height).map_err(failed)?;
        let converter = Bgra2Nv12::new(device).map_err(failed)?;

        // SAFETY: the device and the texture are owned by this struct and so outlive the
        // encoder, which is dropped with it.
        let encoder = unsafe {
            NvencEncoder::new(
                device.as_raw(),
                target.texture().as_raw(),
                EncoderConfig {
                    codec: config.codec,
                    width,
                    height,
                    fps: config.fps,
                    bitrate_bps: config.bitrate_bps,
                    max_slice_bytes: SLICES_PER_FRAME,
                },
            )
        }
        .map_err(failed)?;

        Ok(Self {
            capture,
            encoder,
            converter,
            target,
            width,
            height,
            submitted: 0,
            emitted: 0,
        })
    }

    /// The picture size the session is actually sending.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// How many frames have been sent.
    #[must_use]
    pub fn sent(&self) -> u32 {
        self.emitted
    }

    /// Whether the encoder cuts frames into slices that can be sent before the frame is done.
    ///
    /// Always, on NVENC. It is the platform where slicing works, which is the platform where
    /// it matters most.
    #[must_use]
    pub fn slicing_supported(&self) -> bool {
        true
    }

    /// Captures one frame, encodes it, and sends its slices.
    ///
    /// # Errors
    ///
    /// Returns a message if the conversion or the encoder fails. A socket that has lost its
    /// client is reported as [`Pumped::PeerGone`] rather than as an error.
    pub fn pump(&mut self, sender: &mut SliceSender, adaptive: bool) -> Result<Pumped, String> {
        let Some(captured) = self.capture.poll(CAPTURE_TIMEOUT) else {
            return Ok(Pumped::Idle);
        };

        let Ok(bgra) = captured.texture() else {
            return Ok(Pumped::Dropped);
        };

        let capture_ts_us = crate::clock::now_us();

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
        // makes every later frame undecodable and the client cannot fix it alone. This is what
        // the Windows path was missing while the macOS one had it.
        let force_idr = self.submitted == 0 || sender.take_keyframe_request();

        self.converter
            .convert(&bgra, &self.target)
            .map_err(|err| err.to_string())?;

        let frame = self
            .encoder
            .encode(capture_ts_us, force_idr)
            .map_err(|err| err.to_string())?;

        self.submitted += 1;

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
            .field("size", &(self.width, self.height))
            .field("emitted", &self.emitted)
            .finish_non_exhaustive()
    }
}
