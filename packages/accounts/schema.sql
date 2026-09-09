-- The account store, as tables rather than as one JSON file rewritten in full.
--
-- What moved here is exactly what `crates/prism-rendezvous/src/accounts.rs` held, in the same
-- shapes and under the same names, so that a store can be carried across without either side
-- learning a new vocabulary. What did not move is the signalling registry: that is soft state
-- in a server's memory, rebuilt by its hosts within fifteen seconds, and it belongs nowhere
-- near a database.
--
-- Nothing here can open a private key. The sealed key is sealed under a secret derived from the
-- password, and that secret is derived on the machine the password was typed on and never sent.

CREATE TABLE IF NOT EXISTS accounts (
  -- The address somebody signs in with, and the only name an account has.
  email        TEXT PRIMARY KEY NOT NULL,
  -- Public by construction: handed out before sign-in, because deriving the secret needs it.
  salt         TEXT NOT NULL,
  -- What recognises a correct authentication secret. A hash of a hash — holding it does not
  -- let anybody sign in, and there is no dictionary to run against an Argon2 output.
  verifier     TEXT NOT NULL,
  -- The second factor's shared secret, in the clear, because a code cannot be checked against
  -- something one-way.
  totp_secret  TEXT NOT NULL,
  -- The private key, sealed under a secret this store has never seen.
  sealed_key   TEXT NOT NULL DEFAULT '',
  -- Whether the address was proved. False on a server with no mail configured, which says so
  -- at startup.
  verified     INTEGER NOT NULL DEFAULT 0,
  -- Whether the relay may be used, which costs bandwidth somebody pays for.
  relay_allowed INTEGER NOT NULL DEFAULT 1,
  -- Whether this account may look after the server: read every account, change what the relay
  -- allows, delete an account, sign everybody out.
  --
  -- A column rather than a shared token, because a token is one secret that opens everything
  -- for everybody who has ever been told it, has no name on it afterwards, and cannot be taken
  -- away from one person without being taken away from all of them. An account already has a
  -- password, a second factor and a name; this says which accounts also carry the server.
  --
  -- Nobody holds it by default. The first one is granted from the machine that owns the
  -- database, with `pnpm --filter @prism/accounts operator <address>`.
  operator      INTEGER NOT NULL DEFAULT 0,
  created_unix INTEGER NOT NULL
);

-- One row per machine on an account. A row rather than a JSON array, so that adding a machine
-- is an insert instead of a rewrite of everything the account knows.
CREATE TABLE IF NOT EXISTS devices (
  email      TEXT NOT NULL REFERENCES accounts(email) ON DELETE CASCADE,
  public_key TEXT NOT NULL,
  label      TEXT NOT NULL,
  added_unix INTEGER NOT NULL,
  PRIMARY KEY (email, public_key)
);

-- What is stored is the SHA-256 of a token and never the token. A session token is a bearer
-- credential — whoever reads one is that account until it expires — so this is enough to
-- recognise one somebody presents and not enough to make one.
CREATE TABLE IF NOT EXISTS sessions (
  token_hash   TEXT PRIMARY KEY NOT NULL,
  email        TEXT NOT NULL REFERENCES accounts(email) ON DELETE CASCADE,
  expires_unix INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_expiry ON sessions (expires_unix);

-- The codes emailed to prove an address is somebody's, and nothing else.
CREATE TABLE IF NOT EXISTS challenges (
  email        TEXT PRIMARY KEY NOT NULL,
  code         TEXT NOT NULL,
  expires_unix INTEGER NOT NULL
);

-- The signalling servers this dashboard asks after.
--
-- A table rather than a setting in the deployment, because one column on it is a number the
-- operator changes: how much traffic that machine's plan allows in a month. A value somebody
-- edits belongs where it can be edited, not in a variable that needs a redeploy to move.
CREATE TABLE IF NOT EXISTS regions (
  -- What an operator calls it, which is a city rather than a hostname.
  name       TEXT PRIMARY KEY NOT NULL,
  -- Where to ask it how it is, as an origin.
  url        TEXT NOT NULL,
  -- Gigabytes the plan allows each month, or NULL when the plan does not meter traffic.
  --
  -- Nullable rather than zero-means-unlimited: the two are opposite conditions and a column
  -- that reads one as the other is a column that eventually silences the wrong alarm.
  limit_gb   INTEGER,
  added_unix INTEGER NOT NULL
);

-- What operators did here, so that an account disappearing has a name and a time against it.
--
-- Written by the server, never by a request, and never deleted from — an audit log an operator
-- can edit is a log that says whatever the last person to hold the account wanted it to say.
CREATE TABLE IF NOT EXISTS audit (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  -- The account that did it, or the empty string when the server did it on nobody's behalf.
  actor      TEXT NOT NULL,
  -- What happened, as a stable token the page turns into a sentence.
  action     TEXT NOT NULL,
  -- What it happened to: an address, a region's name, or empty.
  subject    TEXT NOT NULL,
  -- Anything worth keeping beyond the two above, as JSON.
  detail     TEXT NOT NULL DEFAULT '',
  at_unix    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS audit_recent ON audit (at_unix DESC);

-- One row, holding what the server needs to be the same server across restarts.
--
-- `decoy` is the reason this table exists: an address nobody has registered still has to be
-- given a salt, or asking for one would say which addresses are accounts. The answer has to be
-- the same every time it is asked, so it is derived from a value this store keeps rather than
-- from anything a request carries.
CREATE TABLE IF NOT EXISTS settings (
  id    INTEGER PRIMARY KEY CHECK (id = 1),
  decoy TEXT NOT NULL
);
