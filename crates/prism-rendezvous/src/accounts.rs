//! Accounts: who may sign in, what machines are theirs, and the key only they can open.
//!
//! The server holds three things per account and is trusted with two of them. It knows the
//! verifier that recognises a correct password, and it knows the second factor's secret,
//! because verifying a code means holding what generated it. It also holds the sealed private
//! key, and that one it cannot open: the secret that would is derived from the same password
//! and never sent here.
//!
//! # What a stolen database gives an attacker
//!
//! Worth stating plainly rather than leaving to be discovered.
//!
//! - **The second factor.** The TOTP secret is here in the clear, because a code cannot be
//!   checked against a secret nobody has. Somebody holding this file can generate codes.
//! - **Nothing that opens a key.** The verifier is a hash of a hash, and the sealed key is
//!   sealed under a secret this server has never seen. Opening one means guessing the password
//!   against the memory-hard hash it was derived through.
//!
//! So a leak costs the second factor and the account's contents, and does not cost the key.
//! That is the line this design draws, and it is why the wrapping secret never arrives.
//!
//! # Why a file rather than a database
//!
//! A self-hosted rendezvous serves a person and their machines. The whole store is a handful
//! of records that change when somebody signs up or adds a computer, and the server is
//! deployed as one static binary with no C dependencies — a property an embedded database
//! would cost, for a scale that does not need one.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use prism_core::account::secret::{SALT_LEN, SECRET_LEN, auth_matches};
use prism_core::account::totp;
use prism_core::net::handshake::KEY_LEN;
use serde::{Deserialize, Serialize};

/// One machine belonging to an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The machine's long-term public key, as hex.
    pub public_key: String,
    /// What its owner calls it.
    pub label: String,
    /// When it was added, in seconds since the epoch.
    pub added_unix: u64,
}

/// Everything the server keeps about one account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// The email address the person signs in with.
    ///
    /// Read under its old name too, so a store written before accounts were addressed by
    /// email is still a store this server can open.
    #[serde(alias = "name")]
    pub email: String,
    /// The salt their password was hashed with, as hex.
    ///
    /// Handed out before sign-in, because deriving the secret needs it. That is not a leak:
    /// the salt is public by construction, and its job is to make one table of precomputed
    /// guesses useless against every account rather than to be secret.
    pub salt: String,
    /// What recognises a correct authentication secret, as hex.
    pub verifier: String,
    /// The second factor's shared secret, as hex.
    pub totp_secret: String,
    /// The private key, sealed under a secret this server has never seen, as hex.
    pub sealed_key: String,
    /// The machines signed in to this account.
    pub devices: Vec<Device>,
    /// Whether the relay may be used, which costs bandwidth somebody pays for.
    pub relay_allowed: bool,
    /// Whether the address was proved to belong to whoever registered it.
    ///
    /// Always true for an account this server made, because it will not make one until the
    /// address is proved. It survives as a field for the stores that came before that was so:
    /// absent means an account written under the old rule, and locking those out for failing a
    /// test that did not exist would be a server that ate its own users.
    #[serde(default = "made_before_this_was_asked")]
    pub verified: bool,
}

/// A code sent to an address, waiting to be typed back in.
///
/// Not an account. Nothing is created for an address until somebody proves they read what was
/// sent to it — which is the whole point: an account that exists before that is an account an
/// attacker can park on somebody else's address, with a password and a second factor of their
/// choosing, waiting for the owner to click something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    /// SHA-256 of the six digits, as hex.
    ///
    /// Hashed because this file is the thing an attacker would have stolen, and the digits are
    /// the whole of what stands between them and an address they do not own.
    pub code_sha256: String,
    /// When it stops working, in seconds since the epoch.
    pub expires_unix: u64,
    /// How many wrong guesses have been made against it.
    pub tries: u32,
}

/// What an account written before verification existed is taken to be.
const fn made_before_this_was_asked() -> bool {
    true
}

