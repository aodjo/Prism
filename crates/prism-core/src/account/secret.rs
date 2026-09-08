//! Turning one password into two secrets that cannot be derived from each other.
//!
//! An account has to do two unrelated jobs with one password: prove to the server who is
//! signing in, and unlock the private key that server must never see. Using the same value for
//! both would hand the second to whoever is told the first — which is the server, on every
//! sign-in, by design.
//!
//! So the password becomes a root key, and the root key becomes two independent secrets:
//!
//! ```text
//!                  Argon2id(password, salt)
//!                            │
//!                          root
//!                    ┌───────┴───────┐
//!                 HKDF"auth"      HKDF"wrap"
//!                    │               │
//!            sent to the server   never leaves this machine
//! ```
//!
//! Neither half tells you anything about the other. A server that has seen every sign-in, and
//! a database that has leaked in full, still cannot open the key: that would need `wrap`, and
//! `wrap` is not derivable from `auth` any more than it is from the hash the server stores.
//!
//! # Why the work factor is what it is
//!
//! The whole design rests on a password, so it rests on how expensive guessing one is.
//! Argon2id is memory-hard, which is what stops the guessing being done cheaply on a graphics
//! card — the thing that makes ordinary password hashes worthless against an attacker who has
//! taken the database.

use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

/// Bytes in every secret here.
pub const SECRET_LEN: usize = 32;

/// Bytes in an account's salt.
pub const SALT_LEN: usize = 16;

/// Memory the password hash must occupy, in kibibytes.
///
/// Sixty-four mebibytes. Above what a phone would like and comfortably within what any machine
/// running a remote desktop has, which is the right trade here: the cost is paid once at
/// sign-in by one person, and paid again for every guess by anybody attacking the database.
const MEMORY_KIB: u32 = 64 * 1024;

/// Passes over that memory.
const PASSES: u32 = 3;

/// How much of the work may happen at once.
///
/// One. Parallelism lets an attacker with many cores finish sooner without spending more
/// memory, and the few hundred milliseconds this takes are not worth shortening.
const LANES: u32 = 1;

/// Info string separating the authentication secret from every other use of the root.
const AUTH_INFO: &[u8] = b"prism-account-auth-v1";

/// Info string separating the wrapping secret from every other use of the root.
const WRAP_INFO: &[u8] = b"prism-account-wrap-v1";

/// What a password expands into.
///
/// Held together only for as long as it takes to use them, because between them they are the
/// whole account.
#[derive(Clone)]
pub struct Secrets {
    /// Proves to the server who is signing in. It sees this.
    pub auth: [u8; SECRET_LEN],
    /// Unlocks the private key. The server must never see this.
    pub wrap: [u8; SECRET_LEN],
}

impl core::fmt::Debug for Secrets {
    /// Prints nothing of either secret.
    ///
    /// A password derivative reaching a log is the same accident as the password reaching one,
    /// and it is an accident that happens by accident.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Secrets").finish_non_exhaustive()
    }
}

impl Drop for Secrets {
    /// Overwrites both secrets before the memory is released.
    fn drop(&mut self) {
        // A volatile write, so the compiler cannot decide that overwriting memory nobody reads
        // again is work it may skip. That optimisation is correct in general and wrong here.
        for byte in self.auth.iter_mut().chain(self.wrap.iter_mut()) {
            // SAFETY: the pointer is to a byte of an array this value owns and is still alive.
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
    }
}

/// A password that could not be turned into secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The password hash refused the work factor or the output length.
    ///
    /// Not something a person can cause; it means this build asked for parameters the library
    /// rejects, which is a mistake in the constants above rather than in what was typed.
    #[error("The password could not be hashed: {reason}")]
    Hash {
        /// What the library said.
        reason: String,
    },
    /// The password is too short to be worth hashing.
    ///
    /// Reaches somebody as a sentence under the form they typed it into, so it is written as
    /// one — see [`crate::account`] and the account server's own errors for the same reason.
    #[error("A password needs at least {minimum} characters.")]
    TooShort {
        /// The shortest accepted.
        minimum: usize,
    },
}

/// Shortest password an account may have.
///
/// Eight, which is not much. The memory-hard hash is what makes a short password survivable at
/// all, and refusing longer minimums here would only push people toward writing one down.
pub const MIN_PASSWORD: usize = 8;

/// Derives both secrets from a password and the account's salt.
///
/// Deliberately slow — a few hundred milliseconds — because every one of those milliseconds is
/// also paid by somebody guessing.
///
/// # Errors
///
/// Returns [`SecretError::TooShort`] for a password under [`MIN_PASSWORD`] characters, and
/// [`SecretError::Hash`] if the hash itself refuses, which would be a fault in this build
/// rather than in the password.
pub fn derive(password: &str, salt: &[u8; SALT_LEN]) -> Result<Secrets, SecretError> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(SecretError::TooShort {
            minimum: MIN_PASSWORD,
        });
    }

    let params = Params::new(MEMORY_KIB, PASSES, LANES, Some(SECRET_LEN)).map_err(|err| {
        SecretError::Hash {
            reason: err.to_string(),
        }
    })?;

    let mut root = [0u8; SECRET_LEN];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut root)
        .map_err(|err| SecretError::Hash {
            reason: err.to_string(),
        })?;

    let secrets = Secrets {
        auth: expand(&root, AUTH_INFO),
        wrap: expand(&root, WRAP_INFO),
    };

    for byte in &mut root {
        // SAFETY: the pointer is to a byte of a live local array this function owns.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }

    Ok(secrets)
}

