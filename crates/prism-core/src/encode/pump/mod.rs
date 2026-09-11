//! The screen's path from the compositor to the wire, in one place.
//!
//! Capture a frame, encode it, send the slices. Three lines to describe and five copies of it
//! had grown across this repository — one per host mode in the command line, one per platform
//! in the session service — which is how the window application came to be missing both of
//! M6's fixes while the command line had them. It was capped at the frame rate a serial
//! encoder gives, and it could not recover from a single lost frame, and nothing said so
//! because each copy looked correct on its own.
//!
//! This is that loop, once. What callers still differ about is what to do around it: how long
//! to run, what to count, what to print. So this owns the pipeline and hands back one frame at
//! a time, rather than owning the loop as well.
//!
//! # One name, two platforms
//!
//! [`ScreenPump`] is whichever of the platform implementations was compiled, and they answer
//! the same questions in the same words. That is what lets the session service and the command
//! line each hold a single loop rather than one per operating system — and it is why the
//! Windows half now recovers from a lost frame, which it did not when it was written out
//! separately and nobody was comparing.

/// The macOS pipeline: ScreenCaptureKit into VideoToolbox.
#[cfg(target_os = "macos")]
mod macos;

/// The Windows pipeline: Windows.Graphics.Capture into NVENC.
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
pub use macos::ScreenPump;
#[cfg(target_os = "windows")]
pub use windows::ScreenPump;

use std::time::Duration;

/// How many frames may be inside the encoder at once.
///
/// Two, where the encoder is asynchronous and will take a second frame while the first is
/// still being worked on. Submitting one and waiting for it leaves the hardware idle through
/// everything else the loop does, so the achievable rate is the sum rather than the larger of
/// the two. Measured at 1440p with HEVC on Apple Silicon: one in flight encodes 600 frames in
/// 9.05 s (66 fps), two in 3.05 s (197 fps), and the per-frame latency does not move
/// (p50 6.14 → 6.17 ms).
///
/// It stops at two because frames beyond that are queued rather than overlapped, and a queue
/// inside the encoder is latency with nobody's name on it: three in flight costs 3.83 ms of
/// p50, and six costs 15 ms. That is the trade the one-frame VBV exists to refuse.
///
/// A synchronous encoder ignores this and runs one at a time, because there is nothing to
/// overlap with.
pub const IN_FLIGHT: usize = 2;

/// How often a screen that is not changing is sent anyway.
///
/// No compositor here delivers frames for a still screen, so without this a session goes
/// silent the moment somebody stops moving: a client that connected to a motionless machine
/// would wait for a picture that only a mouse could produce, and one that lost a packet while
/// nothing was happening would keep the hole until something did.
///
/// Twice a second, because nothing in the picture changed and a frame that repeats one the
/// encoder has already seen is a few hundred bytes.
pub(crate) const REPEAT_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for the encoder before giving up on a frame.
///
/// Thirty times the measured encode latency at 1440p. Reaching it means the encoder has
/// stopped rather than fallen behind.
///
/// Only an asynchronous encoder has anything to wait for; a synchronous one has already
/// finished by the time it returns.
#[cfg(target_os = "macos")]
pub(crate) const ENCODE_TIMEOUT: Duration = Duration::from_millis(200);

/// What a pipeline is being asked to produce.
#[derive(Debug, Clone, Copy)]
pub struct PumpConfig {
    /// Frames per second to capture at.
    pub fps: u32,
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Width to scale frames to, or zero for the display's native width.
    ///
    /// A Retina display is far larger than anything worth streaming at frame rate, and the
    /// compositor scales for free while it is already touching the pixels.
    pub width: u32,
    /// Height to scale frames to, or zero for the display's native height.
    pub height: u32,
    /// The codec the two machines agreed on.
    ///
    /// Taken from the session rather than from anything decided earlier: a host that encodes
    /// one thing having agreed another produces a client that decodes nothing and reports no
    /// error, because a decoder waiting for parameter sets it will never see has nothing to
    /// complain about.
    pub codec: crate::net::negotiate::Codec,
}

/// What one turn of the pipeline produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pumped {
    /// A frame was captured, encoded and sent.
    Sent,
    /// A frame went in and the pipeline is still filling, so none came out yet.
    Filling,
    /// The screen is not changing and there was nothing left to send this turn.
    ///
    /// Ordinary, and not a fault: the last frame has already gone out and the next repeat of
    /// it is not due yet. A session spends most of a quiet minute here.
    Still,
    /// The compositor delivered nothing within the timeout, and there was nothing to repeat.
    ///
    /// A still screen is not this: no compositor here sends frames for one, so the pipeline
    /// sends the last frame again and reports what that produced. This is the narrower case
    /// of a session where no frame has ever arrived, which means the capture never started
    /// producing rather than that nothing is moving.
    Idle,
    /// A frame went in and nothing came out of the encoder in time.
    Dropped,
    /// The client is no longer there.
    ///
    /// A connected UDP socket learns this from the port-unreachable the far machine's kernel
    /// sends when nothing is listening any more, which arrives as a refused connection on the
    /// next write. It is how an ordinary disconnection looks from here, so it ends the session
    /// rather than failing it — a host that showed an error every time somebody closed their
    /// client would be showing an error after most sessions.
    PeerGone,
}

/// Whether a socket error means the client has gone rather than that something broke.
pub(crate) fn peer_gone(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
    )
}

/// Turns a failed send into either an ordinary ending or a real error.
///
/// The ordinary ending is said on standard error all the same, which the application keeps. It
/// ends the session for the machine watching too, and when that machine says the host vanished,
/// this line is what says which of the two let go first.
pub(crate) fn after_send(err: &std::io::Error) -> Result<Pumped, String> {
    if peer_gone(err) {
        eprintln!("host: a packet to the client was refused ({err}), so it is taken to have gone");

        Ok(Pumped::PeerGone)
    } else {
        Err(err.to_string())
    }
}
