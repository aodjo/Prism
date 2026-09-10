/**
 * Brings a live database up to the shape `schema.sql` describes.
 *
 * Two halves, because SQLite treats the two cases differently.
 *
 * **Tables and indexes** are `CREATE ... IF NOT EXISTS` in `schema.sql`, so that file is simply
 * applied every time: a new table appears, an existing one is left alone. Nothing here lists
 * them, deliberately — a copy of the schema kept in this script is a second place to describe
 * one thing, and the copy is the one that gets forgotten.
 *
 * **Columns** cannot work that way. SQLite has no `ADD COLUMN IF NOT EXISTS`, and a column
 * added to a table that already exists is not added by `CREATE TABLE IF NOT EXISTS`. So those
 * are listed below, and this asks the database what it actually has before issuing each one.
 *
 * Running it on a fresh database and on the one serving accounts right now both end at the same
 * shape, and running it twice is running it once.
 */

import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The database these accounts live in. */
const DATABASE = 'prism-accounts';

/** This package, whatever anybody's working directory is. */
const HERE = dirname(dirname(fileURLToPath(import.meta.url)));

/** The wrangler this package installed, which is not on anybody's `PATH`. */
const WRANGLER = join(HERE, 'node_modules/.bin/wrangler');

/**
 * Columns that were added after a table was first created, and what to add them with.
 *
 * Append here rather than editing an entry: an entry that has already run on the live database
 * will not run again, so changing one changes nothing and only misleads the next reader.
 */
const COLUMNS = [
  {
    table: 'accounts',
    column: 'operator',
    sql: 'ALTER TABLE accounts ADD COLUMN operator INTEGER NOT NULL DEFAULT 0',
  },
  {
    table: 'builds',
    column: 'filename',
    sql: "ALTER TABLE builds ADD COLUMN filename TEXT NOT NULL DEFAULT ''",
  },
  {
    table: 'builds',
    column: 'installer_filename',
    sql: "ALTER TABLE builds ADD COLUMN installer_filename TEXT NOT NULL DEFAULT ''",
  },
  {
    table: 'builds',
    column: 'installer_bytes',
    sql: 'ALTER TABLE builds ADD COLUMN installer_bytes INTEGER NOT NULL DEFAULT 0',
  },
];

/** The one description of what this database is meant to look like. */
const SCHEMA = join(HERE, 'schema.sql');

/**
 * Runs one statement against the database and returns what it answered.
 *
 * @param {string} sql - The statement.
 * @param {boolean} remote - Whether to talk to the deployed database rather than the local one.
 * @returns {unknown[]} The rows, which is empty for a statement that returns none.
 * @throws {Error} If wrangler refused, with wrangler's own message.
 */
function ask(sql, remote) {
  const output = execFileSync(
    WRANGLER,
    ['d1', 'execute', DATABASE, remote ? '--remote' : '--local', '--json', '--command', sql],
    { cwd: HERE, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] },
  );

  // Wrangler prints its own noise before the JSON when it feels like it.
  const start = output.indexOf('[');

  return start < 0 ? [] : (JSON.parse(output.slice(start))[0]?.results ?? []);
}

const remote = process.argv.includes('--remote');
const added = [];

// The schema file itself, every time. Every statement in it is `IF NOT EXISTS`, so issuing it
// against a database that already has everything does nothing at all.
//
// It used to be a list of `CREATE TABLE` statements copied into this script, and the copy is
// what went wrong: `region_reports` was added to `schema.sql`, this list was not, and the
// migration reported "already has every column" against a database with no such table. Two
// places to describe one thing, and the one being edited was not the one being run.
execFileSync(
  WRANGLER,
  ['d1', 'execute', DATABASE, remote ? '--remote' : '--local', '--file', SCHEMA],
  { cwd: HERE, encoding: 'utf8', stdio: ['ignore', 'ignore', 'inherit'] },
);

for (const { table, column, sql } of COLUMNS) {
  const present = ask(`PRAGMA table_info(${table})`, remote).some((row) => row.name === column);

  if (present) {
    continue;
  }

  ask(sql, remote);
  added.push(`${table}.${column}`);
}

console.log(
  added.length > 0
    ? `Added ${added.join(', ')} to the ${remote ? 'deployed' : 'local'} database.`
    : `The ${remote ? 'deployed' : 'local'} database already has every column.`,
);
