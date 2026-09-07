import { app, BrowserWindow, ipcMain, shell } from 'electron';
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { Holder } from '@prism/account/holder';
import { toDataURL } from 'qrcode';

import type { AccountEnrolmentView, Settings, StreamState } from './api.js';
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

/** How wide the settings window is. Fixed, because its content is a column of rows. */
const WINDOW_WIDTH = 420;

/**
 * What the setup flow and the home window open at.
 *
 * The design was drawn at 1440 × 900. Opening at that size would fill a laptop display edge to
 * edge on first launch, so the window starts smaller and the layout is written to hold its
 * composition at any size rather than only at the one it was drawn at.
 */
const STAGE_WIDTH = 1280;

/** And how tall. */
const STAGE_HEIGHT = 840;

/** The shortest the window goes, so a failed render is not an invisible one. */
const MIN_WINDOW_HEIGHT = 220;

/** The tallest it goes, so a long list of hosts does not fill the screen. */
const MAX_WINDOW_HEIGHT = 760;

/** The settings window, which is what the original panel became. */
let window: BrowserWindow | null = null;

/** The setup flow, open only until it is finished or skipped. */
let setup: BrowserWindow | null = null;

/** The home window, which is where somebody spends their time. */
let home: BrowserWindow | null = null;

/** The stream process and its state. */
let stream: Stream | null = null;

/** What this machine is configured to do. */
let settings: Settings = { ...DEFAULTS };

/**
 * The account, which both applications hold the same way.
 *
 * It reads the server address out of these settings and writes the signalling address back
 * into them, because where to register is something the account knows and this machine does
 * not until it has asked.
 */
const account = new Holder(prism, {
  server: () => settings.accountServer,
  rendezvous: () => settings.rendezvous,
  setRendezvous: (address: string) => {
    settings = { ...settings, rendezvous: address };
    saveSettings(settings);
  },
});

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
 * Sends the stream's state to every window that is open.
 *
 * Three of them can be, and all three draw some part of what a stream is doing. Sending to the
 * one that happened to start it would leave the others showing what was true a minute ago.
 *
 * @param {StreamState} state - What the stream is doing.
 * @returns {void}
 */
function broadcastStream(state: StreamState): void {
  for (const open of [window, setup, home]) {
    if (open && !open.isDestroyed()) {
      open.webContents.send('stream:state', state);
    }
  }
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

  echoConsole(created);
  void created.loadFile(join(here, '..', 'renderer', 'index.html'));

  return created;
}

/**
 * Builds one of the two full-size windows.
 *
 * Both are the same window with a different page in it: same chrome, same bridge, same size.
 * The difference is which of them a launch opens, and that is decided by whether setup has
 * been through once.
 *
 * @param {string} page - The file in `renderer/` to load.
 * @returns {BrowserWindow} The created window.
 */
