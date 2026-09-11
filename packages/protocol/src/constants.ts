/**
 * Wire format constants shared by the Rust core and the TypeScript control plane.
 *
 * These values are mirrored in `crates/prism-core/src/net/packet.rs` and asserted
 * against `packages/protocol/vectors.json` by both test suites. Changing any of them
 * requires updating the vectors first.
 */

/** Wire format revision. Bumped on any incompatible layout change. */
export const FORMAT_VERSION = 1;

/** Maximum UDP payload in bytes, held under the safe PMTU floor so packets never fragment. */
export const MAX_PACKET_SIZE = 1200;

/** Byte length of a video packet header, including the leading channel tag. */
export const VIDEO_HEADER_LEN = 20;

/**
 * Bytes a sealed packet costs beyond its plaintext.
 *
 * Eight for the nonce counter that travels in the clear and sixteen for the authentication
 * tag. Everything else — the channel tag included — is inside the seal.
 */
export const SEAL_OVERHEAD = 24;

/**
 * Largest plaintext packet that still fits on the wire once sealed.
 *
 * `MAX_PACKET_SIZE` describes what leaves the socket, so the budget the packet formats are
 * built against is that minus what the seal costs.
 */
export const MAX_PLAINTEXT_SIZE = MAX_PACKET_SIZE - SEAL_OVERHEAD;

/** Largest slice fragment that fits in one video packet. */
export const MAX_VIDEO_PAYLOAD = MAX_PLAINTEXT_SIZE - VIDEO_HEADER_LEN;

/** Exact byte length of a feedback packet; it carries no variable-length payload. */
export const FEEDBACK_PACKET_LEN = 18;

/**
 * Feedback flag: the client cannot decode what it is being sent and needs a fresh start.
 *
 * Set when a frame never completes or the decoder refuses one. Every frame is a reference,
 * so one gap makes every later frame undecodable until a keyframe arrives, and the client
 * cannot produce one by itself. The host rate limits its answer, because a keyframe is a
 * bitrate spike and a client that asked on every frame would turn the stream into a
 * sequence of them.
 */
export const FEEDBACK_WANTS_KEYFRAME = 0x01;

/** Feedback flag bits that carry no meaning yet; a packet setting one is rejected. */
export const FEEDBACK_FLAGS_RESERVED_MASK = 0xfe;

/** Byte length of a control packet header: the channel tag and the message type. */
export const CONTROL_HEADER_LEN = 2;

/** Exact byte length of a clock synchronisation ping. */
export const CLOCK_PING_LEN = 10;

/** Exact byte length of a clock synchronisation pong. */
export const CLOCK_PONG_LEN = 26;

/** Exact byte length of an input event packet. */
export const INPUT_PACKET_LEN = 15;

/** Exact byte length of a cursor position message. */
export const CURSOR_POSITION_LEN = 18;

/** Exact byte length of a goodbye: the header and nothing after it. */
export const GOODBYE_LEN = CONTROL_HEADER_LEN;

/**
 * Byte length of a parity packet header, including the leading channel tag.
 *
 * Deliberately the same as `VIDEO_HEADER_LEN`. A parity shard has to be exactly as long as
 * the data shards it repairs, so a header even one byte longer would push the packet past
 * `MAX_PACKET_SIZE` and fragment it.
 */
export const FEC_HEADER_LEN = 20;

/** Largest parity shard that fits in one packet. */
export const MAX_FEC_PAYLOAD = MAX_PLAINTEXT_SIZE - FEC_HEADER_LEN;

/** Byte length of an audio packet header, including the leading channel tag. */
export const AUDIO_HEADER_LEN = 13;

/**
 * Largest Opus packet that fits in one datagram.
 *
 * Far more than one is ever needed — five milliseconds of stereo at a hundred and twenty
 * kilobits is about eighty bytes — which is the point: audio never fragments and never has to
 * be reassembled, so a lost audio packet costs exactly one frame and nothing else.
 */
