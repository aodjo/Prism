//! Behavioural tests for the congestion controller.
//!
//! Everything here is driven through the public surface with fabricated timestamps, which
//! is the whole reason the controller reads no clock of its own. The tests assert
//! properties — the rate never leaves its bounds, a clean link ends at the ceiling, a
//! constant clock offset error changes nothing — rather than the specific numbers the
//! current parameters happen to produce, so retuning a default does not force a rewrite.

use prism_core::net::cc::{
    CongestionConfig, CongestionController, CongestionStats, DelaySample, RateAction,
    equivalent_packet_loss,
};

/// Frame interval at 120 fps, the frame rate the project targets.
const FRAME_US_120: u64 = 8_333;

/// Frame interval at 30 fps.
///
/// Worth testing separately: it is longer than the shortest tick, so every decision window
/// holds exactly one frame and the minimum filter has nothing to work with. That is the
/// hostile case for outlier rejection, and the plan records the ~58 ms stall happening at
/// 30 fps as well as 60.
const FRAME_US_30: u64 = 33_333;

/// Baseline one-way delay in microseconds, near this project's measured LAN arrival p50.
const BASE_DELAY_US: i64 = 1_400;

/// The unexplained periodic stall from `docs/plan.md`, in microseconds.
const STALL_US: i64 = 58_000;

/// Frames in an acknowledgement bitmap, matching the wire format's 32-frame history.
const BITMAP_FRAMES: f32 = 32.0;

/// What a run of frames produced.
///
/// Every decision taken during the run is recorded rather than only the last one, because
/// the cooldown means the final tick of a congested run is usually a hold. Asking "did it
/// ever cut for loss" is the question the tests actually want answered.
struct Run {
    /// Time just after the last frame of the run, in microseconds.
    end_us: u64,
    /// Whether any tick raised the rate.
    increased: bool,
    /// Whether any tick cut the rate because delay was trending up.
    cut_for_delay: bool,
    /// Whether any tick cut the rate because of loss.
    cut_for_loss: bool,
    /// Observation time of each tick that cut the rate, in microseconds.
    cut_times_us: Vec<u64>,
    /// Largest delay gradient the controller computed during the run, in microseconds.
    max_abs_gradient_us: i64,
}

/// Feeds a run of frames into the controller and reports what it decided.
///
/// `delay` is called with the frame's index within this run so a caller can shape a ramp,
/// a spike, or a flat line without building a vector first.
fn feed(
    cc: &mut CongestionController,
    start_us: u64,
    frame_us: u64,
    frames: usize,
    mut delay: impl FnMut(usize) -> i64,
    loss: f32,
) -> Run {
    let mut run = Run {
        end_us: start_us,
        increased: false,
        cut_for_delay: false,
        cut_for_loss: false,
        cut_times_us: Vec::new(),
        max_abs_gradient_us: 0,
    };
    let mut ticks = cc.stats().ticks;

    for index in 0..frames {
        let observed_at_us = run.end_us;
        cc.observe(&DelaySample {
            one_way_delay_us: delay(index),
            observed_at_us,
            frame_loss: loss,
        });
        run.end_us += frame_us;

        let stats = cc.stats();
        if stats.ticks != ticks {
            ticks = stats.ticks;
            run.max_abs_gradient_us = run.max_abs_gradient_us.max(stats.last_gradient_us.abs());
            match stats.last_action {
                RateAction::Increase => run.increased = true,
                RateAction::DecreaseDelay => {
                    run.cut_for_delay = true;
                    run.cut_times_us.push(observed_at_us);
                }
                RateAction::DecreaseLoss => {
                    run.cut_for_loss = true;
                    run.cut_times_us.push(observed_at_us);
                }
                RateAction::Hold => {}
            }
        }
    }

    run
}

/// Feeds a flat, lossless run.
fn feed_clean(cc: &mut CongestionController, start_us: u64, frame_us: u64, frames: usize) -> Run {
    feed(cc, start_us, frame_us, frames, |_| BASE_DELAY_US, 0.0)
}

/// Returns the frame loss that a given packet loss produces.
///
/// The forward direction of the relation `equivalent_packet_loss` inverts, written out
/// independently here so the two are a genuine cross-check rather than one calling the
/// other. A frame survives only if all of its packets do.
fn frame_loss_for(packet_loss: f32, packets_per_frame: f32) -> f32 {
    1.0 - (1.0 - packet_loss).powf(packets_per_frame)
}

