//! Prism wire format.
//!
//! This module is the Rust half of a contract shared with the TypeScript control
//! plane. Both sides are pinned by `packages/protocol/vectors.json`; changing a layout
//! starts by editing the vectors, then updating this module and
//! `packages/protocol/src/packet.ts` together.
//!
//! All multi-byte fields are little-endian. Encoding writes into a caller-supplied
//! buffer and decoding borrows from the input, so nothing on the frame path allocates.

use thiserror::Error;

/// Wire format revision. Bumped on any incompatible layout change.
pub const FORMAT_VERSION: u32 = 1;

/// Maximum UDP payload in bytes, held under the safe PMTU floor so packets never fragment.
pub const MAX_PACKET_SIZE: usize = 1200;

/// Byte length of a video packet header, including the leading channel tag.
pub const VIDEO_HEADER_LEN: usize = 20;

/// Largest slice fragment that fits in one video packet.
pub const MAX_VIDEO_PAYLOAD: usize = MAX_PACKET_SIZE - VIDEO_HEADER_LEN;

/// Exact byte length of a feedback packet; it carries no variable-length payload.
pub const FEEDBACK_PACKET_LEN: usize = 17;

/// Byte length of a control packet header: the channel tag and the message type.
pub const CONTROL_HEADER_LEN: usize = 2;

/// Exact byte length of a clock synchronisation ping.
pub const CLOCK_PING_LEN: usize = 10;

/// Exact byte length of a clock synchronisation pong.
pub const CLOCK_PONG_LEN: usize = 26;

/// Exact byte length of an input event packet.
pub const INPUT_PACKET_LEN: usize = 15;

/// Exact byte length of a cursor position message.
pub const CURSOR_POSITION_LEN: usize = 18;

/// Reserved video flag bits; any packet setting one of these is rejected.
pub const VIDEO_FLAGS_RESERVED_MASK: u8 = 0xf8;

/// Slice belongs to an IDR frame.
pub const FLAG_IDR: u8 = 0x01;

/// Slice is the last one of its frame.
pub const FLAG_LAST_OF_FRAME: u8 = 0x02;

/// Frame is marked as a long-term reference.
pub const FLAG_LTR_REF: u8 = 0x04;

/// Channel tag carried in the first byte of every packet.
///
/// A single UDP flow multiplexes all five channels. The tag is read before anything
/// else and decides which decoder handles the remaining bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Channel {
    /// Session setup, codec negotiation, and cursor updates.
    Control = 0,
    /// Encoded video slices, host to client.
    Video = 1,
    /// Opus frames, host to client.
    Audio = 2,
    /// Keyboard, mouse, and gamepad events, client to host.
    Input = 3,
    /// Frame acknowledgements and clock sync samples, client to host.
    Feedback = 4,
}

impl TryFrom<u8> for Channel {
    type Error = ProtocolError;

    /// Converts a raw tag byte into a [`Channel`].
    ///
    /// Unknown tags are rejected rather than ignored, so a channel added in a future
    /// revision can never be silently misrouted into an existing decoder.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::UnknownChannel`] if the tag is not a defined channel.
    fn try_from(tag: u8) -> Result<Self, Self::Error> {
        match tag {
            0 => Ok(Channel::Control),
            1 => Ok(Channel::Video),
            2 => Ok(Channel::Audio),
            3 => Ok(Channel::Input),
            4 => Ok(Channel::Feedback),
            other => Err(ProtocolError::UnknownChannel(other)),
        }
    }
}

/// Message type carried in the second byte of a control packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlType {
    /// Client's half of a clock synchronisation exchange.
    ClockPing = 0,
    /// Host's answer, carrying both of its own timestamps.
    ClockPong = 1,
    /// Where the host's pointer is, so the client can draw the cursor itself.
    CursorPosition = 2,
}

impl TryFrom<u8> for ControlType {
    type Error = ProtocolError;

