//! Timestamps for latency measurement.
//!
//! Frames are stamped at capture and the stamp travels in every packet, so the receiver
//! can price the whole journey from any packet that arrives.
//!
//! The epoch is the Unix epoch, which makes stamps directly comparable between two
//! processes on one machine. Across machines the two clocks differ by an unknown offset,
//! and correcting for it is what the clock synchronisation in M2 is for; until then,
//! cross-machine latency figures are only meaningful up to that offset.

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current time in microseconds since the Unix epoch.
///
/// Saturates to zero if the system clock is set before the epoch, which keeps the
/// capture path free of error handling for a case that cannot occur in practice.
///
/// # Examples
///
/// ```
/// # use prism_core::clock::now_us;
/// let before = now_us();
/// let after = now_us();
/// assert!(after >= before);
/// ```
#[must_use]
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}
