//! Tests for the Noise_IK session handshake.
//!
//! The handshake is where authentication actually happens; the seal only enforces what the
//! handshake decided. So the cases that matter here are the ones where a peer is not who it
//! claims: an unpaired key, a key swapped in transit, a responder impersonated. Each has to
//! end in a handshake that fails rather than a session that works.

use prism_core::net::packet::MAX_PACKET_SIZE;

use prism_core::net::handshake::{
    Handshake, HandshakeError, INIT_OVERHEAD, Identity, MAX_HANDSHAKE_PAYLOAD, RESPONSE_OVERHEAD,
};

/// Runs a full exchange and returns both sides' sessions.
fn complete() -> (
    prism_core::net::handshake::Session,
    prism_core::net::handshake::Session,
) {
    let client = Identity::generate().expect("an identity is generatable");
    let host = Identity::generate().expect("an identity is generatable");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    let mut first = [0u8; INIT_OVERHEAD];
    let written = initiator.write_message(&[], &mut first).expect("writes");

    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];
    responder
        .read_message(&first[..written], &mut payload)
        .expect("reads");

    let mut second = [0u8; RESPONSE_OVERHEAD];
    let written = responder.write_message(&[], &mut second).expect("writes");
    initiator
        .read_message(&second[..written], &mut payload)
        .expect("reads");

    (
        initiator.into_session().expect("finished"),
        responder.into_session().expect("finished"),
    )
}

#[test]
fn a_handshake_takes_one_round_trip() {
    // The property the whole pattern was chosen for. Two messages, and the session is live.
    // Anything more would put an extra RTT in front of every reconnect.
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

    assert!(
        !responder.is_finished(),
        "the responder still has to answer"
    );

    let written = responder.write_message(&[], &mut message).expect("writes");
    initiator
        .read_message(&message[..written], &mut payload)
        .expect("reads");

    assert!(initiator.is_finished());
    assert!(responder.is_finished());
}

#[test]
fn the_two_sides_derive_keys_that_open_each_others_packets() {
    // The check that the split was taken from the right end. Getting the two halves backwards
    // produces a handshake that succeeds and a session where nothing decrypts, which is a
    // failure that looks like a network problem.
    let (mut client, mut host) = complete();

    let mut packet = vec![0u8; b"input event".len() + 24];
    client
        .sealer
        .seal(b"input event", &mut packet)
        .expect("seals");
    assert_eq!(
        host.opener.open(&mut packet).expect("opens"),
        b"input event"
    );

    let mut packet = vec![0u8; b"video slice".len() + 24];
    host.sealer
        .seal(b"video slice", &mut packet)
        .expect("seals");
    assert_eq!(
        client.opener.open(&mut packet).expect("opens"),
        b"video slice"
    );
}

#[test]
fn each_direction_has_its_own_key() {
    // If both directions shared a key, each side's counters would collide with the other's,
    // and colliding counters under GCM is nonce reuse. This is the test that catches it: the
    // client's own packet must not open with the client's own opener.
    let (mut client, _) = complete();

    let mut packet = vec![0u8; 5 + 24];
    client.sealer.seal(b"hello", &mut packet).expect("seals");

    assert!(
        client.opener.open(&mut packet).is_err(),
        "one key is doing both directions"
    );
}

#[test]
fn the_responder_learns_who_the_initiator_is() {
    // The whole authentication story: the responder reads the first message, gets a static
    // key out of it, and checks that key against what pairing recorded. Without this the
    // handshake authenticates nothing at all.
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    assert_eq!(
        responder.peer_static(),
        None,
        "the responder cannot know the peer before reading anything"
    );

    let mut message = [0u8; INIT_OVERHEAD];
    let written = initiator.write_message(&[], &mut message).expect("writes");

    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];
    responder
        .read_message(&message[..written], &mut payload)
        .expect("reads");

    assert_eq!(responder.peer_static().as_ref(), Some(client.public()));
}

#[test]
fn the_initiator_confirms_it_reached_the_host_it_meant_to() {
    let (client_session, host_session) = complete();

    assert_ne!(client_session.peer_static, host_session.peer_static);
}

#[test]
fn a_handshake_aimed_at_the_wrong_host_key_fails() {
    // What an attacker gets for standing in the middle with their own key: nothing. The
    // initiator encrypts to the key pairing gave it, and only the holder of that key can read
    // the first message.
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");
    let impostor = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, impostor.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    let mut message = [0u8; INIT_OVERHEAD];
    let written = initiator.write_message(&[], &mut message).expect("writes");

    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];
    assert_eq!(
        responder.read_message(&message[..written], &mut payload),
        Err(HandshakeError::NotAuthentic)
    );
}

