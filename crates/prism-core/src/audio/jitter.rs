//! Holding sound just long enough that the network's unevenness stops being audible.
//!
//! Packets do not arrive at a steady rate — a frame sent every five milliseconds arrives at
//! four, then seven, then five — and playback consumes them at exactly one every five. Feeding
//! the output directly from the network therefore runs dry, and running dry is a click.
//!
//! So a few frames are held back. That delay is pure cost and the buffer's whole job is to
//! hold the smallest number that keeps the output fed, measured from what the path is actually
//! doing rather than set to a figure that is safe everywhere and wrong here.
//!
//! # Why this is not the video pacer
//!
//! The video pacer holds pictures to a common age and drops one when it is too late, because a
//! picture shown a frame late is invisible and a picture skipped is nearly so. Neither is true
//! of sound: a five millisecond hole is a click that every listener hears, and playing a frame
//! late is better than not playing it. This buffer therefore conceals gaps rather than
//! skipping them, and it never drops a frame that has arrived in order.

use std::collections::BTreeMap;

use crate::audio::FRAME_US;

/// How many frames the buffer will hold before it decides the stream restarted.
///
/// Half a second of audio. Beyond that a sender has plainly stopped and started rather than
/// merely paused, and holding the old frames would play half a second of stale sound before
/// the new stream was heard.
const MAX_HELD: usize = 100;

/// How many frames of headroom to keep beyond what the measured jitter needs.
///
/// One frame. The measurement is of the recent past and the next packet is in the future, so a
/// buffer sized exactly to what has happened runs dry the first time something takes longer.
const HEADROOM: u32 = 1;

/// The most delay the buffer will ever add, in frames.
///
/// Sixty milliseconds. A path that needs more than that is a path where sound will be audibly
/// behind the picture no matter what this does, and adding more delay to chase it makes the
/// worse problem worse.
const MAX_DEPTH: u32 = 12;

/// The least it will ever hold.
///
/// Two frames. One would mean the output is fed by whichever packet happens to have arrived,
/// which on a perfectly steady path works and on any real one does not.
const MIN_DEPTH: u32 = 2;

/// What came out of the buffer when playback asked for a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pull<'a> {
    /// A frame that arrived, in order.
    Frame {
        /// Its sequence number.
        sequence: u32,
        /// The Opus packet.
        payload: &'a [u8],
    },
    /// The frame at this position never arrived, and the decoder should conceal it.
    Missing {
        /// The sequence number that was expected.
        sequence: u32,
    },
    /// Nothing is ready yet: the buffer is still filling, or the stream has stopped.
    Empty,
}

/// Holds arriving audio frames and hands them to playback in order.
#[derive(Debug)]
pub struct JitterBuffer {
    /// Frames held by sequence number. Ordered, because playback wants the oldest and
    /// arrivals are not ordered.
    held: BTreeMap<u32, Vec<u8>>,
    /// The next sequence playback will ask for, once the buffer has started.
    next: Option<u32>,
    /// How many frames to hold before starting, and to aim to keep holding.
    depth: u32,
    /// The widest gap seen recently between the expected and actual arrival of a frame.
    spread_us: u32,
    /// When the previous packet arrived, for measuring that spread.
    last_arrival_us: Option<u64>,
    late: u64,
    lost: u64,
    duplicated: u64,
    /// Where a frame handed to playback lives while the caller reads it.
    ///
    /// One buffer rather than an allocation per frame: playback pulls two hundred times a
    /// second and none of them needs the heap.
    scratch: Vec<u8>,
}

