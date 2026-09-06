//! Injecting input on macOS through CoreGraphics events.
//!
//! **This requires Accessibility permission.** Without it `CGEventPost` silently does
//! nothing — no error, no event — which is a far worse failure than a refusal, so the
//! injector checks at startup and reports it.
//!
//! Two pieces of state have to be tracked rather than derived. The pointer position,
//! because the wire carries relative motion and CoreGraphics wants an absolute point; and
//! the modifier keys, because a synthesised key event does not pick up the shift that a
//! previous synthesised event pressed, so every event has to be told which modifiers are
//! held.

use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton,
};

use crate::input::{Injector, InputError, PointerSample};
use crate::net::packet::{InputEvent, MouseButton};

/// Where injected events enter the system.
///
/// The HID tap is the lowest point available, so events look to applications as though
/// they came from real hardware. A session tap would be filtered differently by some
/// games.
const TAP: CGEventTapLocation = CGEventTapLocation::HIDEventTap;

/// Injects input events onto this machine.
pub struct MacInjector {
    source: objc2_core_foundation::CFRetained<CGEventSource>,
    bounds: CGRect,
    position: CGPoint,
    buttons: [bool; 3],
    flags: CGEventFlags,
}

impl MacInjector {
    /// Returns where the injector believes the pointer is.
    #[must_use]
    pub fn position(&self) -> (f64, f64) {
        (self.position.x, self.position.y)
    }

    /// Moves the pointer by a relative amount and posts the motion.
    ///
    /// The delta is written onto the event as well as being folded into the position,
    /// because a game reading raw pointer input wants the movement, not where the cursor
    /// ended up.
    fn move_pointer(&mut self, dx: f64, dy: f64) -> Result<(), InputError> {
        self.position.x = (self.position.x + dx).clamp(
            self.bounds.origin.x,
            self.bounds.origin.x + self.bounds.size.width - 1.0,
        );
        self.position.y = (self.position.y + dy).clamp(
            self.bounds.origin.y,
            self.bounds.origin.y + self.bounds.size.height - 1.0,
        );

        let (kind, button) = match self.held_button() {
            Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            Some(MouseButton::Middle) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };

        let event = CGEvent::new_mouse_event(Some(&self.source), kind, self.position, button)
            .ok_or(InputError::Inject {
                reason: "could not build a pointer event",
            })?;

        // A game reading raw pointer input wants the movement, not the destination.
        {
            CGEvent::set_integer_value_field(
                Some(&event),
                objc2_core_graphics::CGEventField::MouseEventDeltaX,
                dx as i64,
            );
            CGEvent::set_integer_value_field(
                Some(&event),
                objc2_core_graphics::CGEventField::MouseEventDeltaY,
                dy as i64,
            );
            CGEvent::set_flags(Some(&event), self.flags);
            CGEvent::post(TAP, Some(&event));
        }

        Ok(())
    }

