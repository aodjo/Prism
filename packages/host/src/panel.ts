/**
 * The tray panel.
 *
 * Four sections of text and five inputs. There is no framework here and no bundler, not
 * because either would be wrong but because a build step for this much would break at some
 * point and cost more than it ever saved.
 *
 * Everything it can do comes from `window.prism`, which the preload defines. There is no Node
 * in this context and no way to reach a key.
 */

import type { HostSnapshot, PrismApi, Settings } from './api.js';

declare global {
  interface Window {
    /** The surface the preload exposes. */
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** Whether a pairing exchange is in flight, so a second click cannot start another. */
let pairing = false;

/** What the last snapshot said the phase was, so the button label follows it. */
let phase = 'idle';

/** The phases in which a session is running and the button should offer to stop it. */
const RUNNING = new Set(['opening', 'waiting', 'streaming']);

/** How long the word "Paired" stays on screen before the code is put away. */
const PAIRED_LINGER_MS = 1500;

/**
 * What each phase is called in the panel.
 *
 * Written out rather than derived from the phase string, because these are the words a person
 * reads and they should not change because a name changed in Rust.
 */
const PHASE_LABELS: Record<string, string> = {
  opening: 'Opening',
  waiting: 'Waiting for a client',
  streaming: 'Streaming',
  stopped: 'Not hosting',
  failed: 'Failed',
  idle: 'Not hosting',
};

/**
 * Finds an element by id.
 *
 * @param {string} id - The element's id.
 * @returns {HTMLElement} The element.
 * @throws {Error} If no element has that id, which means the markup and this script have
 * drifted apart and every later line would fail in a less obvious way.
 */
function el(id: string): HTMLElement {
  const found = document.getElementById(id);
  if (!found) {
    throw new Error(`the panel is missing #${id}`);
  }

  return found;
}

/**
 * Finds an input by id.
 *
 * @param {string} id - The input's id.
 * @returns {HTMLInputElement} The input.
 * @throws {Error} If the element is missing or is not an input.
 */
function input(id: string): HTMLInputElement {
  const found = el(id);
  if (!(found instanceof HTMLInputElement)) {
    throw new Error(`#${id} is not an input`);
  }

  return found;
}

/**
 * Finds a button by id.
 *
 * @param {string} id - The button's id.
 * @returns {HTMLButtonElement} The button.
 * @throws {Error} If the element is missing or is not a button.
 */
function button(id: string): HTMLButtonElement {
  const found = el(id);
  if (!(found instanceof HTMLButtonElement)) {
    throw new Error(`#${id} is not a button`);
  }

  return found;
}

/**
 * Shortens a public key for display.
 *
 * A key is sixty-four hex characters and nobody reads all of them. The first and last six are
 * enough to tell two machines apart at a glance and to check against what the other end shows.
 *
 * @param {string} key - The key as hex.
 * @returns {string} An abbreviated form, or the key itself when it is already short.
 */
function short(key: string): string {
  return key.length <= 16 ? key : `${key.slice(0, 6)}…${key.slice(-6)}`;
}

/**
 * Renders a bit rate in the unit it reads best in.
 *
 * @param {bigint} bps - Bits per second.
 * @returns {string} A rate with its unit.
 */
function rate(bps: bigint): string {
  const value = Number(bps);

  if (value >= 1e6) {
    return `${(value / 1e6).toFixed(1)} Mbps`;
  }
  if (value >= 1e3) {
    return `${(value / 1e3).toFixed(0)} kbps`;
  }

  return `${value} bps`;
}

/**
 * Shows a row with a value, or hides it.
 *
 * @param {string} rowId - The row's id.
 * @param {string} valueId - The id of the element holding the value.
 * @param {string | null} value - The value, or `null` to hide the row.
 * @returns {void}
 */
function setRow(rowId: string, valueId: string, value: string | null): void {
  const row = el(rowId);

  if (value === null) {
    row.hidden = true;
    return;
  }

  row.hidden = false;
  el(valueId).textContent = value;
}

/**
 * Reports a failure in one of the panel's two error lines.
 *
 * @param {string} id - Which line to use.
 * @param {unknown} error - Whatever was thrown.
 * @returns {void}
 */
function showError(id: string, error: unknown): void {
  const line = el(id);
  line.hidden = false;
  line.textContent = error instanceof Error ? error.message : String(error);
}

/**
 * Draws a session snapshot.
 *
 * @param {HostSnapshot | null} snapshot - What the session is doing, or `null` when there is
 * no session.
 * @returns {void}
 */
function render(snapshot: HostSnapshot | null): void {
  phase = snapshot?.phase ?? 'idle';

  const dot = el('dot');
  dot.className = 'dot';

  el('phase').textContent = PHASE_LABELS[phase] ?? phase;

  if (phase === 'streaming') {
    dot.classList.add('live');
  } else if (phase === 'waiting' || phase === 'opening') {
    dot.classList.add('waiting');
  } else if (phase === 'failed') {
    dot.classList.add('bad');
  }

  el('toggle').textContent = RUNNING.has(phase) ? 'Stop' : 'Start hosting';

  const streaming = snapshot !== null && phase === 'streaming';

  setRow('peer-row', 'peer', snapshot?.peer ? short(snapshot.peer) : null);
  setRow('observed-row', 'observed', snapshot?.observed ?? null);
  setRow('rate-row', 'rate', streaming ? rate(snapshot.bitrateBps) : null);
  setRow('frames-row', 'frames', streaming ? String(snapshot.frames) : null);

  const error = el('session-error');
  error.hidden = !snapshot?.error;
  error.textContent = snapshot?.error ?? '';

  fit();
}

/**
 * Draws the list of paired machines.
 *
 * @param {readonly string[]} peers - Every paired machine's key, as hex.
 * @returns {void}
 */
function renderPeers(peers: readonly string[]): void {
  const [only] = peers;

  el('peers').textContent =
    peers.length === 0 ? 'None yet' : peers.length === 1 && only ? short(only) : `${peers.length} devices`;
}

/**
 * Loads the stored settings into the inputs.
 *
 * @async
 * @returns {Promise<void>}
 */
async function loadSettings(): Promise<void> {
  const settings = await prism.getSettings();

  input('rendezvous').value = settings.rendezvous;
  input('fps').value = String(settings.fps);
  input('bitrate').value = String(Math.round(settings.bitrateBps / 1e6));
  input('inject').checked = settings.injectInput;
  input('autostart').checked = settings.autoStart;
}

/**
 * Stores one changed setting.
 *
 * @async
 * @param {Partial<Settings>} change - The field that changed.
 * @returns {Promise<void>}
 */
async function save(change: Partial<Settings>): Promise<void> {
  await prism.setSettings(change);
}

/**
 * Runs one pairing exchange, showing the code while it waits.
 *
 * @async
 * @returns {Promise<void>}
 */
async function pair(): Promise<void> {
  if (pairing) {
    return;
  }

  pairing = true;
  button('pair-button').disabled = true;
  el('pair-error').hidden = true;

  try {
    const code = await prism.pairingCode();
    el('pairing-code').textContent = code;
    el('pairing-hint').textContent = 'Type this on the other machine';
    el('pairing').hidden = false;
    fit();

    const settings = await prism.getSettings();
    const { peers } = await prism.awaitPairing(settings.bind, code);

    el('pairing-hint').textContent = 'Paired';
    renderPeers(peers);
    fit();

    setTimeout(() => {
      el('pairing').hidden = true;
    }, PAIRED_LINGER_MS);
  } catch (error) {
    el('pairing').hidden = true;
    showError('pair-error', error);
    fit();
  } finally {
    pairing = false;
    button('pair-button').disabled = false;
  }
}

/**
 * Starts or stops the session, whichever the current phase calls for.
 *
 * @async
 * @returns {Promise<void>}
 */
async function toggle(): Promise<void> {
  try {
    render(RUNNING.has(phase) ? await prism.stopHosting() : await prism.startHosting());
  } catch (error) {
    showError('session-error', error);
  }
}

/**
 * Wires every control to what it changes.
 *
 * @returns {void}
 */
function listen(): void {
  el('toggle').addEventListener('click', () => {
    void toggle();
  });

  button('pair-button').addEventListener('click', () => {
    void pair();
  });

  input('rendezvous').addEventListener('change', (event) => {
    void save({ rendezvous: (event.target as HTMLInputElement).value.trim() });
  });

  input('fps').addEventListener('change', (event) => {
    void save({ fps: Number((event.target as HTMLInputElement).value) });
  });

  input('bitrate').addEventListener('change', (event) => {
    void save({ bitrateBps: Math.round(Number((event.target as HTMLInputElement).value) * 1e6) });
  });

  input('inject').addEventListener('change', (event) => {
    void save({ injectInput: (event.target as HTMLInputElement).checked });
  });

  input('autostart').addEventListener('change', (event) => {
    void save({ autoStart: (event.target as HTMLInputElement).checked });
  });

  prism.onSnapshot(render);
}

/**
 * Tells the main process how tall the content has become.
 *
 * Called after anything that can change the height, rather than on a timer, because a window
 * that resizes itself while a person is looking at it is worse than one that is slightly the
 * wrong size.
 *
 * @returns {void}
 */
function fit(): void {
  prism.fit(document.documentElement.scrollHeight);
}

/**
 * Fills the panel in with what the machine currently is and is doing.
 *
 * @async
 * @returns {Promise<void>}
 */
async function start(): Promise<void> {
  const identity = await prism.identity();

  el('me').textContent = short(identity.publicKey);
  el('me').title = identity.publicKey;
  renderPeers(identity.peers);

  await loadSettings();
  render(await prism.snapshot());
  fit();
}

listen();
void start();
