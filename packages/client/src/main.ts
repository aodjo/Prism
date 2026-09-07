import { app, BrowserWindow, ipcMain } from 'electron';
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { AccountClient, AccountError } from '@prism/account';
import { forgetSession, keepSession, storedSession } from '@prism/account/stored-session';
import { toDataURL } from 'qrcode';

import type { AccountDeviceView, AccountEnrolmentView, AccountState, Settings, StreamState } from './api.js';
import { DEFAULTS, loadSettings, saveSettings } from './settings.js';
import { Stream } from './stream.js';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));

/**
 * The native addon.
 *
 * Used for identity and pairing only. The stream itself runs in a process of its own, so
 * nothing on the frame path is loaded into this one at all.
 */
const prism = require('@prism/native') as typeof import('@prism/native');

/** How wide the window is. Fixed, because its content is a column of hosts and settings. */
const WINDOW_WIDTH = 420;

/** The shortest the window goes, so a failed render is not an invisible one. */
const MIN_WINDOW_HEIGHT = 220;

/** The tallest it goes, so a long list of hosts does not fill the screen. */
const MAX_WINDOW_HEIGHT = 760;

/** The window. */
let window: BrowserWindow | null = null;

/** The stream process and its state. */
let stream: Stream | null = null;

/** What this machine is configured to do. */
let settings: Settings = { ...DEFAULTS };

/** The account server, rebuilt whenever its address changes. */
let account: AccountClient | null = null;

/** The name signed in as, or `null`. */
let accountName: string | null = null;

/** Every machine the account knows, as of the last time it said. */
let accountDevices: readonly AccountDeviceView[] = [];

/** Whether the account may use the relay. */
let relayAllowed = false;

/** What went wrong the last time the account was asked something. */
let accountError: string | null = null;

/**
 * The attempt to reuse a token kept from a previous run.
 *
 * Held so that the window can wait for it. Without that, the first thing drawn after opening
 * the application is a sign-in form, replaced a moment later by the account that was signed in
 * all along — which reads as having been signed out.
 */
let resuming: Promise<void> | null = null;

/**
 * Returns a client for the configured account server, building one if the address has changed.
 *
 * Rebuilt rather than reconfigured, because a client holds a session and a session belongs to
 * the server that issued it. Carrying one across a change of address would send somebody's
 * token to a machine that never gave it to them.
 *
 * @returns {AccountClient | null} The client, or `null` when no server is configured.
 */
function accountClient(): AccountClient | null {
  const server = settings.accountServer.trim();

  if (server === '') {
    account = null;
    accountName = null;
    return null;
  }

  if (!account || account.base !== server) {
    // A token is only worth anything to the server that issued it, so pointing this machine at
    // a different one throws it away rather than offering it to a stranger. Building the first
    // client of a run is not that: there is nothing to point away from, and the token waiting
    // on disk is the one this client is about to use.
    if (account) {
      forgetSession();
    }

    account = new AccountClient(server, prism.accountAuth);
    accountName = null;
  }

  return account;
}

/**
 * Describes the account for the window.
 *
 * @returns {AccountState} What is known right now.
 */
function accountState(): AccountState {
  return {
    server: settings.accountServer,
    name: accountName,
    publicKey: prism.identityPublicKey(),
    devices: accountDevices,
    relayAllowed,
    error: accountError,
  };
}

/**
 * Turns a failure into the sentence a person should read.
 *
 * @param {unknown} error - Whatever was thrown.
 * @returns {string} The message.
 */
function accountMessage(error: unknown): string {
  if (error instanceof AccountError) {
    return error.message;
  }

  return error instanceof Error ? error.message : String(error);
}

/**
 * Signs in with the token an earlier run kept, if there is one and it is still good.
 *
 * A server that cannot be reached is not the same as a token that has expired: the first is
 * temporary and the token stays, the second is permanent and it goes. Treating them alike
 * would sign somebody out of their own account because their network was down for a minute.
 *
 * @async
 * @returns {Promise<void>}
 */
async function resumeAccount(): Promise<void> {
  const stored = storedSession();
  if (!stored) {
    return;
  }

  const client = accountClient();
  if (!client) {
    return;
  }

  try {
    const session = await client.resume(stored.token);

    if (!session) {
      forgetSession();
      return;
    }

    accountName = session.name;
    relayAllowed = session.relayAllowed;
    adoptDevices(session.devices);
    accountError = null;
  } catch (error) {
    accountError = accountMessage(error);
  }
}