/// Why something could not be done to an account.
///
/// # These are the words somebody reads
///
/// Not a log line and not a code the interface translates: the sentence written here is the
/// sentence rendered under the form, so it is written as one — what happened, and what to do
/// about it. Rust's convention of a lowercase fragment is set aside for that reason. An error
/// that only says what failed leaves somebody retyping a password that was never wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    /// Somebody already registered that address, and proved it was theirs.
    #[error("That address already has an account — sign in instead.")]
    EmailTaken,
    /// The address is registered but nobody has opened what was sent to it.
    #[error("Open the link sent to that address, then sign in.")]
    NotVerified,
    /// The code is wrong, spent, lapsed, or has been guessed at too many times.
    ///
    /// One error for all four, because telling them apart tells somebody guessing whether they
    /// are close and whether the address is worth guessing at.
    #[error("That code is wrong or has expired. Ask for a new one.")]
    BadCode,
    /// What was given is not an address anything could be delivered to.
    #[error("That does not look like an email address.")]
    BadEmail,
    /// The sign-in did not succeed.
    ///
    /// One error for every reason: no such account, wrong password, wrong code. Telling them
    /// apart tells somebody guessing which half they got right, and whether an address has an
    /// account at all.
    #[error("The email, password or code is wrong.")]
    Refused,
    /// A field was not the length it has to be.
    ///
    /// Nothing a person typed: these are the fields the application derives and sends. Said so
    /// plainly, because somebody looking at it should not go hunting through their own
    /// typing for a mistake that is not there.
    #[error("The application sent a {field} this server could not read.")]
    Malformed {
        /// Which one.
        field: &'static str,
    },
    /// The store could not be read or written.
    #[error("The account store could not be {doing}: {reason}")]
    Store {
        /// What was being attempted.
        doing: &'static str,
        /// What the system said.
        reason: String,
    },
}

/// Shortest an address this server will take, which is `a@b.c`.
pub const MIN_EMAIL: usize = 5;

/// Longest, which is what the standard allows a whole address to be.
pub const MAX_EMAIL: usize = 254;

/// How long a code sent to an address is good for.
///
/// Fifteen minutes. Long enough to switch to a mail reader and come back, short enough that a
/// code left in an inbox is not still a way in tomorrow.
pub const CHALLENGE_LIFETIME_SECS: u64 = 15 * 60;

/// How many wrong guesses a code survives.
///
/// Six digits is a million possibilities, which is a lot for a person and nothing at all for a
/// script. What makes the code strong is not its length but that it stops answering.
pub const CHALLENGE_TRIES: u32 = 5;

/// What a new account is created from.
#[derive(Debug, Clone)]
pub struct Registration {
    /// The email address they will sign in with.
    pub email: String,
    /// The salt their client hashed the password with.
    pub salt: [u8; SALT_LEN],
    /// The authentication secret their client derived.
    pub auth: [u8; SECRET_LEN],
    /// The private key, already sealed by their client.
    pub sealed_key: Vec<u8>,
}

/// Every account this server knows, and every address waiting to prove itself.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    accounts: Vec<Account>,
    /// Codes sent and not yet used, by address.
    ///
    /// Kept in the same file so a server restarted while somebody was halfway through signing
    /// up does not lose the code they are looking at.
    #[serde(default)]
    challenges: std::collections::HashMap<String, Challenge>,
}

/// The accounts, and the file they are kept in.
#[derive(Debug)]
pub struct Accounts {
    by_email: HashMap<String, Account>,
    /// Addresses that have been sent a code and have not used it.
    challenges: HashMap<String, Challenge>,
    path: PathBuf,
    /// Makes the salt handed out for an unknown name look like a real one.
    ///
    /// Without it, asking for a salt is a way to find out which names exist — and a name is
    /// half of what somebody guessing needs.
    decoy: [u8; SECRET_LEN],
}

impl Accounts {
    /// Opens the store, creating an empty one if the file is not there yet.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Store`] if the file exists and cannot be read or parsed. A
    /// missing file is not an error — it is a server that has not been used yet — but an
    /// unreadable one is, because starting fresh over somebody's accounts would be worse than
    /// refusing to start.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AccountError> {
        let path = path.into();

