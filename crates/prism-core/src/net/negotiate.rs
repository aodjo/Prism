//! Agreeing on a codec and a picture, in the handshake that was already happening.
//!
//! Both machines have hardware, and neither has the same hardware. A host with a recent NVIDIA
//! card encodes AV1; one from a few years earlier does not. A client on Apple silicon decodes
//! HEVC in hardware; one on an older Intel Mac may not. Guessing wrong in either direction
//! produces a session that either looks worse than it needed to or does not decode at all.
//!
//! # Why this rides inside the handshake
//!
//! The Noise handshake already carries a payload in each direction and already costs one round
//! trip. Negotiating separately would add a second, in front of every connection, to move
//! twenty bytes. So the client's capabilities travel in the message that opens the session and
//! the host's decision comes back in the message that completes it — by the time the session is
//! live, it is configured.
//!
//! The client's half is encrypted to the host's static key but is not yet forward secret, which
//! is the ordinary property of a first message in this pattern. Nothing here is a secret worth
//! more than the fact that a connection happened.
//!
//! # Why the host decides
//!
//! One side has to, and it is the side that will do the encoding: it knows what its own
//! hardware will accept, and it is the only one that can tell whether a request it cannot meet
//! should be refused or quietly served at something lower. A client that asked for AV1 and gets
//! H.264 has a working session rather than an argument.

/// Codecs a machine may be able to encode or decode.
///
/// A set rather than a preference list. Both sides say what they can do and the host chooses,
/// so an ordering carried on the wire would be a second opinion about a decision that has an
/// owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Codecs(u8);

/// H.264, which every machine this runs on can do.
pub const H264: Codecs = Codecs(1 << 0);

/// HEVC, worth about half the bitrate of H.264 for the same picture.
pub const HEVC: Codecs = Codecs(1 << 1);

/// AV1, which needs an NVIDIA Ada, an AMD RDNA 3, or an Intel Arc to encode.
pub const AV1: Codecs = Codecs(1 << 2);

/// The bits this version defines, so a future one's extra codecs are ignored rather than
/// misread as something this build knows.
const KNOWN: u8 = 0b0000_0111;

impl Codecs {
    /// A set with nothing in it.
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// Adds a codec.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns whether this set has a codec.
    #[must_use]
    pub const fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    /// Returns the codecs both sides can do.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns the best codec in the set, or `None` if it is empty.
    ///
    /// Best means fewest bits for the same picture, which is the order AV1, HEVC, H.264. That
    /// is also the order of how recent the hardware has to be, so the choice is always the most
    /// that both machines can actually do.
    #[must_use]
    pub const fn best(self) -> Option<Codec> {
        if self.has(AV1) {
            Some(Codec::Av1)
        } else if self.has(HEVC) {
            Some(Codec::Hevc)
        } else if self.has(H264) {
            Some(Codec::H264)
        } else {
            None
        }
    }

    /// Reads a set from the wire, dropping bits this version does not define.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & KNOWN)
    }

    /// Returns the bits as they travel.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

/// One codec, chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    /// H.264. The floor, and the one every machine has.
    H264 = 0,
    /// HEVC.
    Hevc = 1,
    /// AV1.
    Av1 = 2,
}

impl Codec {
    /// Reads a codec from the wire.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::H264),
            1 => Some(Self::Hevc),
            2 => Some(Self::Av1),
            _ => None,
        }
    }

    /// Returns the set containing only this codec.
    #[must_use]
    pub const fn as_set(self) -> Codecs {
        match self {
            Self::H264 => H264,
            Self::Hevc => HEVC,
            Self::Av1 => AV1,
        }
    }
}

