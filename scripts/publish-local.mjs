/**
 * Builds this machine's platform and publishes it to the local line.
 *
 * What this is for: a one-line fix takes three minutes to build on the machine that made it and
 * ten to come back from a build farm that is making the same thing again on five runners. When
 * the change is being tested rather than released, the second wait buys nothing.
 *
 * It does what CI does for one platform — name the build, build it, sign it — and then puts the
 * bundle where the updater looks, instead of onto a release. Released versions still come from
 * a tag and a workflow: this is only the line between them.
 *
 * Usage, from the repository root:
 *
 *   pnpm publish-local
 *   pnpm publish-local --notes "why the caret sat low"
 *   pnpm publish-local --build 912
 *
 * What it needs, and what it does not:
 *
 * - `PRISM_UPDATE_KEY`, the path to the minisign private key, defaulting to
 *   `~/Documents/prism-keys/prism-update.key`. Without it the bundle has no signature and the
 *   updater refuses it after downloading in full, which is the worst of both.
 * - `PRISM_UPDATE_KEY_PASSWORD` if that key has one.
 * - `APPLE_SIGNING_IDENTITY` on macOS, so the build carries the same code identity as the
 *   released ones. Without it every rebuild is a different ad-hoc identity, which is a new
 *   keychain prompt each time and a screen recording grant that has to be given again.
 * - An operator session, which it asks for: the same address and password the dashboard takes.
 *
 * @module
 */

