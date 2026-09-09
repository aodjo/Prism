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

// The dashboard, served from this Worker rather than deployed beside it: an operator's tool
// that ships separately is one that drifts from the thing it operates, and the page and the
// API would then be able to disagree about what an account is.
import { APP, CSS, HTML, UI, WASM } from './dashboard.js';
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
  /**
   * What a signalling region proves itself with when it reports.
   *
   * Without it no report is accepted at all. An open ingest would let anybody write whatever
   * numbers they liked into an operator's dashboard — and a dashboard that can be lied to is
   * worse than one that says nothing, because somebody acts on it.
   */
  readonly PRISM_REPORT_TOKEN?: string;
}

/** What this server says it is when asked. Bumped when the API it serves changes. */
const VERSION = '1.0.0';

/**
 * How long a session lasts.
 *
 * Thirty days. A machine that streams every day should not be asked for a password and a code
 * twice a week; what makes that safe is that a session can be ended from the dashboard at any
 * moment, which is faster than any expiry would be.
 */
const SESSION_SECONDS = 30 * 24 * 60 * 60;

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

/**
 * What the dashboard is allowed to do in a browser.
 *
 * `'self'` and nothing else: the page loads its own files, talks to its own origin, and can
 * reach nowhere else. `'wasm-unsafe-eval'` is what lets it compile `argon2.wasm`, which is the
 * whole reason a password can be checked without being sent here.
 *
 * Styles are the one exception. A marker's position on the map and a bar's width are computed
 * from what the server said, so they can only be set as attributes; `'unsafe-inline'` there
 * permits that and nothing else — it does not let a script run. Every string the page shows is
 * written with `textContent`, so there is no injection point for it to matter at.
 */
const DASHBOARD_POLICY =
  "default-src 'none'; script-src 'self' 'wasm-unsafe-eval';" +
  " style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:;" +
  " form-action 'none'; base-uri 'none'; frame-ancestors 'none'";

/**
 * The dashboard's files, by the path a browser asks for them at.
 *
 * The page answers at `/` as well as at `/admin`, so that `admin.presm.kr` opens it and
 * `accounts.presm.kr/admin` still does. Both names reach this same Worker, which is what keeps
 * the page and the API it calls on one origin — a second deployment would need CORS, and the
 * policy above would have to name a host it is allowed to talk to.
 *
 * Nothing collides: every API route is under `/v1/`.
 */
const DASHBOARD: Record<string, { body: string | ArrayBuffer; type: string }> = {
  '/': { body: HTML, type: 'text/html; charset=utf-8' },
  '/admin': { body: HTML, type: 'text/html; charset=utf-8' },
  '/admin/': { body: HTML, type: 'text/html; charset=utf-8' },
  '/admin/style.css': { body: CSS, type: 'text/css; charset=utf-8' },
  '/admin/app.js': { body: APP, type: 'text/javascript; charset=utf-8' },
  '/admin/ui.js': { body: UI, type: 'text/javascript; charset=utf-8' },
  '/admin/argon2.wasm': { body: WASM, type: 'application/wasm' },
};

/**
 * Serves one of the dashboard's files.
 *
 * @param {string} path - What was asked for.
 * @returns {Response} The file, or a refusal.
 */
function dashboard(path: string): Response {
  const file = DASHBOARD[path];

  if (!file) {
    return fail(404, 'There is nothing at that address.');
  }

  return new Response(file.body, {
    headers: {
      'content-type': file.type,
      'content-security-policy': DASHBOARD_POLICY,
      'referrer-policy': 'no-referrer',
      'x-content-type-options': 'nosniff',
      // The page and its code change together, so a browser holding yesterday's script against
      // today's API is a browser calling routes that have moved.
      'cache-control': 'no-cache',
    },
  });
}

/** Where the builds live. Public, so nothing here needs a credential to read it. */
const RELEASES = 'https://api.github.com/repos/aodjo/Prism/releases';

/**
 * Answers an installed application asking whether it is current.
 *
 * Tauri's updater asks a URL carrying its platform and the version it is running, and reads
 * back either a manifest naming a newer one or a 204 meaning there is nothing. It checks the
 * signature in that manifest against a public key compiled into the binary, so this endpoint
 * cannot make a machine install anything — the worst it can do is point at the wrong file, and
 * the wrong file will not verify.
 *
 * Which line to answer from comes from a header rather than the path, because the path is a
 * template the bundler fills in at build time: an application that carried the channel in its
 * URL could only ever ask about the line it was built on, and moving between them is a setting.
 *
 * @param {Env} env - The runtime.
 * @param {string} path - `/v1/update/{target}/{arch}/{version}`.
 * @param {string} channel - `production` or `development`.
 * @returns {Promise<Response>} A manifest, or 204 when the running version is current.
 */