/// Reason a negotiation message could not be read, or a session could not be agreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NegotiateError {
    /// The message was not the length its version requires.
    #[error("a {kind} message is {expected} bytes, got {actual}")]
    BadLength {
        /// Which message.
        kind: &'static str,
        /// Length that arrived.
        actual: usize,
        /// Length that was required.
        expected: usize,
    },

    /// The caller's buffer cannot hold the message.
    #[error("buffer is {actual} bytes, needs {expected}")]
    BufferTooSmall {
        /// Buffer length supplied.
        actual: usize,
        /// Buffer length required.
        expected: usize,
    },

    /// The peer speaks a revision this build does not.
    #[error("the peer negotiates at revision {theirs}, this build speaks {REVISION}")]
    WrongRevision {
        /// What the peer said.
        theirs: u16,
    },

    /// The host named a codec this version does not define.
    #[error("the host chose codec {byte}, which this build does not know")]
    UnknownCodec {
        /// The byte that was rejected.
        byte: u8,
    },

    /// There is no codec both machines can do.
    ///
    /// Should be impossible: every machine this runs on does H.264, and a client that offered
    /// nothing has said it cannot decode video at all.
    #[error("no codec is available to both machines")]
    NoCommonCodec,
}

/// The revision of this agreement.
///
/// Bumped when the meaning of a field changes, and checked before anything is read: two builds
/// that disagree about what a byte means and carry on anyway produce a session that fails
/// somewhere far from the cause.
pub const REVISION: u16 = 1;

/// Bytes in the client's half.
pub const OFFER_LEN: usize = 12;

/// Bytes in the host's half.
pub const ACCEPT_LEN: usize = 14;

/// What the client can do, sent in the message that opens the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    /// Codecs this machine can decode, in hardware or otherwise.
    pub codecs: Codecs,
    /// The largest picture it will accept.
    ///
    /// Its display, usually. A host streaming more than the client can show is spending
    /// bitrate on pixels that will be thrown away before anybody sees them.
    pub max_width: u16,
    /// The largest picture it will accept.
    pub max_height: u16,
    /// The highest rate it can present.
    pub max_fps: u16,
    /// Whether it wants sound.
    pub audio: bool,
}

impl Offer {
    /// Writes the offer into `out`.
    ///
    /// # Errors
    ///
    /// Returns [`NegotiateError::BufferTooSmall`] if `out` cannot hold it.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, NegotiateError> {
        if out.len() < OFFER_LEN {
            return Err(NegotiateError::BufferTooSmall {
                actual: out.len(),
                expected: OFFER_LEN,
            });
        }

        out[0..2].copy_from_slice(&REVISION.to_le_bytes());
        out[2] = self.codecs.bits();
        out[3] = u8::from(self.audio);
        out[4..6].copy_from_slice(&self.max_width.to_le_bytes());
        out[6..8].copy_from_slice(&self.max_height.to_le_bytes());
        out[8..10].copy_from_slice(&self.max_fps.to_le_bytes());
        // Two bytes held back. A field added later can use them without changing the length,
        // and a build that does not know about it reads zeroes rather than misreading the
        // field after it.
        out[10..12].copy_from_slice(&0u16.to_le_bytes());

        Ok(OFFER_LEN)
    }

    /// Reads an offer.
    ///
    /// # Errors
    ///
    /// Returns [`NegotiateError::BadLength`] for a message of the wrong size and
    /// [`NegotiateError::WrongRevision`] for one this build does not speak.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiateError> {
        if bytes.len() != OFFER_LEN {
            return Err(NegotiateError::BadLength {
                kind: "offer",
                actual: bytes.len(),
                expected: OFFER_LEN,
            });
        }

        let revision = u16::from_le_bytes([bytes[0], bytes[1]]);
        if revision != REVISION {
            return Err(NegotiateError::WrongRevision { theirs: revision });
        }

        Ok(Self {
            codecs: Codecs::from_bits(bytes[2]),
            audio: bytes[3] != 0,
            max_width: u16::from_le_bytes([bytes[4], bytes[5]]),
            max_height: u16::from_le_bytes([bytes[6], bytes[7]]),
            max_fps: u16::from_le_bytes([bytes[8], bytes[9]]),
        })
    }
}

/// What the host decided, sent in the message that completes the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accept {
    /// The codec the stream will use.
    pub codec: Codec,
    /// The picture the stream will carry.
    pub width: u16,
    /// The picture the stream will carry.
    pub height: u16,
    /// The rate it will run at.
    pub fps: u16,
    /// The bitrate it will aim for.
    pub bitrate_bps: u32,
    /// Whether sound will be sent.
    pub audio: bool,
}

