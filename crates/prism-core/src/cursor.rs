//! Tracking where the host's pointer is, so the client can draw it.
//!
//! The host keeps its cursor out of the captured video, which leaves the client to draw
//! one. Drawing it only where the host last said it was would make the cursor inherit the
//! whole video latency — the single most obvious way a remote desktop feels remote.
//!
//! So the client predicts. It already knows every movement it sent, so it applies each one
//! the instant it happens and draws there. The host's readings are correction, not the
//! source: they carry everything the client could not have known — the host's own user, a
//! window warping the pointer, an edge it clamped against.
//!
//! Correction is the standard reconciliation. Each reading says which host-clock instant it
//! was taken at, and the client stamps its own movements in the same clock, so a reading
//! already accounts for every movement older than it. Those are dropped; the newer ones are
//! replayed on top of the reading. Without the replay the cursor would snap backwards by
//! one round trip every time a reading arrived.

use std::collections::VecDeque;

use crate::net::packet::CursorPosition;

/// How many sent movements may await correction before the oldest are forgotten.
///
/// A thousand-hertz mouse against a sixty-hertz host fills about seventeen. The cap is far
/// above that so it is only reached when readings have stopped arriving entirely, and at
/// that point the oldest movements are the ones least worth keeping.
const MAX_PENDING: usize = 512;

/// One movement the client has sent and the host has not yet confirmed.
#[derive(Debug, Clone, Copy)]
struct PendingMove {
    /// When it happened, in the host's clock — the same stamp that went on the wire.
    origin_ts_us: u64,
    dx: i16,
    dy: i16,
}

/// Where the client believes the host's pointer is.
///
/// Starts with no belief at all: until the host has reported once, the client does not know
/// the screen it is predicting against, and a cursor drawn at a guessed position would be
/// worse than none.
#[derive(Debug, Default)]
pub struct CursorTracker {
    position: Option<Position>,
    pending: VecDeque<PendingMove>,
    forgotten: u64,
}

/// A believed pointer position and the screen it is on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Position {
    x: f32,
    y: f32,
    screen_width: u16,
    screen_height: u16,
}

impl CursorTracker {
    /// Creates a tracker that believes nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a movement the client has just sent, and applies it immediately.
    ///
    /// `origin_ts_us` is the stamp that went on the wire, in the host's clock, which is what
    /// lets a later reading say whether it already accounts for this movement.
    ///
    /// Movements sent before the host has ever reported are still recorded. The screen is
    /// unknown until then, so they cannot be applied, but they can be replayed onto the
    /// first reading that arrives.
    pub fn moved(&mut self, origin_ts_us: u64, dx: i16, dy: i16) {
        if self.pending.len() == MAX_PENDING {
            self.pending.pop_front();
            self.forgotten += 1;
        }

        self.pending.push_back(PendingMove {
            origin_ts_us,
            dx,
            dy,
        });

        if let Some(position) = self.position.as_mut() {
            position.apply(dx, dy);
        }
    }

    /// Corrects the belief against a reading from the host.
    ///
    /// Movements the reading already accounts for are dropped; the rest are replayed on top
    /// of it, so a correction never rewinds movement the host has simply not seen yet.
    ///
    /// The comparison is only as good as the clock synchronisation behind the stamps. Before
    /// an offset is established the client stamps movements in its own clock, which can be
    /// far from the host's, and reconciliation degrades to either trusting the reading
    /// outright or trusting the prediction outright. Both are survivable, and the offset is
    /// usually established within the first few hundred milliseconds of a session.
    pub fn observe(&mut self, reading: CursorPosition) {
        while self
            .pending
            .front()
            .is_some_and(|pending| pending.origin_ts_us <= reading.sample_ts_us)
        {
            self.pending.pop_front();
        }

        let mut position = Position {
            x: f32::from(reading.x),
            y: f32::from(reading.y),
            screen_width: reading.screen_width,
            screen_height: reading.screen_height,
        };

        for pending in &self.pending {
            position.apply(pending.dx, pending.dy);
        }

        self.position = Some(position);
    }

    /// Returns where to draw the cursor, as a fraction of the host's screen.
    ///
    /// Normalised rather than in pixels because the client's window is rarely the size of
    /// the host's screen, and the fraction is what survives the difference.
    ///
    /// `None` until the host has reported at least once.
    #[must_use]
    pub fn normalised(&self) -> Option<(f32, f32)> {
        let position = self.position?;

        Some((
            position.x / f32::from(position.screen_width.max(1)),
            position.y / f32::from(position.screen_height.max(1)),
        ))
    }

    /// Returns how many movements are still awaiting correction.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Returns how many movements were forgotten because corrections stopped arriving.
    ///
    /// Non-zero means the host went quiet for long enough that the cursor is now predicting
    /// against readings it can no longer fully reconcile, which is worth reporting rather
    /// than hiding.
    #[must_use]
    pub fn forgotten(&self) -> u64 {
        self.forgotten
    }
}

impl Position {
    /// Applies one movement, clamped to the screen.
    ///
    /// The host clamps its own pointer at the edges, so a prediction that ran past them
    /// would disagree with every reading until the pointer came back. Clamping here keeps
    /// the two in step for free.
    fn apply(&mut self, dx: i16, dy: i16) {
        self.x = (self.x + f32::from(dx)).clamp(0.0, f32::from(self.screen_width.max(1) - 1));
        self.y = (self.y + f32::from(dy)).clamp(0.0, f32::from(self.screen_height.max(1) - 1));
    }
}
