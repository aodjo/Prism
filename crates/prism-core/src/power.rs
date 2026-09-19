//! Keeping a screen awake for as long as a session needs it.
//!
//! Both ends need it and for different reasons. A host whose display sleeps stops having a
//! picture to send — the compositor has nothing to hand the capture, so the far side is left
//! watching the last frame before the screen went dark, with everything still connected and
//! nothing reporting a fault. And a client shows a stream the way a video plays: for minutes at
//! a time with nobody touching the keyboard, which is exactly what an idle timer is counting.
//!
//! What is asked for is that the *display* stay on, not that the machine stay awake for work.
//! Somebody watching a screen wants the screen; a machine that also refused to idle would be a
//! machine spending power on nothing.

/// A reason a screen is being kept on, held for as long as the reason lasts.
///
/// Taking one asks the system to keep the display awake; dropping it takes the request back.
/// Nothing is reported when the system will not answer: a screen that sleeps is a worse session
/// and not a broken one, and there is nothing a person could do about it from here.
///
/// # Threads
///
/// On Windows the request belongs to the thread that made it, so one of these must be dropped
/// on the thread that created it. Everywhere else it belongs to the process and may be moved.
#[derive(Debug)]
pub struct Awake(
    // Never read, and that is the whole of its job: what it holds is given back when this is
    // dropped, so the request lasts exactly as long as somebody keeps the value.
    //
    // Allowed rather than expected, because whether the lint fires at all depends on the
    // target: only the platforms that have something to give back put a `Drop` on what is held
    // here, and an expectation is an error on the ones that do not.
    #[allow(
        dead_code,
        reason = "held for its drop, which is what ends the request"
    )]
    Held,
);

impl Awake {
    /// Asks the system to keep the display on until this is dropped.
    ///
    /// `reason` is what the system shows somebody asking why their screen is not sleeping, so
    /// it is a sentence about this session rather than the name of the program.
    #[must_use]
    pub fn hold(reason: &str) -> Self {
        Self(Held::take(reason))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_core_foundation::{CFRetained, CFString};

    /// What the system calls an assertion that is switched on.
    const ON: u32 = 255;

    /// The assertion that keeps the display awake without keeping the machine busy.
    const PREVENT_DISPLAY_SLEEP: &str = "PreventUserIdleDisplaySleep";

    // SAFETY: these are the two calls of IOKit's power management interface, declared as its
    // header declares them. `IOPMAssertionID` and `IOPMAssertionLevel` are both `uint32_t` and
    // `IOReturn` is a signed 32-bit status, which is ignored here for the reason the type's
    // documentation gives.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: *const CFString,
            level: u32,
            name: *const CFString,
            id: *mut u32,
        ) -> i32;

        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// The assertion, while it is held.
    #[derive(Debug)]
    pub struct Held(Option<u32>);

    impl Held {
        /// Takes the assertion out, or nothing if the system refused.
        pub fn take(reason: &str) -> Self {
            let kind = CFString::from_str(PREVENT_DISPLAY_SLEEP);
            let name = CFString::from_str(reason);
            let mut id = 0u32;

            // SAFETY: both strings live until this call returns, which is the whole of what the
            // function does with them — it copies what it keeps. The identifier is written only
            // when the call succeeds, which is what the status is checked for.
            let taken = unsafe {
                IOPMAssertionCreateWithName(
                    CFRetained::as_ptr(&kind).as_ptr(),
                    ON,
                    CFRetained::as_ptr(&name).as_ptr(),
                    &raw mut id,
                )
            };

            Self((taken == 0).then_some(id))
        }
    }

    impl Drop for Held {
        /// Gives the assertion back, letting the display sleep again.
        fn drop(&mut self) {
            if let Some(id) = self.0 {
                // SAFETY: the identifier came from a call that succeeded above and has not been
                // released before — this runs once, because `Drop` runs once.
                unsafe {
                    IOPMAssertionRelease(id);
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, SetThreadExecutionState,
    };

    /// The request, while it is held.
    ///
    /// Nothing to remember: the state belongs to the thread rather than to a handle, so what
    /// undoes it is asking for the plain continuous state back.
    #[derive(Debug)]
    pub struct Held;

    impl Held {
        /// Asks this thread's execution state to keep the display on.
        pub fn take(reason: &str) -> Self {
            let _ = reason;

            // SAFETY: the call takes flags and returns the previous state, and touches nothing
            // this process owns.
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED);
            }

            Self
        }
    }

    impl Drop for Held {
        /// Lets the display sleep again.
        fn drop(&mut self) {
            // SAFETY: as above.
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    /// Nothing, on a platform this has not been written for.
    #[derive(Debug)]
    pub struct Held;

    impl Held {
        /// Does nothing, and says so by holding nothing.
        pub fn take(reason: &str) -> Self {
            let _ = reason;

            Self
        }
    }
}

use platform::Held;
