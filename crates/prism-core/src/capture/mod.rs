//! Screen capture.
//!
//! Every platform has its own way to hand a compositor's output to another process, and
//! all of them are configured towards the same end: deliver frames in the encoder's
//! native pixel format, in GPU memory, without a trip through system memory, and without
//! the compositor waiting on the consumer.

#[cfg(target_os = "macos")]
pub mod screencapturekit;
#[cfg(target_os = "windows")]
pub mod wgc;

/// How a capture session should be configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Frames per second to request. The compositor may deliver fewer when nothing moves.
    pub fps: u32,
    /// The widest frames may be, or zero for no limit.
    ///
    /// A limit rather than a size: frames keep the display's shape and fit inside it, so a
    /// box of another shape costs a little of one side rather than squashing the picture. The
    /// compositor scales for free while it is already touching the pixels, and the session
    /// sets this to what the client is showing so nothing is sent that nobody sees.
    ///
    /// Windows Graphics Capture has no such scaling, so on Windows this is not applied and
    /// frames are the monitor's own size.
    pub width: u32,
    /// The tallest frames may be, or zero for no limit.
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

/// Returns the largest size with a screen's shape that fits inside a box.
///
/// Never larger than the screen itself, because pixels a screen does not have are pixels the
/// encoder would spend its bitrate inventing. Even in both directions, because every codec here
/// subsamples colour by two. A zero on either side of the box means that side is not limited.
///
/// # Examples
///
/// ```
/// # use prism_core::capture::fit_within;
/// // A Retina screen shown in a window of another shape: the width fills, the height follows.
/// assert_eq!(fit_within((3456, 1932), (2560, 1504)), (2560, 1430));
/// // A box larger than the screen asks for nothing more than the screen has.
/// assert_eq!(fit_within((1920, 1080), (65534, 65534)), (1920, 1080));
/// ```
#[must_use]
pub fn fit_within(screen: (u32, u32), limit: (u32, u32)) -> (u32, u32) {
    let (width, height) = (screen.0.max(2), screen.1.max(2));
    let across = if limit.0 == 0 { width } else { limit.0 };
    let down = if limit.1 == 0 { height } else { limit.1 };

    if width <= across && height <= down {
        return (width & !1, height & !1);
    }

    let scale = f64::min(
        f64::from(across) / f64::from(width),
        f64::from(down) / f64::from(height),
    );

    (
        ((f64::from(width) * scale) as u32).max(2) & !1,
        ((f64::from(height) * scale) as u32).max(2) & !1,
    )
}

#[cfg(test)]
mod tests {
    use super::fit_within;

    #[test]
    fn a_screen_keeps_its_shape_inside_a_box_of_another() {
        let (width, height) = fit_within((3456, 1932), (2560, 1504));

        assert_eq!(width, 2560);
        assert!(height <= 1504);

        let screen = 3456.0 / 1932.0;
        let frame = f64::from(width) / f64::from(height);
        assert!((screen - frame).abs() < 0.01, "{screen} against {frame}");
    }

    #[test]
    fn a_tall_box_limits_the_height_instead() {
        let (width, height) = fit_within((1920, 1080), (1920, 600));

        assert_eq!(height, 600);
        assert!(width < 1920);
    }

    #[test]
    fn no_limit_is_the_screen_itself() {
        assert_eq!(fit_within((3456, 1932), (0, 0)), (3456, 1932));
        assert_eq!(
            fit_within((1729, 967), (0, 0)),
            (1728, 966),
            "even, for the codec"
        );
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
