/**
 * The home window.
 *
 * A list of machines and everything about the one that is picked. The picture of that machine
 * is not here — it is decoded and drawn by a process of its own, which is what keeps a video
 * frame from ever having to become a JavaScript value. What this window has is the numbers
 * that process reports, once a second, and the controls that start and stop it.
 */

import type {
  AccountDeviceView,
  AccountState,
  PrismApi,
  Settings,
  StreamState,
} from './api.js';
import { latency } from './format.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** How many seconds of history a sparkline holds. */
const HISTORY = 26;

/** How tall a sparkline's tallest bar is, in pixels. */
const SPARK_HEIGHT = 28;

/**
 * The ceiling the round-trip sparkline is drawn against, in milliseconds.
 *
 * Fixed rather than taken from the run, because latency is the one figure here where lower is
 * better: scaled to its own maximum, a connection that never left half a millisecond would
 * draw full-height bars and read as a warning. Forty is a little over the point where this
 * stops being usable for a game, so a run that stays near the floor looks like what it is.
 */
const RTT_CEILING = 40;

/** The phases where a stream is running or on its way to running. */
const RUNNING: ReadonlySet<string> = new Set(['connecting', 'streaming']);

/** Every machine this one can reach. */
let machines: readonly string[] = [];

/** What the account knows about them. */
let devices: readonly AccountDeviceView[] = [];

/** This machine's own key, which is not one of the machines it can reach. */
let ownKey = '';

/** The machine whose panel is showing. */
let picked: string | null = null;

/** What this machine is configured to do. */
let settings: Settings | null = null;

/** The last thing the stream said. */
let stream: StreamState = { phase: 'idle', host: null, terms: null, stats: null, log: [] };

/** A minute of each figure, oldest first. */
const history = {
  rtt: [] as number[],
  fps: [] as number[],
  rate: [] as number[],
};

/**
 * Returns an element by id.
 *
 * @param {string} id - The element's id.
 * @returns {HTMLElement} The element.
 * @throws {Error} If the markup does not have it.
 */
function el(id: string): HTMLElement {
  const found = document.getElementById(id);

  if (!found) {
    throw new Error(`the window has no #${id}`);
  }

  return found;
}

/**
 * Shortens a public key to something a person can compare at a glance.
 *
 * @param {string} key - The key, as hex.
 * @returns {string} The first and last few characters.
 */
function short(key: string): string {
  return key.length <= 16 ? key : `${key.slice(0, 6)}…${key.slice(-6)}`;
}

/**
 * Returns what to call a machine.
 *
 * @param {string} key - Its public key, as hex.
 * @returns {string} Its name.
 */
function machineName(key: string): string {
  return devices.find((device) => device.publicKey === key)?.label || short(key);
}

/**
 * Returns where a machine is, as far as this one knows.
 *
 * @param {string} key - Its public key, as hex.
 * @returns {string} Its address, or how it will be found without one.
 */
function machineWhere(key: string): string {
  const address = settings?.addresses[key];

  if (address) {
    return address;
  }

  return settings?.rendezvous ? 'through the rendezvous server' : 'no address yet';
}

/**
 * Returns how a machine is doing: watched, reachable, or neither.
 *
 * @param {string} key - Its public key, as hex.
 * @returns {string} `live`, `idle` or `off`.
 */
function machineState(key: string): string {
  if (stream.host === key && RUNNING.has(stream.phase)) {
    return 'live';
  }

  return settings?.addresses[key] || settings?.rendezvous ? 'idle' : 'off';
}

/* ── The sidebar ──────────────────────────────────────────────────────────────────────── */

/**
 * Draws the machines, filtered by whatever is in the search box.
 *
 * @returns {void}
 */