/// Returns the frame loss a bitmap with `missing` of its 32 frames absent reports.
fn frame_loss_from_bitmap(missing: u32) -> f32 {
    missing as f32 / BITMAP_FRAMES
}

/// A deterministic pseudo-random source for the bounds fuzz.
///
/// An LCG rather than a real generator because the test needs to be reproducible from the
/// seed alone and does not need statistical quality.
struct Lcg(u64);

impl Lcg {
    /// Returns the next value.
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// Returns the next value reduced into `0..modulus`.
    fn below(&mut self, modulus: u64) -> u64 {
        self.next() % modulus
    }
}

#[test]
fn a_clean_link_climbs_to_the_ceiling() {
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    feed_clean(&mut cc, 0, FRAME_US_120, 1_200);

    assert_eq!(
        cc.target_bps(),
        config.max_bps,
        "ten seconds of flat delay and no loss must reach the ceiling"
    );
    assert_eq!(cc.stats().backoffs, 0, "nothing here justifies a back-off");
}

#[test]
fn the_rate_is_bounded_above_however_long_the_link_stays_clean() {
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);
    let mut now = 0;

    for _ in 0..40 {
        now = feed_clean(&mut cc, now, FRAME_US_120, 1_200).end_us;
        assert!(
            cc.target_bps() <= config.max_bps,
            "the rate climbed past the ceiling"
        );
    }

    assert_eq!(cc.target_bps(), config.max_bps);
}

#[test]
fn sustained_rising_delay_backs_off() {
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let now = feed_clean(&mut cc, 0, FRAME_US_120, 1_200).end_us;
    let before = cc.target_bps();

    // A queue building in front of the stream: delay climbing steadily, no loss yet,
    // which is the signal the whole design exists to catch early.
    let run = feed(
        &mut cc,
        now,
        FRAME_US_120,
        120,
        |index| BASE_DELAY_US + (index as i64) * 800,
        0.0,
    );

    assert!(
        cc.target_bps() < before,
        "a second of rising delay must cut the rate, was {before}, now {}",
        cc.target_bps()
    );
    assert!(
        run.cut_for_delay && !run.cut_for_loss,
        "the cut must be attributed to delay, since there was no loss at all"
    );
}

#[test]
fn sustained_loss_backs_off() {
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let now = feed_clean(&mut cc, 0, FRAME_US_120, 1_200).end_us;
    let before = cc.target_bps();

    // Loss without any delay signal at all, which is what a policer or a wireless link
    // looks like: the packets never queue, they simply stop existing.
    let loss = frame_loss_for(config.loss_decrease_floor * 2.0, config.packets_per_frame);
    let run = feed(&mut cc, now, FRAME_US_120, 240, |_| BASE_DELAY_US, loss);

    assert!(
        cc.target_bps() < before,
        "loss well above the back-off threshold must cut the rate"
    );
    assert!(
        run.cut_for_loss && !run.cut_for_delay,
        "the cut must be attributed to loss, since the delay never moved"
    );
}

#[test]
fn loss_below_the_increase_ceiling_does_not_stop_the_climb() {
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let loss = frame_loss_for(config.loss_increase_ceiling / 2.0, config.packets_per_frame);
    feed(&mut cc, 0, FRAME_US_120, 1_200, |_| BASE_DELAY_US, loss);

    assert_eq!(
        cc.target_bps(),
        config.max_bps,
        "loss under the ceiling is noise, not a reason to stall"
    );
    assert_eq!(cc.stats().backoffs, 0);
}