/// Separates one use of the root key from every other.
fn expand(root: &[u8; SECRET_LEN], info: &[u8]) -> [u8; SECRET_LEN] {
    let mut out = [0u8; SECRET_LEN];

    Hkdf::<Sha256>::new(None, root)
        .expand(info, &mut out)
        .expect("thirty-two bytes is within HKDF's output limit");

    out
}

/// What the server keeps so that it can recognise the authentication secret again.
///
/// A plain hash rather than another password hash. The input is already thirty-two bytes of
/// Argon2 output, so there is no dictionary to run against it: an attacker holding this has
/// nothing cheaper to do than guess the password, which is exactly the work Argon2 priced.
#[must_use]
pub fn auth_verifier(auth: &[u8; SECRET_LEN]) -> [u8; SECRET_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(b"prism-account-verifier-v1");
    hasher.update(auth);

    hasher.finalize().into()
}

/// Whether an authentication secret matches the stored verifier.
///
/// Compared in constant time, so that a server answering many attempts does not leak how much
/// of a guess was right through how long it took to say no.
#[must_use]
pub fn auth_matches(auth: &[u8; SECRET_LEN], verifier: &[u8; SECRET_LEN]) -> bool {
    use subtle::ConstantTimeEq;

    auth_verifier(auth).ct_eq(verifier).into()
}

/// Generates a salt for a new account.
///
/// # Errors
///
/// Returns an error if the system has no randomness, which is a condition no account should be
/// created under.
pub fn new_salt() -> Result<[u8; SALT_LEN], getrandom::Error> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt)?;

    Ok(salt)
}

#[cfg(test)]
mod tests {
    use super::{MIN_PASSWORD, SALT_LEN, SecretError, auth_matches, auth_verifier, derive};

    /// A fixed salt, so a test says the same thing every time it runs.
    const SALT: [u8; SALT_LEN] = [7; SALT_LEN];

    #[test]
    fn the_two_secrets_are_nothing_like_each_other() {
        // The whole design rests on this. If the server could get from what it is told to what
        // it must never have, storing the wrapped key on it would be storing the key on it.
        let secrets = derive("correct horse battery", &SALT).expect("derives");

        assert_ne!(secrets.auth, secrets.wrap);
    }

    #[test]
    fn the_same_password_and_salt_always_give_the_same_secrets() {
        // Signing in on a second machine has to reach the same key, from nothing but what was
        // typed and what the server stored.
        let first = derive("correct horse battery", &SALT).expect("derives");
        let again = derive("correct horse battery", &SALT).expect("derives");

        assert_eq!(first.auth, again.auth);
        assert_eq!(first.wrap, again.wrap);
    }

    #[test]
    fn a_different_salt_gives_different_secrets() {
        // Which is what stops one table of precomputed guesses from opening every account.
        let mine = derive("correct horse battery", &SALT).expect("derives");
        let theirs = derive("correct horse battery", &[9; SALT_LEN]).expect("derives");

        assert_ne!(mine.auth, theirs.auth);
        assert_ne!(mine.wrap, theirs.wrap);
    }

    #[test]
    fn one_character_changes_everything() {
        let right = derive("correct horse battery", &SALT).expect("derives");
        let wrong = derive("correct horse batterz", &SALT).expect("derives");

        assert_ne!(right.auth, wrong.auth);
        assert_ne!(right.wrap, wrong.wrap);
    }

    #[test]
    fn a_short_password_is_refused_before_any_work_is_done() {
        assert_eq!(
            derive("short", &SALT).unwrap_err(),
            SecretError::TooShort {
                minimum: MIN_PASSWORD
            }
        );
    }

    #[test]
    fn the_verifier_recognises_the_secret_it_was_made_from() {
        let secrets = derive("correct horse battery", &SALT).expect("derives");
        let stored = auth_verifier(&secrets.auth);

        assert!(auth_matches(&secrets.auth, &stored));
    }

    #[test]
    fn the_verifier_refuses_a_different_secret() {
        let mine = derive("correct horse battery", &SALT).expect("derives");
        let theirs = derive("incorrect horse battery", &SALT).expect("derives");

        assert!(!auth_matches(&theirs.auth, &auth_verifier(&mine.auth)));
    }

    #[test]
    fn the_verifier_is_not_the_secret() {
        // Stored in a database that may one day be read by somebody who should not have it.
        let secrets = derive("correct horse battery", &SALT).expect("derives");

        assert_ne!(auth_verifier(&secrets.auth), secrets.auth);
    }

    #[test]
    fn the_wrapping_secret_never_reaches_the_verifier() {
        // The server holds the verifier. If the wrapping secret could be recognised by it, the
        // server would be able to test guesses at the thing it must not have.
        let secrets = derive("correct horse battery", &SALT).expect("derives");

        assert!(!auth_matches(&secrets.wrap, &auth_verifier(&secrets.auth)));
    }
}
