import {
  CLOCK_PING_LEN,
  CLOCK_PONG_LEN,
  CONTROL_HEADER_LEN,
  Channel,
  ControlType,
  FEEDBACK_PACKET_LEN,
  MAX_VIDEO_PAYLOAD,
  VIDEO_FLAGS_RESERVED_MASK,
  VIDEO_HEADER_LEN,
} from './constants.js';
import { PrismProtocolError } from './errors.js';

/** Largest value representable by the u16 header fields. */
const U16_MAX = 0xffff;

/** Largest value representable by the u32 header fields. */
const U32_MAX = 0xffffffff;

/** Largest value representable by the u64 timestamp fields. */
const U64_MAX = 0xffffffffffffffffn;

/**
 * One fragment of an encoded video slice, as carried on channel 1.
 *
 * A frame is split into slices by the encoder and each slice into packets of at most
 * `MAX_VIDEO_PAYLOAD` bytes. `captureTsUs` is stamped once per frame and copied into
 * every packet of that frame, which is what anchors the end-to-end latency chain.
 */
export interface VideoPacket {
  frameId: number;
  sliceId: number;
  pktIdx: number;
  pktCount: number;
  flags: number;
  captureTsUs: bigint;
  payload: Uint8Array;
}

/**
 * Client-to-host receive report, as carried on channel 4.
 *
 * `recvBitmap` drives long-term-reference invalidation on the encoder: the host encodes
 * against the newest frame the client has confirmed, so packet loss never forces an IDR
 * and never produces a visible hitch. `clientTsUs` doubles as a clock-sync sample.
 */
export interface FeedbackPacket {
  lastFrameId: number;
  recvBitmap: number;
  clientTsUs: bigint;
}

/**
 * Reads the channel tag from the first byte of a packet.
 *
 * This is the only field that may be read before validation, and it decides which
 * decoder handles the rest of the bytes. Unknown tags are rejected rather than ignored
 * so that a future channel cannot be silently misrouted into an existing decoder.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {Channel} The channel this packet belongs to.
 * @throws {PrismProtocolError} If the packet is empty or the tag is not a defined channel.
 *
 * @example
 * const channel = channelOf(datagram);
 * if (channel === Channel.Video) handleVideo(decodeVideoPacket(datagram));
 */
export function channelOf(bytes: Uint8Array): Channel {
  const tag = bytes.at(0);
  if (tag === undefined) {
    throw new PrismProtocolError('packet is empty, no channel tag');
  }

  if (tag > Channel.Feedback) {
    throw new PrismProtocolError(`unknown channel tag ${tag}`);
  }

  return tag as Channel;
}

/**
 * Serialises a video packet into its 20-byte header followed by the slice payload.
 *
 * All multi-byte fields are written little-endian. The result is always at most
 * `MAX_PACKET_SIZE` bytes, so it never fragments on a path with a 1280-byte MTU.
 *
 * @param {VideoPacket} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of `VIDEO_HEADER_LEN + payload.length` bytes.
 * @throws {PrismProtocolError} If any field is out of range, a reserved flag bit is set, or the payload exceeds `MAX_VIDEO_PAYLOAD`.
 *
 * @example
 * const bytes = encodeVideoPacket({
 *   frameId: 42, sliceId: 0, pktIdx: 0, pktCount: 1,
 *   flags: VideoFlags.Idr | VideoFlags.LastOfFrame,
 *   captureTsUs: 1_108_152_157_446n,
 *   payload: sliceBytes,
 * });
 * socket.send(bytes); // 20 + sliceBytes.length bytes on the wire
 */
export function encodeVideoPacket(packet: VideoPacket): Uint8Array {
  assertU32('frameId', packet.frameId);
  assertU16('sliceId', packet.sliceId);
  assertU16('pktIdx', packet.pktIdx);
  assertU16('pktCount', packet.pktCount);
  assertU8('flags', packet.flags);
  assertU64('captureTsUs', packet.captureTsUs);

  if (packet.flags & VIDEO_FLAGS_RESERVED_MASK) {
    throw new PrismProtocolError(`reserved video flag bits set in 0x${packet.flags.toString(16)}`);
  }

  if (packet.payload.length > MAX_VIDEO_PAYLOAD) {
    throw new PrismProtocolError(
      `payload is ${packet.payload.length} bytes, exceeds MAX_VIDEO_PAYLOAD of ${MAX_VIDEO_PAYLOAD}`,
    );
  }

  const bytes = new Uint8Array(VIDEO_HEADER_LEN + packet.payload.length);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Video);
  view.setUint32(1, packet.frameId, true);
  view.setUint16(5, packet.sliceId, true);
  view.setUint16(7, packet.pktIdx, true);
  view.setUint16(9, packet.pktCount, true);
  view.setUint8(11, packet.flags);
  view.setBigUint64(12, packet.captureTsUs, true);
  bytes.set(packet.payload, VIDEO_HEADER_LEN);

  return bytes;
}

