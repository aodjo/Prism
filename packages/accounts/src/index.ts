/**
 * The account server, as a Worker over D1.
 *
 * The same API the Rust one served, endpoint for endpoint and sentence for sentence, so the
 * machines talking to it need no change: they already speak HTTPS to a name.
 *
 * What moving it buys is durability. The store was one JSON file on one disk of one cheap
 * server, rewritten whole on every change, and it is the only copy of every account's sealed
 * key and second factor. Here it is a database somebody else keeps, replicates and backs up.
 *
 * What it does not buy is speed, and it was never meant to. No stream touches this server —
 * `crates/prism-core/src/control/` has no account call in it — so a session is not one
 * millisecond faster or slower for where accounts live. Signalling, which IS on the session
 * path when a relay is used, stays exactly where it was: a Rust binary on a machine per region,
 * because it speaks UDP and cannot move here.
 *
 * Nothing here can open a private key. The sealed key is sealed under a secret derived from the
 * password on the machine it was typed on, and that secret is never sent.
 */

import {
  base32,
  hex,
  randomBytes,
  sameSecret,
  sha256,
  totpMatches,
  totpUri,
  unbase32,
  unhex,
  verifierOf,
} from './crypto.js';

/** What the runtime hands each request. */
export interface Env {
  /** The account store. */
  readonly prism_accounts: D1Database;
  /** Where signed-in machines should look for signalling, as `host:port`. */
  readonly PRISM_ADVERTISE?: string;
  /** The mail provider's key. Without it an address is only ever a name here. */
  readonly PRISM_RESEND_KEY?: string;
  /** The address confirmations come from. */
  readonly PRISM_MAIL_FROM?: string;
  /** Lets whoever holds it list and delete accounts. Absent means those routes do not exist. */
  readonly PRISM_ADMIN_TOKEN?: string;
}

/** How long a session lasts. */
const SESSION_SECONDS = 12 * 60 * 60;

/** How long an emailed signup code is good for. */
const CHALLENGE_SECONDS = 15 * 60;

/** How many bytes a session token is made of. */
const TOKEN_BYTES = 32;

/** How many bytes the second factor's shared secret is. */
const TOTP_BYTES = 20;

/** The length of the salt a password is hashed with. */
const SALT_BYTES = 16;

/** The length of the secret derived from a password. */
const SECRET_BYTES = 32;

/** The shortest and longest an address may be. */
const EMAIL_LENGTH = { min: 3, max: 254 };

/** Deliberately vague, so that saying no never says which of the three was wrong. */
const REFUSED = 'The email, password or code is wrong.';

/** The moment, in whole seconds. */
function nowUnix(): number {
  return Math.floor(Date.now() / 1000);
}

/**
 * Answers with JSON.
 *
 * @param {unknown} body - What to send.
 * @param {number} [status=200] - The status.
 * @returns {Response} The response.
 */
function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

/**
 * Answers with a sentence somebody reads.
 *
 * @param {number} status - The status.
 * @param {string} error - The sentence.
 * @returns {Response} The response.
 */
function fail(status: number, error: string): Response {
  return json({ error }, status);
}

/** What the application sent that could not be read. */
function malformed(field: string): Response {
  return fail(400, `The application sent a ${field} this server could not read.`);
}

/**
 * Whether a name is one an account may have.
 *
 * Deliberately loose. The only claim worth making is that this could be delivered to; the
 * address is proved by a code arriving at it, not by a pattern, and a stricter rule would
 * mostly reject addresses that are perfectly real.
 */
function looksLikeEmail(email: string): boolean {
  const [local, domain, ...rest] = email.split('@');

  return (
    rest.length === 0 &&
    email.length >= EMAIL_LENGTH.min &&
    email.length <= EMAIL_LENGTH.max &&
    (local?.length ?? 0) > 0 &&
    (domain?.length ?? 0) > 0 &&
    domain?.includes('.') === true &&
    !email.includes(' ')
  );
}

