/**
 * Prism wire format, shared by the Rust data plane and the TypeScript control plane.
 *
 * The layouts implemented here are specified in `docs/wire-format.md` and pinned by
 * `packages/protocol/vectors.json`, which both `cargo test` and `vitest` assert against.
 * Any format change starts by editing the vectors.
 */

export {
  CLOCK_PING_LEN,
  CLOCK_PONG_LEN,
  CONTROL_HEADER_LEN,
  Channel,
  ControlType,
  FEEDBACK_PACKET_LEN,
  FORMAT_VERSION,
  MAX_PACKET_SIZE,
  MAX_VIDEO_PAYLOAD,
  VIDEO_FLAGS_RESERVED_MASK,
  VIDEO_HEADER_LEN,
  VideoFlags,
} from './constants.js';
export { PrismProtocolError } from './errors.js';
export {
  channelOf,
  controlTypeOf,
  decodeClockPing,
  decodeClockPong,
  decodeFeedbackPacket,
  decodeVideoPacket,
  encodeClockPing,
  encodeClockPong,
  encodeFeedbackPacket,
  encodeVideoPacket,
} from './packet.js';
export type { ClockPing, ClockPong, FeedbackPacket, VideoPacket } from './packet.js';
