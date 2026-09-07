//! Tests for codec and picture negotiation.
//!
//! The failures worth guarding here are the quiet ones. A session that agrees on a codec the
//! client cannot decode shows a black window with no error anywhere; one that agrees on a
//! picture larger than the client's screen spends bitrate on pixels nobody sees. Both look
//! like something else entirely from the outside.

use prism_core::net::negotiate::{
    ACCEPT_LEN, AV1, Accept, Codec, Codecs, H264, HEVC, HostAbility, NegotiateError, OFFER_LEN,
    Offer, REVISION, decide,
};

/// A host that can do everything, on a 1440p screen.
fn capable_host() -> HostAbility {
    HostAbility {
        codecs: Codecs::none().with(H264).with(HEVC).with(AV1),
        width: 2560,
        height: 1440,
        fps: 120,
        bitrate_bps: 40_000_000,
        audio: true,
    }
}

/// A client that can do everything, on a 1440p screen.
fn capable_client() -> Offer {
    Offer {
        codecs: Codecs::none().with(H264).with(HEVC).with(AV1),
        max_width: 2560,
        max_height: 1440,
        max_fps: 120,
        audio: true,
    }
}

#[test]
fn the_best_codec_both_can_do_is_the_one_chosen() {
    let accept = decide(capable_host(), capable_client()).expect("agrees");

    assert_eq!(accept.codec, Codec::Av1);
}

#[test]
fn a_codec_only_one_side_has_is_not_chosen() {
    // The quiet failure this exists to prevent: a host that encoded AV1 to a client that cannot
    // decode it produces a black window and no error anywhere.
    let mut client = capable_client();
    client.codecs = Codecs::none().with(H264).with(HEVC);

    assert_eq!(
        decide(capable_host(), client).expect("agrees").codec,
        Codec::Hevc
    );

    let mut host = capable_host();
    host.codecs = Codecs::none().with(H264);

    assert_eq!(
        decide(host, capable_client()).expect("agrees").codec,
        Codec::H264
    );
}

#[test]
fn h264_is_the_floor_that_always_works() {
    // Every machine this runs on does H.264, which is why it is the fallback and why a session
    // should never fail to find a codec.
    let mut host = capable_host();
    host.codecs = Codecs::none().with(H264).with(AV1);

    let mut client = capable_client();
    client.codecs = Codecs::none().with(H264).with(HEVC);

    assert_eq!(
        decide(host, client).expect("agrees").codec,
        Codec::H264,
        "two machines with no advanced codec in common did not fall back"
    );
}

#[test]
fn two_machines_with_nothing_in_common_are_refused_rather_than_guessed_at() {
    let mut client = capable_client();
    client.codecs = Codecs::none().with(AV1);

    let mut host = capable_host();
    host.codecs = Codecs::none().with(H264);

    assert_eq!(
        decide(host, client),
        Err(NegotiateError::NoCommonCodec),
        "a codec was invented for two machines that share none"
    );
}

#[test]
fn the_picture_is_the_smaller_of_what_each_side_wants() {
    // A host sending more pixels than the client can show spends bitrate on pixels thrown away
    // before anybody sees them, and more frames than it can present on frames nobody sees.
    let mut client = capable_client();
    client.max_width = 1920;
    client.max_height = 1080;
    client.max_fps = 60;

    let accept = decide(capable_host(), client).expect("agrees");

    assert_eq!((accept.width, accept.height, accept.fps), (1920, 1080, 60));
}

#[test]
fn a_client_that_wants_more_than_the_host_has_gets_what_the_host_has() {
    let mut host = capable_host();
    host.width = 1920;
    host.height = 1080;
    host.fps = 60;

    let accept = decide(host, capable_client()).expect("agrees");

    assert_eq!((accept.width, accept.height, accept.fps), (1920, 1080, 60));
}

#[test]
fn an_odd_dimension_is_rounded_down_rather_than_refused() {
    // Every codec here subsamples chroma by two in both directions, so an odd dimension has no
    // representation. Refusing would turn a display with an unusual size into a machine that
    // cannot be streamed to at all.
    let mut client = capable_client();
    client.max_width = 1921;
    client.max_height = 1081;

    let accept = decide(capable_host(), client).expect("agrees");

    assert_eq!((accept.width, accept.height), (1920, 1080));
}

#[test]
fn a_rate_never_lands_at_zero() {
    let mut client = capable_client();
    client.max_fps = 0;

    assert_eq!(decide(capable_host(), client).expect("agrees").fps, 1);
}