    /// Converts a raw type byte into a [`ControlType`].
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::UnknownControlType`] for a type this build does not know,
    /// which is how a message added in a future revision is refused rather than misread.
    fn try_from(tag: u8) -> Result<Self, Self::Error> {
        match tag {
            0 => Ok(ControlType::ClockPing),
            1 => Ok(ControlType::ClockPong),
            2 => Ok(ControlType::CursorPosition),
            other => Err(ProtocolError::UnknownControlType(other)),
        }
    }
}

/// Reason a byte sequence was rejected as malformed.
///
/// Decoders return this instead of a partially populated packet, so a corrupt or
/// hostile datagram can never reach the pipeline as if it were valid. Callers on the
/// receive path count the error and drop the packet.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtocolError {
    /// Packet had no bytes at all, so not even the channel tag could be read.
    #[error("packet is empty, no channel tag")]
    Empty,

    /// Leading byte was not one of the defined channel tags.
    #[error("unknown channel tag {0}")]
    UnknownChannel(u8),

    /// Control message type was not one this build knows.
    #[error("unknown control type {0}")]
    UnknownControlType(u8),

    /// Input event kind was not one this build knows.
    #[error("unknown input kind {0}")]
    UnknownInputKind(u8),

    /// Pointer button index was not one this build knows.
    #[error("unknown mouse button {0}")]
    UnknownMouseButton(i16),

    /// Packet was routed to a decoder for a different channel.
    #[error("expected channel {expected:?}, got tag {got}")]
    WrongChannel {
        /// Channel the decoder handles.
        expected: Channel,
        /// Tag actually found in the packet.
        got: u8,
    },

    /// Packet was shorter than the fixed header it claims to carry.
    #[error("packet is {actual} bytes, needs at least {needed}")]
    TooShort {
        /// Bytes actually present.
        actual: usize,
        /// Bytes the layout requires.
        needed: usize,
    },

    /// Fixed-size packet did not have exactly the required length.
    #[error("packet is {actual} bytes, expected exactly {expected}")]
    WrongLength {
        /// Bytes actually present.
        actual: usize,
        /// Bytes the layout requires.
        expected: usize,
    },

    /// A reserved flag bit was set, meaning the sender speaks a format this build does not.
    #[error("reserved video flag bits set in {0:#04x}")]
    ReservedFlags(u8),

    /// Slice fragment exceeded what one packet can carry.
    #[error("payload is {actual} bytes, exceeds MAX_VIDEO_PAYLOAD of {MAX_VIDEO_PAYLOAD}")]
    PayloadTooLarge {
        /// Payload length that was rejected.
        actual: usize,
    },

    /// Cursor position claimed a screen with no area.
    ///
    /// The client divides by these to place the cursor, so a zero would either crash it or
    /// silently put the cursor nowhere. A screen of no pixels is not a thing that exists.
    #[error("cursor position claims a {width}x{height} screen")]
    EmptyScreen {
        /// Width the sender claimed.
        width: u16,
        /// Height the sender claimed.
        height: u16,
    },

    /// Caller-supplied encode buffer was too small for the packet.
    #[error("buffer is {actual} bytes, needs {needed}")]
    BufferTooSmall {
        /// Buffer length that was supplied.
        actual: usize,
        /// Buffer length required.
        needed: usize,
    },
}

/// One fragment of an encoded video slice, as carried on [`Channel::Video`].
///
/// A frame is split into slices by the encoder and each slice into packets of at most
/// [`MAX_VIDEO_PAYLOAD`] bytes. `capture_ts_us` is stamped once per frame and copied
/// into every packet of that frame, which is what anchors the end-to-end latency chain.
///
/// The payload borrows from the buffer it was decoded out of, so a decoded packet
/// cannot outlive the reassembly buffer that owns its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket<'a> {
    /// Monotonic frame counter, wraps at `u32::MAX`.
    pub frame_id: u32,
    /// Slice index within the frame.
    pub slice_id: u16,
    /// Packet index within the slice.
    pub pkt_idx: u16,
    /// Total packets making up this slice.
    pub pkt_count: u16,
    /// Bit flags; see [`FLAG_IDR`], [`FLAG_LAST_OF_FRAME`], and [`FLAG_LTR_REF`].
    pub flags: u8,
    /// Host clock at capture time, in microseconds.
    pub capture_ts_us: u64,
    /// Slice fragment carried by this packet.
    pub payload: &'a [u8],
}

impl<'a> VideoPacket<'a> {
    /// Returns the number of bytes [`Self::encode_into`] will write.
    ///
    /// Callers size their send buffer with this before encoding, which is why encoding
    /// itself never needs to allocate.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{VideoPacket, VIDEO_HEADER_LEN};
    /// let packet = VideoPacket {
    ///     frame_id: 1, slice_id: 0, pkt_idx: 0, pkt_count: 1,
    ///     flags: 0, capture_ts_us: 0, payload: &[1, 2, 3],
    /// };
    /// assert_eq!(packet.encoded_len(), VIDEO_HEADER_LEN + 3);
    /// ```
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        VIDEO_HEADER_LEN + self.payload.len()
    }

    /// Serialises this packet into `buf` and returns how many bytes were written.
    ///
    /// Writes the 20-byte header followed by the payload, all little-endian. Nothing is
    /// allocated; the caller owns the buffer and reuses it across frames.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::ReservedFlags`] if a reserved flag bit is set,
    /// [`ProtocolError::PayloadTooLarge`] if the payload exceeds [`MAX_VIDEO_PAYLOAD`],
    /// or [`ProtocolError::BufferTooSmall`] if `buf` cannot hold the packet.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{VideoPacket, MAX_PACKET_SIZE};
    /// let packet = VideoPacket {
    ///     frame_id: 42, slice_id: 0, pkt_idx: 0, pkt_count: 1,
    ///     flags: 0x03, capture_ts_us: 1_108_152_157_446, payload: &[0xaa],
    /// };
    /// let mut buf = [0u8; MAX_PACKET_SIZE];
    /// assert_eq!(packet.encode_into(&mut buf).unwrap(), 21);
    /// ```
    pub fn encode_into(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        if self.flags & VIDEO_FLAGS_RESERVED_MASK != 0 {
            return Err(ProtocolError::ReservedFlags(self.flags));
        }

        if self.payload.len() > MAX_VIDEO_PAYLOAD {
            return Err(ProtocolError::PayloadTooLarge {
                actual: self.payload.len(),
            });
        }

        let needed = self.encoded_len();
        if buf.len() < needed {
            return Err(ProtocolError::BufferTooSmall {
                actual: buf.len(),
                needed,
            });
        }

        buf[0] = Channel::Video as u8;
        buf[1..5].copy_from_slice(&self.frame_id.to_le_bytes());
        buf[5..7].copy_from_slice(&self.slice_id.to_le_bytes());
        buf[7..9].copy_from_slice(&self.pkt_idx.to_le_bytes());
        buf[9..11].copy_from_slice(&self.pkt_count.to_le_bytes());
        buf[11] = self.flags;
        buf[12..20].copy_from_slice(&self.capture_ts_us.to_le_bytes());
        buf[VIDEO_HEADER_LEN..needed].copy_from_slice(self.payload);

        Ok(needed)
    }

    /// Parses a video packet, validating every field before returning it.
    ///
    /// The returned payload borrows from `bytes` rather than copying, which is what
    /// keeps the receive path allocation-free.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Empty`] for a zero-length input,
    /// [`ProtocolError::WrongChannel`] if the tag is not [`Channel::Video`],
    /// [`ProtocolError::TooShort`] if fewer than [`VIDEO_HEADER_LEN`] bytes are present,
    /// or [`ProtocolError::ReservedFlags`] if a reserved flag bit is set.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::VideoPacket;
    /// let bytes = [1, 42, 0, 0, 0, 0, 0, 0, 0, 1, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0xaa];
    /// let packet = VideoPacket::decode(&bytes).unwrap();
    /// assert_eq!(packet.frame_id, 42);
    /// assert_eq!(packet.payload, &[0xaa]);
    /// ```
    pub fn decode(bytes: &'a [u8]) -> Result<Self, ProtocolError> {
        let channel = channel_of(bytes)?;
        if channel != Channel::Video {
            return Err(ProtocolError::WrongChannel {
                expected: Channel::Video,
                got: bytes[0],
            });
        }

        if bytes.len() < VIDEO_HEADER_LEN {
            return Err(ProtocolError::TooShort {
                actual: bytes.len(),
                needed: VIDEO_HEADER_LEN,
            });
        }

        let flags = bytes[11];
        if flags & VIDEO_FLAGS_RESERVED_MASK != 0 {
            return Err(ProtocolError::ReservedFlags(flags));
        }

        Ok(Self {
            frame_id: u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]),
            slice_id: u16::from_le_bytes([bytes[5], bytes[6]]),
            pkt_idx: u16::from_le_bytes([bytes[7], bytes[8]]),
            pkt_count: u16::from_le_bytes([bytes[9], bytes[10]]),
            flags,
            capture_ts_us: u64::from_le_bytes([
                bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18],
                bytes[19],
            ]),
            payload: &bytes[VIDEO_HEADER_LEN..],
        })
    }
}

