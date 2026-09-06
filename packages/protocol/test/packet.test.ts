import { describe, expect, it } from 'vitest';

import vectors from '../vectors.json' with { type: 'json' };
import {
  CLOCK_PING_LEN,
  CLOCK_PONG_LEN,
  CONTROL_HEADER_LEN,
  Channel,
  ControlType,
  FEEDBACK_PACKET_LEN,
  FORMAT_VERSION,
  MAX_PACKET_SIZE,
  MAX_VIDEO_PAYLOAD,
  PrismProtocolError,
  VIDEO_FLAGS_RESERVED_MASK,
  VIDEO_HEADER_LEN,
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
} from '../src/index.js';

/**
 * Converts a lowercase hex string into the bytes it represents.
 *
 * Used to turn the fixtures in `vectors.json` into buffers. An empty string yields an
 * empty array, which the zero-payload vectors rely on.
 *
 * @param {string} hex - Hex string of even length, without separators or a `0x` prefix.
 * @returns {Uint8Array} The decoded bytes, of length `hex.length / 2`.
 *
 * @example
 * hexToBytes('aabbcc'); // Uint8Array [ 170, 187, 204 ]
 */
function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/**
 * Renders bytes as a lowercase hex string for comparison against the fixtures.
 *
 * @param {Uint8Array} bytes - Bytes to render.
 * @returns {string} Lowercase hex, two characters per byte, with no separators.
 *
 * @example
 * bytesToHex(new Uint8Array([0xaa, 0xbb])); // 'aabb'
 */
function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

describe('constants match the shared vectors', () => {
  it('agrees with vectors.json on every size and tag', () => {
    expect(FORMAT_VERSION).toBe(vectors.formatVersion);
    expect(MAX_PACKET_SIZE).toBe(vectors.constants.maxPacketSize);
    expect(VIDEO_HEADER_LEN).toBe(vectors.constants.videoHeaderLen);
    expect(MAX_VIDEO_PAYLOAD).toBe(vectors.constants.maxVideoPayload);
    expect(FEEDBACK_PACKET_LEN).toBe(vectors.constants.feedbackPacketLen);
    expect(VIDEO_FLAGS_RESERVED_MASK).toBe(vectors.videoFlags.reservedMask);

    expect(Channel.Control).toBe(vectors.channels.control);
    expect(Channel.Video).toBe(vectors.channels.video);
    expect(Channel.Audio).toBe(vectors.channels.audio);
    expect(Channel.Input).toBe(vectors.channels.input);
    expect(Channel.Feedback).toBe(vectors.channels.feedback);

    expect(CONTROL_HEADER_LEN).toBe(vectors.constants.controlHeaderLen);
    expect(CLOCK_PING_LEN).toBe(vectors.constants.clockPingLen);
    expect(CLOCK_PONG_LEN).toBe(vectors.constants.clockPongLen);
    expect(ControlType.ClockPing).toBe(vectors.controlTypes.clockPing);
    expect(ControlType.ClockPong).toBe(vectors.controlTypes.clockPong);
  });
});

describe('clock synchronisation packets', () => {
  for (const vector of vectors.clockPings) {
    it(`encodes and decodes the ${vector.name} ping`, () => {
      const bytes = encodeClockPing({ t1Us: BigInt(vector.fields.t1Us) });

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(CLOCK_PING_LEN);
      expect(decodeClockPing(hexToBytes(vector.hex)).t1Us).toBe(BigInt(vector.fields.t1Us));
    });
  }

  for (const vector of vectors.clockPongs) {
    it(`encodes and decodes the ${vector.name} pong`, () => {
      const packet = {
        t1Us: BigInt(vector.fields.t1Us),
        t2Us: BigInt(vector.fields.t2Us),
        t3Us: BigInt(vector.fields.t3Us),
      };
      const bytes = encodeClockPong(packet);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(CLOCK_PONG_LEN);
      expect(decodeClockPong(hexToBytes(vector.hex))).toEqual(packet);
    });
  }

  it('reads the control type before validating the rest', () => {
    expect(controlTypeOf(new Uint8Array([0, 0]))).toBe(ControlType.ClockPing);
    expect(controlTypeOf(new Uint8Array([0, 1]))).toBe(ControlType.ClockPong);
  });

  it('refuses to decode one control message as another', () => {
    const ping = encodeClockPing({ t1Us: 1n });
    expect(() => decodeClockPong(ping)).toThrow(PrismProtocolError);
  });
});

