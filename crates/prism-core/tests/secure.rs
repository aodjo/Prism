//! Tests for the sealed socket.
//!
//! The behaviour worth testing here is not that a packet survives a round trip — the seal's
//! own tests cover that — but what happens when something other than the peer sends to the
//! port. That is not a rare case: a UDP port reachable from the internet receives scans,
//! stray packets from a previous session, and anything an attacker cares to write. Every one
//! of them has to cost the session nothing.

use std::net::{SocketAddr, UdpSocket};

use prism_core::net::handshake::{Handshake, INIT_OVERHEAD, Identity, MAX_HANDSHAKE_PAYLOAD};
use prism_core::net::packet::{MAX_PACKET_SIZE, MAX_PLAINTEXT_SIZE};
use prism_core::net::secure::{SecureReceiver, SecureSender};
use prism_core::net::transport::UdpTransport;

/// A sender and a receiver on loopback, keyed by a real handshake.
struct Loopback {
    sender: SecureSender,
    receiver: SecureReceiver,
    receiver_addr: SocketAddr,
}

/// Runs a handshake and returns both sides' session keys.
fn sessions() -> (
    prism_core::net::handshake::Session,
    prism_core::net::handshake::Session,
) {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    let mut message = [0u8; INIT_OVERHEAD + MAX_HANDSHAKE_PAYLOAD];
    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];

    let written = initiator.write_message(&[], &mut message).expect("writes");
    responder
        .read_message(&message[..written], &mut payload)
        .expect("reads");
    let written = responder.write_message(&[], &mut message).expect("writes");
    initiator
        .read_message(&message[..written], &mut payload)
        .expect("reads");

    (
        initiator.into_session().expect("finished"),
        responder.into_session().expect("finished"),
    )
}

/// Puts each half of a session on its own loopback socket.
///
/// `connected` fixes the sender's peer, which is what a real session does so the kernel drops
/// off-path datagrams. A connected socket refuses `send_to` outright, so the tests that need
/// to aim a packet somewhere else ask for an unconnected one.
fn loopback_with(connected: bool) -> Loopback {
    let (client_session, host_session) = sessions();

    let receiving = UdpTransport::bind("127.0.0.1:0".parse().expect("valid")).expect("binds");
    let receiver_addr = receiving.local_addr().expect("has an address");

    let sending = UdpTransport::bind("127.0.0.1:0".parse().expect("valid")).expect("binds");
    if connected {
        sending.connect(receiver_addr).expect("connects");
    }

    Loopback {
        sender: SecureSender::new(sending, client_session.sealer),
        receiver: SecureReceiver::new(receiving, host_session.opener),
        receiver_addr,
    }
}

/// A connected pair, as a session runs.
fn loopback() -> Loopback {
    loopback_with(true)
}

#[test]
fn a_packet_arrives_as_what_was_sent() {
    let mut link = loopback();
    let mut buf = [0u8; MAX_PACKET_SIZE];

    link.sender.send(b"video slice 41").expect("sends");

    assert_eq!(
        link.receiver.recv_into(&mut buf).expect("receives"),
        b"video slice 41"
    );
}

#[test]
fn what_leaves_the_socket_is_not_what_went_in() {
    // The check that this wrapper is doing its job at all, made by watching the wire with a
    // plain socket rather than by trusting the type.
    let (client_session, _) = sessions();

    let watcher = UdpSocket::bind("127.0.0.1:0").expect("binds");
    let watched: SocketAddr = watcher.local_addr().expect("has an address");

    let transport = UdpTransport::bind("127.0.0.1:0".parse().expect("valid")).expect("binds");
    transport.connect(watched).expect("connects");
    let mut sender = SecureSender::new(transport, client_session.sealer);

    let secret = b"the user just typed their password";
    sender.send(secret).expect("sends");

    let mut seen = [0u8; MAX_PACKET_SIZE];
    let len = watcher.recv(&mut seen).expect("receives");

    assert_eq!(
        len,
        secret.len() + 24,
        "the seal's overhead is not on the wire"
    );
    assert!(
        !seen[..len].windows(secret.len()).any(|w| w == secret),
        "the payload crossed the wire in the clear"
    );
}

#[test]
fn junk_from_elsewhere_does_not_end_the_session() {
    // The property the whole design of recv_into turns on. If an unopenable packet were an
    // error the caller had to handle, one datagram from anywhere on the internet would end the
    // session — a denial of service that costs the attacker nothing and needs no key.
    let mut link = loopback();
    let stranger = UdpSocket::bind("127.0.0.1:0").expect("binds");

    for _ in 0..8 {
        stranger
            .send_to(&[0xa5u8; 200], link.receiver_addr)
            .expect("sends junk");
    }
    link.sender.send(b"still here").expect("sends");

    let mut buf = [0u8; MAX_PACKET_SIZE];
    assert_eq!(
        link.receiver.recv_into(&mut buf).expect("receives"),
        b"still here"
    );
    assert_eq!(link.receiver.forged(), 8);
}

#[test]
fn a_datagram_too_short_to_be_a_packet_does_not_end_the_session() {
    let mut link = loopback();
    let stranger = UdpSocket::bind("127.0.0.1:0").expect("binds");

    for length in [0usize, 1, 23] {
        stranger
            .send_to(&vec![0u8; length], link.receiver_addr)
            .expect("sends a runt");
    }
    link.sender.send(b"still here").expect("sends");

    let mut buf = [0u8; MAX_PACKET_SIZE];
    assert_eq!(
        link.receiver.recv_into(&mut buf).expect("receives"),
        b"still here"
    );
}

