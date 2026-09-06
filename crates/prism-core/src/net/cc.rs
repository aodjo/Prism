//! Congestion control: turning delay and loss observations into a target bitrate.
//!
//! The plan's rule is that latency beats image quality — in a game a softer picture is
//! always cheaper than a queue — so this is an AIMD controller driven primarily by the
//! *gradient* of one-way delay, with frame loss as a backstop for paths that drop without
//! ever queueing (a policer, or a wireless link).
//!
//! It is a pure state machine. It owns no socket, no thread and no clock: every
//! observation carries its own timestamp, so a test can replay an hour of a session in
//! microseconds and get exactly the decisions the live controller would have made.
//! Folding one observation is constant time and allocates nothing, which is what lets it
//! sit on the feedback path at frame rate.
//!
//! # Why the gradient, and why a clock offset error cancels
//!
//! The measured one-way delay of frame `i` is
//!
//! ```text
//! d_i = (arrival_i - capture_i) + e_i
//! ```
//!
//! where `capture_i` is the host's stamp already corrected by the *estimated* clock
//! offset and `e_i` is the error left in that estimate. `e_i` is not a rounding detail:
//! the cross-machine run recorded in `docs/plan.md` measured an offset of -818.76 ms, so
//! an error of a few per cent of it dwarfs every latency this project cares about. An
//! absolute-delay threshold is therefore not merely imprecise here, it is meaningless.
//!
//! Each tick the controller compares the minimum `d` over the current window against the
//! minimum over the previous one:
//!
//! ```text
//! g = (min_k true + e_k) - (min_{k-1} true + e_{k-1})
//! ```
//!
//! If the offset error is the same across two adjacent windows it cancels **exactly**,
//! whatever its magnitude and sign. Only *change* in the error survives the subtraction.
//! Two free-running crystals drift by at most a few hundred parts per million relative to
//! each other; across the longest tick this controller uses (100 ms) that is under 30 µs,
//! two orders of magnitude below the smallest gradient it will act on (1 ms). Drift is
//! not merely small here, it is unreachable. That is the whole reason the design survives
//! a long session on an offset estimated once at the start.
//!
//! The direct consequence is that [`DelaySample::one_way_delay_us`] is **signed**. With a
//! large offset error the measured delay is routinely negative, and clamping it at zero —
//! which the CLI's display path deliberately does, because a negative age is nonsense to
//! show a user — would destroy the very quantity the gradient is taken of.
//!
//! # Loss arrives in frames, not packets
//!
//! The client acknowledges frames with a bitmap of the last thirty-two, so the loss
//! figure reaching this module is a fraction of **frames**. A frame at this project's
//! measured sizes is about thirty-five packets (40 KB over a 1180-byte payload), and a
//! frame is lost if any one of its packets is:
//!
//! ```text
//! frame_loss = 1 - (1 - packet_loss)^packets_per_frame
//! ```
//!
//! At thirty-five packets per frame, 0.09% packet loss reads as 3% frame loss — a factor
//! of thirty-five. Every loss threshold in the congestion control literature (Google
//! Congestion Control's 2% and 10%, for example) is written in packet terms, so applying
//! one of them directly to a frame-loss figure makes the controller more than an order of
//! magnitude twitchier than its author intended.
//!
//! This module converts the *observation* into the packet domain rather than pushing the
//! *thresholds* into the frame domain, because that is the direction in which the numbers
//! stay meaningful: 10% packet loss is 97.5% frame loss at thirty-five packets per frame,
//! a threshold that would never fire. See [`equivalent_packet_loss`]. Packets per frame is
//! configuration rather than a constant, because a 1440p key frame and a static desktop
//! differ by an order of magnitude.
//!
//! The defaults are calibrated against the granularity the feedback actually has. With
//! thirty-five packets per frame, the increase ceiling of 0.15% packet loss is 5.1% frame
//! loss and the back-off floor of 0.5% is 16.1%: one frame missing from a thirty-two frame
//! bitmap (3.1%) does not stall the climb, two (6.3%) hold the rate steady, and six
//! (18.8%) cut it.
//!
//! # Tuning against a path, not against this LAN
//!
//! Every time-based parameter is derived from a round trip the caller measures (the
//! min-RTT sample from clock sync is exactly the right input) rather than fixed, because
//! parameters that suit a 0.48 ms LAN oscillate against a 30-50 ms internet path.
//!
//! | Parameter | Derivation | LAN (0.48 ms) | WAN (40 ms) |
//! |---|---|---|---|
//! | tick | `2 x rtt`, clamped to 20-100 ms | 20 ms | 80 ms |
//! | back-off cooldown | `8 x rtt`, clamped to 0.1-1 s | 100 ms | 320 ms |
//! | gradient threshold | `rtt / 8`, clamped to 1-5 ms | 1 ms | 5 ms |
//!
//! The tick is two round trips because a controller cannot see the effect of its own
//! change sooner than one, and acting faster than that is how oscillation starts. Its
//! 20 ms floor exists because `2 x 0.48 ms` would hold at most one frame even at 120 fps,
//! leaving the minimum filter nothing to filter.
//!
//! The cooldown is longer because after a cut the bottleneck queue still has to drain
//! before the delay signal reflects it. Inside it the trend counter is frozen rather than
//! merely ignored, so delay still rising is understood as the congestion already answered
//! and not as a fresh event. Without that, one episode is counted many times over, which
//! is exactly how a controller ratchets itself to the floor and stays there.
//!
//! The gradient threshold has to sit above the path's own jitter. Video arrival on this
//! LAN measured p50 1.41 ms and p99 2.82 ms, so roughly a millisecond of window-to-window
//! movement in the minimum is noise here; on a 40 ms path it is not.
//!
//! # Isolated outliers are not a trend
//!
//! `docs/plan.md` records an unexplained periodic ~58 ms stall on this machine, believed
//! to be Apple Silicon media-engine contention between an encoder and a decoder sharing
//! one box. It is one frame, it repeats, and to a controller that averages delay it looks
//! exactly like congestion. Two things stop it here:
//!
//! - The per-window statistic is the **minimum**, not the mean or the median. Queueing
//!   only ever adds to delay, so the smallest sample in a window is the least-queued one.
//!   A window holding one 58 ms frame and one clean frame is indistinguishable from a
//!   window of two clean frames.
//! - A back-off needs [`CongestionConfig::rising_ticks_to_back_off`] net-rising ticks. The
//!   counter rises on a rising tick and falls on every other kind, so a spike large enough
//!   to own an entire window costs one tick up and gets it back on the next.