/** One machine on an account, as the wire names it. */
interface Device {
  readonly public_key: string;
  readonly label: string;
}

/** Reads an account's machines, oldest first. */
async function devicesOf(env: Env, email: string): Promise<Device[]> {
  const { results } = await env.prism_accounts
    .prepare('SELECT public_key, label FROM devices WHERE email = ? ORDER BY added_unix')
    .bind(email)
    .all<Device>();

  return results ?? [];
}

/**
 * The value that makes an unregistered address answer like a registered one.
 *
 * Asking for a salt must answer for every address, or the answer says which addresses are
 * accounts. The made-up one has to be the same every time it is asked, so it is derived from a
 * value this store keeps rather than from anything a request carries. Made once, on first use.
 */
async function decoy(env: Env): Promise<Uint8Array> {
  const row = await env.prism_accounts
    .prepare('SELECT decoy FROM settings WHERE id = 1')
    .first<{ decoy: string }>();

  if (row) {
    return unhex(row.decoy) ?? randomBytes(32);
  }

  const made = randomBytes(32);
  await env.prism_accounts
    .prepare('INSERT OR IGNORE INTO settings (id, decoy) VALUES (1, ?)')
    .bind(hex(made))
    .run();

  const settled = await env.prism_accounts
    .prepare('SELECT decoy FROM settings WHERE id = 1')
    .first<{ decoy: string }>();

  return (settled && unhex(settled.decoy)) || made;
}

/** Returns the account behind a bearer token, or `null` when there is none. */
async function whose(env: Env, request: Request): Promise<string | null> {
  const token = request.headers.get('authorization')?.replace(/^Bearer /, '');

  if (!token) {
    return null;
  }

  const row = await env.prism_accounts
    .prepare('SELECT email, expires_unix FROM sessions WHERE token_hash = ?')
    .bind(hex(await sha256(token)))
    .first<{ email: string; expires_unix: number }>();

  if (!row || row.expires_unix <= nowUnix()) {
    return null;
  }

  return row.email;
}

/** What signing in and resuming both answer with. */
async function describe(env: Env, email: string): Promise<Record<string, unknown>> {
  const account = await env.prism_accounts
    .prepare('SELECT sealed_key, relay_allowed FROM accounts WHERE email = ?')
    .bind(email)
    .first<{ sealed_key: string; relay_allowed: number }>();

  return {
    sealed_key: account?.sealed_key ?? '',
    devices: await devicesOf(env, email),
    relay_allowed: (account?.relay_allowed ?? 1) === 1,
    rendezvous: env.PRISM_ADVERTISE ?? '',
  };
}

/**
 * Sends a signup code, if this server has anything to send it with.
 *
 * @returns {Promise<boolean>} Whether one was sent and has to be typed back in.
 */
