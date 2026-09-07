//! The second factor: a code from an authenticator app.
//!
//! RFC 6238, which is what every authenticator app implements. Six digits from a shared secret
//! and the current half minute, so a stolen password alone is not a session.
//!
//! # Why SHA-1
//!
//! It is the algorithm the RFC defaults to and the only one every authenticator app is certain
//! to accept. That is not a security claim about SHA-1 — HMAC does not need collision
//! resistance, and the secret is a hundred and sixty bits of randomness rather than anything
//! guessable — but it is the reason a stronger-looking choice here would mostly produce codes
//! that do not match what somebody's phone shows.
//!
//! # Why a window
//!
//! Clocks drift and people type slowly. Accepting the step either side of now costs an
//! attacker nothing they did not already have — they still need the secret — and saves a
//! person from being told their correct code is wrong.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;

/// Bytes in a secret.
///
/// Twenty, which is what the RFC's own examples use and what authenticator apps expect.
pub const SECRET_LEN: usize = 20;

/// Digits in a code.
pub const DIGITS: u32 = 6;

/// Seconds each code is valid for.
pub const STEP_SECONDS: u64 = 30;

/// How many steps either side of now are accepted.
///
/// One. Thirty seconds of slack in each direction covers clock drift and the time it takes to
/// read a code off a phone; more would widen the window a stolen code is useful in for no
/// benefit anybody would notice.
pub const WINDOW: u64 = 1;

/// Generates a secret for a new account.
///
/// # Errors
///
/// Returns an error if the system has no randomness, which is a condition no second factor
/// should be created under.
pub fn new_secret() -> Result<[u8; SECRET_LEN], getrandom::Error> {
    let mut secret = [0u8; SECRET_LEN];
    getrandom::fill(&mut secret)?;

    Ok(secret)
}

/// Returns the code for one step counter.
///
/// Split out from the time so that it can be tested against the RFC's published vectors, which
/// are stated as counters rather than as clock readings.
#[must_use]
pub fn code_at(secret: &[u8], counter: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts a key of any length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();

    // Dynamic truncation: the low nibble of the last byte says where in the digest to read
    // four bytes from, so that which bits become the code depends on the digest itself.
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);

    binary % 10u32.pow(DIGITS)
}

/// Returns the code for a moment, as seconds since the epoch.
#[must_use]
pub fn code_at_time(secret: &[u8], unix_seconds: u64) -> u32 {
    code_at(secret, unix_seconds / STEP_SECONDS)
}

/// Whether a typed code is right for this moment.
///
/// Compared in constant time and across the accepted window, so a server answering many
/// attempts leaks neither which digits were right nor which step matched.
#[must_use]
pub fn verify(secret: &[u8], typed: u32, unix_seconds: u64) -> bool {
    let now = unix_seconds / STEP_SECONDS;
    let mut matched = subtle::Choice::from(0u8);

    // Every step is checked, and the loop does not stop early. Returning as soon as one
    // matches would tell an attacker with a stopwatch which step their guess landed on.
    for step in now.saturating_sub(WINDOW)..=now.saturating_add(WINDOW) {
        matched |= code_at(secret, step).ct_eq(&typed);
    }

    matched.into()
}

/// The alphabet RFC 4648 base32 uses, which is what authenticator apps read.
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Renders a secret the way an authenticator app expects to be given one.
///
/// Base32 without padding. The padding is legal and several popular apps refuse it, which is
/// the sort of detail that turns into "the code never works" rather than into an error.
#[must_use]
pub fn to_base32(secret: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer = 0u16;
    let mut bits = 0u32;

    for &byte in secret {
        buffer = (buffer << 8) | u16::from(byte);
        bits += 8;

        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 0x1f) as usize;
            out.push(BASE32[index] as char);
        }
    }

    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(BASE32[index] as char);
    }

    out
}

/// Builds the `otpauth://` link an authenticator app reads from a QR code.
///
/// The issuer appears twice — once in the label and once as a parameter — because apps
/// disagree about which one they read, and one that reads neither shows an account with no
/// name against it.
#[must_use]
pub fn provisioning_uri(secret: &[u8], account: &str) -> String {
    let account = urlencode(account);

    format!(
        "otpauth://totp/Prism:{account}?secret={}&issuer=Prism&algorithm=SHA1&digits={DIGITS}&period={STEP_SECONDS}",
        to_base32(secret)
    )
}