/// Shortest tick the controller will use, in microseconds.
///
/// Below this a window holds at most one frame even at 120 fps, and a one-sample window
/// gives the minimum filter nothing to filter out.
const MIN_TICK_US: u64 = 20_000;

/// Longest tick the controller will use, in microseconds.
///
/// Bounds how long a real congestion episode can go unanswered, whatever the round trip.
const MAX_TICK_US: u64 = 100_000;

/// Tick length as a multiple of the measured round trip.
const TICK_ROUND_TRIPS: u64 = 2;

/// Shortest post-back-off cooldown, in microseconds.
const MIN_COOLDOWN_US: u64 = 100_000;

/// Longest post-back-off cooldown, in microseconds.
const MAX_COOLDOWN_US: u64 = 1_000_000;

/// Cooldown length as a multiple of the measured round trip.
const COOLDOWN_ROUND_TRIPS: u64 = 8;

/// Smallest delay rise, in microseconds, that counts as rising rather than as jitter.
const MIN_GRADIENT_RISE_US: i64 = 1_000;

/// Largest delay rise threshold, in microseconds.
///
/// Past this the controller would be ignoring queue growth that already costs a fifth of
/// the whole 25 ms latency budget.
const MAX_GRADIENT_RISE_US: i64 = 5_000;

