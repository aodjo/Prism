//! The account password derivation, compiled for a browser.
//!
//! The operator dashboard is a web page, and signing in to it means proving knowledge of a
//! password the same way the application does: by spending sixty-four mebibytes and three
//! passes of Argon2id on it and sending what comes out. A browser has no Argon2 — it is not in
//! WebCrypto and the page's content policy forbids fetching one — so the same Rust that does it
//! everywhere else is compiled to WebAssembly and served from the page's own origin.
//!
//! Doing it in the browser rather than on the server is not a preference. The server checks a
//! hash of `auth`; if it were sent the password it would hold `root`, and from `root` it could
//! derive `wrap`, and with `wrap` it could open every sealed private key it stores. See
//! [`prism_secret`] for why those are two values.
//!
//! # What this deliberately does not export
//!
//! `wrap`. The dashboard reads accounts and ends sessions; it never opens a private key, so the
//! code running in that tab cannot compute the secret that would. [`prism_secret::derive`]
//! produces both and only the authentication half leaves this module.
//!
//! # Calling it
//!
//! Three functions over the module's linear memory, with no glue:
//!
//! ```text
//! const page = await WebAssembly.instantiateStreaming(fetch('/admin/argon2.wasm'))
//! const { prism_alloc, prism_free, prism_auth, memory } = page.instance.exports
//!
//! // password and salt in, thirty-two bytes out
//! const ok = prism_auth(passwordPtr, passwordLen, saltPtr, outPtr)
//! ```
//!
//! Every pointer must come from [`prism_alloc`] and be returned with [`prism_free`].

use prism_secret::{SALT_LEN, SECRET_LEN, SecretError};

/// The derivation succeeded and the output holds the authentication secret.
pub const OK: i32 = 0;

/// The password was shorter than [`prism_secret::MIN_PASSWORD`] characters.
///
/// Told apart from the others because it is the one a person can act on, and because it is
/// refused before any work happens rather than after a wait.
pub const TOO_SHORT: i32 = -1;

/// The password hash refused the parameters, which would be a fault in this build.
pub const REFUSED: i32 = -2;

/// The bytes handed over were not valid UTF-8.
pub const NOT_TEXT: i32 = -3;

/// Reserves `len` bytes in this module's memory and returns where they start.
///
/// The caller writes a password or a salt there before calling [`prism_auth`], and hands the
/// same pointer and length back to [`prism_free`] afterwards.
///
/// Returns a null pointer if `len` is zero, which nothing here should ask for.
#[unsafe(no_mangle)]
pub extern "C" fn prism_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return core::ptr::null_mut();
    }

    let mut buffer = Vec::<u8>::with_capacity(len);
    let pointer = buffer.as_mut_ptr();
    core::mem::forget(buffer);

    pointer
}

/// Releases what [`prism_alloc`] reserved, overwriting it first.
///
/// The overwrite is the point: a password sits in this memory, and WebAssembly memory is a
/// buffer the page can read for as long as the tab is open.
///
/// # Safety
///
/// `pointer` must be one [`prism_alloc`] returned for exactly `len` bytes, and must not have
/// been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn prism_free(pointer: *mut u8, len: usize) {
    if pointer.is_null() || len == 0 {
        return;
    }

    // SAFETY: the caller guarantees this pointer and length came from one `prism_alloc` call
    // and have not been freed, which is what `Vec::from_raw_parts` requires.
    let mut buffer = unsafe { Vec::from_raw_parts(pointer, len, len) };

    for byte in &mut buffer {
        // SAFETY: the pointer is to a byte of a live vector this function now owns.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }

    drop(buffer);
}

/// Derives the authentication secret and writes its thirty-two bytes to `out`.
///
/// Takes a few hundred milliseconds and holds sixty-four mebibytes while it runs, so a page
/// calling it should do so in a worker rather than on the thread that is drawing.
///
/// Returns [`OK`], or one of [`TOO_SHORT`], [`REFUSED`] and [`NOT_TEXT`]. On anything but
/// [`OK`] the output is left untouched.
///
/// # Safety
///
/// `password` must point to `password_len` readable bytes, `salt` to [`SALT_LEN`] readable
/// bytes, and `out` to [`SECRET_LEN`] writable bytes. All three must come from
/// [`prism_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn prism_auth(
    password: *const u8,
    password_len: usize,
    salt: *const u8,
    out: *mut u8,
) -> i32 {
    if password.is_null() || salt.is_null() || out.is_null() {
        return NOT_TEXT;
    }

    // SAFETY: the caller guarantees `password` is readable for `password_len` bytes and that
    // the allocation outlives this call, which it does — it frees after this returns.
    let typed = unsafe { core::slice::from_raw_parts(password, password_len) };

    let Ok(text) = core::str::from_utf8(typed) else {
        return NOT_TEXT;
    };

    // SAFETY: the caller guarantees `salt` is readable for `SALT_LEN` bytes.
    let bytes = unsafe { core::slice::from_raw_parts(salt, SALT_LEN) };
    let mut fixed = [0u8; SALT_LEN];
    fixed.copy_from_slice(bytes);

    match prism_secret::derive(text, &fixed) {
        // `secrets` drops at the end of this block, which overwrites both halves. Only the
        // authentication one is copied out; the wrapping one never leaves this function.
        Ok(secrets) => {
            // SAFETY: the caller guarantees `out` is writable for `SECRET_LEN` bytes, and the
            // source is a distinct array on this stack, so the two cannot overlap.
            unsafe { core::ptr::copy_nonoverlapping(secrets.auth.as_ptr(), out, SECRET_LEN) };

            OK
        }
        Err(SecretError::TooShort { .. }) => TOO_SHORT,
        Err(SecretError::Hash { .. }) => REFUSED,
    }
}
