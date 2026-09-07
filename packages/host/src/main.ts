import { app, BrowserWindow, ipcMain, Menu, screen, shell } from 'electron';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync } from 'node:fs';

import type { Settings } from './api.js';
import { DEFAULTS, loadSettings, saveSettings } from './settings.js';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));

/**
 * The native addon.
 *
 * Loaded through `createRequire` because the addon is a Node-API binary rather than an ES
 * module. One build of it loads unchanged in both Node and Electron, which is the whole
 * reason the surface is Node-API and not a V8 addon.
 */
const prism = require('@prism/native') as typeof import('@prism/native');

/**
 * How often the tray window is told what the session is doing.
 *
 * Ten times a second is the ceiling the whole design is built around: statistics that update
 * faster cost frames to produce and tell a person nothing, and a frame is what this exists to
 * protect.
 */
const SNAPSHOT_INTERVAL_MS = 200;

/** How wide the panel is. Fixed, because its content is a column of labelled rows. */
const PANEL_WIDTH = 380;

/** The shortest the panel goes, so a failed render is not an invisible window. */
const MIN_PANEL_HEIGHT = 200;

/** The tallest it goes, so a long list of paired devices does not fill the screen. */
const MAX_PANEL_HEIGHT = 720;

/** How wide the grip is — the strip that stays against the screen edge when the panel folds. */
const HANDLE_WIDTH = 30;

/** How tall the grip is on its own, which is all the screen gives up while it is folded. */
const HANDLE_HEIGHT = 56;

/** The shelf: a handle at the edge of the screen and the panel it pulls out. */
let panel: BrowserWindow | null = null;

/** Whether the panel is pulled out. Nothing is pushed to a shelf nobody has opened. */
let shelfOpen = false;

/** The running session, or `null` when this machine is not hosting. */
let host: InstanceType<typeof prism.Host> | null = null;

/** The timer pushing snapshots to the panel. */
let ticker: NodeJS.Timeout | null = null;

/** What this machine is configured to do. */
let settings: Settings = { ...DEFAULTS };

/**
 * Builds the shelf.
 *
 * A handle fixed to the right edge of the screen with the panel folded behind it, rather than
 * a menu bar item. Hosting is a thing somebody switches on and then forgets, and what they
 * want afterwards is to glance at the edge of the screen and see whether it is still on —
 * which a handle that carries the session's colour answers without being opened.
 *
 * Frameless, transparent and always on top. Every part of it that is drawn is opaque, so the
 * window never sits over the screen swallowing clicks that were meant for what is behind it.
 *
 * @returns {BrowserWindow} The created window, hidden until it has been placed.
 */