async function update(env: Env, path: string, channel: string): Promise<Response> {
  const [target = '', arch = '', running = ''] = path.slice('/v1/update/'.length).split('/');

  if (!target || !arch || !running) {
    return malformed('update path');
  }

  const answer = await fetch(`${RELEASES}?per_page=30`, {
    headers: { 'user-agent': 'prism-accounts', accept: 'application/vnd.github+json' },
    // Releases change when one is cut and not otherwise, so asking GitHub on every launch of
    // every machine would be spending somebody's rate limit on an answer that did not move.
    cf: { cacheTtl: 300, cacheEverything: true },
  }).catch(() => null);

  if (!answer?.ok) {
    return new Response(null, { status: 204 });
  }

  const releases = (await answer.json().catch(() => [])) as {
    tag_name: string;
    body: string | null;
    published_at: string;
    draft: boolean;
    prerelease: boolean;
    assets: { name: string; browser_download_url: string }[];
  }[];

  // A development build is a prerelease of the version being worked toward, so the two lines
  // are the same list read with different eyes: production takes releases, development takes
  // everything and lets the ordering below decide.
  const wanted = releases.filter(
    (release) => !release.draft && (channel === 'development' || !release.prerelease),
  );

  const newest = wanted
    .map((release) => ({ release, version: release.tag_name.replace(/^v/u, '') }))
    .sort((left, right) => compareVersions(right.version, left.version))[0];

  if (!newest || compareVersions(newest.version, running) <= 0) {
    return new Response(null, { status: 204 });
  }

  const found = assetFor(newest.release.assets, target, arch);

  if (!found) {
    return new Response(null, { status: 204 });
  }

  // The updater wants the signature itself, not somewhere to get it: it base64-decodes whatever
  // this field holds and checks the download against it. A link here parses as a signature that
  // does not verify, which is an update that downloads in full and then refuses to install —
  // and says so nowhere a person can see.
  const signature = await signatureOf(found.signatureUrl);

  if (!signature) {
    return new Response(null, { status: 204 });
  }

  return json({
    version: newest.version,
    notes: newest.release.body ?? '',
    pub_date: newest.release.published_at,
    url: found.url,
    signature,
  });
}

/**
 * Fetches the contents of a signature file.
 *
 * A few hundred bytes, cached for as long as the release list is, because the two change
 * together: a signature belongs to one file in one release and neither is ever rewritten.
 *
 * @async
 * @param {string} url - Where the `.sig` file is.
 * @returns {Promise<string | null>} What it holds, or null if it could not be read.
 */
async function signatureOf(url: string): Promise<string | null> {
  const answer = await fetch(url, {
    headers: { 'user-agent': 'prism-accounts' },
    cf: { cacheTtl: 300, cacheEverything: true },
  }).catch(() => null);

  if (!answer?.ok) {
    return null;
  }

  const signature = (await answer.text().catch(() => '')).trim();

  return signature.length > 0 ? signature : null;
}

/**
 * What the updater calls a platform, and what the release files call it.
 *
 * Tauri asks with the names Rust uses for a target — `darwin`, `aarch64` — and the files are
 * named for what somebody downloading one would call their machine. The two have to be mapped
 * somewhere and this is the only place that sees both.
 */
const PLATFORMS: Record<string, string> = {
  darwin: 'macos',
  windows: 'windows',
  linux: 'linux',
  aarch64: 'arm64',
  x86_64: 'x64',
};

/**
 * Which extension carries an installable update on each platform.
 *
 * Not always the one a person downloads: a `.dmg` is for double-clicking and cannot be applied
 * to a running application, so macOS updates from the archive beside it. Windows and Linux do
 * update from the file somebody would have downloaded, because Tauri v2 signs the installer and
 * the AppImage themselves rather than wrapping them — there is no `.nsis.zip` and no
 * `.AppImage.tar.gz` to look for, and asking for one is how this answered nothing for two
 * platforms while insisting the release was fine.
 */
const UPDATABLE: Record<string, string> = {
  darwin: '.app.tar.gz',
  windows: '.exe',
  linux: '.AppImage',
};

/**
 * Finds the bundle and its signature for one platform in a release's assets.
 *
 * A release missing the pair for a platform answers nothing for it rather than pointing at
 * something else: an unsigned or mismatched file would be refused by the application anyway,
 * and refusing it here says so without spending somebody's bandwidth first.
 *
 * @param {{name: string, browser_download_url: string}[]} assets - What the release carries.
 * @param {string} target - `darwin`, `windows` or `linux`.
 * @param {string} arch - `aarch64` or `x86_64`.
 * @returns {{url: string, signatureUrl: string} | null} Where each of the two is, or null.
 */
