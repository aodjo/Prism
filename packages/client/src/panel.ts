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

import type {
  AccountDeviceView,
  AccountState,
  PrismApi,
  Settings,
  StreamState,
} from './api.js';

declare global {
  interface Window {
    /** The surface the preload exposes. */
    readonly prism: PrismApi;
  }
}

const prism = window.prism;

/** The phases in which a stream is running and can be stopped. */
const RUNNING = new Set(['connecting', 'streaming']);

/**
 * What each phase is called in the window.
 *
 * Said from the person's point of view rather than the session's. "Stopped" is what the
 * process did; "Not connected" is what they can see.
 */
const PHASE_LABELS: Record<string, string> = {
  idle: 'Not connected',
  connecting: 'Connecting to',
  streaming: 'Streaming from',
  stopped: 'Not connected',
  failed: 'Disconnected from',
};

/** This machine's own key, shown when no stream is naming another one. */
let identityKey = '';

/** The same key in full, so this machine can be told apart from the ones it may connect to. */
let ownKey = '';

/** Every machine the account knows, so the list can call them what their owner calls them. */
let accountDevices: readonly AccountDeviceView[] = [];

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

  // The phase is an attribute rather than a set of classes, so the stylesheet decides what
  // each state looks like in one place instead of the script deciding it in another.
  el('state').dataset['phase'] = phase;

  const label = PHASE_LABELS[phase] ?? phase;
  el('phase').textContent = state.host ? `${label} ${machineName(state.host)}` : label;

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

  // Nothing is said about a connection nobody has asked for. The band is the loudest thing in
  // the window, and spending it on "Not connected" would leave it with nothing louder for the
  // moment a stream actually drops.
  el('state').hidden = phase === 'idle' || phase === 'stopped';

  renderHosts();
  fit();
}

/** Every host this machine has paired with. */
let hosts: readonly string[] = [];

/**
 * Names this machine at the top of the window.
 *
 * It has a name only once an account has been signed in to and given one, so until then it is
 * described rather than named.
 *
 * @returns {void}
 */
function renderCrown(): void {
  const mine = accountDevices.find((device) => device.publicKey === ownKey);

  el('me-name').textContent = mine?.label || 'This machine';
}

/**
 * Returns what to call a machine.
 *
 * The account is asked first, because a person named their machines and a public key is what
 * is left when nobody has.
 *
 * @param {string} key - The machine's public key as hex.
 * @returns {string} Its name.
 */
function machineName(key: string): string {
  return accountDevices.find((device) => device.publicKey === key)?.label || short(key);
}

/**
 * Draws every machine this one can reach.
 *
 * One list rather than two. A machine arrives here either by being paired with directly or by
 * being on the same account, and which of those happened is not something anybody wants to
 * read two lists to find out.
 *
 * @returns {void}
 */
function renderHosts(): void {
  const list = el('hosts');
  list.textContent = '';

  // This machine is in both sources and belongs in neither list. Connecting to yourself is
  // not something anybody wants, and offering it is how it gets tried.
  const keys = [
    ...new Set([...accountDevices.map((device) => device.publicKey), ...hosts]),
  ].filter((key) => key !== ownKey);

  if (keys.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    // An empty screen is an invitation rather than a report.
    empty.textContent = 'Sign in, or pair with a code below';
    list.append(empty);

    return;
  }

  for (const host of keys) {
    list.append(hostRow(host));
  }
}

/**
 * Builds one machine's row.
 *
 * @param {string} host - The machine's public key as hex.
 * @returns {HTMLElement} The row.
 */
function hostRow(host: string): HTMLElement {
  const row = document.createElement('div');
  row.className = 'host';

  const name = document.createElement('span');
  name.className = 'host-name';
  name.textContent = machineName(host);
  name.title = host;

  // Where this host is, when there is no rendezvous server to ask. The host's own panel shows
  // the address it is listening on; this is where it gets typed, and it is remembered so it is
  // typed once rather than every time.
  const where = document.createElement('span');
  where.className = 'host-where';

  const address = document.createElement('input');
  address.type = 'text';
  address.placeholder = settings.rendezvous === '' ? '192.168.1.5:47200' : 'via rendezvous';
  address.spellcheck = false;
  address.value = settings.addresses[host] ?? '';
  address.addEventListener('change', () => {
    const next = { ...settings.addresses, [host]: address.value.trim() };
    settings = { ...settings, addresses: next };
    void save({ addresses: next });
  });
  where.append(address);

  const actions = document.createElement('span');
  actions.className = 'host-do';

  if (accountDevices.some((device) => device.publicKey === host)) {
    const forget = document.createElement('button');
    forget.className = 'forget';
    forget.textContent = 'Forget';
    forget.addEventListener('click', () => {
      void (async () => {
        forget.disabled = true;
        try {
          renderAccount(await prism.accountForgetDevice(host));
        } catch (error) {
          showError('account-error', error);
        }
        fit();
      })();
    });
    actions.append(forget);
  }

  const connect = document.createElement('button');
  connect.textContent = 'Connect';
  connect.disabled = RUNNING.has(phase);
  connect.addEventListener('click', () => {
    void start(host, address.value.trim());
  });
  actions.append(connect);

  row.append(name, actions, where);

  return row;
}