/// Client-to-host receive report, as carried on [`Channel::Feedback`].
///
/// `recv_bitmap` drives long-term-reference invalidation on the encoder: the host
/// encodes against the newest frame the client has confirmed, so packet loss never
/// forces an IDR and never produces a visible hitch. `client_ts_us` doubles as the
/// clock-sync sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackPacket {
    /// Highest frame the client has fully reassembled.
    pub last_frame_id: u32,
    /// Bit `n` set means frame `last_frame_id - 1 - n` was also received.
    pub recv_bitmap: u32,
    /// Client clock when the report was produced, in microseconds.
    pub client_ts_us: u64,
}

impl FeedbackPacket {
    /// Serialises this report into `buf` and returns how many bytes were written.
    ///
    /// Feedback is sent for every received frame and is the highest-priority traffic on
    /// the return path; it is never batched, because a late acknowledgement stalls the
    /// encoder's long-term reference selection and costs a frame.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::BufferTooSmall`] if `buf` is shorter than
    /// [`FEEDBACK_PACKET_LEN`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{FeedbackPacket, FEEDBACK_PACKET_LEN};
    /// let report = FeedbackPacket { last_frame_id: 256, recv_bitmap: 0xffff_fff0, client_ts_us: 1_000_000 };
    /// let mut buf = [0u8; FEEDBACK_PACKET_LEN];
    /// assert_eq!(report.encode_into(&mut buf).unwrap(), FEEDBACK_PACKET_LEN);
    /// ```
    pub fn encode_into(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        if buf.len() < FEEDBACK_PACKET_LEN {
            return Err(ProtocolError::BufferTooSmall {
                actual: buf.len(),
                needed: FEEDBACK_PACKET_LEN,
            });
        }

        buf[0] = Channel::Feedback as u8;
        buf[1..5].copy_from_slice(&self.last_frame_id.to_le_bytes());
        buf[5..9].copy_from_slice(&self.recv_bitmap.to_le_bytes());
        buf[9..17].copy_from_slice(&self.client_ts_us.to_le_bytes());

        Ok(FEEDBACK_PACKET_LEN)
    }