function assetFor(
  assets: { name: string; browser_download_url: string }[],
  target: string,
  arch: string,
): { url: string; signatureUrl: string } | null {
  const os = PLATFORMS[target];
  const machine = PLATFORMS[arch];
  const extension = UPDATABLE[target];

  if (!os || !machine || !extension) {
    return null;
  }

  const prefix = `prism-${os}-${machine}-`;
  const wanted = assets.find(
    (asset) => asset.name.startsWith(prefix) && asset.name.endsWith(extension),
  );

  if (!wanted) {
    return null;
  }

  const signature = assets.find((asset) => asset.name === `${wanted.name}.sig`);

  return signature
    ? { url: wanted.browser_download_url, signatureUrl: signature.browser_download_url }
    : null;
}

/**
 * Orders two versions the way semver does.
 *
 * Written out rather than pulled in, because the whole of what is needed is here: three numbers
 * and a prerelease tag, where having one makes a version *lower* than the same version without.
 * That last rule is the one the release line depends on — `1.1.0` supersedes `1.1.0-dev.847` —
 * and it is the one a naive string comparison gets backwards.
 *
 * @param {string} left - A version, without a leading `v`.
 * @param {string} right - The other.
 * @returns {number} Positive when `left` is newer, negative when older, zero when the same.
 */
function compareVersions(left: string, right: string): number {
  const split = (version: string) => {
    const [core = '', pre = ''] = version.split('-');

    return { numbers: core.split('.').map(Number), pre };
  };

  const a = split(left);
  const b = split(right);

  for (let at = 0; at < 3; at += 1) {
    const difference = (a.numbers[at] ?? 0) - (b.numbers[at] ?? 0);

    if (difference !== 0) {
      return difference;
    }
  }

  if (a.pre === b.pre) {
    return 0;
  }

  // A release beats every prerelease of itself.
  if (!a.pre) {
    return 1;
  }

  if (!b.pre) {
    return -1;
  }

  // Both are prereleases of the same version: `dev.848` against `dev.847`. Compared by their
  // numeric tail so that ten sorts after nine.
  const tail = (pre: string) => Number(pre.split('.').pop() ?? 0);

  return tail(a.pre) - tail(b.pre);
}

/**
 * How long a region's last report stays believable.
 *
 * A region reports every minute, so this is four missed ones. Long enough that a restart or a
 * lost packet does not paint a healthy region red, short enough that a machine which has gone
 * away stops being presented as though its numbers still meant something.
 */
const STALE_SECONDS = 240;

/**
 * What a region's address has to look like.
 *
 * `host:port`, because that is what a region *is*: one UDP port that clients send to. It used
 * to be an `https://` origin, from when the dashboard fetched each region for its numbers —
 * regions push now, so nothing here ever opens it, and requiring a scheme would be asking an
 * operator to invent one.
 */
const ADDRESS = /^\[?[A-Za-z0-9.\-:]+\]?:\d{1,5}$/u;

/**
 * Compares two strings without letting how long it takes say how much of one is right.
 *
 * The report token is a shared secret, and a comparison that stops at the first wrong character
 * tells anybody who can time it where that character was. Both are read to the end, and the
 * length is checked by the caller before this is reached.
 */
function timingSafeEqual(a: string, b: string): boolean {
  let difference = a.length ^ b.length;

  for (let at = 0; at < a.length; at += 1) {
    difference |= a.charCodeAt(at) ^ b.charCodeAt(at % b.length);
  }

  return difference === 0;
}

/** What one signalling server was found to be doing. */
interface Region {
  /** What an operator calls it, which is a city rather than a hostname. */
  name: string;
  /** Where to ask it how it is. */
  url: string;
  /** Gigabytes its plan allows each month, or `null` when the plan does not meter traffic. */
  limit_gb: number | null;
  /**
   * Whether it has reported recently enough to be believed.
   *
   * A region sends every minute, so silence for several minutes is a region that is down, gone,
   * or unable to reach here — which are different problems with the same symptom and the same
   * answer for an operator: stop trusting these numbers.
   */
  up: boolean;
  /** How many seconds ago it last reported, or `null` if it never has. */
  heard_seconds: number | null;
  /**
   * Whether that server has ever told us what it is doing.
   *
   * False against a region running a build from before reporting existed, or one that has never
   * been given a token. The dashboard shows a dash rather than a zero for everything below,
   * because "nothing is happening" and "nobody has heard from the machine that knows" are
   * different answers.
   */
  reports: boolean;
  /** Hosts registered and reachable there. */
  hosts: number;
  /** Relayed sessions it is carrying. */
  carrying: number;
  /** What those sessions are costing its link right now. */
  now_mbps: number;
  /** The most they have cost it since it started. */
  peak_mbps: number;
  /** What its link will carry. */
  link_mbps: number;
  /** Bytes it has relayed since it started. */
  carried_bytes: number;
  /** What it says it is running. */
  build: string;
  /**
   * How long since it last restarted.
   *
   * Every total above covers exactly this window. A region holds no state, so there is nowhere
   * for a monthly figure to live and nothing here pretends there is.
   */
  uptime_seconds: number;
}