        let stored = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<Stored>(&bytes).map_err(|err| AccountError::Store {
                    doing: "read",
                    reason: err.to_string(),
                })?
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Stored::default(),
            Err(err) => {
                return Err(AccountError::Store {
                    doing: "read",
                    reason: err.to_string(),
                });
            }
        };

        let mut decoy = [0u8; SECRET_LEN];
        getrandom::fill(&mut decoy).map_err(|err| AccountError::Store {
            doing: "seeded with randomness",
            reason: err.to_string(),
        })?;

        Ok(Self {
            challenges: stored.challenges,
            by_email: stored
                .accounts
                .into_iter()
                .map(|account| (account.email.clone(), account))
                .collect(),
            path,
            decoy,
        })
    }

    /// How many accounts exist.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_email.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_email.is_empty()
    }

    /// Removes an account and anything waiting against its address.
    ///
    /// Everything: the verifier, the second factor, the sealed key and the machines. None of it
    /// can be reconstructed, which is the point — an account somebody asked to be rid of that
    /// left a shadow behind would not have been deleted.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Store`] if the result cannot be written.
    pub fn forget(&mut self, email: &str) -> Result<bool, AccountError> {
        let had = self.by_email.remove(email).is_some();
        let waiting = self.challenges.remove(email).is_some();

        if had || waiting {
            self.save()?;
        }

        Ok(had)
    }

    /// Every account, for an operator looking at what is on their own server.
    pub fn all(&self) -> impl Iterator<Item = &Account> {
        self.by_email.values()
    }

    /// Says whether an address is still going spare, without doing anything about it.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::BadEmail`] or [`AccountError::EmailTaken`].
    pub fn check_free(&self, email: &str) -> Result<(), AccountError> {
        check_email(email)?;

        if self.by_email.contains_key(email) {
            return Err(AccountError::EmailTaken);
        }

        Ok(())
    }

    /// Creates an account without proving the address belongs to anybody.
    ///
    /// What a server with no mail configured does, and the only thing it can do. Kept separate
    /// from [`Self::register`] rather than folded in behind an empty code, so that a server
    /// that *can* send has no path through it that skips the proof.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::EmailTaken`], [`AccountError::BadEmail`], or
    /// [`AccountError::Store`].
    pub fn register_unproved(
        &mut self,
        registration: Registration,
    ) -> Result<[u8; totp::SECRET_LEN], AccountError> {
        self.check_free(&registration.email)?;

        self.create(registration)
    }

    /// Sends nothing and stores nothing but the code that proves an address.
    ///
    /// Returns the six digits for the caller to mail. No account is created here, and that is
    /// the point: an account made before its address is proved is one somebody can park on an
    /// address they do not own, holding a password and a second factor of their choosing until
    /// the real owner does something that turns it on.
    ///
    /// Asking twice replaces the code, so somebody who lost the first mail can ask again.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::BadEmail`] for an address nothing could be delivered to,
    /// [`AccountError::EmailTaken`] if it already has an account, and
    /// [`AccountError::Store`] if the result cannot be written.
    pub fn challenge(&mut self, email: &str, now_unix: u64) -> Result<String, AccountError> {
        check_email(email)?;

        if self.by_email.contains_key(email) {
            return Err(AccountError::EmailTaken);
        }

        let code = new_code().map_err(|err| AccountError::Store {
            doing: "seeded with randomness",
            reason: err.to_string(),
        })?;

        self.challenges.insert(
            email.to_owned(),
            Challenge {
                code_sha256: hex(&sha256(code.as_bytes())),
                expires_unix: now_unix + CHALLENGE_LIFETIME_SECS,
                tries: 0,
            },
        );

        self.save()?;

        Ok(code)
    }

    /// Creates an account, once the code sent to its address comes back.
    ///
    /// The second factor's secret is returned rather than stored-and-fetched because this is
    /// the only moment it may leave the server: after this the server will only ever check
    /// codes against it. Handing it out at any later point would mean a password alone could
    /// fetch the thing the password is supposed to be paired with.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::BadCode`] if the code is wrong, spent, lapsed or has been
    /// guessed at too often, [`AccountError::EmailTaken`] if somebody registered the address
    /// in between, [`AccountError::BadEmail`], and [`AccountError::Store`].
    pub fn register(
        &mut self,
        registration: Registration,
        code: &str,
        now_unix: u64,
    ) -> Result<[u8; totp::SECRET_LEN], AccountError> {
        check_email(&registration.email)?;

        if self.by_email.contains_key(&registration.email) {
            return Err(AccountError::EmailTaken);
        }

        self.redeem(&registration.email, code, now_unix)?;

        self.create(registration)
    }

    /// Writes the account and returns its second factor.
    fn create(
        &mut self,
        registration: Registration,
    ) -> Result<[u8; totp::SECRET_LEN], AccountError> {
        let totp_secret = totp::new_secret().map_err(|err| AccountError::Store {
            doing: "seeded with randomness",
            reason: err.to_string(),
        })?;

        self.by_email.insert(
            registration.email.clone(),
            Account {
                email: registration.email,
                salt: hex(&registration.salt),
                verifier: hex(&prism_core::account::secret::auth_verifier(
                    &registration.auth,
                )),
                totp_secret: hex(&totp_secret),
                sealed_key: hex(&registration.sealed_key),
                devices: Vec::new(),
                // On, because it is reached rather than chosen: a pair that can punch a hole
                // to each other never touches it, and a pair that cannot has no other way to
                // meet at all. Leaving it off by default meant the ones who needed it were the
                // ones it was refused to. An operator paying for the bandwidth can still say
                // no — see `set_relay_allowed`.
                relay_allowed: true,
                verified: true,
            },
        );

        self.save()?;

        Ok(totp_secret)
    }

    /// Spends the code standing against an address, or says why it cannot be spent.
    ///
    /// A wrong guess costs one of the tries whether or not the caller comes back, and running
    /// out throws the challenge away. Six digits is nothing to a script; what makes them worth
    /// anything is that they stop answering.
    fn redeem(&mut self, email: &str, code: &str, now_unix: u64) -> Result<(), AccountError> {
        let Some(challenge) = self.challenges.get_mut(email) else {
            return Err(AccountError::BadCode);
        };

        if challenge.expires_unix <= now_unix || challenge.tries >= CHALLENGE_TRIES {
            self.challenges.remove(email);
            self.save()?;

            return Err(AccountError::BadCode);
        }

        if challenge.code_sha256 != hex(&sha256(code.as_bytes())) {
            challenge.tries += 1;
            self.save()?;

            return Err(AccountError::BadCode);
        }

        self.challenges.remove(email);

        Ok(())
    }

    /// Returns the salt to hash a password with, for a name that may or may not exist.
    ///
    /// An unknown name gets a salt of its own that never changes: derived from the name and a
    /// value this server generated at startup. Somebody probing for accounts sees a plausible
    /// answer either way, and the same answer every time, so neither the shape of the reply nor
    /// its repetition says whether anybody is there.
    #[must_use]
    pub fn salt_for(&self, email: &str) -> [u8; SALT_LEN] {
        if let Some(account) = self.by_email.get(email)
            && let Some(salt) = unhex_array::<SALT_LEN>(&account.salt)
        {
            return salt;
        }

        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"prism-decoy-salt-v1");
        hasher.update(self.decoy);
        hasher.update(email.as_bytes());

        let digest = hasher.finalize();
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&digest[..SALT_LEN]);

        salt
    }

    /// Checks a sign-in, and returns the account when it is right.
    ///
    /// Both factors are always checked, even once one has failed. Stopping at the first
    /// failure would let somebody with a stopwatch learn whether the password was right by
    /// how long the refusal took.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Refused`] for every kind of failure, including a name that does
    /// not exist.
    pub fn sign_in(
        &self,
        email: &str,
        auth: &[u8; SECRET_LEN],
        code: u32,
        now_unix: u64,
    ) -> Result<&Account, AccountError> {
        let account = self.by_email.get(email);

        let password_right = account
            .and_then(|account| unhex_array::<SECRET_LEN>(&account.verifier))
            .is_some_and(|verifier| auth_matches(auth, &verifier));

        let code_right = account
            .and_then(|account| unhex(&account.totp_secret))
            .is_some_and(|secret| totp::verify(&secret, code, now_unix));

        if !(password_right && code_right) {
            return Err(AccountError::Refused);
        }

        // Checked last on purpose. Answering "that address is not verified" to somebody who
        // has not proved they know the password would tell a stranger which addresses have
        // accounts waiting on a link — which is the one thing worth knowing to go looking for
        // that link.
        let account = account.ok_or(AccountError::Refused)?;

        if account.verified {
            Ok(account)
        } else {
            Err(AccountError::NotVerified)
        }
    }

    /// Returns an account by name, for a caller that has already established who they are.
    #[must_use]
    pub fn get(&self, email: &str) -> Option<&Account> {
        self.by_email.get(email)
    }

    /// Adds a machine to an account, or renames one already there.
    ///
    /// Adding a key that is already present replaces its label rather than listing it twice,
    /// because signing in again on the same machine is the ordinary case and a list with
    /// duplicates in it is a list nobody can act on.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Refused`] if no such account exists, and
    /// [`AccountError::Malformed`] if the key is not a public key.
    pub fn add_device(
        &mut self,
        email: &str,
        public_key: &[u8; KEY_LEN],
        label: &str,
        now_unix: u64,
    ) -> Result<(), AccountError> {
        let key = hex(public_key);
        let account = self.by_email.get_mut(email).ok_or(AccountError::Refused)?;

        if let Some(existing) = account
            .devices
            .iter_mut()
            .find(|device| device.public_key == key)
        {
            existing.label = label.to_owned();
        } else {
            account.devices.push(Device {
                public_key: key,
                label: label.to_owned(),
                added_unix: now_unix,
            });
        }

        self.save()
    }

    /// Removes a machine from an account.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Refused`] if no such account exists, and
    /// [`AccountError::Store`] if the change cannot be written.
    pub fn remove_device(
        &mut self,
        email: &str,
        public_key: &[u8; KEY_LEN],
    ) -> Result<(), AccountError> {
        let key = hex(public_key);
        let account = self.by_email.get_mut(email).ok_or(AccountError::Refused)?;

        account.devices.retain(|device| device.public_key != key);

        self.save()
    }

    /// Replaces the sealed key, which is what changing a password amounts to here.
    ///
    /// The server cannot check that the new blob is anything in particular — it could not open
    /// the old one either — so this is a straight replacement, and the client is what makes
    /// sure the two secrets it derived belong together.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Refused`] if no such account exists.
    pub fn replace_key(
        &mut self,
        email: &str,
        salt: &[u8; SALT_LEN],
        auth: &[u8; SECRET_LEN],
        sealed_key: &[u8],
    ) -> Result<(), AccountError> {
        let account = self.by_email.get_mut(email).ok_or(AccountError::Refused)?;

        account.salt = hex(salt);
        account.verifier = hex(&prism_core::account::secret::auth_verifier(auth));
        account.sealed_key = hex(sealed_key);

        self.save()
    }

    /// Says whether an account may use the relay.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Refused`] if no such account exists.
    pub fn set_relay_allowed(&mut self, email: &str, allowed: bool) -> Result<(), AccountError> {
        let account = self.by_email.get_mut(email).ok_or(AccountError::Refused)?;
        account.relay_allowed = allowed;

        self.save()
    }

    /// Writes the store, replacing the file only once the new one is complete.
    ///
    /// Through a temporary file and a rename, because a crash partway through writing accounts
    /// over themselves would leave a file that parses as fewer accounts than there are — and
    /// the server refuses to start on a file it cannot parse but starts happily on one that is
    /// merely wrong.
    fn save(&self) -> Result<(), AccountError> {
        let stored = Stored {
            accounts: self.by_email.values().cloned().collect(),
            challenges: self.challenges.clone(),
        };

        let json = serde_json::to_vec_pretty(&stored).map_err(|err| AccountError::Store {
            doing: "written",
            reason: err.to_string(),
        })?;

        let temporary = self.path.with_extension("tmp");
        write_then_rename(&temporary, &self.path, &json).map_err(|err| AccountError::Store {
            doing: "written",
            reason: err.to_string(),
        })
    }
}

