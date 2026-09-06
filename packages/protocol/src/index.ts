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
  CURSOR_POSITION_LEN,
  AUDIO_HEADER_LEN,
  FEC_HEADER_LEN,
  MAX_AUDIO_PAYLOAD,
  MAX_FEC_PAYLOAD,
  MAX_PLAINTEXT_SIZE,
  SEAL_OVERHEAD,
  MAX_FIELD_SHARDS,
  Channel,
  ControlType,
  FEEDBACK_PACKET_LEN,
  INPUT_PACKET_LEN,
  InputKind,
  MouseButton,
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
  decodeAudioPacket,
  decodeClockPing,
  decodeClockPong,
  decodeCursorPosition,
  decodeFecPacket,
  decodeFeedbackPacket,
  decodeInputPacket,
  decodeVideoPacket,
  encodeAudioPacket,
  encodeClockPing,
  encodeClockPong,
  encodeCursorPosition,
  encodeFecPacket,
  sliceLenOf,
  encodeFeedbackPacket,
  encodeInputPacket,
  encodeVideoPacket,
} from './packet.js';
export type {
  AudioPacket,
  ClockPing,
  ClockPong,
  CursorPosition,
  FecPacket,
  FeedbackPacket,
  InputEvent,
  InputPacket,
  VideoPacket,
} from './packet.js';
