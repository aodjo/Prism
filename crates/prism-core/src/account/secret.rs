//! Turning one password into two secrets that cannot be derived from each other.
//!
//! The derivation itself lives in [`prism_secret`], because three programs have to reach the
//! same answer from the same password — this one, the account server, and the operator
//! dashboard, which derives in a browser through `prism-account-wasm`. A second copy of it
//! would be a second answer, appearing as an account that signs in on one and not the other.
//!
//! Everything that crate exposes is re-exported here, so nothing that already says
//! `prism_core::account::secret::derive` has to learn a new path. What stays is [`new_salt`],
//! which needs randomness from the operating system and therefore cannot be in a crate that
//! compiles for a browser.

pub use prism_secret::{
    MIN_PASSWORD, SALT_LEN, SECRET_LEN, SecretError, Secrets, auth_matches, auth_verifier, derive,
};

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
    use super::{SALT_LEN, new_salt};

    #[test]
    fn a_salt_is_not_the_same_twice() {
        // The one thing this file still owns. Two accounts created on one machine sharing a
        // salt would let one table of precomputed guesses open both.
        let first = new_salt().expect("the system has randomness");
        let again = new_salt().expect("the system has randomness");

        assert_ne!(first, again);
        assert_eq!(first.len(), SALT_LEN);
    }
}
