/**
 * Builds the stream process and puts it where the shell's bundle will pick it up.
 *
 * Watching another machine happens in a process of its own — that is what keeps a video frame
 * out of the interface by construction rather than by discipline — so the application is two
 * binaries, and an installer carrying only the shell is an installer that cannot stream. It
 * looked exactly like a working build until somebody pressed Connect.
 *
 * # Why the copy is named for the target
 *
 * It ships as an `externalBin` rather than as a bundle resource, and Tauri looks those up by
 * appending the target triple: `sidecar/prism-stream` in the configuration means the file
 * `sidecar/prism-stream-aarch64-apple-darwin` on disk. The suffix is stripped again on the way
 * into the bundle, so what arrives beside the shell is plain `prism-stream`.
 *
 * That indirection buys two things a resource could not. Tauri signs external binaries with the
 * same identity, entitlements and hardened runtime as the shell itself — a Mach-O under
 * `Contents/Resources` is signed by nobody, not even by `codesign --deep`, which does not
 * descend there, and Apple's notary service rejects the bundle for it. And on Linux it lands in
 * `usr/bin` beside the shell instead of `usr/lib/Prism`, which is where the shell actually looks.
 */

import { execFileSync } from 'node:child_process';
import { cpSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

/** Where the shell's bundle configuration expects to find it. */
const into = join(root, 'crates', 'prism-tauri', 'sidecar');

/** What cargo calls the binary it just built. */
const built = `prism-stream${process.platform === 'win32' ? '.exe' : ''}`;

/**
 * The machine this build is for, as Rust names it.
 *
 * Asked of the compiler rather than assembled from `process.platform` and `process.arch`, so
 * that a name Node and Rust disagree about — and they disagree about several — cannot make the
 * bundler look for a file nobody wrote.
 */
const triple = /^host: (.+)$/mu.exec(
  execFileSync('rustc', ['-vV'], { encoding: 'utf8' }),
)?.[1];

if (!triple) {
  process.stderr.write('could not ask rustc what it builds for\n');
  process.exit(1);
}

/** What the bundler will look for. */
const shipped = `prism-stream-${triple}${process.platform === 'win32' ? '.exe' : ''}`;

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
cpSync(join(root, 'target', 'release', built), join(into, shipped));

process.stdout.write(`sidecar: ${join(into, shipped)}\n`);