#[test]
fn the_thresholds_are_calibrated_against_the_bitmap_the_feedback_actually_carries() {
    // The whole point of converting into the packet domain. One frame missing from a
    // thirty-two frame bitmap is 3.1% frame loss, which a threshold written in packet
    // terms and applied directly would read as a catastrophe.
    let config = CongestionConfig::for_round_trip_us(480);

    for (missing, expected) in [
        (1, RateAction::Increase),
        (2, RateAction::Hold),
        (6, RateAction::DecreaseLoss),
    ] {
        let mut cc = CongestionController::new(config);
        let before = cc.target_bps();

        let run = feed(
            &mut cc,
            0,
            FRAME_US_120,
            120,
            |_| BASE_DELAY_US,
            frame_loss_from_bitmap(missing),
        );

        let reported = f64::from(cc.stats().packet_loss) * 100.0;
        assert!(
            !run.cut_for_delay,
            "{missing} of 32 frames missing must never look like a delay trend"
        );

        match expected {
            RateAction::Increase => {
                assert!(
                    run.increased && !run.cut_for_loss,
                    "{reported:.4}% packet loss"
                );
                assert!(cc.target_bps() > before);
            }
            RateAction::Hold => {
                assert!(
                    !run.increased && !run.cut_for_loss,
                    "{reported:.4}% packet loss should neither climb nor cut"
                );
                assert_eq!(cc.target_bps(), before);
            }
            _ => {
                assert!(run.cut_for_loss, "{reported:.4}% packet loss should cut");
                assert!(cc.target_bps() < before);
            }
        }
    }
}

#[test]
fn an_isolated_58ms_stall_does_not_cause_a_lasting_collapse() {
    // docs/plan.md records a periodic ~58 ms stall on this machine, the same size at 30 and
    // 60 fps, believed to be media-engine contention rather than congestion. A controller
    // that reads it as congestion collapses the rate and never recovers.
    for frame_us in [FRAME_US_120, FRAME_US_30] {
        let config = CongestionConfig::for_round_trip_us(480);
        let mut cc = CongestionController::new(config);
        let frames_per_second = (1_000_000 / frame_us) as usize;

        feed(
            &mut cc,
            0,
            frame_us,
            frames_per_second * 20,
            |index| {
                if index % frames_per_second == frames_per_second / 2 {
                    BASE_DELAY_US + STALL_US
                } else {
                    BASE_DELAY_US
                }
            },
            0.0,
        );

        assert_eq!(
            cc.stats().backoffs,
            0,
            "one stalled frame a second at {frame_us} us per frame is not a delay trend"
        );
        assert_eq!(
            cc.target_bps(),
            config.max_bps,
            "the rate must still reach the ceiling despite the stalls"
        );
    }
}

#[test]
fn a_single_stall_at_any_phase_of_the_tick_costs_nothing_lasting() {
    // The dangerous alignment is a stall that lands alone in a decision window, so the
    // window's minimum is the stalled sample. Every phase is tried rather than one, because
    // whether that happens depends entirely on where the tick boundary fell.
    for frame_us in [FRAME_US_120, FRAME_US_30] {
        for phase in 0..24usize {
            let config = CongestionConfig::for_round_trip_us(480);
            let mut cc = CongestionController::new(config);

            let now = feed_clean(&mut cc, 0, frame_us, 60).end_us;
            let before = cc.target_bps();

            let after_stall = feed(
                &mut cc,
                now,
                frame_us,
                24,
                |index| {
                    if index == phase {
                        BASE_DELAY_US + STALL_US
                    } else {
                        BASE_DELAY_US
                    }
                },
                0.0,
            )
            .end_us;

            assert_eq!(
                cc.stats().backoffs,
                0,
                "a lone stall at phase {phase} ({frame_us} us per frame) was read as a trend"
            );

            feed_clean(&mut cc, after_stall, frame_us, 240);
            assert!(
                cc.target_bps() > before,
                "the rate must keep climbing after a lone stall at phase {phase}"
            );
        }
    }
}

#[test]
fn a_stall_that_spares_some_frames_in_every_window_is_not_congestion() {
    // The difference between a host-side stall and a congested path: congestion delays
    // every frame behind it, while media-engine contention delays the frames it happens to
    // land on and leaves their neighbours untouched. A window that still contains a clean
    // frame is proof the path itself is not queueing, whatever the other frames did.
    //
    // The magnitudes deliberately climb and reset rather than repeating, so a controller
    // summarising each window by its worst or its average sample sees a rising trend and
    // backs off. One summarising by its best sees nothing at all, which is correct.
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let run = feed(
        &mut cc,
        0,
        FRAME_US_120,
        2_400,
        |index| {
            if index % 2 == 0 {
                let severity = (index / 2) % 6 + 1;
                BASE_DELAY_US + STALL_US * severity as i64 / 6
            } else {
                BASE_DELAY_US
            }
        },
        0.0,
    );

    assert!(
        !run.cut_for_delay && !run.cut_for_loss,
        "stalls that spare a frame in every window are not a path signal"
    );
    assert_eq!(
        cc.target_bps(),
        config.max_bps,
        "the rate must still reach the ceiling through the stalls"
    );
}

