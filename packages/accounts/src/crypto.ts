/**
 * The small amount of cryptography an account server does.
 *
 * Small on purpose, and that is what lets this run anywhere. The expensive part — turning a
 * password into a secret — happens on the machine the password was typed on:
 * `crates/prism-core/src/account/secret.rs` runs Argon2id there and sends only the derived
 * value. What arrives here is already a 32-byte secret, so recognising it is one hash and a
 * comparison, and a server that never does memory-hard work is a server that fits in a
 * request-scoped runtime.
 *
 * Everything here matches the Rust it replaces byte for byte. The domain strings are part of
 * the format: change one and every existing account stops recognising its own password.
 */

/** Hex, lowercase, which is how every value in this store is written. */
export function hex(bytes: Uint8Array): string {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

/**
 * Reads hex back into bytes.
 *
 * @param {string} text - The hex, of any case.
 * @returns {Uint8Array | null} The bytes, or `null` if it is not hex of an even length.
 */
export function unhex(text: string): Uint8Array | null {
  if (text.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(text)) {
    return null;
  }

  const bytes = new Uint8Array(text.length / 2);

  for (let at = 0; at < bytes.length; at += 1) {
    bytes[at] = Number.parseInt(text.slice(at * 2, at * 2 + 2), 16);
  }

  return bytes;
}

/**
 * SHA-256 of the pieces, concatenated in order.
 *
 * @param {...(Uint8Array | string)} parts - What to hash.
 * @returns {Promise<Uint8Array>} The digest.
 */
export async function sha256(...parts: (Uint8Array | string)[]): Promise<Uint8Array> {
  const encoder = new TextEncoder();
  const chunks = parts.map((part) => (typeof part === 'string' ? encoder.encode(part) : part));
  const total = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  const joined = new Uint8Array(total);

  let at = 0;
  for (const chunk of chunks) {
    joined.set(chunk, at);
    at += chunk.length;
  }

  return new Uint8Array(await crypto.subtle.digest('SHA-256', joined));
}

/**
 * What recognises a correct authentication secret.
 *
 * A hash of a hash. Holding this does not let anybody sign in, and there is no dictionary to
 * run against it: what it hashes is already an Argon2 output.
 *
 * @param {Uint8Array} auth - The 32-byte secret the client derived and sent.
 * @returns {Promise<string>} The verifier, as hex.
 */
export async function verifierOf(auth: Uint8Array): Promise<string> {
  return hex(await sha256('prism-account-verifier-v1', auth));
}

/**
 * Whether two values are the same, in time that does not depend on how much of one is right.
 *
 * A server answering many attempts leaks through how long it takes to say no, unless the
 * comparison refuses to stop early.
 *
 * @param {string} left - One value, as hex.
 * @param {string} right - The other.
 * @returns {boolean} Whether they match.
 */
export function sameSecret(left: string, right: string): boolean {
  if (left.length !== right.length) {
    return false;
  }

  let difference = 0;

  for (let at = 0; at < left.length; at += 1) {
    difference |= left.charCodeAt(at) ^ right.charCodeAt(at);
  }

  return difference === 0;
}

/** The alphabet an authenticator app reads a secret in. */
const BASE32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';

/**
 * Writes bytes in the base32 an authenticator app expects.
 *
 * Unpadded, which is what every authenticator reads and what the `otpauth://` link carries.
 *
 * @param {Uint8Array} bytes - The secret.
 * @returns {string} Its base32.
 */
export function base32(bytes: Uint8Array): string {
  let bits = 0;
  let value = 0;
  let out = '';

  for (const byte of bytes) {
    value = (value << 8) | byte;
    bits += 8;

    while (bits >= 5) {
      out += BASE32[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
  }

  if (bits > 0) {
    out += BASE32[(value << (5 - bits)) & 31];
  }

  return out;
}

/** Reads a base32 secret back into bytes. */
export function unbase32(text: string): Uint8Array | null {
  let bits = 0;
  let value = 0;
  const out: number[] = [];

  for (const character of text.toUpperCase().replace(/=+$/, '')) {
    const index = BASE32.indexOf(character);

    if (index < 0) {
      return null;
    }

    value = (value << 5) | index;
    bits += 5;

    if (bits >= 8) {
      out.push((value >>> (bits - 8)) & 255);
      bits -= 8;
    }
  }

  return new Uint8Array(out);
}

/** How many digits a code has. */
const DIGITS = 6;

/** How long one code lasts, in seconds. */
const STEP = 30;

/** How many steps either side of now are accepted, for clocks that disagree a little. */
const WINDOW = 1;

/**
 * One time-based code for a secret at a moment.
 *
 * @param {Uint8Array} secret - The shared secret.
 * @param {number} counter - Which step, which is the Unix time divided by the step.
 * @returns {Promise<string>} The code, zero padded.
 */
async function codeAt(secret: Uint8Array, counter: number): Promise<string> {
  const message = new Uint8Array(8);
  new DataView(message.buffer).setBigUint64(0, BigInt(counter));

  const key = await crypto.subtle.importKey(
    'raw',
    secret as BufferSource,
    { name: 'HMAC', hash: 'SHA-1' },
    false,
    ['sign'],
  );
  const mac = new Uint8Array(await crypto.subtle.sign('HMAC', key, message as BufferSource));

  const offset = (mac[mac.length - 1] as number) & 0x0f;
  const truncated =
    (((mac[offset] as number) & 0x7f) << 24) |
    ((mac[offset + 1] as number) << 16) |
    ((mac[offset + 2] as number) << 8) |
    (mac[offset + 3] as number);

  return String(truncated % 10 ** DIGITS).padStart(DIGITS, '0');
}

/**
 * Whether a code is one this secret produces around now.
 *
 * Every candidate is computed and compared, and the loop does not stop at the first match, so
 * the answer takes the same time whichever step was right.
 *
 * @param {Uint8Array} secret - The account's shared secret.
 * @param {string} code - The six digits somebody typed.
 * @param {number} nowUnix - The current time in seconds.
 * @returns {Promise<boolean>} Whether it is accepted.
 */
export async function totpMatches(
  secret: Uint8Array,
  code: string,
  nowUnix: number,
): Promise<boolean> {
  const wanted = code.trim();
  const step = Math.floor(nowUnix / STEP);
  let matched = false;

  for (let drift = -WINDOW; drift <= WINDOW; drift += 1) {
    const candidate = await codeAt(secret, step + drift);

    matched = sameSecret(candidate, wanted) || matched;
  }

  return matched;
}

/**
 * The link an authenticator app reads from a QR code.
 *
 * @param {string} email - Whose account it is, shown in the app.
 * @param {string} secret - The shared secret, in base32.
 * @returns {string} The `otpauth://` link.
 */
export function totpUri(email: string, secret: string): string {
  const label = encodeURIComponent(`Prism:${email}`);

  return (
    `otpauth://totp/${label}?secret=${secret}&issuer=Prism` +
    `&algorithm=SHA1&digits=${DIGITS}&period=${STEP}`
  );
}

/**
 * Bytes nobody can predict.
 *
 * @param {number} length - How many.
 * @returns {Uint8Array} The bytes.
 */
export function randomBytes(length: number): Uint8Array {
  return crypto.getRandomValues(new Uint8Array(length));
}