function createShelf(): BrowserWindow {
  const window = new BrowserWindow({
    width: HANDLE_WIDTH,
    height: HANDLE_HEIGHT,
    show: false,
    frame: false,
    transparent: true,
    backgroundColor: '#00000000',
    resizable: false,
    movable: false,
    minimizable: false,
    maximizable: false,
    fullscreenable: false,
    skipTaskbar: true,
    webPreferences: {
      preload: join(here, 'preload.cjs'),
      // The renderer draws a panel and nothing more. It has no reason to reach Node, and a
      // renderer that can is a renderer that can be talked into reaching a private key.
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  void window.loadFile(join(here, '..', 'renderer', 'index.html'));

  // Above ordinary windows but below anything the system puts on top of everything. A shelf
  // that covered a system alert would be a shelf somebody had to move to answer it.
  window.setAlwaysOnTop(true, 'floating');
  window.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: false });

  // Clicking away folds it, the way any panel pulled out over other windows should.
  window.on('blur', () => {
    if (shelfOpen) {
      window.webContents.send('shelf:fold');
    }
  });

  return window;
}

/**
 * Puts the shelf against the right edge of the screen at the size it is currently asking for.
 *
 * The handle is centred in the window and the window is centred on the display, so the handle
 * stays exactly where it was when the panel folds out from behind it. A shelf whose grip moved
 * every time it opened would be one nobody could find twice.
 *
 * @param {number} height - How tall the panel wants to be, when it is open.
 * @returns {void}
 */
function placeShelf(height: number): void {
  if (!panel || panel.isDestroyed()) {
    return;
  }

  const work = screen.getDisplayNearestPoint(screen.getCursorScreenPoint()).workArea;

  const width = shelfOpen ? PANEL_WIDTH + HANDLE_WIDTH : HANDLE_WIDTH;
  const tall = shelfOpen
    ? Math.round(Math.min(Math.max(height, MIN_PANEL_HEIGHT), MAX_PANEL_HEIGHT))
    : HANDLE_HEIGHT;

  panel.setBounds(
    {
      width,
      height: tall,
      x: work.x + work.width - width,
      y: Math.round(work.y + (work.height - tall) / 2),
    },
    false,
  );
}

/**
 * Sends the current session snapshot to the panel.
 *
 * Skipped entirely when the panel is hidden. A snapshot nobody is looking at is work done on
 * the same machine that is encoding video.
 *
 * @returns {void}
 */
function pushSnapshot(): void {
  if (!panel || panel.isDestroyed() || !shelfOpen) {
    return;
  }

  panel.webContents.send('host:snapshot', host ? host.snapshot() : null);
}

/**
 * Shows what the handle offers on a right click.
 *
 * The one thing somebody might want without opening the panel is to stop hosting, and the one
 * thing they cannot do from inside the panel is quit.
 *
 * @returns {void}
 */
function handleMenu(): void {
  const snapshot = host?.snapshot() ?? null;
  const phase = snapshot?.phase ?? 'idle';

  const description =
    phase === 'streaming'
      ? 'Prism — streaming'
      : phase === 'waiting'
        ? 'Prism — waiting for a client'
        : phase === 'failed'
          ? `Prism — ${snapshot?.error ?? 'failed'}`
          : 'Prism — not hosting';

  const menu = Menu.buildFromTemplate([
    { label: description, enabled: false },
    { type: 'separator' },
    {
      label: host ? 'Stop hosting' : 'Start hosting',
      click: () => {
        if (host) {
          stopHosting();
        } else {
          try {
            startHosting();
          } catch {
            // The panel is where a failure gets explained; the menu only offers the action.
            panel?.webContents.send('shelf:unfold');
          }
        }
      },
    },
    { type: 'separator' },
    { label: 'Quit', click: () => app.quit() },
  ]);

  if (panel && !panel.isDestroyed()) {
    menu.popup({ window: panel });
  } else {
    menu.popup();
  }
}

/**
 * Starts a host session with the stored settings.
 *
 * @returns {void}
 * @throws {Error} If no client has been paired, if an address cannot be parsed, or if the
 * session thread cannot be started. All three are worth showing rather than swallowing.
 */
function startHosting(): void {
  if (host) {
    return;
  }

  // Built up rather than written out, because the addon's options are genuinely optional and
  // `exactOptionalPropertyTypes` draws the distinction between a field that is absent and one
  // that is present and undefined. Absent is what "be reachable only directly" means.
  const options: import('@prism/native').HostOptions = {
    bind: settings.bind,
    fps: settings.fps,
    bitrateBps: settings.bitrateBps,
    injectInput: settings.injectInput,
  };

  if (settings.rendezvous !== '') {
    options.rendezvous = settings.rendezvous;
  }

  host = new prism.Host(options);

  ticker = setInterval(() => {
    pushSnapshot();
  }, SNAPSHOT_INTERVAL_MS);

}

/**
 * Ends the running session, if there is one.
 *
 * @returns {void}
 */
function stopHosting(): void {
  if (ticker) {
    clearInterval(ticker);
    ticker = null;
  }

  host?.stop();
  host = null;

  pushSnapshot();
}

/**
 * Registers every call the panel is allowed to make.
 *
 * This list is the whole surface between the window and the machine's keys. Nothing here
 * returns a private key, and nothing here takes a frame.
 *
 * @returns {void}
 */
function registerHandlers(): void {
  ipcMain.handle('permissions:get', () => prism.permissions(settings.injectInput));

  ipcMain.handle('permissions:request', (_event, id: string) => {
    // The system prompts at most once. After that it answers the same way forever and shows
    // nothing, so a refusal here means the only way forward is the settings pane.
    const granted = prism.requestPermission(id);
    if (!granted) {
      const grant = prism
        .permissions(settings.injectInput)
        .missing.find((missing) => missing.id === id);

      if (grant) {
        void shell.openExternal(grant.settingsUrl);
      }
    }

    return prism.permissions(settings.injectInput);
  });

  ipcMain.handle('prism:identity', () => ({
    version: prism.version(),
    wireFormat: prism.wireFormatVersion(),
    publicKey: prism.identityPublicKey(),
    peers: prism.pairedPeers(),
  }));

  ipcMain.handle('settings:get', () => settings);

  ipcMain.handle('settings:set', (_event, next: Partial<Settings>) => {
    settings = { ...settings, ...next };
    saveSettings(settings);

    return settings;
  });

  ipcMain.handle('pairing:code', () => prism.generatePairingCode());

  ipcMain.handle('pairing:await', async (_event, bind: string, code: string) => {
    const peer = await prism.pairAsHost(bind, code);

    return { peer, peers: prism.pairedPeers() };
  });

  ipcMain.handle('host:start', () => {
    startHosting();

    return host?.snapshot() ?? null;
  });

  ipcMain.handle('host:stop', () => {
    stopHosting();

    return null;
  });

  ipcMain.handle('host:snapshot', () => host?.snapshot() ?? null);

  // The renderer says whether the panel is out and how tall it wants to be; the shape of the
  // window follows from those two. Measured rather than calculated, because a missing grant or
  // a pairing code adds a section that was not there a moment ago.
  ipcMain.on('shelf:state', (_event, open: boolean, height: number) => {
    shelfOpen = open;
    placeShelf(height);

    if (open) {
      panel?.focus();
      pushSnapshot();
    }
  });

  ipcMain.on('shelf:menu', () => {
    handleMenu();
  });
}

void app.whenReady().then(() => {
  settings = loadSettings();

  // No dock icon: this is a menu bar application, and a dock icon for something with no
  // window of its own is a second place to click that does nothing different.
  app.dock?.hide();

  registerHandlers();

  panel = createShelf();
  placeShelf(MIN_PANEL_HEIGHT);
  panel.showInactive();

  // A developer affordance, and the only way to see what this window looks like without a
  // person in front of it. Left in because a panel nobody can screenshot is a panel that
  // regresses silently: an id renamed in the markup breaks the script that reads it, and
  // nothing else in the build would notice.
  const screenshot = process.env['PRISM_PANEL_SCREENSHOT'];
  if (screenshot) {
    void capturePanel(screenshot);
    return;
  }

  const drive = process.env['PRISM_PANEL_DRIVE'];
  if (drive) {
    void drivePanel(drive);
    return;
  }

  if (settings.autoStart) {
    try {
      startHosting();
    } catch {
      // Reported in the panel when it is opened. A dialog at launch, before anybody has asked
      // for anything, is the wrong place to explain a configuration problem.
    }
  }
});

/**
 * Opens the panel, writes a picture of it, and quits.
 *
 * @async
 * @param {string} path - Where to write the PNG.
 * @returns {Promise<void>}
 */
async function capturePanel(path: string): Promise<void> {
  if (!panel) {
    app.quit();
    return;
  }

  panel.removeAllListeners('blur');
  panel.show();

  // Long enough for the renderer to have asked the main process who this machine is and to
  // have drawn the answer. A shorter wait photographs an empty panel.
  await new Promise((resolve) => setTimeout(resolve, 1200));

  // Pulled out, because a picture of the grip on its own says nothing about the panel.
  panel.webContents.send('shelf:unfold');
  await new Promise((resolve) => setTimeout(resolve, 500));

  const image = await panel.webContents.capturePage();
  writeFileSync(path, image.toPNG());

  const text = await panel.webContents.executeJavaScript('document.body.innerText');
  process.stdout.write(`${String(text)}\n`);

  app.quit();
}

/**
 * Runs a script inside the panel and prints what it returned, then quits.
 *
 * The companion to the screenshot: that says what the panel looks like, this says whether it
 * does anything. The script runs in the renderer, so it reaches the application exactly the
 * way a person does — through the buttons in the markup and the surface the preload exposes,
 * with no privileged access of its own. That is the point: a session opened any other way
 * would prove the native code works and nothing about the application on top of it.
 *
 * A developer affordance, enabled only by an environment variable naming a file on this
 * machine.
 *
 * @async
 * @param {string} path - The script to run, as a file of JavaScript.
 * @returns {Promise<void>}
 */
async function drivePanel(path: string): Promise<void> {
  if (!panel) {
    app.quit();
    return;
  }

  panel.removeAllListeners('blur');
  panel.show();

  // Long enough for the renderer to have asked who this machine is and drawn the answer,
  // which every script here starts from.
  await new Promise((resolve) => setTimeout(resolve, 1200));

  panel.webContents.send('shelf:unfold');
  await new Promise((resolve) => setTimeout(resolve, 500));

  try {
    const source = readFileSync(path, 'utf8');
    const result: unknown = await panel.webContents.executeJavaScript(source, true);
    process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  } catch (error) {
    process.stdout.write(`drive failed: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }

  app.quit();
}

// A menu bar application outlives its windows by design.
app.on('window-all-closed', () => {});

app.on('before-quit', () => {
  stopHosting();
});
