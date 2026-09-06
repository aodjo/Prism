//! Latency measurement.
//!
//! Every milestone in this project is judged by a p99 number, so measurement is part of
//! the pipeline rather than something bolted on afterwards. Recording a sample happens
//! on the frame path and must stay allocation-free and constant time; summarising is
//! driven by the stats reporter at 10 Hz, where a sort of a few hundred values costs
//! nothing.
//!
//! Samples are microseconds. A `u32` holds just over 71 minutes, which is far beyond any
//! latency worth recording — anything near the ceiling is a bug rather than a
//! measurement.

/// Fixed-size window of latency samples.
///
/// Keeps the most recent `capacity` samples and discards older ones, so the summary
/// always describes recent behaviour rather than the whole session. That matters for a
/// live HUD: a burst of jitter two minutes ago should not still be shaping the p99 on
/// screen.
///
/// # Examples
///
/// ```
/// # use prism_core::stats::LatencyRecorder;
/// let mut recorder = LatencyRecorder::new(128);
/// for sample in [10_000, 12_000, 11_000, 40_000] {
///     recorder.record(sample);
/// }
///
/// let summary = recorder.summarize().unwrap();
/// assert_eq!(summary.count, 4);
/// assert_eq!(summary.min_us, 10_000);
/// assert_eq!(summary.max_us, 40_000);
/// assert_eq!(summary.p50_us, 11_000);
/// ```
#[derive(Debug)]
pub struct LatencyRecorder {
    samples: Box<[u32]>,
    len: usize,
    next: usize,
    scratch: Vec<u32>,
}

/// Statistics over the samples currently held by a [`LatencyRecorder`].
///
/// Percentiles use the nearest-rank definition: the p99 of one hundred samples is the
/// ninety-ninth smallest. No interpolation, so every reported value is a sample that
/// actually occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySummary {
    /// How many samples the summary covers.
    pub count: usize,
    /// Smallest sample, in microseconds.
    pub min_us: u32,
    /// Largest sample, in microseconds.
    pub max_us: u32,
    /// Arithmetic mean, in microseconds, rounded down.
    pub mean_us: u32,
    /// Median, in microseconds.
    pub p50_us: u32,
    /// 95th percentile, in microseconds.
    pub p95_us: u32,
    /// 99th percentile, in microseconds. This is the number the milestones are judged on.
    pub p99_us: u32,
}

impl LatencyRecorder {
    /// Creates a recorder holding the most recent `capacity` samples.
    ///
    /// At 120 fps a capacity of 1024 covers roughly eight and a half seconds, which is
    /// long enough for a p99 to mean something and short enough to react to a change.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::stats::LatencyRecorder;
    /// let recorder = LatencyRecorder::new(1024);
    /// assert!(recorder.is_empty());
    /// ```
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "latency recorder capacity must be at least one sample"
        );

        Self {
            samples: vec![0; capacity].into_boxed_slice(),
            len: 0,
            next: 0,
            scratch: Vec::with_capacity(capacity),
        }
    }

    /// Records one sample, overwriting the oldest once the window is full.
    ///
    /// Constant time and allocation-free, because this runs once per frame on the
    /// receive path.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::stats::LatencyRecorder;
    /// let mut recorder = LatencyRecorder::new(2);
    /// recorder.record(1);
    /// recorder.record(2);
    /// recorder.record(3);
    /// assert_eq!(recorder.len(), 2);
    /// assert_eq!(recorder.summarize().unwrap().min_us, 2);
    /// ```
    pub fn record(&mut self, sample_us: u32) {
        self.samples[self.next] = sample_us;
        self.next = (self.next + 1) % self.samples.len();
        self.len = (self.len + 1).min(self.samples.len());
    }

    /// Returns how many samples the window currently holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no samples have been recorded since the last clear.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the window's capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.samples.len()
    }

    /// Discards every sample without releasing the window's memory.
    pub fn clear(&mut self) {
        self.len = 0;
        self.next = 0;
    }

    /// Summarises the samples currently held, or `None` if there are none.
    ///
    /// Sorts a scratch copy, so this is the expensive half of the type and belongs on
    /// the reporting path rather than the frame path. The scratch buffer is retained
    /// between calls, so repeated summarising does not allocate.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::stats::LatencyRecorder;
    /// let mut recorder = LatencyRecorder::new(100);
    /// assert!(recorder.summarize().is_none());
    ///
    /// for sample in 1..=100 {
    ///     recorder.record(sample);
    /// }
    /// let summary = recorder.summarize().unwrap();
    /// assert_eq!(summary.p50_us, 50);
    /// assert_eq!(summary.p99_us, 99);
    /// ```
    pub fn summarize(&mut self) -> Option<LatencySummary> {
        if self.len == 0 {
            return None;
        }

        self.scratch.clear();
        self.scratch.extend_from_slice(&self.samples[..self.len]);
        self.scratch.sort_unstable();

        let sorted = &self.scratch;
        let total: u64 = sorted.iter().map(|&s| u64::from(s)).sum();

        Some(LatencySummary {
            count: self.len,
            min_us: sorted[0],
            max_us: sorted[self.len - 1],
            mean_us: (total / self.len as u64) as u32,
            p50_us: nearest_rank(sorted, 0.50),
            p95_us: nearest_rank(sorted, 0.95),
            p99_us: nearest_rank(sorted, 0.99),
        })
    }
}

/// Returns the nearest-rank percentile of an ascending slice.
///
/// The rank is `ceil(fraction * n)`, clamped to the slice, so p0 is the smallest sample
/// and p100 the largest. No interpolation: every value returned is one that was actually
/// measured.
///
/// # Panics
///
/// Panics if `sorted` is empty.
fn nearest_rank(sorted: &[u32], fraction: f64) -> u32 {
    debug_assert!(!sorted.is_empty(), "percentiles need at least one sample");

    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}
