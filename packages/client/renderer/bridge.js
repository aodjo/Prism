"use strict";
(() => {
  // src/bridge.ts
  async function call(command, args) {
    const internals = window.__TAURI_INTERNALS__;
    if (!internals) {
      throw new Error(`no shell to call ${command} through`);
    }
    return await internals.invoke(command, args);
  }
  function inTauri() {
    return window.__TAURI_INTERNALS__ !== void 0;
  }
  var NOT_YET = (handler) => new Error(`the ${handler} handler has not moved to the Tauri shell yet`);
  var NO_ACCOUNT = {
    server: "",
    email: null,
    publicKey: "",
    devices: [],
    relayAllowed: false,
    error: null
  };
  var NO_STREAM = { phase: "idle", host: null, terms: null, stats: null, log: [] };
  function installBridge() {
    if (window.prism !== void 0 || !inTauri()) {
      return;
    }
    const api = {
      // ── Ported: these are calls into prism-core with nothing in between ──────────────────
      identity: async () => ({
        version: await call("version"),
        wireFormat: await call("wire_format_version"),
        publicKey: await call("identity_public_key")
      }),
      // ── Pending: still handled by the Electron main process ──────────────────────────────
      permissions: () => Promise.reject(NOT_YET("permissions:get")),
      requestPermission: () => Promise.reject(NOT_YET("permissions:request")),
      startSharing: () => Promise.reject(NOT_YET("share:start")),
      stopSharing: () => Promise.reject(NOT_YET("share:stop")),
      sharing: () => Promise.resolve(null),
      onSharing: () => {
      },
      finishSetup: () => {
      },
      openSettings: () => {
      },
      fit: () => {
      },
      connect: () => Promise.reject(NOT_YET("stream:connect")),
      disconnect: () => Promise.reject(NOT_YET("stream:disconnect")),
      streamState: () => Promise.resolve(NO_STREAM),
      onStream: () => {
      },
      sessions: () => Promise.resolve([]),
      onSessions: () => {
      },
      getSettings: () => Promise.reject(NOT_YET("settings:get")),
      setSettings: () => Promise.reject(NOT_YET("settings:set")),
      rendezvousServers: () => Promise.resolve([]),
      accountState: () => Promise.resolve(NO_ACCOUNT),
      onAccount: () => {
      },
      accountChallenge: () => Promise.reject(NOT_YET("account:challenge")),
      accountRegister: () => Promise.reject(NOT_YET("account:register")),
      accountSignIn: () => Promise.reject(NOT_YET("account:signIn")),
      accountSignOut: () => Promise.reject(NOT_YET("account:signOut")),
      accountRename: () => Promise.reject(NOT_YET("account:rename")),
      accountForgetDevice: () => Promise.reject(NOT_YET("account:forgetDevice"))
    };
    window.prism = api;
  }
  installBridge();
})();
//# sourceMappingURL=bridge.js.map
