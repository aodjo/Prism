//! Tests for the relay.
//!
//! A relay carries a session's own traffic, so the property that matters above all others is
//! that it carries it **unchanged** and delivers it to the one peer it is for. Everything else
//! here is about the ways a stranger might try to be that peer, or to make the server carry
//! traffic for nobody.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use prism_rendezvous::relay::{Forward, IDLE_TTL, MAX_SESSIONS, PAIRING_TTL, Relays};

/// A token the server would have handed both peers.
const TOKEN: [u8; 8] = [0x9f, 0x2c, 0x41, 0x08, 0xb3, 0xd7, 0x5e, 0x1a];

/// A different one.
const OTHER: [u8; 8] = [0x11; 8];

/// Where one peer appears.
fn host() -> SocketAddr {
    "198.51.100.7:41000".parse().expect("valid")
}

/// Where the other does.
fn client() -> SocketAddr {
    "203.0.113.9:52000".parse().expect("valid")
}

/// Somewhere neither of them is.
fn stranger() -> SocketAddr {
    "192.0.2.44:33000".parse().expect("valid")
}

/// Allocates a session and presents both tokens, returning the moment it opened.
fn opened(relays: &mut Relays) -> Instant {
    let now = Instant::now();

    assert!(relays.allocate(TOKEN, now));
    assert_eq!(relays.accept(host(), &TOKEN, now), Forward::Registered);
    assert_eq!(
        relays.accept(client(), &TOKEN, now),
        Forward::Opened(host())
    );

    now
}

#[test]
fn a_packet_goes_to_the_other_side_and_nowhere_else() {
    let mut relays = Relays::new();
    let now = opened(&mut relays);

    assert_eq!(
        relays.accept(host(), &[0u8; 1200], now),
        Forward::To(client())
    );
    assert_eq!(
        relays.accept(client(), &[0u8; 300], now),
        Forward::To(host())
    );
    assert_eq!(relays.open(), 1);
}

#[test]
fn a_stranger_gets_nothing_forwarded() {
    // The relay is between two addresses. A third sending to the same port must not have its
    // packets delivered as though a peer had sent them, which on a sealed session would be
    // noise the peer discards — and on the way to becoming something worse.
    let mut relays = Relays::new();
    let now = opened(&mut relays);

    assert_eq!(
        relays.accept(stranger(), &[0u8; 500], now),
        Forward::Ignored
    );
}

#[test]
fn a_stranger_presenting_a_live_token_is_refused() {
    // Someone who observed a token — from a log, from a compromised peer — must not be able to
    // take a place in a relay that is already carrying two.
    let mut relays = Relays::new();
    let now = opened(&mut relays);

    assert_eq!(relays.accept(stranger(), &TOKEN, now), Forward::Ignored);

    // And the relay still carries the two it had.
    assert_eq!(
        relays.accept(host(), &[0u8; 100], now),
        Forward::To(client())
    );
}

#[test]
fn a_token_nobody_allocated_is_answered_with_silence() {
    // A reply would confirm to a scan that this port relays for somebody.
    let mut relays = Relays::new();

    assert_eq!(
        relays.accept(host(), &OTHER, Instant::now()),
        Forward::Ignored
    );
}

#[test]
fn traffic_before_the_relay_opens_goes_nowhere() {
    let mut relays = Relays::new();
    let now = Instant::now();

    assert!(relays.allocate(TOKEN, now));
    assert_eq!(relays.accept(host(), &TOKEN, now), Forward::Registered);

    assert_eq!(relays.accept(host(), &[0u8; 400], now), Forward::Ignored);
}

#[test]
fn a_peer_that_presents_again_is_answered_again() {
    // Presentations repeat until the peer hears back, because the answer can be lost. A repeat
    // must not be mistaken for a second peer.
    let mut relays = Relays::new();
    let now = Instant::now();

    assert!(relays.allocate(TOKEN, now));
    assert_eq!(relays.accept(host(), &TOKEN, now), Forward::Registered);
    assert_eq!(relays.accept(host(), &TOKEN, now), Forward::Registered);
    assert_eq!(relays.waiting(), 1);
    assert_eq!(relays.open(), 0);

    assert_eq!(
        relays.accept(client(), &TOKEN, now),
        Forward::Opened(host())
    );
    assert_eq!(relays.open(), 1);

    // And once open, a straggling presentation from either side is still just an answer.
    assert_eq!(relays.accept(host(), &TOKEN, now), Forward::Registered);
    assert_eq!(relays.accept(client(), &TOKEN, now), Forward::Registered);
    assert_eq!(relays.open(), 1);
}

#[test]
fn a_relay_that_is_used_stays_open() {
    let mut relays = Relays::new();
    let mut at = opened(&mut relays);

    for _ in 0..20 {
        at += IDLE_TTL / 2;
        assert_eq!(
            relays.accept(host(), &[0u8; 100], at),
            Forward::To(client())
        );
        relays.expire(at);
    }

    assert_eq!(relays.open(), 1);
}

#[test]
fn a_relay_that_goes_quiet_is_dropped() {
    let mut relays = Relays::new();
    let now = opened(&mut relays);

    relays.expire(now + IDLE_TTL + Duration::from_secs(1));

    assert_eq!(relays.open(), 0);
    assert_eq!(
        relays.accept(host(), &[0u8; 100], now),
        Forward::Ignored,
        "a dropped relay still had a route"
    );
}

