//! Node-API surface over the Prism core.
//!
//! This crate exposes **control calls and low-rate stats only**. Video frames, GPU
//! textures, and packets never cross this boundary: a frame that enters V8 has already
//! cost more than the entire latency budget. Anything added here that takes or returns
//! frame data is a bug.
//!
//! The resulting `.node` is a Node-API binary, so one build loads unchanged in both
//! Node and Electron.
//!
//! # Why the long calls are tasks
//!
//! Pairing waits for a person to walk a number between two machines, so it can block for
//! minutes. Running that on the JavaScript thread would freeze the window it is drawing. Each
//! one is therefore an [`AsyncTask`], which runs on libuv's thread pool and resolves a promise
//! — the right shape for work that blocks on a socket rather than work that is busy.

mod tasks;

use napi::bindgen_prelude::AsyncTask;
use napi_derive::napi;
use prism_core::identity;
use prism_core::net::pairing::Pin;

use crate::tasks::{PairAsClient, PairAsHost};

/// Returns the Prism version string.
///
/// Used by the Electron shells to confirm the native addon loaded and to report the
/// core version alongside the UI version in diagnostics.
#[napi]
#[must_use]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Returns the wire format revision this build speaks.
///
/// The control plane compares this against `FORMAT_VERSION` from `@prism/protocol`
/// during session setup; a mismatch means the addon and the TypeScript bundle were
/// built from different commits and the session is refused rather than risking a
/// silently misparsed stream.
#[napi]
#[must_use]
pub fn wire_format_version() -> u32 {
    prism_core::net::packet::FORMAT_VERSION
}

/// Returns this machine's public key as hex, creating its long-term key on first use.
///
/// The private half never crosses this boundary and there is no call that would return it.
/// What the user interface needs is the public half: it is what the other machine pins, what
/// a diagnostics panel shows, and what a support conversation refers to.
///
/// # Errors
///
/// Fails if the key cannot be read or written, which on a machine with a home directory means
/// a permissions problem worth surfacing rather than working around.
#[napi]
pub fn identity_public_key() -> napi::Result<String> {
    let path = identity::default_path().map_err(to_napi)?;
    let identity = identity::load_or_create(&path).map_err(to_napi)?;

    Ok(identity::to_hex(identity.public()))
}

/// Returns the public keys of every machine this one has paired with.
///
/// # Errors
///
/// Fails if the store exists but cannot be read. A machine that has never paired returns an
/// empty list rather than an error: that is a state, not a fault.
#[napi]
pub fn paired_peers() -> napi::Result<Vec<String>> {
    let path = identity::default_peers_path().map_err(to_napi)?;

    Ok(identity::known_peers(&path)
        .map_err(to_napi)?
        .iter()
        .map(identity::to_hex)
        .collect())
}

/// Generates a six digit pairing code.
///
/// Returned rather than generated inside [`pair_as_host`] because the code has to be on screen
/// before that call returns, and it does not return until somebody has used it.
///
/// # Errors
///
/// Fails if the platform has no usable randomness, which is a condition no pairing should
/// proceed under.
#[napi]
pub fn generate_pairing_code() -> napi::Result<String> {
    Ok(Pin::generate().map_err(to_napi)?.to_display())
}

/// Waits for one client to pair using `code`, and returns its public key as hex.
///
/// The code is single use: whether the attempt succeeds or fails, this call is finished with
/// it and a second attempt needs a fresh one. That is the whole reason six digits is enough.
///
/// # Errors
///
/// Rejects if nobody pairs before the code expires, if the code was mistyped, or if the
/// address cannot be bound.
#[napi(ts_return_type = "Promise<string>")]
pub fn pair_as_host(bind: String, code: String) -> AsyncTask<PairAsHost> {
    AsyncTask::new(PairAsHost { bind, code })
}

/// Pairs with a host that is showing `code`, and returns its public key as hex.
///
/// # Errors
///
/// Rejects if the code was not accepted — which covers a mistyped code, a code already used,
/// and a machine answering that is not the one showing it, none of which can be told apart —
/// or if the host does not answer at all.
#[napi(ts_return_type = "Promise<string>")]
pub fn pair_as_client(host: String, code: String) -> AsyncTask<PairAsClient> {
    AsyncTask::new(PairAsClient { host, code })
}

/// Turns any error into one JavaScript can throw.
fn to_napi(err: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(err.to_string())
}
