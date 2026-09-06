//! Estimating the offset between two machines' clocks.
//!
//! Both sides stamp frames against the Unix epoch, which only makes the numbers
//! comparable on one machine. Across machines the offset is unknown, and without
//! correcting for it a latency figure is not merely imprecise but meaningless — a client
//! whose clock trails the host's measures negative latency, which an earlier version of
//! this project silently reported as zero.
//!
//! The exchange is Cristian's algorithm, and its one assumption is that the two legs of
//! the round trip took equally long. That assumption is wrong in general and least wrong
//! when the round trip was fast, so the sample with the smallest round trip is kept and
//! the rest discarded.

use crate::net::packet::ClockPong;

/// One usable measurement of the offset between the two clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    /// How long the exchange took, excluding the host's own handling time.
    pub round_trip_us: u64,
    /// How far the host's clock is ahead of this one. Negative means it is behind.
    pub offset_us: i64,
}

/// Tracks the best offset estimate seen so far.
///
/// # Examples
///
/// ```
/// # use prism_core::net::clocksync::ClockSync;
/// # use prism_core::net::packet::ClockPong;
/// let mut sync = ClockSync::new();
/// assert!(sync.offset_us().is_none());
///
/// // The host's clock is a full second ahead, and each leg took 500 us.
/// sync.observe(&ClockPong { t1_us: 1_000_000, t2_us: 2_000_500, t3_us: 2_000_600 }, 1_001_100);
///
/// assert_eq!(sync.offset_us(), Some(1_000_000));
/// assert_eq!(sync.round_trip_us(), Some(1_000));
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ClockSync {
    best: Option<ClockSample>,
    accepted: u32,
    rejected: u32,
}

impl ClockSync {
    /// Creates an estimator with no samples yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one answer into the estimate and returns the sample it produced.
    ///
    /// Returns `None` for an exchange that cannot have happened — the reply arriving
    /// before the ping was sent, or the host answering before it received — which is what
    /// a clock stepping mid-exchange looks like. Such a sample is counted as rejected
    /// rather than allowed to poison the estimate.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::clocksync::ClockSync;
    /// # use prism_core::net::packet::ClockPong;
    /// let mut sync = ClockSync::new();
    /// // The reply claims to predate the ping.
    /// assert!(sync.observe(&ClockPong { t1_us: 100, t2_us: 0, t3_us: 0 }, 50).is_none());
    /// assert_eq!(sync.rejected(), 1);
    /// ```
    pub fn observe(&mut self, pong: &ClockPong, t4_us: u64) -> Option<ClockSample> {
        let t1 = i128::from(pong.t1_us);
        let t2 = i128::from(pong.t2_us);
        let t3 = i128::from(pong.t3_us);
        let t4 = i128::from(t4_us);

        let handling = t3 - t2;
        let elapsed = t4 - t1;
        let round_trip = elapsed - handling;

        if handling < 0 || elapsed < 0 || round_trip < 0 {
            self.rejected += 1;
            return None;
        }

        let sample = ClockSample {
            round_trip_us: round_trip as u64,
            offset_us: (((t2 - t1) + (t3 - t4)) / 2) as i64,
        };

        self.accepted += 1;
        if self
            .best
            .is_none_or(|best| sample.round_trip_us < best.round_trip_us)
        {
            self.best = Some(sample);
        }

        Some(sample)
    }

    /// Returns how far the host's clock is ahead of this one, once a sample exists.
    #[must_use]
    pub fn offset_us(&self) -> Option<i64> {
        self.best.map(|sample| sample.offset_us)
    }

    /// Returns the round trip of the sample the estimate rests on.
    ///
    /// Worth reporting alongside the offset: the estimate is only as trustworthy as this
    /// number is small, because a slow exchange had more room to be delayed unevenly.
    #[must_use]
    pub fn round_trip_us(&self) -> Option<u64> {
        self.best.map(|sample| sample.round_trip_us)
    }

    /// Returns how many exchanges produced a usable sample.
    #[must_use]
    pub fn accepted(&self) -> u32 {
        self.accepted
    }

    /// Returns how many exchanges were impossible and discarded.
    #[must_use]
    pub fn rejected(&self) -> u32 {
        self.rejected
    }

    /// Converts a host timestamp into this machine's clock.
    ///
    /// Returns `None` before any sample has been taken, and for a timestamp that maps to
    /// before the epoch, which would mean the offset is wildly wrong.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::clocksync::ClockSync;
    /// # use prism_core::net::packet::ClockPong;
    /// let mut sync = ClockSync::new();
    /// sync.observe(&ClockPong { t1_us: 1_000, t2_us: 6_000, t3_us: 6_000 }, 2_000);
    ///
    /// // The host runs 4500 us ahead, so one of its stamps maps back by that much.
    /// assert_eq!(sync.offset_us(), Some(4_500));
    /// assert_eq!(sync.to_local_us(10_000), Some(5_500));
    /// ```
    #[must_use]
    pub fn to_local_us(&self, host_ts_us: u64) -> Option<u64> {
        let offset = self.offset_us()?;
        let local = i128::from(host_ts_us) - i128::from(offset);

        (local >= 0).then_some(local as u64)
    }
}
