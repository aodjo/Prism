/**
 * Grants or withdraws the right to look after this server.
 *
 * There is no way to do this over the API without already holding it, which is the point: the
 * first operator has to be made by somebody who owns the database rather than by anybody who
 * can reach the server. After that an operator can appoint the next one from the dashboard.
 *
 * @example
 * node scripts/operator.mjs me@junx.dev --remote
 * node scripts/operator.mjs me@junx.dev --revoke --remote
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
 * Quotes a value as a SQL string literal.
 *
 * `wrangler d1 execute` takes a statement and nothing else — there is no way to bind a value —
 * so the address has to go into the text. It is checked against [`looksLikeEmail`] before it
 * gets here and the quotes are doubled, which is what SQLite escapes them with.
 *
 * @param {string} value - What to quote.
 * @returns {string} The literal.
 */
function quoted(value) {
  return `'${value.replaceAll("'", "''")}'`;
}

/**
 * Runs one statement against the database and returns what it answered.
 *
 * @param {string} sql - The statement, with `?` for each value.
 * @param {string[]} values - What to put where the question marks are, in order.
 * @param {boolean} remote - Whether to talk to the deployed database rather than the local one.
 * @returns {unknown[]} The rows, which is empty for a statement that returns none.
 * @throws {Error} If wrangler refused, with wrangler's own message.
 */
function ask(sql, values, remote) {
  let filled = sql;

  for (const value of values) {
    filled = filled.replace('?', quoted(value));
  }

  const output = execFileSync(
    WRANGLER,
    ['d1', 'execute', DATABASE, remote ? '--remote' : '--local', '--json', '--command', filled],
    { cwd: HERE, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] },
  );

  const start = output.indexOf('[');

  return start < 0 ? [] : (JSON.parse(output.slice(start))[0]?.results ?? []);
}

const args = process.argv.slice(2);
const remote = args.includes('--remote');
const revoke = args.includes('--revoke');
const email = args.find((arg) => !arg.startsWith('--'));

if (!email) {
  console.error('Which address? node scripts/operator.mjs <address> [--revoke] [--remote]');
  process.exit(2);
}

// Checked because it is about to be written into a statement rather than bound to one. An
// address that is not an address is a typo at best, and this is the one script that runs with
// whatever credentials the machine holding the database has.
if (!/^[^\s'"@;]+@[^\s'"@;]+\.[^\s'"@;]+$/u.test(email)) {
  console.error(`${email} does not look like an address.`);
  process.exit(2);
}

const exists = ask('SELECT email, operator FROM accounts WHERE email = ?', [email], remote);

if (exists.length === 0) {
  console.error(`There is no account for ${email}. It has to sign up first.`);
  process.exit(1);
}

// Refused rather than allowed, because a server with no operator cannot be given one back
// through the dashboard — only from here, by somebody who owns the database.
if (revoke) {
  const others = ask(
    'SELECT COUNT(*) AS n FROM accounts WHERE operator = 1 AND email <> ?',
    [email],
    remote,
  );

  if ((others[0]?.n ?? 0) === 0) {
    console.error(`${email} is the only operator. Appoint another one before taking this away.`);
    process.exit(1);
  }
}

ask('UPDATE accounts SET operator = ? WHERE email = ?', [revoke ? '0' : '1', email], remote);

console.log(
  revoke
    ? `${email} no longer looks after this server.`
    : `${email} now looks after this server. Sign in at the dashboard with that account.`,
);
