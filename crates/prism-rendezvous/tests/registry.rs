//! Tests for what the rendezvous server remembers.
//!
//! The server holds no secrets, so nothing here is about confidentiality. What it can do is
//! send a client to the wrong place, and every test below is about the ways someone might try
//! to make it — plus the ways its two tables could be made to grow without bound.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use prism_rendezvous::registry::{
    CHALLENGE_TTL, CONNECT_INTERVAL, Proved, REGISTRATION_TTL, Registry,
};

/// A host key.
const HOST: [u8; 32] = [0xa7; 32];

/// A different host key.
const OTHER: [u8; 32] = [0x3e; 32];

/// A challenge secret.
const SECRET: [u8; 16] = [0x5b; 16];

/// Where a host appears to be.
fn home() -> SocketAddr {
    "198.51.100.7:41000".parse().expect("valid")
}

/// Somewhere else entirely.
fn elsewhere() -> SocketAddr {
    "203.0.113.9:52000".parse().expect("valid")
}

/// Registers `HOST` at `home()` and returns the moment it happened.
fn registered(registry: &mut Registry) -> Instant {
    let now = Instant::now();

    registry.challenge_issued(home(), HOST, SECRET, now);
    assert_eq!(
        registry.prove(home(), &HOST, &SECRET, now),
        Proved::Registered
    );

    now
}

#[test]
fn a_proved_claim_becomes_a_registration() {
    let mut registry = Registry::new();
    let now = registered(&mut registry);

    assert_eq!(registry.lookup(&HOST, now), Some(home()));
    assert_eq!(registry.registered(), 1);
}

#[test]
fn an_unproved_claim_is_not_a_registration() {
    // Without this, anyone who has ever seen a host's public key could claim it and every
    // client would afterwards be sent to them. They would learn nothing, because the Noise
    // handshake would fail, but the host would be unreachable — which is the attack.
    let mut registry = Registry::new();
    let now = Instant::now();

    registry.challenge_issued(home(), HOST, SECRET, now);

    assert_eq!(
        registry.prove(home(), &HOST, &[0x00; 16], now),
        Proved::Wrong
    );
    assert_eq!(registry.lookup(&HOST, now), None);
}

#[test]
fn a_wrong_answer_spends_the_challenge() {
    // Otherwise one challenge could be guessed against indefinitely. Sixteen random bytes is
    // far beyond guessing either way, but a challenge that survives a wrong answer is the
    // shape that eventually guards something shorter.
    let mut registry = Registry::new();
    let now = Instant::now();

    registry.challenge_issued(home(), HOST, SECRET, now);
    assert_eq!(
        registry.prove(home(), &HOST, &[0x00; 16], now),
        Proved::Wrong
    );
    assert_eq!(
        registry.prove(home(), &HOST, &SECRET, now),
        Proved::NoChallenge,
        "the right answer still worked after a wrong one"
    );
}

#[test]
fn an_answer_from_a_different_address_proves_nothing() {
    // The challenge went to one address. Another address answering it, even correctly, has
    // shown only that it saw the first exchange.
    let mut registry = Registry::new();
    let now = Instant::now();

    registry.challenge_issued(home(), HOST, SECRET, now);

    assert_eq!(
        registry.prove(elsewhere(), &HOST, &SECRET, now),
        Proved::NoChallenge
    );
    assert_eq!(registry.lookup(&HOST, now), None);
}

#[test]
fn an_answer_naming_a_different_key_proves_nothing() {
    let mut registry = Registry::new();
    let now = Instant::now();

    registry.challenge_issued(home(), HOST, SECRET, now);

    assert_eq!(registry.prove(home(), &OTHER, &SECRET, now), Proved::Wrong);
    assert_eq!(registry.lookup(&OTHER, now), None);
}

#[test]
fn a_challenge_expires() {
    let mut registry = Registry::new();
    let now = Instant::now();

    registry.challenge_issued(home(), HOST, SECRET, now);

    let late = now + CHALLENGE_TTL + Duration::from_secs(1);
    assert_eq!(
        registry.prove(home(), &HOST, &SECRET, late),
        Proved::NoChallenge
    );
}

