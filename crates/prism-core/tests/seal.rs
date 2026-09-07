//! Tests for packet sealing.
//!
//! Two properties matter here and they are tested separately, because passing one while
//! failing the other is exactly the shape of a broken cipher integration. Confidentiality is
//! the easy one. Authenticity is the one this session cannot run without: a host that
//! accepts unauthenticated packets accepts injected keystrokes from anyone who can reach the
//! port, so every way a packet can be wrong gets its own test.

use prism_core::net::packet::{MAX_PACKET_SIZE, MAX_PLAINTEXT_SIZE, SEAL_OVERHEAD};
use prism_core::net::seal::{Opener, SealError, Sealer};

/// A key that is not all one byte, so a byte-order mistake would show.
const KEY: [u8; 32] = [
    0x9f, 0x2c, 0x41, 0x08, 0xb3, 0xd7, 0x5e, 0x1a, 0x66, 0xf0, 0x93, 0x2b, 0xc4, 0x77, 0x0d, 0xe9,
    0x35, 0x8a, 0x12, 0xbf, 0x6d, 0x40, 0xa7, 0x59, 0xe2, 0x1c, 0x84, 0xfb, 0x30, 0x6e, 0xd5, 0x97,
];

/// A second key, for the wrong-key case.
const OTHER_KEY: [u8; 32] = [0x5a; 32];

/// Builds a sealer and an opener that share a key, as one direction of a session does.
fn pair() -> (Sealer, Opener) {
    (Sealer::new(&KEY), Opener::new(&KEY))
}

/// Seals one message and returns the packet.
fn sealed(sealer: &mut Sealer, plaintext: &[u8]) -> Vec<u8> {
    let mut packet = vec![0u8; plaintext.len() + SEAL_OVERHEAD];
    let written = sealer.seal(plaintext, &mut packet).expect("seals");

    assert_eq!(written, packet.len(), "the seal wrote a different length");
    packet
}

#[test]
fn a_sealed_packet_opens_back_into_what_went_in() {
    let (mut sealer, mut opener) = pair();

    let mut packet = sealed(&mut sealer, b"the quick brown fox");

    assert_eq!(
        opener.open(&mut packet).expect("opens"),
        b"the quick brown fox"
    );
}

#[test]
fn the_payload_does_not_travel_in_the_clear() {
    // The check that the cipher is actually wired in rather than the plaintext being copied
    // through with a tag stapled on.
    let (mut sealer, _) = pair();

    let plaintext = b"password: hunter2";
    let packet = sealed(&mut sealer, plaintext);

    assert!(
        !packet.windows(plaintext.len()).any(|w| w == plaintext),
        "the plaintext appears verbatim in the sealed packet"
    );
}

#[test]
fn sealing_costs_exactly_what_the_wire_budget_says_it_does() {
    // The budget the packetiser sizes its payloads against. If this drifts, video packets
    // start exceeding the PMTU floor and fragment, which is a latency bug that looks like a
    // network problem.
    let (mut sealer, _) = pair();

    for length in [0, 1, 20, 512, MAX_PLAINTEXT_SIZE] {
        let packet = sealed(&mut sealer, &vec![0xab; length]);
        assert_eq!(packet.len(), length + SEAL_OVERHEAD);
    }
}

#[test]
fn a_full_size_plaintext_seals_to_exactly_the_packet_ceiling() {
    let (mut sealer, mut opener) = pair();

    let plaintext = vec![0x5c; MAX_PLAINTEXT_SIZE];
    let mut packet = sealed(&mut sealer, &plaintext);

    assert_eq!(packet.len(), MAX_PACKET_SIZE);
    assert_eq!(opener.open(&mut packet).expect("opens"), &plaintext[..]);
}

#[test]
fn a_plaintext_over_the_budget_is_refused_rather_than_truncated() {
    let (mut sealer, _) = pair();

    let plaintext = vec![0u8; MAX_PLAINTEXT_SIZE + 1];
    let mut packet = vec![0u8; plaintext.len() + SEAL_OVERHEAD];

    assert_eq!(
        sealer.seal(&plaintext, &mut packet),
        Err(SealError::TooLarge {
            actual: MAX_PLAINTEXT_SIZE + 1
        })
    );
    assert_eq!(sealer.sent(), 0, "a refused seal must not spend a counter");
}

#[test]
fn a_buffer_that_cannot_hold_the_seal_is_refused_rather_than_overrun() {
    let (mut sealer, _) = pair();

    let mut packet = [0u8; 8];

    assert_eq!(
        sealer.seal(b"four", &mut packet),
        Err(SealError::BufferTooSmall {
            actual: 8,
            needed: 4 + SEAL_OVERHEAD
        })
    );
    assert_eq!(sealer.sent(), 0, "a refused seal must not spend a counter");
}