/**
 * Reads what every configured signalling region last said about itself.
 *
 * One query. Regions push their own numbers here on a timer, so opening this page costs a
 * database read rather than five round trips to machines scattered around the world — and a
 * region that is down slows nothing, because nothing waits on it.
 *
 * A region listed with no report is a region that has never been heard from. That is a real
 * state and it is shown as one: it means the machine is running a build from before reporting
 * existed, or was never given a token, not that it is idle.
 */
async function regions(env: Env): Promise<Region[]> {
  const { results } = await env.prism_accounts
    .prepare(
      'SELECT r.name, r.url, r.limit_gb, s.at_unix, s.build, s.uptime_seconds, s.hosts,' +
        ' s.carrying, s.now_mbps, s.peak_mbps, s.link_mbps, s.carried_bytes' +
        ' FROM regions r LEFT JOIN region_reports s ON s.region = r.name' +
        ' ORDER BY r.name',
    )
    .all<{
      name: string;
      url: string;
      limit_gb: number | null;
      at_unix: number | null;
      build: string | null;
      uptime_seconds: number | null;
      hosts: number | null;
      carrying: number | null;
      now_mbps: number | null;
      peak_mbps: number | null;
      link_mbps: number | null;
      carried_bytes: number | null;
    }>();

  const now = nowUnix();

  return (results ?? []).map((row) => {
    const heard = row.at_unix === null ? null : Math.max(0, now - row.at_unix);

    return {
      name: row.name,
      url: row.url,
      limit_gb: row.limit_gb,
      up: heard !== null && heard <= STALE_SECONDS,
      heard_seconds: heard,
      reports: row.at_unix !== null,
      hosts: row.hosts ?? 0,
      carrying: row.carrying ?? 0,
      now_mbps: row.now_mbps ?? 0,
      peak_mbps: row.peak_mbps ?? 0,
      link_mbps: row.link_mbps ?? 0,
      carried_bytes: row.carried_bytes ?? 0,
      build: row.build ?? '',
      uptime_seconds: row.uptime_seconds ?? 0,
    };
  });
}

/**
 * Takes one region's report.
 *
 * Replaces whatever that region said last. Nothing here is history: a report is the current
 * state of a machine that keeps none of its own, and the moment a newer one arrives the older
 * is of no use to anybody.
 *
 * The timestamp is this server's, not the region's. A region with a wrong clock would otherwise
 * be able to make itself look permanently fresh, or permanently stale.
 *
 * @async
 * @param {Env} env - The runtime.
 * @param {Request} request - The report, as JSON, with a bearer token.
 * @returns {Promise<Response>} 204 when it was taken.
 */
async function takeReport(env: Env, request: Request): Promise<Response> {
  const expected = (env.PRISM_REPORT_TOKEN ?? '').trim();

  // No token configured means no reports accepted. Refusing is the safe direction: an open
  // ingest lets anybody write numbers an operator will act on.
  if (expected.length === 0) {
    return new Response(null, { status: 404 });
  }

  const offered = (request.headers.get('authorization') ?? '').replace(/^Bearer /iu, '');

  if (offered.length !== expected.length || !timingSafeEqual(offered, expected)) {
    return new Response(null, { status: 401 });
  }

  const said = (await request.json().catch(() => null)) as Record<string, unknown> | null;
  const region = String(said?.region ?? '').trim();

  if (!said || region.length === 0) {
    return malformed('region');
  }

  await env.prism_accounts
    .prepare(
      'INSERT INTO region_reports (region, at_unix, build, uptime_seconds, hosts, carrying,' +
        ' waiting, now_mbps, peak_mbps, link_mbps, carried_bytes, sessions, introduced)' +
        ' VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)' +
        ' ON CONFLICT(region) DO UPDATE SET at_unix = excluded.at_unix, build = excluded.build,' +
        ' uptime_seconds = excluded.uptime_seconds, hosts = excluded.hosts,' +
        ' carrying = excluded.carrying, waiting = excluded.waiting,' +
        ' now_mbps = excluded.now_mbps, peak_mbps = excluded.peak_mbps,' +
        ' link_mbps = excluded.link_mbps, carried_bytes = excluded.carried_bytes,' +
        ' sessions = excluded.sessions, introduced = excluded.introduced',
    )
    .bind(
      region,
      nowUnix(),
      String(said.build ?? ''),
      Number(said.uptime_seconds ?? 0),
      Number(said.hosts ?? 0),
      Number(said.carrying ?? 0),
      Number(said.waiting ?? 0),
      Number(said.now_mbps ?? 0),
      Number(said.peak_mbps ?? 0),
      Number(said.link_mbps ?? 0),
      Number(said.carried_bytes ?? 0),
      JSON.stringify(said.sessions ?? []),
      JSON.stringify(said.introduced ?? []),
    )
    .run();

  return new Response(null, { status: 204 });
}

