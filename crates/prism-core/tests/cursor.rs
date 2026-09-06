//! Tests for client-side cursor prediction.
//!
//! The whole point of predicting is that the cursor moves before the host has confirmed
//! anything, and the whole risk of predicting is that a correction undoes movement the host
//! simply has not seen yet. These check both halves against each other.

use prism_core::cursor::CursorTracker;
use prism_core::net::packet::CursorPosition;

/// Builds a reading from the host at a given instant of its clock.
fn reading(sample_ts_us: u64, x: u16, y: u16) -> CursorPosition {
    CursorPosition {
        sample_ts_us,
        x,
        y,
        screen_width: 2560,
        screen_height: 1440,
    }
}

#[test]
fn nothing_is_drawn_before_the_host_has_ever_reported() {
    let mut tracker = CursorTracker::new();

    tracker.moved(10, 40, 30);

    assert_eq!(
        tracker.normalised(),
        None,
        "the screen being predicted against is not known yet"
    );
}

#[test]
fn movement_sent_before_the_first_reading_is_replayed_onto_it() {
    let mut tracker = CursorTracker::new();

    tracker.moved(2000, 100, 50);
    tracker.observe(reading(1000, 800, 400));

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!(
        (x * 2560.0 - 900.0).abs() < 0.5,
        "the movement the reading predates was replayed"
    );
    assert!((y * 1440.0 - 450.0).abs() < 0.5);
}

#[test]
fn a_movement_shows_immediately_rather_than_waiting_for_the_host() {
    let mut tracker = CursorTracker::new();
    tracker.observe(reading(1000, 800, 400));

    tracker.moved(1500, 60, -40);

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!((x * 2560.0 - 860.0).abs() < 0.5, "applied without waiting");
    assert!((y * 1440.0 - 360.0).abs() < 0.5);
}

#[test]
fn a_stale_reading_does_not_rewind_movement_the_host_has_not_seen() {
    // This is the failure prediction exists to avoid. The host reports where it was one
    // round trip ago; without replaying what has been sent since, the cursor jumps back.
    let mut tracker = CursorTracker::new();
    tracker.observe(reading(1000, 800, 400));

    tracker.moved(1100, 10, 0);
    tracker.moved(1200, 10, 0);
    tracker.moved(1300, 10, 0);

    // The host answers about instant 1150, so it has only seen the first movement.
    tracker.observe(reading(1150, 810, 400));

    let (x, _) = tracker.normalised().expect("a reading has arrived");
    assert!(
        (x * 2560.0 - 830.0).abs() < 0.5,
        "the two movements the host had not seen were replayed, not discarded"
    );
}

#[test]
fn a_reading_that_accounts_for_everything_leaves_nothing_pending() {
    let mut tracker = CursorTracker::new();

    tracker.moved(1000, 10, 10);
    tracker.moved(1100, 10, 10);
    tracker.observe(reading(1200, 900, 500));

    assert_eq!(tracker.pending(), 0);

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!(
        (x * 2560.0 - 900.0).abs() < 0.5,
        "the reading is taken as-is when it is newer than everything sent"
    );
    assert!((y * 1440.0 - 500.0).abs() < 0.5);
}

#[test]
fn the_host_moving_its_own_pointer_wins() {
    // The client sent nothing, so whatever the host reports is the whole truth.
    let mut tracker = CursorTracker::new();
    tracker.observe(reading(1000, 800, 400));

    tracker.observe(reading(2000, 100, 100));

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!((x * 2560.0 - 100.0).abs() < 0.5);
    assert!((y * 1440.0 - 100.0).abs() < 0.5);
}

#[test]
fn prediction_stops_at_the_edge_the_host_would_stop_at() {
    let mut tracker = CursorTracker::new();
    tracker.observe(reading(1000, 10, 10));

    for _ in 0..100 {
        tracker.moved(1100, -500, -500);
    }

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!(x >= 0.0, "clamped rather than running off the left");
    assert!(y >= 0.0, "clamped rather than running off the top");

    for _ in 0..100 {
        tracker.moved(1200, 500, 500);
    }

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!(x < 1.0, "clamped rather than running off the right");
    assert!(y < 1.0, "clamped rather than running off the bottom");
}

#[test]
fn a_host_that_goes_quiet_forgets_the_oldest_rather_than_growing_without_end() {
    let mut tracker = CursorTracker::new();
    tracker.observe(reading(1000, 800, 400));

    for i in 0..5000 {
        tracker.moved(2000 + i, 1, 0);
    }

    assert!(
        tracker.pending() <= 512,
        "the queue is bounded, not unbounded"
    );
    assert!(
        tracker.forgotten() > 0,
        "and it says so rather than hiding it"
    );
}

#[test]
fn a_reading_carries_the_screen_it_was_taken_on() {
    // A host that changes resolution mid-session must not leave the client scaling against
    // a screen that no longer exists.
    let mut tracker = CursorTracker::new();
    tracker.observe(reading(1000, 1280, 720));

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!((x - 0.5).abs() < 0.01, "half way across a 2560 wide screen");
    assert!((y - 0.5).abs() < 0.01);

    tracker.observe(CursorPosition {
        sample_ts_us: 2000,
        x: 640,
        y: 360,
        screen_width: 1280,
        screen_height: 720,
    });

    let (x, y) = tracker.normalised().expect("a reading has arrived");
    assert!((x - 0.5).abs() < 0.01, "still half way, on the new screen");
    assert!((y - 0.5).abs() < 0.01);
}
