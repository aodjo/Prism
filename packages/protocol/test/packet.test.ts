import { describe, expect, it } from 'vitest';

import vectors from '../vectors.json' with { type: 'json' };
import {
  AUDIO_HEADER_LEN,
  MAX_AUDIO_PAYLOAD,
  decodeAudioPacket,
  encodeAudioPacket,
  CLOCK_PING_LEN,
  CLOCK_PONG_LEN,
  CONTROL_HEADER_LEN,
  CURSOR_POSITION_LEN,
  FEC_HEADER_LEN,
  GOODBYE_LEN,
  MAX_FEC_PAYLOAD,
  MAX_PLAINTEXT_SIZE,
  SEAL_OVERHEAD,
  Channel,
  ControlType,
  FEEDBACK_PACKET_LEN,
  INPUT_PACKET_LEN,
  InputKind,
  MouseButton,
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
  decodeCursorPosition,
  decodeFecPacket,
  decodeFeedbackPacket,
  decodeGoodbye,
  decodeInputPacket,
  decodeVideoPacket,
  encodeClockPing,
  encodeClockPong,
  encodeCursorPosition,
  encodeFecPacket,
  encodeGoodbye,
  sliceLenOf,
  encodeFeedbackPacket,
  encodeInputPacket,
  encodeVideoPacket,
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
  decodeFileAnswer,
  decodeFileAsk,
  decodeFileChunk,
  decodeFileList,
  decodeFileListing,
  decodeFileOffer,
  decodeFileReport,
  encodeFileAnswer,
  encodeFileAsk,
  encodeFileChunk,
  encodeFileList,
  encodeFileListing,
  encodeFileOffer,
  encodeFileReport,
  fileTypeOf,
  plainFileName,
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
    expect(CURSOR_POSITION_LEN).toBe(vectors.constants.cursorPositionLen);
    expect(ControlType.CursorPosition).toBe(vectors.controlTypes.cursorPosition);
    expect(GOODBYE_LEN).toBe(vectors.constants.goodbyeLen);
    expect(ControlType.Goodbye).toBe(vectors.controlTypes.goodbye);
  });
});

describe('goodbyes', () => {
  for (const vector of vectors.goodbyes) {
    it(`round-trips ${vector.name}`, () => {
      expect(bytesToHex(encodeGoodbye())).toBe(vector.hex);
      expect(() => decodeGoodbye(hexToBytes(vector.hex))).not.toThrow();
    });
  }
});