#[test]
fn the_rate_recovers_once_congestion_clears() {
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let mut now = feed_clean(&mut cc, 0, FRAME_US_120, 1_200).end_us;
    let healthy = cc.target_bps();

    now = feed(
        &mut cc,
        now,
        FRAME_US_120,
        360,
        |index| BASE_DELAY_US + (index as i64) * 800,
        frame_loss_for(config.loss_decrease_floor * 2.0, config.packets_per_frame),
    )
    .end_us;

    let congested = cc.target_bps();
    assert!(
        congested < healthy,
        "three seconds of rising delay and heavy loss must cut the rate"
    );

    // The path clears completely: the queue has drained, so the delay is back at baseline.
    feed_clean(&mut cc, now, FRAME_US_120, 2_400);

    assert_eq!(
        cc.target_bps(),
        config.max_bps,
        "a controller that ratchets down and never climbs back is worse than none"
    );
}

#[test]
fn a_constant_clock_offset_error_changes_no_decision() {
    // The point of taking a gradient. Every measured one-way delay carries the error left
    // in the clock offset estimate; this project has measured an offset of -818.76 ms
    // between two machines, so that error can be enormous. If it cancels in the
    // subtraction, the controller behaves identically. If it does not, the controller is
    // steering on a number that means nothing.
    let reference = trace_of(0);

    for offset_us in [
        -818_760_i64,
        -3_000_000,
        -1,
        1,
        250_000,
        5_000_000,
        1_000_000_000,
    ] {
        let shifted = trace_of(offset_us);

        assert_eq!(
            shifted.0, reference.0,
            "an offset error of {offset_us} us changed the rate the controller chose"
        );
        assert_eq!(
            shifted.1, reference.1,
            "an offset error of {offset_us} us changed the controller's internal state"
        );
    }
}

#[test]
fn realistic_clock_drift_changes_no_decision() {
    // A constant error cancels exactly; drift is what survives the subtraction. Two
    // free-running crystals drift by at most a few hundred parts per million relative to
    // each other, which across a 25 ms window is tens of nanoseconds. This asserts the
    // claim the design rests on: at that rate the residue is far below anything the
    // controller reacts to.
    let reference = trace_of(0);

    for ppm in [-200_i64, -50, 50, 200] {
        let config = CongestionConfig::for_round_trip_us(480);
        let mut cc = CongestionController::new(config);
        let mut rates = Vec::new();
        let mut now = 0u64;

        for index in 0..SCENARIO_FRAMES {
            let drift_us = (now as i64) * ppm / 1_000_000;
            rates.push(cc.observe(&DelaySample {
                one_way_delay_us: scenario_delay_us(index) + drift_us,
                observed_at_us: now,
                frame_loss: scenario_loss(index),
            }));
            now += FRAME_US_120;
        }

        assert_eq!(
            rates, reference.0,
            "{ppm} ppm of clock drift changed the rate the controller chose"
        );
    }
}

#[test]
fn a_negative_measured_delay_is_handled_like_any_other() {
    // What a client whose clock runs ahead of the host's actually measures. Clamping this
    // at zero, which the display path deliberately does, would flatten the gradient to
    // nothing and silently disable the controller.
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let now = feed(
        &mut cc,
        0,
        FRAME_US_120,
        1_200,
        |_| BASE_DELAY_US - 900_000,
        0.0,
    )
    .end_us;
    assert_eq!(cc.target_bps(), config.max_bps);

    feed(
        &mut cc,
        now,
        FRAME_US_120,
        120,
        |index| BASE_DELAY_US - 900_000 + (index as i64) * 800,
        0.0,
    );

    assert!(
        cc.target_bps() < config.max_bps,
        "a rise from -900 ms to -800 ms is still a rise"
    );
}

