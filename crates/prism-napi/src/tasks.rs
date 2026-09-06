//! The calls that block, run off the JavaScript thread.
//!
//! Pairing waits for a person: one side puts six digits on a screen and the other waits for
//! somebody to walk them over and type them. That is seconds at best and two minutes at
//! worst, and doing it on the thread drawing the window would freeze the window.
//!
//! Each task here blocks on a socket rather than working, which is what libuv's thread pool is
//! for. None of them touches a frame, and none of them ever will: this file is control plane
//! by construction.

use std::net::SocketAddr;

use napi::{Env, Task};
use prism_core::control::pair;
use prism_core::identity;
use prism_core::net::pairing::Pin;

/// Shows a code and waits for one client to use it.
pub struct PairAsHost {
    /// Address to listen on, as text, because that is what a settings field holds.
    pub bind: String,
    /// The six digits already on screen.
    pub code: String,
}

impl Task for PairAsHost {
    type Output = [u8; 32];
    type JsValue = String;

    /// Runs the exchange on the thread pool.
    fn compute(&mut self) -> napi::Result<Self::Output> {
        let bind: SocketAddr = self.bind.parse().map_err(reason)?;
        let pin = Pin::parse(&self.code).map_err(reason)?;

        let (identity, peers) = load()?;

        pair::host(bind, &identity, &peers, pin).map_err(reason)
    }

    /// Hands the client's key back as hex.
    fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(identity::to_hex(&output))
    }
}

/// Carries a typed code to a host.
pub struct PairAsClient {
    /// Address the host is showing, as text.
    pub host: String,
    /// The six digits a person typed.
    pub code: String,
}

impl Task for PairAsClient {
    type Output = [u8; 32];
    type JsValue = String;

    /// Runs the exchange on the thread pool.
    fn compute(&mut self) -> napi::Result<Self::Output> {
        let host: SocketAddr = self.host.parse().map_err(reason)?;
        let (identity, peers) = load()?;

        pair::client(host, &self.code, &identity, &peers).map_err(reason)
    }

    /// Hands the host's key back as hex.
    fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(identity::to_hex(&output))
    }
}

/// Loads this machine's identity and the path its paired peers are kept at.
fn load() -> napi::Result<(prism_core::net::handshake::Identity, std::path::PathBuf)> {
    let path = identity::default_path().map_err(reason)?;
    let identity = identity::load_or_create(&path).map_err(reason)?;
    let peers = identity::default_peers_path().map_err(reason)?;

    Ok((identity, peers))
}

/// Turns any error into one JavaScript can throw.
fn reason(err: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(err.to_string())
}
