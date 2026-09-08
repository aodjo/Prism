//! The calls that block, run off the JavaScript thread.
//!
//! A session waits on a socket rather than working, which is what libuv's thread pool is for.
//! Nothing here touches a frame, and nothing here ever will: this file is control plane by
//! construction.

use napi::{Env, Task};
use prism_core::identity;

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

/// Builds a host configuration from what the window asked for.
///
/// Every field falls back to the core's default, so a caller that passes an empty object gets
/// a session configured the way the core thinks a session should be.
pub fn host_config(
    options: &crate::HostOptions,
) -> napi::Result<prism_core::control::host::HostConfig> {
    let mut config = prism_core::control::host::HostConfig::default();

    if let Some(bind) = options.bind.as_deref() {
        config.bind = bind.parse().map_err(reason)?;
    }
    if let Some(server) = options.rendezvous.as_deref() {
        config.rendezvous = Some(server.parse().map_err(reason)?);
    }
    if let Some(fps) = options.fps {
        config.fps = fps.clamp(1, 480);
    }
    if let Some(bitrate) = options.bitrate_bps {
        config.bitrate_bps = bitrate.clamp(500_000, 200_000_000);
    }
    if let Some(inject) = options.inject_input {
        config.inject_input = inject;
    }

    Ok(config)
}

/// Loads this machine's identity and every machine on the account that may watch it.
///
/// An empty list is not refused. Sharing is this machine saying its own screen may be watched
/// by the account it belongs to — a statement about itself, true whether or not a second
/// machine exists yet. Somebody who turns it on before installing Prism anywhere else has done
/// nothing wrong, and a switch that refused to move until some other machine appeared would be
/// answering a question nobody asked.
pub fn host_keys() -> napi::Result<prism_core::control::host::HostKeys> {
    let (identity, peers) = load()?;
    let allowed = identity::known_peers(&peers).map_err(reason)?;

    Ok(prism_core::control::host::HostKeys { identity, allowed })
}

/// Turns a snapshot into something JavaScript can read.
pub fn describe(snapshot: &prism_core::control::host::Snapshot) -> crate::HostSnapshot {
    use prism_core::control::host::Phase;

    crate::HostSnapshot {
        phase: match snapshot.phase {
            Phase::Opening => "opening",
            Phase::Waiting => "waiting",
            Phase::Streaming => "streaming",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
        }
        .to_string(),
        observed: snapshot.observed.map(|address| address.to_string()),
        local: snapshot.local.map(|address| address.to_string()),
        peer: snapshot.peer.as_ref().map(identity::to_hex),
        frames: snapshot.frames.into(),
        packets: snapshot.packets.into(),
        bytes: snapshot.bytes.into(),
        bitrate_bps: snapshot.bitrate_bps.into(),
        audio_frames: snapshot.audio_frames.into(),
        error: snapshot.error.clone(),
    }
}

/// The snapshot of a session that has already been stopped.
pub fn stopped_snapshot() -> crate::HostSnapshot {
    crate::HostSnapshot {
        phase: "stopped".to_string(),
        observed: None,
        local: None,
        peer: None,
        frames: 0u64.into(),
        packets: 0u64.into(),
        bytes: 0u64.into(),
        bitrate_bps: 0u64.into(),
        audio_frames: 0u64.into(),
        error: None,
    }
}

/// Derives the secret that signs somebody in, on the thread pool.
///
/// Deliberately slow — a memory-hard hash over a password takes a few hundred milliseconds —
/// which is exactly why it may not run on the thread drawing the window.
pub struct DeriveAuth {
    /// What was typed.
    pub password: String,
    /// The account's salt, as hex.
    pub salt: String,
}

impl Task for DeriveAuth {
    type Output = String;
    type JsValue = String;

    /// Hashes the password and keeps only the half the server is told.
    ///
    /// The wrapping half is derived here too and dropped without leaving this function. It has
    /// no business in JavaScript: nothing in the window needs it, and a value that never
    /// crosses that boundary cannot be logged, serialised, or sent somewhere by mistake.
    fn compute(&mut self) -> napi::Result<Self::Output> {
        use prism_core::account::secret;

        let salt = hex_array::<{ secret::SALT_LEN }>(&self.salt)
            .ok_or_else(|| napi::Error::from_reason("the salt is not hex"))?;

        let secrets = secret::derive(&self.password, &salt).map_err(reason)?;

        Ok(secrets
            .auth
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    /// Hands the secret back as hex.
    fn resolve(&mut self, _env: Env, output: Self::Output) -> napi::Result<Self::JsValue> {
        Ok(output)
    }
}

/// Reads hex into an array of a known size.
fn hex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }

    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect();

    bytes?.try_into().ok()
}
