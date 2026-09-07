//! Tests for PIN pairing.
//!
//! Six digits is a million possibilities and a laptop tries a million of anything in under a
//! second, so the only thing standing between a pairing code and an attacker is that a guess
//! cannot be checked offline and the host allows exactly one online attempt. Both halves of
//! that are tested here; either one alone is worth nothing.

use prism_core::net::pairing::{
    ACCEPT_LEN, HELLO_LEN, OFFER_LEN, PairingClient, PairingError, PairingHost, Pin,
};

/// A key that stands in for a machine's long-term identity.
const HOST_KEY: [u8; 32] = [0xa1; 32];

/// The other machine's.
const CLIENT_KEY: [u8; 32] = [0xb2; 32];

/// Runs a full exchange under `client_pin`, with the host holding `host_pin`.
///
/// Returns what each side ended up with: the host's view of the client's key, and the
/// client's view of the host's.
fn exchange(host_pin: Pin, client_pin: &Pin) -> Result<([u8; 32], [u8; 32]), PairingError> {
    let mut host = PairingHost::new(host_pin, HOST_KEY);
    let mut client = PairingClient::new(client_pin, CLIENT_KEY);

    let mut offer = [0u8; OFFER_LEN];
    let offered = host.answer(client.hello(), &mut offer)?;

    let mut accept = [0u8; ACCEPT_LEN];
    let (host_key_seen, accepted) = client.finish(&offer[..offered], &mut accept)?;

    let client_key_seen = host.accept(&accept[..accepted])?;

    Ok((client_key_seen, host_key_seen))
}

#[test]
fn the_right_code_leaves_each_side_holding_the_others_key() {
    let pin = Pin::generate().expect("generates");

    let (client_key_seen, host_key_seen) = exchange(pin.clone(), &pin).expect("pairs");

    assert_eq!(client_key_seen, CLIENT_KEY);
    assert_eq!(host_key_seen, HOST_KEY);
}

#[test]
fn a_wrong_code_fails_and_says_nothing_about_which_digit() {
    // The failure is a seal that does not open, so it carries no detail at all. That is the
    // point: any signal about how close a guess was would turn a million guesses into far
    // fewer.
    let host_pin = Pin::parse("123456").expect("parses");
    let client_pin = Pin::parse("123457").expect("parses");

    assert_eq!(exchange(host_pin, &client_pin), Err(PairingError::Failed));
}

#[test]
fn every_wrong_code_fails_the_same_way() {
    // Guesses that differ in one digit and guesses that differ in all six must be
    // indistinguishable, or the host is an oracle.
    let host_pin = Pin::parse("000000").expect("parses");

    for guess in ["000001", "100000", "999999", "012345"] {
        let client_pin = Pin::parse(guess).expect("parses");
        assert_eq!(
            exchange(host_pin.clone(), &client_pin),
            Err(PairingError::Failed),
            "{guess} failed differently"
        );
    }
}

#[test]
fn a_code_is_spent_by_one_attempt_however_it_went() {
    // The whole security argument. A code that allowed a second guess would allow a
    // millionth, and a million guesses at six digits is certainty.
    let pin = Pin::parse("246810").expect("parses");
    let mut host = PairingHost::new(pin, HOST_KEY);

    let wrong = Pin::parse("111111").expect("parses");
    let first = PairingClient::new(&wrong, CLIENT_KEY);
    let mut offer = [0u8; OFFER_LEN];
    host.answer(first.hello(), &mut offer)
        .expect("the first attempt is answered");

    let second = PairingClient::new(&wrong, CLIENT_KEY);
    assert_eq!(
        host.answer(second.hello(), &mut offer),
        Err(PairingError::Spent),
        "a second guess was allowed"
    );
}

#[test]
fn a_successful_pairing_also_spends_the_code() {
    // Not only failures. A code that stayed live after a successful pairing would let anyone
    // who saw the screen pair their own machine afterwards.
    let pin = Pin::generate().expect("generates");
    let mut host = PairingHost::new(pin.clone(), HOST_KEY);

    let mut client = PairingClient::new(&pin, CLIENT_KEY);
    let mut offer = [0u8; OFFER_LEN];
    let offered = host.answer(client.hello(), &mut offer).expect("answers");
    let mut accept = [0u8; ACCEPT_LEN];
    client
        .finish(&offer[..offered], &mut accept)
        .expect("finishes");

    let intruder = PairingClient::new(&pin, [0xcc; 32]);
    assert_eq!(
        host.answer(intruder.hello(), &mut offer),
        Err(PairingError::Spent)
    );
}

#[test]
fn an_impostor_answering_in_the_hosts_place_is_refused() {
    // Someone who reaches the client before the real host does, without the code. They can
    // produce a well-formed SPAKE2 element; what they cannot produce is a sealed key the
    // client will open.
    let real = Pin::parse("135790").expect("parses");
    let guessed = Pin::parse("111111").expect("parses");

    let mut client = PairingClient::new(&real, CLIENT_KEY);
    let mut impostor = PairingHost::new(guessed, [0xee; 32]);

    let mut offer = [0u8; OFFER_LEN];
    let offered = impostor
        .answer(client.hello(), &mut offer)
        .expect("an impostor can still answer");

    let mut accept = [0u8; ACCEPT_LEN];
    assert_eq!(
        client.finish(&offer[..offered], &mut accept),
        Err(PairingError::Failed),
        "the client accepted a key from a host that did not know the code"
    );
}

