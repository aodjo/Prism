//! Turning a six digit number a person reads off a screen into two pinned public keys.
//!
//! Pairing happens once per pair of machines. After it, each side holds the other's static
//! key and [`crate::net::handshake`] does the rest forever; the number is never used again.
//!
//! # Why a PIN needs SPAKE2 rather than a hash
//!
//! Six digits is a million possibilities, which a laptop exhausts in well under a second.
//! Anything that lets an attacker check guesses offline — a hash of the PIN on the wire, a
//! key derived from it directly, a transcript that can be tested against — is therefore not
//! a secret at all. SPAKE2 is a password-authenticated key exchange: the only way to test a
//! guess is to run the protocol against the real host, which gets exactly **one** attempt
//! before the PIN is retired. A million guesses at one round trip each, against a host that
//! stops after the first wrong one, is not an attack.
//!
//! # The exchange
//!
//! Three messages, one round trip and a half. The client dials, because a person has just
//! typed a number into the client and is waiting.
//!
//! ```text
//! client → host   hello    the client's SPAKE2 element
//! host  → client  offer    the host's SPAKE2 element, then the host's static key, sealed
//! client → host   accept   the client's static key, sealed
//! ```
//!
//! There is no separate key confirmation step. The seal is the confirmation: a wrong PIN
//! produces a different key, the sealed part does not open, and the exchange ends there.
//!
//! # What the PIN protects and what it does not
//!
//! It proves the two machines are the two the person is standing between. It does not make
//! the resulting keys secret from that person, and it is not meant to: they own both
//! machines. Someone who watches the whole exchange without the PIN learns two public keys
//! and nothing else, which is what public keys are for.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use spake2::{Ed25519Group, Identity as Spake2Identity, Password, Spake2};

use crate::net::handshake::KEY_LEN;

/// Digits in a pairing code.
///
/// Six is what a person will read off a screen and type without resenting it. The security
/// does not come from the length — it comes from the host allowing one attempt.
pub const PIN_DIGITS: usize = 6;

/// Bytes in the SPAKE2 element each side sends.
const ELEMENT_LEN: usize = 33;

/// Bytes of authentication tag on the sealed halves.
const TAG_LEN: usize = 16;

/// Length of the client's opening message.
pub const HELLO_LEN: usize = ELEMENT_LEN;

/// Length of the host's answer: its element, then its sealed static key.
pub const OFFER_LEN: usize = ELEMENT_LEN + KEY_LEN + TAG_LEN;

/// Length of the client's closing message: its sealed static key.
pub const ACCEPT_LEN: usize = KEY_LEN + TAG_LEN;

/// Names the host in the SPAKE2 transcript, so the two roles cannot be swapped.
const HOST_ID: &[u8] = b"prism-pairing-host-v1";

/// Names the client in the SPAKE2 transcript.
const CLIENT_ID: &[u8] = b"prism-pairing-client-v1";

/// Separates the two directions' keys, so one direction's seal cannot open the other's.
const HOST_TO_CLIENT: &[u8] = b"prism-pairing-host-to-client";

/// Separates the two directions' keys.
const CLIENT_TO_HOST: &[u8] = b"prism-pairing-client-to-host";

/// Reason a pairing attempt could not continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    /// A message was not the length its step requires.
    #[error("a pairing message was {actual} bytes, expected {expected}")]
    BadLength {
        /// Length that arrived.
        actual: usize,
        /// Length that was required.
        expected: usize,
    },

    /// The caller's buffer cannot hold the message to be written.
    #[error("buffer is {actual} bytes, needs {needed}")]
    BufferTooSmall {
        /// Buffer length supplied.
        actual: usize,
        /// Buffer length required.
        needed: usize,
    },

    /// The peer's SPAKE2 element was not a valid one.
    ///
    /// Indistinguishable, deliberately, from a wrong PIN.
    #[error("the pairing attempt failed")]
    Failed,

    /// The PIN has already been used for an attempt.
    ///
    /// The whole security argument rests on this. A PIN that allowed a second guess would
    /// allow a millionth.
    #[error("this pairing code has been used and a new one is needed")]
    Spent,

    /// A pairing code was not six digits.
    #[error("a pairing code is {PIN_DIGITS} digits")]
    BadPin,
}

/// A freshly generated pairing code.
///
/// Displayed by the host and typed into the client. Single use: the host retires it whether
/// the attempt succeeded or failed.
#[derive(Clone, PartialEq, Eq)]
pub struct Pin([u8; PIN_DIGITS]);

impl Pin {
    /// Generates a code from the system's randomness.
    ///
    /// Rejection sampled rather than reduced modulo ten, because the modulo would make the
    /// low digits slightly likelier and shrink the space an attacker has to search.
    ///
    /// # Errors
    ///
    /// Returns [`PairingError::Failed`] if the platform has no usable randomness, which is a
    /// condition no pairing should proceed under.
    pub fn generate() -> Result<Self, PairingError> {
        let mut digits = [0u8; PIN_DIGITS];

        for digit in &mut digits {
            loop {
                let mut byte = [0u8; 1];
                getrandom::fill(&mut byte).map_err(|_| PairingError::Failed)?;

                // 250 is the largest multiple of ten at or below 256, so bytes above it are
                // drawn again rather than folded in and biasing the result.
                if byte[0] < 250 {
                    *digit = byte[0] % 10;
                    break;
                }
            }
        }

        Ok(Self(digits))
    }

