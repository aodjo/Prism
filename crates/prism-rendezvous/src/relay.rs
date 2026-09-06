//! Carrying traffic between two peers that could not reach each other directly.
//!
//! Punching works whenever at least one router gives a stable mapping to any destination,
//! which is most of them. It fails when both ends give each destination a different mapping —
//! a symmetric NAT on both sides, which is what carrier-grade NAT and much phone tethering
//! looks like. For those pairs there is no address to punch to, and the only thing left is a
//! machine both can reach that will pass the bytes along.
//!
//! # Why this is the exception path
//!
//! It costs the server's bandwidth and adds the server's distance to every round trip. A
//! session relayed through a machine on the other side of the country is a session with tens
//! of milliseconds added to a budget of twenty-five. So it is asked for after punching fails,
//! never used by default, and the interface has to say when it is in use — a person wondering
//! why the picture feels heavy deserves to know the answer is the network path and not the
//! encoder.
//!
//! # Why it is a separate port
//!
//! Signalling and relayed traffic arrive at the same server, and one of them is a sealed
//! packet that looks like nothing in particular by design. If they shared a port the server
//! would have to guess which it was holding, and a sealed video packet that happened to parse
//! as a signalling message would be swallowed rather than forwarded. A second port removes the
//! question: everything arriving there is traffic, and the only thing that is not is the eight
//! byte token a peer presents to be paired.
//!
//! # What it never does
//!
//! It does not look inside. The bytes it forwards are sealed under keys it has never seen and
//! cannot derive, and it forwards them verbatim — no header, no rewriting, no length change. A
//! relayed session and a direct one are byte for byte the same session.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use prism_core::net::rendezvous::RELAY_TOKEN_LEN;

/// How long a relay session survives with nothing flowing.
///
/// A minute. Long enough that a paused session is not torn down, short enough that a machine
/// somebody closed the lid on stops holding a slot.
pub const IDLE_TTL: Duration = Duration::from_secs(60);

/// How long a half-open pairing waits for its other side.
///
/// Both peers are told at the same moment and both send immediately, so anything beyond a few
/// seconds is a peer that is not coming.
pub const PAIRING_TTL: Duration = Duration::from_secs(20);

/// Most relay sessions the server will carry at once.
///
/// A ceiling to make a runaway visible. A relayed session is real bandwidth — tens of megabits
/// each — so the practical limit is the link long before it is this number.
pub const MAX_SESSIONS: usize = 1_000;

/// One side of a relay, once it has presented its token.
#[derive(Debug, Clone, Copy)]
struct Side {
    address: SocketAddr,
    seen: Instant,
}

/// A relay in one of its two states.
#[derive(Debug)]
enum Session {
    /// One side has presented its token and the other has not.
    Waiting { first: Side },
    /// Both sides are known and traffic is flowing.
    Open { a: Side, b: Side },
}

/// What to do with a datagram that arrived at the relay port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forward {
    /// Send it, unchanged, to this address.
    To(SocketAddr),
    /// It was a token presentation and there is nothing to send yet.
    Registered,
    /// Both sides are now present; tell this address the relay is open.
    Opened(SocketAddr),
    /// Nothing to do with it.
    Ignored,
}

/// Every relay the server is carrying.
#[derive(Debug, Default)]
pub struct Relays {
    /// Sessions the signalling side has allocated, by token.
    sessions: HashMap<[u8; RELAY_TOKEN_LEN], Session>,
    /// Which session an address belongs to, so a forward is one lookup rather than a scan.
    ///
    /// This is the map that matters for throughput: it is consulted for every packet of every
    /// relayed session, tens of thousands a second, and a linear search over sessions would
    /// make the server's cost quadratic in how many it is carrying.
    routes: HashMap<SocketAddr, [u8; RELAY_TOKEN_LEN]>,
}

impl Relays {
    /// Creates an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a session for a token, ready for both sides to present it.
    ///
    /// Returns whether there was room. A refusal is the server saying it is full, which the
    /// caller reports rather than papering over: a session that silently never opens is worse
    /// than one that says it cannot.
    pub fn allocate(&mut self, token: [u8; RELAY_TOKEN_LEN], now: Instant) -> bool {
        if self.sessions.contains_key(&token) {
            return true;
        }

        if self.sessions.len() >= MAX_SESSIONS {
            return false;
        }

        // Placed as waiting with a side that will be replaced by the first real presentation.
        // Recorded now so the pairing window starts when the peers were told, not when the
        // first of them happened to answer.
        self.sessions.insert(
            token,
            Session::Waiting {
                first: Side {
                    address: SocketAddr::from(([0, 0, 0, 0], 0)),
                    seen: now,
                },
            },
        );

        true
    }

