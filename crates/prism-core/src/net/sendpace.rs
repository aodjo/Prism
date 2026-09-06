//! Deciding when each packet is allowed to leave the host.
//!
//! An encoder that has just finished a frame hands the network its packets in a few
//! hundred microseconds, and a socket will happily put all of them on the link back to
//! back. The narrowest link on the path cannot, so the excess lands in whatever queue
//! sits in front of it, and both outcomes are bad. Either the queue overflows and the
//! loss arrives as a *run* of consecutive packets, which is the worst shape loss can
//! take for video because consecutive packets belong to the same slice; or the queue
//! absorbs the burst and everything behind it waits — including the input events coming
//! the other way, which is a queue this host built and then had to sit behind.
//!
//! Spreading the same bytes across roughly [`SPREAD_PERCENT`] of the frame interval
//! costs nothing in throughput and removes both. Nothing here needs the frame rate to do
//! it: a frame carries `bitrate / fps` bytes and is given `0.8 / fps` seconds to send
//! them in, so the frame rate cancels and the whole policy reduces to pacing at a fixed
//! multiple of the target bitrate. The remaining fifth of the interval is what absorbs a
//! frame that encodes long without pushing into the next one.
//!
//! # This is not the display pacer
//!
//! [`crate::render::pacing`] is a different pacer, on the other machine, solving the
//! opposite problem. It decides how long a *decoded picture* is held before it is
//! *shown*, to hide the spread in arrival times, and it derives its delay by *measuring*
//! that spread. This one decides how long a *packet* is held before it is *sent*, to
//! keep the host from overrunning the path, and it derives its delay from a rate it is
//! *told*. One smooths what has already arrived; the other smooths what is about to
//! leave.
//!
//! # Why a virtual clock and not a token bucket
//!
//! The two are duals, and the virtual clock — one nanosecond timestamp saying when the
//! next packet is due — wins on three counts here. Its whole state is a single `u64`, so
//! there is no fractional token balance to round and no rounding error to accumulate
//! across the four thousand packets a second this sees. Bounding the credit earned while
//! idle is one `max` against the current time rather than a separate clamp that has to
//! be remembered on every path. And a rate change, which congestion control will make
//! often, only changes what future packets cost: a token bucket holds a balance
//! denominated in bytes whose *value in time* silently changes underneath it when the
//! rate moves.
//!
//! # Burst allowance
//!
//! A pacer with no burst allowance stalls the very first packet of every frame for a full
//! packet time, which is latency spent to prevent nothing — one packet cannot overflow
//! anything. So the pacer carries a tolerance of [`DEFAULT_BURST_BYTES`], and the floor
//! of [`MIN_BURST_BYTES`] makes a zero allowance impossible to configure by accident.
//! The allowance is counted in bytes rather than in time because the thing it protects —
//! the buffer in front of the bottleneck link — is itself measured in bytes, and because
//! a byte allowance keeps its meaning as congestion control moves the rate around. Four
//! packets is under five kilobytes, which no realistic switch buffer notices, and it is
//! enough that the head of a slice leaves the moment the encoder produces it.
//!
//! # Timer granularity: the caller must batch its waits
//!
//! At 1440p120 and 40 Mbps the pacer runs at 50 Mbps, a 1200-byte packet costs 192 µs,
//! and the stream averages 4167 packets per second — about 35 packets per frame,
//! released 192 µs apart across 6.7 ms of the 8.3 ms interval.
//!
//! A thread sleep per packet is **not** viable at that spacing. Windows' default timer
//! tick is 15.6 ms and even a high-resolution waitable timer lands near half a
//! millisecond; Linux and macOS overshoot a `nanosleep` by tens to hundreds of
//! microseconds unless the thread has been given a real-time policy. Asking for 192 µs
//! thirty-five times a frame would be thirty-five scheduler round trips whose error is
//! comparable to the interval being asked for — the rate would come out wrong *and* the
//! syscalls would cost more than the sends they were spacing.
//!
//! So the caller batches, and [`PacerConfig::min_sleep`] is how it says so. A wait
//! shorter than that is reported as zero and the time it was owed is **carried in the
//! virtual clock**, to be paid off by a longer wait a few packets later. The long-run
//! rate is unchanged — that is the point, and the tests hold it to it — while the spacing
//! is coarsened into groups the operating system can actually deliver: at the default
//! millisecond, six sleeps per frame of six packets each instead of thirty-five sleeps
//! that would not have been honoured. A caller with a better timer, a busy-wait loop or
//! a real-time thread, sets `min_sleep` to zero and gets the exact per-packet answer.
//!
//! # Shape
//!
//! The pacer owns no socket, no thread, and no buffer. It reads no clock — the caller
//! passes the time in, which is what makes it testable and what lets the send loop reuse
//! a timestamp it already had. It allocates nothing, ever. Slice-level streaming makes
//! this the only workable shape: a slice must go out the instant it is encoded, so there
//! is no frame to hold and dole out, only a question asked once per packet.

