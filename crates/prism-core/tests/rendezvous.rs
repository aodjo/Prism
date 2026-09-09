//! Tests for the rendezvous protocol.
//!
//! Two things are being guarded. The codec has to survive everything an open UDP port
//! receives, which is mostly not this protocol. And the registration challenge has to be
//! answerable only by the holder of the key being claimed, because without that anyone who
//! has ever seen a host's public key can make that host unreachable.

use std::net::SocketAddr;

use prism_core::net::handshake::Identity;
use prism_core::net::rendezvous::{
    MAX_MESSAGE_LEN, Message, PROOF_LEN, RELAY_TOKEN_LEN, RendezvousError, answer, challenge,
};

/// A key that is not all one byte, so a field-order mistake would show.
const HOST: [u8; 32] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
    0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0,
];

/// A second key, distinct from the first in every byte.
const CLIENT: [u8; 32] = [0x5c; 32];

/// An IPv4 address with a port that would catch a byte-order mistake.
fn v4() -> SocketAddr {
    "203.0.113.42:47201".parse().expect("valid")
}

/// An IPv6 address, because a home connection may well have one and no NAT at all.
fn v6() -> SocketAddr {
    "[2001:db8::1234:5678]:9999".parse().expect("valid")
}

/// Every message this version defines, for the round trip.
fn every_message() -> Vec<Message> {
    vec![
        Message::Register { host: HOST },
        Message::Challenge {
            ephemeral: CLIENT,
            sealed: [0x7a; PROOF_LEN + 16],
        },
        Message::Prove {
            host: HOST,
            secret: [0x3b; PROOF_LEN],
        },
        Message::Registered { observed: v4() },
        Message::Registered { observed: v6() },
        Message::Connect {
            host: HOST,
            client: CLIENT,
        },
        Message::Incoming {
            client: CLIENT,
            address: v4(),
        },
        Message::Found {
            address: v6(),
            observed: v4(),
        },
        Message::UnknownHost,
        Message::Keepalive { host: HOST },
        Message::Relay {
            host: HOST,
            client: CLIENT,
        },
        Message::Relaying {
            port: 47_301,
            token: [0x5a, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
        },
    ]
}

#[test]
fn every_message_survives_a_round_trip() {
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    for message in every_message() {
        let len = message.encode_into(&mut buf).expect("encodes");
        let back = Message::decode(&buf[..len]).expect("decodes");

        assert_eq!(back, message);
    }
}

#[test]
fn no_message_exceeds_the_ceiling_its_buffers_are_sized_for() {
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    for message in every_message() {
        let len = message.encode_into(&mut buf).expect("encodes");
        assert!(len <= MAX_MESSAGE_LEN, "{message:?} needed {len} bytes");
    }
}

#[test]
fn an_ipv6_address_survives_intact() {
    // A home connection with IPv6 has no NAT at all, which is the easiest case there is — so
    // it must not be the one the codec gets wrong.
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    let message = Message::Found {
        address: v6(),
        observed: v6(),
    };
    let len = message.encode_into(&mut buf).expect("encodes");

    assert_eq!(Message::decode(&buf[..len]).expect("decodes"), message);
}

#[test]
fn an_unknown_type_byte_is_refused() {
    // The version after this one will send messages this one has never heard of, and acting
    // on a message only half understood is how a parser becomes a vulnerability.
    //
    // The low one has to stay ahead of the last tag `net::rendezvous` defines, or this stops
    // testing what it says it tests and starts testing whatever was added last.
    for tag in [0x00u8, 0x0e, 0x7f, 0xff] {
        assert_eq!(
            Message::decode(&[tag]),
            Err(RendezvousError::UnknownType { tag })
        );
    }
}

#[test]
fn an_empty_datagram_is_refused() {
    assert_eq!(Message::decode(&[]), Err(RendezvousError::Empty));
}

#[test]
fn a_truncated_message_is_refused() {
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    for message in every_message() {
        let len = message.encode_into(&mut buf).expect("encodes");

        for short in 1..len {
            assert!(
                Message::decode(&buf[..short]).is_err(),
                "{message:?} decoded from only {short} of {len} bytes"
            );
        }
    }
}

#[test]
fn a_message_with_bytes_left_over_is_refused() {
    // A trailing byte means the sender and this reader disagree about the format. Ignoring it
    // would let two versions appear to interoperate while meaning different things.
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    for message in every_message() {
        let len = message.encode_into(&mut buf).expect("encodes");
        buf[len] = 0x00;

        assert!(
            Message::decode(&buf[..len + 1]).is_err(),
            "{message:?} decoded with a trailing byte"
        );
    }
}

#[test]
fn an_address_family_that_is_neither_four_nor_six_is_refused() {
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    let len = Message::Registered { observed: v4() }
        .encode_into(&mut buf)
        .expect("encodes");
    buf[1] = 5;

    assert_eq!(
        Message::decode(&buf[..len]),
        Err(RendezvousError::BadAddress { family: 5 })
    );
}

#[test]
fn random_bytes_never_panic() {
    // Everything that reaches an open UDP port lands in this decoder.
    let mut state = 0x2545_f491_4f6c_dd1du64;

    for _ in 0..20_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;

        let len = (state % 200) as usize;
        let junk: Vec<u8> = (0..len).map(|i| (state >> (i % 56)) as u8).collect();

        let _ = Message::decode(&junk);
    }
}

