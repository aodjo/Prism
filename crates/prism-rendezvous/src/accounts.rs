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
    /// Whether somebody proved the address is theirs by opening what was sent to it.
    ///
    /// Absent in a store written before addresses were proved at all, and read as `true` when
    /// it is: those accounts were made under the old rule and locking them out for failing a
    /// test that did not exist would be a server that ate its own users.
    #[serde(default = "made_before_this_was_asked")]
    pub verified: bool,
    /// The link that has been sent and not yet opened, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

/// A verification link that has been sent and not yet opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// SHA-256 of the token in the link, as hex.
    ///
    /// Hashed for the same reason the password verifier is: what is in the link is enough to
    /// take the account, and this file is the thing an attacker would have stolen.
    pub token_sha256: String,
    /// When the link stops working, in seconds since the epoch.
    pub expires_unix: u64,
}

/// What an account written before verification existed is taken to be.
const fn made_before_this_was_asked() -> bool {
    true
}

/// Why something could not be done to an account.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    /// Somebody already registered that address, and proved it was theirs.
    #[error("that email address already has an account")]
    EmailTaken,
    /// The address is registered but nobody has opened what was sent to it.
    #[error("open the link sent to that address before signing in")]
    NotVerified,
    /// The link is not one this server sent, or it has already been used, or it has lapsed.
    #[error("that link is no longer good — register again to get a new one")]
    BadToken,
    /// What was given is not an address anything could be delivered to.
    #[error("that does not look like an email address")]
    BadEmail,
    /// The sign-in did not succeed.
    ///
    /// One error for every reason: no such account, wrong password, wrong code. Telling them
    /// apart tells somebody guessing which half they got right, and whether a name exists at
    /// all.
    #[error("the email, password or code is wrong")]
    Refused,
    /// A field was not the length it has to be.
    #[error("{field} is malformed")]
    Malformed {
        /// Which one.
        field: &'static str,
    },
    /// The store could not be read or written.
    #[error("the account store could not be {doing}: {reason}")]
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

/// How long a verification link is good for.
///
/// A day. Long enough to survive being read the next morning, short enough that a link sitting
/// in a mailbox somebody lost control of stops being a way in.
pub const LINK_LIFETIME_SECS: u64 = 24 * 60 * 60;

/// Bytes in a verification token, before it is written as hex.
const TOKEN_LEN: usize = 32;

/// What registering produced.
#[derive(Debug, Clone)]
pub struct Enrolled {
    /// The second factor's secret, to be shown once and never again.
    pub totp_secret: [u8; totp::SECRET_LEN],
    /// The token to put in the link, or `None` when this server does not prove addresses.
    pub token: Option<String>,
}

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

/// Every account this server knows.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    accounts: Vec<Account>,
}

/// The accounts, and the file they are kept in.
#[derive(Debug)]
pub struct Accounts {
    by_email: HashMap<String, Account>,
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

