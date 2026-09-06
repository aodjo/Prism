//! Injecting the client's input on the host.
//!
//! This is the one part of the pipeline where latency is felt rather than seen. Video
//! arriving a few milliseconds late looks the same; a cursor that answers a few
//! milliseconds late feels wrong immediately. So input takes the shortest path there is:
//! no pacing, no batching, sent the instant it happens and injected the instant it lands.

#[cfg(target_os = "macos")]
pub mod macos;

/// Reason an input event could not be injected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    /// The user has not granted permission to control the machine.
    ///
    /// On macOS this is the Accessibility entry in Privacy settings. Injection silently
    /// does nothing without it rather than failing, which is worse than an error, so the
    /// injector checks up front and says so.
    #[error("permission to control this machine has not been granted")]
    PermissionDenied,

    /// The platform refused to build or post an event.
    #[error("could not inject input: {reason}")]
    Inject {
        /// What went wrong.
        reason: &'static str,
    },
}
