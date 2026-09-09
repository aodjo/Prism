//! What the server remembers, and for how long.
//!
//! Two tables and nothing else. Hosts that have proved their key and where they were last
//! seen, and challenges that have been issued but not yet answered. Both expire, because a
//! table that only grows is a table an attacker fills.
//!
//! # Why a registration cannot move without a new proof
//!
//! A keepalive refreshes a registration only when it arrives from the address already
//! recorded. If it did not, anyone who knew a host's public key could send one keepalive from
//! their own address and every client would afterwards be introduced to them. They would learn
//! nothing — the Noise handshake would fail — but the host would be unreachable, which is the
//! whole attack. A host whose address genuinely changed registers again and proves the key.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use prism_core::net::handshake::KEY_LEN;
use prism_core::net::rendezvous::PROOF_LEN;

/// How long a registration survives without a keepalive.
///
/// Comfortably longer than the keepalive interval, so one lost datagram does not unregister a
/// host, and short enough that a host which went away stops being offered to clients within a
/// minute or two.
pub const REGISTRATION_TTL: Duration = Duration::from_secs(90);

/// How long an unanswered challenge is remembered.
///
/// Long enough for a round trip to anywhere on earth several times over, short enough that
/// issuing challenges is not a way to fill memory.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(10);

/// Shortest gap between two introductions asked for by the same address.
///
/// Every introduction makes the server send a datagram to a host. That datagram cannot be
/// aimed — it goes to the registered address, not one the caller chose — so it is not an
/// amplifier, but without a floor it is still a way to flood a host with wake-ups.
pub const CONNECT_INTERVAL: Duration = Duration::from_millis(200);

/// Most hosts the server will hold at once.
///
/// A ceiling rather than a target. Registration costs a proof, so filling this takes real
/// keys and real round trips, but a table with no ceiling is still a table that can be filled.
pub const MAX_REGISTRATIONS: usize = 100_000;

/// A host that has proved its key.
#[derive(Debug, Clone, Copy)]
struct Registration {
    address: SocketAddr,
    /// Where the host said it is on its own network.
    ///
    /// Kept because two machines behind one router cannot reach each other at the address this
    /// server sees. Never used by this server for anything — it is repeated to a client and
    /// nothing else, and a host that lies about it has misdirected its own clients.
    local: SocketAddr,
    refreshed: Instant,
}

/// A challenge issued and not yet answered.
#[derive(Debug, Clone, Copy)]
struct Pending {
    host: [u8; KEY_LEN],
    secret: [u8; PROOF_LEN],
    issued: Instant,
}

/// Everything the server knows.
#[derive(Debug, Default)]
pub struct Registry {
    hosts: HashMap<[u8; KEY_LEN], Registration>,
    /// Keyed by the address the claim came from, because that is all the server has to go on
    /// before the claim is proved.
    pending: HashMap<SocketAddr, Pending>,
    last_connect: HashMap<SocketAddr, Instant>,
}

/// What answering a claim produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proved {
    /// The claim was proved and the host is now registered.
    Registered,
    /// There was no outstanding challenge for this address, or it had expired.
    NoChallenge,
    /// The secret was wrong, so the claim was not proved.
    Wrong,
    /// The table is full.
    Full,
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a challenge issued to `from` for `host`.
    ///
    /// One outstanding challenge per address. A second claim from the same address replaces
    /// the first rather than accumulating, so a peer that retries costs nothing.
    pub fn challenge_issued(
        &mut self,
        from: SocketAddr,
        host: [u8; KEY_LEN],
        secret: [u8; PROOF_LEN],
        now: Instant,
    ) {
        self.pending.insert(
            from,
            Pending {
                host,
                secret,
                issued: now,
            },
        );
    }

    /// Checks an answer and registers the host if it is right.
    pub fn prove(
        &mut self,
        from: SocketAddr,
        host: &[u8; KEY_LEN],
        secret: &[u8; PROOF_LEN],
        local: SocketAddr,
        now: Instant,
    ) -> Proved {
        let Some(pending) = self.pending.get(&from).copied() else {
            return Proved::NoChallenge;
        };

        if now.duration_since(pending.issued) > CHALLENGE_TTL {
            self.pending.remove(&from);
            return Proved::NoChallenge;
        }

        // Compared without an early exit. The secret is not long-lived, but a comparison that
        // stops at the first differing byte is a shape that should never be written here at
        // all, because the next one like it may guard something that matters more.
        let matches = pending.host == *host
            && pending
                .secret
                .iter()
                .zip(secret)
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0;

        if !matches {
            // Spent either way, so a wrong answer costs a fresh round trip rather than
            // allowing guesses against one challenge.
            self.pending.remove(&from);
            return Proved::Wrong;
        }

        self.pending.remove(&from);

        if !self.hosts.contains_key(host) && self.hosts.len() >= MAX_REGISTRATIONS {
            return Proved::Full;
        }

        self.hosts.insert(
            *host,
            Registration {
                address: from,
                local,
                refreshed: now,
            },
        );

        Proved::Registered
    }

    /// Refreshes a registration, but only from the address it was made at.
    ///
    /// Returns whether it was refreshed, which is what the server answers with — a host that
    /// stops being answered knows to register again rather than silently becoming
    /// unreachable.
    pub fn refresh(&mut self, from: SocketAddr, host: &[u8; KEY_LEN], now: Instant) -> bool {
        let Some(registration) = self.hosts.get_mut(host) else {
            return false;
        };

        if registration.address != from {
            return false;
        }

        registration.refreshed = now;

        true
    }

    /// Returns where a host is, if it is registered and has not gone quiet.
    #[must_use]
    pub fn lookup(&self, host: &[u8; KEY_LEN], now: Instant) -> Option<SocketAddr> {
        self.found(host, now).map(|(address, _)| address)
    }

    /// Returns where a host is and where it said it is on its own network.
    ///
    /// Both, because a client behind the same router as the host needs the second and cannot
    /// be told which it needs until it has compared the first with its own.
    #[must_use]
    pub fn found(&self, host: &[u8; KEY_LEN], now: Instant) -> Option<(SocketAddr, SocketAddr)> {
        let registration = self.hosts.get(host)?;

        (now.duration_since(registration.refreshed) <= REGISTRATION_TTL)
            .then_some((registration.address, registration.local))
    }

    /// Returns whether `from` may ask for an introduction now, and records that it did.
    pub fn may_connect(&mut self, from: SocketAddr, now: Instant) -> bool {
        if let Some(last) = self.last_connect.get(&from) {
            if now.duration_since(*last) < CONNECT_INTERVAL {
                return false;
            }
        }

        self.last_connect.insert(from, now);

        true
    }

    /// Drops everything that has expired.
    ///
    /// Called on a timer rather than on every datagram, because walking the tables is the one
    /// piece of work here whose cost grows with how many hosts there are.
    pub fn expire(&mut self, now: Instant) {
        self.hosts.retain(|_, registration| {
            now.duration_since(registration.refreshed) <= REGISTRATION_TTL
        });
        self.pending
            .retain(|_, pending| now.duration_since(pending.issued) <= CHALLENGE_TTL);
        self.last_connect
            .retain(|_, last| now.duration_since(*last) <= CONNECT_INTERVAL * 16);
    }

    /// Returns how many hosts are registered.
    #[must_use]
    pub fn registered(&self) -> usize {
        self.hosts.len()
    }

    /// Returns how many challenges are outstanding.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.pending.len()
    }
}
