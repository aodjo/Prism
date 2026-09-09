/**
 * Works out what this build calls itself, and writes it where the bundler will read it.
 *
 * Two numbers, because they answer different questions. The **version** is chosen by a person
 * and says what the software is; the **build number** is counted by the machine that produced
 * it and says which copy this is. A person reads the first and reports the second.
 *
 * They cannot simply be printed side by side, because the updater compares versions and semver
 * ignores build metadata when it does: `1.0.0+848` is not greater than `1.0.0+847`, so an
 * application versioned that way would never see an update. What orders correctly is a
 * prerelease identifier, and that is what a development build is — a numbered step toward the
 * version that has not been released yet.
 *
 *     production   1.0.0            tagged by hand on main
 *     development  1.1.0-dev.847    the next version, build 847
 *
 * `1.1.0-dev.848 > 1.1.0-dev.847`, and `1.1.0 > 1.1.0-dev.anything`. So a development build
 * updates to the next development build, and the day the release is cut it updates to that and
 * stops being a prerelease. Nobody has to move channel for it to happen.
 *
 * Reads `PRISM_CHANNEL` (`production` or `development`, default `development`) and
 * `PRISM_BUILD` (the CI run number, default `0`). Writes `tauri.conf.json` in place and prints
 * the version, so a workflow can capture it.
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The repository root. */
const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));

/** What the bundler reads. */
const CONFIG = join(ROOT, 'crates/prism-tauri/tauri.conf.json');

/**
 * The version a release carries, and the one development builds count toward.
 *
 * The single place it is written down. A release is cut by setting this and tagging; every
 * development build between two releases is a prerelease of the one being worked toward.
 */
const VERSION = '1.0.0';

const channel = process.env.PRISM_CHANNEL ?? 'development';
const build = Number(process.env.PRISM_BUILD ?? 0);

if (channel !== 'production' && channel !== 'development') {
  console.error(`PRISM_CHANNEL must be production or development, not ${channel}`);
  process.exit(2);
}

if (!Number.isInteger(build) || build < 0) {
  console.error(`PRISM_BUILD must be a whole number, not ${process.env.PRISM_BUILD}`);
  process.exit(2);
}

const version = channel === 'production' ? VERSION : `${VERSION}-dev.${build}`;

const config = JSON.parse(readFileSync(CONFIG, 'utf8'));
config.version = version;
writeFileSync(CONFIG, `${JSON.stringify(config, null, 2)}\n`);

// Printed as `key=value` lines so a workflow can read them straight into its own environment.
console.log(`version=${version}`);
console.log(`channel=${channel}`);
console.log(`build=${build}`);