    /// Parses a feedback packet, requiring an exact length match.
    ///
    /// Unlike video packets, feedback carries no variable-length payload, so anything
    /// other than exactly [`FEEDBACK_PACKET_LEN`] bytes indicates corruption or a
    /// version mismatch and is rejected outright.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Empty`] for a zero-length input,
    /// [`ProtocolError::WrongChannel`] if the tag is not [`Channel::Feedback`], or
    /// [`ProtocolError::WrongLength`] if the length is not exactly
    /// [`FEEDBACK_PACKET_LEN`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::FeedbackPacket;
    /// let bytes = [4, 0, 1, 0, 0, 0xf0, 0xff, 0xff, 0xff, 0x40, 0x42, 0x0f, 0, 0, 0, 0, 0];
    /// let report = FeedbackPacket::decode(&bytes).unwrap();
    /// assert_eq!(report.last_frame_id, 256);
    /// ```
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let channel = channel_of(bytes)?;
        if channel != Channel::Feedback {
            return Err(ProtocolError::WrongChannel {
                expected: Channel::Feedback,
                got: bytes[0],
            });
        }

        if bytes.len() != FEEDBACK_PACKET_LEN {
            return Err(ProtocolError::WrongLength {
                actual: bytes.len(),
                expected: FEEDBACK_PACKET_LEN,
            });
        }

        Ok(Self {
            last_frame_id: u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]),
            recv_bitmap: u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]),
            client_ts_us: u64::from_le_bytes([
                bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
                bytes[16],
            ]),
        })
    }
}

/// Reads the channel tag from the first byte of a packet.
///
/// This is the only field that may be read before validation, and it decides which
/// decoder handles the rest of the bytes.
///
/// # Errors
///
/// Returns [`ProtocolError::Empty`] if the packet has no bytes, or
/// [`ProtocolError::UnknownChannel`] if the tag is not a defined channel.
///
/// # Examples
///
/// ```
/// # use prism_core::net::packet::{channel_of, Channel};
/// assert_eq!(channel_of(&[1, 0, 0]).unwrap(), Channel::Video);
/// assert!(channel_of(&[]).is_err());
/// ```
pub fn channel_of(bytes: &[u8]) -> Result<Channel, ProtocolError> {
    let &tag = bytes.first().ok_or(ProtocolError::Empty)?;
    Channel::try_from(tag)
}

/// The client's half of a clock synchronisation exchange.
///
/// Sent on [`Channel::Control`]. The host answers with a [`ClockPong`] carrying both of
/// its own timestamps, from which the client derives the offset between the two clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPing {
    /// Client clock when the ping was sent, in microseconds.
    pub t1_us: u64,
}