/**
 * Records what the account said, and trusts every machine it named.
 *
 * Trusting is the point of the whole arrangement: two machines signed in to the same account
 * are told about each other, which is what a six digit code used to do.
 *
 * @param {readonly {publicKey: string, label: string}[]} devices - What the account listed.
 * @returns {void}
 */
function adoptDevices(devices: readonly { publicKey: string; label: string }[]): void {
  const mine = prism.identityPublicKey();

  prism.accountTrustDevices(devices.map((device) => device.publicKey));

  accountDevices = devices.map((device) => ({
    publicKey: device.publicKey,
    label: device.label,
    isThisMachine: device.publicKey === mine,
  }));
}

/**
 * Sends the stream's state to the window.
 *
 * @param {StreamState} state - What the stream is doing.
 * @returns {void}
 */
function pushStream(state: StreamState): void {
  if (!window || window.isDestroyed()) {
    return;
  }

  window.webContents.send('stream:state', state);
}

/**
 * Builds the window.
 *
 * An ordinary window rather than a tray panel: a person picks a machine, watches it, and comes
 * back. That is something they open, not something that lives in the menu bar.
 *
 * @returns {BrowserWindow} The created window.
 */
function createWindow(): BrowserWindow {
  const created = new BrowserWindow({
    width: WINDOW_WIDTH,
    height: MIN_WINDOW_HEIGHT,
    resizable: false,
    maximizable: false,
    fullscreenable: false,
    title: 'Prism',
    titleBarStyle: process.platform === 'darwin' ? 'hiddenInset' : 'default',
    webPreferences: {
      preload: join(here, 'preload.cjs'),
      // The window draws a list and nothing more. It has no reason to reach Node, and a
      // renderer that can is a renderer that can be talked into reaching a private key.
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  void created.loadFile(join(here, '..', 'renderer', 'index.html'));

  return created;
}

/**
 * Registers every call the window is allowed to make.
 *
 * @returns {void}
 */
function registerHandlers(): void {
  ipcMain.handle('prism:identity', () => ({
    version: prism.version(),
    wireFormat: prism.wireFormatVersion(),
    publicKey: prism.identityPublicKey(),
    hosts: prism.pairedPeers(),
  }));

  ipcMain.handle('account:state', async () => {
    await resuming;
    return accountState();
  });

  ipcMain.handle('account:register', async (_event, name: string, password: string) => {
    const client = accountClient();
    if (!client) {
      throw new Error('set an account server first');
    }

    accountError = null;
    const enrolment = await client.register(name, password);

    // Drawn here rather than in the window, because the window may not load anything and this
    // process may. What crosses is a picture of a link the account server already sent.
    const qr = await toDataURL(enrolment.totpUri, { margin: 1, width: 220 });

    return { qr, secret: enrolment.totpSecret } satisfies AccountEnrolmentView;
  });

  ipcMain.handle(
    'account:signIn',
    async (_event, name: string, password: string, code: string, label: string) => {
      const client = accountClient();
      if (!client) {
        throw new Error('set an account server first');
      }

      try {
        const session = await client.signIn(name, password, code);
        accountName = name;
        relayAllowed = session.relayAllowed;

        // This machine tells the account about itself before reading the list, so that the
        // list it reads already has it in — otherwise the first sign-in on a machine shows
        // every computer except the one in front of you.
        const devices = await client.registerDevice(prism.identityPublicKey(), label);

        adoptDevices(devices);
        accountError = null;

        // Kept only once both halves have worked. A token stored before this machine had been
        // registered would come back to a list that does not have it in.
        keepSession({ name, token: session.token });
      } catch (error) {
        accountName = null;
        accountError = accountMessage(error);
        throw new Error(accountError);
      }

      return accountState();
    },
  );

  ipcMain.handle('account:signOut', async () => {
    await account?.signOut();
    forgetSession();
    accountName = null;
    accountError = null;

    // The machines stay trusted. They were paired, and signing out is not a statement that
    // they are not yours.
    accountDevices = [];

    return accountState();
  });

  ipcMain.handle('account:forgetDevice', async (_event, publicKey: string) => {
    const client = accountClient();
    if (!client) {
      throw new Error('set an account server first');
    }

    try {
      adoptDevices(await client.forgetDevice(publicKey));
      accountError = null;
    } catch (error) {
      accountError = accountMessage(error);
      throw new Error(accountError);
    }

    return accountState();
  });

  ipcMain.handle('settings:get', () => settings);

  ipcMain.handle('settings:set', (_event, next: Partial<Settings>) => {
    settings = { ...settings, ...next };
    saveSettings(settings);

    return settings;
  });

  ipcMain.handle('pairing:run', async (_event, host: string, code: string) => {
    const peer = await prism.pairAsClient(host, code);

    return { peer, hosts: prism.pairedPeers() };
  });

  ipcMain.handle('stream:connect', (_event, host: string, address: string) => {
    stream ??= new Stream(pushStream);

    // What the window passed, or what was stored for this host last time. The rendezvous
    // server is the fallback, and `Stream.start` refuses when there is neither.
    const direct = address.trim() || (settings.addresses[host] ?? '');

    return stream.start(host, direct, settings);
  });

  ipcMain.handle('stream:disconnect', () => stream?.stop() ?? idle());

  ipcMain.handle('stream:state', () => stream?.state() ?? idle());

  ipcMain.on('window:fit', (_event, height: number) => {
    if (!window || window.isDestroyed()) {
      return;
    }

    const wanted = Math.round(Math.min(Math.max(height, MIN_WINDOW_HEIGHT), MAX_WINDOW_HEIGHT));
    if (window.getContentBounds().height !== wanted) {
      window.setContentSize(WINDOW_WIDTH, wanted, false);
    }
  });
}

/**
 * The state of a machine that has never streamed.
 *
 * @returns {StreamState} An idle state.
 */
function idle(): StreamState {
  return { phase: 'idle', host: null, log: [] };
}

/**
 * Opens the window, writes a picture of it, and quits.
 *
 * A developer affordance, and the only way to see what this window looks like without a person
 * in front of it. Left in because a window nobody can screenshot regresses silently: an id
 * renamed in the markup breaks the script that reads it and nothing else would notice.
 *
 * @async
 * @param {string} path - Where to write the PNG.
 * @returns {Promise<void>}
 */
async function captureWindow(path: string): Promise<void> {
  if (!window) {
    app.quit();
    return;
  }

  await new Promise((resolve) => setTimeout(resolve, 1200));

  // A picture of the window doing nothing only ever shows one of its states. Running a script
  // first is what makes the others photographable at all.
  const first = process.env['PRISM_WINDOW_DRIVE'];
  if (first) {
    await window.webContents.executeJavaScript(readFileSync(first, 'utf8'), true);
  }

  const image = await window.webContents.capturePage();
  writeFileSync(path, image.toPNG());

  const text = await window.webContents.executeJavaScript('document.body.innerText');
  process.stdout.write(`${String(text)}\n`);

  app.quit();
}

void app.whenReady().then(() => {
  settings = loadSettings();
  registerHandlers();
  resuming = resumeAccount();

  window = createWindow();

  const screenshot = process.env['PRISM_WINDOW_SCREENSHOT'];
  if (screenshot) {
    void captureWindow(screenshot);
    return;
  }

  const drive = process.env['PRISM_WINDOW_DRIVE'];
  if (drive) {
    void driveWindow(drive);
  }
});

/**
 * Runs a script inside the window and prints what it returned, then quits.
 *
 * The companion to the screenshot: that says what the window looks like, this says whether it
 * does anything. The script runs in the renderer, so it reaches the application exactly the
 * way a person does — through the buttons in the markup and the surface the preload exposes,
 * with no privileged access of its own. A stream opened any other way would prove the native
 * code works and nothing about the application on top of it.
 *
 * A developer affordance, enabled only by an environment variable naming a file on this
 * machine.
 *
 * @async
 * @param {string} path - The script to run, as a file of JavaScript.
 * @returns {Promise<void>}
 */
async function driveWindow(path: string): Promise<void> {
  if (!window) {
    app.quit();
    return;
  }

  // Long enough for the renderer to have asked who this machine is and drawn the answer,
  // which every script here starts from.
  await new Promise((resolve) => setTimeout(resolve, 1200));

  try {
    const source = readFileSync(path, 'utf8');
    const result: unknown = await window.webContents.executeJavaScript(source, true);
    process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  } catch (error) {
    process.stdout.write(
      `drive failed: ${error instanceof Error ? error.message : String(error)}\n`,
    );
    process.exitCode = 1;
  }

  app.quit();
}

app.on('window-all-closed', () => {
  app.quit();
});

app.on('before-quit', () => {
  // A stream that outlived its window would keep a remote screen on this machine with nothing
  // on screen to say so, and no way to stop it short of finding the process.
  stream?.stop();
});
