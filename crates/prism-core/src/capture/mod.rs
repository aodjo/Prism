//! Screen capture.
//!
//! Every platform has its own way to hand a compositor's output to another process, and
//! all of them are configured towards the same end: deliver frames in the encoder's
//! native pixel format, in GPU memory, without a trip through system memory, and without
//! the compositor waiting on the consumer.

#[cfg(target_os = "macos")]
pub mod screencapturekit;

/// How a capture session should be configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Frames per second to request. The compositor may deliver fewer when nothing moves.
    pub fps: u32,
    /// Width to scale frames to, or zero for the display's native width.
    ///
    /// A Retina display is far larger than anything worth streaming at frame rate, and
    /// the compositor scales for free while it is already touching the pixels.
    pub width: u32,
    /// Height to scale frames to, or zero for the display's native height.
    pub height: u32,
    /// Draw the cursor into the captured frames.
    ///
    /// Off by default, because the client draws its own cursor at its native refresh rate.
    /// A cursor baked into the video inherits the video's latency, and a cursor that lags
    /// is the single most noticeable part of a remote desktop.
    pub show_cursor: bool,
    /// How many frames the compositor may buffer before it drops the oldest.
    ///
    /// Small on purpose. A deep queue means the capture side absorbs a stall by handing
    /// over stale frames rather than dropping them.
    pub queue_depth: usize,
}

impl Default for CaptureConfig {
    /// Returns a configuration for 60 fps with a client-drawn cursor.
    fn default() -> Self {
        Self {
            fps: 60,
            width: 0,
            height: 0,
            show_cursor: false,
            queue_depth: 3,
        }
    }
}

/// Reason a capture session could not start or could not deliver frames.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// The user has not granted permission to record the screen.
    ///
    /// On macOS this is the Screen Recording entry in Privacy settings. The prompt only
    /// appears once; after a refusal the application must be added by hand, which is why
    /// this is reported distinctly rather than as a generic failure.
    #[error("screen recording permission has not been granted")]
    PermissionDenied,

    /// No display was available to capture.
    #[error("no display was available to capture")]
    NoDisplay,

    /// The platform refused to create or start the capture session.
    #[error("could not start capture: {reason}")]
    Start {
        /// What went wrong.
        reason: String,
    },
}
