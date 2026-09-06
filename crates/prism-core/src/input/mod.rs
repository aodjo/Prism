//! Injecting the client's input on the host.
//!
//! This is the one part of the pipeline where latency is felt rather than seen. Video
//! arriving a few milliseconds late looks the same; a cursor that answers a few
//! milliseconds late feels wrong immediately. So input takes the shortest path there is:
//! no pacing, no batching, sent the instant it happens and injected the instant it lands.
//!
//! Each platform injects through its own API, and the differences run deeper than the
//! call names — macOS has to be granted permission and track its own pointer and modifier
//! state, Windows has neither problem but needs scan codes instead of key codes. The
//! [`Injector`] trait is what they have in common, and [`PlatformInjector`] resolves to
//! whichever one this build targets, so nothing above this module is written twice.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

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

    /// The system accepted the call and inserted nothing.
    ///
    /// Windows reports refusal this way rather than through a permission check, so the
    /// code is the only thing that separates the causes. `5`, access denied, is User
    /// Interface Privilege Isolation refusing a process of lower integrity than the
    /// foreground window. A process with no interactive desktop at all — a service, or an
    /// SSH login — has nothing to inject into and fails here too.
    #[error("the system refused the event (Win32 error {code})")]
    Refused {
        /// The Win32 error code the failed call left behind.
        code: u32,
    },

    /// This build has no injector for the platform it is running on.
    #[error("input injection is not implemented on this platform yet")]
    Unsupported,
}

/// Puts input events onto the machine the host is running on.
///
/// Dispatch is static — [`PlatformInjector`] is a concrete type, not a trait object — so
/// the trait costs nothing at runtime. It exists to keep the platform implementations the
/// same shape as each other, which a type alias alone would not enforce.
pub trait Injector: Sized {
    /// Creates an injector for this machine.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::PermissionDenied`] if the platform requires permission to
    /// control the machine and it has not been granted, [`InputError::Unsupported`] on a
    /// platform with no implementation, and [`InputError::Inject`] if the platform will
    /// not set up whatever injection needs.
    fn new() -> Result<Self, InputError>;

    /// Injects one event.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::Inject`] if the event cannot be built, which includes keys
    /// this build cannot map, and [`InputError::Refused`] if the system takes the call
    /// and does nothing with it.
    fn inject(&mut self, event: InputEvent) -> Result<(), InputError>;

    /// Returns whether injected events are reaching the system.
    ///
    /// Worth checking once, after a few events have gone out. Both platforms can accept
    /// an event and do nothing with it, for different reasons, and neither says so on the
    /// call that failed — so this is separate from [`Injector::inject`] rather than part
    /// of it. It answers about the recent past, not the last call.
    fn injection_is_landing(&self) -> bool;
}

use crate::net::packet::InputEvent;

/// Where the pointer is on this machine, and how big the screen holding it is.
///
/// The screen travels with the position because the two are only meaningful together: the
/// client scales the position into its own window, and it cannot do that against a screen
/// size it has to remember from an earlier message that may never have arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerSample {
    /// Pixels from the left of the primary display.
    pub x: u16,
    /// Pixels from the top of the primary display.
    pub y: u16,
    /// Width of the primary display in pixels; never zero.
    pub screen_width: u16,
    /// Height of the primary display in pixels; never zero.
    pub screen_height: u16,
}

/// Reads where the pointer is on this machine.
///
/// Deliberately not a method on [`Injector`]. Reading the pointer has nothing to do with
/// injecting: the host samples it every frame from the thread that sends video, which owns
/// no injector, and the answer has to include movement the host's own user made rather than
/// only what this process injected.
///
/// Returns `None` on a platform with no implementation, and on any platform that will not
/// say — a session with no desktop has no pointer to report.
#[must_use]
pub fn pointer() -> Option<PointerSample> {
    #[cfg(target_os = "macos")]
    {
        macos::pointer()
    }

    #[cfg(target_os = "windows")]
    {
        windows::pointer()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// The injector for the platform this build targets.
#[cfg(target_os = "macos")]
pub type PlatformInjector = macos::MacInjector;

/// The injector for the platform this build targets.
#[cfg(target_os = "windows")]
pub type PlatformInjector = windows::WindowsInjector;

/// The injector for the platform this build targets.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub type PlatformInjector = Unsupported;

/// Stands in for an injector on a platform that has none yet.
///
/// It fails at construction rather than accepting events and discarding them, so a host
/// started on such a platform says so once at startup instead of looking like it works.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub struct Unsupported;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl Injector for Unsupported {
    fn new() -> Result<Self, InputError> {
        Err(InputError::Unsupported)
    }

    fn inject(&mut self, _event: InputEvent) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }

    fn injection_is_landing(&self) -> bool {
        false
    }
}