/**
 * What the signalling servers are carrying, and what they only introduced.
 *
 * Two different kinds of knowledge, kept apart because they are not equally true. A relayed
 * session passes through a rendezvous, so that server counts its bytes and knows the moment it
 * stops. An introduction ends when the address is handed over: the two machines then talk
 * directly and this server never hears from them again, so it can date the introduction and
 * nothing else.
 *
 * Machines are named by public key on the wire and by label here, joined through the `devices`
 * table. A key nobody registered is shown as itself, shortened.
 *
 * @param {Env} env - The runtime.
 * @param {string|null} email - One account's activity, or `null` for every account's.
 * @returns {Promise<object>} `reports` is false when no region can answer this yet.
 */
async function activityFor(env: Env, email: string | null): Promise<Record<string, unknown>> {
  const reported = await env.prism_accounts
    .prepare('SELECT region, sessions, introduced FROM region_reports')
    .all<{ region: string; sessions: string; introduced: string }>();

  const found = reported.results ?? [];

  if (found.length === 0) {
    return { reports: false, carrying: [], introduced: [] };
  }

  const { results } = await env.prism_accounts
    .prepare('SELECT public_key, label, email FROM devices')
    .all<{ public_key: string; label: string; email: string }>();
  const machines = new Map((results ?? []).map((row) => [row.public_key, row]));

  /**
   * Turns a public key into what an operator calls that machine.
   *
   * @param {string} key - As hex.
   * @returns {{label: string, email: string}} The machine, or a shortened key.
   */
  const machine = (key: string) =>
    machines.get(key) ?? { label: `${key.slice(0, 4)}…${key.slice(-4)}`, email: '' };

  const carrying: Record<string, unknown>[] = [];
  const introduced: Record<string, unknown>[] = [];

  for (const row of found) {
    for (const entry of parseList(row.sessions)) {
      const host = machine(String(entry.host));
      const client = machine(String(entry.client));

      if (email && host.email !== email && client.email !== email) {
        continue;
      }

      carrying.push({
        email: host.email || client.email,
        from: host.label,
        to: client.label,
        region: row.region,
        since_unix: entry.since_unix,
        bytes: entry.bytes,
        token: entry.token,
      });
    }

    for (const entry of parseList(row.introduced)) {
      const host = machine(String(entry.host));
      const client = machine(String(entry.client));

      if (email && host.email !== email && client.email !== email) {
        continue;
      }

      introduced.push({
        email: host.email || client.email,
        from: host.label,
        to: client.label,
        region: row.region,
        at_unix: entry.at_unix,
        relayed: Boolean(entry.relayed),
      });
    }
  }

  introduced.sort((left, right) => Number(right.at_unix) - Number(left.at_unix));

  return { reports: true, carrying, introduced: introduced.slice(0, 50) };
}

/**
 * Reads one of a report's JSON columns back into a list.
 *
 * These columns are written by this server from what a region sent, so a body that will not
 * parse means the row was written by something else. An empty list rather than a throw: one
 * region's bad row must not empty the page for every other region.
 *
 * @param {string} stored - The column's contents.
 * @returns {Record<string, string|number>[]} What it held, or nothing.
 */
function parseList(stored: string): Record<string, string | number>[] {
  try {
    const read = JSON.parse(stored) as unknown;

    return Array.isArray(read) ? (read as Record<string, string | number>[]) : [];
  } catch {
    return [];
  }
}

/**
 * Writes down that an operator did something.
 *
 * Never fails a request: a change that happened but was not recorded is better than a change
 * refused because the recording failed, and the change is the thing somebody asked for.
 *
 * @param {Env} env - The runtime.
 * @param {string} actor - The account that did it, or the empty string for the server itself.
 * @param {string} action - A stable token; the dashboard turns it into a sentence.
 * @param {string} subject - What it was done to.
 * @param {string} [detail] - Anything else worth keeping.
 * @returns {Promise<void>}
 */
