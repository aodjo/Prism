//! Telling the host which frames arrived.
//!
//! The client reports the newest frame it has fully reassembled and a bitmap of the
//! thirty-two before it. That is a complete picture of a short recent history in nine
//! bytes, and it is what two other things are built on: the encoder needs to know which
//! frames are safe to reference so that loss never forces a keyframe, and the congestion
//! controller needs to know what fraction of frames are going missing.
//!
//! Reports are cumulative rather than incremental. A lost report costs nothing, because the
//! next one carries the same history again — which is the only sane design for something
//! travelling over the same lossy path it is reporting on.

use crate::net::packet::FeedbackPacket;

/// How many frames before the newest fit in the bitmap.
const HISTORY: u32 = 32;

/// Records which frames have been fully received.
///
/// Frame identifiers are monotonic and wrap, so every comparison here is wrapping. At a
/// hundred and twenty frames a second a `u32` takes over a year to come round, but the
/// arithmetic is the same cost either way and getting it wrong would be a fault that only
/// appears after a year of uptime.
#[derive(Debug, Default)]
pub struct AckTracker {
    newest: Option<u32>,
    bitmap: u32,
}

impl AckTracker {
    /// Creates a tracker that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a frame was fully reassembled.
    ///
    /// Frames may arrive out of order, so this handles both directions: a newer frame
    /// shifts the history along, and an older one sets its bit if it is still inside the
    /// window. A frame older than the window is dropped, because there is nowhere to say it
    /// arrived and the host has long since moved on.
    pub fn received(&mut self, frame_id: u32) {
        let Some(newest) = self.newest else {
            self.newest = Some(frame_id);
            return;
        };

        let advance = frame_id.wrapping_sub(newest);

        if is_newer(frame_id, newest) {
            // The old newest becomes an ordinary member of the history, one place below the
            // new arrival. A jump of more than the window empties the bitmap, which is
            // correct: nothing in it can still be described.
            self.bitmap = if advance >= HISTORY {
                0
            } else {
                let shifted = self.bitmap << advance;
                shifted | (1u32 << (advance - 1))
            };
            self.newest = Some(frame_id);
            return;
        }

        let behind = newest.wrapping_sub(frame_id);
        if (1..=HISTORY).contains(&behind) {
            self.bitmap |= 1u32 << (behind - 1);
        }
    }

    /// Builds the report to send to the host.
    ///
    /// `None` until at least one frame has been received, because there is nothing
    /// meaningful to say before then and a report claiming frame zero would be a lie the
    /// encoder would act on.
    #[must_use]
    pub fn report(&self, client_ts_us: u64) -> Option<FeedbackPacket> {
        Some(FeedbackPacket {
            last_frame_id: self.newest?,
            recv_bitmap: self.bitmap,
            client_ts_us,
        })
    }

    /// Returns the newest frame fully received, if any.
    #[must_use]
    pub fn newest(&self) -> Option<u32> {
        self.newest
    }
}

/// Returns how many of the frames a report describes are missing.
///
/// The report covers the newest frame plus the [`HISTORY`] before it. The newest is present
/// by definition — it is what the report is anchored on — so only the bitmap can show gaps.
///
/// This counts FRAMES, not packets, and the difference matters more than it looks. A frame
/// is tens of packets, so a link losing a small fraction of packets loses a much larger
/// fraction of frames. Anything comparing this against a threshold from the congestion
/// control literature, which is written in packet terms, has to convert first.
///
/// # Examples
///
/// ```
/// # use prism_core::net::ack::missing_in_history;
/// assert_eq!(missing_in_history(0xFFFF_FFFF), 0);
/// assert_eq!(missing_in_history(0), 32);
/// ```
#[must_use]
pub fn missing_in_history(recv_bitmap: u32) -> u32 {
    recv_bitmap.count_zeros()
}

/// Returns whether `candidate` is newer than `reference`, allowing for wrapping.
///
/// Half the identifier space is treated as the future and half as the past, which is the
/// standard sequence-number comparison. It is the only correct answer without extra state:
/// with a counter that wraps, "newer" is only meaningful within half a lap.
///
/// Public because the host needs the same judgement when reports arrive out of order, and
/// two implementations of this comparison would eventually disagree.
///
/// # Examples
///
/// ```
/// # use prism_core::net::ack::is_newer;
/// assert!(is_newer(5, 4));
/// assert!(!is_newer(4, 5));
/// assert!(is_newer(0, u32::MAX), "the frame after a wrap is newer, not oldest");
/// ```
#[must_use]
pub fn is_newer(candidate: u32, reference: u32) -> bool {
    candidate != reference && (candidate.wrapping_sub(reference) as i32) > 0
}
