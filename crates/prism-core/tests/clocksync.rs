//! Tests for clock offset estimation.
//!
//! The estimator exists because a cross-machine latency figure is meaningless without it,
//! so these tests check the arithmetic against exchanges whose answer is known by
//! construction, and check that it refuses the exchanges that cannot have happened.

use prism_core::net::clocksync::ClockSync;
use prism_core::net::packet::ClockPong;

/// Builds the pong and arrival time for an exchange with a known offset and latency.
///
/// The host's clock runs `offset_us` ahead of the client's, each leg of the round trip
/// takes `leg_us`, and the host spends `handling_us` between receiving and answering.
fn exchange(t1_us: u64, offset_us: i64, leg_us: u64, handling_us: u64) -> (ClockPong, u64) {
    let host_recv = (t1_us as i64 + leg_us as i64 + offset_us) as u64;
    let host_send = host_recv + handling_us;
    let t4_us = t1_us + leg_us + handling_us + leg_us;

    (
        ClockPong {
            t1_us,
            t2_us: host_recv,
            t3_us: host_send,
        },
        t4_us,
    )
}

#[test]
fn an_estimator_with_no_samples_knows_nothing() {
    let sync = ClockSync::new();

    assert!(sync.offset_us().is_none());
    assert!(sync.round_trip_us().is_none());
    assert!(sync.to_local_us(1_000).is_none());
    assert_eq!(sync.accepted(), 0);
    assert_eq!(sync.rejected(), 0);
}

#[test]
fn a_symmetric_exchange_recovers_the_offset_exactly() {
    for offset in [0i64, 1_000, -1_000, 5_000_000, -5_000_000] {
        let mut sync = ClockSync::new();
        let (pong, t4) = exchange(1_000_000, offset, 250, 100);

        let sample = sync.observe(&pong, t4).expect("the exchange is possible");

        assert_eq!(sample.offset_us, offset, "offset {offset}");
        assert_eq!(sample.round_trip_us, 500, "two legs of 250 us");
        assert_eq!(sync.offset_us(), Some(offset));
    }
}

#[test]
fn the_host_handling_time_is_excluded_from_the_round_trip() {
    let mut sync = ClockSync::new();
    let (pong, t4) = exchange(1_000_000, 0, 300, 50_000);

    let sample = sync.observe(&pong, t4).unwrap();

    assert_eq!(
        sample.round_trip_us, 600,
        "the 50 ms the host held the ping does not count"
    );
    assert_eq!(sample.offset_us, 0);
}

#[test]
fn the_fastest_exchange_wins() {
    let mut sync = ClockSync::new();

    sync.observe(
        &exchange(1_000, 7_000, 5_000, 0).0,
        exchange(1_000, 7_000, 5_000, 0).1,
    );
    let slow = sync.round_trip_us().unwrap();

    let (fast_pong, fast_t4) = exchange(50_000, 7_000, 100, 0);
    sync.observe(&fast_pong, fast_t4);

    assert!(sync.round_trip_us().unwrap() < slow);
    assert_eq!(sync.offset_us(), Some(7_000));
    assert_eq!(sync.accepted(), 2);
}

#[test]
fn a_slower_exchange_does_not_replace_a_faster_one() {
    let mut sync = ClockSync::new();

    let (fast_pong, fast_t4) = exchange(1_000, 3_000, 100, 0);
    sync.observe(&fast_pong, fast_t4);

    let (slow_pong, slow_t4) = exchange(50_000, 3_000, 9_000, 0);
    sync.observe(&slow_pong, slow_t4);

    assert_eq!(sync.round_trip_us(), Some(200), "the fast sample is kept");
}

#[test]
fn impossible_exchanges_are_refused() {
    let mut sync = ClockSync::new();

    // The reply arrived before the ping was sent.
    assert!(
        sync.observe(
            &ClockPong {
                t1_us: 1_000,
                t2_us: 1_000,
                t3_us: 1_000
            },
            500
        )
        .is_none()
    );

    // The host answered before it received.
    assert!(
        sync.observe(
            &ClockPong {
                t1_us: 0,
                t2_us: 900,
                t3_us: 100
            },
            1_000
        )
        .is_none()
    );

    // The round trip works out negative, which no real exchange can produce.
    assert!(
        sync.observe(
            &ClockPong {
                t1_us: 0,
                t2_us: 0,
                t3_us: 5_000
            },
            1_000
        )
        .is_none()
    );

    assert_eq!(sync.rejected(), 3);
    assert_eq!(sync.accepted(), 0);
    assert!(
        sync.offset_us().is_none(),
        "a rejected sample must not become the estimate"
    );
}

#[test]
fn host_timestamps_convert_into_local_time() {
    let mut sync = ClockSync::new();
    let (pong, t4) = exchange(1_000_000, 2_500_000, 200, 0);
    sync.observe(&pong, t4);

    assert_eq!(sync.offset_us(), Some(2_500_000));
    assert_eq!(sync.to_local_us(10_000_000), Some(7_500_000));
}

#[test]
fn a_host_timestamp_that_maps_before_the_epoch_is_refused() {
    let mut sync = ClockSync::new();
    let (pong, t4) = exchange(1_000_000, 5_000_000, 100, 0);
    sync.observe(&pong, t4);

    assert!(
        sync.to_local_us(1_000).is_none(),
        "the offset is larger than the timestamp"
    );
}

#[test]
fn a_client_clock_behind_the_host_yields_a_positive_offset() {
    let mut sync = ClockSync::new();
    let (pong, t4) = exchange(1_000, 1_500_000, 400, 0);

    let sample = sync.observe(&pong, t4).unwrap();

    assert!(sample.offset_us > 0, "a host ahead of us reads positive");
    assert_eq!(sample.offset_us, 1_500_000);
}
