//! Injecting input on Linux, through the desktop portal.
//!
//! A Wayland desktop lets no client move the pointer or type into another client's window. The
//! remote desktop portal is the sanctioned way around that: the person at the machine agreed to
//! it when they first shared the screen, and the same session that carries the screen carries
//! the input. So this has no connection of its own — it sends through whichever session the
//! capture has open, and has nothing to send through when none is.
//!
//! What it sends are evdev codes, which is what the portal speaks: the numbers the kernel gives
//! a physical key or button, the same whatever keyboard layout is active. The wire carries USB
//! HID usages, which also name physical keys, so a key pressed over there is the key in the same
//! place here — the property the other two injectors keep with scan codes and key codes.

#![cfg(linux_desktop)]

use crate::capture::portal;
use crate::input::{HeldKeys, Injector, InputError};
use crate::net::packet::{InputEvent, MouseButton};

/// The largest position the wire carries, which is the far edge of the screen.
const EDGE: f64 = 65_535.0;

/// Injects input events into this desktop, while the portal has granted it.
#[derive(Debug, Default)]
pub struct LinuxInjector {
    landing: bool,
    buttons: [bool; 3],
    keys: HeldKeys,
}

impl LinuxInjector {
    /// Sends one event through the session that is open, if one is.
    fn send(
        &mut self,
        event: impl FnOnce(&portal::Input) -> Result<(), String>,
    ) -> Result<(), InputError> {
        let Some(input) = portal::input() else {
            self.landing = false;

            return Err(InputError::Inject {
                reason: "the desktop has not granted input to this session",
            });
        };

        match event(&input) {
            Ok(()) => {
                self.landing = true;

                Ok(())
            }
            Err(_) => {
                self.landing = false;

                Err(InputError::Inject {
                    reason: "the desktop portal refused the event",
                })
            }
        }
    }

    /// Presses or releases a pointer button.
    fn press_button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InputError> {
        self.buttons[button as usize] = pressed;

        let code = match button {
            MouseButton::Left => BTN_LEFT,
            MouseButton::Right => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
        };

        self.send(|input| input.button(code, pressed))
    }

    /// Presses or releases a key identified by its USB HID usage code.
    fn press_key(&mut self, usage: u16, pressed: bool) -> Result<(), InputError> {
        let Some(code) = hid_to_evdev(usage) else {
            return Err(InputError::Inject {
                reason: "this key has no evdev code",
            });
        };

        let outcome = self.send(|input| input.key(i32::from(code), pressed));

        // Remembered only once the portal has taken a press, and forgotten on any release: a key
        // that never went down must not be released later, and a release the portal refuses is
        // not worth trying again on every cleanup.
        self.keys.set(usage, pressed && outcome.is_ok());

        outcome
    }
}

impl Injector for LinuxInjector {
    fn new() -> Result<Self, InputError> {
        // Nothing to check yet: the session this sends through is opened by the capture, which
        // starts after this does. An event that arrives before it is refused one at a time.
        Ok(Self::default())
    }

    fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            // Relative, and `caged` changes nothing: the portal hands this to the compositor as
            // a mouse's own motion, so there is no position kept here to run into an edge.
            InputEvent::MouseMove { dx, dy, .. } => {
                self.send(|input| input.move_by(f64::from(dx), f64::from(dy)))
            }
            InputEvent::MouseTo { x, y } => {
                self.send(|input| input.move_to(f64::from(x) / EDGE, f64::from(y) / EDGE))
            }
            InputEvent::MouseButton { button, pressed } => self.press_button(button, pressed),
            // The wire's down and right are positive, and so are Wayland's.
            InputEvent::MouseScroll { dx, dy } => {
                self.send(|input| input.scroll(i32::from(dx), i32::from(dy)))
            }
            InputEvent::Key { usage, pressed } => self.press_key(usage, pressed),
        }
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        let held = self.keys;
        let mut outcome = Ok(());

        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            if self.buttons[button as usize] {
                outcome = outcome.and(self.press_button(button, false));
            }
        }

        for usage in held.iter() {
            outcome = outcome.and(self.press_key(usage, false));
        }

        outcome
    }

    fn injection_is_landing(&self) -> bool {
        self.landing
    }
}

/// `BTN_LEFT` from `linux/input-event-codes.h`.
const BTN_LEFT: i32 = 0x110;

/// `BTN_RIGHT`.
const BTN_RIGHT: i32 = 0x111;

/// `BTN_MIDDLE`.
const BTN_MIDDLE: i32 = 0x112;