export const MAX_AUDIO_PAYLOAD = MAX_PLAINTEXT_SIZE - AUDIO_HEADER_LEN;

/**
 * Most shards a Reed-Solomon block may hold, data and parity together.
 *
 * GF(2^8) has 256 elements, and Prism stops one short so a block's shard count fits a
 * single byte on the wire.
 */
export const MAX_FIELD_SHARDS = 255;

/**
 * What an input packet describes.
 *
 * The wire layout is one fixed size for all four, with the two coordinate fields
 * reinterpreted per kind. A tagged union with per-kind lengths would save a few bytes on
 * a packet that is already tiny, at the cost of a decoder that has to branch before it
 * knows how much to read.
 */
export enum InputKind {
  MouseMove = 0,
  MouseButton = 1,
  MouseScroll = 2,
  Key = 3,
  MouseTo = 4,
}

/** Which pointer button an event refers to. */
export enum MouseButton {
  Left = 0,
  Right = 1,
  Middle = 2,
}

/**
 * Channel tag carried in the first byte of every packet.
 *
 * A single UDP flow multiplexes all six channels. The tag is read before anything
 * else and decides which decoder handles the remaining bytes.
 */
export enum Channel {
  Control = 0,
  Video = 1,
  Audio = 2,
  Input = 3,
  Feedback = 4,
  Fec = 5,
  File = 6,
}

/**
 * Bit flags carried in the video packet header.
 *
 * Bits 3 through 7 are reserved and must be zero. A decoder that sees a set reserved
 * bit rejects the packet rather than guessing at a future format.
 */
export enum VideoFlags {
  None = 0,
  Idr = 0x01,
  LastOfFrame = 0x02,
  LtrRef = 0x04,
}

/** Reserved video flag bits; any packet setting one of these is rejected. */
export const VIDEO_FLAGS_RESERVED_MASK = 0xf8;

/**
 * Message type carried in the second byte of a control packet.
 *
 * Values from four upward are reserved; a decoder that sees one rejects the packet rather
 * than guessing at a future revision.
 */
export enum ControlType {
  ClockPing = 0,
  ClockPong = 1,
  CursorPosition = 2,
  Goodbye = 3,
}

/** Byte length of a file message header: the channel tag and the message type. */
export const FILE_HEADER_LEN = 2;

/**
 * Byte length of a file chunk header, including the leading channel tag.
 *
 * The tag, the type, the transfer this belongs to and which chunk of it this is.
 */
export const FILE_CHUNK_HEADER_LEN = 10;

/** Largest piece of a file that fits in one packet. */
export const MAX_FILE_PAYLOAD = MAX_PLAINTEXT_SIZE - FILE_CHUNK_HEADER_LEN;

/** Bytes of a file offer before the name. */
export const FILE_OFFER_FIXED_LEN = 20;

/** Exact byte length of an answer to an offer. */
export const FILE_ANSWER_LEN = 8;

/** Exact byte length of a receiver's report. */
export const FILE_REPORT_LEN = 14;

/** Bytes of a listing before its entries. */
export const FILE_LISTING_FIXED_LEN = 5;

/** Bytes of one listing entry before its name. */
export const FILE_ENTRY_FIXED_LEN = 9;

/** Bytes of a request for one file before the name. */
export const FILE_ASK_FIXED_LEN = 3;

/**
 * Longest file name the wire carries, in bytes of UTF-8.
 *
 * An offer has to fit in one packet, and a name is the only part of it that varies.
 */
export const MAX_FILE_NAME = 255;

/** Message type carried in the second byte of a file packet. */
export enum FileType {
  Offer = 0,
  Answer = 1,
  Chunk = 2,
  Report = 3,
  List = 4,
  Listing = 5,
  Ask = 6,
}

/** Why an offered file was not taken. */
export enum FileRefusal {
  Declined = 0,
  TooLarge = 1,
  BadName = 2,
  NotWritable = 3,
}
