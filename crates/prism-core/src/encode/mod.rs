//! Video encoding.
//!
//! Each platform has its own hardware encoder and its own API for it, but they are all
//! configured towards the same end: emit a frame's worth of bitstream as fast as
//! possible, in pieces small enough to start sending before the frame is finished, and
//! never buffer.
//!
//! The output of every backend is an Annex B elementary stream split into slices. A
//! slice here is one NAL unit including its start code, which is what the packetiser
//! consumes and what the decoder can be fed directly once the pieces are put back in
//! order.

#[cfg(target_os = "windows")]
pub mod nv12;
#[cfg(target_os = "windows")]
pub mod nvenc;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

use core::ops::Range;

/// Four-byte Annex B start code prefixed to every NAL unit.
///
/// The three-byte form is legal too, but a fixed four-byte prefix keeps the offset
/// arithmetic on both sides trivial and costs one byte per NAL unit.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// How an encoder session should be configured.
///
/// The low-latency settings that matter are not expressed here because they are not
/// optional: no B-frames, no lookahead, real-time mode, and a VBV window of a single
/// frame are applied by every backend unconditionally. A configuration that could turn
/// them off would only ever be used by mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frames per second the encoder should expect.
    pub fps: u32,
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Soft ceiling on the bytes in one slice, which is how a frame gets cut into
    /// several NAL units so transmission can start early. Zero leaves slicing to the
    /// encoder's own judgement.
    pub max_slice_bytes: u32,
}

impl EncoderConfig {
    /// Returns the bytes a single frame may occupy at the configured rate.
    ///
    /// This is the VBV buffer size every backend uses: exactly one frame. A larger
    /// window lets the encoder emit an oversized frame that then takes several frame
    /// times to transmit, which is the single largest source of latency spikes in a
    /// video stream.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::encode::EncoderConfig;
    /// let config = EncoderConfig {
    ///     width: 1920, height: 1080, fps: 60,
    ///     bitrate_bps: 24_000_000, max_slice_bytes: 0,
    /// };
    /// assert_eq!(config.frame_byte_budget(), 50_000);
    /// ```
    #[must_use]
    pub fn frame_byte_budget(&self) -> u32 {
        if self.fps == 0 {
            return 0;
        }
        self.bitrate_bps / 8 / self.fps
    }
}

/// One encoded frame as an Annex B elementary stream.
///
/// `data` holds the whole frame, and `slices` indexes the NAL units within it, each
/// range covering a start code and the NAL that follows. Sending the slices in order and
/// concatenating them on the far side reproduces `data` exactly.
///
/// Both buffers are recycled between frames, so a running encoder does not allocate.
#[derive(Debug, Default)]
pub struct EncodedFrame {
    /// Presentation timestamp in microseconds, as supplied to the encoder.
    pub pts_us: u64,
    /// Whether this frame can be decoded without any earlier frame.
    pub is_idr: bool,
    /// The complete Annex B bitstream for this frame.
    pub data: Vec<u8>,
    /// One range per NAL unit, in order, covering start code and payload.
    pub slices: Vec<Range<usize>>,
}

impl EncodedFrame {
    /// Clears the frame for reuse without releasing its buffers.
    pub fn reset(&mut self) {
        self.pts_us = 0;
        self.is_idr = false;
        self.data.clear();
        self.slices.clear();
    }

    /// Appends one NAL unit, writing the start code and recording its range.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::encode::{EncodedFrame, START_CODE};
    /// let mut frame = EncodedFrame::default();
    /// frame.push_nal(&[0x67, 0x42]);
    /// frame.push_nal(&[0x65, 0x88]);
    ///
    /// assert_eq!(frame.slices.len(), 2);
    /// assert_eq!(&frame.data[frame.slices[0].clone()], &[0, 0, 0, 1, 0x67, 0x42]);
    /// assert_eq!(frame.data.len(), 2 * (START_CODE.len() + 2));
    /// ```
    pub fn push_nal(&mut self, nal: &[u8]) {
        let start = self.data.len();
        self.data.extend_from_slice(&START_CODE);
        self.data.extend_from_slice(nal);
        self.slices.push(start..self.data.len());
    }

    /// Returns the bytes of one slice, or `None` if the index is out of range.
    #[must_use]
    pub fn slice(&self, index: usize) -> Option<&[u8]> {
        self.slices
            .get(index)
            .map(|range| &self.data[range.clone()])
    }
}

/// Reason an encoder could not be created or could not encode a frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// The platform encoder rejected the session parameters.
    #[error("could not create the encoder session: {reason} (status {status})")]
    SessionCreate {
        /// What was being attempted.
        reason: &'static str,
        /// Platform status code.
        status: i32,
    },

    /// A session property the low-latency configuration depends on was refused.
    #[error("encoder refused the {property} property (status {status})")]
    Property {
        /// Property that was refused.
        property: &'static str,
        /// Platform status code.
        status: i32,
    },

    /// The encoder failed while encoding a frame.
    #[error("encoding a frame failed (status {status})")]
    Encode {
        /// Platform status code.
        status: i32,
    },

    /// An input buffer could not be obtained or written to.
    #[error("could not prepare an input frame: {reason}")]
    InputBuffer {
        /// What went wrong.
        reason: &'static str,
    },
}
