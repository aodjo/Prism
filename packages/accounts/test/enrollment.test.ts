import { DatabaseSync } from 'node:sqlite';
import fs from 'node:fs';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import worker from '../src/index.js';
import type { Env } from '../src/index.js';
import { unbase32 } from '../src/crypto.js';

vi.mock('../src/dashboard.js', () => ({ APP: '', CSS: '', HTML: '', THEME: '', UI: '', WASM: '' }));

/** Generates a code as an authenticator would, independently of the API's verifier. */
async function codeFor(secret: string): Promise<string> {
  const counter = new Uint8Array(8);
  new DataView(counter.buffer).setBigUint64(0, BigInt(Math.floor(Date.now() / 30_000)));
  const key = await crypto.subtle.importKey('raw', unbase32(secret)!, { name: 'HMAC', hash: 'SHA-1' }, false, ['sign']);
  const mac = new Uint8Array(await crypto.subtle.sign('HMAC', key, counter));
  const at = mac[mac.length - 1]! & 15;
  const number = new DataView(mac.buffer).getUint32(at) & 0x7fff_ffff;
  return String(number % 1_000_000).padStart(6, '0');
}

describe('interrupted authenticator enrollment', () => {
  let db: DatabaseSync;
  let env: Env;
  const email = 'enrollment@example.invalid';
  const auth = '11'.repeat(32);
  const salt = '22'.repeat(16);

  beforeEach(() => {
    db = new DatabaseSync(':memory:');
    db.exec(fs.readFileSync(new URL('../schema.sql', import.meta.url), 'utf8'));
    const database = {
      /** Adapts SQLite statements to the D1 methods used by the real Worker. */
      prepare(sql: string) {
        let values: any[] = [];
        const query = {
          bind(...args: any[]) { values = args; return query; },
          async first() { return db.prepare(sql).get(...values) ?? null; },
          async all() { return { results: db.prepare(sql).all(...values) }; },
          async run() { return { meta: { changes: db.prepare(sql).run(...values).changes } }; },
        };
        return query;
      },
      /** Executes each D1 batch atomically, including enrollment promotion. */
      async batch(queries: { run(): Promise<unknown> }[]) {
        db.exec('BEGIN');
        try {
          const result = [];
          for (const query of queries) result.push(await query.run());
          db.exec('COMMIT');
          return result;
        } catch (error) {
          db.exec('ROLLBACK');
          throw error;
        }
      },
    };
    env = { prism_accounts: database, PRISM_RESEND_KEY: 'test-only', PRISM_MAIL_FROM: 'test@example.invalid' } as unknown as Env;
    vi.stubGlobal('fetch', vi.fn(async () => new Response('{}')));
  });

  afterEach(() => { db.close(); vi.unstubAllGlobals(); });

  /** Calls the Worker directly; no deployed accounts or mail providers are reached. */
  async function post(path: string, body: unknown): Promise<Response> {
    return worker.fetch(new Request(`https://test.invalid${path}`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
    }), env);
  }

  /** Proves a fresh email challenge and returns the unfinished authenticator setup. */
  async function enroll() {
    const challenge = await post('/v1/accounts/challenge', { email });
    expect(challenge.status).toBe(200);
    const waiting = db.prepare('SELECT code FROM challenges WHERE email = ?').get(email)!;
    const response = await post('/v1/accounts', { email, code: waiting.code, salt, auth });
    expect(response.status).toBe(200);
    return response.json() as Promise<{ totp_secret: string }>;
  }

  it('allows signup to restart after the QR screen is lost', async () => {
    const first = await enroll();
    expect(db.prepare('SELECT COUNT(*) AS n FROM accounts').get()!.n).toBe(0);
    const second = await enroll();
    expect(second.totp_secret).not.toBe(first.totp_secret);
    expect(db.prepare('SELECT COUNT(*) AS n FROM pending_accounts').get()!.n).toBe(1);
    const response = await worker.fetch(new Request(`https://test.invalid/v1/salt?email=${email}`), env);
    expect(await response.json()).toEqual({ salt });
  });

  it('activates only after both the password and authenticator are proved', async () => {
    const enrollment = await enroll();
    const code = await codeFor(enrollment.totp_secret);
    expect((await post('/v1/sessions', { email, auth: '33'.repeat(32), code })).status).toBe(401);
    expect((await post('/v1/sessions', { email, auth, code: 'invalid' })).status).toBe(401);
    expect(db.prepare('SELECT COUNT(*) AS n FROM accounts').get()!.n).toBe(0);
    const signedIn = await post('/v1/sessions', { email, auth, code });
    expect(signedIn.status).toBe(200);
    expect((await signedIn.json() as { token: string }).token).toBeTruthy();
    expect(db.prepare('SELECT COUNT(*) AS n FROM pending_accounts').get()!.n).toBe(0);
    expect(db.prepare('SELECT COUNT(*) AS n FROM accounts').get()!.n).toBe(1);
    expect((await post('/v1/accounts/challenge', { email })).status).toBe(409);
    expect((await post('/v1/sessions', { email, auth, code })).status).toBe(200);
  });

  it('refuses an expired enrollment and permits starting it again', async () => {
    const enrollment = await enroll();
    db.prepare('UPDATE pending_accounts SET expires_unix = 0').run();
    const code = await codeFor(enrollment.totp_secret);
    expect((await post('/v1/sessions', { email, auth, code })).status).toBe(401);
    expect(db.prepare('SELECT COUNT(*) AS n FROM sessions').get()!.n).toBe(0);
    await enroll();
  });

  it('does not activate an enrollment replaced while its code is checked', async () => {
    const enrollment = await enroll();
    const code = await codeFor(enrollment.totp_secret);
    const batch = env.prism_accounts.batch.bind(env.prism_accounts);
    vi.spyOn(env.prism_accounts, 'batch').mockImplementationOnce(async (queries) => {
      db.prepare('UPDATE pending_accounts SET totp_secret = ?').run('AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA');
      return batch(queries);
    });
    expect((await post('/v1/sessions', { email, auth, code })).status).toBe(401);
    expect(db.prepare('SELECT COUNT(*) AS n FROM accounts').get()!.n).toBe(0);
    expect(db.prepare('SELECT COUNT(*) AS n FROM sessions').get()!.n).toBe(0);
  });
});