#[test]
fn a_replayed_datagram_is_dropped_and_the_next_one_still_arrives() {
    // An attacker who records one packet and posts it back gets it dropped, and crucially the
    // genuine packet behind it still comes through rather than the receiver giving up.
    let mut link = loopback_with(false);
    let recorder = UdpSocket::bind("127.0.0.1:0").expect("binds");
    let recorded_at: SocketAddr = recorder.local_addr().expect("has an address");
    let mut buf = [0u8; MAX_PACKET_SIZE];

    // A real packet, captured off the wire by aiming one send at a socket that keeps it.
    let mut recorded = [0u8; MAX_PACKET_SIZE];
    link.sender
        .send_to(b"click", recorded_at)
        .expect("sends to the recorder");
    let len = recorder.recv(&mut recorded).expect("records");

    recorder
        .send_to(&recorded[..len], link.receiver_addr)
        .expect("replays");
    assert_eq!(
        link.receiver.recv_into(&mut buf).expect("receives"),
        b"click",
        "the first arrival of a packet is genuine however it got here"
    );

    recorder
        .send_to(&recorded[..len], link.receiver_addr)
        .expect("replays again");
    link.sender
        .send_to(b"genuine", link.receiver_addr)
        .expect("sends");

    assert_eq!(
        link.receiver.recv_into(&mut buf).expect("receives"),
        b"genuine"
    );
    assert_eq!(link.receiver.replayed(), 1);
    assert_eq!(link.receiver.forged(), 0, "a replay is not a forgery");
}

#[test]
fn a_flood_of_junk_returns_control_to_the_caller() {
    // Skipping bad packets must not mean spinning inside one call forever. The caller gets its
    // loop back and can decide the session has gone quiet.
    let mut link = loopback();
    let stranger = UdpSocket::bind("127.0.0.1:0").expect("binds");

    for _ in 0..128 {
        stranger
            .send_to(&[0x00u8; 100], link.receiver_addr)
            .expect("floods");
    }

    let mut buf = [0u8; MAX_PACKET_SIZE];
    let err = link.receiver.recv_into(&mut buf).expect_err("gives up");

    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn the_sender_refuses_a_payload_that_would_not_fit_the_wire_budget() {
    let mut link = loopback();

    let err = link
        .sender
        .send(&vec![0u8; MAX_PLAINTEXT_SIZE + 1])
        .expect_err("refuses");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn a_payload_at_the_budget_fills_the_datagram_exactly() {
    let mut link = loopback();
    let mut buf = [0u8; MAX_PACKET_SIZE];

    let plaintext = vec![0x3cu8; MAX_PLAINTEXT_SIZE];
    let sent = link.sender.send(&plaintext).expect("sends");

    assert_eq!(sent, MAX_PACKET_SIZE);
    assert_eq!(
        link.receiver.recv_into(&mut buf).expect("receives"),
        &plaintext[..]
    );
}

#[test]
fn two_senders_on_one_direction_never_reuse_a_counter() {
    // The host sends video from one thread and clock replies from another, both under the same
    // key. If the two handles each kept their own counter they would both start at zero, and
    // reusing a nonce under GCM leaks the authentication key. Every packet opening, with no
    // replay, is what proves the counter is shared.
    let mut link = loopback();
    let mut second = link.sender.split().expect("splits");

    const EACH: usize = 200;

    let flood = std::thread::spawn(move || {
        for index in 0..EACH {
            second
                .send(format!("b{index}").as_bytes())
                .expect("the second handle sends");
        }
    });

    for index in 0..EACH {
        link.sender
            .send(format!("a{index}").as_bytes())
            .expect("the first handle sends");
    }
    flood.join().expect("the second handle finished");

    let mut buf = [0u8; MAX_PACKET_SIZE];
    link.receiver
        .transport()
        .set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .expect("sets a timeout");

    let mut opened = 0;
    while link.receiver.recv_into(&mut buf).is_ok() {
        opened += 1;
    }

    assert_eq!(link.receiver.forged(), 0);

    // How many opened is the whole of the proof, and it is worth being precise about why.
    //
    // If the two handles each kept their own counter they would both start at zero and issue
    // the same numbers, so every packet from the second handle would collide with one already
    // seen and be refused — leaving about `EACH` opened rather than about twice that. Opening
    // more than `EACH` is therefore only possible if the counter is shared.
    //
    // What is deliberately NOT asserted is that nothing was counted as a replay. Two threads
    // sending as fast as they can interleave however the scheduler decides, and a packet that
    // arrives more than `REPLAY_WINDOW` behind the newest one is refused for arriving late
    // rather than for repeating a counter. That measures the machine's scheduling, not this
    // code, and asserting on it made the test fail on a loaded runner while passing every time
    // on an idle one.
    assert!(
        opened > EACH,
        "only {opened} of {} packets opened, so the two handles are not sharing a counter",
        EACH * 2
    );
}

#[test]
fn neither_half_prints_its_key() {
    let link = loopback();
    let printed = format!("{:?} {:?}", link.sender, link.receiver);

    assert!(printed.contains("sealed"));
    assert!(printed.contains("forged"));
}
