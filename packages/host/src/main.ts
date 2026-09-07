import { app, BrowserWindow, ipcMain, Menu, nativeImage, screen, shell, Tray } from 'electron';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync } from 'node:fs';

import { TRAY_ICON } from './icon.js';
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

/** The tray icon and its menu. */
let tray: Tray | null = null;

/** The panel the tray icon opens. */
let panel: BrowserWindow | null = null;

/** The running session, or `null` when this machine is not hosting. */
let host: InstanceType<typeof prism.Host> | null = null;

/** The timer pushing snapshots to the panel. */
let ticker: NodeJS.Timeout | null = null;

/** What this machine is configured to do. */
let settings: Settings = { ...DEFAULTS };

/**
 * Builds the window the tray icon opens.
 *
 * Frameless and always on top, positioned under the tray icon, and hidden rather than closed
 * when it loses focus — the shape a menu bar application has, rather than a window somebody
 * has to find again.
 *
 * @returns {BrowserWindow} The created window, hidden until the icon is clicked.
 */
function createPanel(): BrowserWindow {
  const window = new BrowserWindow({
    width: PANEL_WIDTH,
    height: MIN_PANEL_HEIGHT,
    show: false,
    frame: false,
    resizable: false,
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

  window.on('blur', () => {
    window.hide();
  });

  return window;
}

/**
 * Positions the panel under the tray icon and shows it.
 *
 * Clamped to the display the icon is on, so a panel opened from an icon near the right edge
 * does not hang off the screen.
 *
 * @returns {void}
 */
function showPanel(): void {
  if (!panel || !tray) {
    return;
  }

  const icon = tray.getBounds();
  const { width, height } = panel.getBounds();
  const work = screen.getDisplayNearestPoint({ x: icon.x, y: icon.y }).workArea;

  const x = Math.round(
    Math.min(Math.max(icon.x + icon.width / 2 - width / 2, work.x), work.x + work.width - width),
  );
  const y = Math.round(Math.min(icon.y + icon.height + 4, work.y + work.height - height));

  panel.setPosition(x, y, false);
  panel.show();
  panel.focus();
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
  if (!panel || panel.isDestroyed() || !panel.isVisible()) {
    return;
  }

  panel.webContents.send('host:snapshot', host ? host.snapshot() : null);
}

/**
 * Sets the tray icon's tooltip and menu to match what the session is doing.
 *
 * The menu is what a person sees without opening the panel, so it carries the one fact that
 * matters: whether this machine is streaming its screen to somebody.
 *
 * @returns {void}
 */
function refreshTray(): void {
  if (!tray) {
    return;
  }

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

  tray.setToolTip(description);
  tray.setContextMenu(
    Menu.buildFromTemplate([
      { label: description, enabled: false },
      { type: 'separator' },
      { label: 'Open Prism', click: showPanel },
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
              showPanel();
            }
          }
        },
      },
      { type: 'separator' },
      { label: 'Quit', click: () => app.quit() },
    ]),
  );
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
    refreshTray();
  }, SNAPSHOT_INTERVAL_MS);

  refreshTray();
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
  refreshTray();
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

  ipcMain.on('panel:fit', (_event, height: number) => {
    if (!panel || panel.isDestroyed()) {
      return;
    }

    const wanted = Math.round(Math.min(Math.max(height, MIN_PANEL_HEIGHT), MAX_PANEL_HEIGHT));
    if (panel.getBounds().height !== wanted) {
      panel.setBounds({ height: wanted }, false);
    }
  });
}

void app.whenReady().then(() => {
  settings = loadSettings();

  // No dock icon: this is a menu bar application, and a dock icon for something with no
  // window of its own is a second place to click that does nothing different.
  app.dock?.hide();

  registerHandlers();

  const icon = nativeImage.createFromDataURL(TRAY_ICON);
  icon.setTemplateImage(true);

  tray = new Tray(icon);
  tray.on('click', showPanel);

  panel = createPanel();
  refreshTray();

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
  showPanel();

  // Long enough for the renderer to have asked the main process who this machine is and to
  // have drawn the answer. A shorter wait photographs an empty panel.
  await new Promise((resolve) => setTimeout(resolve, 1200));

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
  showPanel();

  // Long enough for the renderer to have asked who this machine is and drawn the answer,
  // which every script here starts from.
  await new Promise((resolve) => setTimeout(resolve, 1200));

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