#[test]
fn an_altered_first_message_is_refused() {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    let mut message = [0u8; INIT_OVERHEAD];
    let written = initiator.write_message(&[], &mut message).expect("writes");
    message[40] ^= 0x01;

    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];
    assert_eq!(
        responder.read_message(&message[..written], &mut payload),
        Err(HandshakeError::NotAuthentic)
    );
}

#[test]
fn random_bytes_arriving_at_the_port_are_refused_rather_than_crashing() {
    // A responder is reachable by anyone who can find the port. Every one of these lands in
    // read_message, so it has to be a refusal and never a panic.
    let host = Identity::generate().expect("generates");
    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];

    for length in [0usize, 1, 32, 95, INIT_OVERHEAD, 200, 1200] {
        let mut responder = Handshake::responder(&host).expect("builds");
        let junk = vec![0xa5u8; length];

        assert_eq!(
            responder.read_message(&junk, &mut payload),
            Err(HandshakeError::NotAuthentic),
            "junk of {length} bytes was not refused"
        );
    }
}

#[test]
fn a_payload_rides_inside_each_message() {
    // The one round trip is only worth having if it carries the negotiation with it. The
    // client's requested parameters go out in the first message and the host's answer comes
    // back in the second, so the session is configured by the time it is live.
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    let request = b"h264,hevc 2560x1440@120";
    let answer = b"h264 1920x1080@60";

    let mut message = [0u8; INIT_OVERHEAD + MAX_HANDSHAKE_PAYLOAD];
    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];

    let written = initiator
        .write_message(request, &mut message)
        .expect("writes");
    let read = responder
        .read_message(&message[..written], &mut payload)
        .expect("reads");
    assert_eq!(&payload[..read], request);

    let written = responder
        .write_message(answer, &mut message)
        .expect("writes");
    let read = initiator
        .read_message(&message[..written], &mut payload)
        .expect("reads");
    assert_eq!(&payload[..read], answer);
}

#[test]
fn the_message_overheads_are_what_the_buffers_are_sized_for() {
    // These constants size every handshake buffer in the transport. If the pattern ever
    // changed, a wrong constant would show up as a truncated message rather than as an error.
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    let mut message = [0u8; INIT_OVERHEAD + MAX_HANDSHAKE_PAYLOAD];
    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];

    let first = initiator
        .write_message(b"abcd", &mut message)
        .expect("writes");
    assert_eq!(first, INIT_OVERHEAD + 4);

    responder
        .read_message(&message[..first], &mut payload)
        .expect("reads");

    let second = responder
        .write_message(b"abcd", &mut message)
        .expect("writes");
    assert_eq!(second, RESPONSE_OVERHEAD + 4);
}

#[test]
fn both_messages_fit_in_one_datagram() {
    // A fragmented handshake fails on exactly the paths that are hardest to debug, so the
    // ceiling is checked rather than assumed.
    const { assert!(INIT_OVERHEAD + MAX_HANDSHAKE_PAYLOAD <= MAX_PACKET_SIZE) };
    const { assert!(RESPONSE_OVERHEAD + MAX_HANDSHAKE_PAYLOAD <= MAX_PACKET_SIZE) };
}

#[test]
fn speaking_out_of_turn_is_refused() {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut responder = Handshake::responder(&host).expect("builds");

    let mut message = [0u8; INIT_OVERHEAD + MAX_HANDSHAKE_PAYLOAD];
    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];

    assert_eq!(
        responder.write_message(&[], &mut message),
        Err(HandshakeError::OutOfTurn),
        "the responder cannot answer a message it has not received"
    );

    let written = initiator.write_message(&[], &mut message).expect("writes");
    assert_eq!(
        initiator.write_message(&[], &mut message),
        Err(HandshakeError::OutOfTurn),
        "the initiator cannot send twice in a row"
    );

    responder
        .read_message(&message[..written], &mut payload)
        .expect("reads");
    assert_eq!(
        responder.read_message(&message[..written], &mut payload),
        Err(HandshakeError::OutOfTurn),
        "a replayed first message must not restart the responder mid-handshake"
    );
}

#[test]
fn session_keys_are_refused_until_the_handshake_finishes() {
    // Handing out keys early would give a session the peer never authenticated.
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let initiator = Handshake::initiator(&client, host.public()).expect("builds");

    assert!(matches!(
        initiator.into_session(),
        Err(HandshakeError::Unfinished)
    ));
}

#[test]
fn a_payload_over_the_ceiling_is_refused() {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Handshake::initiator(&client, host.public()).expect("builds");
    let mut message = vec![0u8; 4096];

    assert_eq!(
        initiator.write_message(&vec![0u8; MAX_HANDSHAKE_PAYLOAD + 1], &mut message),
        Err(HandshakeError::PayloadTooLarge {
            actual: MAX_HANDSHAKE_PAYLOAD + 1
        })
    );
}

