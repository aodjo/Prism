/**
 * Builds the stream client and puts it where the shell's bundle will pick it up.
 *
 * Watching another machine happens in a process of its own — that is what keeps a video frame
 * out of the interface by construction rather than by discipline — so the application is two
 * binaries, and an installer carrying only the shell is an installer that cannot stream. It
 * looked exactly like a working build until somebody pressed Connect.
 *
 * The binary is copied under its own name rather than renamed, because the shell looks for
 * `prism-stream` with the platform's executable suffix and Windows will not run a file without
 * one. One file in a directory of its own, so the bundle can take the directory without naming
 * what is in it.
 */

import { execFileSync } from 'node:child_process';
import { cpSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

/** Where the shell's bundle configuration expects to find it. */
const into = join(root, 'crates', 'prism-tauri', 'sidecar');

/** What the stream process is called on this platform. */
const name = `prism-stream${process.platform === 'win32' ? '.exe' : ''}`;

/**
 * The features the stream process needs to be worth shipping.
 *
 * Without `window` it decodes and counts frames and draws nothing, which is the shape wanted for
 * a measurement run and useless as the half of an application somebody watches through.
 */
const FEATURES = 'window';

execFileSync(
  'cargo',
  ['build', '--release', '--locked', '-p', 'prism-stream', '--features', FEATURES],
  { cwd: root, stdio: 'inherit' },
);

rmSync(into, { recursive: true, force: true });
mkdirSync(into, { recursive: true });
cpSync(join(root, 'target', 'release', name), join(into, name));

process.stdout.write(`sidecar: ${join(into, name)}\n`);