describe('input packets', () => {
  it('agrees with vectors.json on the sizes and kinds', () => {
    expect(INPUT_PACKET_LEN).toBe(vectors.constants.inputPacketLen);
    expect(InputKind.MouseMove).toBe(vectors.inputKinds.mouseMove);
    expect(InputKind.MouseButton).toBe(vectors.inputKinds.mouseButton);
    expect(InputKind.MouseScroll).toBe(vectors.inputKinds.mouseScroll);
    expect(InputKind.Key).toBe(vectors.inputKinds.key);
    expect(InputKind.MouseTo).toBe(vectors.inputKinds.mouseTo);
    expect(MouseButton.Left).toBe(vectors.mouseButtons.left);
  });

  for (const vector of vectors.inputPackets) {
    it(`encodes and decodes the ${vector.name} vector`, () => {
      const { kind, x, y, flags } = vector.fields;
      const pressed = (flags & 1) !== 0;

      let event;
      if (kind === InputKind.MouseMove) event = { kind: InputKind.MouseMove as const, dx: x, dy: y };
      else if (kind === InputKind.MouseScroll)
        event = { kind: InputKind.MouseScroll as const, dx: x, dy: y };
      else if (kind === InputKind.MouseButton)
        event = { kind: InputKind.MouseButton as const, button: x as MouseButton, pressed };
      else if (kind === InputKind.MouseTo) event = { kind: InputKind.MouseTo as const, x, y };
      else event = { kind: InputKind.Key as const, usage: x, pressed };

      const packet = { originTsUs: BigInt(vector.fields.originTsUs), event };
      const bytes = encodeInputPacket(packet);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(INPUT_PACKET_LEN);
      expect(decodeInputPacket(hexToBytes(vector.hex))).toEqual(packet);
    });
  }

  it('refuses a button index it does not know', () => {
    const bytes = hexToBytes(vectors.inputPackets[1]!.hex);
    bytes[10] = 9;
    expect(() => decodeInputPacket(bytes)).toThrow(PrismProtocolError);
  });

  it('refuses to aim the pointer off the screen', () => {
    // Packed into two bytes as it stands, 65536 would wrap to zero and put the pointer on the
    // opposite edge from the one it was aimed at.
    for (const x of [-1, 65536, 0.5]) {
      expect(
        () =>
          encodeInputPacket({
            originTsUs: 0n,
            event: { kind: InputKind.MouseTo, x, y: 0 },
          }),
        String(x),
      ).toThrow(PrismProtocolError);
    }
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

describe('cursor position', () => {
  for (const vector of vectors.cursorPositions) {
    it(`encodes and decodes the ${vector.name} vector`, () => {
      const packet = {
        sampleTsUs: BigInt(vector.fields.sampleTsUs),
        x: vector.fields.x,
        y: vector.fields.y,
        screenWidth: vector.fields.screenWidth,
        screenHeight: vector.fields.screenHeight,
      };
      const bytes = encodeCursorPosition(packet);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(CURSOR_POSITION_LEN);
      expect(decodeCursorPosition(hexToBytes(vector.hex))).toEqual(packet);
    });
  }

  it('refuses to encode a screen with no area', () => {
    expect(() =>
      encodeCursorPosition({
        sampleTsUs: 0n,
        x: 0,
        y: 0,
        screenWidth: 0,
        screenHeight: 1080,
      }),
    ).toThrow(PrismProtocolError);
  });

  it('refuses to decode one control message as another', () => {
    const ping = encodeClockPing({ t1Us: 1n });
    expect(() => decodeCursorPosition(ping)).toThrow(PrismProtocolError);
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

  it('at the payload limit fills the plaintext budget, which seals to exactly MAX_PACKET_SIZE', () => {
    const bytes = encodeVideoPacket({
      frameId: 1,
      sliceId: 0,
      pktIdx: 0,
      pktCount: 1,
      flags: 0,
      captureTsUs: 0n,
      payload: new Uint8Array(MAX_VIDEO_PAYLOAD),
    });

    expect(bytes.length).toBe(MAX_PLAINTEXT_SIZE);
    expect(bytes.length + SEAL_OVERHEAD).toBe(MAX_PACKET_SIZE);
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
        flags: vector.fields.flags,
      });

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(FEEDBACK_PACKET_LEN);
    });

    it(`decodes the ${vector.name} vector back to the original fields`, () => {
      const packet = decodeFeedbackPacket(hexToBytes(vector.hex));

      expect(packet.lastFrameId).toBe(vector.fields.lastFrameId);
      expect(packet.recvBitmap).toBe(vector.fields.recvBitmap);
      expect(packet.clientTsUs).toBe(BigInt(vector.fields.clientTsUs));
      expect(packet.flags).toBe(vector.fields.flags);
    });
  }
});

/**
 * Routes a packet to the decoder its channel tag selects.
 *
 * Split out of the rejection test so that a vector on a channel with no branch here fails
 * loudly rather than passing because nothing ran. That is not hypothetical: the parity
 * vectors passed for a while because `channelOf` did not yet know channel five, so they
 * were refused as an unknown channel and would have passed with no parity decoder at all.
 *
 * @param {Uint8Array} bytes - Raw packet, already decrypted.
 * @returns {void} Nothing; it decodes for the side effect of throwing.
 * @throws {PrismProtocolError} Whatever the selected decoder rejects the packet with.
 *
 * @example
 * decodeByChannel(encodeVideoPacket(packet));
 */
function decodeByChannel(bytes: Uint8Array): void {
  const channel = channelOf(bytes);

  switch (channel) {
    case Channel.Video:
      decodeVideoPacket(bytes);
      return;
    case Channel.Feedback:
      decodeFeedbackPacket(bytes);
      return;
    case Channel.Input:
      decodeInputPacket(bytes);
      return;
    case Channel.Fec:
      decodeFecPacket(bytes);
      return;
    case Channel.Control: {
      const type = controlTypeOf(bytes);
      if (type === ControlType.ClockPing) decodeClockPing(bytes);
      else if (type === ControlType.CursorPosition) decodeCursorPosition(bytes);
      else if (type === ControlType.Goodbye) decodeGoodbye(bytes);
      else decodeClockPong(bytes);
      return;
    }
    case Channel.Audio:
      decodeAudioPacket(bytes);
      return;
    case Channel.File: {
      const type = fileTypeOf(bytes);
      if (type === FileType.Offer) decodeFileOffer(bytes);
      else if (type === FileType.Answer) decodeFileAnswer(bytes);
      else if (type === FileType.Chunk) decodeFileChunk(bytes);
      else if (type === FileType.Report) decodeFileReport(bytes);
      else if (type === FileType.List) decodeFileList(bytes);
      else if (type === FileType.Listing) decodeFileListing(bytes);
      else decodeFileAsk(bytes);
      return;
    }
  }
}

describe('audio packet', () => {
  for (const vector of vectors.audioPackets) {
    it(`encodes and decodes the ${vector.name} vector`, () => {
      const packet = {
        sequence: vector.fields.sequence,
        captureTsUs: BigInt(vector.fields.captureTsUs),
        payload: hexToBytes(vector.payloadHex),
      };
      const bytes = encodeAudioPacket(packet);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(AUDIO_HEADER_LEN + packet.payload.length);
      expect(decodeAudioPacket(hexToBytes(vector.hex))).toEqual(packet);
    });
  }

  it('rejects a frame larger than MAX_AUDIO_PAYLOAD', () => {
    expect(() =>
      encodeAudioPacket({
        sequence: 0,
        captureTsUs: 0n,
        payload: new Uint8Array(MAX_AUDIO_PAYLOAD + 1),
      }),
    ).toThrow(PrismProtocolError);
  });

  it('rejects a packet from another channel', () => {
    const bytes = encodeAudioPacket({
      sequence: 0,
      captureTsUs: 0n,
      payload: new Uint8Array(4),
    });
    bytes[0] = Channel.Video;

    expect(() => decodeAudioPacket(bytes)).toThrow(PrismProtocolError);
  });
});

describe('fec packet', () => {
  for (const vector of vectors.fecPackets) {
    it(`encodes and decodes the ${vector.name} vector`, () => {
      const packet = {
        frameId: vector.fields.frameId,
        sliceId: vector.fields.sliceId,
        dataCount: vector.fields.dataCount,
        parityCount: vector.fields.parityCount,
        shardIndex: vector.fields.shardIndex,
        tailLen: vector.fields.tailLen,
        captureTsUs: BigInt(vector.fields.captureTsUs),
        payload: hexToBytes(vector.payloadHex),
      };
      const bytes = encodeFecPacket(packet);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(FEC_HEADER_LEN + packet.payload.length);
      expect(decodeFecPacket(hexToBytes(vector.hex))).toEqual(packet);
    });
  }

  it('agrees with vectors.json on the sizes and the channel', () => {
    expect(FEC_HEADER_LEN).toBe(vectors.constants.fecHeaderLen);
    expect(MAX_FEC_PAYLOAD).toBe(vectors.constants.maxFecPayload);
    expect(AUDIO_HEADER_LEN).toBe(vectors.constants.audioHeaderLen);
    expect(MAX_AUDIO_PAYLOAD).toBe(vectors.constants.maxAudioPayload);
    expect(Channel.Fec).toBe(vectors.channels.fec);
  });

  it('a parity shard is exactly as long as the data shards it repairs', () => {
    // The reason the header is capped at 20 bytes. A longer header would leave room for
    // less than a full shard and Reed-Solomon needs every shard the same length.
    expect(MAX_FEC_PAYLOAD).toBe(MAX_VIDEO_PAYLOAD);
    expect(FEC_HEADER_LEN + MAX_FEC_PAYLOAD).toBe(MAX_PLAINTEXT_SIZE);
    expect(MAX_PLAINTEXT_SIZE + SEAL_OVERHEAD).toBe(MAX_PACKET_SIZE);
  });

  it('recovers the slice length without the packet that would have carried it', () => {
    // The whole reason tailLen exists: a receiver learns a slice's length from its final
    // packet, so a slice whose final packet was lost and rebuilt from parity would yield
    // an empty bitstream with no error to say so.
    const packet = {
      frameId: 1,
      sliceId: 0,
      dataCount: 4,
      parityCount: 1,
      shardIndex: 0,
      tailLen: 37,
      captureTsUs: 0n,
      payload: new Uint8Array([1, 2, 3]),
    };

    expect(sliceLenOf(packet)).toBe(3 * MAX_VIDEO_PAYLOAD + 37);
  });

  it('refuses a shard index that is not below the parity count', () => {
    expect(() =>
      encodeFecPacket({
        frameId: 0,
        sliceId: 0,
        dataCount: 4,
        parityCount: 2,
        shardIndex: 2,
        tailLen: 1,
        captureTsUs: 0n,
        payload: new Uint8Array([0]),
      }),
    ).toThrow(PrismProtocolError);
  });
});

describe('malformed packets are rejected', () => {
  for (const vector of vectors.rejects) {
    it(`rejects ${vector.name} (${vector.reason})`, () => {
      const bytes = hexToBytes(vector.hex);

      expect(() => decodeByChannel(bytes)).toThrow(PrismProtocolError);
    });
  }

  it('rejects each vector through its own decoder, not through the channel tag', () => {
    // The guard against the vacuous pass above: every reject vector whose channel tag is
    // one this build knows must survive channelOf and be refused further down.
    for (const vector of vectors.rejects) {
      const bytes = hexToBytes(vector.hex);
      const tag = bytes.at(0);

      if (tag === undefined || tag > Channel.File) {
        continue;
      }

      expect(() => channelOf(bytes), `${vector.name} names a known channel`).not.toThrow();
    }
  });
});

describe('file channel', () => {
  it('agrees with the vectors about its constants', () => {
    expect(Channel.File).toBe(vectors.channels.file);
    expect(FileType.Offer).toBe(vectors.fileTypes.offer);
    expect(FileType.Answer).toBe(vectors.fileTypes.answer);
    expect(FileType.Chunk).toBe(vectors.fileTypes.chunk);
    expect(FileType.Report).toBe(vectors.fileTypes.report);
    expect(FileType.List).toBe(vectors.fileTypes.list);
    expect(FileType.Listing).toBe(vectors.fileTypes.listing);
    expect(FileType.Ask).toBe(vectors.fileTypes.ask);

    expect(FileRefusal.Declined).toBe(vectors.fileRefusals.declined);
    expect(FileRefusal.TooLarge).toBe(vectors.fileRefusals.tooLarge);
    expect(FileRefusal.BadName).toBe(vectors.fileRefusals.badName);
    expect(FileRefusal.NotWritable).toBe(vectors.fileRefusals.notWritable);

    expect(FILE_HEADER_LEN).toBe(vectors.constants.fileHeaderLen);
    expect(FILE_CHUNK_HEADER_LEN).toBe(vectors.constants.fileChunkHeaderLen);
    expect(MAX_FILE_PAYLOAD).toBe(vectors.constants.maxFilePayload);
    expect(FILE_OFFER_FIXED_LEN).toBe(vectors.constants.fileOfferFixedLen);
    expect(FILE_ANSWER_LEN).toBe(vectors.constants.fileAnswerLen);
    expect(FILE_REPORT_LEN).toBe(vectors.constants.fileReportLen);
    expect(FILE_LISTING_FIXED_LEN).toBe(vectors.constants.fileListingFixedLen);
    expect(FILE_ENTRY_FIXED_LEN).toBe(vectors.constants.fileEntryFixedLen);
    expect(FILE_ASK_FIXED_LEN).toBe(vectors.constants.fileAskFixedLen);
    expect(MAX_FILE_NAME).toBe(vectors.constants.maxFileName);
  });

  for (const vector of vectors.fileOffers) {
    it(`encodes and decodes the ${vector.name} offer`, () => {
      const offer = {
        id: vector.fields.id,
        size: BigInt(vector.fields.size),
        chunks: vector.fields.chunks,
        name: vector.fields.name,
      };
      const bytes = encodeFileOffer(offer);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(decodeFileOffer(hexToBytes(vector.hex))).toEqual(offer);
    });
  }

  for (const vector of vectors.fileAnswers) {
    it(`encodes and decodes the ${vector.name} answer`, () => {
      const answer = {
        id: vector.fields.id,
        accepted: vector.fields.accepted,
        refusal: vector.fields.refusal as FileRefusal,
      };
      const bytes = encodeFileAnswer(answer);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(FILE_ANSWER_LEN);
      expect(decodeFileAnswer(hexToBytes(vector.hex))).toEqual(answer);
    });
  }

  for (const vector of vectors.fileChunks) {
    it(`encodes and decodes the ${vector.name} chunk`, () => {
      const chunk = {
        id: vector.fields.id,
        index: vector.fields.index,
        payload: hexToBytes(vector.payloadHex),
      };
      const bytes = encodeFileChunk(chunk);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(decodeFileChunk(hexToBytes(vector.hex))).toEqual(chunk);
    });
  }

  for (const vector of vectors.fileReports) {
    it(`encodes and decodes the ${vector.name} report`, () => {
      const report = {
        id: vector.fields.id,
        have: vector.fields.have,
        arrived: vector.fields.arrived,
      };
      const bytes = encodeFileReport(report);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(bytes.length).toBe(FILE_REPORT_LEN);
      expect(decodeFileReport(hexToBytes(vector.hex))).toEqual(report);
    });
  }

  for (const vector of vectors.fileLists) {
    it(`encodes and decodes the ${vector.name} request`, () => {
      expect(bytesToHex(encodeFileList())).toBe(vector.hex);
      expect(() => decodeFileList(hexToBytes(vector.hex))).not.toThrow();
    });
  }

  for (const vector of vectors.fileListings) {
    it(`encodes and decodes the ${vector.name} listing`, () => {
      const listing = {
        more: vector.fields.more,
        files: vector.fields.files.map((file) => ({
          size: BigInt(file.size),
          name: file.name,
        })),
      };
      const bytes = encodeFileListing(listing);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(decodeFileListing(hexToBytes(vector.hex))).toEqual(listing);
    });
  }

  for (const vector of vectors.fileAsks) {
    it(`encodes and decodes the ${vector.name} ask`, () => {
      const ask = { name: vector.fields.name };
      const bytes = encodeFileAsk(ask);

      expect(bytesToHex(bytes)).toBe(vector.hex);
      expect(decodeFileAsk(hexToBytes(vector.hex))).toEqual(ask);
    });
  }

  it('refuses to send a name that would be written somewhere else', () => {
    for (const name of ['', '.', '..', 'a/b', 'a\\b', 'a\0b']) {
      expect(plainFileName(name), name).toBe(false);
      expect(() => encodeFileAsk({ name }), name).toThrow(PrismProtocolError);
    }
  });

  it('carries a name that is not spelled in this alphabet', () => {
    const offer = { id: 1, size: 4n, chunks: 1, name: '보고서.txt' };

    expect(decodeFileOffer(encodeFileOffer(offer))).toEqual(offer);
  });

  it('refuses a chunk larger than one packet holds', () => {
    expect(() =>
      encodeFileChunk({ id: 0, index: 0, payload: new Uint8Array(MAX_FILE_PAYLOAD + 1) }),
    ).toThrow(PrismProtocolError);
  });
});