    /// Creates an account and returns the second factor's secret, once.
    ///
    /// The secret is returned rather than stored-and-fetched because this is the only moment
    /// it may leave the server: the client shows it as a code to scan, and after that the
    /// server will only ever check codes against it.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::EmailTaken`], [`AccountError::BadEmail`], or
    /// [`AccountError::Store`] if the file cannot be written.
    pub fn register(
        &mut self,
        registration: Registration,
        verify: bool,
        now_unix: u64,
    ) -> Result<Enrolled, AccountError> {
        check_email(&registration.email)?;

        // An address nobody has proved belongs to them is an address still going spare. Taking
        // it over is what keeps somebody from parking on an address they do not own and
        // locking out the person who does — the whole squatting problem verification is for
        // would otherwise survive it.
        if let Some(existing) = self.by_email.get(&registration.email)
            && existing.verified
        {
            return Err(AccountError::EmailTaken);
        }

        let totp_secret = totp::new_secret().map_err(|err| AccountError::Store {
            doing: "seeded with randomness",
            reason: err.to_string(),
        })?;

        let token = if verify {
            Some(new_token().map_err(|err| AccountError::Store {
                doing: "seeded with randomness",
                reason: err.to_string(),
            })?)
        } else {
            None
        };

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
                // Off until somebody decides otherwise. The relay costs bandwidth, and a
                // server that gave it away by default would be one nobody could afford to run.
                relay_allowed: false,
                verified: !verify,
                pending: token.as_ref().map(|token| Pending {
                    token_sha256: hex(&sha256(token.as_bytes())),
                    expires_unix: now_unix + LINK_LIFETIME_SECS,
                }),
            },
        );

        self.save()?;

        Ok(Enrolled { totp_secret, token })
    }

    /// Takes back an account whose address was never proved.
    ///
    /// For the one case that needs it: a registration whose link could not be sent. Leaving
    /// that account in place would hold the address against the person trying again, and it
    /// could never sign in anyway.
    ///
    /// Refuses to touch a verified account, so this cannot become a way to delete one.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::Store`] if the result cannot be written.
    pub fn forget_unverified(&mut self, email: &str) -> Result<(), AccountError> {
        if self
            .by_email
            .get(email)
            .is_some_and(|account| account.verified)
        {
            return Ok(());
        }

        self.by_email.remove(email);
        self.save()
    }

    /// Marks an address proved, given the token that was sent to it.
    ///
    /// The token is spent: opening the same link twice fails the second time, because what
    /// makes it good is deleted as it is accepted.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::BadToken`] if no account is waiting on that token, or the one
    /// that was has lapsed, and [`AccountError::Store`] if the result cannot be written.
    pub fn verify(&mut self, token: &str, now_unix: u64) -> Result<String, AccountError> {
        let wanted = hex(&sha256(token.as_bytes()));

        let email = self
            .by_email
            .values()
            .find(|account| {
                account.pending.as_ref().is_some_and(|pending| {
                    pending.token_sha256 == wanted && pending.expires_unix > now_unix
                })
            })
            .map(|account| account.email.clone())
            .ok_or(AccountError::BadToken)?;

        if let Some(account) = self.by_email.get_mut(&email) {
            account.verified = true;
            account.pending = None;
        }

        self.save()?;

        Ok(email)
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

/// Makes a token for one verification link.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the platform has no usable randomness. A guessable
/// link is a way into somebody else's account, so this fails rather than falling back.
fn new_token() -> io::Result<String> {
    let mut token = [0u8; TOKEN_LEN];
    getrandom::fill(&mut token).map_err(|err| io::Error::other(err.to_string()))?;

    Ok(hex(&token))
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
    use super::{AccountError, Accounts, LINK_LIFETIME_SECS, MAX_EMAIL, Registration};
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
    fn an_address_cannot_sign_in_until_the_link_is_opened() {
        let (mut accounts, _path) = store("until-opened");

        let enrolled = accounts
            .register(registration("someone@example.com"), true, 1_700_000_000)
            .expect("registers");

        let token = enrolled.token.clone().expect("a link was made");
        let now = 1_700_000_000;
        let code = totp::code_at_time(&enrolled.totp_secret, now);

        assert_eq!(
            accounts
                .sign_in("someone@example.com", &[2; SECRET_LEN], code, now)
                .unwrap_err(),
            AccountError::NotVerified,
        );

        assert_eq!(
            accounts.verify(&token, now).expect("verifies"),
            "someone@example.com",
        );

        assert!(
            accounts
                .sign_in("someone@example.com", &[2; SECRET_LEN], code, now)
                .is_ok(),
        );
    }

    #[test]
    fn a_link_works_once_and_not_after_it_lapses() {
        let (mut accounts, _path) = store("once-only");

        let token = accounts
            .register(registration("someone@example.com"), true, 0)
            .expect("registers")
            .token
            .expect("a link was made");

        // Past its day, so the same link that would have worked no longer does.
        assert_eq!(
            accounts.verify(&token, LINK_LIFETIME_SECS + 1).unwrap_err(),
            AccountError::BadToken,
        );

        assert!(accounts.verify(&token, 10).is_ok());

        // Spent. Opening it again is not a second chance at anything.
        assert_eq!(
            accounts.verify(&token, 10).unwrap_err(),
            AccountError::BadToken,
        );
    }

    #[test]
    fn an_unproved_address_can_be_registered_over() {
        let (mut accounts, _path) = store("register-over");

        let first = accounts
            .register(registration("someone@example.com"), true, 0)
            .expect("registers")
            .token
            .expect("a link was made");

        // Nobody proved the first one, so the address is still going spare. Without this,
        // registering an address somebody else owns would lock them out of it for good.
        let second = accounts
            .register(registration("someone@example.com"), true, 0)
            .expect("registers again")
            .token
            .expect("a link was made");

        assert_ne!(first, second);
        assert_eq!(
            accounts.verify(&first, 10).unwrap_err(),
            AccountError::BadToken,
        );
        assert!(accounts.verify(&second, 10).is_ok());

        // Proved now, so it is taken.
        assert_eq!(
            accounts
                .register(registration("someone@example.com"), true, 0)
                .unwrap_err(),
            AccountError::EmailTaken,
        );
    }

    #[test]
    fn an_account_written_before_verification_existed_still_signs_in() {
        let (_fresh, path) = store("legacy-account");

        // A store as an older server wrote it: no `verified`, no `pending`. Reading those as
        // unverified would lock out every account made before the rule existed.
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
            .register(registration("someone@example.com"), false, 0)
            .expect("registers");

        let now = 1_700_000_000;
        let code = totp::code_at_time(&secret.totp_secret, now);

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
            .register(registration("someone@example.com"), false, 0)
            .expect("registers");

        let now = 1_700_000_000;
        let code = totp::code_at_time(&secret.totp_secret, now);

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
            .register(registration("someone@example.com"), false, 0)
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
            .register(registration("someone@example.com"), false, 0)
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
            .register(registration("someone@example.com"), false, 0)
            .expect("registers");

        assert_eq!(accounts.salt_for("someone@example.com"), [1; SALT_LEN]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_same_name_cannot_be_taken_twice() {
        let (mut accounts, path) = store("taken");
        accounts
            .register(registration("someone@example.com"), false, 0)
            .expect("registers");

        assert_eq!(
            accounts
                .register(registration("someone@example.com"), false, 0)
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
                    .register(registration(address), false, 0)
                    .unwrap_err(),
                AccountError::BadEmail,
                "{address:?} was accepted"
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_relay_is_off_until_somebody_says_otherwise() {
        // It costs bandwidth that somebody pays for, so it is not something a new account
        // should quietly arrive holding.
        let (mut accounts, path) = store("relay");
        accounts
            .register(registration("someone@example.com"), false, 0)
            .expect("registers");

        assert!(
            !accounts
                .get("someone@example.com")
                .expect("exists")
                .relay_allowed
        );

        accounts
            .set_relay_allowed("someone@example.com", true)
            .expect("allows");
        assert!(
            accounts
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
            .register(registration("someone@example.com"), false, 0)
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
            .register(registration("someone@example.com"), false, 0)
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
            .register(registration("someone@example.com"), false, 0)
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
                    totp::code_at_time(&secret.totp_secret, now),
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
            .register(registration("someone@example.com"), false, 0)
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
            .register(registration("someone@example.com"), false, 0)
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