#[test]
fn an_identity_survives_being_stored_and_reloaded() {
    // The host generates its identity once and reads it back on every later run. A derived
    // public half that did not match would make the machine unrecognisable to every peer that
    // ever paired with it.
    let original = Identity::generate().expect("generates");
    let reloaded = Identity::from_private(original.private()).expect("reloads");

    assert_eq!(original.public(), reloaded.public());

    let client = Identity::generate().expect("generates");
    let mut initiator = Handshake::initiator(&client, original.public()).expect("builds");
    let mut responder = Handshake::responder(&reloaded).expect("builds");

    let mut message = [0u8; INIT_OVERHEAD];
    let written = initiator.write_message(&[], &mut message).expect("writes");

    let mut payload = [0u8; MAX_HANDSHAKE_PAYLOAD];
    responder
        .read_message(&message[..written], &mut payload)
        .expect("a reloaded identity is the same identity");
}

#[test]
fn two_identities_are_never_the_same() {
    let first = Identity::generate().expect("generates");
    let second = Identity::generate().expect("generates");

    assert_ne!(first.public(), second.public());
    assert_ne!(first.private(), second.private());
}

#[test]
fn two_handshakes_between_the_same_pair_derive_different_keys() {
    // Forward secrecy: the ephemeral keys are what make each session's keys unrelated, so a
    // static key recovered later opens nothing that was recorded earlier.
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut keys = Vec::new();

    for _ in 0..2 {
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

        let mut session = initiator.into_session().expect("finished");
        let mut sealed = vec![0u8; 4 + 24];
        session.sealer.seal(b"same", &mut sealed).expect("seals");
        keys.push(sealed);
    }

    assert_ne!(
        keys[0], keys[1],
        "two sessions produced identical ciphertext for identical plaintext, so the \
         ephemeral keys are not doing their job"
    );
}

#[test]
fn an_identity_does_not_print_its_private_key() {
    let identity = Identity::generate().expect("generates");
    let printed = format!("{identity:?}");

    let private = identity
        .private()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    assert!(!printed.contains(&private), "the private key reached Debug");
}

// The drivers below are what the transport actually uses. Everything they add over the raw
// state machine exists because the path is UDP: messages are lost, arrive twice, and arrive
// from strangers.

use prism_core::net::handshake::{Answer, Initiator, PeerPolicy, Responder};

/// A reply buffer large enough for any handshake answer.
const REPLY: usize = RESPONSE_OVERHEAD + MAX_HANDSHAKE_PAYLOAD;

#[test]
fn the_drivers_complete_a_handshake_and_carry_their_payloads() {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");
    let client_public = *client.public();
    let host_public = *host.public();

    let mut initiator =
        Initiator::new(&client, host.public(), b"hello from the client").expect("starts");
    let mut responder = Responder::new(host, PeerPolicy::Paired(vec![client_public]));

    let mut reply = [0u8; REPLY];
    let Answer::Reply(len) = responder.accept(
        initiator.first_message(),
        b"hello from the host",
        &mut reply,
    ) else {
        panic!("the responder did not answer a genuine first message");
    };

    assert!(initiator.accept(&reply[..len]), "the answer was refused");

    let client_side = initiator.take().expect("a session");
    let host_side = responder.take().expect("a session");

    assert_eq!(client_side.peer_payload, b"hello from the host");
    assert_eq!(host_side.peer_payload, b"hello from the client");

    // Each side ends up naming the other, which is what the caller checks against pairing.
    assert_eq!(client_side.session.peer_static, host_public);
    assert_eq!(host_side.session.peer_static, client_public);
}

#[test]
fn a_lost_answer_is_recovered_by_resending_the_same_first_message() {
    // The case this driver exists for. If the responder started over instead of repeating its
    // answer, it would derive a second set of keys while the initiator waited forever for an
    // answer to the first — a session that never starts, with nothing in the logs to say why.
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Initiator::new(&client, host.public(), &[]).expect("starts");
    let mut responder = Responder::new(host, PeerPolicy::Any);

    let mut first_reply = [0u8; REPLY];
    let Answer::Reply(first_len) =
        responder.accept(initiator.first_message(), &[], &mut first_reply)
    else {
        panic!("no answer");
    };
    let established = responder.take().expect("a session");

    // That answer is lost on the way back, so the initiator sends the same bytes again.
    let mut second_reply = [0u8; REPLY];
    let Answer::Reply(second_len) =
        responder.accept(initiator.first_message(), &[], &mut second_reply)
    else {
        panic!("the retransmission went unanswered");
    };

    assert_eq!(
        &first_reply[..first_len],
        &second_reply[..second_len],
        "the responder answered a retransmission with different bytes"
    );
    assert!(
        responder.take().is_none(),
        "a retransmission produced a second session"
    );

    assert!(initiator.accept(&second_reply[..second_len]));
    let mut client_side = initiator.take().expect("a session");
    let mut host_side = established;

    let mut packet = vec![0u8; 4 + 24];
    client_side
        .session
        .sealer
        .seal(b"live", &mut packet)
        .expect("seals");
    assert_eq!(
        host_side.session.opener.open(&mut packet).expect("opens"),
        b"live",
        "the keys the responder kept do not match the ones the initiator ended up with"
    );
}

