//! Deciding when to show a decoded picture.
//!
//! Showing each picture the moment it decodes gives the lowest possible latency and the
//! worst possible smoothness: frames arrive unevenly, so they appear unevenly, and the
//! eye reads that as stutter even when no frame was lost. Holding a fixed two frames
//! smooths it completely and costs a fixed two frames of delay.
//!
//! Neither is right. The delay this pipeline can afford is exactly the spread of arrival
//! times and no more, so the pacer measures that spread and holds each picture only long
//! enough to cover it. On a clean path the spread is near zero and so is the delay.
//!
//! The measurement is the picture's **age** — how long ago it was captured, in this
//! machine's clock — which is available because the clock offset is known. Ages that
//! cluster tightly mean a steady path and allow a short delay; ages that scatter mean the
//! delay has to cover the late ones or they arrive after their turn.

use core::time::Duration;

use crate::stats::LatencyRecorder;

/// How many pictures to observe before recomputing the target delay.
///
/// The target comes from a percentile, which means sorting, so it is recomputed about
/// twice a second rather than once a frame.
const RECOMPUTE_EVERY: u32 = 30;

/// Fraction of the gap the delay closes per recomputation when it is shrinking.
///
/// Delay rises immediately when the path gets worse and comes down geometrically when it
/// recovers. A buffer that collapses the moment conditions improve spends its life
/// oscillating, and every oscillation downward shows up as a repeated or dropped frame —
/// but one that creeps down in fixed steps takes minutes to give back delay the path no
/// longer needs.
const SHRINK_DIVISOR: u32 = 4;

/// Smallest amount the delay may shrink per recomputation, in microseconds.
///
/// Without a floor the geometric decay never quite arrives.
const SHRINK_FLOOR_US: u32 = 200;

/// How many recent ages to consider when choosing the delay.
///
/// About two seconds at sixty frames per second: long enough that one unlucky frame does
/// not move the target, short enough that the pacer is describing the path as it is now.
const WINDOW: usize = 128;

/// Decides how long each decoded picture should be held before it is shown.
///
/// # Examples
///
/// ```
/// # use prism_core::render::pacing::PresentPacer;
/// let mut pacer = PresentPacer::new(30_000);
///
/// // A path that delivers every picture at the same age needs no smoothing at all.
/// for _ in 0..200 {
///     pacer.hold_for(5_000);
/// }
/// assert_eq!(pacer.hold_for(5_000), std::time::Duration::ZERO);
/// ```
#[derive(Debug)]
pub struct PresentPacer {
    ages: LatencyRecorder,
    delay_us: u32,
    max_delay_us: u32,
    since_recompute: u32,
    has_target: bool,
    shown_late: u64,
    total: u64,
    held: LatencyRecorder,
}

impl PresentPacer {
    /// Creates a pacer that will never hold a picture longer than `max_delay_us`.
    ///
    /// The ceiling matters because the delay is derived from measurements, and a path
    /// that briefly falls apart would otherwise push it somewhere the session never
    /// recovers from. Beyond the ceiling the right answer is to drop frames, not to add
    /// delay nobody asked for.
    #[must_use]
    pub fn new(max_delay_us: u32) -> Self {
        Self {
            ages: LatencyRecorder::new(WINDOW),
            delay_us: 0,
            max_delay_us,
            since_recompute: 0,
            has_target: false,
            shown_late: 0,
            total: 0,
            held: LatencyRecorder::new(WINDOW),
        }
    }

    /// Records a picture's age and returns how long to hold it before showing it.
    ///
    /// A picture already older than the target delay is shown immediately and counted as
    /// late; there is nothing to gain by holding a frame that is behind already.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::render::pacing::PresentPacer;
    /// # use std::time::Duration;
    /// let mut pacer = PresentPacer::new(30_000);
    ///
    /// // Establish a target from a jittery path.
    /// for age in [4_000, 12_000, 5_000, 11_000, 4_500, 13_000] {
    ///     for _ in 0..40 {
    ///         pacer.hold_for(age);
    ///     }
    /// }
    ///
    /// // An early picture waits for the late ones; a late one does not wait at all.
    /// assert!(pacer.hold_for(4_000) > Duration::ZERO);
    /// assert_eq!(pacer.hold_for(20_000), Duration::ZERO);
    /// ```
    pub fn hold_for(&mut self, age_us: u32) -> Duration {
        self.ages.record(age_us);
        self.total += 1;
        self.since_recompute += 1;

        if self.since_recompute >= RECOMPUTE_EVERY {
            self.since_recompute = 0;
            self.recompute();
        }

        if self.has_target && age_us > self.delay_us {
            self.shown_late += 1;
        }

        let hold = self.delay_us.saturating_sub(age_us);
        self.held.record(hold);
        Duration::from_micros(u64::from(hold))
    }

    /// Returns the delay the pacer is currently targeting, in microseconds.
    #[must_use]
    pub fn delay_us(&self) -> u32 {
        self.delay_us
    }

    /// Returns whether the pacer has any room to hold pictures at all.
    ///
    /// A pacer built with a ceiling of zero never delays anything, and its late count is
    /// then meaningless — every picture is trivially later than a target of nothing.
    /// Reports should say the pacer is off rather than quote that number.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.max_delay_us > 0
    }

    /// Returns how many pictures arrived too late to be held.
    ///
    /// A rate near zero means the delay is covering the path's spread. A high rate means
    /// the path is worse than the ceiling allows the pacer to absorb.
    #[must_use]
    pub fn shown_late(&self) -> u64 {
        self.shown_late
    }

    /// Returns how many pictures the pacer has seen.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Returns the distribution of how long pictures were actually held.
    ///
    /// This is the latency the pacer added, which is the price paid for smoothness and
    /// belongs next to the end-to-end figure rather than hidden inside it.
    pub fn held_summary(&mut self) -> Option<crate::stats::LatencySummary> {
        self.held.summarize()
    }

    /// Recomputes the target delay from the ages seen recently.
    ///
    /// The target is the 99th percentile age, so all but the latest one per cent of
    /// pictures are ready before their turn. It rises to a worse path immediately and
    /// falls back towards a better one in steps.
    fn recompute(&mut self) {
        let Some(summary) = self.ages.summarize() else {
            return;
        };

        let target = summary.p99_us.min(self.max_delay_us);

        self.delay_us = if target > self.delay_us {
            target
        } else {
            let gap = self.delay_us - target;
            self.delay_us - gap.min(SHRINK_FLOOR_US.max(gap / SHRINK_DIVISOR))
        };

        self.has_target = true;
    }
}
