//! Tests for the received-frame acknowledgement tracker.
//!
//! Two things are built on this report and both act on it: the encoder chooses which frame
//! to reference from it, and the congestion controller reads loss from it. A bitmap that is
//! off by one place claims a frame arrived that did not, and the encoder then references a
//! frame the client cannot decode — which corrupts the stream silently rather than loudly.

use prism_core::net::ack::{AckTracker, missing_in_history};

#[test]
fn nothing_is_reported_before_anything_arrives() {
    let tracker = AckTracker::new();

    assert!(
        tracker.report(1000).is_none(),
        "a report anchored on frame zero would be a lie the encoder acts on"
    );
}

#[test]
fn the_first_frame_anchors_the_report_with_an_empty_history() {
    let mut tracker = AckTracker::new();
    tracker.received(7);

    let report = tracker.report(1000).expect("a frame has arrived");
    assert_eq!(report.last_frame_id, 7);
    assert_eq!(
        report.recv_bitmap, 0,
        "there is no history behind the first"
    );
    assert_eq!(report.client_ts_us, 1000);
}

#[test]
fn a_clean_run_fills_the_history() {
    let mut tracker = AckTracker::new();
    for frame_id in 0..40 {
        tracker.received(frame_id);
    }

    let report = tracker.report(0).expect("frames have arrived");
    assert_eq!(report.last_frame_id, 39);
    assert_eq!(
        report.recv_bitmap,
        u32::MAX,
        "every one of the previous thirty-two arrived"
    );
    assert_eq!(missing_in_history(report.recv_bitmap), 0);
}

#[test]
fn a_gap_shows_up_in_the_right_place() {
    let mut tracker = AckTracker::new();
    for frame_id in 0..10 {
        if frame_id != 7 {
            tracker.received(frame_id);
        }
    }

    let report = tracker.report(0).expect("frames have arrived");
    assert_eq!(report.last_frame_id, 9);

    // Frame 9 is the anchor, so bit 0 is frame 8, bit 1 is frame 7. That is the one missing.
    assert_eq!(report.recv_bitmap & 0b1, 0b1, "frame 8 arrived");
    assert_eq!(report.recv_bitmap & 0b10, 0, "frame 7 did not");
    assert_eq!(report.recv_bitmap & 0b100, 0b100, "frame 6 arrived");
}

#[test]
fn a_frame_that_arrives_late_is_still_credited() {
    // Reordering is normal on a real path, and a frame counted as lost because it overtook
    // its neighbour would make the encoder throw away a reference it could have used.
    let mut tracker = AckTracker::new();
    tracker.received(0);
    tracker.received(1);
    tracker.received(3);

    // Anchored on frame 3, bit 0 is frame 2 and bit 1 is frame 1.
    let before = tracker.report(0).expect("frames have arrived");
    assert_eq!(before.recv_bitmap & 0b1, 0, "frame 2 has not arrived");
    assert_eq!(before.recv_bitmap & 0b10, 0b10, "but frame 1 did");

    tracker.received(2);

    let after = tracker.report(0).expect("frames have arrived");
    assert_eq!(
        after.last_frame_id, 3,
        "a late frame does not move the anchor"
    );
    assert_eq!(after.recv_bitmap & 0b1, 0b1, "frame 2 is now credited");
}

#[test]
fn a_frame_older_than_the_window_is_dropped_rather_than_wrapped_into_it() {
    // The dangerous failure: an out-of-range shift that silently sets some other frame's
    // bit, telling the encoder a frame arrived that never did.
    let mut tracker = AckTracker::new();
    for frame_id in 0..50 {
        tracker.received(frame_id);
    }

    let before = tracker.report(0).expect("frames have arrived");
    tracker.received(2);
    let after = tracker.report(0).expect("frames have arrived");

    assert_eq!(
        before.recv_bitmap, after.recv_bitmap,
        "a frame far outside the window changes nothing"
    );
    assert_eq!(before.last_frame_id, after.last_frame_id);
}

#[test]
fn a_jump_forward_beyond_the_window_clears_the_history() {
    let mut tracker = AckTracker::new();
    for frame_id in 0..40 {
        tracker.received(frame_id);
    }

    tracker.received(200);

    let report = tracker.report(0).expect("frames have arrived");
    assert_eq!(report.last_frame_id, 200);
    assert_eq!(
        report.recv_bitmap, 0,
        "nothing in the old history is still describable, and claiming otherwise would \
         acknowledge frames that were never received"
    );
}

#[test]
fn a_jump_of_exactly_the_window_clears_the_history() {
    // The boundary where a shift of 32 on a u32 is undefined in most languages and a panic
    // in debug Rust. Worth pinning on its own.
    let mut tracker = AckTracker::new();
    for frame_id in 0..40 {
        tracker.received(frame_id);
    }

    tracker.received(39 + 32);

    let report = tracker.report(0).expect("frames have arrived");
    assert_eq!(report.last_frame_id, 71);
    assert_eq!(report.recv_bitmap, 0);
}

#[test]
fn a_jump_just_inside_the_window_keeps_only_what_is_still_describable() {
    let mut tracker = AckTracker::new();
    for frame_id in 0..40 {
        tracker.received(frame_id);
    }

    tracker.received(39 + 31);

    // Anchored on 70, bit n is frame 69 - n. Frames 40 to 69 never arrived, so bits 0 to 29
    // are clear. Bit 30 is frame 39, the old anchor, and bit 31 is frame 38 — the only two
    // received frames still inside the window.
    let report = tracker.report(0).expect("frames have arrived");
    assert_eq!(report.last_frame_id, 70);
    assert_eq!(report.recv_bitmap, (1u32 << 31) | (1u32 << 30));
    assert_eq!(missing_in_history(report.recv_bitmap), 30);
}

#[test]
fn identifiers_that_wrap_are_still_ordered_correctly() {
    // A frame counter is monotonic and wraps. Comparing raw values would make the frame
    // after the wrap look like the oldest thing ever seen, and the tracker would stop
    // advancing for good.
    let mut tracker = AckTracker::new();
    tracker.received(u32::MAX - 1);
    tracker.received(u32::MAX);
    tracker.received(0);
    tracker.received(1);

    let report = tracker.report(0).expect("frames have arrived");
    assert_eq!(report.last_frame_id, 1, "the wrap did not stall the anchor");
    assert_eq!(
        report.recv_bitmap & 0b111,
        0b111,
        "the three frames across the wrap are all credited"
    );
}

#[test]
fn a_repeated_frame_does_not_shift_the_history() {
    let mut tracker = AckTracker::new();
    tracker.received(5);
    tracker.received(6);
    let before = tracker.report(0).expect("frames have arrived");

    tracker.received(6);
    let after = tracker.report(0).expect("frames have arrived");

    assert_eq!(before.last_frame_id, after.last_frame_id);
    assert_eq!(
        before.recv_bitmap, after.recv_bitmap,
        "a duplicate is not a new frame"
    );
}

#[test]
fn missing_counts_the_gaps_and_nothing_else() {
    assert_eq!(missing_in_history(u32::MAX), 0);
    assert_eq!(missing_in_history(0), 32);
    assert_eq!(missing_in_history(u32::MAX ^ 0b101), 2);
}
