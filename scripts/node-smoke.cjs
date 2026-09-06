/**
 * Verifies that the Prism native addon loads in plain Node.
 *
 * The counterpart to `electron-smoke.cjs`. Together they prove the single Node-API
 * binary works in both runtimes, which is what lets the build ship one artifact per
 * platform instead of one per platform and runtime.
 *
 * @example
 * node scripts/node-smoke.cjs
 * // prism native addon loaded in Node v24.18.0: version 0.0.0, wire format v1
 */

const native = require('../packages/native/index.js');
const vectors = require('../packages/protocol/vectors.json');

/**
 * Loads the addon and exits with a status reflecting whether it behaved correctly.
 *
 * Exits 0 when the addon loads and reports the same wire format revision as
 * `packages/protocol/vectors.json`, and 1 otherwise.
 *
 * @returns {void} Nothing; the process exits before returning normally.
 *
 * @example
 * runSmokeTest();
 */
function runSmokeTest() {
  const version = native.version();
  const wireFormatVersion = native.wireFormatVersion();

  if (wireFormatVersion !== vectors.formatVersion) {
    console.error(
      `wire format mismatch: addon reports v${wireFormatVersion}, ` +
        `@prism/protocol expects v${vectors.formatVersion}`,
    );
    process.exit(1);
  }

  console.log(
    `prism native addon loaded in Node ${process.version}: ` +
      `version ${version}, wire format v${wireFormatVersion}`,
  );
}

runSmokeTest();
