//! Tests for input translation.
//!
//! The HID to macOS key mapping is a hand-written table of eighty-odd entries, which is
//! exactly the kind of thing that carries a transposed pair nobody notices until someone
//! types a bracket. These check the entries that anchor it and the properties that would
//! catch a duplicate or a hole.

#![cfg(target_os = "macos")]

use std::collections::HashSet;

use prism_core::input::InputError;
use prism_core::input::macos::{MacInjector, hid_to_virtual_key, modifier_flag};
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

    let (start_x, start_y) = injector.position();

    injector
        .inject(InputEvent::MouseMove { dx: 60, dy: 40 })
        .expect("a plain move is always injectable");

    // Posting is asynchronous, so the pointer needs a moment to catch up before it is
    // fair to ask where it is.
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert!(
        injector.injection_is_landing(),
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