async function sendCode(env: Env, email: string, code: string): Promise<boolean> {
  if (!env.PRISM_RESEND_KEY || !env.PRISM_MAIL_FROM) {
    return false;
  }

  const sent = await fetch('https://api.resend.com/emails', {
    method: 'POST',
    headers: {
      authorization: `Bearer ${env.PRISM_RESEND_KEY}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify({
      from: env.PRISM_MAIL_FROM,
      to: [email],
      subject: 'Your Prism code',
      text: `${code}\n\nIt is good for fifteen minutes. If you did not ask for it, ignore this.`,
    }),
  });

  if (!sent.ok) {
    throw new Error(`the confirmation could not be sent (${sent.status})`);
  }

  return true;
}

/** Removes what has expired, so neither table grows without bound. */
async function sweep(env: Env): Promise<void> {
  const now = nowUnix();

  await env.prism_accounts.batch([
    env.prism_accounts.prepare('DELETE FROM sessions WHERE expires_unix <= ?').bind(now),
    env.prism_accounts.prepare('DELETE FROM challenges WHERE expires_unix <= ?').bind(now),
  ]);
}

export default {
  /**
   * Answers one request.
   *
   * @param {Request} request - What arrived.
   * @param {Env} env - The bindings.
   * @returns {Promise<Response>} The answer.
   */
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    /** Reads the body, or nothing when it is not the JSON it claimed to be. */
    const body = async <T>(): Promise<T | null> => {
      try {
        return (await request.json()) as T;
      } catch {
        return null;
      }
    };

    try {
      if (path === '/v1/health' && method === 'GET') {
        const count = await env.prism_accounts
          .prepare('SELECT COUNT(*) AS n FROM accounts')
          .first<{ n: number }>();

        // Whether an address means anything here is said out loud. Without a mail key nobody is
        // asked to prove one, so an address is only ever a name — fine for a server one person
        // runs for their own machines, and wrong for one strangers can reach. The Rust server
        // this replaced printed that at startup; a Worker has no startup to print at, so the
        // only place it can be seen is somewhere somebody can ask.
        return json({
          ok: true,
          accounts: count?.n ?? 0,
          verifiesAddresses: Boolean(env.PRISM_RESEND_KEY && env.PRISM_MAIL_FROM),
        });
      }

      // Answered for every address, registered or not, because an answer only registered
      // addresses got would be a way to ask which addresses are accounts.
      if (path === '/v1/salt' && method === 'GET') {
        const email = (url.searchParams.get('email') ?? '').trim().toLowerCase();

        if (!looksLikeEmail(email)) {
          return malformed('email');
        }

        const account = await env.prism_accounts
          .prepare('SELECT salt FROM accounts WHERE email = ?')
          .bind(email)
          .first<{ salt: string }>();

        if (account) {
          return json({ salt: account.salt });
        }

        const made = await sha256('prism-decoy-salt-v1', await decoy(env), email);

        return json({ salt: hex(made.slice(0, SALT_BYTES)) });
      }

      if (path === '/v1/accounts/challenge' && method === 'POST') {
        const sent = await body<{ email?: string }>();
        const email = (sent?.email ?? '').trim().toLowerCase();

        if (!looksLikeEmail(email)) {
          return fail(400, 'That does not look like an email address.');
        }

        const taken = await env.prism_accounts
          .prepare('SELECT 1 AS one FROM accounts WHERE email = ?')
          .bind(email)
          .first();

        if (taken) {
          return fail(409, 'That address already has an account — sign in instead.');
        }

        if (!env.PRISM_RESEND_KEY) {
          return json({ sent: false });
        }

        const code = String(crypto.getRandomValues(new Uint32Array(1))[0] as number % 1_000_000)
          .padStart(6, '0');

        await sendCode(env, email, code);
        await env.prism_accounts
          .prepare(
            'INSERT INTO challenges (email, code, expires_unix) VALUES (?, ?, ?) ' +
              'ON CONFLICT(email) DO UPDATE SET code = excluded.code, expires_unix = excluded.expires_unix',
          )
          .bind(email, code, nowUnix() + CHALLENGE_SECONDS)
          .run();

        return json({ sent: true });
      }

      if (path === '/v1/accounts' && method === 'POST') {
        const sent = await body<{
          email?: string;
          code?: string;
          salt?: string;
          auth?: string;
          sealed_key?: string;
        }>();

        const email = (sent?.email ?? '').trim().toLowerCase();

        if (!looksLikeEmail(email)) {
          return fail(400, 'That does not look like an email address.');
        }

        const salt = unhex(sent?.salt ?? '');
        const auth = unhex(sent?.auth ?? '');

        if (!salt || salt.length !== SALT_BYTES) {
          return malformed('salt');
        }

        if (!auth || auth.length !== SECRET_BYTES) {
          return malformed('auth');
        }

        const taken = await env.prism_accounts
          .prepare('SELECT 1 AS one FROM accounts WHERE email = ?')
          .bind(email)
          .first();

        if (taken) {
          return fail(409, 'That address already has an account — sign in instead.');
        }

        // A code is required only where one could have been sent. A server with no mail
        // configured creates accounts already verified and says so at startup, which is right
        // for a server one person runs for their own machines.
        let verified = true;

        if (env.PRISM_RESEND_KEY) {
          const waiting = await env.prism_accounts
            .prepare('SELECT code, expires_unix FROM challenges WHERE email = ?')
            .bind(email)
            .first<{ code: string; expires_unix: number }>();

          const given = (sent?.code ?? '').trim();

          if (!waiting || waiting.expires_unix <= nowUnix() || !sameSecret(waiting.code, given)) {
            return fail(403, 'That code is wrong or has expired. Ask for a new one.');
          }

          verified = true;
        }

        const secret = randomBytes(TOTP_BYTES);
        const encoded = base32(secret);

        await env.prism_accounts.batch([
          env.prism_accounts
            .prepare(
              'INSERT INTO accounts (email, salt, verifier, totp_secret, sealed_key, verified,' +
                ' relay_allowed, created_unix) VALUES (?, ?, ?, ?, ?, ?, 1, ?)',
            )
            .bind(
              email,
              hex(salt),
              await verifierOf(auth),
              encoded,
              sent?.sealed_key ?? '',
              verified ? 1 : 0,
              nowUnix(),
            ),
          env.prism_accounts.prepare('DELETE FROM challenges WHERE email = ?').bind(email),
        ]);

        return json({ totp_uri: totpUri(email, encoded), totp_secret: encoded });
      }

      if (path === '/v1/sessions' && method === 'POST') {
        const sent = await body<{ email?: string; auth?: string; code?: number | string }>();
        const email = (sent?.email ?? '').trim().toLowerCase();
        const auth = unhex(sent?.auth ?? '');

        if (!auth || auth.length !== SECRET_BYTES) {
          return malformed('auth');
        }

        const account = await env.prism_accounts
          .prepare('SELECT verifier, totp_secret, verified FROM accounts WHERE email = ?')
          .bind(email)
          .first<{ verifier: string; totp_secret: string; verified: number }>();

        // Every wrong answer is the same answer. One that said which of the three was wrong
        // would be an answer worth guessing against.
        if (!account || !sameSecret(account.verifier, await verifierOf(auth))) {
          return fail(401, REFUSED);
        }

        if (account.verified !== 1) {
          return fail(403, 'Open the link sent to that address, then sign in.');
        }

        const secret = unbase32(account.totp_secret);
        const code = String(sent?.code ?? '').padStart(6, '0');

        if (!secret || !(await totpMatches(secret, code, nowUnix()))) {
          return fail(401, REFUSED);
        }

        const token = hex(randomBytes(TOKEN_BYTES));

        await sweep(env);
        await env.prism_accounts
          .prepare('INSERT INTO sessions (token_hash, email, expires_unix) VALUES (?, ?, ?)')
          .bind(hex(await sha256(token)), email, nowUnix() + SESSION_SECONDS)
          .run();

        return json({ token, ...(await describe(env, email)) });
      }

      // Everything below needs a session.
      const signedIn = await whose(env, request);

      if (path === '/v1/session' && method === 'GET') {
        if (!signedIn) {
          return fail(401, REFUSED);
        }

        return json({ email: signedIn, ...(await describe(env, signedIn)) });
      }

      if (path === '/v1/session' && method === 'DELETE') {
        const token = request.headers.get('authorization')?.replace(/^Bearer /, '');

        if (token) {
          await env.prism_accounts
            .prepare('DELETE FROM sessions WHERE token_hash = ?')
            .bind(hex(await sha256(token)))
            .run();
        }

        return json({ ok: true });
      }

      if (path === '/v1/devices' && method === 'GET') {
        if (!signedIn) {
          return fail(401, REFUSED);
        }

        return json({ devices: await devicesOf(env, signedIn) });
      }

      // Registering a key the account already has is how a label changes, which is why renaming
      // a machine needs no endpoint of its own and cannot leave one listed twice.
      if (path === '/v1/devices' && method === 'POST') {
        if (!signedIn) {
          return fail(401, REFUSED);
        }

        const sent = await body<{ public_key?: string; label?: string }>();
        const key = (sent?.public_key ?? '').trim().toLowerCase();

        if (unhex(key)?.length !== 32) {
          return malformed('public key');
        }

        await env.prism_accounts
          .prepare(
            'INSERT INTO devices (email, public_key, label, added_unix) VALUES (?, ?, ?, ?) ' +
              'ON CONFLICT(email, public_key) DO UPDATE SET label = excluded.label',
          )
          .bind(signedIn, key, (sent?.label ?? '').slice(0, 120), nowUnix())
          .run();

        return json({ devices: await devicesOf(env, signedIn) });
      }

      if (path.startsWith('/v1/devices/') && method === 'DELETE') {
        if (!signedIn) {
          return fail(401, REFUSED);
        }

        const key = decodeURIComponent(path.slice('/v1/devices/'.length)).toLowerCase();

        await env.prism_accounts
          .prepare('DELETE FROM devices WHERE email = ? AND public_key = ?')
          .bind(signedIn, key)
          .run();

        return json({ devices: await devicesOf(env, signedIn) });
      }

      if (path === '/v1/account/key' && method === 'PUT') {
        if (!signedIn) {
          return fail(401, REFUSED);
        }

        const sent = await body<{ salt?: string; auth?: string; sealed_key?: string }>();
        const salt = unhex(sent?.salt ?? '');
        const auth = unhex(sent?.auth ?? '');

        if (!salt || salt.length !== SALT_BYTES) {
          return malformed('salt');
        }

        if (!auth || auth.length !== SECRET_BYTES) {
          return malformed('auth');
        }

        await env.prism_accounts
          .prepare('UPDATE accounts SET salt = ?, verifier = ?, sealed_key = ? WHERE email = ?')
          .bind(hex(salt), await verifierOf(auth), sent?.sealed_key ?? '', signedIn)
          .run();

        return json({ ok: true });
      }

      // Only when an operator asked for them. Without a token these are not routes at all, so a
      // server nobody configured for administration answers as though they were never written.
      if (env.PRISM_ADMIN_TOKEN && path.startsWith('/v1/admin/')) {
        const given = request.headers.get('authorization')?.replace(/^Bearer /, '') ?? '';

        if (!sameSecret(given, env.PRISM_ADMIN_TOKEN)) {
          return fail(401, REFUSED);
        }

        if (path === '/v1/admin/accounts' && method === 'GET') {
          const { results } = await env.prism_accounts
            .prepare(
              'SELECT a.email, a.verified, a.relay_allowed,' +
                ' (SELECT COUNT(*) FROM devices d WHERE d.email = a.email) AS devices' +
                ' FROM accounts a ORDER BY a.created_unix',
            )
            .all();

          return json({ accounts: results ?? [] });
        }

        if (path.startsWith('/v1/admin/accounts/') && method === 'DELETE') {
          const email = decodeURIComponent(path.slice('/v1/admin/accounts/'.length));
          const gone = await env.prism_accounts
            .prepare('DELETE FROM accounts WHERE email = ?')
            .bind(email)
            .run();

          return json({ deleted: (gone.meta.changes ?? 0) > 0 });
        }
      }

      return fail(404, 'There is nothing at that address.');
    } catch (error) {
      // Nothing about the store reaches the caller. What went wrong here is this server's
      // problem, and a message describing its internals is a message describing its internals
      // to whoever asked.
      console.error(error);

      return fail(503, 'The server could not finish that. Try again in a moment.');
    }
  },
} satisfies ExportedHandler<Env>;
