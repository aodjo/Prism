//! Software H.264 decoding, for a Linux client.
//!
//! The counterpart of [`crate::encode::openh264`]: Cisco's decoder, in this binary, on the
//! processor. What it hands back is a picture in system memory, which the stream window uploads
//! to a texture — one copy the other two clients do not make, and the price of a decoder that
//! is on every Linux machine rather than on the ones whose driver happens to decode.
//!
//! # Buffers that come back
//!
//! A decoded picture is copied out of OpenH264's own buffer, because that one is overwritten by
//! the next frame and the window may still be drawing it. The copy goes into a buffer that was
//! used before: every [`Picture`] carries the way home to the decoder that made it, and giving
//! one up — the window dropping it once it has been drawn, or the queue dropping it because a
//! newer one arrived — sends its buffer back. So the frame path allocates while the first few
//! pictures are in flight and not again, which is the rule every other part of it keeps.

#![cfg(linux_desktop)]

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig};
use openh264::formats::YUVSource;

use crate::decode::{DecodeError, NAL_PPS, NAL_SPS, nal_type, nal_units};
use crate::yuv::I420;

/// What OpenH264 reports a failure with, which carries a message rather than a code.
const FAILED: i32 = -1;

/// A decoded picture, held until whoever is showing it lets go.
pub struct Picture {
    /// Presentation timestamp in microseconds, as supplied to the decoder.
    pub pts_us: u64,
    planes: Option<I420>,
    home: Sender<I420>,
}

impl Picture {
    /// The picture's planes.
    #[must_use]
    pub fn planes(&self) -> &I420 {
        // Present from construction until drop, which is the only thing that takes it.
        self.planes.as_ref().unwrap_or_else(|| unreachable!())
    }

    /// The picture's width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.planes().width()
    }

    /// The picture's height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.planes().height()
    }
}

impl Drop for Picture {
    /// Gives the buffer back to the decoder, or lets it go if the decoder has gone first.
    fn drop(&mut self) {
        if let Some(planes) = self.planes.take() {
            let _ = self.home.send(planes);
        }
    }
}

impl core::fmt::Debug for Picture {
    /// Names the picture, not its bytes.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Picture")
            .field("pts_us", &self.pts_us)
            .field("planes", &self.planes)
            .finish_non_exhaustive()
    }
}

/// An H.264 decoder on the processor.
pub struct SoftwareDecoder {
    decoder: Decoder,
    /// Whether the stream has described itself yet. Until it has, there is nothing to decode.
    described: bool,
    pending: Option<Picture>,
    spare: Receiver<I420>,
    home: Sender<I420>,
    errors: Vec<i32>,
}

impl SoftwareDecoder {
    /// Opens a decoder.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::SessionCreate`] if OpenH264 will not start one.
    pub fn new() -> Result<Self, DecodeError> {
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(|_| DecodeError::SessionCreate {
                reason: "OpenH264 would not start a decoder",
                status: FAILED,
            })?;

        let (home, spare) = channel();

        Ok(Self {
            decoder,
            described: false,
            pending: None,
            spare,
            home,
            errors: Vec::new(),
        })
    }

    /// Decodes one Annex B frame.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::NoParameterSets`] until the stream has carried a sequence and a
    /// picture parameter set — which is the ordinary state of a client that joined between two
    /// keyframes — and [`DecodeError::Decode`] if OpenH264 refuses the frame.
    pub fn decode(&mut self, annexb: &[u8], pts_us: u64) -> Result<(), DecodeError> {
        if !self.described {
            let carries = |wanted| {
                nal_units(annexb)
                    .filter_map(nal_type)
                    .any(|kind| kind == wanted)
            };

            self.described = carries(NAL_SPS) && carries(NAL_PPS);

            if !self.described {
                return Err(DecodeError::NoParameterSets);
            }
        }

        let decoded = match self.decoder.decode(annexb) {
            Ok(Some(decoded)) => decoded,
            Ok(None) => return Ok(()),
            Err(_) => {
                self.errors.push(FAILED);

                return Err(DecodeError::Decode { status: FAILED });
            }
        };

        let planes = copy_out(&decoded, &self.spare);

        // Replacing one that nobody took sends that one home, which is the newest-wins rule the
        // other decoders keep as well: a picture that waited is already too late.
        self.pending = Some(Picture {
            pts_us,
            planes: Some(planes),
            home: self.home.clone(),
        });

        Ok(())
    }

    /// Returns the picture the last frame produced, if it produced one.
    ///
    /// The timeout is accepted for the same shape as the other backends and is not waited on: a
    /// software decoder has finished by the time `decode` returns.
    pub fn poll(&mut self, _timeout: Duration) -> Option<Picture> {
        self.pending.take()
    }

    /// Returns and clears the failures the decoder reported.
    pub fn take_errors(&mut self) -> Vec<i32> {
        core::mem::take(&mut self.errors)
    }
}