async function record(
  env: Env,
  actor: string,
  action: string,
  subject: string,
  detail = '',
): Promise<void> {
  await env.prism_accounts
    .prepare(
      'INSERT INTO audit (actor, action, subject, detail, at_unix) VALUES (?, ?, ?, ?, ?)',
    )
    .bind(actor, action, subject, detail, nowUnix())
    .run()
    .catch(() => undefined);
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

/**
 * Returns the account behind a bearer token if it may look after this server, else `null`.
 *
 * The same token a machine streams with. There is no second credential to issue, remember or
 * lose, and no way to hold this right without also holding an account somebody can name, take
 * away, and see in the list.
 */
async function operator(env: Env, request: Request): Promise<string | null> {
  const email = await whose(env, request);

  if (!email) {
    return null;
  }

  const row = await env.prism_accounts
    .prepare('SELECT operator FROM accounts WHERE email = ?')
    .bind(email)
    .first<{ operator: number }>();

  return row?.operator === 1 ? email : null;
}

/**
 * How many accounts may still look after this server once `email` stops.
 *
 * Guards the two actions that can leave nobody able to open the dashboard. A server in that
 * state is not broken — every account still streams — but it can only be given an operator
 * back from the machine that owns the database.
 */
async function operatorsBesides(env: Env, email: string): Promise<number> {
  const row = await env.prism_accounts
    .prepare('SELECT COUNT(*) AS n FROM accounts WHERE operator = 1 AND email <> ?')
    .bind(email)
    .first<{ n: number }>();

  return row?.n ?? 0;
}

/** What signing in and resuming both answer with. */
async function describe(env: Env, email: string): Promise<Record<string, unknown>> {
  const account = await env.prism_accounts
    .prepare('SELECT sealed_key, operator FROM accounts WHERE email = ?')
    .bind(email)
    .first<{ sealed_key: string; operator: number }>();

  return {
    sealed_key: account?.sealed_key ?? '',
    devices: await devicesOf(env, email),
    // Every account may be relayed. Kept in the answer because machines already running read
    // it, and a field that vanishes is a machine that thinks it has been refused.
    relay_allowed: true,
    // Said on every sign-in so the dashboard knows on its first answer whether to draw itself
    // or to say plainly that this account does not open it.
    operator: account?.operator === 1,
    rendezvous: env.PRISM_ADVERTISE ?? '',
  };
}

/**
 * A confirmation the mail provider would not send.
 *
 * Told apart from anything else that can go wrong here because it is the one failure a person
 * can act on — a different address, or an operator who has to fix their own credentials — and
 * the one that must not be reported as "try again in a moment", which it will not survive.
 */
class MailRefused extends Error {}

/**
 * Sends a signup code, if this server has anything to send it with.
 *
 * @returns {Promise<boolean>} Whether one was sent and has to be typed back in.
 * @throws {MailRefused} If the provider refused it.
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
    // What the provider said, not just that it said no. This reaches whoever is registering,
    // and the outcome comes first because that is the part that changes what they do next:
    // nothing here is about them — it is this server's own mail credentials, or an address the
    // provider will not deliver to — and they deserve to know no account was made rather than
    // wondering whether to try a different address.
    const said = await sent.text().catch(() => '');

    throw new MailRefused(
      `No account was made: the confirmation could not be sent. ${sent.status} ${said}`.trim(),
    );
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
          version: VERSION,
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

      // Everything below this line needs an account the server has marked as an operator. The
      // credential is the session that account already has, so there is no second secret to
      // issue, and withdrawing the right is a column on one row rather than a rotation
      // everybody who was ever told the old value has to be walked through.
      if (path.startsWith('/v1/admin/')) {
        const acting = await operator(env, request);

        if (!acting) {
          // Told apart from a wrong password, because the sign-in worked: the answer says the
          // account is fine and this page is not its business, which is what the dashboard
          // shows instead of an error.
          return signedIn
            ? fail(403, 'That account does not look after this server.')
            : fail(401, REFUSED);
        }

        if (path === '/v1/admin/accounts' && method === 'GET') {
          const { results } = await env.prism_accounts
            .prepare(
              'SELECT a.email, a.verified, a.operator, a.created_unix,' +
                ' (SELECT COUNT(*) FROM devices d WHERE d.email = a.email) AS devices,' +
                // When the newest session was opened, which is the last time the password was
                // typed. Worked out here rather than in the page, so that changing how long a
                // session lasts does not silently change what the dashboard says happened.
                ' (SELECT MAX(s.expires_unix) - ? FROM sessions s WHERE s.email = a.email)' +
                '   AS last_seen' +
                ' FROM accounts a ORDER BY a.created_unix DESC',
            )
            .bind(SESSION_SECONDS)
            .all();

          return json({ accounts: results ?? [], you: acting });
        }

        // What the strip along the top is: the few numbers an operator would otherwise open a
        // terminal for. Every one of them is measured here rather than remembered, so a stale
        // answer is not possible — only a slow one.
        if (path === '/v1/admin/overview' && method === 'GET') {
          const started = Date.now();
          const counts = await env.prism_accounts
            .prepare(
              'SELECT (SELECT COUNT(*) FROM accounts) AS accounts,' +
                ' (SELECT COUNT(*) FROM accounts WHERE verified = 0) AS unverified,' +
                ' (SELECT COUNT(*) FROM devices) AS devices,' +
                ' (SELECT COUNT(*) FROM sessions WHERE expires_unix > ?) AS sessions',
            )
            .bind(nowUnix())
            .first<Record<string, number>>();

          return json({
            version: VERSION,
            database: { latency_ms: Date.now() - started, ...counts },
            regions: await regions(env),
          });
        }

        // One account, everything about it. What this cannot show is how long a session that
        // was merely introduced has lasted: after the address is handed over the two machines
        // talk directly and never pass this way again.
        if (path.startsWith('/v1/admin/accounts/') && method === 'GET') {
          const email = decodeURIComponent(path.slice('/v1/admin/accounts/'.length));
          const account = await env.prism_accounts
            .prepare('SELECT email, verified, operator, created_unix FROM accounts WHERE email = ?')
            .bind(email)
            .first();

          if (!account) {
            return fail(404, 'There is no account for that address.');
          }

          return json({
            account,
            devices: await devicesOf(env, email),
            activity: await activityFor(env, email),
          });
        }

        if (path.startsWith('/v1/admin/accounts/') && path.includes('/devices/') && method === 'DELETE') {
          const [rest = '', key = ''] = path
            .slice('/v1/admin/accounts/'.length)
            .split('/devices/');
          const email = decodeURIComponent(rest);

          await env.prism_accounts
            .prepare('DELETE FROM devices WHERE email = ? AND public_key = ?')
            .bind(email, key.toLowerCase())
            .run();

          await record(env, acting, 'device.remove', email, key.slice(0, 8));

          return json({ devices: await devicesOf(env, email) });
        }

        if (path.startsWith('/v1/admin/accounts/') && method === 'DELETE') {
          const email = decodeURIComponent(path.slice('/v1/admin/accounts/'.length));

          if ((await operatorsBesides(env, email)) === 0) {
            return fail(409, '마지막 운영자입니다. 다른 계정을 먼저 운영자로 지정하세요.');
          }

          const gone = await env.prism_accounts
            .prepare('DELETE FROM accounts WHERE email = ?')
            .bind(email)
            .run();

          if ((gone.meta.changes ?? 0) > 0) {
            await record(env, acting, 'account.delete', email);
          }

          return json({ deleted: (gone.meta.changes ?? 0) > 0 });
        }

        // Whether an account looks after the server, which is the one thing here that cannot
        // be handed out by anybody who does not already hold it. The relay is not on this list:
        // every account may be relayed, so there is nothing to grant.
        if (path.startsWith('/v1/admin/accounts/') && method === 'PATCH') {
          const email = decodeURIComponent(path.slice('/v1/admin/accounts/'.length));
          const sent = await body<{ operator?: boolean }>();
          const carries = sent?.operator;

          if (typeof carries !== 'boolean') {
            return malformed('operator');
          }

          if (!carries && (await operatorsBesides(env, email)) === 0) {
            return fail(409, '마지막 운영자입니다. 다른 계정을 먼저 운영자로 지정하세요.');
          }

          await env.prism_accounts
            .prepare('UPDATE accounts SET operator = ? WHERE email = ?')
            .bind(carries ? 1 : 0, email)
            .run();

          await record(env, acting, carries ? 'operator.grant' : 'operator.revoke', email);

          return json({ ok: true });
        }

        // The regions this dashboard asks after, and the one number on them an operator sets:
        // how much traffic that machine's plan allows in a month. Some of these are on
        // unmetered links and some are not, and only the person paying knows which.
        if (path === '/v1/admin/regions' && method === 'GET') {
          return json({ regions: await regions(env) });
        }

        if (path === '/v1/admin/regions' && method === 'POST') {
          const sent = await body<{ name?: string; url?: string; limit_gb?: number | null }>();
          const name = (sent?.name ?? '').trim();
          const url = (sent?.url ?? '').trim();

          if (!name || name.length > 60) {
            return malformed('name');
          }

          if (!ADDRESS.test(url)) {
            return malformed('url');
          }

          await env.prism_accounts
            .prepare(
              'INSERT INTO regions (name, url, limit_gb, added_unix) VALUES (?, ?, ?, ?)' +
                ' ON CONFLICT(name) DO UPDATE SET url = excluded.url, limit_gb = excluded.limit_gb',
            )
            .bind(name, url, sent?.limit_gb ?? null, nowUnix())
            .run();

          await record(env, acting, 'region.add', name, url);

          return json({ ok: true });
        }

        if (path.startsWith('/v1/admin/regions/') && method === 'PATCH') {
          const name = decodeURIComponent(path.slice('/v1/admin/regions/'.length));
          const sent = await body<{ url?: string; limit_gb?: number | null }>();
          const url = (sent?.url ?? '').trim();

          if (url && !ADDRESS.test(url)) {
            return malformed('url');
          }

          const changed = await env.prism_accounts
            .prepare('UPDATE regions SET url = COALESCE(?, url), limit_gb = ? WHERE name = ?')
            .bind(url || null, sent?.limit_gb ?? null, name)
            .run();

          if ((changed.meta.changes ?? 0) === 0) {
            return fail(404, 'There is no region by that name.');
          }

          await record(
            env,
            acting,
            'region.change',
            name,
            sent?.limit_gb ? `${sent.limit_gb} GB` : '무제한',
          );

          return json({ ok: true });
        }

        if (path.startsWith('/v1/admin/regions/') && method === 'DELETE') {
          const name = decodeURIComponent(path.slice('/v1/admin/regions/'.length));

          await env.prism_accounts.prepare('DELETE FROM regions WHERE name = ?').bind(name).run();
          await record(env, acting, 'region.remove', name);

          return json({ ok: true });
        }

        if (path === '/v1/admin/activity' && method === 'GET') {
          return json(await activityFor(env, null));
        }

        if (path === '/v1/admin/relay' && method === 'GET') {
          const found = await regions(env);
          const reporting = found.filter((region) => region.reports);

          return json({
            reports: reporting.length > 0,
            regions: reporting,
            // Days come from the regions themselves, which do not keep a history yet. Sent as
            // an empty list rather than invented, so the page draws nothing instead of a shape.
            days: [],
            accounts: [],
          });
        }

        if (path === '/v1/admin/audit' && method === 'GET') {
          const { results } = await env.prism_accounts
            .prepare(
              'SELECT actor, action, subject, detail, at_unix FROM audit' +
                ' ORDER BY at_unix DESC LIMIT 200',
            )
            .all();

          return json({ entries: results ?? [] });
        }

        // Signs everybody out, everywhere, including whoever asked. What an operator reaches
        // for when a machine may have been read: it is the only action here that takes effect
        // on machines already running.
        if (path === '/v1/admin/sessions' && method === 'DELETE') {
          const gone = await env.prism_accounts.prepare('DELETE FROM sessions').run();
          await record(env, acting, 'sessions.clear', '', `세션 ${gone.meta.changes ?? 0}개`);

          return json({ signedOut: gone.meta.changes ?? 0 });
        }
      }

      // What an installed application asks when it wonders whether it is current.
      //
      // Open, deliberately. There is nothing private in a version number, every answer is a
      // pointer at a public release, and what makes an update safe to install is the signature
      // on it rather than who was allowed to ask about it. Requiring an account here would
      // mean an application that cannot update until somebody signs in, which is exactly the
      // machine most in need of one.
      if (path.startsWith('/v1/update/') && method === 'GET') {
        return update(env, path, request.headers.get('x-prism-channel') ?? 'production');
      }

      // A signalling region saying what it is doing. It sends; nothing asks it — which is what
      // keeps a region to one open UDP port and no certificate of its own.
      if (path === '/v1/regions/report' && method === 'POST') {
        return takeReport(env, request);
      }

      // The dashboard itself, which is the same API with somewhere to click.
      if (path === '/' || path === '/admin' || path.startsWith('/admin/')) {
        return method === 'GET' ? dashboard(path) : fail(405, 'That is not a method for this.');
      }

      return fail(404, 'There is nothing at that address.');
    } catch (error) {
      if (error instanceof MailRefused) {
        return fail(502, error.message);
      }

      // Nothing about the store reaches the caller. What went wrong here is this server's
      // problem, and a message describing its internals is a message describing its internals
      // to whoever asked.
      console.error(error);

      return fail(503, 'The server could not finish that. Try again in a moment.');
    }
  },
} satisfies ExportedHandler<Env>;
