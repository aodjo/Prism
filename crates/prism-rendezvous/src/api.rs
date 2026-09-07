//! The account API, over HTTPS.
//!
//! Everything here happens at human speed — signing in, listing machines, changing a password —
//! so it is the one part of this project where a request costing a millisecond or ten does not
//! matter. That is why it is HTTP over TLS rather than something built on the UDP socket the
//! rest of the server uses: the traffic needs confidentiality and integrity against anyone on
//! the path, and TLS is the answer to that which does not have to be invented.
//!
//! # What crosses the wire
//!
//! The authentication secret does, on every sign-in. The wrapping secret never does — see
//! [`prism_core::account::secret`] for why those are two different things and why it matters
//! that only one of them is here.
//!
//! So the confidentiality TLS provides is doing real work: without it, anyone on the path
//! learns the value that signs somebody in. It does not protect the key, because nothing here
//! could: the key is sealed before it arrives and this server has never held what opens it.
//!
//! # Sessions end when the server restarts
//!
//! Tokens live in memory. A restarted server honours none of them, and everyone signs in
//! again. That is a feature rather than a shortcut: the alternative is tokens that outlive the
//! process that issued them, which means a stolen token outlives everything anybody could do
//! about it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use prism_core::account::secret::{SALT_LEN, SECRET_LEN};
use prism_core::account::totp;
use prism_core::net::handshake::KEY_LEN;
use serde::{Deserialize, Serialize};

use crate::accounts::{AccountError, Accounts, Device, Registration};

/// How long a session lasts without being used.
///
/// Twelve hours: long enough that somebody using their machines through a day is not asked
/// again, short enough that a token taken off a machine left unattended stops working before
/// they are back.
const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// Bytes in a session token.
const TOKEN_LEN: usize = 32;

/// A signed-in session.
#[derive(Debug, Clone)]
struct Session {
    name: String,
    expires_unix: u64,
}