use core::time::Duration;

use crate::net::packet::MAX_PACKET_SIZE;

/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Percentage of the frame interval a frame's packets are spread across.
///
/// The remaining fifth of the interval is headroom: a frame that encodes larger than its
/// budget, or arrives late, has somewhere to go without colliding with the next one.
pub const SPREAD_PERCENT: u32 = 80;

/// Lowest bitrate the pacer will pace at, in bits per second.
///
/// Congestion control drives the rate and can be asked, by a path that has collapsed, to
/// drive it to zero. Honouring that literally would divide by zero, and if it did not it
/// would wedge the session behind an unbounded wait — including the probe that would
/// have raised the rate again. Below a megabit a full packet already takes longer than a
/// frame interval, so a session down here should be dropping frames rather than pacing
/// them; the pacer holds the floor and leaves stopping to the layer that can resume.
pub const MIN_BITRATE_BPS: u32 = 1_000_000;

/// Bitrate a pacer starts at when the caller does not name one, in bits per second.
///
/// A plausible 1080p60 figure, meant only to be somewhere sane before congestion control
/// has measured anything.
pub const DEFAULT_BITRATE_BPS: u32 = 20_000_000;

/// Smallest burst allowance the pacer will accept, in bytes.
///
/// One full packet, because an allowance below that delays the first packet after any
/// idle period and prevents nothing by doing so.
pub const MIN_BURST_BYTES: u32 = MAX_PACKET_SIZE as u32;

/// Burst allowance used when the caller does not choose one, in bytes.
pub const DEFAULT_BURST_BYTES: u32 = 4 * MIN_BURST_BYTES;

/// Shortest wait worth handing to a thread sleep on the platform with the coarsest timer.
pub const DEFAULT_MIN_SLEEP: Duration = Duration::from_millis(1);

/// How the send pacer should shape its traffic.
#[derive(Debug, Clone, Copy)]
pub struct PacerConfig {
    /// Target bitrate for the video stream, in bits per second.
    ///
    /// This is the rate the stream should *average*, not the rate packets leave at: the
    /// pacer sends at `bitrate_bps * 100 / SPREAD_PERCENT` so a frame's worth of bytes
    /// occupies [`SPREAD_PERCENT`] of the frame interval. Values below
    /// [`MIN_BITRATE_BPS`] are raised to it.
    pub bitrate_bps: u32,
    /// How many bytes may leave back to back before pacing takes hold.
    ///
    /// Values below [`MIN_BURST_BYTES`] are raised to it.
    pub burst_bytes: u32,
    /// Shortest wait the caller is willing to actually sleep for.
    ///
    /// Anything shorter is reported as zero and carried forward. [`Duration::ZERO`] asks
    /// for the exact per-packet answer, which only a caller with a sub-millisecond timer
    /// can honour.
    pub min_sleep: Duration,
}

impl Default for PacerConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: DEFAULT_BITRATE_BPS,
            burst_bytes: DEFAULT_BURST_BYTES,
            min_sleep: DEFAULT_MIN_SLEEP,
        }
    }
}

/// Decides how long the caller must wait before putting each packet on the wire.
///
/// # Examples
///
/// ```
/// # use core::time::Duration;
/// # use prism_core::net::sendpace::{PacerConfig, SendPacer};
/// let mut pacer = SendPacer::new(PacerConfig {
///     bitrate_bps: 40_000_000,
///     min_sleep: Duration::ZERO,
///     ..PacerConfig::default()
/// });
///
/// // A whole frame is offered the instant the encoder finishes it, and the caller waits
/// // wherever it is told to, so `now` ends up being how long the frame took to leave.
/// let mut now = 0u64;
/// for _ in 0..35 {
///     now += pacer.wait_before(1200, now).as_nanos() as u64;
/// }
///
/// // Thirty-five packets is 42 kB, which at the paced 50 Mbps takes 6.7 ms, less the
/// // burst allowance the pacer gave away at the head of the frame.
/// assert!(now > 5_000_000 && now < 7_000_000, "{now} ns");
/// ```
#[derive(Debug)]
pub struct SendPacer {
    bitrate_bps: u32,
    pacing_rate_bps: u64,
    burst_bytes: u32,
    burst_ns: u64,
    min_sleep_ns: u64,
    next_ns: u64,
    packets: u64,
    waits: u64,
    waited_ns: u64,
}

