//! Permission to be relayed, in a form the server carrying the traffic can check by itself.
//!
//! Relaying costs the operator bandwidth somebody pays for, so an account can be allowed it or
//! not. The awkward part is where that decision is made and where it is enforced: accounts live
//! on one server and the relay is on another, and those two never speak.
//!
//! Asking would be the obvious answer and is the wrong one. A signalling server that had to
//! call the account server before relaying would stop relaying whenever accounts were
//! unreachable — turning an outage of the one thing no session touches into an outage of the
//! fallback that rescues sessions. So the permission travels with the peer instead: the account
//! server mints a grant at sign-in, the peer presents it when it asks to be relayed, and the
//! relay checks it against a secret the two share. No lookup, no round trip, and nothing to be
//! down.
//!
//! The tag is a keyed hash rather than a signature. A signature would let the relay verify
//! without being able to mint, which is the better property — but both servers here belong to
//! one operator, so the difference buys little against the cost of a second key algorithm in a
//! crate that has none. What it does buy is worth stating: **anybody who reads the relay's
//! secret can issue grants**, so it is configured the way a secret is and not written down
//! beside the address.
//!
//! What the tag covers is the whole of what it means: this key, until this moment. Binding the
//! key is what stops a grant being passed to a friend; binding the expiry is what stops one
//! being kept forever.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::net::handshake::KEY_LEN;

/// How many bytes of the keyed hash travel.
///
/// Half of SHA-256. What an attacker gains by guessing one is a relay session, and a hundred and
/// twenty-eight bits is far past the point where that is worth attempting; the rest is datagram
/// space in a protocol that fits in one.
pub const TAG_LEN: usize = 16;

/// How long a grant lasts.
///
/// Long enough that somebody who signed in this morning can still be relayed this evening, short
/// enough that revoking an account's relay stops mattering within a day rather than never. The
/// account server decides the actual expiry; this is what it uses when nothing says otherwise.
pub const GRANT_SECONDS: u64 = 24 * 60 * 60;

/// Separates this use of the secret from any other, so one can never be replayed as another.
const DOMAIN: &[u8] = b"prism-relay-grant-v1";

/// Leave for clocks that disagree.
///
/// Two machines that have never spoken agree on the time to within about this. Refusing a grant
/// because the relay's clock runs a minute fast would be a session lost to something neither end
/// can see.
const SKEW_SECONDS: u64 = 60;

/// Permission for one machine to be relayed, until a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// When it stops being good, as seconds since the Unix epoch.
    pub expires_unix: u64,
    /// The keyed hash over the key and that moment.
    pub tag: [u8; TAG_LEN],
}

impl Grant {
    /// A grant that is not one.
    ///
    /// What a peer presents when it has none — an account that may not relay, or a machine that
    /// paired without an account at all. A relay configured to require grants refuses it, and
    /// one that is not ignores the whole question.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            expires_unix: 0,
            tag: [0u8; TAG_LEN],
        }
    }

    /// Whether this is the absence of a grant rather than one.
    #[must_use]
    pub fn is_none(&self) -> bool {
        *self == Self::none()
    }
}

/// Computes the tag a grant for this key and moment should carry.
fn tag_for(secret: &[u8], key: &[u8; KEY_LEN], expires_unix: u64) -> [u8; TAG_LEN] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .expect("HMAC takes a key of any length");

    mac.update(DOMAIN);
    mac.update(key);
    mac.update(&expires_unix.to_be_bytes());

    let full = mac.finalize().into_bytes();
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&full[..TAG_LEN]);

    tag
}

/// Issues a grant, which is what the account server does when somebody signs in.
///
/// Present here rather than only in the server that uses it so that the two sides cannot drift:
/// what mints and what checks are the same function read twice.
#[must_use]
pub fn issue(secret: &[u8], key: &[u8; KEY_LEN], expires_unix: u64) -> Grant {
    Grant {
        expires_unix,
        tag: tag_for(secret, key, expires_unix),
    }
}

/// Whether a grant permits this key to be relayed now.
///
/// The comparison is constant time, so a relay answering many attempts does not leak how much of
/// a guess was right through how long it took to say no.
#[must_use]
pub fn permits(secret: &[u8], key: &[u8; KEY_LEN], grant: &Grant, now_unix: u64) -> bool {
    use subtle::ConstantTimeEq;

    // Checked before the hash rather than after, because an expired grant is a real one and
    // there is nothing to be learned from how long it takes to reject it.
    if grant.expires_unix.saturating_add(SKEW_SECONDS) < now_unix {
        return false;
    }

    tag_for(secret, key, grant.expires_unix)
        .ct_eq(&grant.tag)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a secret two servers share";
    const NOW: u64 = 1_800_000_000;

    fn key(byte: u8) -> [u8; KEY_LEN] {
        [byte; KEY_LEN]
    }

    #[test]
    fn a_grant_permits_the_key_it_was_issued_for() {
        let grant = issue(SECRET, &key(1), NOW + GRANT_SECONDS);

        assert!(permits(SECRET, &key(1), &grant, NOW));
    }

    #[test]
    fn a_grant_does_not_travel_to_another_machine() {
        // The whole point of binding the key: a grant handed to a friend is a grant that does
        // nothing for them.
        let grant = issue(SECRET, &key(1), NOW + GRANT_SECONDS);

        assert!(!permits(SECRET, &key(2), &grant, NOW));
    }

    #[test]
    fn a_grant_stops_being_good() {
        let grant = issue(SECRET, &key(1), NOW);

        assert!(permits(SECRET, &key(1), &grant, NOW));
        assert!(!permits(SECRET, &key(1), &grant, NOW + SKEW_SECONDS + 1));
    }

    #[test]
    fn a_clock_that_runs_a_little_fast_does_not_lose_a_session() {
        let grant = issue(SECRET, &key(1), NOW);

        assert!(permits(SECRET, &key(1), &grant, NOW + SKEW_SECONDS - 1));
    }

    #[test]
    fn a_grant_from_another_secret_is_refused() {
        let grant = issue(b"someone else's secret", &key(1), NOW + GRANT_SECONDS);

        assert!(!permits(SECRET, &key(1), &grant, NOW));
    }

    #[test]
    fn the_expiry_cannot_be_moved_without_the_secret() {
        let mut grant = issue(SECRET, &key(1), NOW);
        grant.expires_unix = NOW + GRANT_SECONDS;

        assert!(!permits(SECRET, &key(1), &grant, NOW));
    }

    #[test]
    fn the_absence_of_a_grant_is_not_a_grant() {
        assert!(Grant::none().is_none());
        assert!(!permits(SECRET, &key(1), &Grant::none(), NOW));
    }
}
