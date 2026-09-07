//! The messages two machines behind NAT use to find each other.
//!
//! A host and a client on different home connections cannot address each other: both sit
//! behind a router that has no inbound mapping until something inside creates one. The
//! rendezvous server is the fixed point they can both reach. Each sends it a datagram, it
//! observes where that datagram came from, and it tells each side where the other is. Then
//! both send to the address they were given at the same time, and each side's outbound packet
//! opens the mapping its router needs for the other's — the hole punch.
//!
//! # The server is not trusted
//!
//! It never sees a session key and cannot make one. The worst a hostile server can do is
//! refuse to introduce two peers, or introduce them to the wrong address — and the wrong
//! address simply fails the Noise handshake, because that handshake proves who is at the
//! other end regardless of who said they would be. Nothing here needs the server to be
//! honest, only reachable.
//!
//! # Why there is no separate STUN server
//!
//! Observing a peer's public address is exactly what STUN does, and any server that receives
//! a UDP datagram already knows it. Signalling over UDP therefore gets address discovery for
//! nothing. It is also why the server is self-hosted rather than a Cloudflare Worker: a
//! Worker cannot receive arbitrary UDP, so it could not do this at all.
//!
//! # What registration proves
//!
//! A host claims a public key when it registers. If a claim were taken at face value, anyone
//! who has seen that key — every machine ever paired with it — could register it and point
//! the host's clients somewhere else. They could not read anything, but the host would be
//! unreachable, which is enough. So the server answers a claim with a challenge only the
//! holder of the matching private key can read: a Diffie-Hellman against the claimed key
//! itself, sealed under the result. Nobody is trusted, and nobody needs to be.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};

use crate::net::handshake::KEY_LEN;

/// Bytes in the secret a challenge carries.
pub const PROOF_LEN: usize = 16;

/// Bytes in a relay token.
///
/// Eight random bytes, which is what the two peers present at the relay port to be paired with
/// each other. It authorises nothing — the session's own handshake does that — so its only job
/// is to be unguessable enough that a stranger cannot be spliced into somebody's relay by
/// chance. Sixty-four bits is far past that.
pub const RELAY_TOKEN_LEN: usize = 8;

/// Bytes of authentication tag on a sealed challenge.
const TAG_LEN: usize = 16;

/// Longest a rendezvous message can be, which every one of them is far below.
pub const MAX_MESSAGE_LEN: usize = 128;

/// Binds the challenge's key derivation to this protocol and version.
const CHALLENGE_INFO: &[u8] = b"prism-rendezvous-challenge-v1";

/// Reason a rendezvous message could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RendezvousError {
    /// The datagram was empty.
    #[error("a rendezvous message cannot be empty")]
    Empty,

    /// The first byte named no message this version knows.
    #[error("unknown rendezvous message type {tag}")]
    UnknownType {
        /// The byte that was rejected.
        tag: u8,
    },

    /// The message was not the length its type requires.
    #[error("a {kind} message was {actual} bytes, expected {expected}")]
    BadLength {
        /// Which message it claimed to be.
        kind: &'static str,
        /// Length that arrived.
        actual: usize,
        /// Length that was required.
        expected: usize,
    },

    /// An address field did not name a version of IP.
    #[error("address family {family} is neither IPv4 nor IPv6")]
    BadAddress {
        /// The byte that was rejected.
        family: u8,
    },

    /// The caller's buffer cannot hold the message.
    #[error("buffer is {actual} bytes, needs {needed}")]
    BufferTooSmall {
        /// Buffer length supplied.
        actual: usize,
        /// Buffer length required.
        needed: usize,
    },

    /// A challenge did not open, so the key was not held.
    #[error("the challenge was not answered correctly")]
    NotProven,
}

/// The first byte of every rendezvous message.
mod tag {
    /// A host claiming a public key.
    pub const REGISTER: u8 = 0x01;
    /// The server asking the claim to be proved.
    pub const CHALLENGE: u8 = 0x02;
    /// The host answering the challenge.
    pub const PROVE: u8 = 0x03;
    /// The server confirming the registration.
    pub const REGISTERED: u8 = 0x04;
    /// A client asking to be introduced to a host.
    pub const CONNECT: u8 = 0x05;
    /// The server telling a host that someone is calling.
    pub const INCOMING: u8 = 0x06;
    /// The server telling a client where the host is.
    pub const FOUND: u8 = 0x07;
    /// The server saying it knows no such host.
    pub const UNKNOWN_HOST: u8 = 0x08;
    /// A host keeping its registration and its NAT mapping alive.
    pub const KEEPALIVE: u8 = 0x09;
    /// A peer asking for the server to carry its traffic after punching failed.
    pub const RELAY: u8 = 0x0a;
    /// The server naming the port and token to relay through.
    pub const RELAYING: u8 = 0x0b;
}