impl core::fmt::Debug for SoftwareDecoder {
    /// Describes the decoder without reaching into OpenH264.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SoftwareDecoder")
            .field("described", &self.described)
            .finish_non_exhaustive()
    }
}

/// Copies a decoded picture into a buffer of its size, reusing one that has come back.
///
/// A returned buffer of another size — from before the stream changed resolution — is let go
/// rather than kept, since nothing will ask for that size again.
fn copy_out(decoded: &DecodedYUV<'_>, spare: &Receiver<I420>) -> I420 {
    let (width, height) = decoded.dimensions();
    let (width, height) = (width as u32 & !1, height as u32 & !1);

    let mut planes = spare
        .try_iter()
        .find(|one| (one.width(), one.height()) == (width, height))
        .unwrap_or_else(|| I420::new(width, height));

    let (luma_stride, chroma_stride, _) = decoded.strides();
    let (across, down) = (width as usize, height as usize);
    let (y, u, v) = planes.planes_mut();

    copy_plane(decoded.y(), luma_stride, y, across, down);
    copy_plane(decoded.u(), chroma_stride, u, across / 2, down / 2);
    copy_plane(decoded.v(), chroma_stride, v, across / 2, down / 2);

    planes
}

/// Copies `rows` rows of `across` bytes from a padded plane into an unpadded one.
fn copy_plane(from: &[u8], stride: usize, into: &mut [u8], across: usize, rows: usize) {
    for (row, line) in into.chunks_exact_mut(across).take(rows).enumerate() {
        if let Some(source) = from.get(row * stride..row * stride + across) {
            line.copy_from_slice(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SoftwareDecoder;
    use crate::encode::EncoderConfig;
    use crate::encode::openh264::SoftwareEncoder;
    use crate::net::negotiate::Codec;
    use crate::yuv::{I420, PixelOrder};

    /// A picture with a white left half and a black right half.
    fn halves(width: u32, height: u32) -> I420 {
        let mut image = vec![0u8; width as usize * height as usize * 4];

        for row in image.chunks_exact_mut(width as usize * 4) {
            row[..width as usize * 2].fill(255);
        }

        let mut picture = I420::new(width, height);
        picture.fill_from(&image, width as usize * 4, width, height, PixelOrder::BGRX);

        picture
    }

    #[test]
    fn a_picture_survives_the_round_trip() {
        let (width, height) = (320, 240);
        let mut encoder = SoftwareEncoder::new(EncoderConfig {
            codec: Codec::H264,
            width,
            height,
            fps: 60,
            bitrate_bps: 4_000_000,
            max_slice_bytes: 0,
        })
        .expect("an encoder");
        let mut decoder = SoftwareDecoder::new().expect("a decoder");

        let source = halves(width, height);
        let mut shown = None;

        for index in 0..5u64 {
            let frame = encoder
                .encode(&source, index * 16_666, index == 0)
                .expect("a frame");

            assert!(!frame.slices.is_empty());
            assert_eq!(frame.is_idr, index == 0, "frame {index}");

            let pts = frame.pts_us;
            decoder.decode(&frame.data, pts).expect("decodes");

            if let Some(picture) = decoder.poll(std::time::Duration::ZERO) {
                shown = Some(picture);
            }
        }

        let picture = shown.expect("a picture came out");

        assert_eq!((picture.width(), picture.height()), (width, height));

        // Lossy, so near rather than equal: bright on the left, dark on the right.
        let row = &picture.planes().y()[120 * width as usize..][..width as usize];
        assert!(row[40] > 220, "left is {}", row[40]);
        assert!(row[280] < 30, "right is {}", row[280]);
    }

    #[test]
    fn nothing_decodes_before_the_stream_describes_itself() {
        let mut decoder = SoftwareDecoder::new().expect("a decoder");

        // A lone P slice, as a client that joined mid-stream would first see.
        let slice = [0, 0, 0, 1, 0x41, 0x9a, 0x00];

        assert_eq!(
            decoder.decode(&slice, 0),
            Err(crate::decode::DecodeError::NoParameterSets)
        );
    }

    #[test]
    fn a_buffer_that_is_given_up_is_used_again() {
        let mut encoder = SoftwareEncoder::new(EncoderConfig {
            codec: Codec::H264,
            width: 64,
            height: 64,
            fps: 30,
            bitrate_bps: 500_000,
            max_slice_bytes: 0,
        })
        .expect("an encoder");
        let mut decoder = SoftwareDecoder::new().expect("a decoder");
        let source = halves(64, 64);

        let mut addresses = Vec::new();

        for index in 0..4u64 {
            let frame = encoder.encode(&source, index, index == 0).expect("a frame");
            decoder.decode(&frame.data, index).expect("decodes");

            let picture = decoder.poll(std::time::Duration::ZERO).expect("a picture");
            addresses.push(picture.planes().y().as_ptr() as usize);
        }

        assert!(
            addresses.iter().all(|&at| at == addresses[0]),
            "a picture that was given up was not reused: {addresses:x?}"
        );
    }
}
