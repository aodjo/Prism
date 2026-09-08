/**
 * The window's way of reaching the machine, when the shell is Tauri rather than Electron.
 *
 * Electron injects `window.prism` from a preload script that runs before the page. Tauri has no
 * preload: a command is called from the page itself. So this file installs the same object the
 * preload installs, backed by `invoke` instead of `ipcRenderer`, and the React code above it
 * cannot tell the difference — which is the whole point, because that code is three thousand
 * lines and none of it should have to care which shell is underneath.
 *
 * Loaded by every page, under both shells. It stands aside when `window.prism` is already
 * there, so the Electron build behaves exactly as it did.
 *
 * This is a migration in progress. Everything under `ported` is a real call into Rust;
 * everything under `pending` is a placeholder that lets the window draw while the rest of the
 * shell moves across, and each one names the Electron handler it is waiting on. The list
 * shrinking to nothing is what finishing this migration means.
 */

import type {
  AccountEnrolmentView,
  AccountState,
  HostPermissions,
  HostSnapshot,
  Identity,
  PrismApi,
  RendezvousServer,
  Session,
  Settings,
  StreamState,
} from './api.js';

declare global {
  interface Window {
    prism: PrismApi;
    /** Tauri's own injection, present only when this page is running inside the Tauri shell. */
    readonly __TAURI_INTERNALS__?: {
      invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
    };
  }
}

/**
 * Calls a command in the shell.
 *
 * Goes through Tauri's own injected entry point rather than `@tauri-apps/api`, so that no part
 * of the renderer bundle depends on a package that only one of the two shells provides.
 *
 * @template T What the command returns.
 * @param {string} command - The command's name, as `#[tauri::command]` spells it.
 * @param {Record<string, unknown>} [args] - Its arguments, keyed by parameter name.
 * @returns {Promise<T>} What it returned.
 * @throws {Error} If the shell is not Tauri, or the command failed.
 */
async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const internals = window.__TAURI_INTERNALS__;

  if (!internals) {
    throw new Error(`no shell to call ${command} through`);
  }

  return (await internals.invoke(command, args)) as T;
}

/**
 * Whether the page is running inside the Tauri shell.
 *
 * @returns {boolean} True when Tauri injected itself into this page.
 */
export function inTauri(): boolean {
  return window.__TAURI_INTERNALS__ !== undefined;
}

/** What a call that has not been ported yet says, so it is never mistaken for a real answer. */
const NOT_YET = (handler: string): Error =>
  new Error(`the ${handler} handler has not moved to the Tauri shell yet`);

/** Nothing is known about the account, which is what a window sees before one is configured. */
const NO_ACCOUNT: AccountState = {
  server: '',
  email: null,
  publicKey: '',
  devices: [],
  relayAllowed: false,
  error: null,
};

/** Nothing is happening, and nothing has happened yet. */
const NO_STREAM: StreamState = { phase: 'idle', host: null, terms: null, stats: null, log: [] };

/**
 * Installs the bridge, unless a shell has already provided one.
 *
 * @returns {void}
 */
export function installBridge(): void {
  if (window.prism !== undefined || !inTauri()) {
    return;
  }

  const api: PrismApi = {
    // ── Ported: these are calls into prism-core with nothing in between ──────────────────
    identity: async (): Promise<Identity> => ({
      version: await call<string>('version'),
      wireFormat: await call<number>('wire_format_version'),
      publicKey: await call<string>('identity_public_key'),
    }),

    // ── Pending: still handled by the Electron main process ──────────────────────────────
    permissions: (): Promise<HostPermissions> => Promise.reject(NOT_YET('permissions:get')),

    requestPermission: (): Promise<HostPermissions> =>
      Promise.reject(NOT_YET('permissions:request')),

    startSharing: (): Promise<HostSnapshot | null> => Promise.reject(NOT_YET('share:start')),

    stopSharing: (): Promise<null> => Promise.reject(NOT_YET('share:stop')),

    sharing: (): Promise<HostSnapshot | null> => Promise.resolve(null),

    onSharing: (): void => {},

    finishSetup: (): void => {},

    openSettings: (): void => {},

    fit: (): void => {},

    connect: (): Promise<void> => Promise.reject(NOT_YET('stream:connect')),

    disconnect: (): Promise<void> => Promise.reject(NOT_YET('stream:disconnect')),

    streamState: (): Promise<StreamState> => Promise.resolve(NO_STREAM),

    onStream: (): void => {},

    sessions: (): Promise<readonly Session[]> => Promise.resolve([]),

    onSessions: (): void => {},

    getSettings: (): Promise<Settings> => Promise.reject(NOT_YET('settings:get')),

    setSettings: (): Promise<Settings> => Promise.reject(NOT_YET('settings:set')),

    rendezvousServers: (): Promise<readonly RendezvousServer[]> => Promise.resolve([]),

    accountState: (): Promise<AccountState> => Promise.resolve(NO_ACCOUNT),

    onAccount: (): void => {},

    accountChallenge: (): Promise<boolean> => Promise.reject(NOT_YET('account:challenge')),

    accountRegister: (): Promise<AccountEnrolmentView> =>
      Promise.reject(NOT_YET('account:register')),

    accountSignIn: (): Promise<AccountState> => Promise.reject(NOT_YET('account:signIn')),

    accountSignOut: (): Promise<AccountState> => Promise.reject(NOT_YET('account:signOut')),

    accountRename: (): Promise<AccountState> => Promise.reject(NOT_YET('account:rename')),

    accountForgetDevice: (): Promise<AccountState> =>
      Promise.reject(NOT_YET('account:forgetDevice')),
  };

  window.prism = api;
}

// Installed as a side effect, and loaded by the page as a classic script rather than a module,
// because a module is deferred and the React bundle reads `window.prism` the moment it runs.
// Ordering by hand is what makes the three thousand lines above this need no change at all.
installBridge();