/// Divisor applied to the round trip to get the gradient threshold.
const GRADIENT_RISE_DIVISOR: u64 = 8;

/// A window longer than this many ticks is a gap, not a measurement.
const STALE_TICKS: u64 = 8;

/// Floor on the stale cutoff, in microseconds.
///
/// Without it a stream feeding the controller slowly — ten frames a second, say — would
/// look permanently stale and the controller would never act at all.
const STALE_FLOOR_US: u64 = 500_000;

/// One observation of how a single frame fared on its way to the client.
///
/// Produced on the host from a feedback report: the delay comes from the difference
/// between the frame's capture stamp and its acknowledgement, and the loss figure from the
/// report's received-frames bitmap.
///
/// # Examples
///
/// ```
/// # use prism_core::net::cc::DelaySample;
/// // Two frames missing from a thirty-two frame acknowledgement bitmap.
/// let sample = DelaySample {
///     one_way_delay_us: 1_400,
///     observed_at_us: 4_000_000,
///     frame_loss: 2.0 / 32.0,
/// };
/// assert_eq!(sample.one_way_delay_us, 1_400);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelaySample {
    /// One-way delay for this frame, in microseconds.
    ///
    /// Signed, and deliberately so. Any error left in the clock offset estimate shifts
    /// every reading by the same amount, and that amount can be large and negative — this
    /// project has measured an offset of -818.76 ms between two machines. The controller
    /// only ever differentiates this value, so a constant shift cancels; clamping it at
    /// zero would throw away the difference the controller runs on.
    pub one_way_delay_us: i64,

    /// When the observation was made, in microseconds on the observer's own clock.
    ///
    /// Only differences between samples matter, so the epoch is irrelevant as long as it
    /// is the same one throughout a session. Samples are expected in non-decreasing order;
    /// one that arrives out of order is folded into the current window rather than
    /// rewinding it.
    pub observed_at_us: u64,

    /// Fraction of recent frames that did not arrive, from 0.0 to 1.0.
    ///
    /// **Frames, not packets.** See the module documentation: a threshold taken from the
    /// congestion control literature and applied to this number directly is wrong by more
    /// than an order of magnitude. Values outside the range, and non-finite values, are
    /// treated as zero rather than allowed to poison the average.
    pub frame_loss: f32,
}

/// What the controller did at the most recent tick.
///
/// Reported so the stats surface can say *why* the rate moved. A user watching quality
/// drop wants to know whether the path is queueing or dropping, and those two call for
/// different remedies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateAction {
    /// The rate was left alone: no trend, or still inside the post-back-off cooldown.
    #[default]
    Hold,
    /// The rate was raised by one additive step.
    Increase,
    /// The rate was cut because one-way delay has been rising for several ticks.
    DecreaseDelay,
    /// The rate was cut because loss crossed the back-off threshold.
    DecreaseLoss,
}

