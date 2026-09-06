//! Tests for the loss injector.
//!
//! This is measurement equipment, and equipment that lies about what it did makes every
//! number taken with it worthless. The properties that matter are that the configured rate
//! is actually achieved, that a seed reproduces a run exactly, and that the boundaries do
//! what they say.

use prism_core::net::loss::LossInjector;

/// Runs `count` packets through an injector and returns how many were dropped.
fn run(per_million: u32, seed: u64, count: usize) -> u64 {
    let mut injector = LossInjector::new(per_million, seed);
    for _ in 0..count {
        injector.should_drop();
    }
    injector.tally().1
}

#[test]
fn a_rate_of_zero_drops_nothing() {
    assert_eq!(run(0, 1, 100_000), 0);
}

#[test]
fn a_rate_of_everything_drops_everything() {
    assert_eq!(run(1_000_000, 1, 10_000), 10_000);
}

#[test]
fn a_rate_above_everything_is_clamped_rather_than_wrapping() {
    assert_eq!(run(u32::MAX, 1, 10_000), 10_000);
}

#[test]
fn the_configured_rate_is_the_rate_actually_applied() {
    // Five percent is the figure M4 is judged at, so it is the one worth pinning. Over a
    // hundred thousand packets the sampling error is well under a tenth of a percent.
    let mut injector = LossInjector::new(50_000, 0xDEAD_BEEF);
    for _ in 0..100_000 {
        injector.should_drop();
    }

    let achieved = injector.achieved_per_million();
    assert!(
        (45_000..=55_000).contains(&achieved),
        "asked for 5% and got {:.2}%",
        f64::from(achieved) / 10_000.0
    );
}

#[test]
fn the_same_seed_gives_the_same_run() {
    // The point of seeding: a failure at 5% loss has to be reproducible, or debugging it is
    // guesswork.
    let first = run(50_000, 12345, 5_000);
    let second = run(50_000, 12345, 5_000);

    assert_eq!(first, second);
}

#[test]
fn different_seeds_give_different_runs() {
    let first = run(50_000, 1, 5_000);
    let second = run(50_000, 2, 5_000);

    assert_ne!(
        first, second,
        "two seeds producing an identical run would mean the seed is being ignored"
    );
}

#[test]
fn a_seed_of_zero_still_produces_a_usable_sequence() {
    // An all-zero xorshift state is a fixed point: it would drop everything or nothing
    // forever, and the caller would have no idea.
    let mut injector = LossInjector::new(50_000, 0);
    for _ in 0..20_000 {
        injector.should_drop();
    }

    let achieved = injector.achieved_per_million();
    assert!(
        (40_000..=60_000).contains(&achieved),
        "a zero seed still gives roughly the configured rate, got {achieved} ppm"
    );
}

#[test]
fn the_tally_counts_everything_it_was_asked_about() {
    let mut injector = LossInjector::new(50_000, 7);
    for _ in 0..1_234 {
        injector.should_drop();
    }

    let (considered, dropped) = injector.tally();
    assert_eq!(considered, 1_234);
    assert!(dropped > 0 && dropped < 1_234);
}

#[test]
fn nothing_considered_reports_no_loss_rather_than_dividing_by_zero() {
    let injector = LossInjector::new(50_000, 1);

    assert_eq!(injector.achieved_per_million(), 0);
    assert_eq!(injector.tally(), (0, 0));
}

#[test]
fn a_low_rate_is_not_rounded_away() {
    // A tenth of a percent has to survive the integer arithmetic; rounding it to zero would
    // silently turn a light-loss test into a clean-link test.
    let mut injector = LossInjector::new(1_000, 99);
    for _ in 0..200_000 {
        injector.should_drop();
    }

    let achieved = injector.achieved_per_million();
    assert!(
        (700..=1_300).contains(&achieved),
        "asked for 1000 ppm and got {achieved}"
    );
}
