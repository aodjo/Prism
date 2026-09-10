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
 * - An operator to say yes in a browser, which it opens. Nothing is typed here, and the
 *   answer is kept for an hour so that a run of four builds is one approval.
 *
 * @module
 */

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

/** Where the account server lives. */
const SERVER = process.env.PRISM_ACCOUNT_SERVER ?? 'https://accounts.presm.kr';

/**
 * Where the token from the last approval is kept.
 *
 * Beside this machine's identity, which is the other credential that is about this machine
 * rather than about the repository. Never in the working tree.
 */
const TOKEN_FILE = join(homedir(), '.prism/publish.json');

/** Where the updater's signing key is kept, unless something says otherwise. */
const DEFAULT_KEY = join(homedir(), 'Documents/prism-keys/prism-update.key');

/**
 * What the updater calls this machine.
 *
 * The names Rust uses for a target, which is what Tauri asks the endpoint with — not the ones
 * Node uses for the same two things.
 *
 * @returns {{target: string, arch: string, bundle: string, installer: string}} The platform,
 *   what its bundle file is called under `target/release/bundle`, and the directory holding
 *   the installer where that is a different file.
 */
function platform() {
  const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64';

  switch (process.platform) {
    // macOS is the only one with two artifacts. The updater takes the archive; a person opens
    // the disk image, and they are not the same bytes. Elsewhere the installer is the file the
    // updater fetches, so there is nothing else to send.
    case 'darwin':
      return { target: 'darwin', arch, bundle: 'macos/Prism.app.tar.gz', installer: 'dmg' };
    case 'win32':
      return { target: 'windows', arch, bundle: 'nsis/Prism_x64-setup.exe', installer: '' };
    case 'linux':
      return { target: 'linux', arch, bundle: 'appimage/prism.AppImage', installer: '' };
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
 * A `PATH` that can reach `cargo`, or an empty string if nothing here can.
 *
 * Tauri shells out to `cargo metadata` before it builds anything, so a `PATH` without it fails a
 * minute in with a message about workspace directories. Rustup installs to `~/.cargo/bin` and
 * puts it on the path from a file the shell sources — which an interactive shell has read and
 * something launched another way may not have.
 *
 * Found rather than demanded, for the same reason the signing identity is: it is already
 * installed, and asking somebody to arrange their environment before running a command is asking
 * them to do what the command could have done.
 *
 * @returns {string} What `PATH` should be, or an empty string when there is no cargo to find.
 */
function reachingCargo() {
  const path = process.env.PATH ?? '';

  try {
    execFileSync('cargo', ['--version'], { stdio: 'ignore' });

    return path;
  } catch {
    // Not on the path as it stands, which is what the rest of this is for.
  }

  const rustup = join(homedir(), '.cargo/bin');

  if (!existsSync(join(rustup, 'cargo'))) {
    return '';
  }

  return `${rustup}:${path}`;
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

  return { build: Number.isInteger(build) && build > 0 ? build : 0, notes };
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
 * The number after the highest one on the local line.
 *
 * Asked of the server rather than counted here, so the line has one sequence however many
 * machines publish to it. Counting locally would give two machines the same number and a version
 * that means one thing on one of them and another on the other.
 *
 * @async
 * @param {string} token - An operator session.
 * @returns {Promise<number>} What this build should be called.
 */
async function nextNumber(token) {
  const listed = await fetch(`${SERVER}/v1/admin/builds?channel=local`, {
    headers: { authorization: `Bearer ${token}` },
  }).catch(() => null);

  if (!listed?.ok) {
    return 1;
  }

  const { builds } = await listed.json().catch(() => ({ builds: [] }));
  const highest = (builds ?? []).reduce((most, one) => {
    const found = /-local\.(\d+)$/u.exec(one.version ?? '');

    return found ? Math.max(most, Number(found[1])) : most;
  }, 0);

  return highest + 1;
}

/**
 * The token this machine was last given, if it is still worth anything.
 *
 * Kept so that publishing four times while fixing one thing does not mean approving four times
 * in a browser. Beside the identity key rather than in the repository: it is about this machine
 * and it is a credential, and neither belongs in a working tree.
 *
 * Checked against the server rather than trusted for its expiry alone. A session can be ended
 * from the dashboard at any moment, and finding that out here costs one request — finding it out
 * after the build costs three minutes.
 *
 * @async
 * @returns {Promise<string>} The token, or an empty string if there is not a usable one.
 */
async function kept() {
  let held = null;

  try {
    held = JSON.parse(readFileSync(TOKEN_FILE, 'utf8'));
  } catch {
    return '';
  }

  if (!held?.token || (held.until ?? 0) * 1000 <= Date.now()) {
    return '';
  }

  const still = await fetch(`${SERVER}/v1/session`, {
    headers: { authorization: `Bearer ${held.token}` },
  }).catch(() => null);

  if (!still?.ok) {
    rmSync(TOKEN_FILE, { force: true });

    return '';
  }

  const who = await still.json().catch(() => ({}));

  if (!who.operator) {
    rmSync(TOKEN_FILE, { force: true });

    return '';
  }

  console.log(`Still allowed as ${who.email}.`);

  return held.token;
}

/**
 * Writes down a token so the next run does not have to ask again.
 *
 * Readable by this account and nobody else. A failure is not reported: the token in hand still
 * works, and the only cost of not keeping it is approving again next time.
 *
 * @param {string} token - What the server issued.
 * @returns {void}
 */
function keep(token) {
  try {
    mkdirSync(dirname(TOKEN_FILE), { recursive: true });
    writeFileSync(TOKEN_FILE, JSON.stringify({ token, until: SESSION_UNTIL() }), { mode: 0o600 });
  } catch {
    // Nothing to do about it, and nothing lost.
  }
}

/**
 * When a freshly issued token stops being worth trying.
 *
 * A little short of the hour the server gives it, so that a run which starts just inside the
 * window does not find the session gone halfway through its upload.
 *
 * @returns {number} A unix time.
 */
const SESSION_UNTIL = () => Math.floor(Date.now() / 1000) + 55 * 60;

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

    console.log(`Allowed by ${email}. This machine will not have to ask again for an hour.`);
    keep(token);

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

const { target, arch, bundle, installer } = platform();
const { build, notes } = asked(process.argv.slice(2));
const key = process.env.PRISM_UPDATE_KEY ?? DEFAULT_KEY;

if (!existsSync(key)) {
  console.error(`no signing key at ${key}. Set PRISM_UPDATE_KEY to where yours is.`);
  process.exit(2);
}

const identity = process.env.APPLE_SIGNING_IDENTITY ?? developerId();
const path = reachingCargo();

if (!path) {
  console.error(
    'cargo is not on the path and there is none at ~/.cargo/bin. Tauri asks it where the\n' +
      'workspace is before it builds anything, so this would fail a minute in. Install Rust,\n' +
      'or open a shell that has sourced ~/.cargo/env.',
  );
  process.exit(2);
}

if (process.platform === 'darwin' && !identity) {
  console.error(
    'This machine has no Developer ID to sign with, so the build would be signed ad-hoc — a\n' +
      'different code identity from every other build, which means a keychain prompt on first\n' +
      'run and a screen recording grant to give again. Install the certificate, or set\n' +
      'APPLE_SIGNING_IDENTITY to name another one.',
  );
  process.exit(2);
}

const token = (await kept()) || (await allowed(`prism · ${target} · ${arch}`));

const numbered = build || (await nextNumber(token));

console.log(`\nbuilding 1.0.0-local.${numbered} for ${target}/${arch}\n`);

run('pnpm', ['package'], {
  PATH: path,
  APPLE_SIGNING_IDENTITY: identity,
  PRISM_CHANNEL: 'local',
  PRISM_BUILD: String(numbered),
  TAURI_SIGNING_PRIVATE_KEY: readFileSync(key, 'utf8').trim(),
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD: process.env.PRISM_UPDATE_KEY_PASSWORD ?? '',
});

const version = `1.0.0-local.${numbered}`;
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

// The disk image, where there is one. Sent after the archive because the archive is what a
// machine updates from: a version that exists at all should be one the updater can serve, and
// an installer with nothing behind it would be a download that installs an update nobody can
// receive.
if (installer) {
  const folder = join(ROOT, 'target/release/bundle', installer);
  const image = readdirSync(folder).find((each) => each.endsWith('.dmg'));

  if (image) {
    const bytes = readFileSync(join(folder, image));

    console.log(`publishing ${image} — ${(bytes.byteLength / 1e6).toFixed(1)} MB`);

    const sent = await fetch(
      `${SERVER}/v1/admin/builds/${version}/${target}/${arch}/installer`,
      {
        method: 'PUT',
        headers: {
          authorization: `Bearer ${token}`,
          'content-type': 'application/octet-stream',
          'x-prism-filename': image,
        },
        body: bytes,
      },
    );

    if (!sent.ok) {
      console.error('the archive went up but the disk image did not');
    }
  }
}

console.log(`\n${version} is on the local line for ${target}/${arch}.`);
console.log('Machines following it will be offered it within half an hour, or at their next launch.');