impl ClockPing {
    /// Serialises this ping into `buf` and returns how many bytes were written.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::BufferTooSmall`] if `buf` is shorter than
    /// [`CLOCK_PING_LEN`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{CLOCK_PING_LEN, ClockPing};
    /// let mut buf = [0u8; CLOCK_PING_LEN];
    /// assert_eq!(ClockPing { t1_us: 1_000_000 }.encode_into(&mut buf).unwrap(), CLOCK_PING_LEN);
    /// ```
    pub fn encode_into(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        if buf.len() < CLOCK_PING_LEN {
            return Err(ProtocolError::BufferTooSmall {
                actual: buf.len(),
                needed: CLOCK_PING_LEN,
            });
        }

        buf[0] = Channel::Control as u8;
        buf[1] = ControlType::ClockPing as u8;
        buf[2..10].copy_from_slice(&self.t1_us.to_le_bytes());

        Ok(CLOCK_PING_LEN)
    }

    /// Parses a clock ping, requiring an exact length match.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::WrongChannel`], [`ProtocolError::UnknownControlType`], or
    /// [`ProtocolError::WrongLength`] as appropriate.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::ClockPing;
    /// let bytes = [0, 0, 0x40, 0x42, 0x0f, 0, 0, 0, 0, 0];
    /// assert_eq!(ClockPing::decode(&bytes).unwrap().t1_us, 1_000_000);
    /// ```
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        expect_control(bytes, ControlType::ClockPing, CLOCK_PING_LEN)?;

        Ok(Self {
            t1_us: read_u64(bytes, 2),
        })
    }
}

/// The host's answer to a [`ClockPing`].
///
/// Carries the ping's own timestamp back so the client can pair the reply, plus the two
/// host timestamps that bracket the host's handling of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPong {
    /// Echoed from the ping, in client time.
    pub t1_us: u64,
    /// Host clock when the ping arrived, in microseconds.
    pub t2_us: u64,
    /// Host clock when this answer was sent, in microseconds.
    pub t3_us: u64,
}

impl ClockPong {
    /// Serialises this answer into `buf` and returns how many bytes were written.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::BufferTooSmall`] if `buf` is shorter than
    /// [`CLOCK_PONG_LEN`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{CLOCK_PONG_LEN, ClockPong};
    /// let pong = ClockPong { t1_us: 1_000_000, t2_us: 1_000_500, t3_us: 1_000_600 };
    /// let mut buf = [0u8; CLOCK_PONG_LEN];
    /// assert_eq!(pong.encode_into(&mut buf).unwrap(), CLOCK_PONG_LEN);
    /// ```
    pub fn encode_into(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        if buf.len() < CLOCK_PONG_LEN {
            return Err(ProtocolError::BufferTooSmall {
                actual: buf.len(),
                needed: CLOCK_PONG_LEN,
            });
        }

        buf[0] = Channel::Control as u8;
        buf[1] = ControlType::ClockPong as u8;
        buf[2..10].copy_from_slice(&self.t1_us.to_le_bytes());
        buf[10..18].copy_from_slice(&self.t2_us.to_le_bytes());
        buf[18..26].copy_from_slice(&self.t3_us.to_le_bytes());

        Ok(CLOCK_PONG_LEN)
    }

    /// Parses a clock pong, requiring an exact length match.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::WrongChannel`], [`ProtocolError::UnknownControlType`], or
    /// [`ProtocolError::WrongLength`] as appropriate.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::ClockPong;
    /// # use prism_core::net::packet::CLOCK_PONG_LEN;
    /// let pong = ClockPong { t1_us: 5, t2_us: 6, t3_us: 7 };
    /// let mut buf = [0u8; CLOCK_PONG_LEN];
    /// pong.encode_into(&mut buf).unwrap();
    /// assert_eq!(ClockPong::decode(&buf).unwrap(), pong);
    /// ```
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        expect_control(bytes, ControlType::ClockPong, CLOCK_PONG_LEN)?;

        Ok(Self {
            t1_us: read_u64(bytes, 2),
            t2_us: read_u64(bytes, 10),
            t3_us: read_u64(bytes, 18),
        })
    }
}

/// Where the host's pointer is, as carried on [`Channel::Control`].
///
/// The host keeps the cursor out of the captured video, so the client has to draw it. That
/// is the point: a cursor baked into the frames inherits the whole video latency, while one
/// drawn by the client answers the hand holding the mouse immediately and is corrected by
/// these messages as they arrive.
///
/// The screen size travels with every message rather than being negotiated once. It is four
/// bytes against a packet already this small, and it means a client that joins late, or
/// misses the message where the host changed resolution, is never left scaling against a
/// screen that no longer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    /// Host clock when the pointer was read, in microseconds.
    ///
    /// This is what makes the message usable for correction rather than only display: the
    /// client stamps the motion it sends in the same clock, so it can tell which of its own
    /// movements this reading already accounts for.
    pub sample_ts_us: u64,
    /// Pixels from the left of the host's primary display.
    pub x: u16,
    /// Pixels from the top of the host's primary display.
    pub y: u16,
    /// Width of the host's primary display in pixels; never zero.
    pub screen_width: u16,
    /// Height of the host's primary display in pixels; never zero.
    pub screen_height: u16,
}

