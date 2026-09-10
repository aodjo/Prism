//! Injecting input on Windows through `SendInput`.
//!
//! Two choices here differ from the macOS injector and are worth stating.
//!
//! Keys are posted as **scan codes**, not virtual keys. A virtual key is interpreted
//! through the active keyboard layout, so injecting `VK_W` on an AZERTY host presses the
//! key that layout calls W and a game reading raw input sees the wrong physical key. A
//! scan code names the physical key directly, which is both what the wire carries and
//! what DirectInput and Raw Input report to games.
//!
//! There is no permission gate to check at startup. Windows reports refusal through
//! `SendInput`'s return value instead — it inserts nothing and sets `ERROR_ACCESS_DENIED`
//! when User Interface Privilege Isolation blocks a lower-integrity process from driving
//! a higher-integrity window. That makes failure loud, unlike `CGEventPost`, so it is
//! checked on every call rather than once.

use windows::Win32::Foundation::{GetLastError, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE_NOCOALESCE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

use crate::input::{HeldKeys, Injector, InputError, PointerSample};
use crate::net::packet::{InputEvent, MouseButton};

/// One notch of a scroll wheel, the unit `mouseData` counts in.
const WHEEL_DELTA: i32 = 120;

/// Injects input events onto this machine.
pub struct WindowsInjector {
    landing: bool,
    buttons: [bool; 3],
    keys: HeldKeys,
}

impl WindowsInjector {
    /// Sends one prepared event, treating a refusal as an error.
    ///
    /// `SendInput` returns how many events it managed to insert, so anything short of the
    /// whole batch is a refusal. It does not say why, and the plausible reasons are far
    /// apart — a privileged foreground window, or no interactive desktop to inject into
    /// at all — so the Win32 code is carried out rather than interpreted here.
    fn send(&mut self, input: INPUT) -> Result<(), InputError> {
        // SAFETY: the slice and the size argument describe the same `INPUT`, which is
        // what `SendInput` requires in order to walk the array.
        let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };

        if sent == 1 {
            self.landing = true;
            Ok(())
        } else {
            self.landing = false;

            // SAFETY: reads the calling thread's own last-error value and takes nothing.
            let code = unsafe { GetLastError() }.0;

            Err(InputError::Refused { code })
        }
    }

    /// Moves the pointer by a relative amount.
    ///
    /// The motion is not coalesced. Windows merges consecutive moves by default, which
    /// saves work for a local mouse whose reports are already contiguous but throws away
    /// detail from a remote one whose reports arrive in bursts after a network delay.
    fn move_pointer(&mut self, dx: i16, dy: i16) -> Result<(), InputError> {
        self.send(mouse_input(
            i32::from(dx),
            i32::from(dy),
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE,
        ))
    }

    /// Puts the pointer a fraction of the way across and down the primary display.
    ///
    /// The one place the two platforms agree without translating: `SendInput` takes an
    /// absolute position as exactly the range the wire carries, zero at one edge and 65535 at
    /// the other.
    fn place_pointer(&mut self, x: u16, y: u16) -> Result<(), InputError> {
        self.send(mouse_input(
            i32::from(x),
            i32::from(y),
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE_NOCOALESCE,
        ))
    }

    /// Returns the keys the injector is holding down.
    ///
    /// Public so a caller can see whether anything is still held, and so the release path
    /// can be tested without a machine to press keys on.
    #[must_use]
    pub fn held_keys(&self) -> HeldKeys {
        self.keys
    }

    /// Presses or releases a pointer button.
    fn press_button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InputError> {
        self.buttons[button as usize] = pressed;

        let flags = match (button, pressed) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };

        self.send(mouse_input(0, 0, 0, flags))
    }

    /// Scrolls the wheel.
    ///
    /// Both axes are inverted against the wire, which counts a downward and rightward
    /// scroll as positive, while Windows counts a wheel pushed away from the user and a
    /// tilt to the right as positive.
    fn scroll(&mut self, dx: i16, dy: i16) -> Result<(), InputError> {
        if dy != 0 {
            self.send(mouse_input(
                0,
                0,
                -i32::from(dy) * WHEEL_DELTA,
                MOUSEEVENTF_WHEEL,
            ))?;
        }

        if dx != 0 {
            self.send(mouse_input(
                0,
                0,
                i32::from(dx) * WHEEL_DELTA,
                MOUSEEVENTF_HWHEEL,
            ))?;
        }

        Ok(())
    }

    /// Presses or releases a key identified by its USB HID usage code.
    fn press_key(&mut self, usage: u16, pressed: bool) -> Result<(), InputError> {
        let Some((scan, extended)) = hid_to_scan_code(usage) else {
            return Err(InputError::Inject {
                reason: "this key has no scan code on Windows",
            });
        };

        let mut flags = KEYEVENTF_SCANCODE;

        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }

        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }

        let outcome = self.send(key_input(scan, flags));

        // A press is remembered only once `SendInput` has taken it, and a release is
        // forgotten whether it did or not: a key-down that never went out must not be
        // released later, and a key-up the system refuses is not worth attempting again on
        // every cleanup for the rest of the session.
        self.keys.set(usage, pressed && outcome.is_ok());

        outcome
    }
}

