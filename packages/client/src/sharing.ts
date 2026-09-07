/**
 * Sharing this machine's screen.
 *
 * The other half of the application. One machine both watches and is watched, so the session
 * that hands this screen out lives beside the one that opens somebody else's — one identity,
 * one account, one window.
 *
 * The session itself is native and runs on its own threads. What crosses into JavaScript is a
 * snapshot of counters, five times a second, and nothing else ever will.
 */

import type { HostSnapshot, Settings } from './api.js';

/** How often the panel is told what the session is doing. */
const SNAPSHOT_INTERVAL_MS = 200;

/** The addon, as much of it as sharing needs. */
type Native = typeof import('@prism/native');

/**
 * One machine's willingness to be watched.
 */
export class Sharing {
  /** The addon. */
  private readonly prism: Native;

  /** Told whenever the session's numbers change. */
  private readonly onChange: (snapshot: HostSnapshot | null) => void;

  /** The running session, or `null` when this machine is not shared. */
  private session: InstanceType<Native['Host']> | null = null;

  /** The timer pushing snapshots out. */
  private ticker: NodeJS.Timeout | null = null;

  /**
   * Builds the sharing half over the addon.
   *
   * @param {Native} prism - The addon.
   * @param {(snapshot: HostSnapshot | null) => void} onChange - Told when the numbers change.
   */
  constructor(prism: Native, onChange: (snapshot: HostSnapshot | null) => void) {
    this.prism = prism;
    this.onChange = onChange;
  }

  /**
   * Starts sharing with the stored settings.
   *
   * @param {Settings} settings - What this machine is configured to do.
   * @returns {HostSnapshot | null} What the session is doing a moment after starting.
   * @throws {Error} If nothing is trusted yet, if an address cannot be parsed, or if the
   *   session threads cannot be started. All three are worth showing rather than swallowing.
   */
  start(settings: Settings): HostSnapshot | null {
    if (this.session) {
      return this.snapshot();
    }

    // Built up rather than written out, because the addon's options are genuinely optional and
    // `exactOptionalPropertyTypes` draws the distinction between a field that is absent and one
    // that is present and undefined. Absent is what "be reachable only directly" means.
    const options: import('@prism/native').HostOptions = {
      bind: settings.bind,
      fps: settings.fps,
      bitrateBps: settings.bitrateBps,
      injectInput: settings.control,
    };

    if (settings.rendezvous !== '') {
      options.rendezvous = settings.rendezvous;
    }

    this.session = new this.prism.Host(options);

    this.ticker = setInterval(() => {
      this.onChange(this.snapshot());
    }, SNAPSHOT_INTERVAL_MS);

    return this.snapshot();
  }

  /**
   * Stops sharing, if this machine was.
   *
   * @returns {null} Nothing, which is what the session is now.
   */
  stop(): null {
    if (this.ticker) {
      clearInterval(this.ticker);
      this.ticker = null;
    }

    this.session?.stop();
    this.session = null;

    this.onChange(null);

    return null;
  }

  /**
   * Returns what the session is doing, or `null` when there is none.
   *
   * @returns {HostSnapshot | null} The counters.
   */
  snapshot(): HostSnapshot | null {
    return this.session ? (this.session.snapshot() as HostSnapshot) : null;
  }
}