impl SendPacer {
    /// Creates a pacer from `config`, raising any out-of-range field to its floor.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::sendpace::{MIN_BITRATE_BPS, MIN_BURST_BYTES};
    /// # use prism_core::net::sendpace::{PacerConfig, SendPacer};
    /// let pacer = SendPacer::new(PacerConfig {
    ///     bitrate_bps: 0,
    ///     burst_bytes: 0,
    ///     ..PacerConfig::default()
    /// });
    ///
    /// assert_eq!(pacer.bitrate_bps(), MIN_BITRATE_BPS);
    /// assert_eq!(pacer.burst_bytes(), MIN_BURST_BYTES);
    /// ```
    #[must_use]
    pub fn new(config: PacerConfig) -> Self {
        let mut pacer = Self {
            bitrate_bps: config.bitrate_bps.max(MIN_BITRATE_BPS),
            pacing_rate_bps: 0,
            burst_bytes: config.burst_bytes.max(MIN_BURST_BYTES),
            burst_ns: 0,
            min_sleep_ns: u64::try_from(config.min_sleep.as_nanos()).unwrap_or(u64::MAX),
            next_ns: 0,
            packets: 0,
            waits: 0,
            waited_ns: 0,
        };
        pacer.retune();
        pacer
    }

    /// Returns how long the caller must wait before sending a packet of `packet_bytes`.
    ///
    /// `now_ns` is the current time in nanoseconds from any fixed origin and must not go
    /// backwards; a monotonic source such as `Instant::now().duration_since(origin)` is
    /// what this expects. `packet_bytes` is whatever the caller wants metered — note that
    /// a UDP payload alone undercounts the link by the 28-byte IPv4 or 48-byte IPv6
    /// header, so a caller that wants the pacing rate to mean *link* rate should add it.
    ///
    /// Every call accounts for the packet whether or not the returned wait is honoured,
    /// so the packet must actually be sent. A wait shorter than
    /// [`PacerConfig::min_sleep`] comes back as [`Duration::ZERO`] and is carried forward
    /// rather than forgiven, which is what keeps the long-run rate exact while the caller
    /// sleeps in batches the operating system can deliver.
    ///
    /// Input and control traffic should bypass the pacer entirely rather than be metered
    /// here: the plan gives the input path priority over everything else, and eighteen
    /// bytes of cursor position cannot congest anything.
    ///
    /// # Examples
    ///
    /// ```
    /// # use core::time::Duration;
    /// # use prism_core::net::sendpace::{PacerConfig, SendPacer};
    /// let mut pacer = SendPacer::new(PacerConfig {
    ///     bitrate_bps: 40_000_000,
    ///     min_sleep: Duration::from_millis(1),
    ///     ..PacerConfig::default()
    /// });
    ///
    /// // Offered a frame all at one instant, the pacer reports nothing for the waits too
    /// // short to sleep for and collects that debt into ones worth sleeping for.
    /// let waits: Vec<Duration> = (0..12).map(|_| pacer.wait_before(1200, 0)).collect();
    ///
    /// assert!(waits.iter().all(|w| w.is_zero() || *w >= Duration::from_millis(1)));
    /// assert!(waits.iter().any(|w| !w.is_zero()));
    /// ```
    pub fn wait_before(&mut self, packet_bytes: usize, now_ns: u64) -> Duration {
        let cost_ns = nanos_for(packet_bytes as u64, self.pacing_rate_bps);

        let wait_ns = self
            .next_ns
            .saturating_sub(self.burst_ns)
            .saturating_sub(now_ns);

        // Taking the later of the two is what bounds the credit an idle pacer earns: once
        // real time has passed the virtual clock, the virtual clock restarts from now and
        // everything older than the burst allowance is gone for good.
        self.next_ns = self.next_ns.max(now_ns).saturating_add(cost_ns);
        self.packets += 1;

        if wait_ns == 0 || wait_ns < self.min_sleep_ns {
            return Duration::ZERO;
        }

        self.waits += 1;
        self.waited_ns = self.waited_ns.saturating_add(wait_ns);
        Duration::from_nanos(wait_ns)
    }