    /// Parses a code a person typed.
    ///
    /// # Errors
    ///
    /// Returns [`PairingError::BadPin`] for anything that is not exactly six digits.
    pub fn parse(text: &str) -> Result<Self, PairingError> {
        let text = text.trim();
        if text.len() != PIN_DIGITS {
            return Err(PairingError::BadPin);
        }

        let mut digits = [0u8; PIN_DIGITS];
        for (slot, character) in digits.iter_mut().zip(text.chars()) {
            *slot = character.to_digit(10).ok_or(PairingError::BadPin)? as u8;
        }

        Ok(Self(digits))
    }

    /// Renders the code for display.
    #[must_use]
    pub fn to_display(&self) -> String {
        self.0.iter().map(|digit| (b'0' + digit) as char).collect()
    }

    /// Returns the bytes SPAKE2 mixes in.
    fn as_password(&self) -> Password {
        Password::new(self.to_display().as_bytes())
    }
}

impl core::fmt::Debug for Pin {
    /// Describes a code without printing it.
    ///
    /// A PIN is short-lived but it is still the secret the whole exchange rests on, and the
    /// places a `Debug` ends up — a log file, an error report — are exactly the places it
    /// should not be.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Pin(******)")
    }
}

/// The host's side of a pairing exchange.
///
/// Holds the code until one attempt has been made, then refuses.
pub struct PairingHost {
    pin: Option<Pin>,
    identity: [u8; KEY_LEN],
    state: Option<(Spake2<Ed25519Group>, Vec<u8>)>,
    keys: Option<(Key, Key)>,
}

impl PairingHost {
    /// Starts a pairing window for `pin`, offering `identity` as this machine's static key.
    #[must_use]
    pub fn new(pin: Pin, identity: [u8; KEY_LEN]) -> Self {
        Self {
            pin: Some(pin),
            identity,
            state: None,
            keys: None,
        }
    }

    /// Answers the client's opening message, writing the offer into `out`.
    ///
    /// Spends the code: whatever happens next, this host will not accept another opening
    /// message until it is given a new one.
    ///
    /// # Errors
    ///
    /// Returns [`PairingError::Spent`] if the code has already been used,
    /// [`PairingError::BadLength`] or [`PairingError::BufferTooSmall`] for a malformed call,
    /// and [`PairingError::Failed`] if the client's element is not a valid one.
    pub fn answer(&mut self, hello: &[u8], out: &mut [u8]) -> Result<usize, PairingError> {
        if hello.len() != HELLO_LEN {
            return Err(PairingError::BadLength {
                actual: hello.len(),
                expected: HELLO_LEN,
            });
        }
        if out.len() < OFFER_LEN {
            return Err(PairingError::BufferTooSmall {
                actual: out.len(),
                needed: OFFER_LEN,
            });
        }

        let pin = self.pin.take().ok_or(PairingError::Spent)?;

        let (state, element) = Spake2::<Ed25519Group>::start_a(
            &pin.as_password(),
            &Spake2Identity::new(HOST_ID),
            &Spake2Identity::new(CLIENT_ID),
        );

        let shared = state.finish(hello).map_err(|_| PairingError::Failed)?;
        let keys = directional_keys(&shared);

        out[..ELEMENT_LEN].copy_from_slice(&element);
        seal_key(&keys.0, &self.identity, &mut out[ELEMENT_LEN..OFFER_LEN])?;

        self.state = None;
        self.keys = Some(keys);

        Ok(OFFER_LEN)
    }

    /// Reads the client's closing message and returns the key it carried.
    ///
    /// # Errors
    ///
    /// Returns [`PairingError::Failed`] if the message does not open, which is what a wrong
    /// PIN produces, and [`PairingError::BadLength`] for a malformed one.
    pub fn accept(&mut self, accept: &[u8]) -> Result<[u8; KEY_LEN], PairingError> {
        if accept.len() != ACCEPT_LEN {
            return Err(PairingError::BadLength {
                actual: accept.len(),
                expected: ACCEPT_LEN,
            });
        }

        let (_, client_to_host) = self.keys.as_ref().ok_or(PairingError::Failed)?;

        open_key(client_to_host, accept)
    }
}

impl core::fmt::Debug for PairingHost {
    /// Describes the host's progress without printing the code or any key.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PairingHost")
            .field("code_available", &self.pin.is_some())
            .field("answered", &self.keys.is_some())
            .finish_non_exhaustive()
    }
}