function drawMachines(): void {
  const list = el('machines');
  const query = (el('search') as HTMLInputElement).value.trim().toLowerCase();

  list.textContent = '';

  const shown = machines.filter(
    (key) => query === '' || machineName(key).toLowerCase().includes(query),
  );

  if (shown.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'machine';
    empty.style.color = 'var(--dim)';
    empty.style.fontSize = '12.5px';
    empty.textContent = machines.length === 0 ? 'None yet' : 'Nothing matches';
    list.append(empty);

    return;
  }

  for (const key of shown) {
    const state = machineState(key);

    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'machine';
    row.dataset['state'] = state;
    row.setAttribute('aria-current', String(key === picked));

    const dot = document.createElement('img');
    dot.src = `assets/status-${state}.svg`;
    dot.alt = '';

    const text = document.createElement('span');
    text.className = 'text';

    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = machineName(key);
    name.title = key;

    const os = document.createElement('span');
    os.className = 'os';
    os.textContent = machineWhere(key);

    text.append(name, os);

    const ping = document.createElement('span');
    ping.className = 'ping';
    ping.textContent =
      state === 'live' && stream.stats ? `${latency(stream.stats.rttMs)} ms` : '—';

    row.append(dot, text, ping);
    row.addEventListener('click', () => {
      picked = key;
      drawMachines();
      drawHead();
    });

    list.append(row);
  }
}

/**
 * Draws who is signed in.
 *
 * @param {AccountState} account - What the main process says.
 * @returns {void}
 */
function drawAccount(account: AccountState): void {
  el('who').textContent = account.name ?? 'Not signed in';
  el('kind').textContent = account.name
    ? account.relayAllowed
      ? 'Relay allowed'
      : 'Direct only'
    : 'This machine only';
}

/* ── The panel ────────────────────────────────────────────────────────────────────────── */

/**
 * Draws the header, the stage and the buttons for whichever machine is picked.
 *
 * @returns {void}
 */
function drawHead(): void {
  const watching = picked !== null && stream.host === picked && RUNNING.has(stream.phase);
  const live = watching && stream.phase === 'streaming';

  el('picked').textContent = picked ? machineName(picked) : 'No device';
  el('picked-where').textContent = picked
    ? `${machineWhere(picked)}${live ? '  ·  direct LAN' : ''}`
    : 'Add one to get started';

  el('live').hidden = !live;
  el('live-text').textContent = stream.stats
    ? `Live · ${latency(stream.stats.rttMs)} ms`
    : 'Live';

  el('disconnect').hidden = !watching;

  const watch = el('watch') as HTMLButtonElement;
  watch.hidden = watching;
  watch.disabled = picked === null;
  watch.textContent = 'Connect';

  el('hud').hidden = !live;

  if (stream.terms) {
    // The size is what the two sides settled on; the rate is what is actually arriving, which
    // is a different question and the more interesting one.
    el('hud-size').textContent = stream.terms.width
      ? `${stream.terms.width} × ${stream.terms.height}`
      : "the host's screen";
    el('hud-fps').textContent = `${stream.terms.fps} fps`;
    el('hud-codec').textContent = stream.terms.codec;
  }

  el('hud-rate').textContent = stream.stats ? `${stream.stats.mbps.toFixed(0)} Mbps` : '—';

  // The likeness stays behind the curtain until a stream is running, and the curtain says why
  // there is nothing moving under it even when there is.
  el('curtain').dataset['live'] = String(live);
  el('curtain-text').textContent = live
    ? `${machineName(picked ?? '')} is on screen in its own window.`
    : watching
      ? 'Opening…'
      : picked === null
        ? 'Pick a machine, and its screen opens in a window of its own.'
        : `Connect, and ${machineName(picked)} opens in a window of its own.`;

  const error = el('curtain-error');
  if (stream.phase === 'failed' && stream.log.length > 0) {
    error.hidden = false;
    error.textContent = stream.log.slice(-4).join('\n');
  } else {
    error.hidden = true;
  }
}

/**
 * Draws the three figures and their history.
 *
 * @returns {void}
 */
function drawMetrics(): void {
  const stats = stream.phase === 'streaming' ? stream.stats : null;

  // Left empty rather than filled with a dash, so the stylesheet can say what an absent
  // figure looks like in one place instead of this one deciding it in another.
  el('m-rtt').textContent = stats ? latency(stats.rttMs) : '';
  el('m-fps').textContent = stats ? stats.fps.toFixed(0) : '';
  el('m-rate').textContent = stats ? stats.mbps.toFixed(0) : '';

  spark('spark-rtt', history.rtt, '77, 232, 176', RTT_CEILING);
  spark('spark-fps', history.fps, '53, 214, 255', null);
  spark('spark-rate', history.rate, '124, 92, 255', null);
}