/// Everything the controller's behaviour depends on.
///
/// The time-based parameters are not stored but derived from [`Self::round_trip_us`], so
/// updating the round trip as the clock sync estimate improves cannot leave them
/// inconsistent with each other.
///
/// # Examples
///
/// ```
/// # use prism_core::net::cc::CongestionConfig;
/// let lan = CongestionConfig::for_round_trip_us(480);
/// assert_eq!(lan.tick_us(), 20_000, "the floor, not 2 x 0.48 ms");
/// assert_eq!(lan.gradient_rise_us(), 1_000);
///
/// let wan = CongestionConfig::for_round_trip_us(40_000);
/// assert_eq!(wan.tick_us(), 80_000);
/// assert_eq!(wan.cooldown_us(), 320_000);
/// assert_eq!(wan.gradient_rise_us(), 5_000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CongestionConfig {
    /// Measured round trip in microseconds, which every time-based parameter scales with.
    ///
    /// The min-RTT sample from clock sync is the right input: it is the round trip least
    /// contaminated by queueing, which is what the controller wants as its baseline.
    pub round_trip_us: u64,

    /// Lowest rate the controller may choose, in bits per second.
    ///
    /// Below this the picture is worse than no picture, so there is nothing left to trade.
    pub min_bps: u32,

    /// Highest rate the controller may choose, in bits per second.
    ///
    /// The default leaves headroom above the 41.1 Mbps this project measured at 1080p120,
    /// because the stated target is 1440p120.
    pub max_bps: u32,

    /// Rate the controller starts at, in bits per second.
    pub start_bps: u32,

    /// How many packets an average frame is split into.
    ///
    /// Converts the frame-loss observation into the packet domain the thresholds are
    /// written in. The default of thirty-five comes from this project's measured 40 KB
    /// frames over a 1180-byte video payload.
    pub packets_per_frame: f32,

    /// Packet loss below which the rate may still climb.
    pub loss_increase_ceiling: f32,

    /// Packet loss above which the rate is cut.
    pub loss_decrease_floor: f32,

    /// How much the rate rises per tick when the path looks clean, in bits per second.
    pub increase_step_bps: u32,

    /// Factor the rate is multiplied by when backing off, between 0 and 1 exclusive.
    ///
    /// The default of 0.7 is deliberately harsher than the 0.85 of loss-based TCP-style
    /// controllers, because the plan puts latency ahead of image quality: over-cutting
    /// costs a few frames of softness and is undone within a second, while under-cutting
    /// leaves a queue standing in front of every frame.
    pub decrease_factor: f32,

    /// How many net-rising ticks a delay trend needs before it is believed.
    ///
    /// The counter rises on a rising tick and falls on any other, so this is what makes a
    /// single outlier — the ~58 ms stall in `docs/plan.md`, for one — cost nothing.
    pub rising_ticks_to_back_off: u32,
}

impl Default for CongestionConfig {
    /// Returns the defaults, assuming a path whose round trip is not yet known.
    ///
    /// Twenty milliseconds is assumed rather than the measured LAN figure, because
    /// mistaking an internet path for a LAN produces oscillation while the reverse only
    /// produces sluggishness. Callers should feed the real round trip in through
    /// [`CongestionController::set_round_trip_us`] as soon as clock sync produces one.
    fn default() -> Self {
        Self {
            round_trip_us: 20_000,
            min_bps: 1_000_000,
            max_bps: 60_000_000,
            start_bps: 10_000_000,
            packets_per_frame: 35.0,
            loss_increase_ceiling: 0.0015,
            loss_decrease_floor: 0.005,
            increase_step_bps: 250_000,
            decrease_factor: 0.7,
            rising_ticks_to_back_off: 3,
        }
    }
}

impl CongestionConfig {
    /// Returns the defaults tuned for a path with the given round trip.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::cc::CongestionConfig;
    /// assert_eq!(CongestionConfig::for_round_trip_us(480).round_trip_us, 480);
    /// ```
    #[must_use]
    pub fn for_round_trip_us(round_trip_us: u64) -> Self {
        Self {
            round_trip_us,
            ..Self::default()
        }
    }

    /// Returns how long a decision window lasts, in microseconds.
    ///
    /// Two round trips, floored at 20 ms and capped at 100 ms. See the module
    /// documentation for where those bounds come from.
    #[must_use]
    pub fn tick_us(&self) -> u64 {
        self.round_trip_us
            .saturating_mul(TICK_ROUND_TRIPS)
            .clamp(MIN_TICK_US, MAX_TICK_US)
    }