    /// Presses or releases a pointer button.
    fn press_button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InputError> {
        self.buttons[button as usize] = pressed;

        let (kind, cg_button) = match (button, pressed) {
            (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (MouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (MouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        };

        let event = CGEvent::new_mouse_event(Some(&self.source), kind, self.position, cg_button)
            .ok_or(InputError::Inject {
                reason: "could not build a button event",
            })?;

        {
            CGEvent::set_flags(Some(&event), self.flags);
            CGEvent::post(TAP, Some(&event));
        }

        Ok(())
    }

    /// Posts a scroll wheel event.
    fn scroll(&mut self, dx: i16, dy: i16) -> Result<(), InputError> {
        let event = {
            CGEvent::new_scroll_wheel_event2(
                Some(&self.source),
                objc2_core_graphics::CGScrollEventUnit::Line,
                2,
                i32::from(-dy),
                i32::from(dx),
                0,
            )
        }
        .ok_or(InputError::Inject {
            reason: "could not build a scroll event",
        })?;

        {
            CGEvent::set_flags(Some(&event), self.flags);
            CGEvent::post(TAP, Some(&event));
        }

        Ok(())
    }

    /// Presses or releases a key identified by its USB HID usage code.
    fn press_key(&mut self, usage: u16, pressed: bool) -> Result<(), InputError> {
        let Some(key) = hid_to_virtual_key(usage) else {
            return Err(InputError::Inject {
                reason: "no macOS key matches that HID usage",
            });
        };

        if let Some(flag) = modifier_flag(usage) {
            self.flags.set(flag, pressed);
        }

        let event = CGEvent::new_keyboard_event(Some(&self.source), key, pressed).ok_or(
            InputError::Inject {
                reason: "could not build a key event",
            },
        )?;

        {
            CGEvent::set_flags(Some(&event), self.flags);
            CGEvent::post(TAP, Some(&event));
        }

        Ok(())
    }

    /// Returns the button being held, if exactly one is.
    ///
    /// Motion while a button is down has to be reported as a drag rather than a move, or
    /// the application never sees the drag.
    fn held_button(&self) -> Option<MouseButton> {
        if self.buttons[0] {
            Some(MouseButton::Left)
        } else if self.buttons[1] {
            Some(MouseButton::Right)
        } else if self.buttons[2] {
            Some(MouseButton::Middle)
        } else {
            None
        }
    }
}

impl core::fmt::Debug for MacInjector {
    /// Describes the injector's tracked state.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MacInjector")
            .field("position", &(self.position.x, self.position.y))
            .field("buttons", &self.buttons)
            .finish_non_exhaustive()
    }
}

// SAFETY: `AXIsProcessTrusted` is a stable Accessibility entry point that takes no
// arguments and only reports whether this process may control the machine. It lives in
// ApplicationServices, which nothing else here pulls in, so the framework is named.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C-unwind" {
    fn AXIsProcessTrusted() -> bool;
}

/// Returns where the system pointer actually is.
///
/// A fresh event with no source carries the current pointer location, which is the
/// cheapest way to ask.
impl Injector for MacInjector {
    fn new() -> Result<Self, InputError> {
        // SAFETY: the function takes no arguments and only reports the trust state.
        if !unsafe { AXIsProcessTrusted() } {
            return Err(InputError::PermissionDenied);
        }

        let source =
            CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or(InputError::Inject {
                reason: "could not create an event source",
            })?;

        let bounds = CGDisplayBounds(CGMainDisplayID());

        // Start from wherever the pointer already is rather than jumping it to the middle
        // of the screen the moment a session connects.
        let position = system_pointer().unwrap_or(CGPoint {
            x: bounds.origin.x + bounds.size.width / 2.0,
            y: bounds.origin.y + bounds.size.height / 2.0,
        });

        Ok(Self {
            source,
            bounds,
            position,
            buttons: [false; 3],
            flags: CGEventFlags::empty(),
        })
    }

    fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { dx, dy } => self.move_pointer(f64::from(dx), f64::from(dy)),
            InputEvent::MouseButton { button, pressed } => self.press_button(button, pressed),
            InputEvent::MouseScroll { dx, dy } => self.scroll(dx, dy),
            InputEvent::Key { usage, pressed } => self.press_key(usage, pressed),
        }
    }

    fn injection_is_landing(&self) -> bool {
        let Some(actual) = system_pointer() else {
            return false;
        };

        (actual.x - self.position.x).abs() < 2.0 && (actual.y - self.position.y).abs() < 2.0
    }
}

/// Reads where the system thinks the pointer is.
///
/// There is no direct call for this — the location rides on an event — so a null event is
/// created purely to be asked where it happened.
fn system_pointer() -> Option<CGPoint> {
    let event = CGEvent::new(None)?;
    Some(CGEvent::location(Some(&event)))
}

/// Reads the pointer position and the size of the display it sits on.
///
/// Coordinates are relative to the main display's own origin, so a pointer on a secondary
/// display to the left of it reports a negative position before clamping. Clamping is the
/// honest answer rather than a failure: the client is showing the main display, and a
/// pointer that has left it belongs at the edge it left by.
#[must_use]
pub fn pointer() -> Option<PointerSample> {
    let point = system_pointer()?;
    let bounds = CGDisplayBounds(CGMainDisplayID());

    Some(PointerSample {
        x: clamp_to_u16(point.x - bounds.origin.x, bounds.size.width),
        y: clamp_to_u16(point.y - bounds.origin.y, bounds.size.height),
        screen_width: clamp_dimension(bounds.size.width),
        screen_height: clamp_dimension(bounds.size.height),
    })
}

/// Clamps a coordinate into the display and onto the `u16` the wire carries.
fn clamp_to_u16(value: f64, extent: f64) -> u16 {
    let limit = f64::from(clamp_dimension(extent).saturating_sub(1));
    value.clamp(0.0, limit) as u16
}

