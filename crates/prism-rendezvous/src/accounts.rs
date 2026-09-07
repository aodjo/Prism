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
    /// What the person signs in as.
    pub name: String,
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
}

/// Why something could not be done to an account.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    /// The name is already taken.
    #[error("that name is already in use")]
    NameTaken,
    /// The name is not one an account may have.
    #[error("a name must be {min} to {max} characters of letters, digits, dot, dash or underscore")]
    BadName {
        /// Shortest allowed.
        min: usize,
        /// Longest allowed.
        max: usize,
    },
    /// The sign-in did not succeed.
    ///
    /// One error for every reason: no such account, wrong password, wrong code. Telling them
    /// apart tells somebody guessing which half they got right, and whether a name exists at
    /// all.
    #[error("the name, password or code is wrong")]
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

/// Shortest an account name may be.
pub const MIN_NAME: usize = 3;

/// Longest an account name may be.
pub const MAX_NAME: usize = 32;

/// What a new account is created from.
#[derive(Debug, Clone)]
pub struct Registration {
    /// What the person will sign in as.
    pub name: String,
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
    by_name: HashMap<String, Account>,
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
            by_name: stored
                .accounts
                .into_iter()
                .map(|account| (account.name.clone(), account))
                .collect(),
            path,
            decoy,
        })
    }

    /// How many accounts exist.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Creates an account and returns the second factor's secret, once.
    ///
    /// The secret is returned rather than stored-and-fetched because this is the only moment
    /// it may leave the server: the client shows it as a code to scan, and after that the
    /// server will only ever check codes against it.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::NameTaken`], [`AccountError::BadName`], or
    /// [`AccountError::Store`] if the file cannot be written.
    pub fn register(
        &mut self,
        registration: Registration,
    ) -> Result<[u8; totp::SECRET_LEN], AccountError> {
        check_name(&registration.name)?;

        if self.by_name.contains_key(&registration.name) {
            return Err(AccountError::NameTaken);
        }

        let totp_secret = totp::new_secret().map_err(|err| AccountError::Store {
            doing: "seeded with randomness",
            reason: err.to_string(),
        })?;

        self.by_name.insert(
            registration.name.clone(),
            Account {
                name: registration.name,
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
            },
        );

        self.save()?;

        Ok(totp_secret)
    }

    /// Returns the salt to hash a password with, for a name that may or may not exist.
    ///
    /// An unknown name gets a salt of its own that never changes: derived from the name and a
    /// value this server generated at startup. Somebody probing for accounts sees a plausible
    /// answer either way, and the same answer every time, so neither the shape of the reply nor
    /// its repetition says whether anybody is there.
    #[must_use]
    pub fn salt_for(&self, name: &str) -> [u8; SALT_LEN] {
        if let Some(account) = self.by_name.get(name)
            && let Some(salt) = unhex_array::<SALT_LEN>(&account.salt)
        {
            return salt;
        }

        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"prism-decoy-salt-v1");
        hasher.update(self.decoy);
        hasher.update(name.as_bytes());

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
        name: &str,
        auth: &[u8; SECRET_LEN],
        code: u32,
        now_unix: u64,
    ) -> Result<&Account, AccountError> {
        let account = self.by_name.get(name);

        let password_right = account
            .and_then(|account| unhex_array::<SECRET_LEN>(&account.verifier))
            .is_some_and(|verifier| auth_matches(auth, &verifier));

        let code_right = account
            .and_then(|account| unhex(&account.totp_secret))
            .is_some_and(|secret| totp::verify(&secret, code, now_unix));

        if password_right && code_right {
            account.ok_or(AccountError::Refused)
        } else {
            Err(AccountError::Refused)
        }
    }

    /// Returns an account by name, for a caller that has already established who they are.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Account> {
        self.by_name.get(name)
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
        name: &str,
        public_key: &[u8; KEY_LEN],
        label: &str,
        now_unix: u64,
    ) -> Result<(), AccountError> {
        let key = hex(public_key);
        let account = self.by_name.get_mut(name).ok_or(AccountError::Refused)?;

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
        name: &str,
        public_key: &[u8; KEY_LEN],
    ) -> Result<(), AccountError> {
        let key = hex(public_key);
        let account = self.by_name.get_mut(name).ok_or(AccountError::Refused)?;

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
        name: &str,
        salt: &[u8; SALT_LEN],
        auth: &[u8; SECRET_LEN],
        sealed_key: &[u8],
    ) -> Result<(), AccountError> {
        let account = self.by_name.get_mut(name).ok_or(AccountError::Refused)?;

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
    pub fn set_relay_allowed(&mut self, name: &str, allowed: bool) -> Result<(), AccountError> {
        let account = self.by_name.get_mut(name).ok_or(AccountError::Refused)?;
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
            accounts: self.by_name.values().cloned().collect(),
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
fn check_name(name: &str) -> Result<(), AccountError> {
    let length = name.chars().count();

    let shaped = (MIN_NAME..=MAX_NAME).contains(&length)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));

    if shaped {
        Ok(())
    } else {
        Err(AccountError::BadName {
            min: MIN_NAME,
            max: MAX_NAME,
        })
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
    use super::{AccountError, Accounts, MAX_NAME, MIN_NAME, Registration};
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

    fn registration(name: &str) -> Registration {
        Registration {
            name: name.to_owned(),
            salt: [1; SALT_LEN],
            auth: [2; SECRET_LEN],
            sealed_key: vec![3; 60],
        }
    }

    #[test]
    fn a_registered_account_can_sign_in() {
        let (mut accounts, path) = store("signin");
        let secret = accounts
            .register(registration("someone"))
            .expect("registers");

        let now = 1_700_000_000;
        let code = totp::code_at_time(&secret, now);

        assert!(
            accounts
                .sign_in("someone", &[2; SECRET_LEN], code, now)
                .is_ok()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_wrong_password_is_refused_even_with_the_right_code() {
        let (mut accounts, path) = store("wrongpass");
        let secret = accounts
            .register(registration("someone"))
            .expect("registers");

        let now = 1_700_000_000;
        let code = totp::code_at_time(&secret, now);

        assert_eq!(
            accounts
                .sign_in("someone", &[9; SECRET_LEN], code, now)
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
            .register(registration("someone"))
            .expect("registers");

        assert_eq!(
            accounts
                .sign_in("someone", &[2; SECRET_LEN], 0, 1_700_000_000)
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
            .register(registration("someone"))
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
            .register(registration("someone"))
            .expect("registers");

        assert_eq!(accounts.salt_for("someone"), [1; SALT_LEN]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_same_name_cannot_be_taken_twice() {
        let (mut accounts, path) = store("taken");
        accounts
            .register(registration("someone"))
            .expect("registers");

        assert_eq!(
            accounts.register(registration("someone")).unwrap_err(),
            AccountError::NameTaken
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_name_has_to_be_a_name() {
        let (mut accounts, path) = store("badname");

        for name in ["", "ab", "has space", "slash/es", &"x".repeat(MAX_NAME + 1)] {
            assert_eq!(
                accounts.register(registration(name)).unwrap_err(),
                AccountError::BadName {
                    min: MIN_NAME,
                    max: MAX_NAME
                },
                "{name:?} was accepted"
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
            .register(registration("someone"))
            .expect("registers");

        assert!(!accounts.get("someone").expect("exists").relay_allowed);

        accounts.set_relay_allowed("someone", true).expect("allows");
        assert!(accounts.get("someone").expect("exists").relay_allowed);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn signing_in_twice_on_one_machine_lists_it_once() {
        let (mut accounts, path) = store("devices");
        accounts
            .register(registration("someone"))
            .expect("registers");

        accounts
            .add_device("someone", &[7; KEY_LEN], "laptop", 100)
            .expect("adds");
        accounts
            .add_device("someone", &[7; KEY_LEN], "the laptop", 200)
            .expect("renames");

        let devices = &accounts.get("someone").expect("exists").devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].label, "the laptop");
        assert_eq!(devices[0].added_unix, 100, "re-adding reset the date");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_device_can_be_removed() {
        let (mut accounts, path) = store("remove");
        accounts
            .register(registration("someone"))
            .expect("registers");
        accounts
            .add_device("someone", &[7; KEY_LEN], "laptop", 100)
            .expect("adds");

        accounts
            .remove_device("someone", &[7; KEY_LEN])
            .expect("removes");

        assert!(accounts.get("someone").expect("exists").devices.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn accounts_survive_the_server_being_restarted() {
        // The entire point of writing them down.
        let (mut accounts, path) = store("persist");
        let secret = accounts
            .register(registration("someone"))
            .expect("registers");
        accounts
            .add_device("someone", &[7; KEY_LEN], "laptop", 100)
            .expect("adds");
        drop(accounts);

        let reopened = Accounts::open(&path).expect("reopens");
        let now = 1_700_000_000;

        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.get("someone").expect("exists").devices.len(), 1);
        assert!(
            reopened
                .sign_in(
                    "someone",
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
            .register(registration("someone"))
            .expect("registers");

        assert_eq!(
            accounts.get("someone").expect("exists").sealed_key,
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
            .register(registration("someone"))
            .expect("registers");

        accounts
            .replace_key("someone", &[8; SALT_LEN], &[9; SECRET_LEN], &[4; 60])
            .expect("replaces");

        let now = 1_700_000_000;
        let secret =
            super::unhex(&accounts.get("someone").expect("exists").totp_secret).expect("hex");
        let code = totp::code_at_time(&secret, now);

        assert_eq!(accounts.salt_for("someone"), [8; SALT_LEN]);
        assert!(
            accounts
                .sign_in("someone", &[9; SECRET_LEN], code, now)
                .is_ok()
        );
        assert_eq!(
            accounts
                .sign_in("someone", &[2; SECRET_LEN], code, now)
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