    /// Takes a datagram that arrived at the relay port.
    ///
    /// A datagram exactly [`RELAY_TOKEN_LEN`] bytes long is a peer presenting its token;
    /// anything else is traffic to forward. The two cannot be confused: a sealed packet is at
    /// least twenty-four bytes.
    pub fn accept(&mut self, from: SocketAddr, datagram: &[u8], now: Instant) -> Forward {
        if datagram.len() == RELAY_TOKEN_LEN {
            let mut token = [0u8; RELAY_TOKEN_LEN];
            token.copy_from_slice(datagram);

            return self.present(from, token, now);
        }

        self.forward(from, now)
    }

    /// Records a peer presenting its token.
    fn present(&mut self, from: SocketAddr, token: [u8; RELAY_TOKEN_LEN], now: Instant) -> Forward {
        let Some(session) = self.sessions.get_mut(&token) else {
            // A token nobody allocated. Silence: answering would confirm to a scan that this
            // port relays for somebody.
            return Forward::Ignored;
        };

        let side = Side {
            address: from,
            seen: now,
        };

        match session {
            Session::Waiting { first } if first.address.port() == 0 => {
                *session = Session::Waiting { first: side };
                self.routes.insert(from, token);

                Forward::Registered
            }
            Session::Waiting { first } if first.address == from => {
                first.seen = now;

                Forward::Registered
            }
            Session::Waiting { first } => {
                let other = first.address;
                *session = Session::Open { a: *first, b: side };
                self.routes.insert(from, token);

                // The side that was already waiting is told too, so it stops presenting and
                // starts sending. Its own presentations would otherwise continue until the
                // pairing window closed on a relay that was already open.
                let _ = other;

                Forward::Opened(other)
            }
            Session::Open { a, b } => {
                // A repeat from a side already present, which happens because a peer presents
                // until it hears back and the reply may be lost.
                if a.address == from {
                    a.seen = now;
                } else if b.address == from {
                    b.seen = now;
                } else {
                    // A third address on somebody else's token. Refused: a relay carries two
                    // peers, and letting a third in would let a stranger's packets be
                    // delivered as though the peer had sent them.
                    return Forward::Ignored;
                }

                Forward::Registered
            }
        }
    }

    /// Works out where a packet from `from` should go.
    fn forward(&mut self, from: SocketAddr, now: Instant) -> Forward {
        let Some(token) = self.routes.get(&from).copied() else {
            return Forward::Ignored;
        };

        let Some(Session::Open { a, b }) = self.sessions.get_mut(&token) else {
            return Forward::Ignored;
        };

        if a.address == from {
            a.seen = now;

            return Forward::To(b.address);
        }

        if b.address == from {
            b.seen = now;

            return Forward::To(a.address);
        }

        Forward::Ignored
    }

    /// Drops sessions that have gone quiet or were never completed.
    pub fn expire(&mut self, now: Instant) {
        let mut dropped: Vec<[u8; RELAY_TOKEN_LEN]> = Vec::new();

        for (token, session) in &self.sessions {
            let stale = match session {
                Session::Waiting { first } => now.duration_since(first.seen) > PAIRING_TTL,
                Session::Open { a, b } => now.duration_since(a.seen.max(b.seen)) > IDLE_TTL,
            };

            if stale {
                dropped.push(*token);
            }
        }

        for token in dropped {
            self.sessions.remove(&token);
        }

        // The route table is rebuilt from what survived rather than pruned alongside, because
        // an address left behind here would silently forward a later session's packets to a
        // peer that is gone.
        self.routes
            .retain(|_, token| self.sessions.contains_key(token));
    }

    /// Returns how many relays are carrying traffic.
    #[must_use]
    pub fn open(&self) -> usize {
        self.sessions
            .values()
            .filter(|session| matches!(session, Session::Open { .. }))
            .count()
    }

    /// Returns how many are waiting for a second side.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.sessions.len() - self.open()
    }
}