/// Clamps a display extent into the `u16` the wire carries, never reaching zero.
///
/// A screen of no pixels is not a thing that exists, and the wire refuses to carry one, so
/// a degenerate reading becomes the smallest screen rather than an error the caller would
/// have to handle every frame.
fn clamp_dimension(extent: f64) -> u16 {
    if extent >= f64::from(u16::MAX) {
        return u16::MAX;
    }

    (extent as u16).max(1)
}

/// Returns the modifier flag a HID usage corresponds to, if it is a modifier at all.
///
/// Public so the mapping can be tested: a modifier that is injected but not recorded
/// leaves every later key missing its shift, which is the kind of fault that only shows
/// up as "capitals do not work".
pub fn modifier_flag(usage: u16) -> Option<CGEventFlags> {
    match usage {
        0xE0 | 0xE4 => Some(CGEventFlags::MaskControl),
        0xE1 | 0xE5 => Some(CGEventFlags::MaskShift),
        0xE2 | 0xE6 => Some(CGEventFlags::MaskAlternate),
        0xE3 | 0xE7 => Some(CGEventFlags::MaskCommand),
        _ => None,
    }
}

/// Maps a USB HID keyboard usage code to a macOS virtual key code.
///
/// The two numbering schemes have nothing in common — macOS's dates from the original
/// Macintosh keyboard and is laid out by physical position — so the table is explicit.
/// Covers the keys a remote session needs; anything else is refused rather than guessed,
/// because a wrong key is worse than no key.
pub fn hid_to_virtual_key(usage: u16) -> Option<u16> {
    let key = match usage {
        0x04 => 0,  // a
        0x05 => 11, // b
        0x06 => 8,  // c
        0x07 => 2,  // d
        0x08 => 14, // e
        0x09 => 3,  // f
        0x0A => 5,  // g
        0x0B => 4,  // h
        0x0C => 34, // i
        0x0D => 38, // j
        0x0E => 40, // k
        0x0F => 37, // l
        0x10 => 46, // m
        0x11 => 45, // n
        0x12 => 31, // o
        0x13 => 35, // p
        0x14 => 12, // q
        0x15 => 15, // r
        0x16 => 1,  // s
        0x17 => 17, // t
        0x18 => 32, // u
        0x19 => 9,  // v
        0x1A => 13, // w
        0x1B => 7,  // x
        0x1C => 16, // y
        0x1D => 6,  // z

        0x1E => 18, // 1
        0x1F => 19, // 2
        0x20 => 20, // 3
        0x21 => 21, // 4
        0x22 => 23, // 5
        0x23 => 22, // 6
        0x24 => 26, // 7
        0x25 => 28, // 8
        0x26 => 25, // 9
        0x27 => 29, // 0

        0x28 => 36, // return
        0x29 => 53, // escape
        0x2A => 51, // backspace
        0x2B => 48, // tab
        0x2C => 49, // space
        0x2D => 27, // minus
        0x2E => 24, // equal
        0x2F => 33, // left bracket
        0x30 => 30, // right bracket
        0x31 => 42, // backslash
        0x33 => 41, // semicolon
        0x34 => 39, // quote
        0x35 => 50, // grave
        0x36 => 43, // comma
        0x37 => 47, // period
        0x38 => 44, // slash
        0x39 => 57, // caps lock

        0x3A => 122, // F1
        0x3B => 120, // F2
        0x3C => 99,  // F3
        0x3D => 118, // F4
        0x3E => 96,  // F5
        0x3F => 97,  // F6
        0x40 => 98,  // F7
        0x41 => 100, // F8
        0x42 => 101, // F9
        0x43 => 109, // F10
        0x44 => 103, // F11
        0x45 => 111, // F12

        0x4F => 124, // right arrow
        0x50 => 123, // left arrow
        0x51 => 125, // down arrow
        0x52 => 126, // up arrow

        0xE0 => 59, // left control
        0xE1 => 56, // left shift
        0xE2 => 58, // left option
        0xE3 => 55, // left command
        0xE4 => 62, // right control
        0xE5 => 60, // right shift
        0xE6 => 61, // right option
        0xE7 => 54, // right command

        _ => return None,
    };

    Some(key)
}