#[test]
fn an_empty_plaintext_still_seals_and_opens() {
    // A keepalive carries nothing but still has to be authentic, so the empty case is a real
    // one rather than a degenerate one.
    let (mut sealer, mut opener) = pair();

    let mut packet = sealed(&mut sealer, b"");

    assert_eq!(packet.len(), SEAL_OVERHEAD);
    assert_eq!(opener.open(&mut packet).expect("opens"), b"");
}

#[test]
fn every_packet_gets_its_own_counter() {
    // Nonce reuse under GCM does not degrade the cipher, it breaks it: two packets under one
    // nonce leak their XOR and the authentication key with it. So the counters are checked
    // for distinctness directly rather than trusted.
    let (mut sealer, _) = pair();

    let counters: Vec<u64> = (0..64)
        .map(|_| {
            let packet = sealed(&mut sealer, b"payload");
            u64::from_le_bytes(packet[..8].try_into().expect("eight bytes"))
        })
        .collect();

    assert_eq!(counters, (0..64).collect::<Vec<u64>>());
    assert_eq!(sealer.sent(), 64);
}

#[test]
fn a_flipped_ciphertext_bit_is_refused() {
    let (mut sealer, mut opener) = pair();

    let mut packet = sealed(&mut sealer, b"move mouse to 100,200");
    packet[12] ^= 0x01;

    assert_eq!(opener.open(&mut packet), Err(SealError::NotAuthentic));
    assert_eq!(opener.forged(), 1);
}

#[test]
fn a_flipped_tag_bit_is_refused() {
    let (mut sealer, mut opener) = pair();

    let mut packet = sealed(&mut sealer, b"click");
    let last = packet.len() - 1;
    packet[last] ^= 0x80;

    assert_eq!(opener.open(&mut packet), Err(SealError::NotAuthentic));
    assert_eq!(opener.forged(), 1);
}

#[test]
fn rewriting_the_counter_is_refused_even_though_it_travels_in_the_clear() {
    // The counter is not covered by the tag as associated data; it does not need to be,
    // because it *is* the nonce. Changing it changes the nonce, and the tag stops verifying.
    // This test is what makes that reasoning a fact rather than an argument.
    let (mut sealer, mut opener) = pair();

    let mut packet = sealed(&mut sealer, b"keystroke");
    packet[..8].copy_from_slice(&9_999u64.to_le_bytes());

    assert_eq!(opener.open(&mut packet), Err(SealError::NotAuthentic));
    assert_eq!(opener.forged(), 1);
}

#[test]
fn a_packet_sealed_under_another_key_is_refused() {
    let mut stranger = Sealer::new(&OTHER_KEY);
    let mut opener = Opener::new(&KEY);

    let mut packet = sealed(&mut stranger, b"inject this");

    assert_eq!(opener.open(&mut packet), Err(SealError::NotAuthentic));
    assert_eq!(opener.forged(), 1);
}

#[test]
fn a_packet_too_short_to_be_one_is_refused_before_anything_is_read() {
    let (_, mut opener) = pair();

    for length in [0, 1, SEAL_OVERHEAD - 1] {
        let mut packet = vec![0u8; length];
        assert_eq!(
            opener.open(&mut packet),
            Err(SealError::TooShort { actual: length })
        );
    }

    assert_eq!(
        opener.forged(),
        0,
        "a malformed length is not a forgery attempt worth counting"
    );
}

#[test]
fn replaying_a_packet_is_refused() {
    // Without this an attacker records one mouse click and posts it back whenever they like.
    // The tag alone does not stop that, because the packet really was written by the peer.
    let (mut sealer, mut opener) = pair();

    let original = sealed(&mut sealer, b"click");

    let mut first = original.clone();
    assert_eq!(opener.open(&mut first).expect("opens"), b"click");

    let mut again = original;
    assert_eq!(
        opener.open(&mut again),
        Err(SealError::Replay { counter: 0 })
    );
    assert_eq!(opener.replayed(), 1);
    assert_eq!(opener.forged(), 0, "a replay is not a forgery");
}

#[test]
fn packets_that_arrive_out_of_order_are_still_accepted() {
    // UDP reorders constantly. A receiver that insisted on monotonic counters would throw
    // away good packets on every path with jitter, which is a correctness bug dressed as a
    // security measure.
    let (mut sealer, mut opener) = pair();

    let packets: Vec<Vec<u8>> = (0..8u8)
        .map(|index| sealed(&mut sealer, &[index]))
        .collect();

    for index in [7usize, 3, 0, 6, 1, 5, 2, 4] {
        let mut packet = packets[index].clone();
        assert_eq!(
            opener.open(&mut packet).expect("opens"),
            &[index as u8][..],
            "packet {index} was refused out of order"
        );
    }

    assert_eq!(opener.replayed(), 0);
    assert_eq!(opener.forged(), 0);
}

