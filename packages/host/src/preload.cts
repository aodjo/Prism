import { contextBridge, ipcRenderer } from 'electron';

// The resolution mode is stated because this file is CommonJS while the package is a module,
// and TypeScript will not guess which one a declaration beside it belongs to. It is types only:
// nothing is imported at run time.
import type { HostPermissions, HostSnapshot, Identity, PrismApi, Settings } from './api.js' with { 'resolution-mode': 'import' };

/**
 * The bridge the panel talks to the machine through.
 *
 * Typed against [`PrismApi`] rather than inferred from this object, so the contract is what the
 * declaration says and not whatever shape this file happens to have. Adding a method here that
 * the contract does not name is a compile error, which is the point: this list is the entire
 * surface between a window and a private key.
 */
const api: PrismApi = {
  identity: (): Promise<Identity> => ipcRenderer.invoke('prism:identity'),

  permissions: (): Promise<HostPermissions> => ipcRenderer.invoke('permissions:get'),

  requestPermission: (id: string): Promise<HostPermissions> =>
    ipcRenderer.invoke('permissions:request', id),

  getSettings: (): Promise<Settings> => ipcRenderer.invoke('settings:get'),

  setSettings: (next: Partial<Settings>): Promise<Settings> =>
    ipcRenderer.invoke('settings:set', next),

  startHosting: (): Promise<HostSnapshot | null> => ipcRenderer.invoke('host:start'),

  stopHosting: (): Promise<null> => ipcRenderer.invoke('host:stop'),

  snapshot: (): Promise<HostSnapshot | null> => ipcRenderer.invoke('host:snapshot'),

  shelf: (open: boolean, height: number): void => {
    ipcRenderer.send('shelf:state', open, height);
  },

  shelfMenu: (): void => {
    ipcRenderer.send('shelf:menu');
  },

  onFold: (listener: (open: boolean) => void): void => {
    ipcRenderer.on('shelf:fold', () => {
      listener(false);
    });
    ipcRenderer.on('shelf:unfold', () => {
      listener(true);
    });
  },

  onSnapshot: (listener: (snapshot: HostSnapshot | null) => void): void => {
    ipcRenderer.on('host:snapshot', (_event, snapshot: HostSnapshot | null) => {
      listener(snapshot);
    });
  },
};

contextBridge.exposeInMainWorld('prism', api);
