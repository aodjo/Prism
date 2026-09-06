//! Tests for display pacing.
//!
//! The pacer's whole justification is that it adds only as much delay as the path's own
//! spread, so these check both halves of that claim: a steady path is not slowed down at
//! all, and a jittery one is smoothed by exactly enough to cover its late arrivals.

use std::time::Duration;

use prism_core::render::pacing::PresentPacer;

/// Feeds an age repeatedly so the pacer has enough samples to settle on a target.
fn settle(pacer: &mut PresentPacer, ages: &[u32], rounds: usize) {
    for _ in 0..rounds {
        for &age in ages {
            pacer.hold_for(age);
        }
    }
}

#[test]
fn a_steady_path_is_not_delayed_at_all() {
    let mut pacer = PresentPacer::new(30_000);
    settle(&mut pacer, &[5_000], 200);

    assert_eq!(pacer.hold_for(5_000), Duration::ZERO);
    assert_eq!(pacer.delay_us(), 5_000, "the target matches the steady age");
    assert_eq!(pacer.shown_late(), 0, "nothing is late when nothing varies");
}

#[test]
fn a_jittery_path_is_held_long_enough_to_cover_its_late_arrivals() {
    let mut pacer = PresentPacer::new(30_000);
    settle(
        &mut pacer,
        &[4_000, 12_000, 5_000, 11_000, 4_500, 13_000],
        40,
    );

    assert!(
        pacer.delay_us() >= 12_000,
        "the target must cover the late arrivals"
    );
    assert!(
        pacer.delay_us() <= 13_000,
        "and no more than the worst of them"
    );

    let early = pacer.hold_for(4_000);
    assert!(
        early >= Duration::from_micros(8_000),
        "an early picture waits, got {early:?}"
    );
}

#[test]
fn a_picture_that_is_already_late_is_shown_immediately() {
    let mut pacer = PresentPacer::new(30_000);
    settle(&mut pacer, &[5_000], 200);

    let before = pacer.shown_late();
    assert_eq!(pacer.hold_for(50_000), Duration::ZERO);
    assert_eq!(
        pacer.shown_late(),
        before + 1,
        "it is counted rather than ignored"
    );
}

#[test]
fn the_delay_never_exceeds_the_ceiling() {
    let mut pacer = PresentPacer::new(5_000);
    settle(&mut pacer, &[80_000, 90_000, 100_000], 60);

    assert_eq!(
        pacer.delay_us(),
        5_000,
        "a collapsing path cannot push the delay past the cap"
    );
    assert!(
        pacer.shown_late() > 0,
        "beyond the cap, pictures are late by definition"
    );
}

#[test]
fn the_delay_rises_at_once_and_comes_down_gradually() {
    let mut pacer = PresentPacer::new(60_000);

    settle(&mut pacer, &[2_000], 300);
    let calm = pacer.delay_us();
    assert_eq!(calm, 2_000);

    settle(&mut pacer, &[40_000], 300);
    let spiked = pacer.delay_us();
    assert_eq!(spiked, 40_000, "a worse path is absorbed straight away");

    settle(&mut pacer, &[2_000], 300);
    let recovering = pacer.delay_us();
    assert!(
        recovering < spiked,
        "the delay does come back down, got {recovering}"
    );
    assert!(
        recovering > calm,
        "but not in one go — a buffer that collapses at once oscillates, got {recovering}"
    );
}

#[test]
fn the_delay_reaches_the_floor_rather_than_creeping_forever() {
    let mut pacer = PresentPacer::new(60_000);

    settle(&mut pacer, &[40_000], 300);
    assert_eq!(pacer.delay_us(), 40_000);

    settle(&mut pacer, &[2_000], 6_000);
    assert_eq!(
        pacer.delay_us(),
        2_000,
        "geometric decay alone would never arrive"
    );
}

#[test]
fn the_added_delay_is_reported_rather_than_hidden() {
    let mut pacer = PresentPacer::new(30_000);
    settle(&mut pacer, &[3_000, 9_000], 60);

    let summary = pacer.held_summary().expect("pictures have been paced");
    assert_eq!(summary.count as u64, pacer.total());
    assert!(
        summary.max_us > 0,
        "an early picture was held for some time"
    );
    assert!(summary.min_us == 0, "a late one was not held at all");
}

#[test]
fn a_pacer_with_no_headroom_never_delays_anything() {
    let mut pacer = PresentPacer::new(0);
    settle(&mut pacer, &[1_000, 20_000, 5_000], 40);

    assert_eq!(pacer.delay_us(), 0);
    assert_eq!(
        pacer.hold_for(1_000),
        Duration::ZERO,
        "a zero ceiling disables pacing"
    );
}
