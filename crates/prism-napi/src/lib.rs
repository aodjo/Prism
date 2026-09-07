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

use napi::bindgen_prelude::{AsyncTask, BigInt};
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

/// One thing the system still has to allow.
#[napi(object)]
pub struct MissingGrant {
    /// Which grant this is: `screen` or `input`.
    pub id: String,
    /// What the system's own settings call it.
    pub name: String,
    /// Why a host needs it, in one sentence.
    pub purpose: String,
    /// A link that opens the settings pane holding it.
    pub settings_url: String,
}

/// What this machine currently allows a host to do.
#[napi(object)]
pub struct HostPermissions {
    /// Whether the screen may be recorded, which also covers capturing its audio.
    pub screen: bool,
    /// Whether this machine may be controlled.
    pub input: bool,
    /// What is still missing, ready to show.
    pub missing: Vec<MissingGrant>,
}

/// Returns what the system allows, without prompting for anything.
///
/// Both grants fail quietly when missing — capture delivers black frames and silence,
/// injection posts events that go nowhere — so an interface that did not ask would show a
/// host that looks like it is working and is not.
///
/// `controlling` says whether the session will accept the client's input. A host that only
/// shows its screen is not asked to justify wanting control of the machine.
///
/// Safe to call as often as a window redraws: nothing here prompts.
#[napi]
#[must_use]
pub fn permissions(controlling: bool) -> HostPermissions {
    let held = prism_core::control::permissions::check();

    HostPermissions {
        screen: held.screen,
        input: held.input,
        missing: held
            .missing(controlling)
            .into_iter()
            .map(|grant| MissingGrant {
                id: match grant {
                    prism_core::control::permissions::Grant::Screen => "screen",
                    prism_core::control::permissions::Grant::Input => "input",
                }
                .to_string(),
                name: grant.name().to_string(),
                purpose: grant.purpose().to_string(),
                settings_url: grant.settings_url().to_string(),
            })
            .collect(),
    }
}

/// Asks the system for one grant, and reports whether it is now held.
///
/// The system prompts at most once in the life of an application. A `false` afterwards means
/// the answer is standing and only a person can change it, in the pane `settingsUrl` names —
/// so a caller that gets `false` should offer to open that rather than ask again.
///
/// # Errors
///
/// Fails if `id` is not a grant this build knows.
#[napi]
pub fn request_permission(id: String) -> napi::Result<bool> {
    use prism_core::control::permissions::Grant;

    let grant = match id.as_str() {
        "screen" => Grant::Screen,
        "input" => Grant::Input,
        other => return Err(napi::Error::from_reason(format!("no such grant: {other}"))),
    };

    Ok(prism_core::control::permissions::request(grant))
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

/// A host session the control plane started and can watch.
///
/// One at a time. A second session on one machine would mean two capture streams and two
/// encoders competing for the same GPU, which is slower than either alone and produces a worse
/// picture for both viewers.
#[napi]
pub struct Host {
    service: Option<prism_core::control::host::HostService>,
}

/// What a host session is doing, as of a moment ago.
///
/// Polled by the interface a few times a second and never faster: a panel that updates more
/// often than a person can read costs frames to produce and tells them nothing.
#[napi(object)]
pub struct HostSnapshot {
    /// `opening`, `waiting`, `streaming`, `stopped` or `failed`.
    pub phase: String,
    /// Where the rendezvous server sees this machine, once it has said.
    pub observed: Option<String>,
    /// The address this machine is actually listening on.
    ///
    /// What somebody on the same network has to be told, and the only way to reach this host
    /// when no rendezvous server is configured.
    pub local: Option<String>,
    /// The connected client's public key as hex, once one has connected.
    pub peer: Option<String>,
    /// Frames captured, encoded and sent.
    pub frames: BigInt,
    /// Packets put on the wire, parity included.
    pub packets: BigInt,
    /// Bytes put on the wire, headers included.
    pub bytes: BigInt,
    /// What sending has worked out to so far, in bits per second.
    pub bitrate_bps: BigInt,
    /// Audio frames captured, encoded and sent.
    pub audio_frames: BigInt,
    /// What went wrong, when the phase is `failed`.
    pub error: Option<String>,
}

/// How a host session should behave.
///
/// Everything is optional because a person starting a session from a window has opinions
/// about at most one of these.
#[napi(object)]
pub struct HostOptions {
    /// Address to listen on. Defaults to a port the operating system chooses, which is right
    /// when a rendezvous server is being used.
    pub bind: Option<String>,
    /// Rendezvous server to register with, so clients can find this machine behind NAT.
    pub rendezvous: Option<String>,
    /// Frames per second to capture at.
    pub fps: Option<u32>,
    /// Target bitrate in bits per second.
    pub bitrate_bps: Option<u32>,
    /// Whether to let the client control this machine.
    pub inject_input: Option<bool>,
}

#[napi]
impl Host {
    /// Starts a session and returns immediately.
    ///
    /// Binding, registering and waiting for a client all happen on a thread of their own, so
    /// this does not block the window. [`Host::snapshot`] says what is happening.
    ///
    /// # Errors
    ///
    /// Fails if an address cannot be parsed, if no client has ever been paired — a host that
    /// admits nobody is the right answer rather than an inconvenience — or if the session
    /// thread cannot be spawned.
    #[napi(constructor)]
    pub fn new(options: HostOptions) -> napi::Result<Self> {
        let config = tasks::host_config(&options)?;
        let keys = tasks::host_keys()?;

        Ok(Self {
            service: Some(
                prism_core::control::host::HostService::start(config, keys).map_err(to_napi)?,
            ),
        })
    }

    /// Returns what the session is doing.
    #[napi]
    #[must_use]
    pub fn snapshot(&self) -> HostSnapshot {
        let Some(service) = self.service.as_ref() else {
            return tasks::stopped_snapshot();
        };

        tasks::describe(&service.snapshot())
    }

    /// Ends the session and waits for its thread.
    ///
    /// Safe to call twice: a window that stops a session and then closes would otherwise have
    /// to remember which it did first.
    #[napi]
    pub fn stop(&mut self) {
        if let Some(service) = self.service.take() {
            service.join();
        }
    }
}
