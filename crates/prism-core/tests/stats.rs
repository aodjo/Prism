//! Tests for latency measurement.
//!
//! Milestones are judged on p99, so these check that the percentile is exactly the
//! nearest-rank sample rather than something interpolated, and that the window really
//! does forget old behaviour.

use prism_core::stats::LatencyRecorder;

#[test]
fn an_empty_recorder_has_nothing_to_summarize() {
    let mut recorder = LatencyRecorder::new(16);

    assert!(recorder.is_empty());
    assert_eq!(recorder.len(), 0);
    assert_eq!(recorder.capacity(), 16);
    assert!(recorder.summarize().is_none());
}

#[test]
fn a_single_sample_is_every_statistic() {
    let mut recorder = LatencyRecorder::new(16);
    recorder.record(42);

    let summary = recorder.summarize().unwrap();
    assert_eq!(summary.count, 1);
    assert_eq!(summary.min_us, 42);
    assert_eq!(summary.max_us, 42);
    assert_eq!(summary.mean_us, 42);
    assert_eq!(summary.p50_us, 42);
    assert_eq!(summary.p95_us, 42);
    assert_eq!(summary.p99_us, 42);
}

#[test]
fn percentiles_are_the_nearest_rank_sample() {
    let mut recorder = LatencyRecorder::new(100);
    for sample in 1..=100u32 {
        recorder.record(sample);
    }

    let summary = recorder.summarize().unwrap();
    assert_eq!(summary.count, 100);
    assert_eq!(summary.min_us, 1);
    assert_eq!(summary.max_us, 100);
    assert_eq!(summary.mean_us, 50, "mean of 1..=100 is 50.5, rounded down");
    assert_eq!(summary.p50_us, 50);
    assert_eq!(summary.p95_us, 95);
    assert_eq!(summary.p99_us, 99);
}

#[test]
fn insertion_order_does_not_affect_the_summary() {
    let mut ascending = LatencyRecorder::new(64);
    let mut descending = LatencyRecorder::new(64);

    for sample in 1..=64u32 {
        ascending.record(sample);
        descending.record(65 - sample);
    }

    assert_eq!(
        ascending.summarize().unwrap(),
        descending.summarize().unwrap()
    );
}

#[test]
fn the_window_forgets_the_oldest_samples() {
    let mut recorder = LatencyRecorder::new(4);

    for sample in [100, 200, 300, 400, 500, 600] {
        recorder.record(sample);
    }

    let summary = recorder.summarize().unwrap();
    assert_eq!(summary.count, 4);
    assert_eq!(summary.min_us, 300, "100 and 200 have been overwritten");
    assert_eq!(summary.max_us, 600);
}

#[test]
fn a_burst_of_jitter_leaves_the_window_eventually() {
    let mut recorder = LatencyRecorder::new(8);

    recorder.record(500_000);
    for _ in 0..7 {
        recorder.record(10_000);
    }
    assert_eq!(recorder.summarize().unwrap().max_us, 500_000);

    recorder.record(10_000);
    assert_eq!(
        recorder.summarize().unwrap().max_us,
        10_000,
        "the spike has aged out"
    );
}

#[test]
fn clearing_discards_every_sample() {
    let mut recorder = LatencyRecorder::new(8);
    for sample in 1..=8u32 {
        recorder.record(sample);
    }

    recorder.clear();

    assert!(recorder.is_empty());
    assert!(recorder.summarize().is_none());
    assert_eq!(recorder.capacity(), 8, "clearing keeps the window's memory");
}

#[test]
fn summarizing_repeatedly_is_stable_and_non_destructive() {
    let mut recorder = LatencyRecorder::new(32);
    for sample in 1..=32u32 {
        recorder.record(sample);
    }

    let first = recorder.summarize().unwrap();
    let second = recorder.summarize().unwrap();

    assert_eq!(first, second);
    assert_eq!(recorder.len(), 32, "summarizing must not consume samples");
}

#[test]
fn a_partially_filled_window_summarizes_only_what_it_holds() {
    let mut recorder = LatencyRecorder::new(1000);
    for sample in [7u32, 3, 9] {
        recorder.record(sample);
    }

    let summary = recorder.summarize().unwrap();
    assert_eq!(summary.count, 3);
    assert_eq!(summary.min_us, 3);
    assert_eq!(summary.max_us, 9);
    assert_eq!(
        summary.p50_us, 7,
        "nearest rank of 3 samples at p50 is the second"
    );
}

#[test]
fn a_window_of_one_tracks_only_the_latest_sample() {
    let mut recorder = LatencyRecorder::new(1);

    recorder.record(11);
    assert_eq!(recorder.summarize().unwrap().p99_us, 11);

    recorder.record(22);
    let summary = recorder.summarize().unwrap();
    assert_eq!(summary.count, 1);
    assert_eq!(summary.p99_us, 22);
}
