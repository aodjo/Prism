//! The title bar controls, on a platform that has not grown them yet.
//!
//! Windows puts a toolbar in a window's frame differently enough that it is its own piece of
//! work, and until it is done this says so by having none. The window loop asks the same
//! questions on both platforms and gets `None` here, which is the answer for a window with no
//! controls in its title bar — not an error, and not a reason for the loop to know which
//! platform it is on.

#![cfg(not(target_os = "macos"))]

use sdl3::video::Window;

/// One control in the title bar.
///
/// Named here as on macOS so the window loop is one loop, and never built: with no controls,
/// nothing is ever pressed. Which is dead code by definition, and the one place it is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Tool {
    /// Hand the pointer and the keyboard to the machine being watched, or take them back.
    Control,
    /// Make the window the size of the picture arriving in it.
    Fit,
    /// Fill the screen, and leave it again.
    Fullscreen,
    /// Send a file to the machine being watched.
    Send,
    /// Ask the machine being watched what it is offering.
    Fetch,
    /// End the session.
    Disconnect,
}

/// The controls in a window's title bar, where there are any.
#[derive(Debug)]
pub struct Toolbar;

impl Toolbar {
    /// Returns `None`, because this platform has no controls to install.
    #[must_use]
    pub fn install(window: &Window) -> Option<Self> {
        let _ = window;

        None
    }

    /// Returns `None`, because nothing here can be pressed.
    #[must_use]
    pub fn pressed(&self) -> Option<Tool> {
        None
    }

    /// Does nothing, because there is no item to redraw.
    pub fn set_controlling(&self, controlling: bool) {
        let _ = controlling;
    }

    /// Returns `None`, because there is no menu to choose from.
    #[must_use]
    pub fn chosen(&self) -> Option<String> {
        None
    }

    /// Does nothing, because there is no control to open a menu under.
    pub fn offer(&self, files: &[(String, u64)], more: bool) {
        let _ = (files, more);
    }
}