    /// Returns how long the controller holds still after a back-off, in microseconds.
    ///
    /// Eight round trips, floored at 100 ms and capped at one second. Long enough for the
    /// bottleneck queue to drain, so the same congestion event is not counted twice.
    #[must_use]
    pub fn cooldown_us(&self) -> u64 {
        self.round_trip_us
            .saturating_mul(COOLDOWN_ROUND_TRIPS)
            .clamp(MIN_COOLDOWN_US, MAX_COOLDOWN_US)
    }

    /// Returns the delay rise, in microseconds, that counts as rising rather than jitter.
    ///
    /// An eighth of the round trip, floored at 1 ms and capped at 5 ms.
    #[must_use]
    pub fn gradient_rise_us(&self) -> i64 {
        let scaled = (self.round_trip_us / GRADIENT_RISE_DIVISOR).min(i64::MAX as u64) as i64;

        scaled.clamp(MIN_GRADIENT_RISE_US, MAX_GRADIENT_RISE_US)
    }

    /// Returns the window length past which a gradient is meaningless, in microseconds.
    ///
    /// A window this long means the feedback stopped rather than that the path slowed
    /// down. Such a window ends the comparison in both directions: it produces no decision
    /// of its own, and it leaves no baseline behind, so the window after the silence is not
    /// measured against one from before it.
    #[must_use]
    pub fn stale_us(&self) -> u64 {
        self.tick_us()
            .saturating_mul(STALE_TICKS)
            .max(STALE_FLOOR_US)
    }
}

/// A snapshot of the controller, for the stats surface.
///
/// Cheap to take and free of anything the frame path would miss, so the 10 Hz reporter can
/// pull one whenever it likes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CongestionStats {
    /// The rate the encoder should be running at, in bits per second.
    pub target_bps: u32,
    /// Change in the minimum one-way delay across the last two windows, in microseconds.
    pub last_gradient_us: i64,
    /// Loss at the last tick, converted into the packet domain.
    pub packet_loss: f32,
    /// How many net-rising ticks have accumulated toward a delay back-off.
    pub rising_ticks: u32,
    /// Windows closed since the controller started.
    pub ticks: u64,
    /// Ticks that raised the rate.
    pub increases: u64,
    /// Ticks that cut the rate.
    pub backoffs: u64,
    /// Whether the controller is still holding still after a back-off.
    pub cooling_down: bool,
    /// What the last closed window decided.
    pub last_action: RateAction,
}

/// Chooses a target bitrate from one-way delay and frame loss.
///
/// Observations are folded in as they arrive and decisions are taken once per tick, so the
/// controller reacts on a schedule set by the path rather than by the frame rate.
///
/// # Examples
///
/// ```
/// # use prism_core::net::cc::{CongestionConfig, CongestionController, DelaySample};
/// let mut cc = CongestionController::new(CongestionConfig::for_round_trip_us(480));
/// let start = cc.target_bps();
///
/// let mut now = 0;
/// for _ in 0..600 {
///     cc.observe(&DelaySample {
///         one_way_delay_us: 1_400,
///         observed_at_us: now,
///         frame_loss: 0.0,
///     });
///     now += 8_333;
/// }
///
/// assert!(cc.target_bps() > start, "a clean link should climb");
/// ```
#[derive(Debug, Clone)]
pub struct CongestionController {
    config: CongestionConfig,
    target_bps: u32,

    started: bool,
    window_start_us: u64,
    window_min_us: i64,
    window_loss_sum: f64,
    window_count: u32,

    prev_min_us: Option<i64>,
    rising_ticks: u32,
    cooldown_until_us: u64,
    last_observed_us: u64,

    last_gradient_us: i64,
    last_packet_loss: f32,
    last_action: RateAction,
    ticks: u64,
    increases: u64,
    backoffs: u64,
}

