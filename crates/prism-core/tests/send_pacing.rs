//! Tests for send pacing.
//!
//! The pacer exists to make two promises at once: the stream averages the rate it was
//! given, and no short window of it ever runs far ahead of that rate. Those are the two
//! things worth checking, so most of what follows drives a virtual clock through a
//! simulated send loop and asserts a property of the whole run rather than the value of
//! any one wait — a test that recomputed the pacer's own arithmetic would agree with a
//! wrong pacer just as readily as a right one.
//!
//! For the same reason no test takes its expected rate or allowance from the pacer's own
//! accessors. A pacer that quietly ignored a rate change would still report the rate it
//! was actually using and agree with itself; the numbers here come from the bitrate the
//! test asked for and the policy the module documents.

use core::time::Duration;

use prism_core::net::sendpace::{
    DEFAULT_BURST_BYTES, MIN_BITRATE_BPS, MIN_BURST_BYTES, PacerConfig, SPREAD_PERCENT, SendPacer,
};

/// Builds a pacer that reports every wait exactly, as a caller with a precise timer would.
fn exact(bitrate_bps: u32) -> SendPacer {
    SendPacer::new(PacerConfig {
        bitrate_bps,
        min_sleep: Duration::ZERO,
        ..PacerConfig::default()
    })
}

/// Builds a pacer that batches its waits, as a caller using a thread sleep must.
fn batched(bitrate_bps: u32, min_sleep: Duration) -> SendPacer {
    SendPacer::new(PacerConfig {
        bitrate_bps,
        min_sleep,
        ..PacerConfig::default()
    })
}

/// Runs a sender that always has the next packet ready and honours every wait it is told
/// to take, returning the virtual time the last packet left at.
fn drain(pacer: &mut SendPacer, packets: usize, bytes: usize, start_ns: u64) -> u64 {
    let mut now = start_ns;
    for _ in 0..packets {
        now += pacer.wait_before(bytes, now).as_nanos() as u64;
    }
    now
}

/// Returns the rate a greedy sender achieved over one run and the clock it finished at.
fn rate_over(pacer: &mut SendPacer, packets: usize, bytes: usize, start_ns: u64) -> (f64, u64) {
    let end = drain(pacer, packets, bytes, start_ns);
    assert!(
        end > start_ns,
        "a run of {packets} packets took no time at all — nothing was paced"
    );

    let bits = (packets * bytes * 8) as f64;
    (bits / ((end - start_ns) as f64 / 1e9), end)
}

/// The rate packets must leave at for a target bitrate, from the policy the module states.
///
/// Deliberately not [`SendPacer::pacing_rate_bps`]: a pacer that dropped a rate change on
/// the floor would report the stale rate it was really using and match itself.
fn paced_rate_of(bitrate_bps: u32) -> f64 {
    f64::from(bitrate_bps) * 100.0 / f64::from(SPREAD_PERCENT)
}

/// Returns how many packets leave back to back before the pacer holds one back.
///
/// Only meaningful on a pacer built by [`exact`]; one that batches reports short waits as
/// zero, which would read here as packets that were never held.
fn free_run(pacer: &mut SendPacer, bytes: usize, now_ns: u64) -> usize {
    let mut free = 0;
    while pacer.wait_before(bytes, now_ns).is_zero() {
        free += 1;
        assert!(free < 100_000, "the pacer never held anything back");
    }
    free
}

/// Asserts that `got` is within `tolerance` of `want`, as a fraction.
fn assert_close(got: f64, want: f64, tolerance: f64, what: &str) {
    let error = (got - want).abs() / want;
    assert!(
        error < tolerance,
        "{what}: got {got:.0} bps, want {want:.0} bps, off by {:.3}%",
        error * 100.0
    );
}

#[test]
fn a_steady_stream_settles_on_the_paced_rate() {
    for bitrate in [2_000_000u32, 5_000_000, 40_000_000, 100_000_000] {
        let mut pacer = exact(bitrate);
        let (got, _) = rate_over(&mut pacer, 5_000, 1200, 1_000_000_000);

        assert_close(
            got,
            paced_rate_of(bitrate),
            0.01,
            &format!("a stream at {bitrate} bps"),
        );
    }
}

