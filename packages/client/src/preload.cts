import { contextBridge, ipcRenderer } from 'electron';

// The resolution mode is stated because this file is CommonJS while the package is a module,
// and TypeScript will not guess which one a declaration beside it belongs to. Types only:
// nothing is imported at run time.
import type { AccountEnrolmentView, AccountState, HostPermissions, Identity, Paired, PrismApi, Settings, StreamState } from './api.js' with { 'resolution-mode': 'import' };

/**
 * The bridge the window talks to the machine through.
 *
 * Typed against [`PrismApi`] rather than inferred from this object, so the contract is what the
 * declaration says. Adding a method the contract does not name is a compile error, which is the
 * point: this list is the entire surface between a window and a private key.
 */
const api: PrismApi = {
  identity: (): Promise<Identity> => ipcRenderer.invoke('prism:identity'),

  permissions: (): Promise<HostPermissions> => ipcRenderer.invoke('permissions:get'),

  requestPermission: (id: string): Promise<HostPermissions> =>
    ipcRenderer.invoke('permissions:request', id),

  finishSetup: (): void => {
    ipcRenderer.send('setup:done');
  },

  openSettings: (): void => {
    ipcRenderer.send('window:settings');
  },

  accountState: (): Promise<AccountState> => ipcRenderer.invoke('account:state'),

  accountRegister: (name: string, password: string): Promise<AccountEnrolmentView> =>
    ipcRenderer.invoke('account:register', name, password),

  accountSignIn: (
    name: string,
    password: string,
    code: string,
    label: string,
  ): Promise<AccountState> => ipcRenderer.invoke('account:signIn', name, password, code, label),

  accountSignOut: (): Promise<AccountState> => ipcRenderer.invoke('account:signOut'),

  accountForgetDevice: (publicKey: string): Promise<AccountState> =>
    ipcRenderer.invoke('account:forgetDevice', publicKey),

  getSettings: (): Promise<Settings> => ipcRenderer.invoke('settings:get'),

  setSettings: (next: Partial<Settings>): Promise<Settings> =>
    ipcRenderer.invoke('settings:set', next),

  pair: (host: string, code: string): Promise<Paired> =>
    ipcRenderer.invoke('pairing:run', host, code),

  connect: (host: string, address: string): Promise<StreamState> =>
    ipcRenderer.invoke('stream:connect', host, address),

  disconnect: (): Promise<StreamState> => ipcRenderer.invoke('stream:disconnect'),

  streamState: (): Promise<StreamState> => ipcRenderer.invoke('stream:state'),

  fit: (height: number): void => {
    ipcRenderer.send('window:fit', height);
  },

  onStream: (listener: (state: StreamState) => void): void => {
    ipcRenderer.on('stream:state', (_event, state: StreamState) => {
      listener(state);
    });
  },
};

contextBridge.exposeInMainWorld('prism', api);