#[test]
fn one_congestion_event_is_never_cut_for_twice_inside_a_cooldown() {
    // A rate cut takes a round trip to reach the bottleneck and longer for its queue to
    // drain, so delay keeps rising for a while after the controller has already answered
    // it. Treating that as new congestion cuts again and again for one event, which is how
    // a controller ends up pinned at its floor on a link that was only briefly busy.
    for round_trip_us in [480, 12_000, 45_000] {
        let config = CongestionConfig::for_round_trip_us(round_trip_us);
        let mut cc = CongestionController::new(config);

        let now = feed_clean(&mut cc, 0, FRAME_US_120, 1_200).end_us;
        let run = feed(
            &mut cc,
            now,
            FRAME_US_120,
            2_400,
            |index| BASE_DELAY_US + (index as i64) * 800,
            0.0,
        );

        assert!(
            run.cut_times_us.len() >= 2,
            "twenty seconds of worsening delay should cut more than once at {round_trip_us} us"
        );

        for pair in run.cut_times_us.windows(2) {
            assert!(
                pair[1] - pair[0] >= config.cooldown_us(),
                "cuts at {} and {} are closer than the {} us cooldown",
                pair[0],
                pair[1],
                config.cooldown_us()
            );
        }
    }
}

#[test]
fn no_gradient_is_ever_measured_across_a_gap_in_the_feedback() {
    // The comparison that has to be suppressed is not the tick the silence ends on but the
    // one after it, which would otherwise measure a window from after the gap against one
    // from before it. Here the delay is 60 ms higher on the far side, so a gradient that
    // spans the gap is unmistakable in the statistics.
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let now = feed_clean(&mut cc, 0, FRAME_US_120, 600).end_us;

    let run = feed(
        &mut cc,
        now + 3_000_000,
        FRAME_US_120,
        600,
        |_| BASE_DELAY_US + 60_000,
        0.0,
    );

    assert!(
        run.max_abs_gradient_us < 60_000,
        "a gradient of {} us was measured across the gap",
        run.max_abs_gradient_us
    );
    assert!(
        !run.cut_for_delay,
        "a flat path on the far side of a gap is not congestion"
    );
}

#[test]
fn a_malformed_loss_figure_does_not_disable_the_loss_branch() {
    // A NaN reaching the running total makes every comparison against it false, so the
    // controller would quietly stop reacting to loss while continuing to look healthy. The
    // bad report is placed often enough that every decision window contains one alongside
    // good ones, which is the case that has to keep working: a window is not written off
    // because one of the reports in it was nonsense.
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let mut now = feed_clean(&mut cc, 0, FRAME_US_120, 1_200).end_us;
    let before = cc.target_bps();

    let real_loss = frame_loss_for(config.loss_decrease_floor * 8.0, config.packets_per_frame);
    let mut cut = false;
    for index in 0..240 {
        cc.observe(&DelaySample {
            one_way_delay_us: BASE_DELAY_US,
            observed_at_us: now,
            frame_loss: if index % 3 == 0 { f32::NAN } else { real_loss },
        });
        now += FRAME_US_120;
        cut |= cc.stats().last_action == RateAction::DecreaseLoss;
    }

    assert!(cut, "loss reported alongside NaNs must still cut the rate");
    assert!(cc.target_bps() < before);
}

#[test]
fn a_gap_in_the_feedback_is_not_a_trend() {
    // Feedback stops for three seconds, then resumes with the delay unchanged. The two ends
    // of that subtraction describe different eras, and treating the gap as a measurement
    // would let a stalled return path masquerade as congestion.
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    let now = feed_clean(&mut cc, 0, FRAME_US_120, 600).end_us;
    let before = cc.target_bps();
    let backoffs = cc.stats().backoffs;

    let resumed = feed(
        &mut cc,
        now + 3_000_000,
        FRAME_US_120,
        4,
        |_| BASE_DELAY_US + 40_000,
        0.0,
    )
    .end_us;

    assert_eq!(
        cc.stats().backoffs,
        backoffs,
        "a gap longer than the stale cutoff must not be read as a delay trend"
    );

    feed_clean(&mut cc, resumed, FRAME_US_120, 600);
    assert!(
        cc.target_bps() >= before,
        "the controller must carry on normally after the gap"
    );
}

