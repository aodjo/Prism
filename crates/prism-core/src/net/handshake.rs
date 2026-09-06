//! Agreeing on session keys with a peer whose identity is already known.
//!
//! The pattern is Noise_IK. `I` is the initiator sending its static key; `K` is that the
//! initiator already **knows** the responder's static key, which it does because pairing put
//! it there. That combination is what buys a one round trip handshake with forward secrecy:
//! the client can encrypt to the host in its very first packet, and the host answers with the
//! second. Anything requiring more round trips would put an extra RTT in front of every
//! reconnect, and reconnects happen on every network change.
//!
//! # What each side proves
//!
//! The initiator proves it holds the private half of the static key it sends. The responder
//! proves the same by being the only party able to read the first message at all. Neither
//! proof is worth anything on its own — the caller has to check the key it got back is the
//! key pairing recorded. [`Handshake::peer_static`] is what it checks, and refusing an
//! unknown key there is the entire authentication story.
//!
//! # Why the raw keys come out
//!
//! Noise's own transport mode keeps a nonce counter it increments per message, which assumes
//! messages arrive in the order they were sent. On UDP they do not. So the split is taken
//! raw and handed to [`crate::net::seal`], which puts the counter on the wire and keeps a
//! replay window — the same guarantee, made to survive reordering.
//!
//! # What this deliberately does not do
//!
//! Nothing here rate limits. A responder performs a Diffie-Hellman before it can tell a
//! genuine first message from a random one, so an unsolicited flood costs it real work. The
//! answer is a cookie exchange in front of the handshake, and it belongs at the transport
//! layer where the peer's address is known, not here.

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use crate::net::seal::{Opener, Sealer};

/// The Noise pattern and primitives, fixed rather than negotiated.
///
/// Negotiating a cipher suite is how protocols acquire downgrade attacks. There is one suite
/// and a version byte in the prologue; a peer that wants a different one is a peer running a
/// different version, and it fails to handshake rather than being talked down to a weaker
/// one.
const PATTERN: &str = "Noise_IK_25519_AESGCM_SHA256";

/// Bound into the handshake transcript so the version cannot be stripped.
///
/// The prologue is mixed into the hash before any key material, so two peers that disagree
/// about it derive different keys and simply fail. That is what stops an attacker replaying a
/// handshake at a future version whose meaning has changed.
const PROLOGUE: &[u8] = b"prism-handshake-v1";

/// Length of a static or ephemeral public key.
pub const KEY_LEN: usize = 32;

/// Bytes the first handshake message costs beyond its payload.
///
/// The initiator's ephemeral public key, its static public key under the first encryption,
/// and the payload's own tag.
pub const INIT_OVERHEAD: usize = 96;

/// Bytes the second handshake message costs beyond its payload.
///
/// The responder's ephemeral public key and the payload's tag.
pub const RESPONSE_OVERHEAD: usize = 48;

/// Largest payload either handshake message will carry.
///
/// Both messages have to fit in one datagram under the same PMTU floor the rest of the
/// protocol respects, because a fragmented handshake is a handshake that fails on exactly the
/// paths hardest to debug.
pub const MAX_HANDSHAKE_PAYLOAD: usize = 1024;

/// Reason a handshake could not proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// A key was not thirty-two bytes.
    #[error("a key must be {KEY_LEN} bytes, got {actual}")]
    BadKeyLength {
        /// Length that was rejected.
        actual: usize,
    },

    /// The payload exceeds what a handshake message carries.
    #[error("handshake payload is {actual} bytes, exceeds {MAX_HANDSHAKE_PAYLOAD}")]
    PayloadTooLarge {
        /// Length that was rejected.
        actual: usize,
    },

    /// The caller's buffer cannot hold the message.
    #[error("buffer is {actual} bytes, needs at least {needed}")]
    BufferTooSmall {
        /// Buffer length supplied.
        actual: usize,
        /// Buffer length required.
        needed: usize,
    },

    /// A message arrived when it was this side's turn to send, or after the handshake ended.
    #[error("the handshake is not expecting a message now")]
    OutOfTurn,

    /// The message did not authenticate.
    ///
    /// For a responder this is the common case rather than an alarming one: any packet at all
    /// arriving at the port lands here. It carries no detail, because the detail of which
    /// step failed is what an attacker learns from.
    #[error("handshake message failed to authenticate")]
    NotAuthentic,

    /// Session keys were asked for before the handshake finished.
    #[error("the handshake has not finished, so there are no session keys yet")]
    Unfinished,

    /// The Noise implementation refused the configuration.
    ///
    /// A build-time mistake rather than anything a peer can cause.
    #[error("the handshake could not be built")]
    Misconfigured,
}

