/**
 * The setup flow.
 *
 * Six screens in one window, shown once. Everything on them is real: the permissions are the
 * ones this machine actually holds, the code pairs, the connection is a connection, and the
 * numbers on the last screen came off the stream. A flow that showed a plausible picture
 * instead would be a flow that passes when the thing behind it is broken.
 */

import type { AccountDeviceView, HostPermissions, PrismApi, Settings, StreamState } from './api.js';
import { latency } from './format.js';

declare global {
  interface Window {
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** The screens, in the order somebody sees them. */
const STEPS = ['welcome', 'intro', 'permissions', 'device', 'connecting', 'ready'] as const;

type Step = (typeof STEPS)[number];

/** Which of the five dots is lit on each screen. The welcome screen is before the count. */
const STEP_DOTS: Partial<Record<Step, string>> = {
  intro: 'assets/steps-2.svg',
  permissions: 'assets/steps-3.svg',
  device: 'assets/steps-4.svg',
  connecting: 'assets/steps-5.svg',
  ready: 'assets/steps-6.svg',
};

/** The screens that offer a Continue rather than doing something else with the bottom right. */
const HAS_NEXT: ReadonlySet<Step> = new Set<Step>(['intro', 'permissions', 'device']);

/** How the three permission rows read, in the order the design puts them. */
const GRANTS = [
  {
    id: 'screen',
    glyph: '▣',
    name: 'Screen Recording',
    why: 'Capture this display so it can be streamed.',
  },
  {
    id: 'input',
    glyph: '⌘',
    name: 'Accessibility',
    why: 'Pass keyboard and mouse input to this machine.',
  },
  {
    id: 'network',
    glyph: '⇄',
    name: 'Local Network',
    why: 'Discover your other devices on this network.',
  },
] as const;

/** Which screen is showing. */
let step: Step = 'welcome';

/** What this machine is configured to do. */
let settings: Settings | null = null;

/** Every machine the account knows, which is what the nearby list is named from. */
let devices: readonly AccountDeviceView[] = [];

/** Every machine this one can already reach, whether by account or by an earlier pairing. */
let known: readonly string[] = [];

/** The machine being connected to, once one has been chosen. */
let target: string | null = null;

/**
 * Returns an element by id.
 *
 * @param {string} id - The element's id.
 * @returns {HTMLElement} The element.
 * @throws {Error} If the markup does not have it, which is a mistake rather than a state.
 */
function el(id: string): HTMLElement {
  const found = document.getElementById(id);

  if (!found) {
    throw new Error(`the window has no #${id}`);
  }

  return found;
}

/**
 * Returns an input by id.
 *
 * @param {string} id - The input's id.
 * @returns {HTMLInputElement} The input.
 */
function input(id: string): HTMLInputElement {
  return el(id) as HTMLInputElement;
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
 * @returns {string} Its name, or its key when nobody has named it.
 */
function machineName(key: string): string {
  return devices.find((device) => device.publicKey === key)?.label || short(key);
}

/**
 * Shows one screen and hides the rest.
 *
 * @param {Step} next - The screen to show.
 * @returns {void}
 */
function show(next: Step): void {
  step = next;
  document.body.dataset['step'] = next;

  for (const name of STEPS) {
    el(`screen-${name}`).hidden = name !== next;
  }

  // The welcome screen has a sky of its own, and no place in the count.
  el('sky-welcome').hidden = next !== 'welcome';
  el('sky-steps').hidden = next === 'welcome';

  const dots = STEP_DOTS[next];
  el('steps').hidden = dots === undefined;
  if (dots) {
    (el('steps') as HTMLImageElement).src = dots;
  }

  el('version').hidden = next !== 'welcome';
  el('skip').hidden = next === 'welcome' || next === 'ready';
  el('next').hidden = !HAS_NEXT.has(next);

  if (next === 'permissions') {
    void drawPermissions();
  }

  if (next === 'device') {
    input('pair-address').value = '';
    for (const box of codeBoxes()) {
      box.value = '';
    }
    codeBoxes()[0]?.focus();
    drawKnown();
  }
}

/**
 * Moves to the screen after this one.
 *
 * @returns {void}
 */
function advance(): void {
  const at = STEPS.indexOf(step);
  const next = STEPS[at + 1];

  if (next) {
    show(next);
  }
}

/* ── 03 · Permissions ─────────────────────────────────────────────────────────────────── */

/**
 * Draws the three permission rows from what this machine actually allows.
 *
 * Local Network is stated as given rather than checked. There is no interface for asking the
 * system about it, and by the time this screen is on a display the application has already
 * used the network to draw it — so reporting anything else would be reporting a guess.
 *
 * @async
 * @returns {Promise<void>}
 */
async function drawPermissions(): Promise<void> {
  let held: HostPermissions;

  try {
    held = await prism.permissions();
  } catch {
    held = { screen: false, input: false, missing: [] };
  }

  const card = el('grants');
  card.textContent = '';

  for (const grant of GRANTS) {
    const has =
      grant.id === 'screen' ? held.screen : grant.id === 'input' ? held.input : true;

    const row = document.createElement('div');
    row.className = 'grant';
    row.dataset['grant'] = grant.id;

    const mark = document.createElement('span');
    mark.className = 'mark';
    mark.textContent = grant.glyph;

    const text = document.createElement('span');
    text.className = 'text';

    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = grant.name;

    const why = document.createElement('span');
    why.className = 'why';
    why.textContent = grant.why;

    text.append(name, why);

    row.append(mark, text, has ? givenTag() : allowButton(grant.id));
    card.append(row);
  }
}

/**
 * Builds the tag that says a grant has been given.
 *
 * @returns {HTMLElement} The tag.
 */
function givenTag(): HTMLElement {
  const tag = document.createElement('span');
  tag.className = 'granted';

  const tick = document.createElement('span');
  tick.className = 'tick';
  tick.textContent = '✓';

  const word = document.createElement('span');
  word.textContent = 'Granted';

  tag.append(tick, word);

  return tag;
}

/**
 * Builds the button that asks for a grant.
 *
 * @param {string} id - The grant's id.
 * @returns {HTMLButtonElement} The button.
 */
function allowButton(id: string): HTMLButtonElement {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'btn-secondary';
  button.textContent = 'Allow';

  button.addEventListener('click', () => {
    void (async () => {
      button.disabled = true;
      try {
        await prism.requestPermission(id);
      } finally {
        // Redrawn either way. The system may have granted it, refused it, or opened its own
        // settings pane — and only the check afterwards says which.
        await drawPermissions();
      }
    })();
  });

  return button;
}

/* ── 04 · Add device ──────────────────────────────────────────────────────────────────── */

/**
 * Returns the six character boxes.
 *
 * @returns {HTMLInputElement[]} The boxes, left to right.
 */
function codeBoxes(): HTMLInputElement[] {
  return [...el('code').querySelectorAll('input')];
}

/**
 * Makes the six boxes behave as one field.
 *
 * Typing moves forward, deleting moves back, and pasting a whole code fills them all — which
 * is what somebody does when the code is on a screen beside them rather than in their head.
 *
 * @returns {void}
 */
function wireCode(): void {
  const boxes = codeBoxes();

  boxes.forEach((box, at) => {
    box.addEventListener('input', () => {
      box.value = box.value.toUpperCase().slice(0, 1);

      if (box.value !== '') {
        boxes[at + 1]?.focus();
      }

      if (typed().length === boxes.length) {
        void pair();
      }
    });

    box.addEventListener('keydown', (event) => {
      if (event.key === 'Backspace' && box.value === '') {
        const before = boxes[at - 1];
        if (before) {
          before.value = '';
          before.focus();
        }
      }
    });

    box.addEventListener('paste', (event) => {
      const text = event.clipboardData?.getData('text') ?? '';
      event.preventDefault();

      const characters = text.toUpperCase().replace(/\s/g, '').split('');
      boxes.forEach((each, index) => {
        each.value = characters[index] ?? '';
      });

      boxes[Math.min(characters.length, boxes.length - 1)]?.focus();

      if (typed().length === boxes.length) {
        void pair();
      }
    });
  });
}

/**
 * Returns what has been typed into the six boxes.
 *
 * @returns {string} The characters, with nothing standing in for the empty ones.
 */
function typed(): string {
  return codeBoxes()
    .map((box) => box.value)
    .join('');
}

/**
 * Pairs with the machine showing the typed code.
 *
 * @async
 * @returns {Promise<void>}
 */
async function pair(): Promise<void> {
  const code = typed();
  const address = input('pair-address').value.trim();

  if (address === '') {
    fail('pair-error', 'Type where that machine is, as address:port');
    input('pair-address').focus();
    return;
  }

  el('pair-error').hidden = true;

  for (const box of codeBoxes()) {
    box.disabled = true;
  }

  try {
    const paired = await prism.pair(address, code);

    // Remembered so that connecting does not ask for the same address a second line later.
    if (settings) {
      const addresses = { ...settings.addresses, [paired.peer]: address };
      settings = await prism.setSettings({ addresses });
    }

    target = paired.peer;
    show('connecting');
    await open(paired.peer, address);
  } catch (error) {
    fail('pair-error', error);

    for (const box of codeBoxes()) {
      box.disabled = false;
      box.value = '';
    }

    codeBoxes()[0]?.focus();
  }
}

/**
 * Draws the machines this one already knows about.
 *
 * The design calls these nearby. What they actually are is the machines on the account, which
 * is the same set for anybody who has signed in and a shorter one for anybody who has not —
 * and a list that claimed to have scanned the network would be claiming something this
 * application does not do.
 *
 * @returns {void}
 */
function drawKnown(): void {
  const list = el('known');
  list.textContent = '';

  if (known.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = 'Nothing yet — a code above is how the first one arrives';
    list.append(empty);

    return;
  }

  for (const key of known) {
    const address = settings?.addresses[key] ?? '';

    const row = document.createElement('div');
    row.className = 'device';

    const status = document.createElement('img');
    status.className = 'status';
    status.src = address ? 'assets/status-live-04.svg' : 'assets/status-idle-04.svg';
    status.alt = '';

    const text = document.createElement('span');
    text.className = 'text';

    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = machineName(key);

    const meta = document.createElement('span');
    meta.className = 'meta';
    meta.textContent = address || 'through the rendezvous server';

    text.append(name, meta);

    const connect = document.createElement('button');
    connect.type = 'button';
    connect.className = 'btn-secondary';
    connect.textContent = 'Connect';
    connect.addEventListener('click', () => {
      target = key;
      show('connecting');
      void open(key, address);
    });

    row.append(status, text, connect);
    list.append(row);
  }
}

/* ── 05 · Connecting ──────────────────────────────────────────────────────────────────── */

/**
 * Opens a stream onto a machine.
 *
 * @async
 * @param {string} host - Its public key, as hex.
 * @param {string} address - Where it is, or empty to let the rendezvous server answer.
 * @returns {Promise<void>}
 */
async function open(host: string, address: string): Promise<void> {
  el('connect-error').hidden = true;
  el('connect-to').textContent = `Connecting to ${machineName(host)}`;
  el('connect-meta').textContent = address
    ? `${address}  ·  direct on your LAN  ·  no relay`
    : 'through the rendezvous server';

  try {
    draw(await prism.connect(host, address));
  } catch (error) {
    fail('connect-error', error);
  }
}

/**
 * Draws whatever the stream is doing, on whichever screen is showing.
 *
 * @param {StreamState} state - What the stream said.
 * @returns {void}
 */
function draw(state: StreamState): void {
  const handshake = state.phase === 'streaming' || state.terms !== null;
  const codec = state.terms !== null;
  const video = state.phase === 'streaming' && (state.stats?.frames ?? 0) > 0;

  mark('stage-handshake', handshake ? 'done' : 'doing');
  mark('stage-codec', codec ? 'done' : handshake ? 'doing' : 'waiting');
  mark('stage-video', video ? 'done' : codec ? 'doing' : 'waiting');

  if (state.terms) {
    el('detail-codec').textContent = state.stats
      ? `${state.terms.codec} · ${state.stats.mbps.toFixed(0)} Mbps`
      : state.terms.codec;
    el('detail-video').textContent = state.terms.width
      ? `${state.terms.width} × ${state.terms.height} @ ${state.terms.fps} Hz`
      : `the host's screen @ ${state.terms.fps} Hz`;
  }

  if (state.phase === 'failed') {
    fail('connect-error', state.log.slice(-6).join('\n'));
  }

  // The last screen is reached by the connection working, not by anybody pressing anything.
  if (step === 'connecting' && video) {
    show('ready');
  }

  if (step === 'ready') {
    drawReady(state);
  }
}

/**
 * Sets one progress row's state.
 *
 * @param {string} id - The row's id.
 * @param {string} state - `waiting`, `doing` or `done`.
 * @returns {void}
 */
function mark(id: string, state: string): void {
  el(id).dataset['state'] = state;
}

/* ── 06 · Ready ───────────────────────────────────────────────────────────────────────── */

/**
 * Draws the last screen from what the stream reported.
 *
 * @param {StreamState} state - What the stream said.
 * @returns {void}
 */
function drawReady(state: StreamState): void {
  const name = machineName(state.host ?? target ?? '');

  el('ready-sub').textContent = `${name} is live. Press ⌘↵ from anywhere to jump straight back in.`;
  el('stat-latency').textContent = state.stats ? `${latency(state.stats.rttMs)} ms` : '—';
  el('stat-codec').textContent = state.terms?.codec ?? '—';
  el('stat-display').textContent = state.terms
    ? state.terms.height
      ? `${state.terms.height}p · ${state.terms.fps} Hz`
      : `${state.terms.fps} Hz`
    : '—';
}

/* ── Wiring ───────────────────────────────────────────────────────────────────────────── */

/**
 * Reports a failure on one of the two error lines.
 *
 * @param {string} id - Which line.
 * @param {unknown} error - Whatever was thrown, or a message.
 * @returns {void}
 */
function fail(id: string, error: unknown): void {
  const line = el(id);
  line.hidden = false;
  line.textContent =
    error instanceof Error ? error.message : typeof error === 'string' ? error : String(error);
}

/**
 * Ends setup and opens the window it was setting up.
 *
 * @returns {void}
 */
function finish(): void {
  prism.finishSetup();
}

el('begin').addEventListener('click', () => {
  advance();
});

el('restore').addEventListener('click', () => {
  // Somebody who already has machines does not need to be told what the product is. What they
  // need is the window their machines are in.
  finish();
});

el('skip').addEventListener('click', finish);
el('enter').addEventListener('click', finish);

el('next').addEventListener('click', () => {
  advance();
});

el('cancel').addEventListener('click', () => {
  void (async () => {
    await prism.disconnect();
    show('device');
  })();
});

// The keystroke the last screen offers, honoured wherever it is pressed on that screen.
document.addEventListener('keydown', (event) => {
  if (step === 'ready' && event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
    finish();
  }
});

wireCode();
prism.onStream(draw);

// The flow's own controls, so that a screen which needs a second machine to reach can still be
// driven and photographed from this one. It adds no privilege — everything here changes what
// is displayed and nothing else — and it is the companion to the screenshot harness the main
// process already carries for the same reason.
Object.defineProperty(window, 'prismSetup', { value: { show, draw } });

void (async () => {
  const [identity, account, stored] = await Promise.all([
    prism.identity(),
    prism.accountState(),
    prism.getSettings(),
  ]);

  settings = stored;
  devices = account.devices;

  // Both sources, minus this machine. A machine arrives here either by having been paired with
  // or by being on the account, and setup should offer whichever is already true.
  known = [
    ...new Set([...account.devices.map((device) => device.publicKey), ...identity.hosts]),
  ].filter((key) => key !== identity.publicKey);

  el('version').textContent = `v${identity.version} · beta`;

  show('welcome');
})();