#[test]
fn a_frame_leaves_spread_across_its_interval_rather_than_at_line_rate() {
    // The host loop as it will really run, at the 1440p120 and 40 Mbps the plan aims at:
    // a frame's packets are offered at the frame boundary and the sender waits where it
    // is told. A sender ignoring the pacer would empty each frame in microseconds; one
    // paced too hard would still be sending when the next frame arrived. Measured over
    // many frames so the burst allowance is amortised rather than counted once.
    let bitrate = 40_000_000u32;
    let fps = 120u64;
    let interval_ns = 1_000_000_000 / fps;
    let frame_packets = (u64::from(bitrate) / 8 / fps / 1200) as usize;
    let frames = 200u64;

    let mut pacer = exact(bitrate);
    let mut clock = 0u64;
    let mut occupied_ns = 0u64;

    for frame in 0..frames {
        let start = clock.max(frame * interval_ns);
        clock = drain(&mut pacer, frame_packets, 1200, start);
        occupied_ns += clock - start;
    }

    let occupied = occupied_ns as f64 / (frames * interval_ns) as f64 * 100.0;

    assert!(
        occupied > 50.0,
        "frames left in {occupied:.1}% of their interval, which is line rate in all but name"
    );
    assert!(
        occupied < f64::from(SPREAD_PERCENT) + 10.0,
        "frames took {occupied:.1}% of their interval and are running into the next one"
    );
    assert!(
        clock < frames * interval_ns,
        "the sender fell behind the frame cadence over {frames} frames"
    );
}

#[test]
fn batching_the_waits_does_not_change_the_rate() {
    // The reason short waits may be reported as zero: the debt is carried, not forgiven,
    // so coarsening the spacing to something a thread sleep can honour leaves the
    // long-run rate exactly where it was.
    for min_sleep in [
        Duration::ZERO,
        Duration::from_micros(200),
        Duration::from_millis(1),
        Duration::from_millis(4),
    ] {
        let mut pacer = batched(40_000_000, min_sleep);
        let (got, _) = rate_over(&mut pacer, 20_000, 1200, 1_000_000_000);

        assert_close(
            got,
            paced_rate_of(40_000_000),
            0.01,
            &format!("with min_sleep {min_sleep:?}"),
        );
    }
}

#[test]
fn every_reported_wait_is_long_enough_to_be_worth_sleeping_for() {
    let min_sleep = Duration::from_millis(1);
    let mut pacer = batched(40_000_000, min_sleep);
    let mut now = 5_000_000u64;

    for i in 0..3_000usize {
        let bytes = 120 + (i * 271) % 1_081;
        let wait = pacer.wait_before(bytes, now);

        assert!(
            wait.is_zero() || wait >= min_sleep,
            "a {wait:?} wait is shorter than the caller can sleep for"
        );
        now += wait.as_nanos() as u64;
    }

    assert!(pacer.waits() > 0, "nothing was ever held back");
}

#[test]
fn a_burst_offered_at_one_instant_is_spread_out_rather_than_passed_through() {
    // A frame's worth of packets handed over together, as an encoder hands them over.
    let packets = 40usize;
    let mut pacer = exact(40_000_000);
    let end = drain(&mut pacer, packets, 1200, 0);

    // What the bytes are worth at the paced rate. An unpaced sender finishes at zero.
    let paced_ns = (packets * 1200 * 8) as f64 / paced_rate_of(40_000_000) * 1e9;
    let took_ns = end as f64;

    assert!(
        took_ns > paced_ns * 0.8,
        "the frame left in {end} ns, far short of the {paced_ns:.0} ns its bytes are worth"
    );
    assert!(
        took_ns < paced_ns,
        "the frame took longer than its own bytes are worth at the paced rate"
    );

    // Everything past the burst allowance had to wait for its turn.
    let free = DEFAULT_BURST_BYTES as usize / 1200 + 1;
    assert_eq!(
        pacer.waits() as usize,
        packets - free,
        "the wrong number of packets were held back"
    );
}

