/**
 * The window's way of reaching the machine, when the shell is Tauri rather than Electron.
 *
 * Electron injects `window.prism` from a preload script that runs before the page. Tauri has no
 * preload: a command is called from the page itself. So this file installs the same object the
 * preload installs, backed by `invoke` instead of `ipcRenderer`, and the React code above it
 * cannot tell the difference — which is the whole point, because that code is three thousand
 * lines and none of it should have to care which shell is underneath.
 *
 * Loaded by every page, under both shells. It stands aside when `window.prism` is already there,
 * so the Electron build behaves exactly as it did.
 *
 * Two things do not survive the boundary unchanged and are repaired here rather than upstream.
 * JSON has no integer wider than a double, so counters cross as decimal text and are rebuilt as
 * `BigInt`. And the shell hands over the provisioning link rather than a picture of it, because
 * a main process drew that picture only for a renderer that could not — this one can.
 */

import { toDataURL } from 'qrcode';

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
    readonly prism: PrismApi;
    /** Tauri's own injection, present only when this page is running inside the Tauri shell. */
    readonly __TAURI_INTERNALS__?: {
      invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
      /** Registers a function and returns the number the shell calls it back by. */
      transformCallback: (callback: (payload: unknown) => void) => number;
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
 * Subscribes to something the shell announces.
 *
 * Tauri's event system is a plugin reached through the same entry point as any other command,
 * so this needs no package either: a callback is registered with the runtime, and the number it
 * comes back as is what the shell is told to call.
 *
 * @template T What the event carries.
 * @param {string} event - The event's name, as `emit` spells it.
 * @param {(payload: T) => void} listener - Called with each one.
 * @returns {void}
 */
function listen<T>(event: string, listener: (payload: T) => void): void {
  const internals = window.__TAURI_INTERNALS__;

  if (!internals) {
    return;
  }

  const handler = internals.transformCallback((message) => {
    listener((message as { payload: T }).payload);
  });

  void internals.invoke('plugin:event|listen', { event, target: { kind: 'Any' }, handler });
}

/**
 * Whether the page is running inside the Tauri shell.
 *
 * @returns {boolean} True when Tauri injected itself into this page.
 */
export function inTauri(): boolean {
  return window.__TAURI_INTERNALS__ !== undefined;
}

/** How often a window asks what the session it is handing out is doing. */
const SHARING_POLL_MS = 200;

/**
 * What a host snapshot looks like before its counters are put back together.
 *
 * The shell sends them as text on purpose. `api.d.ts` declares them `bigint` because the
 * Node-API surface handed them over as one, and a JSON number becomes a double the moment
 * `JSON.parse` reads it — a count past nine quadrillion would arrive rounded with nothing on
 * either side saying so.
 */
type RawSnapshot = Omit<HostSnapshot, 'frames' | 'packets' | 'bytes' | 'bitrateBps'> & {
  readonly frames: string;
  readonly packets: string;
  readonly bytes: string;
  readonly bitrateBps: string;
};

/**
 * Rebuilds the counters the shell sent as text.
 *
 * @param {RawSnapshot} raw - What the command returned.
 * @returns {HostSnapshot} The same thing, with its counters the type the window expects.
 */
function revive(raw: RawSnapshot): HostSnapshot {
  return {
    ...raw,
    frames: BigInt(raw.frames),
    packets: BigInt(raw.packets),
    bytes: BigInt(raw.bytes),
    bitrateBps: BigInt(raw.bitrateBps),
  };
}

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
    identity: async (): Promise<Identity> => {
      const [version, wireFormat, publicKey, hosts] = await Promise.all([
        call<string>('version'),
        call<number>('wire_format_version'),
        call<string>('identity_public_key'),
        call<string[]>('paired_peers'),
      ]);

      return { version, wireFormat, publicKey, hosts };
    },

    getSettings: (): Promise<Settings> => call<Settings>('get_settings'),

    // Sent whole rather than as the field that changed, because the window already holds a copy
    // and sending a part would leave two places deciding what the rest still is.
    setSettings: async (next: Partial<Settings>): Promise<Settings> =>
      call<Settings>('set_settings', {
        next: { ...(await call<Settings>('get_settings')), ...next },
      }),

    permissions: (): Promise<HostPermissions> => call<HostPermissions>('permissions'),

    requestPermission: (id: string): Promise<HostPermissions> =>
      call<HostPermissions>('request_permission', { id }),

    startSharing: async (): Promise<HostSnapshot> => revive(await call<RawSnapshot>('start_sharing')),

    stopSharing: (): Promise<null> => call<null>('stop_sharing'),

    sharing: async (): Promise<HostSnapshot | null> => {
      const raw = await call<RawSnapshot | null>('sharing_state');

      return raw ? revive(raw) : null;
    },

    // Asked for rather than announced. A session that is not running has nothing to say, and one
    // that is says the same handful of counters — so a window that wants them asks at a rate it
    // chooses, which is what keeps this under the ten-a-second ceiling by construction.
    onSharing: (listener: (snapshot: HostSnapshot | null) => void): void => {
      setInterval(() => {
        void api.sharing().then(listener);
      }, SHARING_POLL_MS);
    },

    connect: (host: string, address: string): Promise<StreamState> =>
      call<StreamState>('stream_connect', { host, address }),

    disconnect: (): Promise<StreamState> => call<StreamState>('stream_disconnect'),

    streamState: (): Promise<StreamState> => call<StreamState>('stream_state'),

    // Announced rather than asked for. What the stream is doing changes when the client says so,
    // and a window polling for a phase change would either miss one or ask far more often than
    // anything changes.
    onStream: (listener: (state: StreamState) => void): void => {
      listen<StreamState>('stream:state', listener);
    },

    sessions: (): Promise<Session[]> => call<Session[]>('get_sessions'),

    onSessions: (listener: (sessions: readonly Session[]) => void): void => {
      listen<Session[]>('sessions:changed', listener);
    },

    accountState: (): Promise<AccountState> => call<AccountState>('account_state'),

    onAccount: (listener: (state: AccountState) => void): void => {
      listen<AccountState>('account:state', listener);
    },

    accountChallenge: (email: string): Promise<boolean> =>
      call<boolean>('account_challenge', { email }),

    // The picture is drawn here rather than in the shell. Under Electron the main process drew it
    // because a renderer with no network origin could not, and a data URI was the only way to get
    // it across; this one is handed the link and draws it where it is shown.
    accountRegister: async (
      email: string,
      password: string,
      code: string,
    ): Promise<AccountEnrolmentView> => {
      const enrolment = await call<{ totpUri: string; totpSecret: string }>('account_register', {
        email,
        password,
        code,
      });

      return {
        qr: await toDataURL(enrolment.totpUri, { margin: 1, width: 220 }),
        secret: enrolment.totpSecret,
      };
    },

    accountSignIn: (
      email: string,
      password: string,
      code: string,
      label: string,
    ): Promise<AccountState> =>
      call<AccountState>('account_sign_in', { email, password, code, label }),

    accountSignOut: (): Promise<AccountState> => call<AccountState>('account_sign_out'),

    accountRename: (label: string): Promise<AccountState> =>
      call<AccountState>('account_rename', { label }),

    accountForgetDevice: (publicKey: string): Promise<AccountState> =>
      call<AccountState>('account_forget_device', { publicKey }),

    // ── Still the Electron shell's, and nothing here yet ─────────────────────────────────────
    // Each is a window the Tauri shell does not open yet rather than a call that is missing:
    // setup finishing, the settings window, and a window sizing itself to what is in it.
    finishSetup: (): void => {},

    openSettings: (): void => {},

    fit: (): void => {},

    // Measured by asking every server a name resolves to, which the shell can do but does not
    // expose yet. An empty list is what a window draws when no server answered, which is the
    // truthful thing to show until this is wired.
    rendezvousServers: (): Promise<RendezvousServer[]> => Promise.resolve([]),
  };

  // `readonly` on the declaration is what stops a window reassigning the surface it talks to.
  // This is the one place that installs it, and the shell that does so is not the window.
  (window as { prism: PrismApi }).prism = api;
}

// Installed as a side effect, and loaded by the page as a classic script rather than a module,
// because a module is deferred and the React bundle reads `window.prism` the moment it runs.
// Ordering by hand is what makes the three thousand lines above this need no change at all.
installBridge();