#[test]
fn a_peer_the_policy_does_not_admit_gets_silence() {
    // Silence rather than a refusal. A rejection would confirm to an unpaired caller that it
    // had found a live host, which is exactly what a scan is looking for.
    let stranger = Identity::generate().expect("generates");
    let paired = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let initiator = Initiator::new(&stranger, host.public(), &[]).expect("starts");
    let mut responder = Responder::new(host, PeerPolicy::Paired(vec![*paired.public()]));

    let mut reply = [0u8; REPLY];
    assert_eq!(
        responder.accept(initiator.first_message(), &[], &mut reply),
        Answer::Ignored
    );
    assert!(responder.take().is_none(), "an unpaired peer got a session");
}

#[test]
fn one_paired_key_among_several_is_admitted() {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");
    let others: Vec<[u8; 32]> = (0..3)
        .map(|_| *Identity::generate().expect("generates").public())
        .collect();

    let mut allowed = others;
    allowed.insert(2, *client.public());

    let initiator = Initiator::new(&client, host.public(), &[]).expect("starts");
    let mut responder = Responder::new(host, PeerPolicy::Paired(allowed));

    let mut reply = [0u8; REPLY];
    assert!(matches!(
        responder.accept(initiator.first_message(), &[], &mut reply),
        Answer::Reply(_)
    ));
}

#[test]
fn junk_never_gets_an_answer() {
    // Every datagram that reaches an open UDP port lands here. None of them may produce a
    // reply, or the port becomes an amplifier pointed at whoever the source address claims
    // to be.
    let host = Identity::generate().expect("generates");
    let mut responder = Responder::new(host, PeerPolicy::Any);
    let mut reply = [0u8; REPLY];

    for length in [0usize, 1, 32, 95, 96, 400, 1200] {
        assert_eq!(
            responder.accept(&vec![0x7fu8; length], &[], &mut reply),
            Answer::Ignored,
            "junk of {length} bytes drew a reply"
        );
    }
}

#[test]
fn a_second_different_handshake_does_not_displace_a_live_session() {
    // Once a session is running, a new first message from anywhere would otherwise tear it
    // down and replace it — a reset any observer could cause by replaying an old capture or
    // simply by connecting.
    let client = Identity::generate().expect("generates");
    let intruder = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Initiator::new(&client, host.public(), &[]).expect("starts");
    let mut responder = Responder::new(host.clone(), PeerPolicy::Any);

    let mut reply = [0u8; REPLY];
    let Answer::Reply(len) = responder.accept(initiator.first_message(), &[], &mut reply) else {
        panic!("no answer");
    };
    assert!(initiator.accept(&reply[..len]));
    let live = responder.take().expect("a session");

    let second = Initiator::new(&intruder, host.public(), &[]).expect("starts");
    assert_eq!(
        responder.accept(second.first_message(), &[], &mut reply),
        Answer::Ignored
    );
    assert!(responder.take().is_none());

    // And the session that was already running still works.
    let mut client_side = initiator.take().expect("a session");
    let mut host_side = live;
    let mut packet = vec![0u8; 5 + 24];
    client_side
        .session
        .sealer
        .seal(b"still", &mut packet)
        .expect("seals");
    assert_eq!(
        host_side.session.opener.open(&mut packet).expect("opens"),
        b"still"
    );
}

#[test]
fn the_initiator_ignores_everything_but_its_answer() {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Initiator::new(&client, host.public(), &[]).expect("starts");

    for length in [0usize, 1, 47, 48, 300] {
        assert!(
            !initiator.accept(&vec![0x22u8; length]),
            "junk of {length} bytes completed the handshake"
        );
    }
    assert!(initiator.take().is_none());
}

#[test]
fn an_answer_arriving_twice_does_not_produce_a_second_session() {
    let client = Identity::generate().expect("generates");
    let host = Identity::generate().expect("generates");

    let mut initiator = Initiator::new(&client, host.public(), &[]).expect("starts");
    let mut responder = Responder::new(host, PeerPolicy::Any);

    let mut reply = [0u8; REPLY];
    let Answer::Reply(len) = responder.accept(initiator.first_message(), &[], &mut reply) else {
        panic!("no answer");
    };

    assert!(initiator.accept(&reply[..len]));
    assert!(
        !initiator.accept(&reply[..len]),
        "a duplicated answer was accepted a second time"
    );
    assert!(initiator.take().is_some());
    assert!(initiator.take().is_none());
}
