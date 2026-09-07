/**
 * Keeping a session token between runs.
 *
 * A token is a credential: whoever holds it is the account until it expires. So it goes
 * through the operating system's own secret store — Keychain on macOS, DPAPI on Windows,
 * libsecret or KWallet on Linux — rather than into the settings file next to the window size.
 *
 * When no secret store is available, nothing is written at all. Falling back to a plain file
 * would turn a machine with no keyring into the one machine where the token sits in readable
 * text, which is exactly backwards.
 */

import { app, safeStorage } from 'electron';
import { readFileSync, writeFileSync, rmSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

/** What was signed in as, and with what. */
export interface StoredSession {
  /** The address, so the window can say who is signed in before the server answers. */
  readonly email: string;
  /** The token that proves it. */
  readonly token: string;
}

/**
 * Returns where the sealed token is kept.
 *
 * @returns {string} Absolute path to the file.
 */
function sessionPath(): string {
  return join(app.getPath('userData'), 'session.bin');
}

/**
 * Stores a session for the next run, if this machine can seal it.
 *
 * @param {StoredSession} session - What to keep.
 * @returns {boolean} Whether it was stored. `false` means the machine has no secret store and
 *   the next run will ask for a password again.
 */
export function keepSession(session: StoredSession): boolean {
  if (!safeStorage.isEncryptionAvailable()) {
    return false;
  }

  const path = sessionPath();
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, safeStorage.encryptString(JSON.stringify(session)), { mode: 0o600 });

  return true;
}

/**
 * Reads back the session kept by an earlier run.
 *
 * Anything unreadable — no secret store, no file, a file written by a different user or under
 * a key that no longer exists — is treated as nobody being signed in.
 *
 * @returns {StoredSession | null} The stored session, or `null`.
 */
export function storedSession(): StoredSession | null {
  try {
    if (!safeStorage.isEncryptionAvailable()) {
      return null;
    }

    const parsed: unknown = JSON.parse(safeStorage.decryptString(readFileSync(sessionPath())));

    if (
      typeof parsed !== 'object' ||
      parsed === null ||
      typeof (parsed as StoredSession).email !== 'string' ||
      typeof (parsed as StoredSession).token !== 'string'
    ) {
      return null;
    }

    return parsed as StoredSession;
  } catch {
    return null;
  }
}

/**
 * Removes the stored session.
 *
 * @returns {void}
 */
export function forgetSession(): void {
  rmSync(sessionPath(), { force: true });
}
