import { spawn, type ChildProcessByStdio } from 'node:child_process';
import type { Readable } from 'node:stream';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import type { Session, Settings, StreamState, StreamStats, StreamTerms } from './api.js';

/**
 * The stream, as a process of its own.
 *
 * On macOS a window must be driven from the process's main thread, and Electron already owns
 * that thread — so a stream window inside this process cannot exist. That constraint pushes
 * towards the arrangement that was better anyway: the capture, decode and present path runs
 * somewhere Electron cannot stall it, in a process that can crash without taking the interface
 * with it, and the rule that a frame never enters V8 holds by construction rather than by
 * discipline.
 */

/** How many lines of the process's output to keep, which is what explains a failure. */
const LOG_LINES = 40;

/**
 * How long a stream is given to end politely before it is ended for it.
 *
 * SDL takes the terminate signal for itself and turns it into a quit event, which the stream
 * only reads between frames — so a stream whose host has gone quiet does not see it until its
 * own idle timeout expires, ten seconds later. Somebody who has ended a session should not
 * still be looking at the other machine's screen, so the wait is short and then it is over.
 */
const GRACE_MS = 1200;

const here = dirname(fileURLToPath(import.meta.url));

/**
 * Finds the headless client binary.
 *
 * Three places, in the order they should win: an explicit override for somebody testing a
 * build, the copy packaged beside the application, and the workspace's own build for a
 * development run.
 *
 * @returns {string} The path to the binary.
 * @throws {Error} If none of them exists, naming where it looked — a missing binary is a
 * packaging mistake and the paths are what identify which one.
 */
export function findBinary(): string {
  const name = process.platform === 'win32' ? 'prism-cli.exe' : 'prism-cli';

  const candidates = [
    process.env['PRISM_CLI'],
    join(process.resourcesPath ?? here, name),
    join(here, '..', '..', '..', 'target', 'release', name),
    join(here, '..', '..', '..', 'target', 'debug', name),
  ].filter((path): path is string => typeof path === 'string' && path.length > 0);

  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }

  throw new Error(`could not find ${name}; looked in ${candidates.join(', ')}`);
}

/** Runs one stream and reports what it is doing. */
export class Stream {
  /**
   * The running process, or `null` when nothing is streaming.
   *
   * Typed for the exact `stdio` it is spawned with: nothing goes in, and both output streams
   * are read. A wider type would let a line be written to a standard input that is not there.
   */
  private child: ChildProcessByStdio<null, Readable, Readable> | null = null;

  /** What the stream is doing. */
  private phase = 'idle';

  /** Which host is being watched. */
  private host: string | null = null;

  /** The tail of the process's output. */
  private log: string[] = [];

  /** What the two sides agreed to, once the process has said so. */
  private terms: StreamTerms | null = null;

  /** The last figures the process reported, or `null` before the first second is up. */
  private stats: StreamStats | null = null;

  /** When the handshake completed, or `null` before it has. */
  private startedAt: number | null = null;

  /**
   * Whether this end asked the stream to stop.
   *
   * A stream that is killed exits on a signal rather than with a status, which is
   * indistinguishable from a crash unless somebody remembers having asked. Ending a session on
   * purpose is not a failure and must not be reported to a person as one.
   */
  private stopping = false;

  /** Every round trip reported this session, summed, so the mean survives the session. */
  private rttSum = 0;

  /** How many were reported. */
  private rttCount = 0;

  /** Called whenever anything above changes. */
  private readonly onChange: (state: StreamState) => void;

  /** Called once when a session that established has ended. */
  private readonly onFinished: (session: Session) => void;

  /**
   * Creates a stream that reports changes to `onChange`.
   *
   * @param {(state: StreamState) => void} onChange - Called on every state change.
   * @param {(session: Session) => void} onFinished - Called when a session that established
   *   has ended, once, with what it came to.
   */
  constructor(
    onChange: (state: StreamState) => void,
    onFinished: (session: Session) => void,
  ) {
    this.onChange = onChange;
    this.onFinished = onFinished;
  }

  /**
   * Returns what the stream is doing.
   *
   * @returns {StreamState} The current state.
   */
  state(): StreamState {
    return {
      phase: this.phase,
      host: this.host,
      terms: this.terms,
      stats: this.stats,
      log: [...this.log],
    };
  }

