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
    /// Step through what this machine's keyboard and pointer are doing to the far one.
    Hands,
    /// Take the window down one step: out of full screen, or into the Dock.
    Shrink,
    /// Fill the screen, and leave it again.
    Fullscreen,
    /// Send a file to the machine being watched.
    Send,
    /// Ask the machine being watched what it is offering.
    Fetch,
    /// Show what the session is doing, or put it away again.
    Stats,
    /// End the session.
    Disconnect,
}

/// What this machine's keyboard and pointer are doing to the far one.
///
/// The same three stops as on macOS, because the window loop is one loop: only the picture of
/// them in a title bar is missing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hands {
    /// Nothing crosses. The far machine is a picture.
    Watching,
    /// Keys and clicks cross, and the pointer points at the picture from this side.
    Controlling,
    /// The pointer is caged here and crosses as movement, which is what a game reads.
    Aiming,
}

impl Hands {
    /// The next stop along, wrapping back to watching.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Hands::Watching => Hands::Controlling,
            Hands::Controlling => Hands::Aiming,
            Hands::Aiming => Hands::Watching,
        }
    }

    /// Whether anything this machine does crosses to the other one.
    #[must_use]
    pub fn sends(self) -> bool {
        !matches!(self, Hands::Watching)
    }

    /// Whether the pointer is caged here and sent as movement.
    #[must_use]
    pub fn caged(self) -> bool {
        matches!(self, Hands::Aiming)
    }
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
    pub fn set_hands(&self, hands: Hands) {
        let _ = hands;
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

    /// Returns `false`, because nothing here keeps track of the window.
    #[must_use]
    pub fn sync_fullscreen(&self) -> bool {
        false
    }

    /// Does nothing, because there is no window held here to put anywhere.
    pub fn miniaturize(&self) {}
}
