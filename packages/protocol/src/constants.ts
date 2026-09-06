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

/** Largest slice fragment that fits in one video packet. */
export const MAX_VIDEO_PAYLOAD = MAX_PACKET_SIZE - VIDEO_HEADER_LEN;

/** Exact byte length of a feedback packet; it carries no variable-length payload. */
export const FEEDBACK_PACKET_LEN = 17;

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
 * A single UDP flow multiplexes all five channels. The tag is read before anything
 * else and decides which decoder handles the remaining bytes.
 */
export enum Channel {
  Control = 0,
  Video = 1,
  Audio = 2,
  Input = 3,
  Feedback = 4,
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
 * Values from two upward are reserved; a decoder that sees one rejects the packet rather
 * than guessing at a future revision.
 */
export enum ControlType {
  ClockPing = 0,
  ClockPong = 1,
  CursorPosition = 2,
}