impl Injector for WindowsInjector {
    fn new() -> Result<Self, InputError> {
        Ok(Self {
            landing: false,
            buttons: [false; 3],
            keys: HeldKeys::default(),
        })
    }

    fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { dx, dy } => self.move_pointer(dx, dy),
            InputEvent::MouseButton { button, pressed } => self.press_button(button, pressed),
            InputEvent::MouseScroll { dx, dy } => self.scroll(dx, dy),
            InputEvent::Key { usage, pressed } => self.press_key(usage, pressed),
            InputEvent::MouseTo { x, y } => self.place_pointer(x, y),
        }
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        // The held set is copied out first because releasing a key edits it, and because
        // an empty set is the state to leave behind even if a send fails: a key this
        // machine will not release is not a key worth trying again on the next cleanup.
        let held = self.keys;
        let mut outcome = Ok(());

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

        outcome
    }

    fn injection_is_landing(&self) -> bool {
        self.landing
    }
}

/// Builds a mouse event.
fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                // Windows reads this back as a signed wheel delta; the cast is
                // two's complement in both directions and loses nothing.
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Builds a keyboard event carrying a scan code.
fn key_input(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                // Ignored while `KEYEVENTF_SCANCODE` is set, which it always is here.
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Returns where the system pointer is, if Windows will say.
///
/// Only used to prove that injection reaches the system; nothing on the injection path
/// needs it, because the wire carries relative motion.
#[must_use]
pub fn system_pointer() -> Option<(i32, i32)> {
    let mut point = POINT::default();

    // SAFETY: the pointer is to a live, correctly typed local.
    unsafe { GetCursorPos(&mut point) }.ok()?;

    Some((point.x, point.y))
}

/// Reads the pointer position and the size of the primary display.
///
/// `SM_CXSCREEN` is the primary display alone, not the bounding box of every monitor, which
/// is what the client is showing and therefore what the position has to be scaled against.
/// A pointer dragged onto a second monitor reports off the edge of that display, so it is
/// clamped back to the edge it left by.
#[must_use]
pub fn pointer() -> Option<PointerSample> {
    let (x, y) = system_pointer()?;

    // SAFETY: both metrics take only their index and return a plain count.
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };

    let screen_width = clamp_dimension(width);
    let screen_height = clamp_dimension(height);

    Some(PointerSample {
        x: clamp_to_u16(x, screen_width),
        y: clamp_to_u16(y, screen_height),
        screen_width,
        screen_height,
    })
}

/// Clamps a coordinate into the display and onto the `u16` the wire carries.
fn clamp_to_u16(value: i32, extent: u16) -> u16 {
    value.clamp(0, i32::from(extent.saturating_sub(1))) as u16
}

/// Clamps a display extent into the `u16` the wire carries, never reaching zero.
///
/// A screen of no pixels is not a thing that exists, and the wire refuses to carry one, so
/// a degenerate reading becomes the smallest screen rather than an error the caller would
/// have to handle every frame.
fn clamp_dimension(extent: i32) -> u16 {
    extent.clamp(1, i32::from(u16::MAX)) as u16
}