/// Writes a file and moves it into place, so a reader never sees a half-written one.
fn write_then_rename(temporary: &Path, final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = final_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(temporary, bytes)?;
    std::fs::rename(temporary, final_path)
}

/// Whether a name is one an account may have.
fn check_email(email: &str) -> Result<(), AccountError> {
    let length = email.chars().count();

    // Deliberately loose. The only claim worth making here is that this could be delivered to;
    // the address is proved by a code arriving at it, not by a pattern, and a stricter rule
    // would mostly reject addresses that are perfectly real.
    let Some((local, domain)) = email.split_once('@') else {
        return Err(AccountError::BadEmail);
    };

    let shaped = (MIN_EMAIL..=MAX_EMAIL).contains(&length)
        && !local.is_empty()
        && !email.contains(char::is_whitespace)
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.');

    if shaped {
        Ok(())
    } else {
        Err(AccountError::BadEmail)
    }
}

/// Returns the SHA-256 of some bytes.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes).into()
}

/// Makes the six digits sent to an address.
///
/// Drawn from a rejection-sampled range rather than by taking a remainder, so every code is as
/// likely as every other. A modulo over a byte stream leaves the low values slightly commoner,
/// which is a small bias and a free one to avoid.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the platform has no usable randomness. A guessable
/// code is a way into somebody else's address, so this fails rather than falling back.
fn new_code() -> io::Result<String> {
    // The largest multiple of a million that fits, so anything above it is thrown away rather
    // than folded back over the low end of the range.
    const CEILING: u32 = u32::MAX - (u32::MAX % 1_000_000);

    loop {
        let mut raw = [0u8; 4];
        getrandom::fill(&mut raw).map_err(|err| io::Error::other(err.to_string()))?;

        let drawn = u32::from_le_bytes(raw);

        if drawn < CEILING {
            return Ok(format!("{:06}", drawn % 1_000_000));
        }
    }
}