/// The client's side of a pairing exchange.
pub struct PairingClient {
    identity: [u8; KEY_LEN],
    state: Option<Spake2<Ed25519Group>>,
    hello: [u8; HELLO_LEN],
}

impl PairingClient {
    /// Starts an attempt with the code the person typed.
    #[must_use]
    pub fn new(pin: &Pin, identity: [u8; KEY_LEN]) -> Self {
        let (state, element) = Spake2::<Ed25519Group>::start_b(
            &pin.as_password(),
            &Spake2Identity::new(HOST_ID),
            &Spake2Identity::new(CLIENT_ID),
        );

        let mut hello = [0u8; HELLO_LEN];
        hello.copy_from_slice(&element);

        Self {
            identity,
            state: Some(state),
            hello,
        }
    }

    /// Returns the opening message to send, and to send again if no answer arrives.
    #[must_use]
    pub fn hello(&self) -> &[u8] {
        &self.hello
    }

    /// Reads the host's offer and writes the closing message into `out`.
    ///
    /// Returns the host's static key alongside the length written.
    ///
    /// # Errors
    ///
    /// Returns [`PairingError::Failed`] if the offer does not open — which is what a wrong
    /// PIN produces, and what an impostor answering in the host's place produces too.
    pub fn finish(
        &mut self,
        offer: &[u8],
        out: &mut [u8],
    ) -> Result<([u8; KEY_LEN], usize), PairingError> {
        if offer.len() != OFFER_LEN {
            return Err(PairingError::BadLength {
                actual: offer.len(),
                expected: OFFER_LEN,
            });
        }
        if out.len() < ACCEPT_LEN {
            return Err(PairingError::BufferTooSmall {
                actual: out.len(),
                needed: ACCEPT_LEN,
            });
        }

        let state = self.state.take().ok_or(PairingError::Spent)?;
        let shared = state
            .finish(&offer[..ELEMENT_LEN])
            .map_err(|_| PairingError::Failed)?;
        let (host_to_client, client_to_host) = directional_keys(&shared);

        let host_key = open_key(&host_to_client, &offer[ELEMENT_LEN..])?;
        seal_key(&client_to_host, &self.identity, &mut out[..ACCEPT_LEN])?;

        Ok((host_key, ACCEPT_LEN))
    }
}

impl core::fmt::Debug for PairingClient {
    /// Describes the client's progress without printing any key.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PairingClient")
            .field("started", &self.state.is_some())
            .finish_non_exhaustive()
    }
}

/// Splits the SPAKE2 output into one key per direction.
///
/// One key used both ways would mean both sides sealing under the same key with the same
/// fixed nonce, which is nonce reuse in the plainest possible form. The exchange is two
/// messages, so a counter would be overkill and a separated key is exactly enough.
fn directional_keys(shared: &[u8]) -> (Key, Key) {
    (
        derive(shared, HOST_TO_CLIENT),
        derive(shared, CLIENT_TO_HOST),
    )
}

/// Derives one directional key from the shared secret.
fn derive(shared: &[u8], info: &[u8]) -> Key {
    let hkdf = hkdf::Hkdf::<sha2::Sha256>::new(None, shared);
    let mut key = [0u8; 32];
    hkdf.expand(info, &mut key)
        .expect("thirty-two bytes is within HKDF's output limit");

    Key::from(key)
}

/// The nonce both sealed halves use.
///
/// Fixed at zero, which is safe only because each key seals exactly one message. That is why
/// the keys are separated by direction rather than shared.
fn pairing_nonce() -> Nonce {
    Nonce::default()
}

/// Seals a static key into `out`.
fn seal_key(key: &Key, value: &[u8; KEY_LEN], out: &mut [u8]) -> Result<(), PairingError> {
    if out.len() < KEY_LEN + TAG_LEN {
        return Err(PairingError::BufferTooSmall {
            actual: out.len(),
            needed: KEY_LEN + TAG_LEN,
        });
    }

    out[..KEY_LEN].copy_from_slice(value);

    let tag = ChaCha20Poly1305::new(key)
        .encrypt_in_place_detached(&pairing_nonce(), &[], &mut out[..KEY_LEN])
        .map_err(|_| PairingError::Failed)?;
    out[KEY_LEN..KEY_LEN + TAG_LEN].copy_from_slice(&tag);

    Ok(())
}

/// Opens a sealed static key.
fn open_key(key: &Key, sealed: &[u8]) -> Result<[u8; KEY_LEN], PairingError> {
    if sealed.len() != KEY_LEN + TAG_LEN {
        return Err(PairingError::BadLength {
            actual: sealed.len(),
            expected: KEY_LEN + TAG_LEN,
        });
    }

    let mut value = [0u8; KEY_LEN];
    value.copy_from_slice(&sealed[..KEY_LEN]);

    ChaCha20Poly1305::new(key)
        .decrypt_in_place_detached(
            &pairing_nonce(),
            &[],
            &mut value,
            Tag::from_slice(&sealed[KEY_LEN..]),
        )
        .map_err(|_| PairingError::Failed)?;

    Ok(value)
}