/**
 * Draws one sparkline.
 *
 * @param {string} id - The sparkline's id.
 * @param {readonly number[]} samples - The history, oldest first.
 * @param {string} rgb - The colour's channels, for the fade along the run.
 * @param {number | null} ceiling - What full height means, or `null` to take it from the run.
 * @returns {void}
 */
function spark(id: string, samples: readonly number[], rgb: string, ceiling: number | null): void {
  const box = el(id);
  box.textContent = '';

  // A run of minimum-height bars is a dotted line, which reads as a measurement of nothing
  // rather than as the absence of one.
  if (samples.length === 0) {
    return;
  }

  const top = ceiling ?? Math.max(...samples);

  for (let at = 0; at < HISTORY; at += 1) {
    const value = samples[samples.length - HISTORY + at];

    const bar = document.createElement('i');
    const share = top > 0 ? Math.min((value ?? 0) / top, 1) : 0;
    bar.style.height = `${Math.round(share * SPARK_HEIGHT)}px`;
    bar.style.background = `rgba(${rgb}, ${(0.25 + (at / HISTORY) * 0.53).toFixed(2)})`;

    box.append(bar);
  }
}

/**
 * Records one second of figures.
 *
 * @param {StreamState} state - What the stream said.
 * @returns {void}
 */
function remember(state: StreamState): void {
  if (state.phase !== 'streaming' || !state.stats) {
    return;
  }

  history.rtt.push(state.stats.rttMs);
  history.fps.push(state.stats.fps);
  history.rate.push(state.stats.mbps);

  for (const run of [history.rtt, history.fps, history.rate]) {
    while (run.length > HISTORY) {
      run.shift();
    }
  }
}

/* ── Wiring ───────────────────────────────────────────────────────────────────────────── */

/**
 * Opens a stream onto the picked machine.
 *
 * @async
 * @returns {Promise<void>}
 */
async function watch(): Promise<void> {
  if (picked === null) {
    return;
  }

  const button = el('watch') as HTMLButtonElement;
  button.disabled = true;

  try {
    draw(await prism.connect(picked, settings?.addresses[picked] ?? ''));
  } catch (error) {
    stream = { ...stream, phase: 'failed', log: [String(error)] };
    draw(stream);
  } finally {
    button.disabled = false;
  }
}

/**
 * Draws everything that depends on the stream.
 *
 * @param {StreamState} state - What the stream said.
 * @returns {void}
 */
function draw(state: StreamState): void {
  stream = state;

  // Picking follows the stream when a stream is what changed, so that starting one from
  // somewhere else does not leave this window looking at a different machine.
  if (state.host && RUNNING.has(state.phase)) {
    picked = state.host;
  }

  remember(state);
  drawMachines();
  drawHead();
  drawMetrics();
}

el('search').addEventListener('input', drawMachines);

el('watch').addEventListener('click', () => {
  void watch();
});

el('disconnect').addEventListener('click', () => {
  void (async () => {
    draw(await prism.disconnect());
  })();
});

el('add').addEventListener('click', () => {
  prism.openSettings();
});

el('account').addEventListener('click', () => {
  prism.openSettings();
});

document.addEventListener('keydown', (event) => {
  if (event.key === 'k' && (event.metaKey || event.ctrlKey)) {
    event.preventDefault();
    el('search').focus();
  }
});

prism.onStream(draw);

// The same seam the setup flow has, for the same reason: a window whose interesting states all
// need a second machine is a window nobody can look at while building it.
Object.defineProperty(window, 'prismHome', { value: { draw } });

void (async () => {
  const [identity, account, stored, state] = await Promise.all([
    prism.identity(),
    prism.accountState(),
    prism.getSettings(),
    prism.streamState(),
  ]);

  ownKey = identity.publicKey;
  settings = stored;
  devices = account.devices;

  // Both sources, minus this machine: one arrives by pairing and the other by signing in, and
  // which of the two brought a machine here is not something anybody wants to read two lists
  // to find out.
  machines = [
    ...new Set([...account.devices.map((device) => device.publicKey), ...identity.hosts]),
  ].filter((key) => key !== ownKey);

  picked = state.host ?? machines[0] ?? null;

  drawAccount(account);
  draw(state);
})();
