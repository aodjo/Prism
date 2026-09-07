//! Tests for input translation.
//!
//! Each platform translates USB HID usage codes into its own numbering with a hand-written
//! table of eighty-odd entries, which is exactly the kind of thing that carries a
//! transposed pair nobody notices until someone types a bracket. These check the entries
//! that anchor each table and the properties that would catch a duplicate or a hole.

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashSet;

    use prism_core::input::macos::{MacInjector, hid_to_virtual_key, modifier_flag};
    use prism_core::input::{Injector, InputError};
    use prism_core::net::packet::InputEvent;

    #[test]
    fn an_injected_move_actually_moves_the_pointer() {
        // `CGEventPost` returns nothing and does nothing when the process is untrusted, so
        // the only way to know injection works is to move the pointer and look.
        let mut injector = match MacInjector::new() {
            Ok(injector) => injector,
            Err(InputError::PermissionDenied) => {
                eprintln!("skipping: this process has no Accessibility permission");
                return;
            }
            Err(err) => panic!("could not create an injector: {err}"),
        };

        // Park the pointer against the top left first. Movement is clamped to the display, so
        // a test that starts wherever the pointer happens to be fails whenever it happens to
        // be near an edge.
        injector
            .inject(InputEvent::MouseMove {
                dx: i16::MIN,
                dy: i16::MIN,
            })
            .expect("a plain move is always injectable");

        // And wait for it to get there before reading where "there" is. Posting is
        // asynchronous, so a start read straight after the parking move is wherever the
        // pointer still was, and every later assertion is measured against a start that was
        // never true. This was the flake.
        assert!(
            wait_until_landed(&injector),
            "the pointer never reached the corner it was parked at"
        );
        let (start_x, start_y) = injector.position();

        injector
            .inject(InputEvent::MouseMove { dx: 60, dy: 40 })
            .expect("a plain move is always injectable");

        assert!(
            wait_until_landed(&injector),
            "the pointer did not follow the injected move"
        );

        let (moved_x, moved_y) = injector.position();
        assert!(
            (moved_x - start_x - 60.0).abs() < 1.0,
            "horizontal movement was applied"
        );
        assert!(
            (moved_y - start_y - 40.0).abs() < 1.0,
            "vertical movement was applied"
        );

        injector
            .inject(InputEvent::MouseMove { dx: -60, dy: -40 })
            .expect("putting the pointer back is the same operation");
    }

    /// Waits until the system pointer agrees with where the injector thinks it put it.
    ///
    /// `CGEventPost` queues an event rather than applying it, so the two disagree for a while
    /// after every injection — for longer on a machine running every test binary at once than
    /// on an idle one. Waiting for them to agree is waiting for the outcome; waiting for a
    /// duration is guessing at it, and the guess is what made this test flake.
    fn wait_until_landed(injector: &MacInjector) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);

        while std::time::Instant::now() < deadline {
            if injector.injection_is_landing() {
                return true;
            }

            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        injector.injection_is_landing()
    }

    #[test]
    fn the_anchors_of_the_letter_row_are_right() {
        // macOS numbers keys by physical position, so the letters are scattered rather than
        // sequential — which is why each of these is worth pinning.
        assert_eq!(hid_to_virtual_key(0x04), Some(0), "a");
        assert_eq!(hid_to_virtual_key(0x16), Some(1), "s");
        assert_eq!(hid_to_virtual_key(0x07), Some(2), "d");
        assert_eq!(hid_to_virtual_key(0x1D), Some(6), "z");
        assert_eq!(hid_to_virtual_key(0x14), Some(12), "q");
        assert_eq!(hid_to_virtual_key(0x1A), Some(13), "w");
    }

    #[test]
    fn the_keys_a_session_cannot_work_without_are_mapped() {
        assert_eq!(hid_to_virtual_key(0x28), Some(36), "return");
        assert_eq!(hid_to_virtual_key(0x29), Some(53), "escape");
        assert_eq!(hid_to_virtual_key(0x2A), Some(51), "backspace");
        assert_eq!(hid_to_virtual_key(0x2B), Some(48), "tab");
        assert_eq!(hid_to_virtual_key(0x2C), Some(49), "space");
    }

    #[test]
    fn the_arrow_keys_are_not_transposed() {
        assert_eq!(hid_to_virtual_key(0x50), Some(123), "left");
        assert_eq!(hid_to_virtual_key(0x4F), Some(124), "right");
        assert_eq!(hid_to_virtual_key(0x51), Some(125), "down");
        assert_eq!(hid_to_virtual_key(0x52), Some(126), "up");
    }

    #[test]
    fn every_letter_and_digit_maps_somewhere_distinct() {
        let mut seen = HashSet::new();

        for usage in 0x04..=0x27u16 {
            let key = hid_to_virtual_key(usage)
                .unwrap_or_else(|| panic!("usage {usage:#04x} has no macOS key"));
            assert!(
                seen.insert(key),
                "usage {usage:#04x} maps to {key}, which is already taken"
            );
        }

        assert_eq!(seen.len(), 36, "twenty-six letters and ten digits");
    }

    #[test]
    fn the_function_row_is_complete_and_distinct() {
        let mut seen = HashSet::new();

        for usage in 0x3A..=0x45u16 {
            let key = hid_to_virtual_key(usage)
                .unwrap_or_else(|| panic!("function key {usage:#04x} has no macOS key"));
            assert!(
                seen.insert(key),
                "function key {usage:#04x} collides on {key}"
            );
        }

        assert_eq!(seen.len(), 12, "F1 through F12");
    }

    #[test]
    fn both_sides_of_every_modifier_map_and_are_distinct() {
        let mut seen = HashSet::new();

        for usage in 0xE0..=0xE7u16 {
            let key = hid_to_virtual_key(usage)
                .unwrap_or_else(|| panic!("modifier {usage:#04x} has no macOS key"));
            assert!(seen.insert(key), "modifier {usage:#04x} collides on {key}");
            assert!(
                modifier_flag(usage).is_some(),
                "modifier {usage:#04x} injects a key but records no flag, so later keys lose it"
            );
        }

        assert_eq!(seen.len(), 8, "four modifiers, left and right");
    }

    #[test]
    fn the_left_and_right_halves_of_a_modifier_share_a_flag() {
        for (left, right) in [(0xE0u16, 0xE4u16), (0xE1, 0xE5), (0xE2, 0xE6), (0xE3, 0xE7)] {
            assert_eq!(
                modifier_flag(left),
                modifier_flag(right),
                "{left:#04x} and {right:#04x} are the same modifier"
            );
        }
    }

    #[test]
    fn an_ordinary_key_records_no_modifier_flag() {
        assert!(modifier_flag(0x04).is_none(), "a is not a modifier");
        assert!(modifier_flag(0x2C).is_none(), "space is not a modifier");
    }

    #[test]
    fn an_unmapped_usage_is_refused_rather_than_guessed() {
        assert_eq!(hid_to_virtual_key(0x00), None, "reserved");
        assert_eq!(
            hid_to_virtual_key(0x32),
            None,
            "non-US hash, deliberately absent"
        );
        assert_eq!(hid_to_virtual_key(0xFF), None, "beyond the table");
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::collections::HashSet;

    use prism_core::input::windows::{WindowsInjector, hid_to_scan_code, system_pointer};
    use prism_core::input::{Injector, InputError};
    use prism_core::net::packet::InputEvent;

    /// Returns where the pointer is once it has stopped moving.
    ///
    /// `SendInput` queues an event rather than applying it, so the pointer trails the call
    /// by an amount that depends on how busy the machine is. Waiting for two readings to
    /// agree costs nothing on an idle machine and does not give up early on a loaded one.
    fn settled_pointer() -> Option<(i32, i32)> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut previous = system_pointer()?;

        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let current = system_pointer()?;

            if current == previous {
                return Some(current);
            }
            previous = current;
        }

        Some(previous)
    }

    #[test]
    fn an_injected_move_actually_moves_the_pointer() {
        let mut injector = WindowsInjector::new().expect("Windows needs no permission up front");

        // Park the pointer against the top left first. Movement is clamped to the desktop,
        // so a test that starts wherever the pointer happens to be fails whenever it
        // happens to be near an edge.
        //
        // This is also where a machine that cannot be driven at all says so — over SSH
        // there is no interactive desktop to inject into — so the whole test steps aside
        // rather than reporting a mapping bug it did not find.
        match injector.inject(InputEvent::MouseMove {
            dx: i16::MIN,
            dy: i16::MIN,
        }) {
            Ok(()) => {}
            Err(err @ InputError::Refused { .. }) => {
                eprintln!("skipping: this session cannot inject input: {err}");
                return;
            }
            Err(err) => panic!("unexpected refusal: {err}"),
        }

        let Some((start_x, start_y)) = settled_pointer() else {
            panic!("Windows would not say where the pointer is");
        };

        match injector.inject(InputEvent::MouseMove { dx: 60, dy: 40 }) {
            Ok(()) => {}
            Err(InputError::Inject { .. }) => {
                eprintln!(
                    "skipping: the system refused the event, which means a more \
                           privileged window holds the foreground"
                );
                return;
            }
            Err(err) => panic!("unexpected refusal: {err}"),
        }

        assert!(
            injector.injection_is_landing(),
            "the system did not accept the injected move"
        );

        let (moved_x, moved_y) = settled_pointer().expect("the pointer still has a position");

        // Only the direction is checked, not the distance. Windows scales relative motion
        // by the pointer speed setting and by acceleration, so the exact landing spot
        // depends on how the machine is configured.
        assert!(moved_x > start_x, "the pointer moved right");
        assert!(moved_y > start_y, "the pointer moved down");
    }

    #[test]
    fn the_anchors_of_the_letter_row_are_right() {
        assert_eq!(hid_to_scan_code(0x04), Some((0x1E, false)), "a");
        assert_eq!(hid_to_scan_code(0x16), Some((0x1F, false)), "s");
        assert_eq!(hid_to_scan_code(0x07), Some((0x20, false)), "d");
        assert_eq!(hid_to_scan_code(0x1A), Some((0x11, false)), "w");
        assert_eq!(hid_to_scan_code(0x14), Some((0x10, false)), "q");
    }

    #[test]
    fn the_keys_a_session_cannot_work_without_are_mapped() {
        assert_eq!(hid_to_scan_code(0x28), Some((0x1C, false)), "return");
        assert_eq!(hid_to_scan_code(0x29), Some((0x01, false)), "escape");
        assert_eq!(hid_to_scan_code(0x2A), Some((0x0E, false)), "backspace");
        assert_eq!(hid_to_scan_code(0x2B), Some((0x0F, false)), "tab");
        assert_eq!(hid_to_scan_code(0x2C), Some((0x39, false)), "space");
    }

    #[test]
    fn the_arrow_keys_are_not_transposed_and_are_all_extended() {
        assert_eq!(hid_to_scan_code(0x50), Some((0x4B, true)), "left");
        assert_eq!(hid_to_scan_code(0x4F), Some((0x4D, true)), "right");
        assert_eq!(hid_to_scan_code(0x51), Some((0x50, true)), "down");
        assert_eq!(hid_to_scan_code(0x52), Some((0x48, true)), "up");
    }

    #[test]
    fn every_letter_and_digit_maps_somewhere_distinct() {
        let mut seen = HashSet::new();

        for usage in 0x04..=0x27u16 {
            let key = hid_to_scan_code(usage)
                .unwrap_or_else(|| panic!("usage {usage:#04x} has no scan code"));
            assert!(
                seen.insert(key),
                "usage {usage:#04x} maps to {key:?}, which is already taken"
            );
        }

        assert_eq!(seen.len(), 36, "twenty-six letters and ten digits");
    }

    #[test]
    fn the_function_row_is_complete_and_distinct() {
        let mut seen = HashSet::new();

        for usage in 0x3A..=0x45u16 {
            let key = hid_to_scan_code(usage)
                .unwrap_or_else(|| panic!("function key {usage:#04x} has no scan code"));
            assert!(
                seen.insert(key),
                "function key {usage:#04x} collides on {key:?}"
            );
        }

        assert_eq!(seen.len(), 12, "F1 through F12");
    }

    #[test]
    fn the_two_halves_of_a_modifier_share_a_code_and_differ_only_in_the_extended_flag() {
        // This is the property the extended flag exists for. Left and right control are
        // both 0x1D on the wire Windows reads; dropping the flag would make every right
        // modifier press as its left twin.
        for (left, right) in [(0xE0u16, 0xE4u16), (0xE2, 0xE6)] {
            let (left_code, left_extended) =
                hid_to_scan_code(left).expect("the left half is mapped");
            let (right_code, right_extended) =
                hid_to_scan_code(right).expect("the right half is mapped");

            assert_eq!(left_code, right_code, "the halves share a scan code");
            assert!(!left_extended, "{left:#04x} is the plain one");
            assert!(right_extended, "{right:#04x} is the extended one");
        }
    }

    #[test]
    fn the_keypad_does_not_collide_with_the_cluster_it_shares_codes_with() {
        // Every one of these pairs is the same byte. Only the extended flag separates
        // Home from keypad 7, so a table that forgot it would type digits at the user
        // whenever they pressed Home.
        for (navigation, keypad) in [
            (0x4Au16, 0x5Fu16), // home, keypad 7
            (0x52, 0x60),       // up, keypad 8
            (0x4B, 0x61),       // page up, keypad 9
            (0x50, 0x5C),       // left, keypad 4
            (0x4F, 0x5E),       // right, keypad 6
            (0x4D, 0x59),       // end, keypad 1
            (0x51, 0x5A),       // down, keypad 2
            (0x4E, 0x5B),       // page down, keypad 3
            (0x49, 0x62),       // insert, keypad 0
            (0x4C, 0x63),       // delete, keypad period
        ] {
            let navigation = hid_to_scan_code(navigation).expect("the navigation key is mapped");
            let keypad = hid_to_scan_code(keypad).expect("the keypad key is mapped");

            assert_eq!(navigation.0, keypad.0, "the pair shares a scan code");
            assert_ne!(navigation, keypad, "the extended flag separates them");
        }
    }

    #[test]
    fn nothing_in_the_whole_table_collides() {
        let mut seen = HashSet::new();

        for usage in 0..=0xFFu16 {
            let Some(key) = hid_to_scan_code(usage) else {
                continue;
            };
            assert!(
                seen.insert(key),
                "usage {usage:#04x} maps to {key:?}, which another key already claims"
            );
        }
    }

    #[test]
    fn an_unmapped_usage_is_refused_rather_than_guessed() {
        assert_eq!(hid_to_scan_code(0x00), None, "reserved");
        assert_eq!(
            hid_to_scan_code(0x32),
            None,
            "non-US hash, deliberately absent"
        );
        assert_eq!(
            hid_to_scan_code(0x48),
            None,
            "pause, whose 0xE1 prefix SendInput cannot express"
        );
        assert_eq!(hid_to_scan_code(0xFF), None, "beyond the table");
    }
}