#[test]
fn idle_time_does_not_build_credit_beyond_the_burst_allowance() {
    // An unbounded pacer idle for an hour would owe itself 22 GB of credit and hand the
    // whole first frame back at line rate — the exact thing this module exists to stop.
    let free_after = |idle_ns: u64| {
        let mut pacer = exact(40_000_000);
        let settled = drain(&mut pacer, 200, 1200, 1_000_000_000);
        free_run(&mut pacer, 1200, settled + idle_ns)
    };

    let one_second = free_after(1_000_000_000);
    let one_minute = free_after(60 * 1_000_000_000);
    let one_hour = free_after(3_600 * 1_000_000_000);

    assert_eq!(
        one_second, one_minute,
        "credit kept accruing between a second and a minute of idling"
    );
    assert_eq!(
        one_minute, one_hour,
        "credit kept accruing between a minute and an hour of idling"
    );

    let free_bytes = one_hour as u32 * 1200;
    assert!(
        free_bytes >= DEFAULT_BURST_BYTES,
        "the allowance was not honoured: only {free_bytes} bytes went out freely"
    );
    assert!(
        free_bytes <= DEFAULT_BURST_BYTES + 1200,
        "{free_bytes} bytes went out freely on a {DEFAULT_BURST_BYTES} byte allowance"
    );
}

#[test]
fn the_burst_allowance_is_counted_in_bytes_rather_than_in_packets() {
    // Four times as many small packets go out freely as large ones, because it is the
    // bytes that could overflow the queue this protects, not the packet count.
    for bytes in [150usize, 300, 600, 1200] {
        let mut pacer = exact(40_000_000);
        let free = free_run(&mut pacer, bytes, 10_000_000_000);
        let free_bytes = free * bytes;
        let allowance = DEFAULT_BURST_BYTES as usize;

        assert!(
            free_bytes >= allowance,
            "{bytes}-byte packets: only {free_bytes} of {allowance} allowed bytes went free"
        );
        assert!(
            free_bytes <= allowance + bytes,
            "{bytes}-byte packets: {free_bytes} bytes went free on a {allowance} byte allowance"
        );
    }
}

#[test]
fn a_rate_change_takes_effect() {
    let mut pacer = exact(40_000_000);
    let mut now = 1_000_000_000u64;

    for bitrate in [40_000_000u32, 8_000_000, 100_000_000, 2_000_000, 25_000_000] {
        pacer.set_bitrate_bps(bitrate);
        assert_eq!(pacer.bitrate_bps(), bitrate);

        let (got, end) = rate_over(&mut pacer, 4_000, 1200, now);
        now = end;

        assert_close(
            got,
            paced_rate_of(bitrate),
            0.02,
            &format!("after moving to {bitrate} bps"),
        );
    }
}

#[test]
fn a_bitrate_below_the_floor_is_clamped_rather_than_stalling_the_session() {
    for bitrate in [0u32, 1, 999_999, MIN_BITRATE_BPS] {
        let mut pacer = exact(bitrate);
        assert_eq!(pacer.bitrate_bps(), MIN_BITRATE_BPS);

        let (got, end) = rate_over(&mut pacer, 500, 1200, 0);

        assert_close(
            got,
            paced_rate_of(MIN_BITRATE_BPS),
            0.02,
            &format!("a pacer asked for {bitrate} bps"),
        );
        assert!(
            end < 5_000_000_000,
            "500 packets at the floor took {end} ns, which is a stall and not a session"
        );
    }
}