/// Renders bytes as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Reads lowercase hex back into bytes, or nothing if it is not hex.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }

    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Reads hex into an array of a known size.
fn unhex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = unhex(text)?;

    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        AccountError, Accounts, CHALLENGE_LIFETIME_SECS, CHALLENGE_TRIES, MAX_EMAIL, Registration,
    };
    use prism_core::account::secret::{SALT_LEN, SECRET_LEN};
    use prism_core::account::totp;
    use prism_core::net::handshake::KEY_LEN;

    /// A store in a directory that goes away with the test.
    fn store(label: &str) -> (Accounts, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "prism-accounts-{label}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        (Accounts::open(&path).expect("opens"), path)
    }

    fn registration(email: &str) -> Registration {
        Registration {
            email: email.to_owned(),
            salt: [1; SALT_LEN],
            auth: [2; SECRET_LEN],
            sealed_key: vec![3; 60],
        }
    }

    #[test]
    fn no_account_exists_until_the_code_comes_back() {
        let (mut accounts, _path) = store("code-first");

        let code = accounts
            .challenge("someone@example.com", 0)
            .expect("sends a code");

        // The whole point. An account that existed here would be one an attacker could park
        // on somebody else's address, holding a password and a second factor of their
        // choosing, waiting for the owner to do something that turned it on.
        assert!(accounts.get("someone@example.com").is_none());

        assert_eq!(
            accounts
                .register(registration("someone@example.com"), "000000", 10)
                .unwrap_err(),
            AccountError::BadCode,
        );

        let secret = accounts
            .register(registration("someone@example.com"), &code, 10)
            .expect("registers");

        assert!(
            accounts
                .get("someone@example.com")
                .expect("exists")
                .verified
        );
        assert!(
            accounts
                .sign_in(
                    "someone@example.com",
                    &[2; SECRET_LEN],
                    totp::code_at_time(&secret, 10),
                    10,
                )
                .is_ok(),
        );
    }

    #[test]
    fn a_code_is_spent_when_it_is_used() {
        let (mut accounts, _path) = store("code-spent");

        let code = accounts.challenge("someone@example.com", 0).expect("sends");
        accounts
            .register(registration("someone@example.com"), &code, 10)
            .expect("registers");

        // The address is taken now, so this stops at the earlier check — but the code is gone
        // either way, which is what keeps a leaked mail from being worth anything later.
        assert_eq!(
            accounts
                .register(registration("someone@example.com"), &code, 10)
                .unwrap_err(),
            AccountError::EmailTaken,
        );
    }

    #[test]
    fn a_code_lapses_and_stops_answering_after_enough_guesses() {
        let (mut accounts, _path) = store("code-limits");

        let code = accounts.challenge("a@b.co", 0).expect("sends");
        assert_eq!(
            accounts
                .register(registration("a@b.co"), &code, CHALLENGE_LIFETIME_SECS + 1)
                .unwrap_err(),
            AccountError::BadCode,
        );

        let code = accounts.challenge("a@b.co", 0).expect("sends again");

        for _ in 0..CHALLENGE_TRIES {
            assert_eq!(
                accounts
                    .register(registration("a@b.co"), "000000", 10)
                    .unwrap_err(),
                AccountError::BadCode,
            );
        }

        // Six digits is nothing to a script. What makes the code worth anything is that it
        // stops answering, so even the right one is refused now.
        assert_eq!(
            accounts
                .register(registration("a@b.co"), &code, 10)
                .unwrap_err(),
            AccountError::BadCode,
        );
    }

    #[test]
    fn asking_twice_replaces_the_code() {
        let (mut accounts, _path) = store("code-again");

        let first = accounts.challenge("a@b.co", 0).expect("sends");
        let second = accounts.challenge("a@b.co", 0).expect("sends again");

        assert_eq!(
            accounts
                .register(registration("a@b.co"), &first, 10)
                .unwrap_err(),
            AccountError::BadCode,
        );
        assert!(
            accounts
                .register(registration("a@b.co"), &second, 10)
                .is_ok()
        );
    }

    #[test]
    fn a_code_is_never_asked_for_an_address_that_already_has_an_account() {
        let (mut accounts, _path) = store("code-taken");

        let code = accounts.challenge("a@b.co", 0).expect("sends");
        accounts
            .register(registration("a@b.co"), &code, 10)
            .expect("registers");

        assert_eq!(
            accounts.challenge("a@b.co", 10).unwrap_err(),
            AccountError::EmailTaken,
        );
    }

    #[test]
    fn an_account_written_before_verification_existed_still_signs_in() {
        let (_fresh, path) = store("legacy-account");

        // A store as an older server wrote it: no `verified`, no challenges. Reading that as
        // unproved would lock out every account made before the rule existed.
        std::fs::write(
            &path,
            br#"{"accounts":[{"name":"aodjo","salt":"08","verifier":"09","totp_secret":"00","sealed_key":"","devices":[],"relay_allowed":false}]}"#,
        )
        .expect("writes");

        let accounts = Accounts::open(&path).expect("opens");

        assert!(accounts.get("aodjo").expect("exists").verified);
    }

    #[test]
    fn a_registered_account_can_sign_in() {
        let (mut accounts, path) = store("signin");
        let secret = accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        let now = 1_700_000_000;
        let code = totp::code_at_time(&secret, now);

        assert!(
            accounts
                .sign_in("someone@example.com", &[2; SECRET_LEN], code, now)
                .is_ok()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_wrong_password_is_refused_even_with_the_right_code() {
        let (mut accounts, path) = store("wrongpass");
        let secret = accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        let now = 1_700_000_000;
        let code = totp::code_at_time(&secret, now);

        assert_eq!(
            accounts
                .sign_in("someone@example.com", &[9; SECRET_LEN], code, now)
                .unwrap_err(),
            AccountError::Refused
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_right_password_alone_is_not_enough() {
        // The whole point of a second factor. A stolen password should not be a session.
        let (mut accounts, path) = store("nocode");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        assert_eq!(
            accounts
                .sign_in("someone@example.com", &[2; SECRET_LEN], 0, 1_700_000_000)
                .unwrap_err(),
            AccountError::Refused
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unknown_name_fails_the_same_way_a_wrong_password_does() {
        // Told apart, these say whether a name exists — which is half of what somebody
        // guessing needs, handed over for free.
        let (mut accounts, path) = store("unknown");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        assert_eq!(
            accounts
                .sign_in("nobody", &[2; SECRET_LEN], 123_456, 1_700_000_000)
                .unwrap_err(),
            AccountError::Refused
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unknown_name_still_gets_a_salt_and_the_same_one_each_time() {
        // Asking for a salt must not be a way to enumerate accounts. A missing answer, or a
        // different answer each time, would both say "nobody here".
        let (accounts, path) = store("decoy");

        let first = accounts.salt_for("nobody");
        let again = accounts.salt_for("nobody");
        let other = accounts.salt_for("somebody-else");

        assert_eq!(first, again, "the decoy salt changed between asks");
        assert_ne!(first, other, "every unknown name shares one decoy salt");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_real_account_gets_the_salt_it_registered_with() {
        let (mut accounts, path) = store("realsalt");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        assert_eq!(accounts.salt_for("someone@example.com"), [1; SALT_LEN]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_same_name_cannot_be_taken_twice() {
        let (mut accounts, path) = store("taken");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        assert_eq!(
            accounts
                .register_unproved(registration("someone@example.com"))
                .unwrap_err(),
            AccountError::EmailTaken
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_account_has_to_be_addressed_by_something_deliverable() {
        let (mut accounts, path) = store("bademail");

        for address in [
            "",
            "nobody",
            "no domain@",
            "@nolocal.com",
            "has space@example.com",
            "trailing@dot.",
            "no.dot@localhost",
            &format!("{}@example.com", "x".repeat(MAX_EMAIL)),
        ] {
            assert_eq!(
                accounts
                    .register_unproved(registration(address))
                    .unwrap_err(),
                AccountError::BadEmail,
                "{address:?} was accepted"
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_relay_is_there_for_a_new_account_and_can_be_taken_away() {
        // On by default because it is reached rather than chosen: an account that needed it
        // and did not have it is a pair of machines that simply cannot meet, with nothing on
        // screen saying why. An operator paying for the bandwidth can still refuse it.
        let (mut accounts, path) = store("relay");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        assert!(
            accounts
                .get("someone@example.com")
                .expect("exists")
                .relay_allowed
        );

        accounts
            .set_relay_allowed("someone@example.com", false)
            .expect("refuses");
        assert!(
            !accounts
                .get("someone@example.com")
                .expect("exists")
                .relay_allowed
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn signing_in_twice_on_one_machine_lists_it_once() {
        let (mut accounts, path) = store("devices");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        accounts
            .add_device("someone@example.com", &[7; KEY_LEN], "laptop", 100)
            .expect("adds");
        accounts
            .add_device("someone@example.com", &[7; KEY_LEN], "the laptop", 200)
            .expect("renames");

        let devices = &accounts.get("someone@example.com").expect("exists").devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].label, "the laptop");
        assert_eq!(devices[0].added_unix, 100, "re-adding reset the date");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_device_can_be_removed() {
        let (mut accounts, path) = store("remove");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");
        accounts
            .add_device("someone@example.com", &[7; KEY_LEN], "laptop", 100)
            .expect("adds");

        accounts
            .remove_device("someone@example.com", &[7; KEY_LEN])
            .expect("removes");

        assert!(
            accounts
                .get("someone@example.com")
                .expect("exists")
                .devices
                .is_empty()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn accounts_survive_the_server_being_restarted() {
        // The entire point of writing them down.
        let (mut accounts, path) = store("persist");
        let secret = accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");
        accounts
            .add_device("someone@example.com", &[7; KEY_LEN], "laptop", 100)
            .expect("adds");
        drop(accounts);

        let reopened = Accounts::open(&path).expect("reopens");
        let now = 1_700_000_000;

        assert_eq!(reopened.len(), 1);
        assert_eq!(
            reopened
                .get("someone@example.com")
                .expect("exists")
                .devices
                .len(),
            1
        );
        assert!(
            reopened
                .sign_in(
                    "someone@example.com",
                    &[2; SECRET_LEN],
                    totp::code_at_time(&secret, now),
                    now
                )
                .is_ok()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_sealed_key_is_kept_exactly_as_it_arrived() {
        // The server cannot check it and must not change it. A byte lost here is a key lost.
        let (mut accounts, path) = store("sealed");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        assert_eq!(
            accounts
                .get("someone@example.com")
                .expect("exists")
                .sealed_key,
            "03".repeat(60)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn changing_the_password_replaces_the_salt_verifier_and_key_together() {
        // All three or none. A salt that no longer matches the verifier is an account nobody
        // can sign in to, including its owner.
        let (mut accounts, path) = store("rekey");
        accounts
            .register_unproved(registration("someone@example.com"))
            .expect("registers");

        accounts
            .replace_key(
                "someone@example.com",
                &[8; SALT_LEN],
                &[9; SECRET_LEN],
                &[4; 60],
            )
            .expect("replaces");

        let now = 1_700_000_000;
        let secret = super::unhex(
            &accounts
                .get("someone@example.com")
                .expect("exists")
                .totp_secret,
        )
        .expect("hex");
        let code = totp::code_at_time(&secret, now);

        assert_eq!(accounts.salt_for("someone@example.com"), [8; SALT_LEN]);
        assert!(
            accounts
                .sign_in("someone@example.com", &[9; SECRET_LEN], code, now)
                .is_ok()
        );
        assert_eq!(
            accounts
                .sign_in("someone@example.com", &[2; SECRET_LEN], code, now)
                .unwrap_err(),
            AccountError::Refused,
            "the old password still works"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_store_is_an_empty_one_and_an_unreadable_store_is_not() {
        // Starting fresh over somebody's accounts would be worse than refusing to start.
        let path = std::env::temp_dir().join(format!("prism-broken-{}.json", std::process::id()));
        std::fs::write(&path, b"{ not json").expect("writes");

        assert!(matches!(
            Accounts::open(&path).unwrap_err(),
            AccountError::Store { .. }
        ));
        let _ = std::fs::remove_file(path);
    }
}