#[test]
fn a_buffer_too_small_to_hold_a_message_is_refused_rather_than_overrun() {
    let message = Message::Connect {
        host: HOST,
        client: CLIENT,
    };

    for size in 0..65 {
        let mut buf = vec![0u8; size];
        assert!(
            message.encode_into(&mut buf).is_err(),
            "a {size}-byte buffer accepted a 65-byte message"
        );
    }
}

#[test]
fn only_the_holder_of_the_key_can_answer_a_challenge() {
    // The whole point of challenging a registration. Without this, anyone who has ever seen a
    // host's public key — every machine it has paired with — could register it and point its
    // clients somewhere else.
    let host = Identity::generate().expect("generates");

    let (message, expected) = challenge(host.public()).expect("challenges");

    let Message::Challenge { ephemeral, sealed } = message else {
        panic!("challenge produced the wrong message");
    };

    assert_eq!(
        answer(host.private(), &ephemeral, &sealed).expect("answers"),
        expected
    );
}

#[test]
fn another_key_cannot_answer_a_challenge() {
    let host = Identity::generate().expect("generates");
    let impostor = Identity::generate().expect("generates");

    let (message, _) = challenge(host.public()).expect("challenges");
    let Message::Challenge { ephemeral, sealed } = message else {
        panic!("wrong message");
    };

    assert_eq!(
        answer(impostor.private(), &ephemeral, &sealed),
        Err(RendezvousError::NotProven)
    );
}

#[test]
fn an_altered_challenge_cannot_be_answered() {
    let host = Identity::generate().expect("generates");

    let (message, _) = challenge(host.public()).expect("challenges");
    let Message::Challenge {
        ephemeral,
        mut sealed,
    } = message
    else {
        panic!("wrong message");
    };

    sealed[0] ^= 0x01;

    assert_eq!(
        answer(host.private(), &ephemeral, &sealed),
        Err(RendezvousError::NotProven)
    );
}

#[test]
fn each_challenge_is_new() {
    // A fixed nonce seals every challenge, which is safe only because the server makes a fresh
    // ephemeral key each time. Two identical challenges would mean two seals under one key
    // with one nonce.
    let host = Identity::generate().expect("generates");

    let (first, first_secret) = challenge(host.public()).expect("challenges");
    let (second, second_secret) = challenge(host.public()).expect("challenges");

    assert_ne!(first, second);
    assert_ne!(first_secret, second_secret);
}

#[test]
fn a_relay_token_is_shorter_than_the_shortest_sealed_packet() {
    // The relay tells a peer presenting its token from a peer sending traffic by length alone.
    // That is only safe while a token cannot be as long as a packet, and this is where that
    // stops being an assumption.
    const {
        assert!(
            RELAY_TOKEN_LEN < prism_core::net::packet::SEAL_OVERHEAD,
            "a token is as long as the shortest sealed packet, so the relay cannot tell them \
             apart"
        );
    };
}