/// Translates a USB HID keyboard usage into the evdev code for the same physical key.
///
/// The two numberings share nothing, so this is a table: letters in HID's alphabetical order
/// land on evdev's row-by-row keyboard order, which is why the codes for A to Z look random.
/// Every key a standard layout has is here, with the Japanese and Korean language keys; a usage
/// with no key behind it is `None` rather than a guess.
///
/// # Examples
///
/// ```
/// # use prism_core::input::linux::hid_to_evdev;
/// assert_eq!(hid_to_evdev(0x04), Some(30)); // A
/// assert_eq!(hid_to_evdev(0x28), Some(28)); // Enter
/// assert_eq!(hid_to_evdev(0xE3), Some(125)); // left meta
/// assert_eq!(hid_to_evdev(0x00), None);
/// ```
#[must_use]
pub fn hid_to_evdev(usage: u16) -> Option<u16> {
    /// A to Z, in HID order.
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];

    let code = match usage {
        0x04..=0x1D => LETTERS[usize::from(usage - 0x04)],
        // 1 to 9, then 0.
        0x1E..=0x26 => usage - 0x1E + 2,
        0x27 => 11,

        0x28 => 28, // enter
        0x29 => 1,  // escape
        0x2A => 14, // backspace
        0x2B => 15, // tab
        0x2C => 57, // space
        0x2D => 12, // minus
        0x2E => 13, // equals
        0x2F => 26, // left bracket
        0x30 => 27, // right bracket
        0x31 => 43, // backslash
        0x32 => 43, // the key beside enter on an ISO board, which evdev calls backslash too
        0x33 => 39, // semicolon
        0x34 => 40, // apostrophe
        0x35 => 41, // grave
        0x36 => 51, // comma
        0x37 => 52, // period
        0x38 => 53, // slash
        0x39 => 58, // caps lock

        // F1 to F10 run on, F11 and F12 were added later and sit elsewhere.
        0x3A..=0x43 => usage - 0x3A + 59,
        0x44 => 87,
        0x45 => 88,

        0x46 => 99,  // print screen
        0x47 => 70,  // scroll lock
        0x48 => 119, // pause
        0x49 => 110, // insert
        0x4A => 102, // home
        0x4B => 104, // page up
        0x4C => 111, // delete
        0x4D => 107, // end
        0x4E => 109, // page down
        0x4F => 106, // right
        0x50 => 105, // left
        0x51 => 108, // down
        0x52 => 103, // up

        0x53 => 69,  // num lock
        0x54 => 98,  // keypad divide
        0x55 => 55,  // keypad multiply
        0x56 => 74,  // keypad minus
        0x57 => 78,  // keypad plus
        0x58 => 96,  // keypad enter
        0x59 => 79,  // keypad 1
        0x5A => 80,  // keypad 2
        0x5B => 81,  // keypad 3
        0x5C => 75,  // keypad 4
        0x5D => 76,  // keypad 5
        0x5E => 77,  // keypad 6
        0x5F => 71,  // keypad 7
        0x60 => 72,  // keypad 8
        0x61 => 73,  // keypad 9
        0x62 => 82,  // keypad 0
        0x63 => 83,  // keypad period
        0x64 => 86,  // the extra key left of Z on an ISO board
        0x65 => 127, // application
        0x67 => 117, // keypad equals

        0x68..=0x73 => usage - 0x68 + 183, // F13 to F24

        0x7F => 113, // mute
        0x80 => 115, // volume up
        0x81 => 114, // volume down

        0x87 => 89,  // ro
        0x88 => 93,  // katakana / hiragana
        0x89 => 124, // yen
        0x8A => 92,  // henkan
        0x8B => 94,  // muhenkan
        0x90 => 122, // hangeul
        0x91 => 123, // hanja

        0xE0 => 29,  // left control
        0xE1 => 42,  // left shift
        0xE2 => 56,  // left alt
        0xE3 => 125, // left meta
        0xE4 => 97,  // right control
        0xE5 => 54,  // right shift
        0xE6 => 100, // right alt
        0xE7 => 126, // right meta

        _ => return None,
    };

    Some(code)
}

#[cfg(test)]
mod tests {
    use super::hid_to_evdev;

    #[test]
    fn letters_land_on_their_keys() {
        // Q W E R T Y along the top row, which evdev numbers 16 onwards.
        let top = [0x14, 0x1A, 0x08, 0x15, 0x17, 0x1C];

        assert_eq!(
            top.map(|usage| hid_to_evdev(usage).expect("a key")),
            [16, 17, 18, 19, 20, 21]
        );
    }

    #[test]
    fn digits_run_one_to_nine_then_zero() {
        assert_eq!(hid_to_evdev(0x1E), Some(2)); // 1
        assert_eq!(hid_to_evdev(0x26), Some(10)); // 9
        assert_eq!(hid_to_evdev(0x27), Some(11)); // 0
    }

    #[test]
    fn function_keys_include_the_two_that_moved() {
        assert_eq!(hid_to_evdev(0x3A), Some(59)); // F1
        assert_eq!(hid_to_evdev(0x43), Some(68)); // F10
        assert_eq!(hid_to_evdev(0x44), Some(87)); // F11
        assert_eq!(hid_to_evdev(0x45), Some(88)); // F12
    }

    #[test]
    fn the_korean_language_keys_are_there() {
        assert_eq!(hid_to_evdev(0x90), Some(122));
        assert_eq!(hid_to_evdev(0x91), Some(123));
    }

    #[test]
    fn no_two_usages_share_a_key_but_the_two_backslashes() {
        let mut seen = std::collections::HashMap::new();

        for usage in 0..=0xFFu16 {
            if let Some(code) = hid_to_evdev(usage) {
                if let Some(earlier) = seen.insert(code, usage) {
                    assert_eq!((earlier, usage), (0x31, 0x32), "evdev {code}");
                }
            }
        }
    }
}
