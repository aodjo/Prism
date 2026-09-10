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

-- The last thing each signalling region said about itself.
--
-- One row per region, replaced wholesale each time a report arrives. A region pushes this; the
-- dashboard never asks it. That is what keeps adding a region to a machine, a port and an
-- address record — being asked would mean every region answering HTTPS from the internet, with
-- a hostname, a certificate and a proxy each — and it is why this table exists at all rather
-- than the dashboard fanning out to five machines every time somebody opens a page.
--
-- Nothing here is history. A report replaces the one before it, and a region that stops
-- reporting leaves its last one behind with a timestamp saying how stale it is.
CREATE TABLE IF NOT EXISTS region_reports (
  -- The region's name, which has to match the one in `regions`.
  region         TEXT PRIMARY KEY NOT NULL,
  -- When this arrived, by the account server's clock rather than the region's.
  --
  -- The server's own, because a region with a wrong clock would otherwise be able to make
  -- itself look permanently fresh or permanently stale.
  at_unix        INTEGER NOT NULL,
  -- What the region says it is running.
  build          TEXT NOT NULL,
  -- How long since it started. Every total below covers exactly this window.
  uptime_seconds INTEGER NOT NULL,
  -- Hosts registered and reachable there.
  hosts          INTEGER NOT NULL,
  -- Relayed sessions it is carrying.
  carrying       INTEGER NOT NULL,
  -- Relays with one side present, waiting for the other.
  waiting        INTEGER NOT NULL,
  -- What those sessions are costing its link, in megabits per second.
  now_mbps       REAL NOT NULL,
  -- The most they have cost it since it started.
  peak_mbps      REAL NOT NULL,
  -- What its link will carry, as the operator configured the region.
  link_mbps      REAL NOT NULL,
  -- Bytes relayed since it started.
  carried_bytes  INTEGER NOT NULL,
  -- The relays it is carrying, as JSON. Read whole and never queried into, so a column rather
  -- than a table: these rows live and die together with the report they came in.
  sessions       TEXT NOT NULL,
  -- The pairs it introduced lately, as JSON, for the same reason.
  introduced     TEXT NOT NULL
);

-- A development build somebody published without cutting a release.
--
-- The released line is GitHub's: a release is a tag, a set of artifacts and a page to read, and
-- none of that is worth reimplementing here. This table is the line between releases, where the
-- alternative is waiting for a build farm to hand back a bundle that already exists on the
-- machine that made the change.
--
-- One row per platform per version, because a version is published one platform at a time —
-- whoever is fixing a Mac bug has a Mac in front of them and nothing else.
CREATE TABLE IF NOT EXISTS builds (
  -- What the build calls itself, without the leading `v`: `1.0.0-dev.211`.
  version       TEXT NOT NULL,
  -- The target as the updater asks about it: `darwin`, `windows`, `linux`.
  target        TEXT NOT NULL,
  -- The architecture as the updater asks about it: `aarch64`, `x86_64`.
  arch          TEXT NOT NULL,
  -- The minisign signature over the bundle, as the updater wants it: the signature itself,
  -- not somewhere to fetch one. It base64-decodes this field and checks the download against
  -- it, so a link here is an update that downloads in full and then refuses to install.
  signature     TEXT NOT NULL,
  -- What this build is, in one line, shown in the window that offers it.
  notes         TEXT NOT NULL DEFAULT '',
  -- What the file is called: `Prism.app.tar.gz`, `prism-macos-arm64-1.0.0-209.app.tar.gz`.
  --
  -- Kept rather than built back out of the columns beside it. A name assembled from a version
  -- and a platform is a guess that happens to be right, and the moment the bundler renames
  -- something it is a guess that is wrong with nothing saying so.
  filename      TEXT NOT NULL DEFAULT '',
  -- How many bytes the object is, so a listing can be read without opening the bucket.
  bytes         INTEGER NOT NULL,
  -- The installer beside it, where the platform has one that is a different file.
  --
  -- macOS is the only one that does: the updater takes a `.app.tar.gz` and a person takes a
  -- `.dmg`, and they are not the same bytes. On Windows and Linux the installer *is* the thing
  -- the updater fetches, so these stay empty and the row describes one file.
  --
  -- Two columns rather than a second row, because this is not an open-ended set of kinds. A
  -- version on a platform has one artifact the updater uses and at most one a person downloads.
  installer_filename TEXT NOT NULL DEFAULT '',
  installer_bytes    INTEGER NOT NULL DEFAULT 0,
  -- When it was published, by this server's clock.
  uploaded_unix INTEGER NOT NULL,
  -- Which account published it, for the same reason every other change here is attributed.
  published_by  TEXT NOT NULL,
  PRIMARY KEY (version, target, arch)
);

-- The one question asked of this table on every launch of every machine: what is the newest
-- build for this platform. Without it that is a scan, and it grows with every build ever
-- published.
CREATE INDEX IF NOT EXISTS builds_by_platform ON builds (target, arch, uploaded_unix DESC);

-- A request from a terminal to publish, waiting for somebody to say yes in a browser.
--
-- The alternative was asking for an address, a password and a six-digit code at a shell prompt,
-- which puts the one credential that decides what every machine runs into a terminal's history
-- and its scrollback. This asks nothing: the machine that wants to publish shows a code, and
-- whoever is already signed in to the dashboard confirms it is the same code.
--
-- Nothing here is long-lived. A grant is gone the moment it is used, and a grant nobody answers
-- is gone within ten minutes.
CREATE TABLE IF NOT EXISTS publish_grants (
  -- The secret the waiting terminal polls with, stored as its hash. A row read out of the
  -- database is therefore not enough to collect the token it leads to.
  device_hash  TEXT PRIMARY KEY NOT NULL,
  -- What a person reads in both places and checks are the same. Short, because its whole job is
  -- to be compared by eye.
  user_code    TEXT NOT NULL,
  -- What asked, in the words it used: `prism · macOS · aarch64`. Shown so that somebody
  -- approving knows what they are approving.
  asked_for    TEXT NOT NULL DEFAULT '',
  -- When it stops being answerable.
  expires_unix INTEGER NOT NULL,
  -- Who said yes, once somebody has. Empty while it is still waiting.
  email        TEXT NOT NULL DEFAULT ''
);

-- Looked up by the short code when somebody approves one, which is the only query that does not
-- have the hash to hand.
CREATE INDEX IF NOT EXISTS publish_grants_by_code ON publish_grants (user_code);