impl CongestionController {
    /// Creates a controller sitting at the configured starting rate.
    ///
    /// # Panics
    ///
    /// Panics if the configuration cannot produce sane decisions: a maximum below the
    /// minimum, a start outside the bounds, fewer than one packet per frame, a decrease
    /// factor outside `(0, 1)`, loss thresholds outside `[0, 1]` or in the wrong order, a
    /// zero increase step, or a zero rising-tick requirement. All of these are programming
    /// errors at construction rather than conditions a running session can reach, so
    /// failing loudly beats silently clamping into behaviour nobody asked for.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::cc::{CongestionConfig, CongestionController};
    /// let config = CongestionConfig::default();
    /// let cc = CongestionController::new(config);
    /// assert_eq!(cc.target_bps(), config.start_bps);
    /// ```
    #[must_use]
    pub fn new(config: CongestionConfig) -> Self {
        assert!(config.min_bps > 0, "minimum bitrate must be positive");
        assert!(
            config.max_bps >= config.min_bps,
            "maximum bitrate must not be below the minimum"
        );
        assert!(
            (config.min_bps..=config.max_bps).contains(&config.start_bps),
            "starting bitrate must lie between the minimum and the maximum"
        );
        assert!(
            config.packets_per_frame.is_finite() && config.packets_per_frame >= 1.0,
            "a frame is at least one packet"
        );
        assert!(
            config.decrease_factor.is_finite()
                && config.decrease_factor > 0.0
                && config.decrease_factor < 1.0,
            "the decrease factor must shrink the rate without zeroing it"
        );
        assert!(
            config.loss_increase_ceiling.is_finite()
                && config.loss_decrease_floor.is_finite()
                && (0.0..=1.0).contains(&config.loss_increase_ceiling)
                && (0.0..=1.0).contains(&config.loss_decrease_floor)
                && config.loss_increase_ceiling <= config.loss_decrease_floor,
            "loss thresholds must be fractions with the increase ceiling at or below the \
             decrease floor"
        );
        assert!(
            config.increase_step_bps > 0,
            "the increase step must actually increase"
        );
        assert!(
            config.rising_ticks_to_back_off > 0,
            "a delay back-off must require at least one rising tick"
        );

        Self {
            config,
            target_bps: config.start_bps,
            started: false,
            window_start_us: 0,
            window_min_us: i64::MAX,
            window_loss_sum: 0.0,
            window_count: 0,
            prev_min_us: None,
            rising_ticks: 0,
            cooldown_until_us: 0,
            last_observed_us: 0,
            last_gradient_us: 0,
            last_packet_loss: 0.0,
            last_action: RateAction::Hold,
            ticks: 0,
            increases: 0,
            backoffs: 0,
        }
    }

    /// Folds one observation in and returns the target rate that now applies.
    ///
    /// Constant time and allocation-free. Most calls only update three accumulators; a
    /// decision is taken on the call that first crosses a tick boundary, and even that is a
    /// handful of comparisons.
    ///
    /// The returned value is the same one [`Self::target_bps`] would give, offered here so
    /// a caller feeding the controller from the feedback path does not need a second call.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::cc::{CongestionConfig, CongestionController, DelaySample};
    /// let mut cc = CongestionController::new(CongestionConfig::default());
    /// let rate = cc.observe(&DelaySample {
    ///     one_way_delay_us: -800_000,
    ///     observed_at_us: 0,
    ///     frame_loss: 0.0,
    /// });
    ///
    /// // A wildly wrong clock offset does not disturb the rate: absolute delay is never
    /// // consulted, only the difference between one window and the next.
    /// assert_eq!(rate, cc.target_bps());
    /// ```
    pub fn observe(&mut self, sample: &DelaySample) -> u32 {
        let now = sample.observed_at_us;

        if !self.started {
            self.started = true;
            self.open_window(now);
        }

        let elapsed = now.saturating_sub(self.window_start_us);
        if elapsed >= self.config.tick_us() && self.window_count > 0 {
            self.close_window(now, elapsed);
            self.open_window(now);
        }

        self.window_min_us = self.window_min_us.min(sample.one_way_delay_us);
        self.window_loss_sum += f64::from(sane_loss(sample.frame_loss));
        self.window_count += 1;
        self.last_observed_us = now;

        self.target_bps
    }