/// One message in the rendezvous protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A host claims a public key and asks to be reachable under it.
    Register {
        /// The key being claimed.
        host: [u8; KEY_LEN],
    },

    /// The server asks the claim to be proved.
    ///
    /// The secret is sealed under a Diffie-Hellman against the claimed key, so only the
    /// holder of its private half can read it.
    Challenge {
        /// The server's throwaway public key for this challenge.
        ephemeral: [u8; KEY_LEN],
        /// The sealed secret and its tag.
        sealed: [u8; PROOF_LEN + TAG_LEN],
    },

    /// The host returns the secret, proving it holds the key.
    Prove {
        /// The key being claimed, repeated so the server needs no per-address state.
        host: [u8; KEY_LEN],
        /// The secret that was inside the challenge.
        secret: [u8; PROOF_LEN],
    },

    /// The server confirms a registration and reports where the host appears to be.
    ///
    /// The observed address is what a host needs to tell whether its router gave it a stable
    /// mapping, and it is the address clients will be sent to.
    Registered {
        /// Where the server saw the host.
        observed: SocketAddr,
    },

    /// A client asks to be introduced to a host.
    Connect {
        /// The host it wants.
        host: [u8; KEY_LEN],
        /// Its own key, so the host learns who is calling before it answers.
        client: [u8; KEY_LEN],
    },

    /// The server tells a registered host that a client is calling.
    ///
    /// The host answers by sending to that address, which opens its own router's mapping so
    /// the client's packets can arrive.
    Incoming {
        /// The caller's key.
        client: [u8; KEY_LEN],
        /// Where the server saw the caller.
        address: SocketAddr,
    },

    /// The server tells a client where the host is.
    Found {
        /// Where the server saw the host.
        address: SocketAddr,
        /// Where the server sees the client, for its own diagnostics.
        observed: SocketAddr,
    },

    /// The server knows no host under that key.
    ///
    /// Which means the host is not running, not that the key is wrong. The two are
    /// indistinguishable from here and the client can say only the former.
    UnknownHost,

    /// A peer asks the server to carry its traffic.
    ///
    /// Sent when punching has failed, which is what happens when both ends are behind a NAT
    /// that gives each destination a different mapping. Relaying is the exception path: it
    /// costs the server's bandwidth and adds its distance to the round trip, so it is asked
    /// for rather than used by default.
    Relay {
        /// The host of the pair.
        host: [u8; KEY_LEN],
        /// The client of the pair.
        client: [u8; KEY_LEN],
    },

    /// The server names where to send and what to present.
    ///
    /// Sent to both peers. Each then sends its token to the relay port, and once both have
    /// been seen the server forwards between the two addresses verbatim.
    Relaying {
        /// The port to send to, on the same address the server was reached at.
        ///
        /// A different port from signalling, so a datagram arriving there needs no inspection
        /// to know it is traffic to forward — which is what keeps the relay from having to
        /// parse, and therefore from being able to misread, a sealed packet.
        port: u16,
        /// What to present at that port.
        token: [u8; RELAY_TOKEN_LEN],
    },

    /// A host holds its registration and its router's mapping open.
    ///
    /// A NAT mapping expires after tens of seconds of silence, so a host that only spoke at
    /// startup would become unreachable without anything appearing to have gone wrong.
    Keepalive {
        /// The key whose registration is being held.
        host: [u8; KEY_LEN],
    },
}

impl Message {
    /// Writes the message into `out` and returns how many bytes were used.
    ///
    /// # Errors
    ///
    /// Returns [`RendezvousError::BufferTooSmall`] if `out` cannot hold it.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, RendezvousError> {
        let mut writer = Writer::new(out);

        match self {
            Self::Register { host } => {
                writer.byte(tag::REGISTER)?;
                writer.key(host)?;
            }
            Self::Challenge { ephemeral, sealed } => {
                writer.byte(tag::CHALLENGE)?;
                writer.key(ephemeral)?;
                writer.bytes(sealed)?;
            }
            Self::Prove { host, secret } => {
                writer.byte(tag::PROVE)?;
                writer.key(host)?;
                writer.bytes(secret)?;
            }
            Self::Registered { observed } => {
                writer.byte(tag::REGISTERED)?;
                writer.address(*observed)?;
            }
            Self::Connect { host, client } => {
                writer.byte(tag::CONNECT)?;
                writer.key(host)?;
                writer.key(client)?;
            }
            Self::Incoming { client, address } => {
                writer.byte(tag::INCOMING)?;
                writer.key(client)?;
                writer.address(*address)?;
            }
            Self::Found { address, observed } => {
                writer.byte(tag::FOUND)?;
                writer.address(*address)?;
                writer.address(*observed)?;
            }
            Self::UnknownHost => {
                writer.byte(tag::UNKNOWN_HOST)?;
            }
            Self::Keepalive { host } => {
                writer.byte(tag::KEEPALIVE)?;
                writer.key(host)?;
            }
            Self::Relay { host, client } => {
                writer.byte(tag::RELAY)?;
                writer.key(host)?;
                writer.key(client)?;
            }
            Self::Relaying { port, token } => {
                writer.byte(tag::RELAYING)?;
                writer.bytes(&port.to_le_bytes())?;
                writer.bytes(token)?;
            }
        }