#[test]
fn the_two_directions_do_not_share_a_key() {
    // Each sealed half uses a fixed nonce, which is only safe because each key seals exactly
    // one message. If both directions shared a key that would be nonce reuse in the plainest
    // possible form, and the host would open its own offer as if it were an accept.
    let pin = Pin::generate().expect("generates");
    let mut host = PairingHost::new(pin.clone(), HOST_KEY);
    let client = PairingClient::new(&pin, CLIENT_KEY);

    let mut offer = [0u8; OFFER_LEN];
    let offered = host.answer(client.hello(), &mut offer).expect("answers");

    // The sealed tail of the offer, replayed as if it were the client's accept.
    let replayed = &offer[offered - ACCEPT_LEN..offered];

    assert_eq!(host.accept(replayed), Err(PairingError::Failed));
}

#[test]
fn a_message_of_the_wrong_length_does_not_spend_the_code() {
    // The length check has to come before the code is taken. If it did not, anyone who can
    // reach the port could burn a pairing window with a single junk datagram, and the person
    // in front of the screen would watch pairing fail with no explanation.
    let pin = Pin::generate().expect("generates");
    let mut host = PairingHost::new(pin.clone(), HOST_KEY);
    let mut offer = [0u8; OFFER_LEN];

    for length in [0usize, 1, HELLO_LEN - 1, HELLO_LEN + 1, 1200] {
        assert_eq!(
            host.answer(&vec![0u8; length], &mut offer),
            Err(PairingError::BadLength {
                actual: length,
                expected: HELLO_LEN
            }),
            "a hello of {length} bytes was not refused"
        );
    }

    // And the genuine client still pairs afterwards, which is the part that matters.
    let mut client = PairingClient::new(&pin, CLIENT_KEY);
    let offered = host
        .answer(client.hello(), &mut offer)
        .expect("junk spent the pairing window");
    let mut accept = [0u8; ACCEPT_LEN];
    let (host_key_seen, _) = client
        .finish(&offer[..offered], &mut accept)
        .expect("finishes");

    assert_eq!(host_key_seen, HOST_KEY);
}

#[test]
fn junk_of_the_right_length_is_refused_rather_than_answered() {
    // This one does spend the code, and it should: a well-formed attempt is an attempt. What
    // it must not do is produce an answer, because an answer to an element that is not a
    // point on the curve is a place where implementations leak.
    let pin = Pin::generate().expect("generates");
    let mut host = PairingHost::new(pin, HOST_KEY);
    let mut offer = [0u8; OFFER_LEN];

    assert_eq!(
        host.answer(&[0x00u8; HELLO_LEN], &mut offer),
        Err(PairingError::Failed)
    );
    assert_eq!(
        host.answer(&[0xffu8; HELLO_LEN], &mut offer),
        Err(PairingError::Spent),
        "the attempt did not spend the code"
    );
}

#[test]
fn a_generated_code_is_six_digits_and_not_always_the_same() {
    let codes: Vec<String> = (0..64)
        .map(|_| Pin::generate().expect("generates").to_display())
        .collect();

    for code in &codes {
        assert_eq!(code.len(), 6);
        assert!(
            code.chars().all(|c| c.is_ascii_digit()),
            "{code} is not digits"
        );
    }

    let distinct: std::collections::HashSet<&String> = codes.iter().collect();
    assert!(
        distinct.len() > 50,
        "only {} distinct codes in 64 draws, so the generator is not random",
        distinct.len()
    );
}

#[test]
fn every_digit_position_takes_every_value() {
    // A modulo over a byte would make the low digits likelier and shrink the space an
    // attacker has to search. Rejection sampling is what avoids that, and this is the check
    // that it is actually happening at every position rather than only the first.
    let mut seen = [[false; 10]; 6];

    for _ in 0..4_000 {
        let code = Pin::generate().expect("generates").to_display();
        for (position, digit) in code.chars().enumerate() {
            seen[position][digit.to_digit(10).expect("a digit") as usize] = true;
        }
    }

    for (position, digits) in seen.iter().enumerate() {
        assert!(
            digits.iter().all(|&hit| hit),
            "position {position} never took some value: {digits:?}"
        );
    }
}

#[test]
fn a_code_that_is_not_six_digits_is_refused() {
    for text in ["", "1", "12345", "1234567", "12345a", "12 456", "-12345"] {
        assert_eq!(Pin::parse(text), Err(PairingError::BadPin), "{text:?}");
    }
}

#[test]
fn a_code_survives_being_displayed_and_typed_back() {
    for _ in 0..32 {
        let generated = Pin::generate().expect("generates");
        let typed = Pin::parse(&generated.to_display()).expect("parses");

        assert_eq!(generated, typed);
    }
}

#[test]
fn surrounding_whitespace_from_a_paste_is_forgiven() {
    assert_eq!(
        Pin::parse(" 123456\n").expect("parses"),
        Pin::parse("123456").expect("parses")
    );
}

#[test]
fn a_code_does_not_print_itself() {
    // Short-lived, but still the secret the whole exchange rests on — and a Debug ends up in
    // exactly the places it should not be.
    let pin = Pin::parse("314159").expect("parses");

    let printed = format!("{pin:?}");

    assert!(
        !printed.contains("314159"),
        "the code reached Debug: {printed}"
    );
}
