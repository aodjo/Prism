//! The private key, sealed so that only the password opens it.
//!
//! A machine's identity key is what proves it is itself. Keeping it only on the machine that
//! generated it means a person with two computers has two identities and must pair them by
//! hand; keeping a copy on the server means the server can be somebody's screen.
//!
//! This is the third answer: the server holds the key, sealed under a secret derived from the
//! password and never sent to it. Signing in on a new machine fetches the sealed blob and
//! opens it locally.
//!
//! # What this trades
//!
//! The key is now only as strong as the password. An attacker who takes the server's database
//! has the sealed blob and can guess at it forever, offline, with nobody to notice. That is
//! precisely the attack the memory-hard hash in [`super::secret`] is priced against, and it is
//! the reason the work factor there is not a knob to turn down.
//!
//! It also means a forgotten password is a lost key. There is no reset: a server that could
//! reset it could open it.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::account::secret::SECRET_LEN;
use crate::net::handshake::KEY_LEN;

/// Bytes in the nonce a sealed key carries.
pub const NONCE_LEN: usize = 12;

/// Bytes in the authentication tag.
pub const TAG_LEN: usize = 16;

/// Bytes in a sealed key: the nonce in the clear, then the key and its tag.
pub const SEALED_LEN: usize = NONCE_LEN + KEY_LEN + TAG_LEN;

/// Associated data every sealed key is bound to.
///
/// Not secret; it is there so that a blob from one version of this format cannot be opened as
/// though it were another, and so that a blob lifted out of one account cannot be presented as
/// belonging to a different one.
const CONTEXT: &[u8] = b"prism-identity-vault-v1";

/// A sealed key that would not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    /// The blob is not the length a sealed key has.
    #[error("a sealed key is {SEALED_LEN} bytes, this is {actual}")]
    WrongLength {
        /// What arrived.
        actual: usize,
    },
    /// The password was wrong, or the blob was altered.
    ///
    /// One error for both, because they are the same event from here: the tag did not verify.
    /// Telling them apart would mean trusting something inside the blob before checking it.
    #[error("the sealed key did not open; the password is wrong or the data was altered")]
    Refused,
    /// The system had no randomness to make a nonce from.
    #[error("no randomness available to seal a key")]
    NoRandomness,
}

/// Seals a private key under the wrapping secret.
///
/// The nonce is fresh every time, so sealing the same key twice produces different bytes —
/// which matters because the server sees these and would otherwise learn that a password had
/// been changed to one it had seen before.
///
/// # Errors
///
/// Returns [`VaultError::NoRandomness`] if a nonce cannot be generated.
pub fn seal(
    private: &[u8; KEY_LEN],
    wrap: &[u8; SECRET_LEN],
) -> Result<[u8; SEALED_LEN], VaultError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| VaultError::NoRandomness)?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(wrap));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: private,
                aad: CONTEXT,
            },
        )
        .map_err(|_| VaultError::Refused)?;

    let mut out = [0u8; SEALED_LEN];
    out[..NONCE_LEN].copy_from_slice(&nonce);
    out[NONCE_LEN..].copy_from_slice(&sealed);

    Ok(out)
}

/// Opens a sealed key with the wrapping secret.
///
/// # Errors
///
/// Returns [`VaultError::WrongLength`] for a blob that is not a sealed key, and
/// [`VaultError::Refused`] when the tag does not verify — which is what a wrong password looks
/// like, and also what tampering looks like.
pub fn open(sealed: &[u8], wrap: &[u8; SECRET_LEN]) -> Result<[u8; KEY_LEN], VaultError> {
    if sealed.len() != SEALED_LEN {
        return Err(VaultError::WrongLength {
            actual: sealed.len(),
        });
    }

    let (nonce, body) = sealed.split_at(NONCE_LEN);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(wrap));
    let opened = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: CONTEXT,
            },
        )
        .map_err(|_| VaultError::Refused)?;

    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&opened);

    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::{SEALED_LEN, VaultError, open, seal};
    use crate::account::secret::SECRET_LEN;
    use crate::net::handshake::KEY_LEN;

    const WRAP: [u8; SECRET_LEN] = [3; SECRET_LEN];
    const KEY: [u8; KEY_LEN] = [42; KEY_LEN];

    #[test]
    fn a_key_that_was_sealed_comes_back() {
        let sealed = seal(&KEY, &WRAP).expect("seals");

        assert_eq!(open(&sealed, &WRAP).expect("opens"), KEY);
    }

    #[test]
    fn the_wrong_secret_does_not_open_it() {
        // Which is what a wrong password is, by the time it reaches here.
        let sealed = seal(&KEY, &WRAP).expect("seals");

        assert_eq!(
            open(&sealed, &[4; SECRET_LEN]).unwrap_err(),
            VaultError::Refused
        );
    }

    #[test]
    fn the_key_is_not_in_the_blob() {
        // The obvious mistake, and one that would look like it worked.
        let sealed = seal(&KEY, &WRAP).expect("seals");

        assert!(
            !sealed.windows(KEY_LEN).any(|window| window == KEY),
            "the private key is sitting in the sealed blob"
        );
    }

    #[test]
    fn sealing_the_same_key_twice_gives_different_bytes() {
        // The server sees these. Identical blobs would tell it that a password had been
        // changed back to one it had already stored.
        let first = seal(&KEY, &WRAP).expect("seals");
        let again = seal(&KEY, &WRAP).expect("seals");

        assert_ne!(first, again);
        assert_eq!(open(&first, &WRAP).unwrap(), open(&again, &WRAP).unwrap());
    }

    #[test]
    fn a_single_altered_byte_is_refused() {
        // Every byte, because a blob that opened despite being changed would mean the server
        // could hand back a key of its choosing.
        let sealed = seal(&KEY, &WRAP).expect("seals");

        for index in 0..SEALED_LEN {
            let mut tampered = sealed;
            tampered[index] ^= 1;

            assert_eq!(
                open(&tampered, &WRAP).unwrap_err(),
                VaultError::Refused,
                "byte {index} could be changed without the seal noticing"
            );
        }
    }

    #[test]
    fn a_blob_of_the_wrong_size_is_refused_by_length() {
        for length in [0usize, 1, SEALED_LEN - 1, SEALED_LEN + 1] {
            assert_eq!(
                open(&vec![0u8; length], &WRAP).unwrap_err(),
                VaultError::WrongLength { actual: length }
            );
        }
    }
}
