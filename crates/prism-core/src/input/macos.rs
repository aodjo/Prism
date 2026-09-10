//! Injecting input on macOS through CoreGraphics events.
//!
//! **This requires Accessibility permission.** Without it `CGEventPost` silently does
//! nothing — no error, no event — which is a far worse failure than a refusal, so the
//! injector checks at startup and reports it.
//!
//! Two pieces of state have to be tracked rather than derived. The pointer position,
//! because the wire can carry relative motion and CoreGraphics wants an absolute point —
//! and an absolute place, when the wire sends one, is posted as the motion to it; and
//! the modifier keys, because a synthesised key event does not pick up the shift that a
//! previous synthesised event pressed, so every event has to be told which modifiers are
//! held.

use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton,
};

use crate::input::{HeldKeys, Injector, InputError, PointerSample};
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
    keys: HeldKeys,
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
        );

        // A press is remembered only once its event exists, and a release is forgotten
        // whether or not one could be built: a key-down that never went out must not be
        // released later, and a key-up the system will not build is not worth attempting
        // again on every cleanup for the rest of the session.
        self.keys.set(usage, pressed && event.is_ok());

        let event = event?;

        {
            CGEvent::set_flags(Some(&event), self.flags);
            CGEvent::post(TAP, Some(&event));
        }

        Ok(())
    }

    /// Returns the keys the injector is holding down.
    ///
    /// Public so a caller can see whether anything is still held, and so the release path
    /// can be tested without a machine to press keys on.
    #[must_use]
    pub fn held_keys(&self) -> HeldKeys {
        self.keys
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
            keys: HeldKeys::default(),
            flags: CGEventFlags::empty(),
        })
    }

    fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { dx, dy } => self.move_pointer(f64::from(dx), f64::from(dy)),
            InputEvent::MouseButton { button, pressed } => self.press_button(button, pressed),
            InputEvent::MouseScroll { dx, dy } => self.scroll(dx, dy),
            InputEvent::Key { usage, pressed } => self.press_key(usage, pressed),
            // Posted as the motion from here to there, so everything a relative move gets —
            // a drag while a button is held, the delta a game reads — an absolute one gets
            // too, and the position this keeps ends up where the client pointed.
            InputEvent::MouseTo { x, y } => {
                let target = place_on(self.bounds, x, y);

                self.move_pointer(target.x - self.position.x, target.y - self.position.y)
            }
        }
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        // The held set is copied out first because releasing a key edits it, and because
        // an empty set is the state to leave behind even if a post fails: a key this
        // machine will not release is not a key worth trying again on the next cleanup.
        let held = self.keys;
        let mut outcome = Ok(());

        // Buttons go up first, while the modifiers they were pressed under are still down,
        // which is the order a person letting go of a shift-click produces.
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            if self.buttons[button as usize] {
                outcome = outcome.and(self.press_button(button, false));
            }
        }

        // `and` keeps the first failure while still evaluating the rest, because the whole
        // point is that every key comes up.
        for usage in held.iter() {
            outcome = outcome.and(self.press_key(usage, false));
        }

        // A modifier the table refused to release would otherwise leave its flag set on
        // every event this injector posts for the rest of the session.
        self.flags = CGEventFlags::empty();

        outcome
    }

    fn injection_is_landing(&self) -> bool {
        let Some(actual) = system_pointer() else {
            return false;
        };

        (actual.x - self.position.x).abs() < 2.0 && (actual.y - self.position.y).abs() < 2.0
    }
}