import { execFileSync } from 'node:child_process';
import { createInterface } from 'node:readline/promises';
import { readFileSync, existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { stdin, stdout } from 'node:process';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

/** Where the account server lives. */
const SERVER = process.env.PRISM_ACCOUNT_SERVER ?? 'https://accounts.presm.kr';

/** Where the updater's signing key is kept, unless something says otherwise. */
const DEFAULT_KEY = join(homedir(), 'Documents/prism-keys/prism-update.key');

/**
 * What the updater calls this machine.
 *
 * The names Rust uses for a target, which is what Tauri asks the endpoint with — not the ones
 * Node uses for the same two things.
 *
 * @returns {{target: string, arch: string, bundle: string}} The platform, and what its bundle
 *   file is called under `target/release/bundle`.
 */
function platform() {
  const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64';

  switch (process.platform) {
    case 'darwin':
      return { target: 'darwin', arch, bundle: 'macos/Prism.app.tar.gz' };
    case 'win32':
      return { target: 'windows', arch, bundle: 'nsis/Prism_x64-setup.exe' };
    case 'linux':
      return { target: 'linux', arch, bundle: 'appimage/prism.AppImage' };
    default:
      throw new Error(`nothing is published for ${process.platform}`);
  }
}

/**
 * Reads the arguments this takes, which are few.
 *
 * @param {string[]} argv - What was passed.
 * @returns {{build: number, notes: string}} The build number and the line describing it.
 */
function asked(argv) {
  let build = Number(process.env.PRISM_BUILD ?? 0);
  let notes = '';

  for (let at = 0; at < argv.length; at += 1) {
    if (argv[at] === '--build') {
      build = Number(argv[at + 1]);
      at += 1;
    } else if (argv[at] === '--notes') {
      notes = argv[at + 1] ?? '';
      at += 1;
    }
  }

  // Counted from the wall clock rather than from anything in the repository, and deliberately
  // large. A published build has to sort above whatever CI last released or the machine that
  // installs it is offered the release straight back — and CI counts its own runs, which this
  // has no way to know.
  if (!Number.isInteger(build) || build <= 0) {
    build = 100_000 + Math.floor((Date.now() - Date.UTC(2026, 0, 1)) / 60_000);
  }

  return { build, notes };
}

/**
 * Runs a command, letting it write to this terminal, and stops everything if it fails.
 *
 * @param {string} command - What to run.
 * @param {string[]} args - Its arguments.
 * @param {object} [env] - Extra environment for it.
 * @returns {void}
 */
function run(command, args, env = {}) {
  execFileSync(command, args, {
    cwd: ROOT,
    stdio: 'inherit',
    env: { ...process.env, ...env },
  });
}

/**
 * Asks for the operator's credentials and returns a session token.
 *
 * The same three things the dashboard asks for. Read here rather than kept in a file, because a
 * token that publishes what every machine runs is not a thing to leave lying next to the code
 * it publishes.
 *
 * @async
 * @returns {Promise<string>} The session token.
 * @throws {Error} If the server refuses.
 */
async function signIn() {
  const ask = createInterface({ input: stdin, output: stdout });
  const email = (await ask.question('operator address: ')).trim().toLowerCase();
  const password = await ask.question('password: ');
  const code = (await ask.question('six-digit code: ')).trim();

  ask.close();

  const salted = await fetch(`${SERVER}/v1/salt?email=${encodeURIComponent(email)}`);

  if (!salted.ok) {
    throw new Error('that address is not an account on this server');
  }

  const { salt } = await salted.json();
  const auth = await deriveAuth(password, Uint8Array.from(salt.match(/../gu).map((pair) => parseInt(pair, 16))));

  const opened = await fetch(`${SERVER}/v1/sessions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ email, auth, code }),
  });

  const answer = await opened.json().catch(() => ({}));

  if (!opened.ok) {
    throw new Error(answer.error ?? 'the server refused that sign-in');
  }

  if (!answer.operator) {
    throw new Error('that account does not look after this server');
  }

  return answer.token;
}

/**
 * Derives the authentication secret from a password, the way every other client does.
 *
 * The module the server itself serves, rather than an Argon2 from somewhere else configured to
 * match. There is one implementation of this on purpose — `prism-secret` says why — and a second
 * one here would show up as an account that signs in on the dashboard and not from a terminal,
 * or the other way round, which is a thing nobody would look for.
 *
 * @async
 * @param {string} password - What was typed.
 * @param {Uint8Array} salt - The account's salt.
 * @returns {Promise<string>} The verifier, as hex.
 * @throws {Error} If the password is too short, or the module refuses it.
 */
async function deriveAuth(password, salt) {
  const fetched = await fetch(`${SERVER}/admin/argon2.wasm`);
  const { instance } = await WebAssembly.instantiate(await fetched.arrayBuffer());
  const { memory, prism_alloc: alloc, prism_free: free, prism_auth: auth } = instance.exports;

  const typed = new TextEncoder().encode(password);
  const passwordAt = alloc(typed.length);
  const saltAt = alloc(salt.length);
  const outAt = alloc(32);

  try {
    new Uint8Array(memory.buffer, passwordAt, typed.length).set(typed);
    new Uint8Array(memory.buffer, saltAt, salt.length).set(salt);

    const code = auth(passwordAt, typed.length, saltAt, outAt);

    if (code === -1) {
      throw new Error('a password is at least eight characters');
    }

    if (code !== 0) {
      throw new Error('that password could not be processed');
    }

    return [...new Uint8Array(memory.buffer, outAt, 32)]
      .map((byte) => byte.toString(16).padStart(2, '0'))
      .join('');
  } finally {
    free(passwordAt, typed.length);
    free(saltAt, salt.length);
    free(outAt, 32);
  }
}

const { target, arch, bundle } = platform();
const { build, notes } = asked(process.argv.slice(2));
const key = process.env.PRISM_UPDATE_KEY ?? DEFAULT_KEY;

if (!existsSync(key)) {
  console.error(`no signing key at ${key}. Set PRISM_UPDATE_KEY to where yours is.`);
  process.exit(2);
}

if (process.platform === 'darwin' && !process.env.APPLE_SIGNING_IDENTITY) {
  console.error(
    'APPLE_SIGNING_IDENTITY is not set, so this build would be signed ad-hoc — a different code\n' +
      'identity from the released builds, which means a keychain prompt and a screen recording\n' +
      'grant to give again. Set it to your Developer ID, which `security find-identity -v -p\n' +
      'codesigning` lists.',
  );
  process.exit(2);
}

const token = await signIn();

console.log(`\nbuilding 1.0.0-local.${build} for ${target}/${arch}\n`);

run('pnpm', ['package'], {
  PRISM_CHANNEL: 'local',
  PRISM_BUILD: String(build),
  TAURI_SIGNING_PRIVATE_KEY: readFileSync(key, 'utf8').trim(),
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD: process.env.PRISM_UPDATE_KEY_PASSWORD ?? '',
});

const version = `1.0.0-local.${build}`;
const made = join(ROOT, 'target/release/bundle', bundle);
const signature = readFileSync(`${made}.sig`, 'utf8').trim();
const body = readFileSync(made);

console.log(`\npublishing ${(body.byteLength / 1e6).toFixed(1)} MB\n`);

const put = await fetch(`${SERVER}/v1/admin/builds/${version}/${target}/${arch}`, {
  method: 'PUT',
  headers: {
    authorization: `Bearer ${token}`,
    'content-type': 'application/octet-stream',
    'x-prism-signature': signature,
    'x-prism-notes': notes,
    // What the bundler called it. Kept rather than rebuilt from the version and the platform,
    // so a list of builds names the files that are actually there.
    'x-prism-filename': basename(made),
  },
  body,
});

if (!put.ok) {
  const why = await put.json().catch(() => ({}));

  console.error(`the server refused it: ${why.error ?? put.status}`);
  process.exit(1);
}

console.log(`${version} is on the local line for ${target}/${arch}.`);
console.log('Machines following it will be offered it within half an hour, or at their next launch.');