impl Default for JitterBuffer {
    /// Creates a buffer at its minimum depth, which is where it starts before it has measured
    /// anything.
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    /// Creates an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            held: BTreeMap::new(),
            next: None,
            depth: MIN_DEPTH,
            spread_us: 0,
            last_arrival_us: None,
            late: 0,
            lost: 0,
            duplicated: 0,
            scratch: Vec::new(),
        }
    }

    /// Takes an arriving frame.
    ///
    /// `arrived_us` is this machine's clock, used only to measure how uneven arrivals are.
    /// A frame older than what playback has already passed is counted and dropped: playing it
    /// would mean going backwards.
    pub fn push(&mut self, sequence: u32, payload: &[u8], arrived_us: u64) {
        self.observe(arrived_us);

        if let Some(next) = self.next {
            // Wrapping-aware: a sequence number is a position on a circle, and a stream that
            // has been running for six hours at two hundred frames a second is a quarter of the
            // way round it.
            if sequence.wrapping_sub(next) > u32::MAX / 2 {
                self.late += 1;
                return;
            }
        }

        if self.held.insert(sequence, payload.to_vec()).is_some() {
            self.duplicated += 1;
        }

        // A stream that jumped rather than paused. Holding what came before would play half a
        // second of stale sound before anything new was heard.
        if self.held.len() > MAX_HELD {
            self.restart();
        }
    }

    /// Hands playback the next frame, or says why it cannot.
    ///
    /// Called at the rate playback consumes frames, which is one every [`FRAME_US`]
    /// microseconds and not on packet arrival.
    pub fn pull(&mut self) -> Pull<'_> {
        let Some(next) = self.next else {
            // Still filling. Nothing plays until there is enough held to keep playing.
            if (self.held.len() as u32) < self.depth {
                return Pull::Empty;
            }

            let first = *self.held.keys().next().expect("the buffer is not empty");
            self.next = Some(first);

            return self.take(first);
        };

        if self.held.contains_key(&next) {
            return self.take(next);
        }

        // Nothing at this position. Either it is still in flight, or it is gone.
        //
        // The distinction is made by whether anything *after* it has arrived: a later frame in
        // hand means this one is not coming, because the path does not reorder by more than a
        // frame or two and waiting longer would only turn a concealed gap into a stall.
        let later = self.held.range(next.wrapping_add(1)..).next().is_some();

        if !later {
            return Pull::Empty;
        }

        self.lost += 1;
        self.next = Some(next.wrapping_add(1));

        Pull::Missing { sequence: next }
    }

    /// Returns how many frames are waiting.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Returns how many frames the buffer is currently holding back before playing.
    ///
    /// This is the delay audio is paying, in frames: multiply by [`FRAME_US`] for the time.
    #[must_use]
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Returns how many frames arrived too late to play.
    #[must_use]
    pub fn late(&self) -> u64 {
        self.late
    }

    /// Returns how many frames never arrived and had to be concealed.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Returns how many frames arrived twice.
    #[must_use]
    pub fn duplicated(&self) -> u64 {
        self.duplicated
    }

    /// Removes a frame and advances the position playback wants next.
    fn take(&mut self, sequence: u32) -> Pull<'_> {
        self.next = Some(sequence.wrapping_add(1));

        let payload = self
            .held
            .remove(&sequence)
            .expect("the caller checked the frame is held");

        // Held rather than returned directly, so the borrow lives as long as the caller needs
        // it without the map holding a copy.
        self.scratch = payload;

        Pull::Frame {
            sequence,
            payload: &self.scratch,
        }
    }

    /// Updates the depth from how uneven arrivals have been.
    ///
    /// The measurement is the gap between arrivals against the gap frames were sent at. A path
    /// that delivers every five milliseconds needs no depth beyond the minimum; one that
    /// delivers at four and then at eleven needs enough to cover the eleven.
    fn observe(&mut self, arrived_us: u64) {
        let Some(previous) = self.last_arrival_us.replace(arrived_us) else {
            return;
        };

        let gap = arrived_us.saturating_sub(previous);
        let excess = gap.saturating_sub(u64::from(FRAME_US)) as u32;

        // Decays, so a single stall does not hold the buffer deep for the rest of the session.
        // Rises immediately, because being one frame short is audible and being one frame deep
        // is not.
        self.spread_us = if excess > self.spread_us {
            excess
        } else {
            self.spread_us - (self.spread_us - excess) / 32
        };

        let needed = self.spread_us.div_ceil(FRAME_US) + HEADROOM;
        self.depth = needed.clamp(MIN_DEPTH, MAX_DEPTH);
    }

    /// Throws away everything held and starts filling again.
    fn restart(&mut self) {
        self.held.clear();
        self.next = None;
        self.last_arrival_us = None;
    }
}
