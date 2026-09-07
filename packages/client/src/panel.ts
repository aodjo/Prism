/**
 * The client window.
 *
 * A list of paired hosts, a way to add one, and three settings. Everything it can do comes
 * from `window.prism`, which the preload defines; there is no Node in this context and no way
 * to reach a key.
 *
 * The stream itself is not here. It runs in a process of its own, and this window only starts
 * it, stops it, and reports what it said.
 */

import type { PrismApi, Settings, StreamState } from './api.js';

declare global {
  interface Window {
    /** The surface the preload exposes. */
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** The phases in which a stream is running and can be stopped. */
const RUNNING = new Set(['connecting', 'streaming']);

/** What each phase is called in the window. */
const PHASE_LABELS: Record<string, string> = {
  idle: 'Not connected',
  connecting: 'Connecting',
  streaming: 'Streaming',
  stopped: 'Not connected',
  failed: 'Disconnected',
};

/** What the last state said the phase was. */
let phase = 'idle';

/** Whether a pairing exchange is in flight, so a second click cannot start another. */
let pairing = false;

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
    throw new Error(`the window is missing #${id}`);
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
 * @param {string} key - The key as hex.
 * @returns {string} An abbreviated form, or the key itself when it is already short.
 */
function short(key: string): string {
  return key.length <= 16 ? key : `${key.slice(0, 6)}…${key.slice(-6)}`;
}

/**
 * Tells the main process how tall the content has become.
 *
 * @returns {void}
 */
function fit(): void {
  prism.fit(document.documentElement.scrollHeight);
}

/**
 * Reports a failure in one of the window's two error lines.
 *
 * @param {string} id - Which line to use.
 * @param {unknown} error - Whatever was thrown.
 * @returns {void}
 */
function showError(id: string, error: unknown): void {
  const line = el(id);
  line.hidden = false;
  line.textContent = error instanceof Error ? error.message : String(error);
  fit();
}

/**
 * Draws the stream's state.
 *
 * @param {StreamState} state - What the stream is doing.
 * @returns {void}
 */
function render(state: StreamState): void {
  phase = state.phase;

  const dot = el('dot');
  dot.className = 'dot';

  if (phase === 'streaming') {
    dot.classList.add('live');
  } else if (phase === 'connecting') {
    dot.classList.add('waiting');
  } else if (phase === 'failed') {
    dot.classList.add('bad');
  }

  const label = PHASE_LABELS[phase] ?? phase;
  el('phase').textContent = state.host ? `${label} — ${short(state.host)}` : label;

  button('disconnect').hidden = !RUNNING.has(phase);

  // Only a failure gets its log shown. A working stream's output is noise a person did not ask
  // for; a failed one's is the only thing that says why.
  const error = el('stream-error');
  if (phase === 'failed' && state.log.length > 0) {
    error.hidden = false;
    error.textContent = state.log.slice(-6).join('\n');
  } else {
    error.hidden = true;
  }

  renderHosts();
  fit();
}

/** Every host this machine has paired with. */
let hosts: readonly string[] = [];

/**
 * Draws the list of paired hosts.
 *
 * @returns {void}
 */
function renderHosts(): void {
  const list = el('hosts');
  list.textContent = '';

  if (hosts.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = 'No host paired yet';
    list.append(empty);

    return;
  }

  for (const host of hosts) {
    const row = document.createElement('div');
    row.className = 'row';

    const name = document.createElement('code');
    name.textContent = short(host);
    name.title = host;

    const connect = document.createElement('button');
    connect.className = 'primary';
    connect.textContent = 'Connect';
    connect.disabled = RUNNING.has(phase);
    connect.addEventListener('click', () => {
      void start(host);
    });

    row.append(name, connect);
    list.append(row);
  }
}

/**
 * Opens a stream onto a host.
 *
 * @async
 * @param {string} host - The host's public key as hex.
 * @returns {Promise<void>}
 */
async function start(host: string): Promise<void> {
  el('stream-error').hidden = true;

  try {
    render(await prism.connect(host, ''));
  } catch (error) {
    showError('stream-error', error);
  }
}

/**
 * Runs one pairing exchange with the address and code that were typed.
 *
 * @async
 * @returns {Promise<void>}
 */
async function pair(): Promise<void> {
  if (pairing) {
    return;
  }

  const address = input('pair-address').value.trim();
  const code = input('pair-code').value.trim();

  el('pair-error').hidden = true;

  if (address === '' || code === '') {
    showError('pair-error', new Error('the host shows both an address and a code'));
    return;
  }

  pairing = true;
  button('pair-button').disabled = true;
  button('pair-button').textContent = 'Pairing…';

  try {
    const result = await prism.pair(address, code);
    hosts = result.hosts;

    input('pair-address').value = '';
    input('pair-code').value = '';
    renderHosts();
    fit();
  } catch (error) {
    showError('pair-error', error);
  } finally {
    pairing = false;
    button('pair-button').disabled = false;
    button('pair-button').textContent = 'Pair';
  }
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
  input('control').checked = settings.control;
  input('smooth').checked = settings.smooth;
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
 * Wires every control to what it changes.
 *
 * @returns {void}
 */
function listen(): void {
  button('disconnect').addEventListener('click', () => {
    void (async () => {
      render(await prism.disconnect());
    })();
  });

  button('pair-button').addEventListener('click', () => {
    void pair();
  });

  input('rendezvous').addEventListener('change', (event) => {
    void save({ rendezvous: (event.target as HTMLInputElement).value.trim() });
  });

  input('control').addEventListener('change', (event) => {
    void save({ control: (event.target as HTMLInputElement).checked });
  });

  input('smooth').addEventListener('change', (event) => {
    void save({ smooth: (event.target as HTMLInputElement).checked });
  });

  prism.onStream(render);
}

/**
 * Fills the window in with what the machine currently is and is doing.
 *
 * @async
 * @returns {Promise<void>}
 */
async function begin(): Promise<void> {
  const identity = await prism.identity();

  el('me').textContent = short(identity.publicKey);
  el('me').title = identity.publicKey;
  hosts = identity.hosts;

  await loadSettings();
  render(await prism.streamState());
}

listen();
void begin();