/// Returns the point on a display a fraction of the way across and down it.
///
/// Zero is the first column and row and 65535 the last, so both edges can be reached. Scaling
/// by the full width instead would put the far edge one point off the screen, where the pointer
/// is clamped back to a place the client did not point at.
fn place_on(bounds: CGRect, x: u16, y: u16) -> CGPoint {
    let across = (bounds.size.width - 1.0).max(0.0);
    let down = (bounds.size.height - 1.0).max(0.0);

    CGPoint {
        x: bounds.origin.x + across * f64::from(x) / f64::from(u16::MAX),
        y: bounds.origin.y + down * f64::from(y) / f64::from(u16::MAX),
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
/// Every number on the right is a `kVK_` constant from `<HIToolbox/Events.h>`, written in
/// decimal because that is what `CGEvent` takes.
///
/// Four things a keyboard has are refused rather than guessed, because a wrong key is
/// worse than no key:
///
/// - **Non-US hash and non-US backslash.** macOS decides which of virtual keys 10 and 50
///   is the grave and which is the extra key from the *type* of keyboard it believes is
///   attached, so either choice types the wrong character on half of them.
/// - **Power.** It is a hardware signal on this machine, not something an event posts.
/// - **F13 through F20.** The first three of them are the same physical keys as Print
///   Screen, Scroll Lock and Pause, which claim those virtual keys above; a table cannot
///   answer to both names at once and the PC names are the ones a client sends.
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

        // A Mac has no key called Print Screen, Scroll Lock, Pause or Insert. It has the
        // keys those sit on: F13, F14, F15 and Help, which is what Apple's own USB driver
        // hands a PC keyboard's four, and what an application on this machine will see.
        0x46 => 105, // print screen (F13)
        0x47 => 107, // scroll lock (F14)
        0x48 => 113, // pause (F15)
        0x49 => 114, // insert (help)

        0x4A => 115, // home
        0x4B => 116, // page up
        0x4C => 117, // forward delete
        0x4D => 119, // end
        0x4E => 121, // page down

        0x4F => 124, // right arrow
        0x50 => 123, // left arrow
        0x51 => 125, // down arrow
        0x52 => 126, // up arrow

        // Num Lock is Clear on a Mac keypad — the same key, doing what that keypad does
        // with it. The keypad digits are deliberately separate from the number row: they
        // carry their own virtual keys, and a game that binds keypad 4 does not want the 4
        // above the letters.
        0x53 => 71, // num lock (clear)
        0x54 => 75, // keypad divide
        0x55 => 67, // keypad multiply
        0x56 => 78, // keypad minus
        0x57 => 69, // keypad plus
        0x58 => 76, // keypad enter
        0x59 => 83, // keypad 1
        0x5A => 84, // keypad 2
        0x5B => 85, // keypad 3
        0x5C => 86, // keypad 4
        0x5D => 87, // keypad 5
        0x5E => 88, // keypad 6
        0x5F => 89, // keypad 7
        0x60 => 91, // keypad 8
        0x61 => 92, // keypad 9
        0x62 => 82, // keypad 0
        0x63 => 65, // keypad decimal

        0x65 => 110, // application
        0x67 => 81,  // keypad equals

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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::{MacInjector, hid_to_virtual_key, place_on};
    use crate::input::{Injector, InputError};
    use crate::net::packet::InputEvent;

    #[test]
    fn a_fraction_of_the_screen_lands_on_the_point_it_names() {
        // The size the host in the virtual machine reports, and an origin that is not zero,
        // because a main display is not always the one the coordinates start at.
        let bounds = CGRect {
            origin: CGPoint { x: 100.0, y: 50.0 },
            size: CGSize {
                width: 1728.0,
                height: 966.0,
            },
        };

        let first = place_on(bounds, 0, 0);
        assert_eq!((first.x, first.y), (100.0, 50.0));

        let last = place_on(bounds, u16::MAX, u16::MAX);
        assert_eq!(
            (last.x, last.y),
            (1827.0, 1015.0),
            "the far corner is reachable"
        );

        let middle = place_on(bounds, u16::MAX / 2, u16::MAX / 2);
        assert!((middle.x - (100.0 + 863.5)).abs() < 0.1, "{middle:?}");
        assert!((middle.y - (50.0 + 482.5)).abs() < 0.1, "{middle:?}");
    }

    #[test]
    fn the_navigation_cluster_reaches_the_keys_it_names() {
        assert_eq!(hid_to_virtual_key(0x49), Some(114), "insert, labelled help");
        assert_eq!(hid_to_virtual_key(0x4A), Some(115), "home");
        assert_eq!(hid_to_virtual_key(0x4B), Some(116), "page up");
        assert_eq!(hid_to_virtual_key(0x4C), Some(117), "forward delete");
        assert_eq!(hid_to_virtual_key(0x4D), Some(119), "end");
        assert_eq!(hid_to_virtual_key(0x4E), Some(121), "page down");
    }

    #[test]
    fn forward_delete_is_not_backspace() {
        // The two are one letter apart in the usage table and a world apart to whoever is
        // typing: 0x2A rubs out what is behind the caret, 0x4C what is in front of it.
        assert_eq!(hid_to_virtual_key(0x2A), Some(51), "backspace");
        assert_eq!(hid_to_virtual_key(0x4C), Some(117), "forward delete");
    }

    #[test]
    fn print_screen_scroll_lock_and_pause_land_on_the_keys_a_mac_has_for_them() {
        assert_eq!(hid_to_virtual_key(0x46), Some(105), "F13");
        assert_eq!(hid_to_virtual_key(0x47), Some(107), "F14");
        assert_eq!(hid_to_virtual_key(0x48), Some(113), "F15");
    }

    #[test]
    fn the_keypad_is_complete() {
        let mut seen = HashSet::new();

        for usage in 0x53..=0x63u16 {
            let key = hid_to_virtual_key(usage)
                .unwrap_or_else(|| panic!("keypad key {usage:#04x} has no macOS key"));
            assert!(
                seen.insert(key),
                "keypad key {usage:#04x} collides on {key}"
            );
        }

        assert_eq!(seen.len(), 17, "num lock, five operators and eleven keys");
    }

    #[test]
    fn the_keypad_digits_are_not_the_number_row() {
        // A game that binds keypad 4 does not want the 4 above the letters, and macOS
        // gives the two rows entirely separate virtual keys, so nothing here may collide.
        for (row, keypad) in [
            (0x27u16, 0x62u16), // 0
            (0x1E, 0x59),       // 1
            (0x21, 0x5C),       // 4
            (0x26, 0x61),       // 9
        ] {
            assert_ne!(
                hid_to_virtual_key(row),
                hid_to_virtual_key(keypad),
                "{row:#04x} and {keypad:#04x} are different keys"
            );
        }
    }

    #[test]
    fn nothing_in_the_whole_table_collides() {
        let mut seen = HashSet::new();

        for usage in 0..=0xFFu16 {
            let Some(key) = hid_to_virtual_key(usage) else {
                continue;
            };
            assert!(
                seen.insert(key),
                "usage {usage:#04x} maps to {key}, which another key already claims"
            );
        }
    }

    #[test]
    fn what_the_table_cannot_answer_for_is_still_refused() {
        assert_eq!(hid_to_virtual_key(0x00), None, "reserved");
        assert_eq!(
            hid_to_virtual_key(0x32),
            None,
            "non-US hash, whose virtual key depends on the keyboard type"
        );
        assert_eq!(
            hid_to_virtual_key(0x64),
            None,
            "non-US backslash, the other half of that ambiguity"
        );
        assert_eq!(hid_to_virtual_key(0x66), None, "power is not an event");
        assert_eq!(
            hid_to_virtual_key(0x68),
            None,
            "F13, which print screen already claims"
        );
        assert_eq!(hid_to_virtual_key(0xFF), None, "beyond the table");
    }

    #[test]
    fn releasing_everything_lets_go_of_every_key_that_was_held() {
        let mut injector = match MacInjector::new() {
            Ok(injector) => injector,
            Err(InputError::PermissionDenied) => {
                eprintln!("skipping: this process has no Accessibility permission");
                return;
            }
            Err(err) => panic!("could not create an injector: {err}"),
        };

        // Modifiers and F13, which are the keys that change nothing on the machine running
        // the test. Injecting a letter here would type it into whatever window has focus.
        for usage in [0xE1u16, 0xE2, 0x46] {
            injector
                .inject(InputEvent::Key {
                    usage,
                    pressed: true,
                })
                .expect("a mapped key is always injectable");
        }

        assert_eq!(injector.held_keys().len(), 3, "three keys are down");

        injector.release_all().expect("releasing is the same call");

        assert!(
            injector.held_keys().is_empty(),
            "a key still held after a release is one stuck on the host for the session"
        );
        assert!(
            injector.flags.is_empty(),
            "and its modifier flag would ride on every event after it"
        );
    }

    #[test]
    fn releasing_nothing_is_not_an_error() {
        let mut injector = match MacInjector::new() {
            Ok(injector) => injector,
            Err(InputError::PermissionDenied) => {
                eprintln!("skipping: this process has no Accessibility permission");
                return;
            }
            Err(err) => panic!("could not create an injector: {err}"),
        };

        assert_eq!(injector.release_all(), Ok(()));
        assert!(injector.held_keys().is_empty());
    }

    #[test]
    fn a_key_the_table_refuses_is_never_recorded_as_held() {
        let mut injector = match MacInjector::new() {
            Ok(injector) => injector,
            Err(InputError::PermissionDenied) => {
                eprintln!("skipping: this process has no Accessibility permission");
                return;
            }
            Err(err) => panic!("could not create an injector: {err}"),
        };

        let refused = injector.inject(InputEvent::Key {
            usage: 0x66,
            pressed: true,
        });

        assert!(refused.is_err(), "power has no key on this machine");
        assert!(
            injector.held_keys().is_empty(),
            "a key that was never pressed must not be released later"
        );
    }
}
