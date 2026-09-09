import { app } from 'electron';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

import type { Settings } from './api.js';
import { PRISM_ACCOUNT_SERVER, PRISM_RENDEZVOUS } from './rendezvous.js';


/**
 * The settings a machine uses before anybody has changed anything.
 *
 * Control on, because a remote desktop nobody can type on is a screen share. Smoothing off,
 * because it costs latency and latency is what this is for; a person on a jittery connection
 * can turn it on and immediately see the trade.
 */
export const DEFAULTS: Settings = {
  rendezvous: PRISM_RENDEZVOUS,
  accountServer: PRISM_ACCOUNT_SERVER,
  control: true,
  smooth: false,
  nickname: '',
  addresses: {},
  pinned: [],
  // The port is fixed rather than left to the operating system. With a rendezvous server it
  // makes no difference, since the port is discovered either way — but without one, a machine
  // on an operating-system-chosen port is a machine nobody can reach: the other end has no way
  // to learn a number nothing told it.
  bind: '0.0.0.0:47200',
  fps: 60,
  bitrateBps: 24_000_000,
  sharing: false,
};

/**
 * Returns where settings are kept.
 *
 * @returns {string} Absolute path to the settings file.
 */
function settingsPath(): string {
  return join(app.getPath('userData'), 'settings.json');
}

/**
 * Reads the settings, falling back to the defaults for anything missing or unreadable.
 *
 * A corrupt file is treated as an absent one: refusing to start because a settings file was
 * truncated is a worse failure than starting with defaults.
 *
 * @returns {Settings} The stored settings merged over the defaults.
 */
export function loadSettings(): Settings {
  try {
    const stored: unknown = JSON.parse(readFileSync(settingsPath(), 'utf8'));

    if (typeof stored !== 'object' || stored === null) {
      return { ...DEFAULTS };
    }

    const known = stored as Partial<Settings>;
    const kept = { ...DEFAULTS };

    // Only what this version has a name for. A setting that has been taken out of the product
    // is otherwise carried forward in the file for good, read by nothing and written by every
    // save — and the next person to look at the file cannot tell which of its keys still mean
    // anything.
    for (const key of Object.keys(kept) as (keyof Settings)[]) {
      const value = known[key];

      if (value !== undefined) {
        (kept as Record<string, unknown>)[key] = value;
      }
    }

    return kept;
  } catch {
    return { ...DEFAULTS };
  }
}

/**
 * Writes the settings, creating the directory if this is the first run.
 *
 * @param {Settings} settings - The settings to store.
 * @returns {void}
 * @throws {Error} If the file cannot be written.
 */
export function saveSettings(settings: Settings): void {
  const path = settingsPath();
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(settings, null, 2)}\n`, 'utf8');
}
