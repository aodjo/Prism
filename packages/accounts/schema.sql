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
