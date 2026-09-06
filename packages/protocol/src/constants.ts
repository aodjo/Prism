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
