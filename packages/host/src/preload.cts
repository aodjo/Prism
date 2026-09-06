import { contextBridge, ipcRenderer } from 'electron';

// The resolution mode is stated because this file is CommonJS while the package is a module,
// and TypeScript will not guess which one a declaration beside it belongs to. It is types only:
// nothing is imported at run time.
import type { HostSnapshot, Identity, Paired, PrismApi, Settings } from './api.js' with { 'resolution-mode': 'import' };

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

  getSettings: (): Promise<Settings> => ipcRenderer.invoke('settings:get'),

  setSettings: (next: Partial<Settings>): Promise<Settings> =>
    ipcRenderer.invoke('settings:set', next),

  pairingCode: (): Promise<string> => ipcRenderer.invoke('pairing:code'),

  awaitPairing: (bind: string, code: string): Promise<Paired> =>
    ipcRenderer.invoke('pairing:await', bind, code),

  startHosting: (): Promise<HostSnapshot | null> => ipcRenderer.invoke('host:start'),

  stopHosting: (): Promise<null> => ipcRenderer.invoke('host:stop'),

  snapshot: (): Promise<HostSnapshot | null> => ipcRenderer.invoke('host:snapshot'),

  fit: (height: number): void => {
    ipcRenderer.send('panel:fit', height);
  },

  onSnapshot: (listener: (snapshot: HostSnapshot | null) => void): void => {
    ipcRenderer.on('host:snapshot', (_event, snapshot: HostSnapshot | null) => {
      listener(snapshot);
    });
  },
};

contextBridge.exposeInMainWorld('prism', api);