/// Everything the API needs to answer a request.
#[derive(Debug)]
pub struct Service {
    accounts: Mutex<Accounts>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl Service {
    /// Builds the service over an account store.
    #[must_use]
    pub fn new(accounts: Accounts) -> Arc<Self> {
        Arc::new(Self {
            accounts: Mutex::new(accounts),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Returns the name a token belongs to, if it is still good.
    ///
    /// Expired tokens are dropped as they are found rather than swept on a timer. A map that
    /// only grows while somebody is signing in repeatedly is not a map worth a background
    /// thread.
    fn whose(&self, token: &str) -> Option<String> {
        let now = now_unix();
        let mut sessions = self.sessions.lock().ok()?;

        match sessions.get(token) {
            Some(session) if session.expires_unix > now => Some(session.name.clone()),
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
        }
    }

    /// Issues a token for a name.
    fn issue(&self, name: &str) -> Result<String, ApiError> {
        let mut raw = [0u8; TOKEN_LEN];
        getrandom::fill(&mut raw).map_err(|_| ApiError::Unavailable)?;

        let token: String = raw.iter().map(|byte| format!("{byte:02x}")).collect();

        self.sessions
            .lock()
            .map_err(|_| ApiError::Unavailable)?
            .insert(
                token.clone(),
                Session {
                    name: name.to_owned(),
                    expires_unix: now_unix() + SESSION_LIFETIME.as_secs(),
                },
            );

        Ok(token)
    }
}

/// Why a request could not be answered.
#[derive(Debug)]
enum ApiError {
    /// The request was not shaped right.
    Malformed(&'static str),
    /// The name, password or code was wrong, or the token was not good.
    ///
    /// One status for all of them, for the same reason the store has one error: telling them
    /// apart is telling somebody guessing how far they got.
    Refused,
    /// The name is taken, or the account is not in a state the request assumes.
    Conflict(String),
    /// Something on this machine failed.
    Unavailable,
}

impl IntoResponse for ApiError {
    /// Turns a failure into a status and a sentence, and nothing more.
    ///
    /// No detail beyond what the caller can act on. An error that explained which of the name,
    /// the password and the code was wrong would be an error worth guessing against.
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Malformed(what) => (StatusCode::BAD_REQUEST, format!("{what} is malformed")),
            ApiError::Refused => (
                StatusCode::UNAUTHORIZED,
                "the name, password or code is wrong".to_owned(),
            ),
            ApiError::Conflict(reason) => (StatusCode::CONFLICT, reason),
            ApiError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "the server could not complete that".to_owned(),
            ),
        };

        (status, Json(ErrorBody { error: message })).into_response()
    }
}

impl From<AccountError> for ApiError {
    /// Maps a store failure onto a request failure without adding detail.
    fn from(error: AccountError) -> Self {
        match error {
            AccountError::Refused => ApiError::Refused,
            AccountError::NameTaken => ApiError::Conflict("that name is already in use".to_owned()),
            AccountError::BadName { .. } => ApiError::Conflict(error.to_string()),
            AccountError::Malformed { field } => ApiError::Malformed(field),
            AccountError::Store { .. } => ApiError::Unavailable,
        }
    }
}

/// The body every failure carries.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// What the server says about itself.
#[derive(Debug, Serialize)]
struct Health {
    ok: bool,
    accounts: usize,
}

/// Which name a salt is being asked for.
#[derive(Debug, Deserialize)]
struct SaltQuery {
    name: String,
}

/// The salt to hash a password with.
#[derive(Debug, Serialize)]
struct SaltBody {
    salt: String,
}

/// What creating an account needs.
#[derive(Debug, Deserialize)]
struct RegisterBody {
    name: String,
    salt: String,
    auth: String,
    sealed_key: String,
}

/// What creating an account returns, once and never again.
#[derive(Debug, Serialize)]
struct RegisteredBody {
    totp_uri: String,
    totp_secret: String,
}

/// What signing in needs.
#[derive(Debug, Deserialize)]
struct SignInBody {
    name: String,
    auth: String,
    code: u32,
}

/// What signing in returns.
#[derive(Debug, Serialize)]
struct SessionBody {
    token: String,
    sealed_key: String,
    devices: Vec<Device>,
    relay_allowed: bool,
}

/// What adding a machine needs.
#[derive(Debug, Deserialize)]
struct DeviceBody {
    public_key: String,
    label: String,
}

/// What changing a password needs.
#[derive(Debug, Deserialize)]
struct RekeyBody {
    salt: String,
    auth: String,
    sealed_key: String,
}

/// The machines on an account.
#[derive(Debug, Serialize)]
struct DevicesBody {
    devices: Vec<Device>,
}

/// Builds the routes.
///
/// Everything is under a version, because a client that is older than its server has to be
/// able to tell rather than to discover it through a field that is missing.
pub fn routes(service: Arc<Service>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/salt", get(salt))
        .route("/v1/accounts", post(register))
        .route("/v1/sessions", post(sign_in))
        .route("/v1/devices", get(list_devices).post(add_device))
        .route("/v1/devices/{public_key}", delete(remove_device))
        .route("/v1/account/key", put(replace_key))
        .with_state(service)
}

/// Says the server is up and how many accounts it holds.
async fn health(State(service): State<Arc<Service>>) -> Result<Json<Health>, ApiError> {
    let accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;

    Ok(Json(Health {
        ok: true,
        accounts: accounts.len(),
    }))
}

/// Returns the salt to hash a password with.
///
/// Always answers, for any name. See [`Accounts::salt_for`] for why an unknown one is not
/// treated differently.
async fn salt(
    State(service): State<Arc<Service>>,
    Query(query): Query<SaltQuery>,
) -> Result<Json<SaltBody>, ApiError> {
    let accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;

    Ok(Json(SaltBody {
        salt: hex(&accounts.salt_for(&query.name)),
    }))
}

/// Creates an account and hands back the second factor, once.
async fn register(
    State(service): State<Arc<Service>>,
    Json(body): Json<RegisterBody>,
) -> Result<Json<RegisteredBody>, ApiError> {
    let salt = unhex_array::<SALT_LEN>(&body.salt).ok_or(ApiError::Malformed("salt"))?;
    let auth = unhex_array::<SECRET_LEN>(&body.auth).ok_or(ApiError::Malformed("auth"))?;
    let sealed_key = unhex(&body.sealed_key).ok_or(ApiError::Malformed("sealed key"))?;

    let mut accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;
    let secret = accounts.register(Registration {
        name: body.name.clone(),
        salt,
        auth,
        sealed_key,
    })?;

    Ok(Json(RegisteredBody {
        totp_uri: totp::provisioning_uri(&secret, &body.name),
        totp_secret: totp::to_base32(&secret),
    }))
}

/// Checks a sign-in and issues a token.
async fn sign_in(
    State(service): State<Arc<Service>>,
    Json(body): Json<SignInBody>,
) -> Result<Json<SessionBody>, ApiError> {
    let auth = unhex_array::<SECRET_LEN>(&body.auth).ok_or(ApiError::Malformed("auth"))?;

    let (sealed_key, devices, relay_allowed) = {
        let accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;
        let account = accounts.sign_in(&body.name, &auth, body.code, now_unix())?;

        (
            account.sealed_key.clone(),
            account.devices.clone(),
            account.relay_allowed,
        )
    };

    Ok(Json(SessionBody {
        token: service.issue(&body.name)?,
        sealed_key,
        devices,
        relay_allowed,
    }))
}

/// Returns the machines on the signed-in account.
async fn list_devices(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
) -> Result<Json<DevicesBody>, ApiError> {
    let name = bearer(&service, &headers)?;
    let accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;

    Ok(Json(DevicesBody {
        devices: accounts
            .get(&name)
            .ok_or(ApiError::Refused)?
            .devices
            .clone(),
    }))
}

/// Adds a machine to the signed-in account, or renames one already there.
async fn add_device(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Json(body): Json<DeviceBody>,
) -> Result<Json<DevicesBody>, ApiError> {
    let name = bearer(&service, &headers)?;
    let key = unhex_array::<KEY_LEN>(&body.public_key).ok_or(ApiError::Malformed("public key"))?;

    let mut accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;
    accounts.add_device(&name, &key, &body.label, now_unix())?;

    Ok(Json(DevicesBody {
        devices: accounts
            .get(&name)
            .ok_or(ApiError::Refused)?
            .devices
            .clone(),
    }))
}

/// Removes a machine from the signed-in account.
async fn remove_device(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Path(public_key): Path<String>,
) -> Result<Json<DevicesBody>, ApiError> {
    let name = bearer(&service, &headers)?;
    let key = unhex_array::<KEY_LEN>(&public_key).ok_or(ApiError::Malformed("public key"))?;

    let mut accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;
    accounts.remove_device(&name, &key)?;

    Ok(Json(DevicesBody {
        devices: accounts
            .get(&name)
            .ok_or(ApiError::Refused)?
            .devices
            .clone(),
    }))
}

/// Replaces the salt, verifier and sealed key together, which is what a password change is.
async fn replace_key(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Json(body): Json<RekeyBody>,
) -> Result<StatusCode, ApiError> {
    let name = bearer(&service, &headers)?;

    let salt = unhex_array::<SALT_LEN>(&body.salt).ok_or(ApiError::Malformed("salt"))?;
    let auth = unhex_array::<SECRET_LEN>(&body.auth).ok_or(ApiError::Malformed("auth"))?;
    let sealed_key = unhex(&body.sealed_key).ok_or(ApiError::Malformed("sealed key"))?;

    let mut accounts = service.accounts.lock().map_err(|_| ApiError::Unavailable)?;
    accounts.replace_key(&name, &salt, &auth, &sealed_key)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Reads the bearer token and says whose session it is.
fn bearer(service: &Service, headers: &HeaderMap) -> Result<String, ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(ApiError::Refused)?;

    service.whose(token).ok_or(ApiError::Refused)
}

/// Seconds since the epoch.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Renders bytes as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Reads lowercase hex back into bytes.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }

    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Reads hex into an array of a known size.
fn unhex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    unhex(text)?.try_into().ok()
}
