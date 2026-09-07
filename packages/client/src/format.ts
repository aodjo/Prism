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