/// Percent-encodes what a label may not carry literally.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{STEP_SECONDS, code_at_time, provisioning_uri, to_base32, verify};

    /// The secret RFC 6238's test vectors use, which is the ASCII digits repeated.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn the_rfc_6238_vectors_come_out_right() {
        // The published answers, truncated to the six digits this uses. Getting the dynamic
        // truncation wrong produces codes that look perfectly plausible and match nothing any
        // authenticator app shows, which is a failure with no error attached to it.
        for (seconds, expected) in [
            (59u64, 287_082u32),
            (1_111_111_109, 81_804),
            (1_111_111_111, 50_471),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
            (20_000_000_000, 353_130),
        ] {
            assert_eq!(
                code_at_time(RFC_SECRET, seconds),
                expected,
                "at {seconds} seconds"
            );
        }
    }

    #[test]
    fn a_code_is_accepted_within_its_own_step() {
        let now = 1_700_000_000;
        let code = code_at_time(RFC_SECRET, now);

        assert!(verify(RFC_SECRET, code, now));
    }

    #[test]
    fn a_code_is_accepted_one_step_either_side() {
        // Clocks drift and people type slowly. Both directions, because the drift can go
        // either way and only one of them is obvious to test.
        let now = 1_700_000_000;

        assert!(verify(
            RFC_SECRET,
            code_at_time(RFC_SECRET, now),
            now + STEP_SECONDS
        ));
        assert!(verify(
            RFC_SECRET,
            code_at_time(RFC_SECRET, now),
            now - STEP_SECONDS
        ));
    }

    #[test]
    fn a_code_two_steps_old_is_refused() {
        // The window has to end somewhere, or a code read over somebody's shoulder stays
        // useful for as long as they are looking away.
        let now = 1_700_000_000;

        assert!(!verify(
            RFC_SECRET,
            code_at_time(RFC_SECRET, now),
            now + STEP_SECONDS * 2
        ));
    }

    #[test]
    fn a_wrong_code_is_refused() {
        let now = 1_700_000_000;
        let right = code_at_time(RFC_SECRET, now);

        assert!(!verify(RFC_SECRET, right + 1, now));
        assert!(!verify(RFC_SECRET, 0, now));
    }

    #[test]
    fn a_different_secret_gives_a_different_code() {
        let now = 1_700_000_000;

        assert_ne!(
            code_at_time(RFC_SECRET, now),
            code_at_time(b"09876543210987654321", now)
        );
    }

    #[test]
    fn base32_matches_the_rfc_4648_examples() {
        // What an authenticator app reads. A transposed alphabet produces a secret that scans
        // and then never agrees with anything.
        assert_eq!(to_base32(b""), "");
        assert_eq!(to_base32(b"f"), "MY");
        assert_eq!(to_base32(b"fo"), "MZXQ");
        assert_eq!(to_base32(b"foo"), "MZXW6");
        assert_eq!(to_base32(b"foob"), "MZXW6YQ");
        assert_eq!(to_base32(b"fooba"), "MZXW6YTB");
        assert_eq!(to_base32(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn base32_is_unpadded() {
        // Legal either way, and several popular apps refuse the padded form.
        assert!(!to_base32(b"foob").contains('='));
    }

    #[test]
    fn the_provisioning_link_carries_what_an_app_needs() {
        let uri = provisioning_uri(RFC_SECRET, "someone@example.com");

        assert!(uri.starts_with("otpauth://totp/Prism:"));
        assert!(uri.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"));
        assert!(uri.contains("issuer=Prism"));
        assert!(uri.contains("digits=6"));
        assert!(uri.contains("period=30"));
        assert!(uri.contains("algorithm=SHA1"));
    }

    #[test]
    fn a_label_with_awkward_characters_is_escaped() {
        // A colon in the label splits it, and a space ends the URI in some readers.
        let uri = provisioning_uri(RFC_SECRET, "a b:c");

        assert!(uri.contains("Prism:a%20b%3Ac"));
    }
}