    /// Returns the rate the encoder should be running at, in bits per second.
    ///
    /// Always within the configured bounds, before the first observation and after every
    /// one.
    #[must_use]
    pub fn target_bps(&self) -> u32 {
        self.target_bps
    }

    /// Returns the configuration in force.
    #[must_use]
    pub fn config(&self) -> CongestionConfig {
        self.config
    }

    /// Retunes the time-based parameters against a freshly measured round trip.
    ///
    /// Clock sync only improves its estimate when a round trip happens to beat every one
    /// before it, so the figure worth acting on arrives some way into a session rather than
    /// at the start. Retuning mid-session is safe: the tick, cooldown and gradient
    /// threshold are derived on demand, so nothing already accumulated is invalidated.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::cc::{CongestionConfig, CongestionController};
    /// let mut cc = CongestionController::new(CongestionConfig::default());
    /// cc.set_round_trip_us(40_000);
    /// assert_eq!(cc.config().tick_us(), 80_000);
    /// ```
    pub fn set_round_trip_us(&mut self, round_trip_us: u64) {
        self.config.round_trip_us = round_trip_us;
    }

    /// Returns a snapshot for the stats surface.
    #[must_use]
    pub fn stats(&self) -> CongestionStats {
        CongestionStats {
            target_bps: self.target_bps,
            last_gradient_us: self.last_gradient_us,
            packet_loss: self.last_packet_loss,
            rising_ticks: self.rising_ticks,
            ticks: self.ticks,
            increases: self.increases,
            backoffs: self.backoffs,
            cooling_down: self.last_observed_us < self.cooldown_until_us,
            last_action: self.last_action,
        }
    }

    /// Starts a fresh accumulation window at `now`.
    fn open_window(&mut self, now: u64) {
        self.window_start_us = now;
        self.window_min_us = i64::MAX;
        self.window_loss_sum = 0.0;
        self.window_count = 0;
    }

    /// Closes the current window and decides what the rate should do.
    ///
    /// The window statistic is the minimum delay, not the mean, because queueing only ever
    /// adds to delay: the smallest sample is the one that queued least, so one enormous
    /// sample among clean ones changes nothing.
    fn close_window(&mut self, now: u64, elapsed: u64) {
        self.ticks += 1;

        let min_us = self.window_min_us;
        let frame_loss = (self.window_loss_sum / f64::from(self.window_count)) as f32;
        let packet_loss = equivalent_packet_loss(frame_loss, self.config.packets_per_frame);
        self.last_packet_loss = packet_loss;

        let previous = self.prev_min_us.take();

        // A window that took far longer than a tick means the feedback stopped, not that
        // the path slowed down. Both the decision this window would produce and the
        // baseline it would leave behind are discarded, because the next window would
        // otherwise be compared against a measurement from before the silence — a
        // difference that spans the gap and describes nothing that happened in it.
        if elapsed > self.config.stale_us() {
            self.rising_ticks = 0;
            self.last_action = RateAction::Hold;
            return;
        }

        self.prev_min_us = Some(min_us);

        let Some(previous) = previous else {
            self.last_action = RateAction::Hold;
            return;
        };

        let gradient_us = min_us.saturating_sub(previous);
        self.last_gradient_us = gradient_us;

        // Inside the cooldown the trend counter is not merely ignored but frozen, because
        // the rate cut that started the cooldown has not reached the bottleneck yet.
        // Delay still rising here is the previous congestion event, not a new one, and
        // letting it accumulate toward the next cut is precisely the double counting that
        // ratchets a controller to the floor.
        if now < self.cooldown_until_us {
            self.last_action = RateAction::Hold;
            return;
        }

        let rising = gradient_us > self.config.gradient_rise_us();
        self.rising_ticks = if rising {
            self.rising_ticks.saturating_add(1)
        } else {
            self.rising_ticks.saturating_sub(1)
        };

        if self.rising_ticks >= self.config.rising_ticks_to_back_off {
            self.decrease(now, RateAction::DecreaseDelay);
        } else if packet_loss > self.config.loss_decrease_floor {
            self.decrease(now, RateAction::DecreaseLoss);
        } else if !rising && packet_loss < self.config.loss_increase_ceiling {
            self.increase();
        } else {
            self.last_action = RateAction::Hold;
        }
    }

