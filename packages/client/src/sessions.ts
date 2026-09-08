import { app } from 'electron';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

import type { Session } from './api.js';

/**
 * What has been watched from this machine, kept between launches.
 *
 * Beside the settings rather than in them, because it grows on its own and settings do not: a
 * file that a person edits should not be one an application appends to every few minutes.
 */

/**
 * How many sessions are kept.
 *
 * Enough that a week of ordinary use is still there, few enough that the file stays something
 * that can be read in one go without thinking about it.
 */
const KEEP = 50;

/**
 * Returns where the history is kept.
 *
 * @returns {string} Absolute path to the history file.
 */
function historyPath(): string {
  return join(app.getPath('userData'), 'sessions.json');
}

/**
 * Reads the history, newest first.
 *
 * A file that cannot be read is treated as an empty one. Losing the record of what was watched
 * is not worth refusing to start over.
 *
 * @returns {Session[]} What has run, most recent first.
 */
export function loadSessions(): Session[] {
  try {
    const stored: unknown = JSON.parse(readFileSync(historyPath(), 'utf8'));

    if (!Array.isArray(stored)) {
      return [];
    }

    return (stored as Session[])
      .filter((one) => typeof one.host === 'string' && typeof one.endedAt === 'number')
      .slice(0, KEEP);
  } catch {
    return [];
  }
}

/**
 * Adds one session to the front of the history and writes it back.
 *
 * @param {readonly Session[]} history - What was there before.
 * @param {Session} session - The one that just ended.
 * @returns {Session[]} The history as it now stands.
 */
export function recordSession(history: readonly Session[], session: Session): Session[] {
  const next = [session, ...history].slice(0, KEEP);

  try {
    const path = historyPath();
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, `${JSON.stringify(next, null, 2)}\n`, 'utf8');
  } catch {
    // The session still happened and the window should still show it. A history that could
    // not be written is a history that will be shorter next launch, which is not a reason to
    // throw at somebody who has just closed a stream.
  }

  return next;
}