#[test]
fn a_pairing_whose_second_side_never_comes_is_dropped() {
    let mut relays = Relays::new();
    let now = Instant::now();

    assert!(relays.allocate(TOKEN, now));
    assert_eq!(relays.accept(host(), &TOKEN, now), Forward::Registered);

    relays.expire(now + PAIRING_TTL + Duration::from_secs(1));

    assert_eq!(relays.waiting(), 0);
}

#[test]
fn an_allocation_nobody_ever_uses_is_dropped() {
    // A peer that asked for a relay and then reached its host directly. Without this the slot
    // would be held until the process restarted.
    let mut relays = Relays::new();
    let now = Instant::now();

    assert!(relays.allocate(TOKEN, now));
    relays.expire(now + PAIRING_TTL + Duration::from_secs(1));

    assert_eq!(relays.waiting(), 0);
}

#[test]
fn a_route_does_not_outlive_its_session() {
    // The route table is what every forwarded packet is looked up in. An address left behind
    // after its session ended would send a later session's packets to a peer that is gone.
    let mut relays = Relays::new();
    let now = opened(&mut relays);
    let later = now + IDLE_TTL + Duration::from_secs(1);

    relays.expire(later);

    // A new relay between the same two addresses, on a new token.
    assert!(relays.allocate(OTHER, later));
    assert_eq!(relays.accept(host(), &OTHER, later), Forward::Registered);
    assert_eq!(
        relays.accept(client(), &OTHER, later),
        Forward::Opened(host())
    );
    assert_eq!(
        relays.accept(host(), &[0u8; 100], later),
        Forward::To(client())
    );
}

#[test]
fn the_table_has_a_ceiling() {
    // A relayed session is real bandwidth, so the link runs out long before this does. It is
    // here so a runaway is visible rather than unbounded.
    let mut relays = Relays::new();
    let now = Instant::now();

    for index in 0..MAX_SESSIONS {
        let mut token = [0u8; 8];
        token[..8].copy_from_slice(&(index as u64).to_le_bytes());
        assert!(relays.allocate(token, now), "refused at {index}");
    }

    assert!(
        !relays.allocate([0xff; 8], now),
        "the ceiling was not enforced"
    );
}

#[test]
fn allocating_the_same_token_twice_is_not_a_second_session() {
    // Peers repeat their request when an answer is lost, which reaches the server as a second
    // allocation for a token it already has.
    let mut relays = Relays::new();
    let now = Instant::now();

    assert!(relays.allocate(TOKEN, now));
    assert!(relays.allocate(TOKEN, now));

    assert_eq!(relays.waiting(), 1);
}

#[test]
fn a_datagram_the_size_of_a_token_is_never_confused_with_traffic() {
    // The relay tells a presentation from traffic by length alone. That is only safe because a
    // sealed packet is at least twenty-four bytes and a token is eight, and this is the test
    // that says so.
    const SEAL_OVERHEAD: usize = 24;

    assert!(
        TOKEN.len() < SEAL_OVERHEAD,
        "a token is as long as the shortest sealed packet, so the two cannot be told apart"
    );

    let mut relays = Relays::new();
    let now = opened(&mut relays);

    // The shortest thing a session can send is still forwarded rather than read as a token.
    assert_eq!(
        relays.accept(host(), &[0u8; SEAL_OVERHEAD], now),
        Forward::To(client())
    );
}

#[test]
fn both_asks_about_one_pair_get_the_same_token() {
    // The bug the first end to end run found. Each peer discovers on its own that punching
    // failed and asks separately; two tokens leave each waiting at the relay for a partner
    // that was never coming, and nothing anywhere says why.
    let mut relays = Relays::new();
    let now = Instant::now();

    let host_key = [0xa1; 32];
    let client_key = [0xb2; 32];

    let first = relays
        .token_for(host_key, client_key, TOKEN, now)
        .expect("a token");
    let second = relays
        .token_for(host_key, client_key, OTHER, now)
        .expect("a token");

    assert_eq!(first, second, "the two asks were given different tokens");
    assert_eq!(
        relays.waiting(),
        1,
        "a second ask allocated a second session"
    );
}

#[test]
fn a_different_pair_gets_a_different_token() {
    let mut relays = Relays::new();
    let now = Instant::now();

    let first = relays
        .token_for([0xa1; 32], [0xb2; 32], TOKEN, now)
        .expect("a token");
    let second = relays
        .token_for([0xa1; 32], [0xc3; 32], OTHER, now)
        .expect("a token");

    assert_ne!(first, second);
    assert_eq!(relays.waiting(), 2);
}

#[test]
fn a_pair_can_relay_again_after_its_session_ended() {
    // Otherwise the second session of the day between two machines would be handed a token
    // whose relay had already been swept away, and would wait at a port with nothing behind
    // it.
    let mut relays = Relays::new();
    let now = Instant::now();

    let host_key = [0xa1; 32];
    let client_key = [0xb2; 32];

    let first = relays
        .token_for(host_key, client_key, TOKEN, now)
        .expect("a token");

    relays.expire(now + PAIRING_TTL + Duration::from_secs(1));

    let second = relays
        .token_for(
            host_key,
            client_key,
            OTHER,
            now + PAIRING_TTL + Duration::from_secs(2),
        )
        .expect("a token");

    assert_ne!(first, second, "a swept-away token was handed out again");
}