#[test]
fn a_registration_cannot_move_without_a_new_proof() {
    // The one that matters. If a keepalive from a new address moved a registration, anyone
    // who knew a host's key could take its place with a single datagram.
    let mut registry = Registry::new();
    let now = registered(&mut registry);

    assert!(
        !registry.refresh(elsewhere(), &HOST, now),
        "a keepalive from elsewhere was accepted"
    );
    assert_eq!(registry.lookup(&HOST, now), Some(home()));
}

#[test]
fn a_registration_can_move_after_a_new_proof() {
    // A home connection's address does change. Proving the key again is what makes that safe,
    // and it has to actually work or a host that reconnected would be stuck.
    let mut registry = Registry::new();
    let now = registered(&mut registry);

    registry.challenge_issued(elsewhere(), HOST, SECRET, now);
    assert_eq!(
        registry.prove(elsewhere(), &HOST, &SECRET, now),
        Proved::Registered
    );
    assert_eq!(registry.lookup(&HOST, now), Some(elsewhere()));
}

#[test]
fn a_registration_expires_when_the_host_goes_quiet() {
    let mut registry = Registry::new();
    let now = registered(&mut registry);

    let late = now + REGISTRATION_TTL + Duration::from_secs(1);

    assert_eq!(registry.lookup(&HOST, late), None);
    registry.expire(late);
    assert_eq!(registry.registered(), 0);
}

#[test]
fn a_keepalive_holds_a_registration_open() {
    let mut registry = Registry::new();
    let now = registered(&mut registry);

    let mut at = now;
    for _ in 0..20 {
        at += REGISTRATION_TTL / 2;
        assert!(
            registry.refresh(home(), &HOST, at),
            "the keepalive was refused"
        );
        assert_eq!(registry.lookup(&HOST, at), Some(home()));
    }
}

#[test]
fn a_keepalive_for_a_host_that_was_never_registered_does_nothing() {
    let mut registry = Registry::new();
    let now = Instant::now();

    assert!(!registry.refresh(home(), &HOST, now));
    assert_eq!(registry.registered(), 0);
}

#[test]
fn introductions_from_one_address_are_spaced_out() {
    // Every introduction makes the server send a datagram to a host. It cannot be aimed — it
    // goes to the registered address — so it is not an amplifier, but without a floor it is
    // still a way to flood a host with wake-ups.
    let mut registry = Registry::new();
    let now = Instant::now();

    assert!(registry.may_connect(home(), now));
    assert!(!registry.may_connect(home(), now));
    assert!(
        registry.may_connect(home(), now + CONNECT_INTERVAL),
        "the floor never lifted"
    );
}

#[test]
fn one_address_being_throttled_does_not_throttle_another() {
    let mut registry = Registry::new();
    let now = Instant::now();

    assert!(registry.may_connect(home(), now));
    assert!(
        registry.may_connect(elsewhere(), now),
        "one caller's rate limit stopped a different caller"
    );
}

#[test]
fn expiry_clears_every_table_it_owns() {
    // A table that only grows is a table an attacker fills, and the rate-limit table is the
    // one that grows fastest because it takes an entry per source address.
    let mut registry = Registry::new();
    let now = Instant::now();

    for port in 1000..1200u16 {
        let address: SocketAddr = format!("198.51.100.7:{port}").parse().expect("valid");
        registry.challenge_issued(address, HOST, SECRET, now);
        registry.may_connect(address, now);
    }

    assert_eq!(registry.outstanding(), 200);

    registry.expire(now + Duration::from_secs(3600));

    assert_eq!(registry.outstanding(), 0);
    assert_eq!(registry.registered(), 0);
}

#[test]
fn a_second_claim_from_one_address_replaces_the_first() {
    // A peer that retries must not accumulate entries, and the newest challenge is the one it
    // is answering.
    let mut registry = Registry::new();
    let now = Instant::now();

    registry.challenge_issued(home(), HOST, [0x11; 16], now);
    registry.challenge_issued(home(), HOST, SECRET, now);

    assert_eq!(registry.outstanding(), 1);
    assert_eq!(
        registry.prove(home(), &HOST, &SECRET, now),
        Proved::Registered
    );
}