/// A long-term key pair identifying one machine.
///
/// Generated once, on first run, and kept for the life of the installation. The public half
/// is what pairing shows the other side and what it pins; changing it is indistinguishable
/// from being a different machine, which is the point.
#[derive(Clone)]
pub struct Identity {
    private: [u8; KEY_LEN],
    public: [u8; KEY_LEN],
}

impl Identity {
    /// Generates a fresh identity.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::Misconfigured`] if the platform has no usable randomness,
    /// which is a condition no session should start under.
    pub fn generate() -> Result<Self, HandshakeError> {
        let builder = Builder::new(params());
        let pair = builder
            .generate_keypair()
            .map_err(|_| HandshakeError::Misconfigured)?;

        Ok(Self {
            private: to_key(&pair.private)?,
            public: to_key(&pair.public)?,
        })
    }

    /// Rebuilds an identity from a stored private key.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::Misconfigured`] if the key does not yield a public half.
    pub fn from_private(private: &[u8; KEY_LEN]) -> Result<Self, HandshakeError> {
        // Deriving the public half rather than storing it means a truncated or edited key
        // file fails the handshake instead of half working.
        let public = public_from_private(private)?;

        Ok(Self {
            private: *private,
            public,
        })
    }

    /// Returns the public key this machine is known by.
    #[must_use]
    pub fn public(&self) -> &[u8; KEY_LEN] {
        &self.public
    }

    /// Returns the private key, for persisting it.
    ///
    /// The caller is responsible for storing this somewhere only this user can read.
    #[must_use]
    pub fn private(&self) -> &[u8; KEY_LEN] {
        &self.private
    }
}

impl core::fmt::Debug for Identity {
    /// Describes the identity by its public half only.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Identity")
            .field("public", &hex(&self.public))
            .finish_non_exhaustive()
    }
}

/// The keys a finished handshake produced, one direction each.
pub struct Session {
    /// Seals packets sent to the peer.
    pub sealer: Sealer,
    /// Opens packets received from the peer.
    pub opener: Opener,
    /// The peer's static public key, as it proved during the handshake.
    pub peer_static: [u8; KEY_LEN],
}

impl core::fmt::Debug for Session {
    /// Describes the session by its peer, never its keys.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("peer_static", &hex(&self.peer_static))
            .finish_non_exhaustive()
    }
}

/// One side of a handshake in progress.
pub struct Handshake {
    state: HandshakeState,
    initiator: bool,
}

impl Handshake {
    /// Starts the side that speaks first, to a peer whose static key is already known.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::Misconfigured`] if the keys are not valid Curve25519 keys.
    pub fn initiator(
        identity: &Identity,
        peer_static: &[u8; KEY_LEN],
    ) -> Result<Self, HandshakeError> {
        let state = Builder::new(params())
            .prologue(PROLOGUE)
            .and_then(|builder| builder.local_private_key(&identity.private))
            .and_then(|builder| builder.remote_public_key(peer_static))
            .and_then(Builder::build_initiator)
            .map_err(|_| HandshakeError::Misconfigured)?;

        Ok(Self {
            state,
            initiator: true,
        })
    }

    /// Starts the side that answers.
    ///
    /// The responder does not name the peer it expects. It cannot: the whole point of `IK` is
    /// that the initiator's identity arrives inside the first message. The caller checks
    /// [`Handshake::peer_static`] once that message is read.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::Misconfigured`] if the identity is not a valid key.
    pub fn responder(identity: &Identity) -> Result<Self, HandshakeError> {
        let state = Builder::new(params())
            .prologue(PROLOGUE)
            .and_then(|builder| builder.local_private_key(&identity.private))
            .and_then(Builder::build_responder)
            .map_err(|_| HandshakeError::Misconfigured)?;

        Ok(Self {
            state,
            initiator: false,
        })
    }

    /// Writes this side's next handshake message into `out`, returning its length.
    ///
    /// The payload rides inside the message. For the initiator it is encrypted to the
    /// responder's static key but not yet forward secret, so it may carry what the session
    /// needs to negotiate and must not carry a secret worth more than the static key. For the
    /// responder it is fully protected.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::PayloadTooLarge`], [`HandshakeError::BufferTooSmall`], or
    /// [`HandshakeError::OutOfTurn`] if it is the peer's turn to speak.
    pub fn write_message(
        &mut self,
        payload: &[u8],
        out: &mut [u8],
    ) -> Result<usize, HandshakeError> {
        if payload.len() > MAX_HANDSHAKE_PAYLOAD {
            return Err(HandshakeError::PayloadTooLarge {
                actual: payload.len(),
            });
        }

        let needed = payload.len() + self.outgoing_overhead();
        if out.len() < needed {
            return Err(HandshakeError::BufferTooSmall {
                actual: out.len(),
                needed,
            });
        }

        if !self.state.is_my_turn() || self.state.is_handshake_finished() {
            return Err(HandshakeError::OutOfTurn);
        }

        self.state
            .write_message(payload, out)
            .map_err(|_| HandshakeError::OutOfTurn)
    }