function createStage(page: string): BrowserWindow {
  const created = new BrowserWindow({
    width: STAGE_WIDTH,
    height: STAGE_HEIGHT,
    minWidth: 1040,
    minHeight: 720,
    title: 'Prism',
    // The design puts its own content where a title bar would be, and carries the traffic
    // lights over the top left of it.
    titleBarStyle: process.platform === 'darwin' ? 'hiddenInset' : 'default',
    backgroundColor: '#08080b',
    webPreferences: {
      preload: join(here, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  echoConsole(created);
  void created.loadFile(join(here, '..', 'renderer', page));

  return created;
}

/**
 * Opens the home window, and closes setup if that is what was showing.
 *
 * @returns {void}
 */
function openHome(): void {
  if (home && !home.isDestroyed()) {
    home.focus();
  } else {
    home = createStage('home.html');
  }

  if (setup && !setup.isDestroyed()) {
    setup.close();
  }

  setup = null;
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

  ipcMain.handle('permissions:get', () => prism.permissions(settings.control));

  ipcMain.handle('permissions:request', (_event, id: string) => {
    // The system prompts at most once. After that it answers the same way forever and shows
    // nothing, so a refusal here means the only way forward is the settings pane.
    const granted = prism.requestPermission(id);

    if (!granted) {
      const grant = prism.permissions(settings.control).missing.find((one) => one.id === id);

      if (grant) {
        void shell.openExternal(grant.settingsUrl);
      }
    }

    return prism.permissions(settings.control);
  });

  ipcMain.on('setup:done', () => {
    settings = { ...settings, setupDone: true };
    saveSettings(settings);
    openHome();
  });

  ipcMain.on('window:settings', () => {
    if (window && !window.isDestroyed()) {
      window.focus();
      return;
    }

    window = createWindow();
  });

  ipcMain.handle('account:state', () => account.view());

  ipcMain.handle('account:register', async (_event, email: string, password: string) => {
    const enrolment = await account.register(email, password);

    // Drawn here rather than in the window, because the window may not load anything and this
    // process may. What crosses is a picture of a link the account server already sent.
    const qr = await toDataURL(enrolment.totpUri, { margin: 1, width: 220 });

    return { qr, secret: enrolment.totpSecret } satisfies AccountEnrolmentView;
  });

  ipcMain.handle(
    'account:signIn',
    (_event, email: string, password: string, code: string, label: string) =>
      account.signIn(email, password, code, label),
  );

  ipcMain.handle('account:signOut', () => account.signOut());

  ipcMain.handle('account:forgetDevice', (_event, publicKey: string) =>
    account.forget(publicKey),
  );

  ipcMain.handle('settings:get', () => settings);

  ipcMain.handle('settings:set', (_event, next: Partial<Settings>) => {
    settings = { ...settings, ...next };
    saveSettings(settings);

    return settings;
  });

  ipcMain.handle('stream:connect', (_event, host: string, address: string) => {
    stream ??= new Stream(broadcastStream);

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
  return { phase: 'idle', host: null, terms: null, stats: null, log: [] };
}

/**
 * Forwards a window's own console to this process, while a harness is driving it.
 *
 * A renderer that throws while React is drawing it leaves an empty page and says so only in a
 * console nobody is watching. Under a screenshot or a drive script that is the difference
 * between a diagnosis and a black rectangle.
 *
 * @param {BrowserWindow} target - The window to listen to.
 * @returns {void}
 */
function echoConsole(target: BrowserWindow): void {
  if (!process.env['PRISM_WINDOW_DRIVE'] && !process.env['PRISM_WINDOW_SCREENSHOT']) {
    return;
  }

  target.webContents.on('console-message', (event) => {
    process.stderr.write(`window: ${event.message}\n`);
  });
}

/**
 * Returns whichever window a person is looking at.
 *
 * @returns {BrowserWindow | null} The window, or `null` when none opened.
 */
function onScreen(): BrowserWindow | null {
  for (const open of [home, setup, window]) {
    if (open && !open.isDestroyed()) {
      return open;
    }
  }

  return null;
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
  const shown = onScreen();

  if (!shown) {
    app.quit();
    return;
  }

  await new Promise((resolve) => setTimeout(resolve, 1200));

  // A picture of the window doing nothing only ever shows one of its states. Running a script
  // first is what makes the others photographable at all.
  const first = process.env['PRISM_WINDOW_DRIVE'];
  if (first) {
    await shown.webContents.executeJavaScript(readFileSync(first, 'utf8'), true);
  }

  const image = await shown.webContents.capturePage();
  writeFileSync(path, image.toPNG());

  const text = await shown.webContents.executeJavaScript('document.body.innerText');
  process.stdout.write(`${String(text)}\n`);

  app.quit();
}

void app.whenReady().then(() => {
  settings = loadSettings();
  registerHandlers();
  account.start();

  // A machine that has been through setup goes straight to the thing setup was for. One that
  // has not is asked the questions setup asks, once.
  //
  // A developer affordance too: the page to open, so that a screenshot can be taken of a
  // window this machine's own state would not otherwise show.
  const forced = process.env['PRISM_WINDOW_PAGE'];

  if (forced === 'setup.html' || (!settings.setupDone && forced !== 'home.html')) {
    setup = createStage('setup.html');
  } else if (forced === 'index.html') {
    window = createWindow();
  } else {
    home = createStage('home.html');
  }

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
  const shown = onScreen();

  if (!shown) {
    app.quit();
    return;
  }

  // Long enough for the renderer to have asked who this machine is and drawn the answer,
  // which every script here starts from.
  await new Promise((resolve) => setTimeout(resolve, 1200));

  try {
    const source = readFileSync(path, 'utf8');

    // A script that ends setup closes the window it is running in, and a promise inside a
    // destroyed renderer never settles. Racing the close keeps that from hanging forever.
    const closed = new Promise<string>((resolve) => {
      shown.once('closed', () => {
        resolve('the window closed while the script was running');
      });
    });

    const result: unknown = await Promise.race([
      shown.webContents.executeJavaScript(source, true),
      closed,
    ]);
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