describe('video packet', () => {
  for (const vector of vectors.videoPackets) {
    it(`encodes the ${vector.name} vector to the exact expected bytes`, () => {
      const bytes = encodeVideoPacket({
        frameId: vector.fields.frameId,
        sliceId: vector.fields.sliceId,
        pktIdx: vector.fields.pktIdx,
        pktCount: vector.fields.pktCount,
        flags: vector.fields.flags,
        captureTsUs: BigInt(vector.fields.captureTsUs),
        payload: hexToBytes(vector.fields.payloadHex),
      });

      expect(bytesToHex(bytes)).toBe(vector.hex);
    });

    it(`decodes the ${vector.name} vector back to the original fields`, () => {
      const packet = decodeVideoPacket(hexToBytes(vector.hex));

      expect(packet.frameId).toBe(vector.fields.frameId);
      expect(packet.sliceId).toBe(vector.fields.sliceId);
      expect(packet.pktIdx).toBe(vector.fields.pktIdx);
      expect(packet.pktCount).toBe(vector.fields.pktCount);
      expect(packet.flags).toBe(vector.fields.flags);
      expect(packet.captureTsUs).toBe(BigInt(vector.fields.captureTsUs));
      expect(bytesToHex(packet.payload)).toBe(vector.fields.payloadHex);
    });
  }

  it('rejects a payload larger than MAX_VIDEO_PAYLOAD', () => {
    expect(() =>
      encodeVideoPacket({
        frameId: 1,
        sliceId: 0,
        pktIdx: 0,
        pktCount: 1,
        flags: 0,
        captureTsUs: 0n,
        payload: new Uint8Array(MAX_VIDEO_PAYLOAD + 1),
      }),
    ).toThrow(PrismProtocolError);
  });

  it('never produces a packet larger than MAX_PACKET_SIZE at the payload limit', () => {
    const bytes = encodeVideoPacket({
      frameId: 1,
      sliceId: 0,
      pktIdx: 0,
      pktCount: 1,
      flags: 0,
      captureTsUs: 0n,
      payload: new Uint8Array(MAX_VIDEO_PAYLOAD),
    });

    expect(bytes.length).toBe(MAX_PACKET_SIZE);
  });

  it('rejects encoding with a reserved flag bit set', () => {
    expect(() =>
      encodeVideoPacket({
        frameId: 1,
        sliceId: 0,
        pktIdx: 0,
        pktCount: 1,
        flags: 0x08,
        captureTsUs: 0n,
        payload: new Uint8Array(0),
      }),
    ).toThrow(PrismProtocolError);
  });

  it('rejects a field that does not fit its wire width', () => {
    expect(() =>
      encodeVideoPacket({
        frameId: 1,
        sliceId: 0x10000,
        pktIdx: 0,
        pktCount: 1,
        flags: 0,
        captureTsUs: 0n,
        payload: new Uint8Array(0),
      }),
    ).toThrow(PrismProtocolError);
  });
});

describe('feedback packet', () => {
  for (const vector of vectors.feedbackPackets) {
    it(`encodes the ${vector.name} vector to the exact expected bytes`, () => {
      const bytes = encodeFeedbackPacket({
        lastFrameId: vector.fields.lastFrameId,
        recvBitmap: vector.fields.recvBitmap,
        clientTsUs: BigInt(vector.fields.clientTsUs),
      });

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(FEEDBACK_PACKET_LEN);
    });

    it(`decodes the ${vector.name} vector back to the original fields`, () => {
      const packet = decodeFeedbackPacket(hexToBytes(vector.hex));

      expect(packet.lastFrameId).toBe(vector.fields.lastFrameId);
      expect(packet.recvBitmap).toBe(vector.fields.recvBitmap);
      expect(packet.clientTsUs).toBe(BigInt(vector.fields.clientTsUs));
    });
  }
});

describe('malformed packets are rejected', () => {
  for (const vector of vectors.rejects) {
    it(`rejects ${vector.name} (${vector.reason})`, () => {
      const bytes = hexToBytes(vector.hex);

      expect(() => {
        const channel = channelOf(bytes);
        if (channel === Channel.Video) decodeVideoPacket(bytes);
        else if (channel === Channel.Feedback) decodeFeedbackPacket(bytes);
        else if (channel === Channel.Control) {
          const type = controlTypeOf(bytes);
          if (type === ControlType.ClockPing) decodeClockPing(bytes);
          else decodeClockPong(bytes);
        }
      }).toThrow(PrismProtocolError);
    });
  }
});