#[test]
fn a_packet_older_than_the_window_is_refused() {
    // The window has to end somewhere, and past its edge the opener genuinely cannot tell a
    // replay from a very late packet. It refuses, because on this path a packet that late is
    // useless anyway.
    let (mut sealer, mut opener) = pair();

    let stale = sealed(&mut sealer, b"stale");

    for _ in 0..65 {
        let mut packet = sealed(&mut sealer, b"fresh");
        opener.open(&mut packet).expect("opens");
    }

    let mut stale = stale;
    assert_eq!(
        opener.open(&mut stale),
        Err(SealError::Replay { counter: 0 })
    );
    assert_eq!(opener.replayed(), 1);
}

#[test]
fn a_packet_at_the_far_edge_of_the_window_is_still_accepted() {
    // One counter younger than the test above. The boundary is where an off-by-one lives, and
    // an off-by-one here silently drops packets on a jittery path.
    let (mut sealer, mut opener) = pair();

    let old = sealed(&mut sealer, b"old");

    for _ in 0..64 {
        let mut packet = sealed(&mut sealer, b"new");
        opener.open(&mut packet).expect("opens");
    }

    let mut old = old;
    assert_eq!(opener.open(&mut old).expect("opens"), b"old");
}

#[test]
fn a_forged_packet_claiming_a_far_future_counter_does_not_move_the_window() {
    // The property the ordering of the two checks exists for. If the counter were judged
    // first, anyone able to write a packet at this port could name a counter of two to the
    // sixty-three and every genuine packet after it would fall outside the window — a denial
    // of service that costs the attacker one packet and needs no key at all.
    let (mut sealer, mut opener) = pair();

    let mut genuine = sealed(&mut sealer, b"first");
    opener.open(&mut genuine).expect("opens");

    let mut forgery = vec![0u8; 32];
    forgery[..8].copy_from_slice(&(1u64 << 63).to_le_bytes());
    assert_eq!(opener.open(&mut forgery), Err(SealError::NotAuthentic));

    let mut next = sealed(&mut sealer, b"second");
    assert_eq!(
        opener.open(&mut next).expect("opens"),
        b"second",
        "a forgery moved the replay window"
    );
}

#[test]
fn the_two_directions_of_a_session_do_not_open_each_other() {
    // Each direction gets its own key precisely so that the counters of one cannot collide
    // with the counters of the other. Sharing one key would be nonce reuse by another name,
    // and this is the test that would fail if someone later decided one key was simpler.
    let mut host_to_client = Sealer::new(&KEY);
    let mut client_side = Opener::new(&OTHER_KEY);

    let mut packet = sealed(&mut host_to_client, b"video slice");

    assert_eq!(client_side.open(&mut packet), Err(SealError::NotAuthentic));
}

#[test]
fn neither_half_prints_its_key() {
    // Sealer and Opener end up inside session structs that get logged when something goes
    // wrong. A derived Debug would put the key in a log file.
    let (sealer, opener) = pair();

    let printed = format!("{sealer:?} {opener:?}");

    assert!(!printed.contains("9f"), "a key byte reached the output");
    assert!(printed.contains("sent"));
    assert!(printed.contains("forged"));
}

#[test]
#[ignore = "a measurement, not an assertion; run with --ignored --nocapture"]
fn how_much_a_full_size_packet_costs_to_seal_and_open() {
    // The number the latency budget needs. A full packet is sealed and opened a hundred
    // thousand times, which is about thirteen seconds of a 1440p120 stream.
    const ROUNDS: u32 = 100_000;

    let (mut sealer, mut opener) = pair();
    let plaintext = vec![0x7eu8; MAX_PLAINTEXT_SIZE];
    let mut packet = vec![0u8; MAX_PACKET_SIZE];

    let start = std::time::Instant::now();
    for _ in 0..ROUNDS {
        let len = sealer.seal(&plaintext, &mut packet).expect("seals");
        opener.open(&mut packet[..len]).expect("opens");
    }
    let elapsed = start.elapsed();

    let per_packet = elapsed.as_secs_f64() / f64::from(ROUNDS) * 1e6;
    println!(
        "seal + open of {MAX_PLAINTEXT_SIZE} bytes: {per_packet:.3} us per packet, \
         {:.1} Gbps",
        f64::from(ROUNDS) * MAX_PLAINTEXT_SIZE as f64 * 8.0 / elapsed.as_secs_f64() / 1e9
    );
}
