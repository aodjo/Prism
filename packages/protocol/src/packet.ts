import {
  CLOCK_PING_LEN,
  INPUT_PACKET_LEN,
  InputKind,
  MouseButton,
  CLOCK_PONG_LEN,
  CONTROL_HEADER_LEN,
  CURSOR_POSITION_LEN,
  AUDIO_HEADER_LEN,
  FEC_HEADER_LEN,
  MAX_AUDIO_PAYLOAD,
  MAX_FEC_PAYLOAD,
  MAX_FIELD_SHARDS,
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

  if (tag > Channel.Fec) {
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
  if (
    type !== ControlType.ClockPing &&
    type !== ControlType.ClockPong &&
    type !== ControlType.CursorPosition
  ) {
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
 * One Reed-Solomon parity shard, as carried on `Channel.Fec`.
 *
 * Parity is computed per slice, over that slice's own packets. A slice is the unit the
 * encoder emits and the unit the reassembler already stores as a contiguous shard matrix,
 * so protecting it needs no rearranging of anything.
 */
export interface FecPacket {
  /** Frame the repaired slice belongs to. */
  frameId: number;
  /** Slice within that frame this parity repairs. */
  sliceId: number;
  /** How many data shards the protected block has. */
  dataCount: number;
  /** How many parity shards were generated for it. */
  parityCount: number;
  /** Which parity shard this packet carries, counted from zero. */
  shardIndex: number;
  /**
   * Bytes in the slice's final data shard, from 1 to `MAX_VIDEO_PAYLOAD`.
   *
   * This is the field that makes recovery correct rather than merely possible. A receiver
   * learns a slice's true length from its final packet, so a slice whose final packet was
   * lost and then rebuilt from parity would have a length of zero and yield an empty
   * bitstream — recovered bytes with no framing, and no error to say so. Every other data
   * shard is exactly `MAX_VIDEO_PAYLOAD`, so this is all that is needed to recover the
   * length from any parity packet.
   */
  tailLen: number;
  /** Host clock at capture time, copied from the frame the slice belongs to. */
  captureTsUs: bigint;
  /** The parity shard, as long as the data shards it repairs. */
  payload: Uint8Array;
}

/**
 * Returns the true byte length of the slice a parity packet repairs.
 *
 * @param {FecPacket} packet - The parity packet to measure against.
 * @returns {number} Length of the protected slice in bytes.
 *
 * @example
 * sliceLenOf({ dataCount: 3, tailLen: 100, ... }); // 2 * 1180 + 100
 */
export function sliceLenOf(packet: FecPacket): number {
  return (packet.dataCount - 1) * MAX_VIDEO_PAYLOAD + packet.tailLen;
}

/**
 * Serialises a parity shard into its 20-byte header plus the shard.
 *
 * @param {FecPacket} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of `FEC_HEADER_LEN` plus the shard.
 * @throws {PrismProtocolError} If a field does not fit its width, the shard exceeds `MAX_FEC_PAYLOAD`, or the block description is one no encoder could have produced.
 *
 * @example
 * encodeFecPacket({ frameId: 1, sliceId: 0, dataCount: 4, parityCount: 1, shardIndex: 0, tailLen: 10, captureTsUs: 0n, payload: new Uint8Array(4) }).length; // 24
 */
export function encodeFecPacket(packet: FecPacket): Uint8Array {
  assertU32('frameId', packet.frameId);
  assertU16('sliceId', packet.sliceId);
  assertU64('captureTsUs', packet.captureTsUs);
  assertFecBlock(packet);

  if (packet.payload.length > MAX_FEC_PAYLOAD) {
    throw new PrismProtocolError(
      `parity shard is ${packet.payload.length} bytes, exceeds MAX_FEC_PAYLOAD of ${MAX_FEC_PAYLOAD}`,
    );
  }

  const bytes = new Uint8Array(FEC_HEADER_LEN + packet.payload.length);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Fec);
  view.setUint32(1, packet.frameId, true);
  view.setUint16(5, packet.sliceId, true);
  view.setUint8(7, packet.dataCount);
  view.setUint8(8, packet.parityCount);
  view.setUint8(9, packet.shardIndex);
  view.setUint16(10, packet.tailLen, true);
  view.setBigUint64(12, packet.captureTsUs, true);
  bytes.set(packet.payload, FEC_HEADER_LEN);

  return bytes;
}

/**
 * Parses a parity packet, copying its shard out of the input.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FecPacket} The decoded parity shard.
 * @throws {PrismProtocolError} If the packet is not on the parity channel, is shorter than its header, or describes a block no encoder could have produced.
 *
 * @example
 * decodeFecPacket(encodeFecPacket(packet)).shardIndex; // 0
 */
export function decodeFecPacket(bytes: Uint8Array): FecPacket {
  const channel = channelOf(bytes);
  if (channel !== Channel.Fec) {
    throw new PrismProtocolError(
      `expected channel ${Channel.Fec}, got tag ${bytes[0]}`,
    );
  }

  if (bytes.length < FEC_HEADER_LEN) {
    throw new PrismProtocolError(
      `fec packet is ${bytes.length} bytes, needs at least ${FEC_HEADER_LEN}`,
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const packet: FecPacket = {
    frameId: view.getUint32(1, true),
    sliceId: view.getUint16(5, true),
    dataCount: view.getUint8(7),
    parityCount: view.getUint8(8),
    shardIndex: view.getUint8(9),
    tailLen: view.getUint16(10, true),
    captureTsUs: view.getBigUint64(12, true),
    payload: bytes.slice(FEC_HEADER_LEN),
  };

  assertFecBlock(packet);

  return packet;
}

/**
 * Throws unless a parity packet describes a block an encoder could have produced.
 *
 * Refused rather than repaired: the recovery path is driven directly by these counts, and a
 * corrupted header would send it looking for shards that do not exist.
 *
 * @param {FecPacket} packet - The packet whose block description to check.
 * @returns {void} Nothing; the check either passes or throws.
 * @throws {PrismProtocolError} Naming the field that is impossible.
 *
 * @example
 * assertFecBlock({ dataCount: 4, parityCount: 1, shardIndex: 0, tailLen: 10, payload: new Uint8Array(1) }); // passes
 */
function assertFecBlock(packet: FecPacket): void {
  if (packet.payload.length === 0) {
    throw new PrismProtocolError('a parity packet carries no shard');
  }
  if (packet.dataCount === 0) {
    throw new PrismProtocolError('a block with no data shards repairs nothing');
  }
  if (packet.parityCount === 0) {
    throw new PrismProtocolError(
      'a block with no parity shards cannot contain this packet',
    );
  }
  if (packet.shardIndex >= packet.parityCount) {
    throw new PrismProtocolError(
      'the parity shard index is not below the parity count',
    );
  }
  if (packet.dataCount + packet.parityCount > MAX_FIELD_SHARDS) {
    throw new PrismProtocolError(
      'data and parity shards exceed what the field allows',
    );
  }
  if (packet.tailLen === 0) {
    throw new PrismProtocolError('the final data shard cannot be empty');
  }
  if (packet.tailLen > MAX_VIDEO_PAYLOAD) {
    throw new PrismProtocolError(
      'the final data shard cannot exceed MAX_VIDEO_PAYLOAD',
    );
  }
}

/**
 * Where the host's pointer is, so the client can draw the cursor itself.
 *
 * The host keeps the cursor out of the captured video, so the client has to draw it. That
 * is the point: a cursor baked into the frames inherits the whole video latency, while one
 * drawn by the client answers the hand holding the mouse immediately and is corrected by
 * these messages as they arrive.
 */
export interface CursorPosition {
  /** Host clock when the pointer was read, in microseconds. */
  sampleTsUs: bigint;
  /** Pixels from the left of the host's primary display. */
  x: number;
  /** Pixels from the top of the host's primary display. */
  y: number;
  /** Width of the host's primary display in pixels; never zero. */
  screenWidth: number;
  /** Height of the host's primary display in pixels; never zero. */
  screenHeight: number;
}

/**
 * Serialises a cursor position into its fixed 18-byte layout.
 *
 * The screen size travels with every message rather than being negotiated once. It is four
 * bytes against a packet already this small, and it means a client that joins late, or
 * misses the message where the host changed resolution, is never left scaling against a
 * screen that no longer exists.
 *
 * @param {CursorPosition} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `CURSOR_POSITION_LEN` bytes.
 * @throws {PrismProtocolError} If a field does not fit its width, or either screen dimension is zero.
 *
 * @example
 * encodeCursorPosition({ sampleTsUs: 1n, x: 8, y: 4, screenWidth: 2560, screenHeight: 1440 }).length; // 18
 */
export function encodeCursorPosition(packet: CursorPosition): Uint8Array {
  assertU64('sampleTsUs', packet.sampleTsUs);
  assertU16('x', packet.x);
  assertU16('y', packet.y);
  assertU16('screenWidth', packet.screenWidth);
  assertU16('screenHeight', packet.screenHeight);
  assertScreenHasArea(packet.screenWidth, packet.screenHeight);

  const bytes = new Uint8Array(CURSOR_POSITION_LEN);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Control);
  view.setUint8(1, ControlType.CursorPosition);
  view.setBigUint64(2, packet.sampleTsUs, true);
  view.setUint16(10, packet.x, true);
  view.setUint16(12, packet.y, true);
  view.setUint16(14, packet.screenWidth, true);
  view.setUint16(16, packet.screenHeight, true);

  return bytes;
}

/**
 * Parses a cursor position, requiring an exact length match and a screen with area.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {CursorPosition} The decoded reading.
 * @throws {PrismProtocolError} If the packet is not a cursor position, is not exactly `CURSOR_POSITION_LEN` bytes, or claims a screen with no area.
 *
 * @example
 * decodeCursorPosition(encodeCursorPosition({ sampleTsUs: 1n, x: 8, y: 4, screenWidth: 16, screenHeight: 9 })).x; // 8
 */
export function decodeCursorPosition(bytes: Uint8Array): CursorPosition {
  expectControl(bytes, ControlType.CursorPosition, CURSOR_POSITION_LEN);

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const screenWidth = view.getUint16(14, true);
  const screenHeight = view.getUint16(16, true);
  assertScreenHasArea(screenWidth, screenHeight);

  return {
    sampleTsUs: view.getBigUint64(2, true),
    x: view.getUint16(10, true),
    y: view.getUint16(12, true),
    screenWidth,
    screenHeight,
  };
}

/**
 * Throws unless a claimed screen has both dimensions.
 *
 * The client divides by these to place the cursor, so a zero would either crash it or
 * silently put the cursor nowhere. A screen of no pixels is not a thing that exists.
 *
 * @param {number} width - Claimed screen width in pixels.
 * @param {number} height - Claimed screen height in pixels.
 * @returns {void} Nothing; the check either passes or throws.
 * @throws {PrismProtocolError} If either dimension is zero.
 *
 * @example
 * assertScreenHasArea(2560, 1440); // passes
 */
function assertScreenHasArea(width: number, height: number): void {
  if (width === 0 || height === 0) {
    throw new PrismProtocolError(
      `cursor position claims a ${width}x${height} screen`,
    );
  }
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

/**
 * One input event, as carried on {@link Channel.Input}.
 *
 * Motion is relative rather than absolute because that is what a captured pointer
 * produces and what a game reads. Keys are identified by USB HID usage code, which both
 * Windows and macOS can be mapped from and neither uses natively.
 */
export type InputEvent =
  | { kind: InputKind.MouseMove; dx: number; dy: number }
  | { kind: InputKind.MouseButton; button: MouseButton; pressed: boolean }
  | { kind: InputKind.MouseScroll; dx: number; dy: number }
  | { kind: InputKind.Key; usage: number; pressed: boolean };

/**
 * An input event with the time it happened.
 *
 * The timestamp is carried in the host's clock, converted by the client before sending:
 * the client is the side that measures the offset between the two, so it is the side that
 * can do the conversion.
 */
export interface InputPacket {
  originTsUs: bigint;
  event: InputEvent;
}

/**
 * Serialises an input event into its fixed 15-byte layout.
 *
 * @param {InputPacket} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `INPUT_PACKET_LEN` bytes.
 * @throws {PrismProtocolError} If the timestamp or a coordinate is out of range for its wire field.
 *
 * @example
 * encodeInputPacket({
 *   originTsUs: 1_000_000n,
 *   event: { kind: InputKind.MouseMove, dx: -5, dy: 10 },
 * }).length; // 15
 */
export function encodeInputPacket(packet: InputPacket): Uint8Array {
  assertU64('originTsUs', packet.originTsUs);

  let x = 0;
  let y = 0;
  let flags = 0;

  switch (packet.event.kind) {
    case InputKind.MouseMove:
    case InputKind.MouseScroll:
      x = packet.event.dx;
      y = packet.event.dy;
      break;
    case InputKind.MouseButton:
      x = packet.event.button;
      flags = packet.event.pressed ? 1 : 0;
      break;
    case InputKind.Key:
      x = packet.event.usage > 0x7fff ? packet.event.usage - 0x10000 : packet.event.usage;
      flags = packet.event.pressed ? 1 : 0;
      break;
  }

  assertI16('x', x);
  assertI16('y', y);

  const bytes = new Uint8Array(INPUT_PACKET_LEN);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Input);
  view.setUint8(1, packet.event.kind);
  view.setBigUint64(2, packet.originTsUs, true);
  view.setInt16(10, x, true);
  view.setInt16(12, y, true);
  view.setUint8(14, flags);

  return bytes;
}

/**
 * Parses an input event, requiring an exact length match.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {InputPacket} The decoded event.
 * @throws {PrismProtocolError} If the channel, length, kind, or button is not one this build knows.
 *
 * @example
 * decodeInputPacket(encodeInputPacket(packet)).event.kind; // InputKind.MouseMove
 */
export function decodeInputPacket(bytes: Uint8Array): InputPacket {
  if (channelOf(bytes) !== Channel.Input) {
    throw new PrismProtocolError(`expected channel ${Channel.Input}, got ${bytes.at(0)}`);
  }

  if (bytes.length !== INPUT_PACKET_LEN) {
    throw new PrismProtocolError(
      `input packet is ${bytes.length} bytes, expected exactly ${INPUT_PACKET_LEN}`,
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const kind = view.getUint8(1);
  const x = view.getInt16(10, true);
  const y = view.getInt16(12, true);
  const pressed = (view.getUint8(14) & 1) !== 0;
  const originTsUs = view.getBigUint64(2, true);

  switch (kind) {
    case InputKind.MouseMove:
      return { originTsUs, event: { kind: InputKind.MouseMove, dx: x, dy: y } };
    case InputKind.MouseScroll:
      return { originTsUs, event: { kind: InputKind.MouseScroll, dx: x, dy: y } };
    case InputKind.MouseButton:
      if (x !== MouseButton.Left && x !== MouseButton.Right && x !== MouseButton.Middle) {
        throw new PrismProtocolError(`unknown mouse button ${x}`);
      }
      return { originTsUs, event: { kind: InputKind.MouseButton, button: x, pressed } };
    case InputKind.Key:
      return {
        originTsUs,
        event: { kind: InputKind.Key, usage: x < 0 ? x + 0x10000 : x, pressed },
      };
    default:
      throw new PrismProtocolError(`unknown input kind ${kind}`);
  }
}

/**
 * Throws unless a value fits a signed 16-bit wire field.
 *
 * @param {string} field - Field name, used in the error message.
 * @param {number} value - Value to check.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If `value` is not an integer in the range -32768 to 32767.
 *
 * @example
 * assertI16('dx', -5); // passes
 */
function assertI16(field: string, value: number): void {
  if (!Number.isInteger(value) || value < -0x8000 || value > 0x7fff) {
    throw new PrismProtocolError(`${field} must be an integer in [-32768, 32767], got ${value}`);
  }
}

/**
 * One encoded audio frame on its way to the client.
 *
 * Audio is not sliced and not reassembled. A frame is small enough that one always fits in one
 * datagram, so a lost packet costs exactly one frame — which the decoder conceals — rather than
 * stalling a reassembly that would then have to be abandoned.
 */
export interface AudioPacket {
  /** Position of this frame in the stream, counted from zero and wrapping. */
  sequence: number;
  /** Host clock when the audio was captured, in microseconds. */
  captureTsUs: bigint;
  /** One Opus packet. */
  payload: Uint8Array;
}

/**
 * Serialises an audio frame.
 *
 * @param {AudioPacket} packet - The frame to encode.
 * @returns {Uint8Array} The packet, ready to be sealed.
 * @throws {PrismProtocolError} If a field is out of range or the Opus packet exceeds `MAX_AUDIO_PAYLOAD`.
 *
 * @example
 * encodeAudioPacket({ sequence: 0, captureTsUs: 0n, payload: new Uint8Array(80) }).length; // 93
 */
export function encodeAudioPacket(packet: AudioPacket): Uint8Array {
  assertU32('sequence', packet.sequence);
  assertU64('captureTsUs', packet.captureTsUs);

  if (packet.payload.length > MAX_AUDIO_PAYLOAD) {
    throw new PrismProtocolError(
      `audio frame is ${packet.payload.length} bytes, exceeds MAX_AUDIO_PAYLOAD of ${MAX_AUDIO_PAYLOAD}`,
    );
  }

  const bytes = new Uint8Array(AUDIO_HEADER_LEN + packet.payload.length);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Audio);
  view.setUint32(1, packet.sequence, true);
  view.setBigUint64(5, packet.captureTsUs, true);
  bytes.set(packet.payload, AUDIO_HEADER_LEN);

  return bytes;
}

/**
 * Parses an audio packet, copying its payload out of the input.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {AudioPacket} The decoded frame.
 * @throws {PrismProtocolError} If the packet is not on the audio channel or is shorter than its header.
 *
 * @example
 * decodeAudioPacket(encodeAudioPacket(packet)).sequence; // 0
 */
export function decodeAudioPacket(bytes: Uint8Array): AudioPacket {
  const channel = channelOf(bytes);
  if (channel !== Channel.Audio) {
    throw new PrismProtocolError(
      `expected channel ${Channel.Audio}, got tag ${bytes[0]}`,
    );
  }

  if (bytes.length < AUDIO_HEADER_LEN) {
    throw new PrismProtocolError(
      `audio packet is ${bytes.length} bytes, needs at least ${AUDIO_HEADER_LEN}`,
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

  return {
    sequence: view.getUint32(1, true),
    captureTsUs: view.getBigUint64(5, true),
    payload: bytes.slice(AUDIO_HEADER_LEN),
  };
}