impl CursorPosition {
    /// Serialises this reading into `buf` and returns how many bytes were written.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::BufferTooSmall`] if `buf` is shorter than
    /// [`CURSOR_POSITION_LEN`], and [`ProtocolError::EmptyScreen`] if either dimension is
    /// zero, so a bad reading is caught where it is made rather than on the far side.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{CURSOR_POSITION_LEN, CursorPosition};
    /// let cursor = CursorPosition {
    ///     sample_ts_us: 1_000_000,
    ///     x: 800,
    ///     y: 450,
    ///     screen_width: 2560,
    ///     screen_height: 1440,
    /// };
    /// let mut buf = [0u8; CURSOR_POSITION_LEN];
    /// assert_eq!(cursor.encode_into(&mut buf).unwrap(), CURSOR_POSITION_LEN);
    /// ```
    pub fn encode_into(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        if buf.len() < CURSOR_POSITION_LEN {
            return Err(ProtocolError::BufferTooSmall {
                actual: buf.len(),
                needed: CURSOR_POSITION_LEN,
            });
        }

        if self.screen_width == 0 || self.screen_height == 0 {
            return Err(ProtocolError::EmptyScreen {
                width: self.screen_width,
                height: self.screen_height,
            });
        }

        buf[0] = Channel::Control as u8;
        buf[1] = ControlType::CursorPosition as u8;
        buf[2..10].copy_from_slice(&self.sample_ts_us.to_le_bytes());
        buf[10..12].copy_from_slice(&self.x.to_le_bytes());
        buf[12..14].copy_from_slice(&self.y.to_le_bytes());
        buf[14..16].copy_from_slice(&self.screen_width.to_le_bytes());
        buf[16..18].copy_from_slice(&self.screen_height.to_le_bytes());

        Ok(CURSOR_POSITION_LEN)
    }

    /// Parses a cursor position, requiring an exact length match and a screen with area.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::WrongChannel`], [`ProtocolError::UnknownControlType`],
    /// [`ProtocolError::WrongLength`], or [`ProtocolError::EmptyScreen`] as appropriate.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{CURSOR_POSITION_LEN, CursorPosition};
    /// let cursor = CursorPosition {
    ///     sample_ts_us: 5,
    ///     x: 1,
    ///     y: 2,
    ///     screen_width: 3,
    ///     screen_height: 4,
    /// };
    /// let mut buf = [0u8; CURSOR_POSITION_LEN];
    /// cursor.encode_into(&mut buf).unwrap();
    /// assert_eq!(CursorPosition::decode(&buf).unwrap(), cursor);
    /// ```
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        expect_control(bytes, ControlType::CursorPosition, CURSOR_POSITION_LEN)?;

        let screen_width = read_u16(bytes, 14);
        let screen_height = read_u16(bytes, 16);

        if screen_width == 0 || screen_height == 0 {
            return Err(ProtocolError::EmptyScreen {
                width: screen_width,
                height: screen_height,
            });
        }

        Ok(Self {
            sample_ts_us: read_u64(bytes, 2),
            x: read_u16(bytes, 10),
            y: read_u16(bytes, 12),
            screen_width,
            screen_height,
        })
    }
}

/// Reads the control message type from a control packet.
///
/// # Errors
///
/// Returns [`ProtocolError::Empty`] for a zero-length input,
/// [`ProtocolError::WrongChannel`] if the tag is not [`Channel::Control`],
/// [`ProtocolError::TooShort`] if the type byte is missing, and
/// [`ProtocolError::UnknownControlType`] for a type this build does not know.
///
/// # Examples
///
/// ```
/// # use prism_core::net::packet::{ControlType, control_type_of};
/// assert_eq!(control_type_of(&[0, 1]).unwrap(), ControlType::ClockPong);
/// assert!(control_type_of(&[0]).is_err());
/// ```
pub fn control_type_of(bytes: &[u8]) -> Result<ControlType, ProtocolError> {
    let channel = channel_of(bytes)?;
    if channel != Channel::Control {
        return Err(ProtocolError::WrongChannel {
            expected: Channel::Control,
            got: bytes[0],
        });
    }

    if bytes.len() < CONTROL_HEADER_LEN {
        return Err(ProtocolError::TooShort {
            actual: bytes.len(),
            needed: CONTROL_HEADER_LEN,
        });
    }

    ControlType::try_from(bytes[1])
}