impl Accept {
    /// Writes the decision into `out`.
    ///
    /// # Errors
    ///
    /// Returns [`NegotiateError::BufferTooSmall`] if `out` cannot hold it.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, NegotiateError> {
        if out.len() < ACCEPT_LEN {
            return Err(NegotiateError::BufferTooSmall {
                actual: out.len(),
                expected: ACCEPT_LEN,
            });
        }

        out[0..2].copy_from_slice(&REVISION.to_le_bytes());
        out[2] = self.codec as u8;
        out[3] = u8::from(self.audio);
        out[4..6].copy_from_slice(&self.width.to_le_bytes());
        out[6..8].copy_from_slice(&self.height.to_le_bytes());
        out[8..10].copy_from_slice(&self.fps.to_le_bytes());
        out[10..14].copy_from_slice(&self.bitrate_bps.to_le_bytes());

        Ok(ACCEPT_LEN)
    }

    /// Reads a decision.
    ///
    /// # Errors
    ///
    /// Returns [`NegotiateError::BadLength`], [`NegotiateError::WrongRevision`], or
    /// [`NegotiateError::UnknownCodec`] if the host named one this build cannot decode — which
    /// means the host is newer and chose something it should not have.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiateError> {
        if bytes.len() != ACCEPT_LEN {
            return Err(NegotiateError::BadLength {
                kind: "accept",
                actual: bytes.len(),
                expected: ACCEPT_LEN,
            });
        }

        let revision = u16::from_le_bytes([bytes[0], bytes[1]]);
        if revision != REVISION {
            return Err(NegotiateError::WrongRevision { theirs: revision });
        }

        let codec =
            Codec::from_byte(bytes[2]).ok_or(NegotiateError::UnknownCodec { byte: bytes[2] })?;

        Ok(Self {
            codec,
            audio: bytes[3] != 0,
            width: u16::from_le_bytes([bytes[4], bytes[5]]),
            height: u16::from_le_bytes([bytes[6], bytes[7]]),
            fps: u16::from_le_bytes([bytes[8], bytes[9]]),
            bitrate_bps: u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]),
        })
    }
}

/// What a host is able and willing to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostAbility {
    /// Codecs this machine can encode.
    pub codecs: Codecs,
    /// The picture it would send if nothing constrained it — its screen.
    pub width: u16,
    /// The picture it would send if nothing constrained it.
    pub height: u16,
    /// The rate it would run at.
    pub fps: u16,
    /// The bitrate it would aim for.
    pub bitrate_bps: u32,
    /// Whether it can send sound.
    pub audio: bool,
}

/// Decides what the session will be.
///
/// Every dimension is the smaller of what each side wants. A host that sent more pixels than
/// the client can show would be spending bitrate on pixels thrown away before anybody saw
/// them, and one that sent frames faster than the client can present would be spending it on
/// frames nobody sees at all.
///
/// # Errors
///
/// Returns [`NegotiateError::NoCommonCodec`] if the two share none, which means the client
/// offered nothing this host can produce — including, if it comes to it, H.264.
pub fn decide(host: HostAbility, offer: Offer) -> Result<Accept, NegotiateError> {
    let codec = host
        .codecs
        .intersection(offer.codecs)
        .best()
        .ok_or(NegotiateError::NoCommonCodec)?;

    let width = host.width.min(offer.max_width);
    let height = host.height.min(offer.max_height);
    let fps = host.fps.min(offer.max_fps);

    // Even, because every codec here subsamples chroma by two in both directions and an odd
    // dimension has no representation. Taking the minimum first and rounding after means a
    // client asking for an odd size gets the even one below it rather than a refusal.
    Ok(Accept {
        codec,
        width: width & !1,
        height: height & !1,
        fps: fps.max(1),
        bitrate_bps: host.bitrate_bps,
        audio: host.audio && offer.audio,
    })
}