#[test]
fn sound_is_sent_only_when_both_sides_want_it() {
    for (host_audio, client_audio, expected) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
        (false, false, false),
    ] {
        let mut host = capable_host();
        host.audio = host_audio;
        let mut client = capable_client();
        client.audio = client_audio;

        assert_eq!(
            decide(host, client).expect("agrees").audio,
            expected,
            "host {host_audio}, client {client_audio}"
        );
    }
}

#[test]
fn an_offer_survives_a_round_trip() {
    let offer = Offer {
        codecs: Codecs::none().with(H264).with(AV1),
        max_width: 3840,
        max_height: 2160,
        max_fps: 240,
        audio: true,
    };

    let mut buf = [0u8; OFFER_LEN];
    assert_eq!(offer.encode_into(&mut buf).expect("encodes"), OFFER_LEN);
    assert_eq!(Offer::decode(&buf).expect("decodes"), offer);
}

#[test]
fn an_acceptance_survives_a_round_trip() {
    let accept = Accept {
        codec: Codec::Hevc,
        width: 2560,
        height: 1440,
        fps: 120,
        bitrate_bps: 40_000_000,
        audio: false,
    };

    let mut buf = [0u8; ACCEPT_LEN];
    assert_eq!(accept.encode_into(&mut buf).expect("encodes"), ACCEPT_LEN);
    assert_eq!(Accept::decode(&buf).expect("decodes"), accept);
}

#[test]
fn a_message_of_the_wrong_length_is_refused() {
    let mut buf = [0u8; 64];
    let offer = Offer {
        codecs: H264,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
        audio: true,
    };
    offer.encode_into(&mut buf).expect("encodes");

    for length in [0usize, 1, OFFER_LEN - 1, OFFER_LEN + 1] {
        assert!(
            Offer::decode(&buf[..length]).is_err(),
            "an offer of {length} bytes was accepted"
        );
    }
}

#[test]
fn a_peer_at_another_revision_is_refused_rather_than_misread() {
    // Two builds that disagree about what a byte means and carry on anyway produce a session
    // that fails somewhere far from the cause.
    let mut buf = [0u8; OFFER_LEN];
    Offer {
        codecs: H264,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
        audio: true,
    }
    .encode_into(&mut buf)
    .expect("encodes");

    buf[0..2].copy_from_slice(&(REVISION + 1).to_le_bytes());

    assert_eq!(
        Offer::decode(&buf),
        Err(NegotiateError::WrongRevision {
            theirs: REVISION + 1
        })
    );
}

#[test]
fn a_codec_this_build_does_not_know_is_refused() {
    // A newer host choosing something this client cannot decode. Refusing is a session that
    // fails to open; accepting is a black window with no error anywhere.
    let mut buf = [0u8; ACCEPT_LEN];
    Accept {
        codec: Codec::H264,
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: 20_000_000,
        audio: false,
    }
    .encode_into(&mut buf)
    .expect("encodes");

    buf[2] = 9;

    assert_eq!(
        Accept::decode(&buf),
        Err(NegotiateError::UnknownCodec { byte: 9 })
    );
}

#[test]
fn codecs_a_future_version_defines_are_ignored_rather_than_misread() {
    // The next version will set bits this one has never heard of. Reading them as codecs it
    // knows would be worse than not seeing them at all.
    let future = Codecs::from_bits(0b1111_1111);

    assert!(future.has(H264));
    assert!(future.has(HEVC));
    assert!(future.has(AV1));
    assert_eq!(future.bits(), 0b0000_0111);
}

#[test]
fn an_empty_set_has_nothing_and_names_nothing() {
    let empty = Codecs::none();

    assert!(empty.is_empty());
    assert!(!empty.has(H264));
    assert_eq!(empty.best(), None);
}

#[test]
fn every_codec_names_itself() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        assert_eq!(Codec::from_byte(codec as u8), Some(codec));
        assert_eq!(codec.as_set().best(), Some(codec));
    }
}

#[test]
fn both_halves_fit_the_handshake_payload_many_times_over() {
    // They ride inside the Noise handshake, which carries a kilobyte. Room to spare is what
    // lets a later version add a field without a second round trip appearing in front of every
    // connection.
    const {
        assert!(OFFER_LEN < prism_core::net::handshake::MAX_HANDSHAKE_PAYLOAD / 8);
        assert!(ACCEPT_LEN < prism_core::net::handshake::MAX_HANDSHAKE_PAYLOAD / 8);
    };
}