/// Checks that a control packet has the expected type and exact length.
///
/// # Errors
///
/// Returns whatever [`control_type_of`] rejects, [`ProtocolError::UnknownControlType`] if
/// the type is not the one expected, or [`ProtocolError::WrongLength`] on a size mismatch.
fn expect_control(bytes: &[u8], expected: ControlType, length: usize) -> Result<(), ProtocolError> {
    let actual = control_type_of(bytes)?;
    if actual != expected {
        return Err(ProtocolError::UnknownControlType(bytes[1]));
    }

    if bytes.len() != length {
        return Err(ProtocolError::WrongLength {
            actual: bytes.len(),
            expected: length,
        });
    }

    Ok(())
}

/// Reads a little-endian `u64` at `offset`.
///
/// # Panics
///
/// Panics if `bytes` is shorter than `offset + 8`; callers validate the length first.
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("length was validated"),
    )
}

/// Reads a little-endian `u16` at `offset`.
///
/// # Panics
///
/// Panics if `bytes` is shorter than `offset + 2`; callers validate the length first.
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("length was validated"),
    )
}

/// Reads a little-endian `i16` at `offset`.
///
/// # Panics
///
/// Panics if `bytes` is shorter than `offset + 2`; callers validate the length first.
fn read_i16(bytes: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("length was validated"),
    )
}

/// What an input packet describes.
///
/// The wire layout is one fixed size for all four, with the two coordinate fields
/// reinterpreted per kind. A tagged union with per-kind lengths would save a few bytes on
/// a packet that is already tiny, at the cost of a decoder that has to branch before it
/// knows how much to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InputKind {
    /// Pointer motion, in device units relative to the last report.
    MouseMove = 0,
    /// A pointer button going down or coming up.
    MouseButton = 1,
    /// Scroll wheel motion.
    MouseScroll = 2,
    /// A key going down or coming up.
    Key = 3,
}

impl TryFrom<u8> for InputKind {
    type Error = ProtocolError;

    /// Converts a raw kind byte into an [`InputKind`].
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::UnknownInputKind`] for a kind this build does not know.
    fn try_from(tag: u8) -> Result<Self, Self::Error> {
        match tag {
            0 => Ok(InputKind::MouseMove),
            1 => Ok(InputKind::MouseButton),
            2 => Ok(InputKind::MouseScroll),
            3 => Ok(InputKind::Key),
            other => Err(ProtocolError::UnknownInputKind(other)),
        }
    }
}

/// Which pointer button an event refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MouseButton {
    /// The primary button.
    Left = 0,
    /// The secondary button.
    Right = 1,
    /// The wheel button.
    Middle = 2,
}

impl TryFrom<i16> for MouseButton {
    type Error = ProtocolError;

    /// Converts a raw button index into a [`MouseButton`].
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::UnknownMouseButton`] for a button this build does not know.
    fn try_from(index: i16) -> Result<Self, Self::Error> {
        match index {
            0 => Ok(MouseButton::Left),
            1 => Ok(MouseButton::Right),
            2 => Ok(MouseButton::Middle),
            other => Err(ProtocolError::UnknownMouseButton(other)),
        }
    }
}

/// One input event, as carried on [`Channel::Input`].
///
/// Motion is relative rather than absolute because that is what a captured pointer
/// produces and what a game reads: an absolute position would have to be scaled between
/// two different screen sizes and would lose precision doing it.
///
/// Keys are identified by USB HID usage code. Both Windows and macOS have their own
/// keyboard numbering and neither is portable, but both can be mapped from HID, which is
/// also what the client's input library reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Pointer motion relative to the last report.
    MouseMove {
        /// Horizontal movement; positive is right.
        dx: i16,
        /// Vertical movement; positive is down.
        dy: i16,
    },
    /// A pointer button changing state.
    MouseButton {
        /// Which button changed.
        button: MouseButton,
        /// Whether it is now down.
        pressed: bool,
    },
    /// Scroll wheel motion.
    MouseScroll {
        /// Horizontal scroll; positive is right.
        dx: i16,
        /// Vertical scroll; positive is down.
        dy: i16,
    },
    /// A key changing state.
    Key {
        /// USB HID usage code for the key.
        usage: u16,
        /// Whether it is now down.
        pressed: bool,
    },
}

