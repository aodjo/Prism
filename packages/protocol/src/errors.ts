/**
 * Error raised when a byte sequence does not conform to the Prism wire format.
 *
 * Decoders throw this instead of returning a partially populated object, so a
 * malformed or hostile packet can never reach the pipeline as if it were valid.
 * Callers on the receive path are expected to catch it, count it, and drop the packet.
 *
 * @example
 * try {
 *   decodeVideoPacket(bytes);
 * } catch (err) {
 *   if (err instanceof PrismProtocolError) stats.malformed += 1;
 * }
 */
export class PrismProtocolError extends Error {
  /**
   * Creates a protocol error describing why a packet was rejected.
   *
   * @param {string} message - Human-readable reason the packet failed validation.
   * @returns {PrismProtocolError} The constructed error with its `name` set for instanceof-free checks.
   *
   * @example
   * throw new PrismProtocolError('video packet shorter than VIDEO_HEADER_LEN');
   */
  constructor(message: string) {
    super(message);
    this.name = 'PrismProtocolError';
  }
}