#[test]
fn a_zero_burst_allowance_is_raised_so_the_head_of_a_frame_never_stalls() {
    let mut pacer = SendPacer::new(PacerConfig {
        bitrate_bps: 40_000_000,
        burst_bytes: 0,
        min_sleep: Duration::ZERO,
    });

    assert_eq!(pacer.burst_bytes(), MIN_BURST_BYTES);
    assert_eq!(
        pacer.wait_before(1200, 0),
        Duration::ZERO,
        "the first packet a pacer ever sees must not be delayed"
    );

    let settled = drain(&mut pacer, 100, 1200, 0);
    assert_eq!(
        pacer.wait_before(1200, settled + 100_000_000),
        Duration::ZERO,
        "the first packet after an idle gap must not be delayed either"
    );
}

#[test]
fn no_window_of_the_stream_runs_ahead_of_the_paced_rate() {
    // The property the whole module is for: over every interval, however chosen, the
    // bytes released cannot exceed what the rate allows plus the burst allowance and the
    // one packet that consumed it.
    let mut pacer = exact(40_000_000);
    let rate = paced_rate_of(40_000_000) as u64;
    let slack_bits = u64::from(DEFAULT_BURST_BYTES) * 8 + 1200 * 8;

    let mut now = 1_000_000_000u64;
    let mut sent_bits = 0u64;
    let mut trace = Vec::with_capacity(400);

    for i in 0..400usize {
        let bytes = 120 + (i * 271) % 1_081;
        now += pacer.wait_before(bytes, now).as_nanos() as u64;
        sent_bits += (bytes * 8) as u64;
        trace.push((now, sent_bits));
    }

    for (i, &(from_ns, from_bits)) in trace.iter().enumerate() {
        for &(to_ns, to_bits) in &trace[i + 1..] {
            let allowed = slack_bits + rate * (to_ns - from_ns) / 1_000_000_000;
            assert!(
                to_bits - from_bits <= allowed,
                "{} bits left in {} ns, where only {allowed} were allowed",
                to_bits - from_bits,
                to_ns - from_ns
            );
        }
    }
}

#[test]
fn absurd_inputs_do_not_panic_or_divide_by_zero() {
    let mut pacer = exact(u32::MAX);
    assert!(pacer.pacing_rate_bps() > 0);

    pacer.wait_before(usize::MAX, 0);
    pacer.wait_before(0, 0);
    pacer.wait_before(1200, u64::MAX);
    // A caller whose clock ran backwards. Wrong, but not a reason to abort a session.
    pacer.wait_before(1200, 0);
    pacer.wait_before(1200, u64::MAX);

    assert_eq!(pacer.packets(), 5);
}

#[test]
fn a_packet_with_no_bytes_costs_no_time() {
    let mut pacer = exact(40_000_000);

    for _ in 0..1_000 {
        assert!(
            pacer.wait_before(0, 0).is_zero(),
            "an empty packet cannot consume a rate measured in bytes"
        );
    }
}

#[test]
fn a_caller_that_will_never_sleep_is_never_asked_to() {
    let mut pacer = SendPacer::new(PacerConfig {
        bitrate_bps: 40_000_000,
        burst_bytes: MIN_BURST_BYTES,
        min_sleep: Duration::MAX,
    });

    for _ in 0..1_000 {
        assert!(
            pacer.wait_before(1200, 0).is_zero(),
            "a wait was reported to a caller that cannot honour any"
        );
    }
}

#[test]
fn the_reported_statistics_match_what_the_caller_was_handed() {
    let mut pacer = batched(40_000_000, Duration::from_millis(1));
    let mut now = 1_000_000_000u64;
    let mut waits = 0u64;
    let mut total = Duration::ZERO;

    for i in 0..2_000usize {
        let bytes = 200 + (i * 91) % 1_001;
        let wait = pacer.wait_before(bytes, now);

        if !wait.is_zero() {
            waits += 1;
        }
        total += wait;
        now += wait.as_nanos() as u64;
    }

    assert_eq!(pacer.packets(), 2_000);
    assert_eq!(pacer.waits(), waits);
    assert_eq!(pacer.total_wait(), total);
    assert!(
        waits > 0 && waits < 2_000,
        "{waits} of 2000 packets waited, which is not a batched stream"
    );
}