/// An input event with the time it happened.
///
/// The timestamp is what makes the input path measurable. Without it the only thing that
/// can be observed about input is whether it arrived, and this is the one part of the
/// pipeline where a few milliseconds are felt directly rather than seen.
///
/// It is carried **in the host's clock**, converted by the client before sending. The
/// client is the side that measures the offset between the two, so it is the side that
/// can do the conversion; a host receiving a raw client timestamp could only compare it
/// against its own clock and get the offset back as latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPacket {
    /// When the event happened, in the host's clock, in microseconds.
    pub origin_ts_us: u64,
    /// What happened.
    pub event: InputEvent,
}

impl InputPacket {
    /// Serialises this event into `buf` and returns how many bytes were written.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::BufferTooSmall`] if `buf` is shorter than
    /// [`INPUT_PACKET_LEN`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{INPUT_PACKET_LEN, InputEvent, InputPacket};
    /// let packet = InputPacket {
    ///     origin_ts_us: 1_000_000,
    ///     event: InputEvent::MouseMove { dx: -5, dy: 10 },
    /// };
    /// let mut buf = [0u8; INPUT_PACKET_LEN];
    /// assert_eq!(packet.encode_into(&mut buf).unwrap(), INPUT_PACKET_LEN);
    /// ```
    pub fn encode_into(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        if buf.len() < INPUT_PACKET_LEN {
            return Err(ProtocolError::BufferTooSmall {
                actual: buf.len(),
                needed: INPUT_PACKET_LEN,
            });
        }

        let (kind, x, y, flags) = match self.event {
            InputEvent::MouseMove { dx, dy } => (InputKind::MouseMove, dx, dy, 0),
            InputEvent::MouseScroll { dx, dy } => (InputKind::MouseScroll, dx, dy, 0),
            InputEvent::MouseButton { button, pressed } => {
                (InputKind::MouseButton, button as i16, 0, u8::from(pressed))
            }
            InputEvent::Key { usage, pressed } => {
                (InputKind::Key, usage as i16, 0, u8::from(pressed))
            }
        };

        buf[0] = Channel::Input as u8;
        buf[1] = kind as u8;
        buf[2..10].copy_from_slice(&self.origin_ts_us.to_le_bytes());
        buf[10..12].copy_from_slice(&x.to_le_bytes());
        buf[12..14].copy_from_slice(&y.to_le_bytes());
        buf[14] = flags;

        Ok(INPUT_PACKET_LEN)
    }

    /// Parses an input event, requiring an exact length match.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::WrongChannel`] if the tag is not [`Channel::Input`],
    /// [`ProtocolError::WrongLength`] on a size mismatch,
    /// [`ProtocolError::UnknownInputKind`] for an unrecognised kind, and
    /// [`ProtocolError::UnknownMouseButton`] for an unrecognised button.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::{INPUT_PACKET_LEN, InputEvent, InputPacket};
    /// let packet = InputPacket {
    ///     origin_ts_us: 7,
    ///     event: InputEvent::Key { usage: 0x04, pressed: true },
    /// };
    /// let mut buf = [0u8; INPUT_PACKET_LEN];
    /// packet.encode_into(&mut buf).unwrap();
    /// assert_eq!(InputPacket::decode(&buf).unwrap(), packet);
    /// ```
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let channel = channel_of(bytes)?;
        if channel != Channel::Input {
            return Err(ProtocolError::WrongChannel {
                expected: Channel::Input,
                got: bytes[0],
            });
        }

        if bytes.len() != INPUT_PACKET_LEN {
            return Err(ProtocolError::WrongLength {
                actual: bytes.len(),
                expected: INPUT_PACKET_LEN,
            });
        }

        let kind = InputKind::try_from(bytes[1])?;
        let x = read_i16(bytes, 10);
        let y = read_i16(bytes, 12);
        let pressed = bytes[14] & 1 != 0;

        let event = match kind {
            InputKind::MouseMove => InputEvent::MouseMove { dx: x, dy: y },
            InputKind::MouseScroll => InputEvent::MouseScroll { dx: x, dy: y },
            InputKind::MouseButton => InputEvent::MouseButton {
                button: MouseButton::try_from(x)?,
                pressed,
            },
            InputKind::Key => InputEvent::Key {
                usage: x as u16,
                pressed,
            },
        };

        Ok(Self {
            origin_ts_us: read_u64(bytes, 2),
            event,
        })
    }
}
