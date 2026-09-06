/**
 * Verifies that the Prism native addon loads inside Electron.
 *
 * Node-API binaries are runtime-agnostic, so the same `.node` that Node loads should
 * load unchanged in Electron. This script is the gate that proves it rather than
 * assuming it, and it runs in CI on every platform. It never opens a window; it loads
 * the addon, checks the addon and the TypeScript protocol package agree on the wire
 * format revision, and exits.
 *
 * @example
 * pnpm exec electron scripts/electron-smoke.cjs
 * // prism native addon loaded in Electron 40.1.0: version 0.0.0, wire format v1
 */

const { app } = require('electron');

const native = require('../packages/native/index.js');
const vectors = require('../packages/protocol/vectors.json');

/**
 * Loads the addon and exits with a status reflecting whether it behaved correctly.
 *
 * Exits 0 when the addon loads and reports the same wire format revision as
 * `packages/protocol/vectors.json`, and 1 otherwise. A mismatch means the addon and the
 * TypeScript bundle were built from different commits, which would let a session
 * silently misparse the stream.
 *
 * @returns {void} Nothing; the process exits before returning normally.
 *
 * @example
 * app.whenReady().then(runSmokeTest);
 */
function runSmokeTest() {
  const version = native.version();
  const wireFormatVersion = native.wireFormatVersion();

  if (wireFormatVersion !== vectors.formatVersion) {
    console.error(
      `wire format mismatch: addon reports v${wireFormatVersion}, ` +
        `@prism/protocol expects v${vectors.formatVersion}`,
    );
    app.exit(1);
    return;
  }

  console.log(
    `prism native addon loaded in Electron ${process.versions.electron}: ` +
      `version ${version}, wire format v${wireFormatVersion}`,
  );
  app.exit(0);
}

app.whenReady().then(runSmokeTest);
