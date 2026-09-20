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
//!
//! A machine with no NVENC encodes through Media Foundation instead, from the same conversion
//! target. See [`crate::encode::mediafoundation`] for what that costs and where.

#![cfg(target_os = "windows")]

use windows::core::Interface;

use crate::capture::wgc::ScreenCapture;
use crate::capture::{CaptureConfig, CaptureError};
use crate::encode::mediafoundation::MediaFoundationEncoder;
use crate::encode::nv12::{Bgra2Nv12, Nv12Texture};
use crate::encode::nvenc::NvencEncoder;
use crate::encode::pump::{PumpConfig, Pumped, REPEAT_INTERVAL, after_send};
use crate::encode::{EncodeError, EncodedFrame, EncoderConfig};
use crate::net::sender::SliceSender;

/// How many slices NVENC cuts each frame into.
///
/// Counted rather than sized: NVENC takes a slice count where VideoToolbox takes a byte limit.
/// Four, so transmission of a frame can begin about a quarter of the way through encoding it
/// rather than at the end.
const SLICES_PER_FRAME: u32 = 4;

/// Whichever encoder this machine turned out to have.
enum Encoder {
    /// NVIDIA's, driven directly.
    Nvenc(NvencEncoder),
    /// Whatever Media Foundation lists, on a machine with no NVENC.
    MediaFoundation(MediaFoundationEncoder),
}

impl Encoder {
    /// Opens NVENC, and Media Foundation if there is none.
    ///
    /// Both reasons are kept when both fail. Which of the two a machine was expected to have
    /// is not something this can know, and the one left out would be the one that mattered.
    fn open(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        target: &Nv12Texture,
        config: EncoderConfig,
    ) -> Result<Self, String> {
        // SAFETY: the device and the texture are owned by the pump that owns this encoder, and
        // so outlive it.
        let nvenc =
            unsafe { NvencEncoder::new(device.as_raw(), target.texture().as_raw(), config) };

        let no_nvenc = match nvenc {
            Ok(encoder) => return Ok(Self::Nvenc(encoder)),
            Err(err) => err,
        };

        MediaFoundationEncoder::new(device, target.texture(), config)
            .map(Self::MediaFoundation)
            .map_err(|no_transform| {
                format!("{no_nvenc}; and through Media Foundation, {no_transform}")
            })
    }

    /// Changes the target bitrate of the running encoder.
    fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        match self {
            Self::Nvenc(encoder) => encoder.set_bitrate_bps(bitrate_bps),
            Self::MediaFoundation(encoder) => encoder.set_bitrate_bps(bitrate_bps),
        }
    }

    /// Encodes what is in the conversion target, returning nothing on a turn where a frame
    /// went in and none has come out yet.
    fn encode(
        &mut self,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Option<&EncodedFrame>, EncodeError> {
        match self {
            Self::Nvenc(encoder) => encoder.encode(pts_us, force_idr).map(Some),
            Self::MediaFoundation(encoder) => encoder.encode(pts_us, force_idr),
        }
    }
}

/// The screen, encoded, ready to send.
///
/// Owns the capture, the conversion target and the encoder. Dropping it stops all three.
pub struct ScreenPump {
    capture: ScreenCapture,
    encoder: Encoder,
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

        let encoder = Encoder::open(
            device,
            &target,
            EncoderConfig {
                codec: config.codec,
                width,
                height,
                fps: config.fps,
                bitrate_bps: config.bitrate_bps,
                max_slice_bytes: SLICES_PER_FRAME,
            },
        )
        .map_err(|reason| CaptureError::Start { reason })?;

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
    /// it matters most. Through Media Foundation a frame comes out however the transform chose
    /// to cut it, which nothing here asks it to do.
    #[must_use]
    pub fn slicing_supported(&self) -> bool {
        matches!(self.encoder, Encoder::Nvenc(_))
    }

    /// Captures one frame, encodes it, and sends its slices.
    ///
    /// A turn where the compositor delivered nothing sends the screen as it last was, so a
    /// still machine is a still picture rather than no picture.
    ///
    /// # Errors
    ///
    /// Returns a message if the conversion or the encoder fails. A socket that has lost its
    /// client is reported as [`Pumped::PeerGone`] rather than as an error.
    pub fn pump(&mut self, sender: &mut SliceSender, adaptive: bool) -> Result<Pumped, String> {
        // Waiting the whole repeat interval is what makes a turn with nothing new the turn
        // that sends the screen again: this encoder hands a frame back as it takes one, so
        // unlike the asynchronous one there is never a frame trapped inside it waiting for a
        // successor before it can come out.
        let fresh = self.capture.poll(REPEAT_INTERVAL);

        // A still screen delivers nothing, and the conversion target still holds whatever the
        // screen last was, so a turn with nothing new encodes that again rather than sending
        // silence. Somebody who starts watching a machine nobody is touching would otherwise
        // sit in front of a black window until a mouse moved on the far end. Nothing but the
        // first turn can do this: there is no picture to repeat before one has arrived.
        if fresh.is_none() && self.submitted == 0 {
            return Ok(Pumped::Idle);
        }

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

        if let Some(captured) = fresh {
            let Ok(bgra) = captured.texture() else {
                return Ok(Pumped::Dropped);
            };

            self.converter
                .convert(&bgra, &self.target)
                .map_err(|err| err.to_string())?;
        }

        let encoded = self
            .encoder
            .encode(capture_ts_us, force_idr)
            .map_err(|err| err.to_string())?;

        // Counted whether or not anything came out, because what it records is that the
        // encoder has been given its first picture — and with it the keyframe the first one
        // has to be, which an encoder that is still filling goes on owing by itself.
        self.submitted += 1;

        let Some(frame) = encoded else {
            return Ok(Pumped::Filling);
        };

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
