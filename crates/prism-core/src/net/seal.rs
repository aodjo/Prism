//! Encrypting packets on the way out and refusing forgeries on the way in.
//!
//! Every packet on the wire is sealed with AES-256-GCM. That is not only about
//! confidentiality: a remote desktop session carries keystrokes and accepts injected input,
//! so an unauthenticated flow is one where anyone who can reach the port can type on the
//! host. Authentication is the part that matters most, and GCM gives both.
//!
//! # What travels in the clear
//!
//! Only the nonce counter. The channel tag and every header sit inside the seal, so an
//! observer learns a packet's size and nothing else — not whether it is video, input, or a
//! keystroke's acknowledgement.
//!
//! # Nonces
//!
//! A counter, one per direction, never reused. Reusing a nonce with GCM is not a weakness
//! but a break: it leaks the XOR of two plaintexts and, worse, the authentication key. So
//! the counter is never allowed to wrap — the session ends first, at a point no real session
//! could reach.
//!
//! # Replay
//!
//! The tag proves a packet was written by the peer; it does not prove the peer wrote it
//! *now*. Without a replay window an attacker could record a mouse click and post it back
//! whenever they liked. A sliding window of accepted counters closes that, and it has to
//! slide rather than insist on order, because UDP reorders constantly and a receiver that
//! demanded monotonic counters would throw away good packets on every path with jitter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce, Tag};

use crate::net::packet::{MAX_PLAINTEXT_SIZE, SEAL_OVERHEAD};

/// Bytes of the nonce counter that travel ahead of the ciphertext.
///
/// Also where an opened packet's plaintext begins, which is what lets a caller open in place
/// and hand out a borrow of the same buffer.
pub const COUNTER_LEN: usize = 8;

/// Bytes of authentication tag GCM appends.
pub const TAG_LEN: usize = 16;

/// How many counters behind the newest one are still accepted.
///
/// Sixty-four covers far more reordering than any path this is meant for produces — the
/// measured cross-machine runs reorder by a handful at most — while staying a single word
/// of state per direction.
const REPLAY_WINDOW: u64 = 64;

/// Highest counter a session will use before refusing to send.
///
/// Two to the sixty-four is unreachable, but stopping short of the wrap is what makes
/// "never reused" a fact rather than an assumption. At a million packets a second this is
/// still half a million years.
const MAX_COUNTER: u64 = u64::MAX - 1;

/// Reason a packet could not be sealed or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    /// The plaintext is larger than a sealed packet may carry.
    #[error(
        "plaintext is {actual} bytes, exceeds the {MAX_PLAINTEXT_SIZE} a sealed packet carries"
    )]
    TooLarge {
        /// Length that was rejected.
        actual: usize,
    },

    /// The caller's buffer cannot hold the sealed packet.
    #[error("buffer is {actual} bytes, needs {needed}")]
    BufferTooSmall {
        /// Buffer length supplied.
        actual: usize,
        /// Buffer length required.
        needed: usize,
    },

    /// The packet is too short to be a sealed one.
    #[error("packet is {actual} bytes, shorter than the {SEAL_OVERHEAD} a seal costs")]
    TooShort {
        /// Length that was rejected.
        actual: usize,
    },

    /// The tag did not verify.
    ///
    /// Either the packet was altered or it was not written by the peer. There is no way to
    /// tell which and no reason to care: both are refused identically, and no detail about
    /// which check failed is reported, because that detail is what an attacker probes with.
    #[error("packet failed authentication")]
    NotAuthentic,

    /// The counter has already been accepted, or is too old to judge.
    ///
    /// Reported separately from a failed tag because it is operationally different: a
    /// replay is an attack or a duplicate, while a bad tag is a forgery or corruption.
    #[error("packet counter {counter} is a replay or older than the window")]
    Replay {
        /// The counter that was refused.
        counter: u64,
    },

    /// The sending counter reached its ceiling.
    #[error("the nonce counter is exhausted, so this session must end")]
    Exhausted,
}

/// Seals outgoing packets for one direction.
///
/// One per direction, each with its own key and its own counter. Sharing a key between
/// directions would let each side's counters collide with the other's, which is nonce reuse
/// by another name.
///
/// The counter is atomic because one direction is written by more than one thread: on the
/// host, video leaves the encoder thread while clock synchronisation replies leave the return
/// path thread, and both are the same direction under the same key. A mutex would serialise a
/// hot path against a once-a-second one; an atomic increment costs a fraction of the
/// encryption it precedes. Use [`Sealer::split`] to hand the second thread its own handle.
pub struct Sealer {
    cipher: Aes256Gcm,
    counter: Arc<AtomicU64>,
}

impl Sealer {
    /// Creates a sealer from a thirty-two byte key.
    #[must_use]
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns a second handle onto the same direction.
    ///
    /// The two handles share one counter, so no nonce is ever issued twice however the sends
    /// interleave. That sharing is the entire point: two independent sealers on one key would
    /// both start at zero and reuse every nonce, which under GCM leaks the authentication key
    /// rather than merely weakening the cipher.
    #[must_use]
    pub fn split(&self) -> Self {
        Self {
            cipher: self.cipher.clone(),
            counter: Arc::clone(&self.counter),
        }
    }