  /**
   * Starts a stream onto a host.
   *
   * @param {string} host - The host's public key as hex.
   * @param {string} address - Its address, or empty to use the rendezvous server.
   * @param {Settings} settings - How to connect and whether to send input.
   * @returns {StreamState} What the stream is doing a moment after starting.
   * @throws {Error} If a stream is already running, if the binary cannot be found, or if
   * neither an address nor a rendezvous server was given.
   */
  start(host: string, address: string, settings: Settings): StreamState {
    if (this.child) {
      throw new Error('a stream is already running');
    }

    if (address === '' && settings.rendezvous === '') {
      throw new Error('set a rendezvous server, or give the host address directly');
    }

    const args = ['client', '--display', '--peer-key', host];

    if (address !== '') {
      args.push('--host', address);
    } else {
      args.push('--rendezvous', settings.rendezvous);
    }

    if (!settings.control) {
      args.push('--no-input');
    }
    if (settings.smooth) {
      args.push('--mode', 'smooth');
    }

    // The stream ends when the host stops sending rather than after a fixed number of frames,
    // and a person switching windows is not a reason to give up on it.
    args.push('--idle-timeout-ms', '10000');

    const child = spawn(findBinary(), args, { stdio: ['ignore', 'pipe', 'pipe'] });

    this.child = child;
    this.host = host;
    this.phase = 'connecting';
    this.log = [];
    this.terms = null;
    this.stats = null;
    this.startedAt = null;
    this.rttSum = 0;
    this.rttCount = 0;
    this.stopping = false;

    const absorb = (chunk: Buffer): void => {
      for (const line of chunk.toString('utf8').split('\n')) {
        if (line.trim() === '') {
          continue;
        }

        this.log.push(line);
        if (this.log.length > LOG_LINES) {
          this.log.shift();
        }

        // The headless client says this exactly once, when the handshake completes. Reading
        // it is what turns "a process is running" into "a person is watching a screen".
        if (line.includes('session established')) {
          this.phase = 'streaming';
          this.startedAt ??= Date.now();
        }

        this.terms = readTerms(line) ?? this.terms;

        const figures = readStats(line);
        if (figures) {
          this.stats = figures;
          this.rttSum += figures.rttMs;
          this.rttCount += 1;
        }
      }

      this.onChange(this.state());
    };

    child.stdout.on('data', absorb);
    child.stderr.on('data', absorb);

    child.on('exit', (code) => {
      const ran = this.finish();

      this.child = null;
      this.host = null;
      this.phase = code === 0 || this.stopping ? 'stopped' : 'failed';
      this.onChange(this.state());

      if (ran) {
        this.onFinished(ran);
      }
    });

    child.on('error', (error) => {
      this.child = null;
      this.phase = 'failed';
      this.log.push(error.message);
      this.onChange(this.state());
    });

    this.onChange(this.state());

    return this.state();
  }

  /**
   * Ends the stream, if one is running.
   *
   * @returns {StreamState} The state once the process has been asked to stop.
   */
  stop(): StreamState {
    const child = this.child;

    if (!child) {
      return this.state();
    }

    this.stopping = true;
    child.kill();

    const insist = setTimeout(() => {
      if (this.child === child) {
        child.kill('SIGKILL');
      }
    }, GRACE_MS);

    // Nothing here should hold the application open. A stream that ends on its own before the
    // grace period is up has already cleared this, and one that has not is being killed either
    // way when the process goes.
    insist.unref();
    child.once('exit', () => {
      clearTimeout(insist);
    });

    return this.state();
  }

  /**
   * Turns what just ended into a session, if it was one.
   *
   * A run that never got past the handshake is not history: nothing was watched, and the
   * failure has already been reported as a failure. Calling this twice returns `null` the
   * second time, because the mark it reads is cleared as it goes.
   *
   * @returns {Session | null} What ran, or `null` if nothing did.
   */
  private finish(): Session | null {
    const startedAt = this.startedAt;

    if (startedAt === null || this.host === null) {
      return null;
    }

    this.startedAt = null;

    return {
      host: this.host,
      startedAt,
      endedAt: Date.now(),
      rttMs: this.rttCount > 0 ? this.rttSum / this.rttCount : 0,
      frames: this.stats?.frames ?? 0,
    };
  }
}

/**
 * Reads the line the client prints once, naming what the two sides settled on.
 *
 * Parsed from the process's own output rather than passed back some other way, because the
 * process is where the negotiation happens and its output is already being read. A line that
 * does not match is not an error: most of them are something else.
 *
 * @param {string} line - One line of the process's output.
 * @returns {StreamTerms | null} The terms, or `null` if this line is not that one.
 */
function readTerms(line: string): StreamTerms | null {
  const match =
    /client: terms codec=(\w+) width=(\d+) height=(\d+) fps=(\d+)/.exec(line);

  if (!match) {
    return null;
  }

  // A client that will take whatever the host's screen is says so with the largest number the
  // field holds. That is not a size anybody wants shown to them.
  const width = Number(match[2]);
  const height = Number(match[3]);
  const capped = width < 65_534 && height < 65_534;

  return {
    codec: String(match[1]),
    width: capped ? width : 0,
    height: capped ? height : 0,
    fps: Number(match[4]),
  };
}

/**
 * Reads the line the client prints once a second while it is running.
 *
 * @param {string} line - One line of the process's output.
 * @returns {StreamStats | null} The figures, or `null` if this line is not one of those.
 */
function readStats(line: string): StreamStats | null {
  const match =
    /client: stats rtt_us=(\d+) fps=([\d.]+) kbps=([\d.]+) frames=(\d+)/.exec(line);

  if (!match) {
    return null;
  }

  return {
    rttMs: Number(match[1]) / 1000,
    fps: Number(match[2]),
    mbps: Number(match[3]) / 1000,
    frames: Number(match[4]),
  };
}