#[test]
fn the_rate_never_leaves_its_bounds_under_abuse() {
    // Everything the feedback path could plausibly hand over, including things it should
    // not: delays at the extremes of the type, non-finite and out-of-range loss figures,
    // timestamps that jump forward and occasionally backwards.
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);
    let mut rng = Lcg(0x5eed_1234_abcd_ef01);
    let mut now = 1_000_000u64;

    for _ in 0..40_000 {
        let one_way_delay_us = match rng.below(16) {
            0 => i64::MIN,
            1 => i64::MAX,
            2 => 0,
            _ => rng.below(4_000_000_000) as i64 - 2_000_000_000,
        };
        let frame_loss = match rng.below(12) {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => -1.0,
            3 => 17.0,
            _ => rng.below(1_001) as f32 / 1000.0,
        };

        now = match rng.below(20) {
            0 => now.saturating_sub(rng.below(5_000_000)),
            1 => now.saturating_add(rng.below(30_000_000)),
            _ => now.saturating_add(rng.below(2 * FRAME_US_120)),
        };

        let target = cc.observe(&DelaySample {
            one_way_delay_us,
            observed_at_us: now,
            frame_loss,
        });

        assert_eq!(
            target,
            cc.target_bps(),
            "observe must report the live target"
        );
        assert!(
            (config.min_bps..=config.max_bps).contains(&target),
            "target {target} left the bounds {}..={}",
            config.min_bps,
            config.max_bps
        );
    }
}

#[test]
fn relentless_congestion_stops_at_the_floor() {
    let config = CongestionConfig::for_round_trip_us(480);
    let mut cc = CongestionController::new(config);

    feed(
        &mut cc,
        0,
        FRAME_US_120,
        12_000,
        |index| BASE_DELAY_US + (index as i64) * 800,
        1.0,
    );

    assert_eq!(
        cc.target_bps(),
        config.min_bps,
        "a hundred seconds of worsening delay and total loss must stop at the floor"
    );
}

#[test]
fn a_pinned_configuration_stays_pinned() {
    let config = CongestionConfig {
        min_bps: 8_000_000,
        max_bps: 8_000_000,
        start_bps: 8_000_000,
        ..CongestionConfig::for_round_trip_us(480)
    };
    let mut cc = CongestionController::new(config);

    let now = feed_clean(&mut cc, 0, FRAME_US_120, 600).end_us;
    assert_eq!(cc.target_bps(), 8_000_000);

    feed(
        &mut cc,
        now,
        FRAME_US_120,
        600,
        |index| BASE_DELAY_US + (index as i64) * 800,
        1.0,
    );
    assert_eq!(cc.target_bps(), 8_000_000);
}

#[test]
fn the_timing_parameters_scale_with_the_round_trip() {
    let lan = CongestionConfig::for_round_trip_us(480);
    let wan = CongestionConfig::for_round_trip_us(45_000);

    assert!(
        wan.tick_us() > lan.tick_us(),
        "a longer path must be given longer to answer"
    );
    assert!(
        wan.cooldown_us() > lan.cooldown_us(),
        "a longer path takes longer to drain after a cut"
    );
    assert!(
        wan.gradient_rise_us() > lan.gradient_rise_us(),
        "a longer path is noisier, so the bar for rising must be higher"
    );

    // Monotonic and bounded across every round trip a real path could report, so no
    // measurement can produce a tick of zero or a cooldown of a day.
    let mut previous = CongestionConfig::for_round_trip_us(0);
    for round_trip_us in (0..2_000_000).step_by(997) {
        let config = CongestionConfig::for_round_trip_us(round_trip_us);

        assert!(config.tick_us() >= previous.tick_us());
        assert!(config.cooldown_us() >= previous.cooldown_us());
        assert!(config.gradient_rise_us() >= previous.gradient_rise_us());
        assert!((20_000..=100_000).contains(&config.tick_us()));
        assert!((100_000..=1_000_000).contains(&config.cooldown_us()));
        assert!((1_000..=5_000).contains(&config.gradient_rise_us()));
        assert!(config.stale_us() > config.tick_us());

        previous = config;
    }
}

#[test]
fn retuning_the_round_trip_mid_session_takes_effect() {
    let mut cc = CongestionController::new(CongestionConfig::for_round_trip_us(480));
    let lan_tick = cc.config().tick_us();

    feed_clean(&mut cc, 0, FRAME_US_120, 600);
    cc.set_round_trip_us(45_000);

    assert!(cc.config().tick_us() > lan_tick);
    assert_eq!(cc.config().round_trip_us, 45_000);
}