/// Translates a USB HID usage code into a set 1 scan code and whether it is extended.
///
/// The extended flag is not decoration: it is the only thing separating a dozen pairs of
/// keys that share a code. Left and right control are both `0x1D`, `Home` and keypad 7
/// are both `0x47`, and the numeric keypad collides with the whole navigation cluster.
///
/// `Pause` is deliberately absent. Its make code is prefixed with `0xE1` rather than
/// `0xE0`, which `SendInput` has no way to express, and the byte it would otherwise use
/// is already `NumLock`.
#[must_use]
pub fn hid_to_scan_code(usage: u16) -> Option<(u16, bool)> {
    let plain = |code: u16| Some((code, false));
    let extended = |code: u16| Some((code, true));

    match usage {
        0x04 => plain(0x1E), // a
        0x05 => plain(0x30), // b
        0x06 => plain(0x2E), // c
        0x07 => plain(0x20), // d
        0x08 => plain(0x12), // e
        0x09 => plain(0x21), // f
        0x0A => plain(0x22), // g
        0x0B => plain(0x23), // h
        0x0C => plain(0x17), // i
        0x0D => plain(0x24), // j
        0x0E => plain(0x25), // k
        0x0F => plain(0x26), // l
        0x10 => plain(0x32), // m
        0x11 => plain(0x31), // n
        0x12 => plain(0x18), // o
        0x13 => plain(0x19), // p
        0x14 => plain(0x10), // q
        0x15 => plain(0x13), // r
        0x16 => plain(0x1F), // s
        0x17 => plain(0x14), // t
        0x18 => plain(0x16), // u
        0x19 => plain(0x2F), // v
        0x1A => plain(0x11), // w
        0x1B => plain(0x2D), // x
        0x1C => plain(0x15), // y
        0x1D => plain(0x2C), // z

        0x1E => plain(0x02), // 1
        0x1F => plain(0x03), // 2
        0x20 => plain(0x04), // 3
        0x21 => plain(0x05), // 4
        0x22 => plain(0x06), // 5
        0x23 => plain(0x07), // 6
        0x24 => plain(0x08), // 7
        0x25 => plain(0x09), // 8
        0x26 => plain(0x0A), // 9
        0x27 => plain(0x0B), // 0

        0x28 => plain(0x1C), // return
        0x29 => plain(0x01), // escape
        0x2A => plain(0x0E), // backspace
        0x2B => plain(0x0F), // tab
        0x2C => plain(0x39), // space
        0x2D => plain(0x0C), // minus
        0x2E => plain(0x0D), // equals
        0x2F => plain(0x1A), // left bracket
        0x30 => plain(0x1B), // right bracket
        0x31 => plain(0x2B), // backslash
        0x33 => plain(0x27), // semicolon
        0x34 => plain(0x28), // quote
        0x35 => plain(0x29), // grave
        0x36 => plain(0x33), // comma
        0x37 => plain(0x34), // period
        0x38 => plain(0x35), // slash
        0x39 => plain(0x3A), // caps lock

        0x3A => plain(0x3B), // f1
        0x3B => plain(0x3C), // f2
        0x3C => plain(0x3D), // f3
        0x3D => plain(0x3E), // f4
        0x3E => plain(0x3F), // f5
        0x3F => plain(0x40), // f6
        0x40 => plain(0x41), // f7
        0x41 => plain(0x42), // f8
        0x42 => plain(0x43), // f9
        0x43 => plain(0x44), // f10
        0x44 => plain(0x57), // f11
        0x45 => plain(0x58), // f12

        0x46 => extended(0x37), // print screen
        0x47 => plain(0x46),    // scroll lock
        0x49 => extended(0x52), // insert
        0x4A => extended(0x47), // home
        0x4B => extended(0x49), // page up
        0x4C => extended(0x53), // delete
        0x4D => extended(0x4F), // end
        0x4E => extended(0x51), // page down

        0x4F => extended(0x4D), // right
        0x50 => extended(0x4B), // left
        0x51 => extended(0x50), // down
        0x52 => extended(0x48), // up

        0x53 => plain(0x45),    // num lock
        0x54 => extended(0x35), // keypad divide
        0x55 => plain(0x37),    // keypad multiply
        0x56 => plain(0x4A),    // keypad minus
        0x57 => plain(0x4E),    // keypad plus
        0x58 => extended(0x1C), // keypad enter
        0x59 => plain(0x4F),    // keypad 1
        0x5A => plain(0x50),    // keypad 2
        0x5B => plain(0x51),    // keypad 3
        0x5C => plain(0x4B),    // keypad 4
        0x5D => plain(0x4C),    // keypad 5
        0x5E => plain(0x4D),    // keypad 6
        0x5F => plain(0x47),    // keypad 7
        0x60 => plain(0x48),    // keypad 8
        0x61 => plain(0x49),    // keypad 9
        0x62 => plain(0x52),    // keypad 0
        0x63 => plain(0x53),    // keypad period
        0x65 => extended(0x5D), // application

        0xE0 => plain(0x1D),    // left control
        0xE1 => plain(0x2A),    // left shift
        0xE2 => plain(0x38),    // left alt
        0xE3 => extended(0x5B), // left meta
        0xE4 => extended(0x1D), // right control
        0xE5 => plain(0x36),    // right shift
        0xE6 => extended(0x38), // right alt
        0xE7 => extended(0x5C), // right meta

        _ => None,
    }
}
