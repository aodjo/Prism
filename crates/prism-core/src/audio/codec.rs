//! Opus, wrapped so the rest of the pipeline never sees it.
//!
//! Two types with one job each and no allocation after construction. The buffers are owned and
//! reused, because five milliseconds of audio is two hundred frames a second and a heap
//! allocation on that path is two hundred a second that nothing needs.
//!
//! # Concealment rather than repair
//!
//! A lost audio packet is not retransmitted and not repaired by parity. The decoder is told a
//! frame is missing and it invents one from what it has: the pitch and spectrum of what came
//! before, faded out. For a single frame this is inaudible, which is a better outcome than any
//! repair scheme that costs bandwidth on every frame to fix the occasional one — and it is why
//! [`AudioDecoder::conceal`] exists and is called rather than the gap simply being skipped.

use rusty_opus::{Application, OpusDecoder, OpusEncoder};

use crate::audio::{CHANNELS, FRAME_INTERLEAVED, FRAME_SAMPLES, SAMPLE_RATE};
use crate::net::packet::MAX_AUDIO_PAYLOAD;

/// Reason audio could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The codec refused to start.
    #[error("the audio codec could not be created: {reason}")]
    Unavailable {
        /// What the codec said.
        reason: String,
    },

    /// A frame was not the length the codec is configured for.
    #[error("an audio frame is {FRAME_INTERLEAVED} interleaved samples, got {actual}")]
    WrongFrameSize {
        /// Length that was rejected.
        actual: usize,
    },

    /// The codec failed on a frame.
    ///
    /// Carries what it said, because unlike a network failure this one is a bug somewhere
    /// rather than a condition to expect.
    #[error("the audio codec failed: {reason}")]
    Failed {
        /// What the codec said.
        reason: String,
    },
}

/// Encodes captured audio into Opus packets.
pub struct AudioEncoder {
    encoder: OpusEncoder,
    packet: Box<[u8; MAX_AUDIO_PAYLOAD]>,
}

impl AudioEncoder {
    /// Creates an encoder at the fixed rate, channel count and frame size.
    ///
    /// The application is `Audio` rather than `Voip`: this carries whatever is playing on a
    /// desktop, which is as likely to be music or a game as a voice, and the voice-tuned mode
    /// would spend its bits in the wrong places for all of them.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::Unavailable`] if the codec refuses the configuration, which for a
    /// fixed configuration means a build problem rather than anything a caller did.
    pub fn new(bitrate_bps: u32) -> Result<Self, CodecError> {
        let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, CHANNELS, Application::Audio)
            .map_err(|reason| CodecError::Unavailable {
                reason: reason.to_string(),
            })?;

        encoder.bitrate_bps = bitrate_bps as i32;

        Ok(Self {
            encoder,
            packet: Box::new([0; MAX_AUDIO_PAYLOAD]),
        })
    }

    /// Changes the rate the encoder aims for.
    ///
    /// Audio's share of the link is small and constant next to video's, so this is not a
    /// congestion actuator. It exists so a session on a narrow link can spend less on sound
    /// than one on a fast one, decided once rather than continuously.
    pub fn set_bitrate_bps(&mut self, bitrate_bps: u32) {
        self.encoder.bitrate_bps = bitrate_bps as i32;
    }

    /// Encodes one frame and returns the packet.
    ///
    /// `samples` is interleaved, both channels, exactly [`FRAME_INTERLEAVED`] long.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::WrongFrameSize`] if the frame is the wrong length and
    /// [`CodecError::Failed`] if the codec refuses it.
    pub fn encode(&mut self, samples: &[f32]) -> Result<&[u8], CodecError> {
        if samples.len() != FRAME_INTERLEAVED {
            return Err(CodecError::WrongFrameSize {
                actual: samples.len(),
            });
        }

        let written = self
            .encoder
            .encode(samples, FRAME_SAMPLES, self.packet.as_mut_slice())
            .map_err(|reason| CodecError::Failed {
                reason: reason.to_string(),
            })?;

        Ok(&self.packet[..written])
    }
}

impl core::fmt::Debug for AudioEncoder {
    /// Describes the encoder by what it is aiming for.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioEncoder")
            .field("bitrate_bps", &self.encoder.bitrate_bps)
            .finish_non_exhaustive()
    }
}

/// Decodes Opus packets back into samples.
pub struct AudioDecoder {
    decoder: OpusDecoder,
    samples: Box<[f32; FRAME_INTERLEAVED]>,
    concealed: u64,
}

impl AudioDecoder {
    /// Creates a decoder at the fixed rate and channel count.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::Unavailable`] if the codec refuses the configuration.
    pub fn new() -> Result<Self, CodecError> {
        let decoder = OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS).map_err(|reason| {
            CodecError::Unavailable {
                reason: reason.to_string(),
            }
        })?;

        Ok(Self {
            decoder,
            samples: Box::new([0.0; FRAME_INTERLEAVED]),
            concealed: 0,
        })
    }

    /// Decodes one packet into interleaved samples.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::Failed`] if the packet is not one this decoder can read, which
    /// on a sealed transport means corruption rather than an attack.
    pub fn decode(&mut self, packet: &[u8]) -> Result<&[f32], CodecError> {
        self.decoder
            .decode(packet, FRAME_SAMPLES, self.samples.as_mut_slice())
            .map_err(|reason| CodecError::Failed {
                reason: reason.to_string(),
            })?;

        Ok(self.samples.as_slice())
    }

    /// Invents the frame that did not arrive.
    ///
    /// Called instead of skipping a gap. The decoder continues the pitch and spectrum of what
    /// came before and fades it, which for one frame is inaudible — where a five millisecond
    /// hole is a click that every listener hears.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::Failed`] if the codec cannot conceal, which happens only before
    /// any frame has been decoded and there is nothing to continue from.
    pub fn conceal(&mut self) -> Result<&[f32], CodecError> {
        self.concealed += 1;

        self.decoder
            .decode(&[], FRAME_SAMPLES, self.samples.as_mut_slice())
            .map_err(|reason| CodecError::Failed {
                reason: reason.to_string(),
            })?;

        Ok(self.samples.as_slice())
    }

    /// Returns how many frames have been invented rather than received.
    ///
    /// Worth surfacing: a handful over a session is a normal path, and a steady stream of them
    /// is a link that is dropping audio a listener can hear.
    #[must_use]
    pub fn concealed(&self) -> u64 {
        self.concealed
    }
}

impl core::fmt::Debug for AudioDecoder {
    /// Describes the decoder by how much it has had to invent.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioDecoder")
            .field("concealed", &self.concealed)
            .finish_non_exhaustive()
    }
}
