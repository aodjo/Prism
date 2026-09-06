import { app } from 'electron';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

import type { Settings } from './api.js';

/**
 * The settings a machine uses before anybody has changed anything.
 *
 * Port zero rather than a fixed one: with a rendezvous server the port is discovered, and
 * pinning it would only matter to somebody forwarding a port by hand, who will set it.
 */
export const DEFAULTS: Settings = {
  rendezvous: '',
  bind: '0.0.0.0:0',
  fps: 60,
  bitrateBps: 24_000_000,
  injectInput: true,
  autoStart: false,
};

/**
 * Returns where settings are kept.
 *
 * Under Electron's per-user data directory rather than beside the application, so an
 * installation shared between accounts gives each of them their own.
 *
 * @returns {string} Absolute path to the settings file.
 */
function settingsPath(): string {
  return join(app.getPath('userData'), 'settings.json');
}

/**
 * Reads the settings, falling back to the defaults for anything missing or unreadable.
 *
 * A corrupt file is treated as an absent one. Refusing to start because a settings file was
 * truncated would be a worse failure than starting with defaults and letting the user notice.
 *
 * @returns {Settings} The stored settings merged over the defaults.
 */
export function loadSettings(): Settings {
  try {
    const stored: unknown = JSON.parse(readFileSync(settingsPath(), 'utf8'));

    if (typeof stored !== 'object' || stored === null) {
      return { ...DEFAULTS };
    }

    return { ...DEFAULTS, ...(stored as Partial<Settings>) };
  } catch {
    return { ...DEFAULTS };
  }
}

/**
 * Writes the settings, creating the directory if this is the first run.
 *
 * @param {Settings} settings - The settings to store.
 * @returns {void}
 * @throws {Error} If the file cannot be written, which the caller surfaces rather than
 * silently continuing with settings that will be gone on the next launch.
 */
export function saveSettings(settings: Settings): void {
  const path = settingsPath();
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(settings, null, 2)}\n`, 'utf8');
}