/**
 * Opens a stream onto a host.
 *
 * @async
 * @param {string} host - The host's public key as hex.
 * @returns {Promise<void>}
 */
async function start(host: string, address: string): Promise<void> {
  el('stream-error').hidden = true;

  try {
    render(await prism.connect(host, address));
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

/** What this machine is configured to do, as the window last read it. */
let settings: Settings = {
  rendezvous: '',
  accountServer: '',
  control: true,
  smooth: false,
  addresses: {},
};

/**
 * Loads the stored settings into the inputs.
 *
 * @async
 * @returns {Promise<void>}
 */
async function loadSettings(): Promise<void> {
  settings = await prism.getSettings();

  input('rendezvous').value = settings.rendezvous;
  input('account-server').value = settings.accountServer;
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
 * Draws what is known about the account.
 *
 * Three states in one section: nobody signed in, somebody signed in, and the one moment a new
 * account's second factor is on screen. The last one hides the others because it is the only
 * time something is shown that cannot be shown again.
 *
 * @param {AccountState} state - What the main process says.
 * @returns {void}
 */
function renderAccount(state: AccountState): void {
  const configured = state.server.trim() !== '';
  const enrolling = !el('account-enrolment').hidden;

  el('account-section').hidden = !configured;
  el('account-signed-out').hidden = enrolling || state.name !== null;
  el('account-signed-in').hidden = enrolling || state.name === null;

  el('account-who').textContent = state.name ?? '';
  el('account-relay').textContent = state.relayAllowed ? 'allowed' : 'not allowed';

  const error = el('account-error');
  error.hidden = state.error === null;
  error.textContent = state.error ?? '';

  accountDevices = state.devices;
  renderCrown();
  renderHosts();

  fit();
}

/**
 * Signs in and reports what happened.
 *
 * @async
 * @returns {Promise<void>}
 */
async function signIn(): Promise<void> {
  const name = input('account-name').value.trim();
  const password = input('account-password').value;
  const code = input('account-code').value.trim();

  el('account-error').hidden = true;
  button('account-signin').disabled = true;
  button('account-signin').textContent = 'Signing in…';

  try {
    // A label the person can recognise later, which is the machine's own name rather than
    // anything they have to think of. It can be renamed by signing in again.
    const state = await prism.accountSignIn(name, password, code, hostLabel());

    input('account-password').value = '';
    input('account-code').value = '';
    renderAccount(state);
    renderHosts();
  } catch (error) {
    showError('account-error', error);
  } finally {
    button('account-signin').disabled = false;
    button('account-signin').textContent = 'Sign in';
  }
}

/**
 * Creates an account and shows the second factor to set up.
 *
 * @async
 * @returns {Promise<void>}
 */
async function createAccount(): Promise<void> {
  const name = input('account-name').value.trim();
  const password = input('account-password').value;

  el('account-error').hidden = true;
  button('account-create').disabled = true;

  try {
    const enrolment = await prism.accountRegister(name, password);

    (el('account-qr') as HTMLImageElement).src = enrolment.qr;
    el('account-secret').textContent = enrolment.secret;

    el('account-enrolment').hidden = false;
    el('account-signed-out').hidden = true;
    el('account-signed-in').hidden = true;
    fit();
  } catch (error) {
    showError('account-error', error);
  } finally {
    button('account-create').disabled = false;
  }
}

/**
 * What to call this machine on the account.
 *
 * @returns {string} A name somebody would recognise.
 */
function hostLabel(): string {
  const platform = navigator.platform || 'computer';

  return `${platform} (${new Date().getFullYear()})`;
}

/**
 * Wires every control to what it changes.
 *
 * @returns {void}
 */
function listen(): void {
  button('account-signin').addEventListener('click', () => {
    void signIn();
  });

  button('account-create').addEventListener('click', () => {
    void createAccount();
  });

  button('account-enrolled').addEventListener('click', () => {
    // Back to the sign-in fields, with the code box waiting: the account exists now and the
    // very next thing to do is use it.
    el('account-enrolment').hidden = true;
    void (async () => {
      renderAccount(await prism.accountState());
      input('account-code').focus();
    })();
  });

  button('account-signout').addEventListener('click', () => {
    void (async () => {
      renderAccount(await prism.accountSignOut());
    })();
  });

  input('account-server').addEventListener('change', (event) => {
    const accountServer = (event.target as HTMLInputElement).value.trim();
    settings = { ...settings, accountServer };
    void (async () => {
      await save({ accountServer });
      renderAccount(await prism.accountState());
    })();
  });

  button('disconnect').addEventListener('click', () => {
    void (async () => {
      render(await prism.disconnect());
    })();
  });

  button('pair-button').addEventListener('click', () => {
    void pair();
  });

  input('rendezvous').addEventListener('change', (event) => {
    const rendezvous = (event.target as HTMLInputElement).value.trim();
    settings = { ...settings, rendezvous };
    void save({ rendezvous });
    // The placeholder in every host row says whether an address is needed, and that answer
    // just changed.
    renderHosts();
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

  ownKey = identity.publicKey;
  identityKey = short(identity.publicKey);
  el('me').textContent = identityKey;
  el('me').title = identity.publicKey;
  renderCrown();
  hosts = identity.hosts;

  await loadSettings();
  renderAccount(await prism.accountState());
  render(await prism.streamState());
}

listen();
void begin();
