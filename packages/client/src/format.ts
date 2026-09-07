/**
 * Turning measurements into something worth reading.
 *
 * Shared by the two windows that show them, because a figure that is rounded one way in the
 * setup flow and another way on the home window is two different claims about the same thing.
 */

/**
 * Formats a latency for display.
 *
 * A round trip on the loopback is tens of microseconds, and rounding that to a whole number
 * prints `0 ms` — which reads as a measurement that failed rather than as one that is very
 * small. Below a millisecond it is reported as being below one, and below ten it keeps a
 * decimal, because that is the range where a tenth is the difference somebody can feel.
 *
 * @param {number} ms - The measurement, in milliseconds.
 * @returns {string} What to show, without a unit.
 */
export function latency(ms: number): string {
  if (ms < 1) {
    return '<1';
  }

  return ms < 10 ? ms.toFixed(1) : ms.toFixed(0);
}

/** A minute, in milliseconds. */
const MINUTE = 60_000;

/** An hour. */
const HOUR = 60 * MINUTE;

/** A day. */
const DAY = 24 * HOUR;

/**
 * Says how long something lasted.
 *
 * Minutes are padded once there are hours beside them, so that a column of durations lines up
 * on the same character and can be compared by length rather than by reading each one.
 *
 * @param {number} ms - How long, in milliseconds.
 * @returns {string} Something like `2h 06m`, `46m`, or `18s`.
 */
export function span(ms: number): string {
  if (ms < MINUTE) {
    return `${Math.max(Math.round(ms / 1000), 1)}s`;
  }

  const hours = Math.floor(ms / HOUR);
  const minutes = Math.floor((ms % HOUR) / MINUTE);

  return hours > 0 ? `${hours}h ${String(minutes).padStart(2, '0')}m` : `${minutes}m`;
}

/**
 * Says how long ago something was, in the largest unit that still means anything.
 *
 * Past a week the interval stops being what somebody is asking — they want the date — so this
 * hands back to [`when`] rather than counting out thirty-one days.
 *
 * @param {number} at - When it was, in milliseconds since the epoch.
 * @param {number} [now] - What to measure against, for tests.
 * @returns {string} Something like `just now`, `14 minutes ago`, `2 hours ago`, or `Sep 5`.
 */
export function ago(at: number, now: number = Date.now()): string {
  const since = Math.max(now - at, 0);

  if (since < MINUTE) {
    return 'just now';
  }

  if (since < HOUR) {
    const minutes = Math.floor(since / MINUTE);

    return `${minutes} minute${minutes === 1 ? '' : 's'} ago`;
  }

  if (since < DAY) {
    const hours = Math.floor(since / HOUR);

    return `${hours} hour${hours === 1 ? '' : 's'} ago`;
  }

  if (since < 7 * DAY) {
    const days = Math.floor(since / DAY);

    return `${days} day${days === 1 ? '' : 's'} ago`;
  }

  return when(at, now).split(' · ')[0] ?? '';
}

/**
 * Names a moment the way somebody would say it out loud.
 *
 * @param {number} at - When it was, in milliseconds since the epoch.
 * @param {number} [now] - What counts as today, for tests.
 * @returns {string} Something like `Today · 21:04` or `Sep 5 · 18:22`.
 */
export function when(at: number, now: number = Date.now()): string {
  const then = new Date(at);
  const clock = `${String(then.getHours()).padStart(2, '0')}:${String(then.getMinutes()).padStart(2, '0')}`;

  const days = Math.round((midnight(now) - midnight(at)) / DAY);

  if (days === 0) {
    return `Today · ${clock}`;
  }

  if (days === 1) {
    return `Yesterday · ${clock}`;
  }

  const month = then.toLocaleString('en-US', { month: 'short' });

  return `${month} ${then.getDate()} · ${clock}`;
}

/**
 * Returns the start of the day a moment falls in, in local time.
 *
 * Days are counted from midnight rather than by dividing the epoch, because a person's day is
 * where they are and the epoch's is in Greenwich.
 *
 * @param {number} at - Any moment in the day, in milliseconds since the epoch.
 * @returns {number} Midnight at the start of it.
 */
function midnight(at: number): number {
  const day = new Date(at);
  day.setHours(0, 0, 0, 0);

  return day.getTime();
}
