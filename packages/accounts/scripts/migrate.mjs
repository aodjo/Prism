/**
 * Brings a live database up to the shape `schema.sql` describes.
 *
 * `schema.sql` is written with `CREATE TABLE IF NOT EXISTS`, which is the right thing for a
 * database that does not exist yet and does nothing at all for one that does: a column added
 * to a table that is already there is not added. This asks the database what it actually has
 * and issues only what is missing, so running it on a fresh database and on the one serving
 * accounts right now both end at the same shape, and running it twice is running it once.
 *
 * SQLite has no `ADD COLUMN IF NOT EXISTS`, which is why this is a script and not a file of SQL.
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
];

/**
 * Tables added after the first deployment.
 *
 * `CREATE TABLE IF NOT EXISTS` is already idempotent, so unlike a column these can simply be
 * issued every time.
 */
const TABLES = [
  'CREATE TABLE IF NOT EXISTS regions (' +
    ' name TEXT PRIMARY KEY NOT NULL,' +
    ' url TEXT NOT NULL,' +
    ' limit_gb INTEGER,' +
    ' added_unix INTEGER NOT NULL)',
  'CREATE TABLE IF NOT EXISTS audit (' +
    ' id INTEGER PRIMARY KEY AUTOINCREMENT,' +
    ' actor TEXT NOT NULL,' +
    ' action TEXT NOT NULL,' +
    ' subject TEXT NOT NULL,' +
    " detail TEXT NOT NULL DEFAULT ''," +
    ' at_unix INTEGER NOT NULL)',
  'CREATE INDEX IF NOT EXISTS audit_recent ON audit (at_unix DESC)',
];

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

for (const sql of TABLES) {
  ask(sql, remote);
}

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
