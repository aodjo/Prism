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
 * - A Developer ID on macOS, which it finds in the keychain. `APPLE_SIGNING_IDENTITY` names
 *   another one where there is a choice. Without any, every rebuild carries a different
 *   ad-hoc identity — a keychain prompt each time, and a screen recording grant to give again.
 * - An operator to say yes in a browser, which it opens. Nothing is typed here.
 *
 * @module
 */

import { execFileSync } from 'node:child_process';
import { readFileSync, existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

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
 * The Developer ID this machine can sign with, if it has one.
 *
 * Found rather than asked for. It is already in the keychain — that is what makes it usable at
 * all — so requiring somebody to name it as well is asking them to type back something the
 * machine could read, and getting it slightly wrong is a build signed ad-hoc with no sign that
 * anything went differently.
 *
 * The first is taken when there are several. A machine with two Developer IDs is one whose
 * certificate is being rotated, and either signs a build somebody is about to install on their
 * own machines.
 *
 * @returns {string} The identity's name, or an empty string on a machine without one.
 */
function developerId() {
  if (process.platform !== 'darwin') {
    return '';
  }

  try {
    const listed = execFileSync('security', ['find-identity', '-v', '-p', 'codesigning'], {
      encoding: 'utf8',
    });

    return /"(Developer ID Application: [^"]+)"/u.exec(listed)?.[1] ?? '';
  } catch {
    // No keychain to ask, which is the same answer as no certificate in it.
    return '';
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
 * Asks a browser for permission to publish, and waits until somebody answers.
 *
 * A password typed at a shell prompt is the one credential that decides what every machine runs,
 * put somewhere with a history file and a scrollback buffer. This asks for nothing: it shows a
 * code, opens the dashboard, and waits for whoever is already signed in there to confirm the
 * code on their screen is the code on this one.
 *
 * Which is also why the two open endpoints behind it are open. Asking hands out a code that does
 * nothing on its own, and waiting is useless without the secret half of it.
 *
 * @async
 * @param {string} what - What is asking, in the words shown to whoever approves it.
 * @returns {Promise<string>} A session token, good for half an hour.
 * @throws {Error} If it is refused, or nobody answers in time.
 */
async function allowed(what) {
  const asked = await fetch(`${SERVER}/v1/publish/request`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ what }),
  });

  if (!asked.ok) {
    throw new Error('the server would not take the request');
  }

  const { device_code: device, user_code: user, verify_url: where, expires_in: patience } =
    await asked.json();

  console.log(`\n  ${user}\n`);
  console.log('Opening the dashboard. Check that code matches, then allow it.');
  console.log(`If nothing opens: ${where}\n`);

  open(where);

  const until = Date.now() + patience * 1000;

  while (Date.now() < until) {
    await new Promise((wake) => setTimeout(wake, 2000));

    const waited = await fetch(
      `${SERVER}/v1/publish/wait?device_code=${encodeURIComponent(device)}`,
    );

    if (waited.status === 202) {
      continue;
    }

    if (waited.status === 404) {
      throw new Error('that request was refused');
    }

    if (!waited.ok) {
      throw new Error('the server stopped answering');
    }

    const { token, email } = await waited.json();

    console.log(`Allowed by ${email}.`);

    return token;
  }

  throw new Error('nobody answered in time');
}

/**
 * Opens a link in whatever the system uses for one.
 *
 * A failure is not reported. The address was printed a moment ago, and a terminal that could not
 * launch a browser has not stopped anybody from opening one.
 *
 * @param {string} link - Where to go.
 * @returns {void}
 */
function open(link) {
  const opener = { darwin: 'open', win32: 'start', linux: 'xdg-open' }[process.platform];

  try {
    execFileSync(opener, [link], { stdio: 'ignore' });
  } catch {
    // Printed above, which is the part that matters.
  }
}

const { target, arch, bundle } = platform();
const { build, notes } = asked(process.argv.slice(2));
const key = process.env.PRISM_UPDATE_KEY ?? DEFAULT_KEY;

if (!existsSync(key)) {
  console.error(`no signing key at ${key}. Set PRISM_UPDATE_KEY to where yours is.`);
  process.exit(2);
}

const identity = process.env.APPLE_SIGNING_IDENTITY ?? developerId();

if (process.platform === 'darwin' && !identity) {
  console.error(
    'This machine has no Developer ID to sign with, so the build would be signed ad-hoc — a\n' +
      'different code identity from every other build, which means a keychain prompt on first\n' +
      'run and a screen recording grant to give again. Install the certificate, or set\n' +
      'APPLE_SIGNING_IDENTITY to name another one.',
  );
  process.exit(2);
}

const token = await allowed(`prism · ${target} · ${arch}`);

console.log(`\nbuilding 1.0.0-local.${build} for ${target}/${arch}\n`);

run('pnpm', ['package'], {
  APPLE_SIGNING_IDENTITY: identity,
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
