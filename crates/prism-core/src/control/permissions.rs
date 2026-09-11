//! What the system has to allow before a host can do its job.
//!
//! Both of these fail quietly when they are missing, which is the whole reason this module
//! exists. A screen capture without the grant starts, delivers frames on schedule, and fills
//! every one with black; system audio does the same with silence. Injection without the grant
//! posts events that go nowhere and reports success. Nothing raises an error, so a host that
//! did not ask first looks like a host that is working and is not.
//!
//! Asking is therefore something that happens before a session rather than during one, and
//! the answer belongs where a person can see it.
//!
//! # Requesting is not the same as granting
//!
//! The system prompts once. After that a request returns the same refusal forever and shows
//! nothing, because the decision now lives in Privacy settings and only a person can change
//! it there. So anything asking on a person's behalf has to be prepared to send them to the
//! settings pane instead, which is what [`Grant::settings_url`] is for.

use core::fmt;

/// One thing the system can allow or refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Recording the screen, which also covers capturing the machine's audio.
    Screen,
    /// Controlling the machine, so a client's keyboard and mouse reach it.
    Input,
}

impl Grant {
    /// Returns what to call this when explaining it to a person.
    ///
    /// The name the system's own settings use, so that what somebody is told to look for is
    /// what they will see when they get there.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Grant::Screen => "Screen Recording",
            Grant::Input => "Accessibility",
        }
    }

    /// Returns why a host needs it, in one sentence.
    #[must_use]
    pub fn purpose(self) -> &'static str {
        match self {
            Grant::Screen => "to send this machine's screen and sound",
            Grant::Input => "to let the other machine's keyboard and mouse control this one",
        }
    }

    /// Returns a link that opens the settings pane holding this grant.
    ///
    /// Opening it is left to the caller, because the application already knows how to open a
    /// link and the core has no business starting processes.
    #[must_use]
    pub fn settings_url(self) -> &'static str {
        match self {
            Grant::Screen => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Grant::Input => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
        }
    }
}

impl fmt::Display for Grant {
    /// Writes the grant's name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What this machine currently allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Whether the screen may be recorded.
    pub screen: bool,
    /// Whether this machine may be controlled.
    pub input: bool,
}

impl Permissions {
    /// Returns what is still missing for a host with these settings.
    ///
    /// `controlling` says whether the session will accept the client's input. A host that only
    /// shows its screen needs nothing from Accessibility, and asking for it anyway is asking
    /// for control of a machine on behalf of somebody who did not want to give it.
    #[must_use]
    pub fn missing(self, controlling: bool) -> Vec<Grant> {
        let mut missing = Vec::new();

        if !self.screen {
            missing.push(Grant::Screen);
        }
        if controlling && !self.input {
            missing.push(Grant::Input);
        }

        missing
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};

    use super::{Grant, Permissions};

    // SAFETY: `AXIsProcessTrusted` and `AXIsProcessTrustedWithOptions` are stable
    // Accessibility entry points that report and optionally ask for the trust state, and
    // `kAXTrustedCheckOptionPrompt` is the constant key the second one reads. They live in
    // ApplicationServices, which is named because nothing else here pulls it in.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C-unwind" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: Option<&CFDictionary>) -> bool;
        static kAXTrustedCheckOptionPrompt: Option<&'static CFString>;
    }

    /// Asks for Accessibility, which is also what puts this copy of the application on the list.
    ///
    /// Reading the trust state is not enough. The list in System Settings holds an entry per
    /// signed copy, and an entry made by an earlier build — signed another way — stays switched
    /// on while this build is refused, so turning it on does nothing. Asking with the prompt is
    /// what adds this build to the list, where switching it on counts; clearing an entry an
    /// earlier build left is the caller's, since it means starting a process.
    fn ask_for_input() -> bool {
        // SAFETY: a constant the framework defines, read once it has been linked.
        let Some(prompt) = (unsafe { kAXTrustedCheckOptionPrompt }) else {
            // SAFETY: takes no arguments and only reports the trust state.
            return unsafe { AXIsProcessTrusted() };
        };

        let options =
            CFDictionary::<CFString, CFBoolean>::from_slices(&[prompt], &[CFBoolean::new(true)]);

        // SAFETY: the dictionary is a valid CFDictionary for the length of the call, keyed by
        // the constant the function documents.
        unsafe { AXIsProcessTrustedWithOptions(Some((*options).as_ref())) }
    }

    /// Returns what this machine currently allows, without prompting for anything.
    pub fn check() -> Permissions {
        Permissions {
            // Preflight rather than request: this is called to draw an interface, and drawing
            // an interface is not a reason to put a system dialog in front of somebody.
            screen: objc2_core_graphics::CGPreflightScreenCaptureAccess(),
            // SAFETY: takes no arguments and only reports the trust state.
            input: unsafe { AXIsProcessTrusted() },
        }
    }

    /// Asks the system for one grant, and reports whether it is now held.
    ///
    /// The prompt appears once in the life of an application. Afterwards this returns the
    /// standing answer and shows nothing, so a caller that gets `false` should offer
    /// [`Grant::settings_url`] rather than asking again.
    pub fn request(grant: Grant) -> bool {
        match grant {
            Grant::Screen => objc2_core_graphics::CGRequestScreenCaptureAccess(),
            Grant::Input => ask_for_input(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::kAXTrustedCheckOptionPrompt;

        #[test]
        fn the_prompt_is_asked_for_under_the_key_the_framework_reads() {
            // Read, never used: asking would put a system dialog in front of whoever runs this.
            // SAFETY: a constant the framework defines, read once it has been linked.
            let key = unsafe { kAXTrustedCheckOptionPrompt }.expect("the framework defines it");

            assert_eq!(key.to_string(), "AXTrustedCheckOptionPrompt");
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{Grant, Permissions};

    /// Returns that everything is allowed, on a system with nothing to ask.
    ///
    /// Windows and Linux gate none of this at startup. Windows refuses an injected event at
    /// the moment it is sent, when the foreground window outranks the sender, and that is
    /// reported where it happens rather than here.
    pub fn check() -> Permissions {
        Permissions {
            screen: true,
            input: true,
        }
    }

    /// Reports that the grant is held, there being nothing to ask for.
    pub fn request(grant: Grant) -> bool {
        let _ = grant;
        true
    }
}

/// Returns what this machine currently allows, without prompting for anything.
///
/// Safe to call as often as an interface redraws.
#[must_use]
pub fn check() -> Permissions {
    platform::check()
}

/// Asks the system for one grant, and reports whether it is now held.
///
/// The system prompts at most once per application. A `false` afterwards means the answer is
/// standing and only a person can change it, in the pane [`Grant::settings_url`] names.
#[must_use]
pub fn request(grant: Grant) -> bool {
    platform::request(grant)
}
