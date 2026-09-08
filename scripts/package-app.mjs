/**
 * Packages the client into an application somebody can run.
 *
 * The workspace cannot be handed to a packager as it stands. pnpm links a package's
 * dependencies rather than copying them, and `@prism/native` and `@prism/account` are links
 * out of `packages/client` into siblings — which a packager either follows out of the tree it
 * was asked to package, or drops. So this stages a plain directory first: the built client,
 * the two workspace packages copied in as real directories, and a `node_modules` installed by
 * npm for the one third-party package the main process needs at runtime.
 *
 * Two packager settings are not optional here. `asar` would seal the `.node` binary inside an
 * archive that `dlopen` cannot open, and the addon then fails to load with no message at all.
 * `prune` removes anything the staged `package.json` does not name as a dependency, which is
 * exactly what the two copied-in workspace packages are.
 *
 * Produces an unsigned build. Signing and notarisation are a separate concern with a separate
 * cost, and a build nobody can run is not a useful thing to wait for.
 */

import { execFileSync } from 'node:child_process';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { packager } from '@electron/packager';

const require = createRequire(import.meta.url);
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** What the application is called, in the places a person sees it. */
const PRODUCT = 'Prism';

/**
 * The name the packaged application reports as its own.
 *
 * Kept identical to the workspace package rather than made pretty, because Electron derives
 * the settings directory from it. A packaged build that called itself anything else would open
 * on an empty profile and ask somebody who is already signed in to sign in again.
 */
const NAME = '@prism/client';

/** Reversed-domain identifier for the macOS bundle. */
const BUNDLE_ID = 'dev.prism.client';

/** The one package the main process loads at runtime that is not part of this repository. */
const RUNTIME_DEPENDENCIES = { qrcode: readVersion('qrcode') };

/**
 * Reads the version the client asks for, so the staged install matches the workspace.
 *
 * @param {string} name - The package to look up.
 * @returns {string} Its version range, as the client's manifest states it.
 * @throws {Error} If the client does not depend on it.
 */
function readVersion(name) {
  const manifest = JSON.parse(
    readFileSync(join(root, 'packages', 'client', 'package.json'), 'utf8'),
  );
  const range = manifest.dependencies?.[name];

  if (!range) {
    throw new Error(`the client does not depend on ${name}; this script is out of date`);
  }

  return range;
}

/**
 * Builds the directory the packager is given.
 *
 * @param {string} where - The directory to build it in, which must be empty.
 * @returns {void}
 */
function stage(where) {
  const client = join(root, 'packages', 'client');

  cpSync(join(client, 'dist'), join(where, 'dist'), { recursive: true });
  cpSync(join(client, 'renderer'), join(where, 'renderer'), { recursive: true });

  writeFileSync(
    join(where, 'package.json'),
    `${JSON.stringify(
      {
        name: NAME,
        productName: PRODUCT,
        version: '0.1.0',
        private: true,
        type: 'module',
        main: 'dist/main.js',
        dependencies: RUNTIME_DEPENDENCIES,
      },
      null,
      2,
    )}\n`,
  );

  // Before the workspace packages are copied in: npm removes anything the manifest does not
  // name, and it does not name them.
  execFileSync('npm', ['install', '--omit=dev', '--no-audit', '--no-fund', '--no-package-lock'], {
    cwd: where,
    stdio: 'inherit',
    shell: process.platform === 'win32',
  });

  for (const name of ['account', 'native']) {
    const into = join(where, 'node_modules', '@prism', name);

    mkdirSync(dirname(into), { recursive: true });
    cpSync(join(root, 'packages', name), into, {
      recursive: true,
      // Its own `node_modules` is a thicket of pnpm links back into the store, and its sources
      // are not what runs.
      filter: (path) => !path.includes('node_modules') && !path.endsWith(`${name}/src`),
    });
  }
}

/**
 * Packages the staged directory for one platform.
 *
 * @async
 * @param {string} from - The staged directory.
 * @param {string} platform - `darwin`, `win32` or `linux`.
 * @param {string} arch - `arm64` or `x64`.
 * @returns {Promise<string[]>} Where the application was written.
 */
async function build(from, platform, arch) {
  return packager({
    dir: from,
    out: join(root, 'out'),
    name: PRODUCT,
    platform,
    arch,
    appBundleId: BUNDLE_ID,
    electronVersion: require('electron/package.json').version,
    overwrite: true,
    asar: false,
    prune: false,
  });
}

const platform = process.argv[2] ?? process.platform;
const arch = process.argv[3] ?? process.arch;
const where = mkdtempSync(join(tmpdir(), 'prism-package-'));

try {
  stage(where);

  for (const written of await build(where, platform, arch)) {
    process.stdout.write(`packaged ${platform}-${arch}: ${written}\n`);
  }
} finally {
  rmSync(where, { recursive: true, force: true });
}