    /// Reads a handshake message from the peer, writing its payload into `payload`.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::OutOfTurn`] if it is this side's turn to speak,
    /// [`HandshakeError::BufferTooSmall`] if the payload buffer is too small, and
    /// [`HandshakeError::NotAuthentic`] for anything that does not decrypt — which is what
    /// every unsolicited packet arriving at the port produces.
    pub fn read_message(
        &mut self,
        message: &[u8],
        payload: &mut [u8],
    ) -> Result<usize, HandshakeError> {
        if self.state.is_my_turn() || self.state.is_handshake_finished() {
            return Err(HandshakeError::OutOfTurn);
        }

        if payload.len() < MAX_HANDSHAKE_PAYLOAD {
            return Err(HandshakeError::BufferTooSmall {
                actual: payload.len(),
                needed: MAX_HANDSHAKE_PAYLOAD,
            });
        }

        self.state
            .read_message(message, payload)
            .map_err(|_| HandshakeError::NotAuthentic)
    }

    /// Returns the peer's static public key once a message carrying it has been read.
    ///
    /// For a responder this is available after the first message and is the value the caller
    /// must check against what pairing recorded. For an initiator it is known from the start.
    #[must_use]
    pub fn peer_static(&self) -> Option<[u8; KEY_LEN]> {
        self.state
            .get_remote_static()
            .and_then(|key| key.try_into().ok())
    }

    /// Returns whether both messages have been exchanged.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// Returns whether this side sends the next message.
    #[must_use]
    pub fn is_my_turn(&self) -> bool {
        self.state.is_my_turn()
    }

    /// Consumes the finished handshake and returns the session keys.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::Unfinished`] if called before both messages were exchanged,
    /// which would otherwise hand out keys the peer has not authenticated.
    pub fn into_session(mut self) -> Result<Session, HandshakeError> {
        if !self.state.is_handshake_finished() {
            return Err(HandshakeError::Unfinished);
        }

        let peer_static = self.peer_static().ok_or(HandshakeError::Unfinished)?;

        // Noise splits into two keys in a fixed order: the first is the initiator's sending
        // key. Each side has to pick the half matching its role, and getting this backwards
        // produces a handshake that succeeds and a session where nothing decrypts.
        let (first, second) = self.state.dangerously_get_raw_split();
        let (send, receive) = if self.initiator {
            (first, second)
        } else {
            (second, first)
        };

        Ok(Session {
            sealer: Sealer::new(&send),
            opener: Opener::new(&receive),
            peer_static,
        })
    }

    /// Bytes of overhead the next outgoing message carries.
    fn outgoing_overhead(&self) -> usize {
        if self.initiator {
            INIT_OVERHEAD
        } else {
            RESPONSE_OVERHEAD
        }
    }
}

impl core::fmt::Debug for Handshake {
    /// Describes the handshake's progress without exposing any key material.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Handshake")
            .field("initiator", &self.initiator)
            .field("finished", &self.state.is_handshake_finished())
            .finish_non_exhaustive()
    }
}

/// Parses the fixed pattern.
///
/// # Panics
///
/// Panics if [`PATTERN`] is not a pattern Noise understands, which is a constant in this file
/// and so a build error rather than anything reachable at run time.
fn params() -> NoiseParams {
    PATTERN.parse().expect("the pattern is a constant")
}

/// Copies a slice into a key array.
fn to_key(bytes: &[u8]) -> Result<[u8; KEY_LEN], HandshakeError> {
    bytes.try_into().map_err(|_| HandshakeError::BadKeyLength {
        actual: bytes.len(),
    })
}

/// Derives the public half of a Curve25519 private key.
///
/// The public half is derived rather than stored alongside the private one so that a key file
/// which was truncated or edited fails immediately instead of producing a handshake that
/// half works.
fn public_from_private(private: &[u8; KEY_LEN]) -> Result<[u8; KEY_LEN], HandshakeError> {
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver, DefaultResolver};

    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or(HandshakeError::Misconfigured)?;

    dh.set(private);

    to_key(dh.pubkey())
}

/// Renders bytes as hex, for `Debug` output that names a key without revealing a secret one.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