/**
 * Parses a video packet, validating every field before returning it.
 *
 * The returned `payload` is a view onto the input buffer rather than a copy, so the
 * caller must not retain it past the lifetime of `bytes`. On the receive path this is
 * intentional: the reassembly buffer owns the memory and nothing else should allocate.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {VideoPacket} The decoded packet, with `payload` aliasing `bytes`.
 * @throws {PrismProtocolError} If the channel tag is wrong, the packet is shorter than `VIDEO_HEADER_LEN`, or a reserved flag bit is set.
 *
 * @example
 * const packet = decodeVideoPacket(datagram);
 * console.log(packet.frameId, packet.payload.length); // 42 1180
 */
export function decodeVideoPacket(bytes: Uint8Array): VideoPacket {
  if (channelOf(bytes) !== Channel.Video) {
    throw new PrismProtocolError(`expected channel ${Channel.Video}, got ${bytes.at(0)}`);
  }

  if (bytes.length < VIDEO_HEADER_LEN) {
    throw new PrismProtocolError(
      `video packet is ${bytes.length} bytes, shorter than VIDEO_HEADER_LEN of ${VIDEO_HEADER_LEN}`,
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const flags = view.getUint8(11);

  if (flags & VIDEO_FLAGS_RESERVED_MASK) {
    throw new PrismProtocolError(`reserved video flag bits set in 0x${flags.toString(16)}`);
  }

  return {
    frameId: view.getUint32(1, true),
    sliceId: view.getUint16(5, true),
    pktIdx: view.getUint16(7, true),
    pktCount: view.getUint16(9, true),
    flags,
    captureTsUs: view.getBigUint64(12, true),
    payload: bytes.subarray(VIDEO_HEADER_LEN),
  };
}

/**
 * Serialises a feedback packet into its fixed 17-byte layout.
 *
 * Feedback is sent on every received frame and is the highest-priority traffic on the
 * return path; it is never batched, because a late ACK stalls the encoder's long-term
 * reference selection and costs a frame.
 *
 * @param {FeedbackPacket} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `FEEDBACK_PACKET_LEN` bytes.
 * @throws {PrismProtocolError} If any field is out of range for its wire type.
 *
 * @example
 * const bytes = encodeFeedbackPacket({
 *   lastFrameId: 256, recvBitmap: 0xfffffff0, clientTsUs: 1_000_000n,
 * });
 * console.log(bytes.length); // 17
 */
export function encodeFeedbackPacket(packet: FeedbackPacket): Uint8Array {
  assertU32('lastFrameId', packet.lastFrameId);
  assertU32('recvBitmap', packet.recvBitmap);
  assertU64('clientTsUs', packet.clientTsUs);

  const bytes = new Uint8Array(FEEDBACK_PACKET_LEN);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Feedback);
  view.setUint32(1, packet.lastFrameId, true);
  view.setUint32(5, packet.recvBitmap, true);
  view.setBigUint64(9, packet.clientTsUs, true);

  return bytes;
}

/**
 * Parses a feedback packet, requiring an exact length match.
 *
 * Unlike video packets, feedback carries no variable-length payload, so anything other
 * than exactly `FEEDBACK_PACKET_LEN` bytes indicates corruption or a version mismatch
 * and is rejected outright.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FeedbackPacket} The decoded receive report.
 * @throws {PrismProtocolError} If the channel tag is wrong or the length is not exactly `FEEDBACK_PACKET_LEN`.
 *
 * @example
 * const report = decodeFeedbackPacket(datagram);
 * encoder.acknowledge(report.lastFrameId, report.recvBitmap);
 */
export function decodeFeedbackPacket(bytes: Uint8Array): FeedbackPacket {
  if (channelOf(bytes) !== Channel.Feedback) {
    throw new PrismProtocolError(`expected channel ${Channel.Feedback}, got ${bytes.at(0)}`);
  }

  if (bytes.length !== FEEDBACK_PACKET_LEN) {
    throw new PrismProtocolError(
      `feedback packet is ${bytes.length} bytes, expected exactly ${FEEDBACK_PACKET_LEN}`,
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

  return {
    lastFrameId: view.getUint32(1, true),
    recvBitmap: view.getUint32(5, true),
    clientTsUs: view.getBigUint64(9, true),
  };
}

/**
 * Throws unless a value is a non-negative integer that fits the given wire width.
 *
 * Shared by every encoder so that an out-of-range field fails at the call site with the
 * field's name, rather than silently wrapping inside `DataView` and producing a packet
 * that decodes to the wrong numbers on the far side.
 *
 * @param {string} field - Field name, used in the error message.
 * @param {number} value - Value to check.
 * @param {number} max - Largest permitted value, inclusive.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If `value` is not an integer in the range 0 to `max`.
 *
 * @example
 * assertRange('sliceId', 70000, 0xffff); // throws PrismProtocolError
 */
function assertRange(field: string, value: number, max: number): void {
  if (!Number.isInteger(value) || value < 0 || value > max) {
    throw new PrismProtocolError(`${field} must be an integer in [0, ${max}], got ${value}`);
  }
}

/**
 * Throws unless a value fits an unsigned 8-bit wire field.
 *
 * @param {string} field - Field name, used in the error message.
 * @param {number} value - Value to check.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If `value` is not an integer in the range 0 to 255.
 *
 * @example
 * assertU8('flags', 0x03); // passes
 */
function assertU8(field: string, value: number): void {
  assertRange(field, value, 0xff);
}

/**
 * Throws unless a value fits an unsigned 16-bit wire field.
 *
 * @param {string} field - Field name, used in the error message.
 * @param {number} value - Value to check.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If `value` is not an integer in the range 0 to 65535.
 *
 * @example
 * assertU16('pktCount', 8); // passes
 */
function assertU16(field: string, value: number): void {
  assertRange(field, value, U16_MAX);
}

/**
 * Throws unless a value fits an unsigned 32-bit wire field.
 *
 * @param {string} field - Field name, used in the error message.
 * @param {number} value - Value to check.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If `value` is not an integer in the range 0 to 4294967295.
 *
 * @example
 * assertU32('frameId', 16909060); // passes
 */
function assertU32(field: string, value: number): void {
  assertRange(field, value, U32_MAX);
}

/**
 * Throws unless a bigint fits an unsigned 64-bit wire field.
 *
 * Timestamps are microsecond counters that exceed `Number.MAX_SAFE_INTEGER`, so they are
 * carried as `bigint` throughout rather than being narrowed to `number` anywhere.
 *
 * @param {string} field - Field name, used in the error message.
 * @param {bigint} value - Value to check.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If `value` is negative or exceeds 2^64 - 1.
 *
 * @example
 * assertU64('captureTsUs', 1_108_152_157_446n); // passes
 */
function assertU64(field: string, value: bigint): void {
  if (typeof value !== 'bigint' || value < 0n || value > U64_MAX) {
    throw new PrismProtocolError(`${field} must be a bigint in [0, ${U64_MAX}], got ${value}`);
  }
}

/**
 * The client's half of a clock synchronisation exchange.
 *
 * The host answers with a {@link ClockPong} carrying both of its own timestamps, from
 * which the client derives the offset between the two clocks.
 */
export interface ClockPing {
  t1Us: bigint;
}

/**
 * The host's answer to a {@link ClockPing}.
 *
 * Carries the ping's own timestamp back so the client can pair the reply, plus the two
 * host timestamps that bracket the host's handling of it.
 */
export interface ClockPong {
  t1Us: bigint;
  t2Us: bigint;
  t3Us: bigint;
}

/**
 * Reads the control message type from a control packet.
 *
 * Like the channel tag, this is read before the rest of the packet is validated, because
 * it decides which decoder handles the remaining bytes.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {ControlType} The message type this packet carries.
 * @throws {PrismProtocolError} If the packet is not a control packet, is too short to hold a type, or names a type this build does not know.
 *
 * @example
 * controlTypeOf(new Uint8Array([0, 1])); // ControlType.ClockPong
 */
export function controlTypeOf(bytes: Uint8Array): ControlType {
  if (channelOf(bytes) !== Channel.Control) {
    throw new PrismProtocolError(`expected channel ${Channel.Control}, got ${bytes.at(0)}`);
  }

  if (bytes.length < CONTROL_HEADER_LEN) {
    throw new PrismProtocolError(
      `control packet is ${bytes.length} bytes, shorter than CONTROL_HEADER_LEN of ${CONTROL_HEADER_LEN}`,
    );
  }

  const type = bytes.at(1);
  if (type !== ControlType.ClockPing && type !== ControlType.ClockPong) {
    throw new PrismProtocolError(`unknown control type ${type}`);
  }

  return type;
}

/**
 * Serialises a clock ping into its fixed 10-byte layout.
 *
 * @param {ClockPing} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `CLOCK_PING_LEN` bytes.
 * @throws {PrismProtocolError} If the timestamp does not fit an unsigned 64-bit field.
 *
 * @example
 * encodeClockPing({ t1Us: 1_000_000n }).length; // 10
 */
export function encodeClockPing(packet: ClockPing): Uint8Array {
  assertU64('t1Us', packet.t1Us);

  const bytes = new Uint8Array(CLOCK_PING_LEN);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Control);
  view.setUint8(1, ControlType.ClockPing);
  view.setBigUint64(2, packet.t1Us, true);

  return bytes;
}

/**
 * Parses a clock ping, requiring an exact length match.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {ClockPing} The decoded ping.
 * @throws {PrismProtocolError} If the packet is not a clock ping or is not exactly `CLOCK_PING_LEN` bytes.
 *
 * @example
 * decodeClockPing(encodeClockPing({ t1Us: 5n })).t1Us; // 5n
 */
export function decodeClockPing(bytes: Uint8Array): ClockPing {
  expectControl(bytes, ControlType.ClockPing, CLOCK_PING_LEN);

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return { t1Us: view.getBigUint64(2, true) };
}

/**
 * Serialises a clock pong into its fixed 26-byte layout.
 *
 * @param {ClockPong} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `CLOCK_PONG_LEN` bytes.
 * @throws {PrismProtocolError} If any timestamp does not fit an unsigned 64-bit field.
 *
 * @example
 * encodeClockPong({ t1Us: 1n, t2Us: 2n, t3Us: 3n }).length; // 26
 */
export function encodeClockPong(packet: ClockPong): Uint8Array {
  assertU64('t1Us', packet.t1Us);
  assertU64('t2Us', packet.t2Us);
  assertU64('t3Us', packet.t3Us);

  const bytes = new Uint8Array(CLOCK_PONG_LEN);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Control);
  view.setUint8(1, ControlType.ClockPong);
  view.setBigUint64(2, packet.t1Us, true);
  view.setBigUint64(10, packet.t2Us, true);
  view.setBigUint64(18, packet.t3Us, true);

  return bytes;
}

/**
 * Parses a clock pong, requiring an exact length match.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {ClockPong} The decoded answer.
 * @throws {PrismProtocolError} If the packet is not a clock pong or is not exactly `CLOCK_PONG_LEN` bytes.
 *
 * @example
 * decodeClockPong(encodeClockPong({ t1Us: 1n, t2Us: 2n, t3Us: 3n })).t2Us; // 2n
 */
export function decodeClockPong(bytes: Uint8Array): ClockPong {
  expectControl(bytes, ControlType.ClockPong, CLOCK_PONG_LEN);

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    t1Us: view.getBigUint64(2, true),
    t2Us: view.getBigUint64(10, true),
    t3Us: view.getBigUint64(18, true),
  };
}

/**
 * Throws unless a control packet has the expected type and exact length.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @param {ControlType} expected - Message type the caller decodes.
 * @param {number} length - Exact byte length that type requires.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If the type or the length does not match.
 *
 * @example
 * expectControl(bytes, ControlType.ClockPing, CLOCK_PING_LEN);
 */
function expectControl(bytes: Uint8Array, expected: ControlType, length: number): void {
  if (controlTypeOf(bytes) !== expected) {
    throw new PrismProtocolError(`expected control type ${expected}, got ${bytes.at(1)}`);
  }

  if (bytes.length !== length) {
    throw new PrismProtocolError(
      `control packet is ${bytes.length} bytes, expected exactly ${length}`,
    );
  }
}
