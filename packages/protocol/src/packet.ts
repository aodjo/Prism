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
  FEEDBACK_FLAGS_RESERVED_MASK,
  FEEDBACK_PACKET_LEN,
  FEEDBACK_WANTS_KEYFRAME,
  MAX_VIDEO_PAYLOAD,
  VIDEO_FLAGS_RESERVED_MASK,
  VIDEO_HEADER_LEN,
  FILE_ANSWER_LEN,
  FILE_ASK_FIXED_LEN,
  FILE_CHUNK_HEADER_LEN,
  FILE_ENTRY_FIXED_LEN,
  FILE_HEADER_LEN,
  FILE_LISTING_FIXED_LEN,
  FILE_OFFER_FIXED_LEN,
  FILE_REPORT_LEN,
  FileRefusal,
  FileType,
  MAX_FILE_NAME,
  MAX_FILE_PAYLOAD,
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
 *
 * `flags` is how the client asks for the one thing it cannot recover on its own; see
 * `FEEDBACK_WANTS_KEYFRAME`.
 */
export interface FeedbackPacket {
  lastFrameId: number;
  recvBitmap: number;
  clientTsUs: bigint;
  flags: number;
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

  if (tag > Channel.File) {
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
 * Serialises a feedback packet into its fixed 18-byte layout.
 *
 * Feedback is sent on every received frame and is the highest-priority traffic on the
 * return path; it is never batched, because a late ACK stalls the encoder's long-term
 * reference selection and costs a frame.
 *
 * @param {FeedbackPacket} packet - Packet fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `FEEDBACK_PACKET_LEN` bytes.
 * @throws {PrismProtocolError} If any field is out of range for its wire type, or if
 * `flags` sets a reserved bit.
 *
 * @example
 * const bytes = encodeFeedbackPacket({
 *   lastFrameId: 256, recvBitmap: 0xfffffff0, clientTsUs: 1_000_000n, flags: 0,
 * });
 * console.log(bytes.length); // 18
 */
export function encodeFeedbackPacket(packet: FeedbackPacket): Uint8Array {
  assertU32('lastFrameId', packet.lastFrameId);
  assertU32('recvBitmap', packet.recvBitmap);
  assertU64('clientTsUs', packet.clientTsUs);
  assertU8('flags', packet.flags);

  if ((packet.flags & FEEDBACK_FLAGS_RESERVED_MASK) !== 0) {
    throw new PrismProtocolError(
      `feedback flags ${packet.flags} set a reserved bit; only ${FEEDBACK_WANTS_KEYFRAME} is defined`,
    );
  }

  const bytes = new Uint8Array(FEEDBACK_PACKET_LEN);
  const view = new DataView(bytes.buffer);

  view.setUint8(0, Channel.Feedback);
  view.setUint32(1, packet.lastFrameId, true);
  view.setUint32(5, packet.recvBitmap, true);
  view.setBigUint64(9, packet.clientTsUs, true);
  view.setUint8(17, packet.flags);

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
 * @throws {PrismProtocolError} If the channel tag is wrong, the length is not exactly
 * `FEEDBACK_PACKET_LEN`, or a reserved flag bit is set.
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
  const flags = view.getUint8(17);

  if ((flags & FEEDBACK_FLAGS_RESERVED_MASK) !== 0) {
    throw new PrismProtocolError(
      `feedback flags ${flags} set a reserved bit; only ${FEEDBACK_WANTS_KEYFRAME} is defined`,
    );
  }

  return {
    lastFrameId: view.getUint32(1, true),
    recvBitmap: view.getUint32(5, true),
    clientTsUs: view.getBigUint64(9, true),
    flags,
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
  | { kind: InputKind.Key; usage: number; pressed: boolean }
  | { kind: InputKind.MouseTo; x: number; y: number };

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
    case InputKind.MouseTo:
      assertFraction('x', packet.event.x);
      assertFraction('y', packet.event.y);
      x = packet.event.x > 0x7fff ? packet.event.x - 0x10000 : packet.event.x;
      y = packet.event.y > 0x7fff ? packet.event.y - 0x10000 : packet.event.y;
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
    case InputKind.MouseTo:
      return {
        originTsUs,
        event: {
          kind: InputKind.MouseTo,
          x: x < 0 ? x + 0x10000 : x,
          y: y < 0 ? y + 0x10000 : y,
        },
      };
    default:
      throw new PrismProtocolError(`unknown input kind ${kind}`);
  }
}

/**
 * Throws unless a value is a fraction of the screen as the wire carries one.
 *
 * Zero is one edge and 65535 the other. Anything outside that would wrap around when it is
 * packed into the two bytes the field has, and put the pointer on the opposite side of the
 * screen from where it was aimed.
 *
 * @param {string} field - Field name, used in the error message.
 * @param {number} value - Value to check.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If `value` is not an integer in the range 0 to 65535.
 *
 * @example
 * assertFraction('x', 32768); // passes
 */
function assertFraction(field: string, value: number): void {
  if (!Number.isInteger(value) || value < 0 || value > 0xffff) {
    throw new PrismProtocolError(`${field} is ${value}, outside 0 to 65535`);
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

/**
 * Throws unless a packet is on the file channel and long enough to read.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @param {number} needed - Bytes the layout requires.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If the channel is wrong or the packet is short.
 *
 * @example
 * expectFile(bytes, FILE_HEADER_LEN);
 */
function expectFile(bytes: Uint8Array, needed: number): void {
  const channel = channelOf(bytes);
  if (channel !== Channel.File) {
    throw new PrismProtocolError(`expected channel ${Channel.File}, got tag ${bytes[0]}`);
  }

  if (bytes.length < needed) {
    throw new PrismProtocolError(
      `file packet is ${bytes.length} bytes, needs at least ${needed}`,
    );
  }
}

/**
 * Reads the message type from a file packet.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FileType} The message type in the second byte.
 * @throws {PrismProtocolError} If the packet is not a file packet or the type is unknown.
 *
 * @example
 * fileTypeOf(new Uint8Array([6, 0])); // FileType.Offer
 */
export function fileTypeOf(bytes: Uint8Array): FileType {
  expectFile(bytes, FILE_HEADER_LEN);

  const tag = bytes[1] as number;
  if (!(tag in FileType)) {
    throw new PrismProtocolError(`unknown file message type ${tag}`);
  }

  return tag as FileType;
}

/**
 * Throws unless a file packet is one particular message, long enough to read.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @param {FileType} expected - Message the caller decodes.
 * @param {number} needed - Bytes that message requires.
 * @returns {void} Nothing; the function is used purely for its throwing behaviour.
 * @throws {PrismProtocolError} If the type is wrong or the packet is short.
 *
 * @example
 * expectFileType(bytes, FileType.Report, FILE_REPORT_LEN);
 */
function expectFileType(bytes: Uint8Array, expected: FileType, needed: number): void {
  const got = fileTypeOf(bytes);
  if (got !== expected) {
    throw new PrismProtocolError(`expected file message ${expected}, got ${got}`);
  }

  if (bytes.length < needed) {
    throw new PrismProtocolError(
      `file packet is ${bytes.length} bytes, needs at least ${needed}`,
    );
  }
}

/**
 * Returns whether a name is one the far side would be willing to write down.
 *
 * A file arriving over a network names the file it becomes, so refusing is the whole job:
 * anything with a separator in it, either of the two relative directories, anything empty,
 * and anything longer than the wire carries.
 *
 * @param {string} name - Name as it appeared on the wire.
 * @returns {boolean} Whether it names a file and nothing else.
 *
 * @example
 * plainFileName('notes.txt'); // true
 * plainFileName('../etc/passwd'); // false
 */
export function plainFileName(name: string): boolean {
  const bytes = new TextEncoder().encode(name).length;

  return (
    name.length > 0 &&
    bytes <= MAX_FILE_NAME &&
    name !== '.' &&
    name !== '..' &&
    !/[/\\\0]/.test(name)
  );
}

/**
 * An offer to send one file, as carried on {@link Channel.File}.
 *
 * Nothing moves until the far side answers. The size is what it will cost and the chunk
 * count is what the receiver reports against, so both are settled before a byte is sent.
 */
export interface FileOffer {
  id: number;
  size: bigint;
  chunks: number;
  name: string;
}

/**
 * Serialises an offer to send one file.
 *
 * @param {FileOffer} offer - Offer fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer holding the offer.
 * @throws {PrismProtocolError} If a field is out of range or the name is not a file name.
 *
 * @example
 * encodeFileOffer({ id: 1, size: 9n, chunks: 1, name: 'notes.txt' }).length; // 29
 */
export function encodeFileOffer(offer: FileOffer): Uint8Array {
  if (!plainFileName(offer.name)) {
    throw new PrismProtocolError(`${offer.name} is not a file name`);
  }

  const name = new TextEncoder().encode(offer.name);
  const bytes = new Uint8Array(FILE_OFFER_FIXED_LEN + name.length);
  const view = new DataView(bytes.buffer);

  bytes[0] = Channel.File;
  bytes[1] = FileType.Offer;
  view.setUint32(2, offer.id, true);
  view.setBigUint64(6, offer.size, true);
  view.setUint32(14, offer.chunks, true);
  view.setUint16(18, name.length, true);
  bytes.set(name, FILE_OFFER_FIXED_LEN);

  return bytes;
}

/**
 * Parses an offer to send one file.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FileOffer} The decoded offer.
 * @throws {PrismProtocolError} If the packet is malformed or names nothing writable.
 *
 * @example
 * decodeFileOffer(encodeFileOffer(offer)).name; // 'notes.txt'
 */
export function decodeFileOffer(bytes: Uint8Array): FileOffer {
  expectFileType(bytes, FileType.Offer, FILE_OFFER_FIXED_LEN);

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const length = view.getUint16(18, true);
  const needed = FILE_OFFER_FIXED_LEN + length;

  if (bytes.length < needed) {
    throw new PrismProtocolError(`file packet is ${bytes.length} bytes, needs ${needed}`);
  }

  const name = decodeName(bytes.subarray(FILE_OFFER_FIXED_LEN, needed));

  return {
    id: view.getUint32(2, true),
    size: view.getBigUint64(6, true),
    chunks: view.getUint32(14, true),
    name,
  };
}

/**
 * Whether an offered file will be taken, as carried on {@link Channel.File}.
 *
 * Also what ends a transfer early: a receiver that has run out of disk, or a person who
 * changed their mind, sends one of these with `accepted` false and the sender stops.
 */
export interface FileAnswer {
  id: number;
  accepted: boolean;
  refusal: FileRefusal;
}

/**
 * Serialises an answer to an offer.
 *
 * @param {FileAnswer} answer - Answer fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `FILE_ANSWER_LEN` bytes.
 *
 * @example
 * encodeFileAnswer({ id: 7, accepted: true, refusal: FileRefusal.Declined }).length; // 8
 */
export function encodeFileAnswer(answer: FileAnswer): Uint8Array {
  const bytes = new Uint8Array(FILE_ANSWER_LEN);
  const view = new DataView(bytes.buffer);

  bytes[0] = Channel.File;
  bytes[1] = FileType.Answer;
  view.setUint32(2, answer.id, true);
  bytes[6] = answer.accepted ? 1 : 0;
  bytes[7] = answer.refusal;

  return bytes;
}

/**
 * Parses an answer to an offer.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FileAnswer} The decoded answer.
 * @throws {PrismProtocolError} If the packet is malformed or the refusal is unknown.
 *
 * @example
 * decodeFileAnswer(encodeFileAnswer(answer)).accepted; // true
 */
export function decodeFileAnswer(bytes: Uint8Array): FileAnswer {
  expectFileType(bytes, FileType.Answer, FILE_ANSWER_LEN);

  const refusal = bytes[7] as number;
  if (!(refusal in FileRefusal)) {
    throw new PrismProtocolError(`unknown file refusal ${refusal}`);
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

  return {
    id: view.getUint32(2, true),
    accepted: bytes[6] !== 0,
    refusal: refusal as FileRefusal,
  };
}

/** One piece of a file that was accepted, as carried on {@link Channel.File}. */
export interface FileChunk {
  id: number;
  index: number;
  payload: Uint8Array;
}

/**
 * Serialises one piece of a file.
 *
 * @param {FileChunk} chunk - Chunk fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer holding header and payload.
 * @throws {PrismProtocolError} If the payload exceeds `MAX_FILE_PAYLOAD`.
 *
 * @example
 * encodeFileChunk({ id: 1, index: 2, payload: new Uint8Array([7, 8, 9]) }).length; // 13
 */
export function encodeFileChunk(chunk: FileChunk): Uint8Array {
  if (chunk.payload.length > MAX_FILE_PAYLOAD) {
    throw new PrismProtocolError(
      `payload is ${chunk.payload.length} bytes, exceeds MAX_FILE_PAYLOAD of ${MAX_FILE_PAYLOAD}`,
    );
  }

  const bytes = new Uint8Array(FILE_CHUNK_HEADER_LEN + chunk.payload.length);
  const view = new DataView(bytes.buffer);

  bytes[0] = Channel.File;
  bytes[1] = FileType.Chunk;
  view.setUint32(2, chunk.id, true);
  view.setUint32(6, chunk.index, true);
  bytes.set(chunk.payload, FILE_CHUNK_HEADER_LEN);

  return bytes;
}

/**
 * Parses one piece of a file.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FileChunk} The decoded chunk, its payload copied out.
 * @throws {PrismProtocolError} If the packet is malformed or oversized.
 *
 * @example
 * decodeFileChunk(encodeFileChunk(chunk)).index; // 2
 */
export function decodeFileChunk(bytes: Uint8Array): FileChunk {
  expectFileType(bytes, FileType.Chunk, FILE_CHUNK_HEADER_LEN);

  const payload = bytes.slice(FILE_CHUNK_HEADER_LEN);
  if (payload.length > MAX_FILE_PAYLOAD) {
    throw new PrismProtocolError(
      `payload is ${payload.length} bytes, exceeds MAX_FILE_PAYLOAD of ${MAX_FILE_PAYLOAD}`,
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

  return {
    id: view.getUint32(2, true),
    index: view.getUint32(6, true),
    payload,
  };
}

/**
 * What the receiver has, as carried on {@link Channel.File}.
 *
 * Two numbers do the whole of the repair. `have` is how many chunks arrived in an unbroken
 * run from the start, so everything below it is settled. `arrived` covers the thirty-two
 * chunks after that: bit *i* set means chunk `have + i` is already here.
 */
export interface FileReport {
  id: number;
  have: number;
  arrived: number;
}

/**
 * Serialises a receiver's report.
 *
 * @param {FileReport} report - Report fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer of exactly `FILE_REPORT_LEN` bytes.
 *
 * @example
 * encodeFileReport({ id: 1, have: 4, arrived: 0b101 }).length; // 14
 */
export function encodeFileReport(report: FileReport): Uint8Array {
  const bytes = new Uint8Array(FILE_REPORT_LEN);
  const view = new DataView(bytes.buffer);

  bytes[0] = Channel.File;
  bytes[1] = FileType.Report;
  view.setUint32(2, report.id, true);
  view.setUint32(6, report.have, true);
  view.setUint32(10, report.arrived, true);

  return bytes;
}

/**
 * Parses a receiver's report.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FileReport} The decoded report.
 * @throws {PrismProtocolError} If the packet is malformed.
 *
 * @example
 * decodeFileReport(encodeFileReport(report)).have; // 4
 */
export function decodeFileReport(bytes: Uint8Array): FileReport {
  expectFileType(bytes, FileType.Report, FILE_REPORT_LEN);

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

  return {
    id: view.getUint32(2, true),
    have: view.getUint32(6, true),
    arrived: view.getUint32(10, true),
  };
}

/**
 * Serialises a request for what the far machine is offering.
 *
 * @returns {Uint8Array} A freshly allocated buffer of exactly `FILE_HEADER_LEN` bytes.
 *
 * @example
 * encodeFileList().length; // 2
 */
export function encodeFileList(): Uint8Array {
  return new Uint8Array([Channel.File, FileType.List]);
}

/**
 * Parses a request for what the far machine is offering.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {void} Nothing; the message carries no fields.
 * @throws {PrismProtocolError} If the packet is not a request.
 *
 * @example
 * decodeFileList(encodeFileList());
 */
export function decodeFileList(bytes: Uint8Array): void {
  expectFileType(bytes, FileType.List, FILE_HEADER_LEN);
}

/** One file the far machine is offering. */
export interface FileEntry {
  size: bigint;
  name: string;
}

/**
 * What the far machine is offering, as carried on {@link Channel.File}.
 *
 * One packet. A folder with more files in it than fit says so with `more` rather than
 * paging: the entries are newest first, so what does not fit is what nobody just put there.
 */
export interface FileListing {
  more: boolean;
  files: FileEntry[];
}

/**
 * Serialises a listing of what this machine is offering.
 *
 * @param {FileListing} listing - Listing fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer holding the listing.
 * @throws {PrismProtocolError} If an entry names something that is not a file name.
 *
 * @example
 * encodeFileListing({ more: false, files: [{ size: 3n, name: 'a.txt' }] }).length; // 19
 */
export function encodeFileListing(listing: FileListing): Uint8Array {
  const encoder = new TextEncoder();
  const names = listing.files.map((file) => {
    if (!plainFileName(file.name)) {
      throw new PrismProtocolError(`${file.name} is not a file name`);
    }

    return encoder.encode(file.name);
  });

  const length =
    FILE_LISTING_FIXED_LEN +
    names.reduce((total, name) => total + FILE_ENTRY_FIXED_LEN + name.length, 0);
  const bytes = new Uint8Array(length);
  const view = new DataView(bytes.buffer);

  bytes[0] = Channel.File;
  bytes[1] = FileType.Listing;
  bytes[2] = listing.more ? 1 : 0;
  view.setUint16(3, listing.files.length, true);

  let at = FILE_LISTING_FIXED_LEN;
  for (const [index, name] of names.entries()) {
    view.setBigUint64(at, (listing.files[index] as FileEntry).size, true);
    bytes[at + 8] = name.length;
    at += FILE_ENTRY_FIXED_LEN;
    bytes.set(name, at);
    at += name.length;
  }

  return bytes;
}

/**
 * Parses a listing of what the far machine is offering.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FileListing} The decoded listing.
 * @throws {PrismProtocolError} If the packet ends inside an entry or names nothing writable.
 *
 * @example
 * decodeFileListing(encodeFileListing(listing)).files.length; // 1
 */
export function decodeFileListing(bytes: Uint8Array): FileListing {
  expectFileType(bytes, FileType.Listing, FILE_LISTING_FIXED_LEN);

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const count = view.getUint16(3, true);
  const files: FileEntry[] = [];
  let at = FILE_LISTING_FIXED_LEN;

  for (let index = 0; index < count; index += 1) {
    if (bytes.length < at + FILE_ENTRY_FIXED_LEN) {
      throw new PrismProtocolError(
        `file packet is ${bytes.length} bytes, needs ${at + FILE_ENTRY_FIXED_LEN}`,
      );
    }

    const size = view.getBigUint64(at, true);
    const length = bytes[at + 8] as number;
    at += FILE_ENTRY_FIXED_LEN;

    if (bytes.length < at + length) {
      throw new PrismProtocolError(
        `file packet is ${bytes.length} bytes, needs ${at + length}`,
      );
    }

    files.push({ size, name: decodeName(bytes.subarray(at, at + length)) });
    at += length;
  }

  return { more: bytes[2] !== 0, files };
}

/**
 * A request for one of the files the far machine offered.
 *
 * What comes back is an offer for it, which is the same conversation a file sent the other
 * way starts with — so a file only ever moves one way through this protocol.
 */
export interface FileAsk {
  name: string;
}

/**
 * Serialises a request for one offered file.
 *
 * @param {FileAsk} ask - Request fields to encode.
 * @returns {Uint8Array} A freshly allocated buffer holding the request.
 * @throws {PrismProtocolError} If the name is not a file name.
 *
 * @example
 * encodeFileAsk({ name: 'a.txt' }).length; // 8
 */
export function encodeFileAsk(ask: FileAsk): Uint8Array {
  if (!plainFileName(ask.name)) {
    throw new PrismProtocolError(`${ask.name} is not a file name`);
  }

  const name = new TextEncoder().encode(ask.name);
  const bytes = new Uint8Array(FILE_ASK_FIXED_LEN + name.length);

  bytes[0] = Channel.File;
  bytes[1] = FileType.Ask;
  bytes[2] = name.length;
  bytes.set(name, FILE_ASK_FIXED_LEN);

  return bytes;
}

/**
 * Parses a request for one offered file.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {FileAsk} The decoded request.
 * @throws {PrismProtocolError} If the packet is malformed or names nothing writable.
 *
 * @example
 * decodeFileAsk(encodeFileAsk({ name: 'a.txt' })).name; // 'a.txt'
 */
export function decodeFileAsk(bytes: Uint8Array): FileAsk {
  expectFileType(bytes, FileType.Ask, FILE_ASK_FIXED_LEN);

  const length = bytes[2] as number;
  const needed = FILE_ASK_FIXED_LEN + length;

  if (bytes.length < needed) {
    throw new PrismProtocolError(`file packet is ${bytes.length} bytes, needs ${needed}`);
  }

  return { name: decodeName(bytes.subarray(FILE_ASK_FIXED_LEN, needed)) };
}

/**
 * Reads a file name out of the bytes that carried it.
 *
 * @param {Uint8Array} bytes - The name's UTF-8 bytes.
 * @returns {string} The name.
 * @throws {PrismProtocolError} If it is not UTF-8 or is not a name a file could be given.
 *
 * @example
 * decodeName(new TextEncoder().encode('a.txt')); // 'a.txt'
 */
function decodeName(bytes: Uint8Array): string {
  let name: string;

  try {
    name = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch {
    throw new PrismProtocolError('not utf-8 is not a file name');
  }

  if (!plainFileName(name)) {
    throw new PrismProtocolError(`${name} is not a file name`);
  }

  return name;
}