        Ok(writer.written())
    }

    /// Reads a message from a datagram.
    ///
    /// # Errors
    ///
    /// Returns [`RendezvousError::Empty`], [`RendezvousError::UnknownType`],
    /// [`RendezvousError::BadLength`] or [`RendezvousError::BadAddress`] for anything that is
    /// not a message this version understands — which is most of what an open port receives.
    pub fn decode(bytes: &[u8]) -> Result<Self, RendezvousError> {
        let (&tag, rest) = bytes.split_first().ok_or(RendezvousError::Empty)?;
        let mut reader = Reader::new(rest);

        let message = match tag {
            tag::REGISTER => Self::Register {
                host: reader.key("register")?,
            },
            tag::CHALLENGE => Self::Challenge {
                ephemeral: reader.key("challenge")?,
                sealed: reader.array("challenge")?,
            },
            tag::PROVE => Self::Prove {
                host: reader.key("prove")?,
                secret: reader.array("prove")?,
            },
            tag::REGISTERED => Self::Registered {
                observed: reader.address("registered")?,
            },
            tag::CONNECT => Self::Connect {
                host: reader.key("connect")?,
                client: reader.key("connect")?,
            },
            tag::INCOMING => Self::Incoming {
                client: reader.key("incoming")?,
                address: reader.address("incoming")?,
            },
            tag::FOUND => Self::Found {
                address: reader.address("found")?,
                observed: reader.address("found")?,
            },
            tag::UNKNOWN_HOST => Self::UnknownHost,
            tag::KEEPALIVE => Self::Keepalive {
                host: reader.key("keepalive")?,
            },
            tag::RELAY => Self::Relay {
                host: reader.key("relay")?,
                client: reader.key("relay")?,
            },
            tag::RELAYING => Self::Relaying {
                port: u16::from_le_bytes(reader.array("relaying")?),
                token: reader.array("relaying")?,
            },
            other => return Err(RendezvousError::UnknownType { tag: other }),
        };

        // A trailing byte means the sender and this reader disagree about the format, and
        // acting on a message only half understood is how a parser becomes a vulnerability.
        reader.expect_empty(kind_of(tag))?;

        Ok(message)
    }
}

/// Names a message type for an error, without allocating.
fn kind_of(tag: u8) -> &'static str {
    match tag {
        tag::REGISTER => "register",
        tag::CHALLENGE => "challenge",
        tag::PROVE => "prove",
        tag::REGISTERED => "registered",
        tag::CONNECT => "connect",
        tag::INCOMING => "incoming",
        tag::FOUND => "found",
        tag::UNKNOWN_HOST => "unknown-host",
        tag::KEEPALIVE => "keepalive",
        tag::RELAY => "relay",
        tag::RELAYING => "relaying",
        _ => "unknown",
    }
}

/// Builds a challenge only the holder of `host` can answer.
///
/// Returns the message to send and the secret the answer has to contain.
///
/// # Errors
///
/// Returns [`RendezvousError::NotProven`] if the platform has no usable randomness, which is
/// a condition no challenge should be issued under.
pub fn challenge(host: &[u8; KEY_LEN]) -> Result<(Message, [u8; PROOF_LEN]), RendezvousError> {
    let mut private = [0u8; KEY_LEN];
    getrandom::fill(&mut private).map_err(|_| RendezvousError::NotProven)?;

    let mut secret = [0u8; PROOF_LEN];
    getrandom::fill(&mut secret).map_err(|_| RendezvousError::NotProven)?;

    let ephemeral =
        crate::net::handshake::public_key(&private).map_err(|_| RendezvousError::NotProven)?;
    let shared =
        crate::net::handshake::agree(&private, host).map_err(|_| RendezvousError::NotProven)?;
    let key = challenge_key(&shared);

    let mut sealed = [0u8; PROOF_LEN + TAG_LEN];
    sealed[..PROOF_LEN].copy_from_slice(&secret);

    let tag = ChaCha20Poly1305::new(&key)
        .encrypt_in_place_detached(&Nonce::default(), &[], &mut sealed[..PROOF_LEN])
        .map_err(|_| RendezvousError::NotProven)?;
    sealed[PROOF_LEN..].copy_from_slice(&tag);

    Ok((Message::Challenge { ephemeral, sealed }, secret))
}