#[test]
fn frame_loss_and_packet_loss_convert_both_ways() {
    for &packets_per_frame in &[1.0_f32, 2.0, 12.0, 35.0, 120.0] {
        for &packet_loss in &[0.0_f32, 0.0001, 0.0009, 0.005, 0.02, 0.1, 0.5] {
            let frame_loss = frame_loss_for(packet_loss, packets_per_frame);

            // Past this the frame loss is so close to one that a 32-bit float cannot hold
            // the difference, so nothing could invert it. That is not a range the
            // controller operates in: every frame is already gone.
            if frame_loss > 0.99 {
                continue;
            }

            let recovered = equivalent_packet_loss(frame_loss, packets_per_frame);
            assert!(
                (recovered - packet_loss).abs() < 1e-4,
                "{packet_loss} packet loss over {packets_per_frame} packets became \
                 {frame_loss} frame loss and came back as {recovered}"
            );
        }
    }

    // The survey's example, stated the way it was reported: a link losing under a tenth of
    // a percent of packets loses several percent of frames.
    assert!(equivalent_packet_loss(0.03, 35.0) < 0.001);

    // Nonsense in, no loss out, rather than a NaN that would disable the loss branch.
    assert_eq!(equivalent_packet_loss(f32::NAN, 35.0), 0.0);
    assert_eq!(equivalent_packet_loss(-1.0, 35.0), 0.0);
    assert_eq!(equivalent_packet_loss(2.0, 35.0), 1.0);
}

#[test]
#[should_panic(expected = "maximum bitrate must not be below the minimum")]
fn an_inverted_bitrate_range_is_refused() {
    let _ = CongestionController::new(CongestionConfig {
        min_bps: 20_000_000,
        max_bps: 10_000_000,
        start_bps: 10_000_000,
        ..CongestionConfig::default()
    });
}

#[test]
#[should_panic(expected = "the decrease factor must shrink the rate")]
fn a_decrease_factor_that_does_not_decrease_is_refused() {
    let _ = CongestionController::new(CongestionConfig {
        decrease_factor: 1.0,
        ..CongestionConfig::default()
    });
}

/// Frames in the shared scenario used by the clock-error tests.
const SCENARIO_FRAMES: usize = 2_400;

/// Returns the one-way delay of frame `index` in the shared scenario, in microseconds.
///
/// Twenty seconds at 120 fps covering every branch the controller has: a clean climb
/// peppered with isolated stalls, a queue building and draining, and a clean tail.
fn scenario_delay_us(index: usize) -> i64 {
    match index {
        0..600 => {
            if index % 120 == 60 {
                BASE_DELAY_US + STALL_US
            } else {
                BASE_DELAY_US
            }
        }
        600..900 => BASE_DELAY_US + (index - 600) as i64 * 800,
        900..1100 => BASE_DELAY_US + 300 * 800,
        1100..1300 => BASE_DELAY_US + (1300 - index) as i64 * 800,
        _ => BASE_DELAY_US,
    }
}

/// Returns the frame loss reported for frame `index` in the shared scenario.
fn scenario_loss(index: usize) -> f32 {
    if (1300..1600).contains(&index) {
        frame_loss_for(0.01, 35.0)
    } else {
        0.0
    }
}

/// Runs the shared scenario with every delay shifted by `offset_us`.
///
/// Returns the rate chosen after each of the 2400 observations along with the final
/// statistics, so a comparison covers not just where the controller ended up but every
/// step it took and the internal counters behind them.
fn trace_of(offset_us: i64) -> (Vec<u32>, CongestionStats) {
    let mut cc = CongestionController::new(CongestionConfig::for_round_trip_us(480));
    let mut rates = Vec::with_capacity(SCENARIO_FRAMES);
    let mut now = 0u64;

    for index in 0..SCENARIO_FRAMES {
        rates.push(cc.observe(&DelaySample {
            one_way_delay_us: scenario_delay_us(index) + offset_us,
            observed_at_us: now,
            frame_loss: scenario_loss(index),
        }));
        now += FRAME_US_120;
    }

    (rates, cc.stats())
}
