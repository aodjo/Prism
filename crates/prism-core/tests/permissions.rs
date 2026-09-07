//! Tests for what a host reports it still needs.
//!
//! The grants themselves depend on what a person has clicked, so what is pinned here is the
//! reasoning around them: which grants a given session actually requires, and that every one
//! it names can be explained and pointed at.

use prism_core::control::permissions::{Grant, Permissions, check};

#[test]
fn a_host_that_only_shows_its_screen_does_not_ask_for_control() {
    // Asking for Accessibility on behalf of somebody who turned control off is asking for
    // control of their machine that they said they did not want to give.
    let nothing = Permissions {
        screen: false,
        input: false,
    };

    assert_eq!(nothing.missing(false), vec![Grant::Screen]);
    assert_eq!(nothing.missing(true), vec![Grant::Screen, Grant::Input]);
}

#[test]
fn nothing_is_asked_for_when_everything_is_held() {
    let all = Permissions {
        screen: true,
        input: true,
    };

    assert!(all.missing(true).is_empty());
    assert!(all.missing(false).is_empty());
}

#[test]
fn control_alone_is_reported_without_the_screen() {
    let screen_only = Permissions {
        screen: true,
        input: false,
    };

    assert!(screen_only.missing(false).is_empty());
    assert_eq!(screen_only.missing(true), vec![Grant::Input]);
}

#[test]
fn every_grant_can_be_named_explained_and_opened() {
    // A missing grant is only useful to report if it comes with somewhere to go about it.
    for grant in [Grant::Screen, Grant::Input] {
        assert!(!grant.name().is_empty(), "{grant:?} has no name");
        assert!(!grant.purpose().is_empty(), "{grant:?} has no purpose");
        assert!(
            grant
                .settings_url()
                .starts_with("x-apple.systempreferences:"),
            "{grant:?} does not open settings: {}",
            grant.settings_url()
        );
        assert_eq!(grant.to_string(), grant.name());
    }
}

#[test]
fn asking_what_is_allowed_never_prompts_and_never_panics() {
    // Called every time an interface redraws, so it has to be cheap and quiet. What it
    // answers depends on the machine; that it answers at all does not.
    let first = check();
    let again = check();

    assert_eq!(first, again, "the answer changed without anybody clicking");
}