    /// Sets the target bitrate, raising it to [`MIN_BITRATE_BPS`] if it is below the floor.
    ///
    /// The new rate applies to every packet from the next call onwards. Debt already owed
    /// at the old rate is kept: it is real time the path has not been given back yet, and
    /// forgiving it on every change would let a controller that oscillates send faster
    /// than either of the rates it oscillates between.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::sendpace::{MIN_BITRATE_BPS, PacerConfig, SendPacer};
    /// let mut pacer = SendPacer::new(PacerConfig::default());
    ///
    /// pacer.set_bitrate_bps(8_000_000);
    /// assert_eq!(pacer.bitrate_bps(), 8_000_000);
    ///
    /// // A controller that has collapsed to nothing is held at the floor, because a
    /// // pacer that stops entirely can never send the probe that would recover.
    /// pacer.set_bitrate_bps(0);
    /// assert_eq!(pacer.bitrate_bps(), MIN_BITRATE_BPS);
    /// ```
    pub fn set_bitrate_bps(&mut self, bitrate_bps: u32) {
        self.bitrate_bps = bitrate_bps.max(MIN_BITRATE_BPS);
        self.retune();
    }

    /// Returns the target bitrate in bits per second, after clamping.
    #[must_use]
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// Returns the rate packets actually leave at, in bits per second.
    ///
    /// This is the target bitrate divided by [`SPREAD_PERCENT`], and it is the number a
    /// measured send rate should be compared against — the target bitrate is what the
    /// stream averages over a whole frame interval, not what it does while a frame is
    /// going out.
    #[must_use]
    pub fn pacing_rate_bps(&self) -> u64 {
        self.pacing_rate_bps
    }

    /// Returns the burst allowance in bytes, after clamping.
    #[must_use]
    pub fn burst_bytes(&self) -> u32 {
        self.burst_bytes
    }

    /// Returns the shortest wait this pacer will report rather than carry forward.
    #[must_use]
    pub fn min_sleep(&self) -> Duration {
        Duration::from_nanos(self.min_sleep_ns)
    }

    /// Returns how many packets the pacer has admitted.
    #[must_use]
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Returns how many packets were held back rather than released immediately.
    #[must_use]
    pub fn waits(&self) -> u64 {
        self.waits
    }

    /// Returns the total time the pacer has asked the caller to wait.
    ///
    /// Set against wall time this is the honest measure of whether the encoder is
    /// outrunning the rate: a sender held for most of a second is producing bytes faster
    /// than the path has been judged able to carry them, and it is the bitrate that
    /// should come down rather than the pacer that should be loosened.
    #[must_use]
    pub fn total_wait(&self) -> Duration {
        Duration::from_nanos(self.waited_ns)
    }

    /// Recomputes everything derived from the bitrate.
    ///
    /// Kept in one place so the invariant the rest of the module leans on — that
    /// `pacing_rate_bps` is never zero — has exactly one thing holding it up.
    fn retune(&mut self) {
        self.pacing_rate_bps = u64::from(self.bitrate_bps) * 100 / u64::from(SPREAD_PERCENT);
        self.burst_ns = nanos_for(u64::from(self.burst_bytes), self.pacing_rate_bps);
    }
}

/// Returns how long `bytes` take to leave at `rate_bps`, in nanoseconds.
///
/// Saturating multiplication keeps a nonsensical packet length from wrapping into a short
/// wait; truncating division makes each packet cost up to one nanosecond less than its
/// true time, which biases the achieved rate up by well under a part per million at any
/// packet rate a video stream reaches — far inside the accuracy of the timer that will
/// honour the wait.
///
/// A zero rate cannot reach here, since both places that set the rate clamp it, but were
/// one ever to slip through the answer is no pacing rather than an unbounded wait: an
/// unpaced session is a worse session, and a stalled one is no session at all.
fn nanos_for(bytes: u64, rate_bps: u64) -> u64 {
    bytes
        .saturating_mul(8)
        .saturating_mul(NANOS_PER_SEC)
        .checked_div(rate_bps)
        .unwrap_or(0)
}