/// Opens a challenge with the private key it was aimed at.
///
/// # Errors
///
/// Returns [`RendezvousError::NotProven`] if the challenge was not aimed at this key, which
/// is what a server trying to make a host prove a key it does not hold produces.
pub fn answer(
    private: &[u8; KEY_LEN],
    ephemeral: &[u8; KEY_LEN],
    sealed: &[u8; PROOF_LEN + TAG_LEN],
) -> Result<[u8; PROOF_LEN], RendezvousError> {
    let shared =
        crate::net::handshake::agree(private, ephemeral).map_err(|_| RendezvousError::NotProven)?;
    let key = challenge_key(&shared);

    let mut secret = [0u8; PROOF_LEN];
    secret.copy_from_slice(&sealed[..PROOF_LEN]);

    ChaCha20Poly1305::new(&key)
        .decrypt_in_place_detached(
            &Nonce::default(),
            &[],
            &mut secret,
            Tag::from_slice(&sealed[PROOF_LEN..]),
        )
        .map_err(|_| RendezvousError::NotProven)?;

    Ok(secret)
}

/// Derives the sealing key for a challenge from the shared secret.
///
/// A fixed nonce is safe here only because the key is fresh for every challenge: the server
/// makes a new ephemeral each time, so no key ever seals twice.
fn challenge_key(shared: &[u8; KEY_LEN]) -> Key {
    let hkdf = hkdf::Hkdf::<sha2::Sha256>::new(None, shared);
    let mut key = [0u8; 32];
    hkdf.expand(CHALLENGE_INFO, &mut key)
        .expect("thirty-two bytes is within HKDF's output limit");

    Key::from(key)
}

/// Writes fields into a caller's buffer, refusing to run past the end.
struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl<'a> Writer<'a> {
    /// Starts writing at the beginning of `out`.
    fn new(out: &'a mut [u8]) -> Self {
        Self { out, at: 0 }
    }

    /// Writes one byte.
    fn byte(&mut self, value: u8) -> Result<(), RendezvousError> {
        self.bytes(&[value])
    }

    /// Writes a key.
    fn key(&mut self, value: &[u8; KEY_LEN]) -> Result<(), RendezvousError> {
        self.bytes(value)
    }

    /// Writes an address as a family byte, the address, then the port.
    fn address(&mut self, value: SocketAddr) -> Result<(), RendezvousError> {
        match value.ip() {
            IpAddr::V4(ip) => {
                self.byte(4)?;
                self.bytes(&ip.octets())?;
            }
            IpAddr::V6(ip) => {
                self.byte(6)?;
                self.bytes(&ip.octets())?;
            }
        }

        self.bytes(&value.port().to_le_bytes())
    }

    /// Writes a run of bytes.
    fn bytes(&mut self, value: &[u8]) -> Result<(), RendezvousError> {
        let end = self.at + value.len();
        if end > self.out.len() {
            return Err(RendezvousError::BufferTooSmall {
                actual: self.out.len(),
                needed: end,
            });
        }

        self.out[self.at..end].copy_from_slice(value);
        self.at = end;

        Ok(())
    }

    /// Returns how many bytes have been written.
    fn written(&self) -> usize {
        self.at
    }
}

/// Reads fields out of a datagram, refusing to run past the end.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// Starts reading at the beginning of `bytes`.
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Reads a fixed-size array.
    fn array<const N: usize>(&mut self, kind: &'static str) -> Result<[u8; N], RendezvousError> {
        let end = self.at + N;
        if end > self.bytes.len() {
            return Err(RendezvousError::BadLength {
                kind,
                actual: self.bytes.len(),
                expected: end,
            });
        }

        let mut value = [0u8; N];
        value.copy_from_slice(&self.bytes[self.at..end]);
        self.at = end;

        Ok(value)
    }

    /// Reads a key.
    fn key(&mut self, kind: &'static str) -> Result<[u8; KEY_LEN], RendezvousError> {
        self.array(kind)
    }

    /// Reads an address.
    fn address(&mut self, kind: &'static str) -> Result<SocketAddr, RendezvousError> {
        let family = self.array::<1>(kind)?[0];

        let ip = match family {
            4 => IpAddr::V4(Ipv4Addr::from(self.array::<4>(kind)?)),
            6 => IpAddr::V6(Ipv6Addr::from(self.array::<16>(kind)?)),
            other => return Err(RendezvousError::BadAddress { family: other }),
        };

        let port = u16::from_le_bytes(self.array::<2>(kind)?);

        Ok(SocketAddr::new(ip, port))
    }

    /// Fails if anything is left over.
    fn expect_empty(&self, kind: &'static str) -> Result<(), RendezvousError> {
        if self.at == self.bytes.len() {
            return Ok(());
        }

        Err(RendezvousError::BadLength {
            kind,
            actual: self.bytes.len(),
            expected: self.at,
        })
    }
}