    /// Cuts the rate multiplicatively and starts the cooldown.
    fn decrease(&mut self, now: u64, why: RateAction) {
        let reduced = f64::from(self.target_bps) * f64::from(self.config.decrease_factor);
        self.target_bps = (reduced as u32).clamp(self.config.min_bps, self.config.max_bps);
        self.cooldown_until_us = now.saturating_add(self.config.cooldown_us());
        self.rising_ticks = 0;
        self.backoffs += 1;
        self.last_action = why;
    }

    /// Raises the rate by one additive step.
    fn increase(&mut self) {
        self.target_bps = self
            .target_bps
            .saturating_add(self.config.increase_step_bps)
            .clamp(self.config.min_bps, self.config.max_bps);
        self.increases += 1;
        self.last_action = RateAction::Increase;
    }
}

/// Converts a frame-loss fraction into the packet loss that would produce it.
///
/// A frame survives only if every one of its packets does, so
/// `frame_loss = 1 - (1 - packet_loss)^n` and this is that relation inverted. It assumes
/// packets are lost independently and that no forward error correction repaired any of
/// them; with FEC in play the answer reads low, which is the harmless direction — loss the
/// receiver never noticed did not cost the user anything.
///
/// This exists because loss thresholds in the congestion control literature are written in
/// packet terms while this project's feedback reports frames. Converting the observation
/// keeps the thresholds meaningful; converting the thresholds instead does not, since 10%
/// packet loss becomes 97.5% frame loss at thirty-five packets per frame.
///
/// Values outside `[0, 1]` and non-finite values are treated as no loss.
///
/// # Examples
///
/// ```
/// # use prism_core::net::cc::equivalent_packet_loss;
/// // The trap this function exists for: 3% frame loss is under a tenth of a percent of
/// // packet loss once the thirty-five packets in a frame are accounted for.
/// let packet_loss = equivalent_packet_loss(0.03, 35.0);
/// assert!((0.0008..0.0010).contains(&packet_loss), "{packet_loss}");
///
/// // Losing every frame means losing every packet, and one packet per frame is identity.
/// assert_eq!(equivalent_packet_loss(1.0, 35.0), 1.0);
/// assert_eq!(equivalent_packet_loss(0.25, 1.0), 0.25);
/// ```
#[must_use]
pub fn equivalent_packet_loss(frame_loss: f32, packets_per_frame: f32) -> f32 {
    let frame_loss = sane_loss(frame_loss);

    if frame_loss >= 1.0 {
        return 1.0;
    }
    if !packets_per_frame.is_finite() || packets_per_frame <= 1.0 {
        return frame_loss;
    }

    let survived = 1.0 - f64::from(frame_loss);
    let per_packet = survived.powf(1.0 / f64::from(packets_per_frame));

    ((1.0 - per_packet) as f32).clamp(0.0, 1.0)
}

/// Returns a loss fraction guaranteed to be a real number in `[0, 1]`.
///
/// Feedback-derived figures reach this module after arithmetic on wire values, so a NaN is
/// reachable from a malformed report. One folded into the running sum would silently
/// disable the loss branch for the rest of the session, which is a far worse failure than
/// reading one bad report as no loss.
fn sane_loss(loss: f32) -> f32 {
    if loss.is_finite() {
        loss.clamp(0.0, 1.0)
    } else {
        0.0
    }
}