    /// Seals `plaintext` into `out` and returns how many bytes were written.
    ///
    /// The sealed packet is the counter, then the ciphertext, then the tag.
    ///
    /// # Errors
    ///
    /// Returns [`SealError::TooLarge`] if the plaintext exceeds what a packet may carry,
    /// [`SealError::BufferTooSmall`] if `out` cannot hold the result, and
    /// [`SealError::Exhausted`] once the counter reaches its ceiling.
    pub fn seal(&mut self, plaintext: &[u8], out: &mut [u8]) -> Result<usize, SealError> {
        if plaintext.len() > MAX_PLAINTEXT_SIZE {
            return Err(SealError::TooLarge {
                actual: plaintext.len(),
            });
        }

        let needed = plaintext.len() + SEAL_OVERHEAD;
        if out.len() < needed {
            return Err(SealError::BufferTooSmall {
                actual: out.len(),
                needed,
            });
        }

        // Reserved with one atomic step so that two handles onto this direction cannot be
        // handed the same counter, and so that reaching the ceiling refuses rather than
        // wrapping past it.
        let counter = self
            .counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current < MAX_COUNTER).then_some(current + 1)
            })
            .map_err(|_| SealError::Exhausted)?;

        out[..COUNTER_LEN].copy_from_slice(&counter.to_le_bytes());
        let body = COUNTER_LEN..COUNTER_LEN + plaintext.len();
        out[body.clone()].copy_from_slice(plaintext);

        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce_for(counter), &[], &mut out[body.clone()])
            .map_err(|_| SealError::NotAuthentic)?;

        out[body.end..body.end + TAG_LEN].copy_from_slice(&tag);

        Ok(needed)
    }

    /// Returns how many packets this direction has sent.
    ///
    /// Counts every handle onto the direction, not only this one, because it is the counter
    /// itself that is being reported.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}

/// Opens incoming packets for one direction, refusing forgeries and replays.
pub struct Opener {
    cipher: Aes256Gcm,
    newest: u64,
    /// Bit `n` set means the counter `newest - 1 - n` has already been accepted.
    seen: u64,
    started: bool,
    forged: u64,
    replayed: u64,
}

impl Opener {
    /// Creates an opener from a thirty-two byte key.
    #[must_use]
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
            newest: 0,
            seen: 0,
            started: false,
            forged: 0,
            replayed: 0,
        }
    }

    /// Opens `packet` in place and returns the plaintext.
    ///
    /// The counter is checked against the replay window **after** the tag verifies, never
    /// before. Rejecting on the counter first would let anyone who can guess a counter
    /// disturb the window without holding the key.
    ///
    /// # Errors
    ///
    /// Returns [`SealError::TooShort`] for a packet that cannot be one,
    /// [`SealError::NotAuthentic`] if the tag does not verify, and [`SealError::Replay`] if
    /// the counter has been seen or has fallen out of the window.
    pub fn open<'a>(&mut self, packet: &'a mut [u8]) -> Result<&'a [u8], SealError> {
        if packet.len() < SEAL_OVERHEAD {
            return Err(SealError::TooShort {
                actual: packet.len(),
            });
        }

        let mut counter_bytes = [0u8; COUNTER_LEN];
        counter_bytes.copy_from_slice(&packet[..COUNTER_LEN]);
        let counter = u64::from_le_bytes(counter_bytes);

        let body_end = packet.len() - TAG_LEN;
        let mut tag_bytes = [0u8; TAG_LEN];
        tag_bytes.copy_from_slice(&packet[body_end..]);

        let body = &mut packet[COUNTER_LEN..body_end];
        if self
            .cipher
            .decrypt_in_place_detached(&nonce_for(counter), &[], body, Tag::from_slice(&tag_bytes))
            .is_err()
        {
            self.forged += 1;
            return Err(SealError::NotAuthentic);
        }

        if !self.accept(counter) {
            self.replayed += 1;
            return Err(SealError::Replay { counter });
        }

        Ok(&packet[COUNTER_LEN..body_end])
    }

    /// Records a counter, returning whether it was new.
    fn accept(&mut self, counter: u64) -> bool {
        if !self.started {
            self.started = true;
            self.newest = counter;
            self.seen = 0;
            return true;
        }

        if counter > self.newest {
            let advance = counter - self.newest;
            self.seen = if advance >= REPLAY_WINDOW {
                0
            } else {
                (self.seen << advance) | (1 << (advance - 1))
            };
            self.newest = counter;
            return true;
        }

        let behind = self.newest - counter;
        if behind == 0 || behind > REPLAY_WINDOW {
            return false;
        }

        let bit = 1u64 << (behind - 1);
        if self.seen & bit != 0 {
            return false;
        }

        self.seen |= bit;
        true
    }

    /// Returns how many packets failed authentication.
    ///
    /// Non-zero means either corruption or someone writing packets at this port. Worth
    /// surfacing rather than counting silently: on a healthy path it is exactly zero.
    #[must_use]
    pub fn forged(&self) -> u64 {
        self.forged
    }

    /// Returns how many authentic packets were refused as replays.
    #[must_use]
    pub fn replayed(&self) -> u64 {
        self.replayed
    }
}

/// Builds the nonce for a counter.
///
/// Ninety-six bits, of which the counter fills the low sixty-four and the rest are zero.
/// The keys are per-direction and per-session, so a counter is all that is needed to keep
/// every nonce under a key distinct.
fn nonce_for(counter: u64) -> Nonce<aes_gcm::aes::cipher::consts::U12> {
    let mut bytes = [0u8; 12];
    bytes[..COUNTER_LEN].copy_from_slice(&counter.to_le_bytes());

    *Nonce::from_slice(&bytes)
}

impl core::fmt::Debug for Sealer {
    /// Describes the sealer without exposing its key.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sealer")
            .field("sent", &self.sent())
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for Opener {
    /// Describes the opener without exposing its key.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Opener")
            .field("newest", &self.newest)
            .field("forged", &self.forged)
            .field("replayed", &self.replayed)
            .finish_non_exhaustive()
    }
}
