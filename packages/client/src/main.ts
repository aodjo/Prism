import { app, BrowserWindow, ipcMain } from 'electron';
import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import type { Settings, StreamState } from './api.js';
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

    return stream.start(host, address, settings);
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

  const image = await window.webContents.capturePage();
  writeFileSync(path, image.toPNG());

  const text = await window.webContents.executeJavaScript('document.body.innerText');
  process.stdout.write(`${String(text)}\n`);

  app.quit();
}

void app.whenReady().then(() => {
  settings = loadSettings();
  registerHandlers();

  window = createWindow();

  const screenshot = process.env['PRISM_WINDOW_SCREENSHOT'];
  if (screenshot) {
    void captureWindow(screenshot);
  }
});

app.on('window-all-closed', () => {
  app.quit();
});

app.on('before-quit', () => {
  // A stream that outlived its window would keep a remote screen on this machine with nothing
  // on screen to say so, and no way to stop it short of finding the process.
  stream?.stop();
});
