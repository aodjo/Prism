//! The account this machine belongs to, and the machines that belong to it with it.
//!
//! Signing in is what replaced reading six digits off one screen and typing them into another:
//! two machines on the same account are told about each other, and being told is the whole of
//! why one will open a session with the other. So this module is three things — a client for
//! the account server's HTTP API, the small state machine that holds one session, and the one
//! call that decides which machines this one will talk to at all.
//!
//! Everything here happens at human speed. None of it is on the frame path and none of it may
//! ever be.
//!
//! # What no longer crosses into JavaScript
//!
//! One password becomes two secrets and only one of them may be sent: `auth` proves who is
//! signing in, `wrap` unlocks a private key the server must never hold. See
//! [`prism_core::account::secret`] for why they are two values and why a server that has seen
//! every sign-in still cannot open a key.
//!
//! Under Electron that split was kept by the napi boundary. `accountAuth` derived both, dropped
//! the wrapping one, and handed the authentication one back to JavaScript as hex — where three
//! modules carried it on its way into a request body — after the password had already been
//! typed in a window, sent over IPC, and held as a string in a second process.
//!
//! Here the whole of it happens inside [`Client::sign_in`]. The salt is fetched, the secrets
//! are derived, the authentication one is written into the request, and [`secret::Secrets`] —
//! which prints nothing of itself and overwrites both halves when it is dropped — is gone
//! before the reply is read. What crosses from the window is the password on its way in and a
//! picture of the account on its way out. The salt, both derived secrets and the session token
//! are values JavaScript never holds, so none of them can be logged by a renderer, serialised
//! into an error report, or sent somewhere by mistake. The one that used to cross and no longer
//! does is the authentication secret itself, which is the value that signs somebody in.
//!
//! # The shapes on the wire
//!
//! Every request and reply below is the server's own, and `prism-rendezvous` already declares
//! them in its `api.rs`. They are written out again rather than imported because a desktop
//! shell cannot depend on the server crate — that would pull an HTTP server, an async runtime
//! and a mailer into an application that needs none of them — and there is no third crate
//! holding the protocol yet. `prism_core::account` is where they belong, since both ends
//! already depend on it; until they move there, this file is a copy that must not drift.

use std::sync::Mutex;
use std::time::Duration;

use prism_core::account::secret::{self, SALT_LEN};
use prism_core::identity;
use prism_core::net::handshake::KEY_LEN;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use crate::settings;

// The settings state that main.rs manages, under a name that says what it holds: this module
// keeps a `Held` of its own, and two of them in one file would be a coin toss at every call.
use crate::Held as Chosen;

/// How long to wait for the server before giving up.
///
/// Twenty seconds. Long enough for a memory-hard hash on the far side of a slow link, short
/// enough that a window is not frozen on a server that has gone away.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The status a failure that never reached the server carries.
///
/// Zero, because no server sends it. What matters about a status here is telling apart the one
/// that means the session is gone — 401 — from every other answer, and a refusal this machine
/// produced on its own is neither that nor anything the server said.
const NO_ANSWER: u16 = 0;

/// What a request with no body sends, which is nothing.
const NOTHING: Option<&()> = None;

// ── The shapes on the wire ──────────────────────────────────────────────────────────────────

/// One machine on an account, as the server writes it.
///
/// The server also sends when the machine was added. Nothing in this shell shows it, so it is
/// left to `serde` to skip rather than carried as a field no code reads.
#[derive(Debug, Clone, Deserialize)]
struct Device {
    /// The machine's long-term public key, as hex.
    public_key: String,
    /// What its owner calls it.
    label: String,
}

/// The salt to hash a password with.
#[derive(Debug, Deserialize)]
struct SaltBody {
    /// The salt, as hex.
    salt: String,
}

/// What asking for a signup code needs.
#[derive(Debug, Serialize)]
struct ChallengeBody {
    /// The address to prove.
    email: String,
}

/// What asking for one produced.
#[derive(Debug, Deserialize)]
struct ChallengedBody {
    /// Whether a code was sent, and therefore whether one has to be typed back in.
    sent: bool,
}

/// What creating an account needs.
#[derive(Debug, Serialize)]
struct RegisterBody {
    /// The address to sign in with.
    email: String,
    /// The six digits sent to that address, or empty on a server that sends nothing.
    code: String,
    /// The salt the password was hashed with, as hex.
    salt: String,
    /// The authentication secret, as hex. The only half of the password that is ever sent.
    auth: String,
    /// The private key, sealed under the half that is not sent, as hex.
    sealed_key: String,
}

/// What creating an account returns, once and never again.
#[derive(Debug, Deserialize)]
struct RegisteredBody {
    /// The link an authenticator app reads from a QR code.
    totp_uri: String,
    /// The same secret as text.
    totp_secret: String,
}

/// What signing in needs.
#[derive(Debug, Serialize)]
struct SignInBody {
    /// The address the account is under.
    email: String,
    /// The authentication secret, as hex.
    auth: String,
    /// The six digits from an authenticator app.
    code: u32,
}

/// What signing in returns.
///
/// The server also hands back the sealed private key. Neither shell opens a vault yet, so it is
/// skipped here exactly as the Electron client skipped it — and when a vault is wired in, this
/// is the field it comes through.
#[derive(Debug, Deserialize)]
struct SessionBody {
    /// Proves later requests are this account's, until the server restarts or it expires.
    token: String,
    // The account's machines come back here too, and are deliberately not read. This machine
    // tells the account about itself before reading the list, so that the list it reads already
    // has it in — otherwise the first sign-in on a machine shows every computer except the one
    // in front of you. What that call returns is the list this holder keeps.
    /// Whether the relay may be used, which costs bandwidth somebody pays for.
    relay_allowed: bool,
    /// Where this account's machines should register for signalling, as `host:port`.
    ///
    /// Defaulted rather than required, because a server older than this build does not send it
    /// and machines that reach each other directly do not need it.
    #[serde(default)]
    rendezvous: String,
}

/// What a token turns out to be worth, when a client already has one.
#[derive(Debug, Deserialize)]
struct ResumedBody {
    /// The address the token was issued to.
    email: String,
    /// Every machine the account knows.
    devices: Vec<Device>,
    /// Whether the relay may be used.
    relay_allowed: bool,
    /// Where this account's machines should register, or empty when the server does not say.
    #[serde(default)]
    rendezvous: String,
}

/// What adding a machine needs.
#[derive(Debug, Serialize)]
struct DeviceBody {
    /// The machine's long-term public key, as hex.
    public_key: String,
    /// What to call it.
    label: String,
}

/// The machines on an account.
#[derive(Debug, Deserialize)]
struct DevicesBody {
    /// Every machine, the one asking included.
    devices: Vec<Device>,
}

// ── What a window is given ──────────────────────────────────────────────────────────────────

/// One machine on the account, as a window shows it.
///
/// Field names are camelCase on the wire because the windows reading them are the same
/// TypeScript that read them from Electron.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    /// Its long-term public key, as hex.
    pub public_key: String,
    /// What its owner calls it.
    pub label: String,
    /// Whether it is the machine showing this.
    pub is_this_machine: bool,
}

/// What is known about the account right now.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    /// Where the account server is, or empty when none is configured.
    pub server: String,
    /// The address signed in as, or `None` when nobody is.
    pub email: Option<String>,
    /// This machine's own public key as hex, or empty when its identity cannot be read.
    pub public_key: String,
    /// Every machine on the account, this one included.
    pub devices: Vec<DeviceView>,
    /// Whether the account may use the relay.
    pub relay_allowed: bool,
    /// What went wrong the last time something was tried, if anything.
    pub error: Option<String>,
}

/// What creating an account produced, and will not produce again.
///
/// The Electron shell drew the QR code here, in the process rather than in the window, because
/// a renderer could not load anything and it could. A webview draws from a data URI without
/// loading anything either, so the picture is the window's to draw and this hands over what it
/// is a picture of.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Enrolment {
    /// The link an authenticator app reads from a QR code.
    pub totp_uri: String,
    /// The same secret as text, for typing in by hand when a camera is not to hand.
    pub totp_secret: String,
}

// ── Talking to the server ───────────────────────────────────────────────────────────────────

/// A request the account server refused.
///
/// Carries the status because the answers that matter are told apart by it and by nothing else:
/// the server deliberately says the same sentence for a wrong password, a wrong code and an
/// address with no account behind it.
#[derive(Debug)]
struct Refusal {
    /// The HTTP status the server answered with, or [`NO_ANSWER`] when nothing did.
    status: u16,
    /// The sentence to show somebody.
    message: String,
}

impl Refusal {
    /// Builds the refusal for something that never reached the server.
    fn local(message: String) -> Self {
        Self {
            status: NO_ANSWER,
            message,
        }
    }
}

/// Which verb a request uses.
///
/// Three, because three is all this API has. A general HTTP client would take the method as a
/// string; this one takes the three it can actually send, so a typo is a build failure.
#[derive(Debug, Clone, Copy)]
enum Verb {
    /// Read something.
    Get,
    /// Create or replace something.
    Post,
    /// Remove something.
    Delete,
}

/// What came back, before anything has decided what shape it should be.
#[derive(Debug)]
struct Reply {
    /// The status, kept so a body that will not parse can say which answer it came from.
    status: u16,
    /// The parsed body, or an empty object when there was none.
    body: Value,
}

impl Reply {
    /// Reads the body into the shape this build expects.
    ///
    /// # Errors
    ///
    /// Fails if the server answered with something this build has no name for, which means the
    /// two halves came from different commits rather than that anybody did anything wrong.
    fn read<T: DeserializeOwned>(self) -> Result<T, Refusal> {
        serde_json::from_value(self.body).map_err(|error| Refusal {
            status: self.status,
            message: format!(
                "the account server sent an answer this build could not read: {error}"
            ),
        })
    }
}

/// The account server, as the thing this shell talks to.
///
/// Holds the session token so that no caller has to remember to attach it, which is the way
/// that eventually gets forgotten on exactly one endpoint.
#[derive(Debug)]
struct Client {
    /// Where the server is, without a trailing slash.
    base: String,
    /// The current session, or `None` when nobody is signed in.
    token: Option<String>,
    /// The connection pool, so a sign-in and the two calls after it share one handshake.
    agent: ureq::Agent,
}

impl Client {
    /// Points a client at a server.
    ///
    /// The address is trimmed here as well as where it is compared, so that one written with a
    /// trailing slash is one server rather than two.
    ///
    /// Status codes are not treated as transport failures, because the body of a refusal is the
    /// sentence somebody has to read and a client that threw the response away would leave
    /// nothing to show but a number.
    fn new(base: &str) -> Self {
        Self {
            base: base.trim().trim_end_matches('/').to_owned(),
            token: None,
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(TIMEOUT))
                .build()
                .new_agent(),
        }
    }

    /// Asks the server to send a signup code to an address.
    ///
    /// Nothing is created by this. The account comes into being only when the code comes back,
    /// which is what stops somebody registering an address they do not own.
    ///
    /// # Errors
    ///
    /// Fails if the address already has an account or is malformed, if the code could not be
    /// sent, or if the server cannot be reached. Returns `false` rather than failing on a server
    /// with no mail configured, where registering asks for nothing.
    fn challenge(&mut self, email: &str) -> Result<bool, Refusal> {
        let body = ChallengeBody {
            email: email.to_owned(),
        };

        let reply = self.send(Verb::Post, "/v1/accounts/challenge", Some(&body))?;

        Ok(reply.read::<ChallengedBody>()?.sent)
    }

    /// Creates an account and returns what to put into an authenticator app.
    ///
    /// The second factor is shown once. There is no way to ask for it again: the server keeps
    /// only what it needs to check codes, which is not enough to show the secret a second time —
    /// and handing it out later would mean a password alone could fetch the thing the password
    /// is supposed to be paired with.
    ///
    /// # Errors
    ///
    /// Fails if the code is wrong or lapsed, if the address already has an account or is
    /// malformed, if the password is too short to hash, or if the server cannot be reached.
    fn register(&mut self, email: &str, password: &str, code: &str) -> Result<Enrolment, Refusal> {
        let salt = self.salt_for(email)?;

        let body = RegisterBody {
            email: email.to_owned(),
            code: code.to_owned(),
            auth: derive_auth(password, &salt)?,
            salt,
            // Empty because neither shell seals a key yet, and the server takes an empty one.
            // The day a vault is wired in, the sealed key goes here and nowhere else.
            sealed_key: String::new(),
        };

        let reply = self.send(Verb::Post, "/v1/accounts", Some(&body))?;
        let registered: RegisteredBody = reply.read()?;

        Ok(Enrolment {
            totp_uri: registered.totp_uri,
            totp_secret: registered.totp_secret,
        })
    }

    /// Signs in, and remembers the session for later calls.
    ///
    /// The password is turned into secrets and the wrapping half is destroyed inside this
    /// function. Nothing else in this process, and nothing at all in the window, ever holds
    /// either half.
    ///
    /// # Errors
    ///
    /// Fails if any of the address, the password and the code is wrong, which the server reports
    /// as one failure so that a refusal says nothing about how close a guess was.
    fn sign_in(&mut self, email: &str, password: &str, code: &str) -> Result<SessionBody, Refusal> {
        let salt = self.salt_for(email)?;

        let body = SignInBody {
            email: email.to_owned(),
            auth: derive_auth(password, &salt)?,
            code: code.trim().parse().map_err(|_| {
                Refusal::local("A code from an authenticator app is six digits.".to_owned())
            })?,
        };

        let session: SessionBody = self.send(Verb::Post, "/v1/sessions", Some(&body))?.read()?;

        self.token = Some(session.token.clone());

        Ok(session)
    }

    /// Signs in with a token kept from a previous run.
    ///
    /// The token is checked by being used, which is the only check worth anything: a token that
    /// looks well-formed and has expired is indistinguishable from a good one until the server
    /// says otherwise.
    ///
    /// # Errors
    ///
    /// Returns `Ok(None)` when the token is no longer good, which is not a failure — it is the
    /// answer. Fails when the server could not be reached, which is a different thing entirely
    /// and must not throw the token away.
    fn resume(&mut self, token: &str) -> Result<Option<ResumedBody>, Refusal> {
        self.token = Some(token.to_owned());

        match self.send(Verb::Get, "/v1/session", NOTHING) {
            Ok(reply) => reply.read().map(Some),
            // The token is already forgotten by the time this arrives: a 401 clears it wherever
            // it happens, because a credential the server has stopped honouring is not one to
            // offer again on the next call.
            Err(refusal) if refusal.status == 401 => Ok(None),
            Err(refusal) => {
                self.token = None;
                Err(refusal)
            }
        }
    }

    /// Tells the account about this machine, and returns every machine it now knows.
    ///
    /// Signing in again on a machine already listed renames it rather than listing it twice,
    /// which is why renaming needs no endpoint of its own.
    ///
    /// # Errors
    ///
    /// Fails if the session has expired or the key is malformed.
    fn register_device(&mut self, public_key: &str, label: &str) -> Result<Vec<Device>, Refusal> {
        let body = DeviceBody {
            public_key: public_key.to_owned(),
            label: label.to_owned(),
        };

        let reply = self.send(Verb::Post, "/v1/devices", Some(&body))?;

        Ok(reply.read::<DevicesBody>()?.devices)
    }

    /// Removes a machine from the account and returns what is left.
    ///
    /// # Errors
    ///
    /// Fails if the session has expired.
    fn forget_device(&mut self, public_key: &str) -> Result<Vec<Device>, Refusal> {
        let path = format!("/v1/devices/{}", escape(public_key));
        let reply = self.send(Verb::Delete, &path, NOTHING)?;

        Ok(reply.read::<DevicesBody>()?.devices)
    }

    /// Ends the session, here and on the server.
    ///
    /// The server is told rather than left to expire the token on its own, because sessions
    /// survive a restart: a token this client merely forgot would go on working for the rest of
    /// its twelve hours in the hands of anybody who had read it.
    ///
    /// Forgetting happens either way. Somebody signing out on a machine they are about to hand
    /// over should not stay signed in on it because the network was down.
    fn sign_out(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };

        // Nothing to do about a failure and nothing to say: the token is gone from this machine,
        // and the server drops it when it expires.
        let _ = self.send_with(Some(&token), Verb::Delete, "/v1/session", NOTHING);
    }

    /// Fetches the salt a password must be hashed with.
    ///
    /// Every address gets an answer, including one with no account — otherwise this call would
    /// be a way to find out which addresses are registered, and an address is half of what
    /// somebody guessing needs.
    ///
    /// # Errors
    ///
    /// Fails if the server cannot be reached.
    fn salt_for(&mut self, email: &str) -> Result<String, Refusal> {
        let path = format!("/v1/salt?email={}", escape(email));
        let reply = self.send(Verb::Get, &path, NOTHING)?;

        Ok(reply.read::<SaltBody>()?.salt)
    }

    /// Sends one request under the session this client holds.
    ///
    /// # Errors
    ///
    /// Fails if the server refused, or could not be reached in time.
    fn send<B: Serialize>(
        &mut self,
        verb: Verb,
        path: &str,
        body: Option<&B>,
    ) -> Result<Reply, Refusal> {
        let token = self.token.clone();

        self.send_with(token.as_deref(), verb, path, body)
    }

    /// Sends one request under a named token rather than the current one.
    ///
    /// Exists for signing out, which has to use a token it has already given up.
    ///
    /// # Errors
    ///
    /// Fails if the server refused, or could not be reached in time.
    fn send_with<B: Serialize>(
        &mut self,
        token: Option<&str>,
        verb: Verb,
        path: &str,
        body: Option<&B>,
    ) -> Result<Reply, Refusal> {
        let url = format!("{}{path}", self.base);

        let outcome = match verb {
            Verb::Get => authorised(self.agent.get(&url), token).call(),
            Verb::Delete => authorised(self.agent.delete(&url), token).call(),
            Verb::Post => {
                let request = authorised(self.agent.post(&url), token);

                match body {
                    Some(value) => request.send_json(value),
                    None => request.send_empty(),
                }
            }
        };

        // A name that does not resolve, a machine that is not there, a certificate that is not
        // trusted. None of them is something the caller can tell apart, and all of them mean the
        // same thing to somebody looking at a window.
        let mut response = outcome.map_err(|error| {
            Refusal::local(format!("the account server could not be reached: {error}"))
        })?;

        let status = response.status().as_u16();

        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| Refusal {
                status,
                message: format!("the account server's answer could not be read: {error}"),
            })?;

        let body = safe_parse(&text);

        if !(200..300).contains(&status) {
            // The session is gone rather than merely refused for this call, so holding onto it
            // would have every later request fail the same way with no sign of why.
            if status == 401 {
                self.token = None;
            }

            return Err(Refusal {
                status,
                message: body.get("error").and_then(Value::as_str).map_or_else(
                    || format!("the account server answered {status}"),
                    str::to_owned,
                ),
            });
        }

        Ok(Reply { status, body })
    }
}

/// Attaches the session token to a request, when there is one.
fn authorised<S>(request: ureq::RequestBuilder<S>, token: Option<&str>) -> ureq::RequestBuilder<S> {
    match token {
        Some(token) => request.header("authorization", format!("Bearer {token}")),
        None => request,
    }
}

// ── The account, as this machine holds it ───────────────────────────────────────────────────

/// Holds one machine's account.
///
/// Everything it knows came from the server the last time it answered. Nothing here is this
/// machine's own opinion: the list of machines is the account's, and the moment somebody signs
/// in somewhere else it is out of date until the next time this asks.
#[derive(Debug, Default)]
struct Holder {
    /// The client, rebuilt whenever the server address changes.
    client: Option<Client>,
    /// The address signed in as, or `None`.
    email: Option<String>,
    /// Every machine the account knows, as of the last time it said.
    devices: Vec<DeviceView>,
    /// Whether the account may use the relay.
    relay_allowed: bool,
    /// What went wrong the last time the account was asked something.
    trouble: Option<String>,
    /// Whether the token an earlier run kept has been offered to the server yet.
    ///
    /// Once per run, on the first question a window asks. The Electron shell started this at
    /// launch and had windows wait on it; a command here already runs off the drawing thread,
    /// so the first question can simply take as long as it takes rather than being answered
    /// "signed out" and corrected a moment later — which is what a window draws as having been
    /// signed out.
    tried_resume: bool,
}

impl Holder {
    /// Describes the account for a window, resuming a kept session on the first call of a run.
    fn view(&mut self, chosen: &Chosen) -> AccountState {
        if !self.tried_resume {
            self.tried_resume = true;
            self.resume(chosen);
        }

        self.snapshot(chosen)
    }

    /// Asks the server to send a signup code to an address.
    ///
    /// # Errors
    ///
    /// Fails if no server is configured, or the server refused.
    fn challenge(&mut self, chosen: &Chosen, email: &str) -> Result<bool, String> {
        self.reach(chosen)?;
        self.trouble = None;

        self.client_mut()?
            .challenge(email)
            .map_err(|refusal| refusal.message)
    }

    /// Creates an account and returns what to put into an authenticator app.
    ///
    /// # Errors
    ///
    /// Fails if no server is configured, or the server refused.
    fn register(
        &mut self,
        chosen: &Chosen,
        email: &str,
        password: &str,
        code: &str,
    ) -> Result<Enrolment, String> {
        self.reach(chosen)?;
        self.trouble = None;

        self.client_mut()?
            .register(email, password, code)
            .map_err(|refusal| refusal.message)
    }

    /// Signs in, tells the account about this machine, and trusts every machine it names.
    ///
    /// # Errors
    ///
    /// Fails if no server is configured, or if any of the address, password and code is wrong,
    /// which is reported as one failure.
    fn sign_in(
        &mut self,
        chosen: &Chosen,
        email: &str,
        password: &str,
        code: &str,
        label: &str,
    ) -> Result<AccountState, String> {
        // Outside what is reported below, because a machine with no account server configured
        // has nothing to say about an account: that is a setting to fill in, not a failure to
        // remember and show under the form.
        self.reach(chosen)?;

        match self.establish(chosen, email, password, code, label) {
            Ok(()) => Ok(self.snapshot(chosen)),
            Err(trouble) => {
                self.email = None;
                self.trouble = Some(trouble.clone());

                Err(trouble)
            }
        }
    }

    /// Ends the session, here and on the server.
    fn sign_out(&mut self, chosen: &Chosen) -> AccountState {
        if let Some(client) = self.client.as_mut() {
            client.sign_out();
        }

        forget_token();

        self.email = None;
        self.trouble = None;

        // The machines stay trusted. They were on the account, and signing out is not a
        // statement that they are not yours.
        self.devices.clear();

        self.snapshot(chosen)
    }

    /// Renames this machine on the account.
    ///
    /// The same call that registered it: the server keeps one entry per key, so registering a
    /// key it already has is how a label is changed. Which means renaming needs no endpoint of
    /// its own, and cannot leave a machine listed twice under two names.
    ///
    /// # Errors
    ///
    /// Fails if nobody is signed in, or the server refuses.
    fn rename(&mut self, chosen: &Chosen, label: &str) -> Result<AccountState, String> {
        self.reach(chosen)?;

        self.report(chosen, |holder| {
            let mine = mine()?;
            let listed = holder
                .client_mut()?
                .register_device(&identity::to_hex(&mine), label)
                .map_err(|refusal| refusal.message)?;

            holder.adopt(&listed, &mine)
        })
    }

    /// Removes a machine from the account.
    ///
    /// # Errors
    ///
    /// Fails if nobody is signed in, or the server refuses.
    fn forget(&mut self, chosen: &Chosen, public_key: &str) -> Result<AccountState, String> {
        self.reach(chosen)?;

        self.report(chosen, |holder| {
            let mine = mine()?;
            let listed = holder
                .client_mut()?
                .forget_device(public_key)
                .map_err(|refusal| refusal.message)?;

            holder.adopt(&listed, &mine)
        })
    }

    /// Asks the account again who its machines are, and says whether they changed.
    ///
    /// The list is not a thing this machine decides, so it goes stale the moment somebody signs
    /// in somewhere else. Cheap enough to do whenever a window comes forward, which is the
    /// moment somebody is about to look at the list and expect it to be right.
    fn refresh(&mut self, chosen: &Chosen) -> bool {
        let before = self.listed();

        self.resume(chosen);

        self.listed() != before
    }

    /// Signs in with the token an earlier run kept, if there is one and it is still good.
    ///
    /// A server that cannot be reached is not the same as a token that has expired: the first is
    /// temporary and the token stays, the second is permanent and it goes. Treating them alike
    /// would sign somebody out of their own account because their network was down for a minute.
    fn resume(&mut self, chosen: &Chosen) {
        let Some(token) = stored_token() else {
            return;
        };

        // A machine with no account server configured has nothing to resume against, and that is
        // not a failure worth showing: it is a machine nobody has finished setting up.
        if self.reach(chosen).is_err() {
            return;
        }

        // Answered before anything is recorded, so that the client is no longer borrowed by the
        // time what it said is written down.
        let outcome = match self.client_mut() {
            Ok(client) => client.resume(&token),
            Err(_) => return,
        };

        match outcome {
            Ok(None) => forget_token(),
            Ok(Some(session)) => match self.settle(&session) {
                Ok(()) => {
                    adopt_rendezvous(chosen, &session.rendezvous);
                    self.trouble = None;
                }
                Err(trouble) => self.trouble = Some(trouble),
            },
            Err(refusal) => self.trouble = Some(refusal.message),
        }
    }

    /// Signs in and puts this machine on the account, in that order.
    ///
    /// # Errors
    ///
    /// Fails at the first step that does, and the caller is what reports it: every failure here
    /// leaves the account signed out rather than half signed in.
    fn establish(
        &mut self,
        chosen: &Chosen,
        email: &str,
        password: &str,
        code: &str,
        label: &str,
    ) -> Result<(), String> {
        let mine = mine()?;

        let session = self
            .client_mut()?
            .sign_in(email, password, code)
            .map_err(|refusal| refusal.message)?;

        self.email = Some(email.to_owned());
        self.relay_allowed = session.relay_allowed;

        // This machine tells the account about itself before reading the list, so that the list
        // it reads already has it in — otherwise the first sign-in on a machine shows every
        // computer except the one in front of you.
        let listed = self
            .client_mut()?
            .register_device(&identity::to_hex(&mine), label)
            .map_err(|refusal| refusal.message)?;

        self.adopt(&listed, &mine)?;
        adopt_rendezvous(chosen, &session.rendezvous);
        self.trouble = None;

        // Kept only once both halves have worked. A token stored before this machine had been
        // registered would come back to a list that does not have it in.
        keep_token(&session.token);

        Ok(())
    }

    /// Records what a resumed session said.
    ///
    /// The address and the relay are taken before the machines are, so that a list this build
    /// cannot make sense of still leaves a window able to say who is signed in.
    ///
    /// # Errors
    ///
    /// Fails if a machine on the account is not named by a public key, or the list of machines
    /// this one will talk to cannot be written.
    fn settle(&mut self, session: &ResumedBody) -> Result<(), String> {
        self.email = Some(session.email.clone());
        self.relay_allowed = session.relay_allowed;

        let mine = mine()?;

        self.adopt(&session.devices, &mine)
    }

    /// Records what the account said, and trusts every machine it named.
    ///
    /// Trusting is the point of the whole arrangement: two machines signed in to the same
    /// account are told about each other, and that is the only thing that makes one willing to
    /// talk to the other.
    ///
    /// # Errors
    ///
    /// Fails if a machine is not named by a public key, or the list cannot be written. The
    /// account's answer is not recorded when it cannot be acted on, because a window showing
    /// machines this one will not talk to is a window that lies.
    fn adopt(&mut self, devices: &[Device], mine: &[u8; KEY_LEN]) -> Result<(), String> {
        trust(devices, mine)?;
        self.devices = views(devices, &identity::to_hex(mine));

        Ok(())
    }

    /// Returns a client for the configured server, building one if the address has changed.
    ///
    /// Rebuilt rather than reconfigured, because a client holds a session and a session belongs
    /// to the server that issued it. Carrying one across a change of address would send
    /// somebody's token to a machine that never gave it to them.
    ///
    /// # Errors
    ///
    /// Fails if no server is configured, which is a setting to fill in rather than a fault.
    fn reach(&mut self, chosen: &Chosen) -> Result<(), String> {
        // Trimmed the same way the client trims its own base, so an address written with a
        // trailing slash is not read as a different server on every call — which would throw the
        // session away each time somebody asked anything.
        let server = server_of(chosen);
        let server = server.trim().trim_end_matches('/');

        if server.is_empty() {
            self.client = None;
            self.email = None;

            return Err("set an account server first".to_owned());
        }

        if self
            .client
            .as_ref()
            .is_some_and(|client| client.base != server)
        {
            // A token is only worth anything to the server that issued it, so pointing a machine
            // at a different one throws it away rather than offering it to a stranger. Building
            // the first client of a run is not that: there is nothing to point away from, and
            // the token waiting in the store is the one that client is about to use.
            forget_token();
            self.client = None;
        }

        if self.client.is_none() {
            self.client = Some(Client::new(server));
            self.email = None;
        }

        Ok(())
    }

    /// Returns the client [`Holder::reach`] built.
    ///
    /// # Errors
    ///
    /// Fails only when called without reaching first, which no path here does.
    fn client_mut(&mut self) -> Result<&mut Client, String> {
        self.client
            .as_mut()
            .ok_or_else(|| "no account server is configured".to_owned())
    }

    /// Runs something against the account and remembers how it went.
    ///
    /// The two calls that change the machine list share this, so that both leave the same trace:
    /// a failure is what a window shows under the list until the next thing works, and a success
    /// clears it rather than leaving yesterday's complaint on screen.
    ///
    /// # Errors
    ///
    /// Whatever the given step failed with, after recording it.
    fn report(
        &mut self,
        chosen: &Chosen,
        act: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<AccountState, String> {
        match act(self) {
            Ok(()) => {
                self.trouble = None;

                Ok(self.snapshot(chosen))
            }
            Err(trouble) => {
                self.trouble = Some(trouble.clone());

                Err(trouble)
            }
        }
    }

    /// The account's machines as one comparable value.
    fn listed(&self) -> Vec<String> {
        self.devices
            .iter()
            .map(|device| device.public_key.clone())
            .collect()
    }

    /// Describes the account without asking anything.
    fn snapshot(&self, chosen: &Chosen) -> AccountState {
        AccountState {
            server: server_of(chosen),
            email: self.email.clone(),
            // Empty when this machine's identity cannot be read, which is what the window already
            // draws before it has asked. A machine that cannot read its own key has a problem
            // every other call will report; refusing to describe the account over it would only
            // hide the account as well.
            public_key: mine().map(|key| identity::to_hex(&key)).unwrap_or_default(),
            devices: self.devices.clone(),
            relay_allowed: self.relay_allowed,
            error: self.trouble.clone(),
        }
    }
}

/// The account as this machine holds it.
///
/// One lock around the whole of it rather than one per field, because every operation is a
/// network round trip that changes several at once, and none of them is asked for often enough
/// for the contention to be worth thinking about.
pub struct Held(Mutex<Holder>);

impl Held {
    /// Builds a holder with nothing resumed yet.
    ///
    /// Nothing reaches the network here. The kept session is offered the first time a window
    /// asks what is known, so that a launch is not held up by a server that is slow to answer.
    #[must_use]
    pub fn new() -> Self {
        Self(Mutex::new(Holder::default()))
    }

    /// Runs something against the held account.
    ///
    /// # Errors
    ///
    /// Fails if a thread panicked while holding the account. Unlike a list of finished sessions,
    /// what this guards is halfway through a sign-in when that happens, and carrying on with it
    /// would mean acting on a session nobody knows the state of.
    fn with<T>(&self, act: impl FnOnce(&mut Holder) -> T) -> Result<T, String> {
        let mut holder = self
            .0
            .lock()
            .map_err(|_| "the account lock was poisoned".to_owned())?;

        Ok(act(&mut holder))
    }
}

impl Default for Held {
    /// A holder with nothing resumed yet, which is the only state a launch can start from.
    fn default() -> Self {
        Self::new()
    }
}

// ── What the settings say, and what the account writes back ─────────────────────────────────

/// Where the account server is, or empty when none is configured.
fn server_of(chosen: &Chosen) -> String {
    chosen
        .0
        .lock()
        .map(|settings| settings.account_server.clone())
        .unwrap_or_default()
}

/// Takes the signalling address the account handed over.
///
/// Anything already configured by hand wins: somebody who typed an address meant it, and an
/// account should not quietly replace it.
///
/// Written through the settings this process holds rather than straight to the file, so that the
/// copy a window is about to ask for is the copy that was saved.
fn adopt_rendezvous(chosen: &Chosen, address: &str) {
    if address.is_empty() {
        return;
    }

    let Ok(mut settings) = chosen.0.lock() else {
        return;
    };

    if !settings.rendezvous.trim().is_empty() {
        return;
    }

    settings.rendezvous = address.to_owned();

    // A signalling address that could not be written is one this machine asks for again next
    // launch, which is not a reason to fail at somebody who has just signed in.
    let _ = settings::save(&settings);
}

// ── This machine, and the machines it will talk to ──────────────────────────────────────────

/// This machine's long-term public key, creating it on first use.
///
/// # Errors
///
/// Fails if the key cannot be read or written, which on a machine with a home directory means a
/// permissions problem worth showing rather than working around.
fn mine() -> Result<[u8; KEY_LEN], String> {
    let path = identity::default_path().map_err(|error| error.to_string())?;
    let identity = identity::load_or_create(&path).map_err(|error| error.to_string())?;

    Ok(*identity.public())
}

/// Makes the account's machines the only ones this one will open a session with.
///
/// The list is replaced rather than added to. Only machines on the account may reach this one,
/// so the account's answer is the whole answer — a key that stayed behind after it left the
/// account, or one recorded by the pairing this replaced, would otherwise still be admitted with
/// nothing on any screen to say so.
///
/// This machine's own key is left out rather than refused, because the account lists it too and a
/// machine that trusted itself would offer itself as somewhere to connect.
///
/// # Errors
///
/// Fails if a machine is not named by a public key, or if the list cannot be written.
fn trust(devices: &[Device], mine: &[u8; KEY_LEN]) -> Result<(), String> {
    let path = identity::default_peers_path().map_err(|error| error.to_string())?;
    let mut theirs: Vec<[u8; KEY_LEN]> = Vec::with_capacity(devices.len());

    for device in devices {
        let key = identity::parse_peer_key(&device.public_key)?;

        if key != *mine && !theirs.contains(&key) {
            theirs.push(key);
        }
    }

    identity::set_peers(&path, &theirs).map_err(|error| error.to_string())
}

/// Turns what the account listed into what a window shows.
fn views(devices: &[Device], mine: &str) -> Vec<DeviceView> {
    devices
        .iter()
        .map(|device| DeviceView {
            public_key: device.public_key.clone(),
            label: device.label.clone(),
            is_this_machine: device.public_key == mine,
        })
        .collect()
}

// ── Keeping the session between runs ────────────────────────────────────────────────────────

/// Keeps the session token for the next run, when this machine can seal it.
///
/// A machine with no secret store keeps nothing and asks for a password again next launch. That
/// is the whole of what a failure here costs, which is why it is not reported: the alternative
/// is a plain file, and that would make the machine with no keyring the one machine where the
/// token sits in readable text.
fn keep_token(token: &str) {
    store::write(token.as_bytes());
}

/// Reads back the token an earlier run kept.
///
/// Anything unreadable — no secret store, no item, an item written under a key that no longer
/// exists — is treated as nobody being signed in.
fn stored_token() -> Option<String> {
    String::from_utf8(store::read()?)
        .ok()
        .filter(|token| !token.is_empty())
}

/// Removes the kept session.
///
/// Also removes the copy the Electron shell sealed, because the two shells share one profile
/// directory and signing out is a thing somebody does because they want it to stop being
/// possible to sign in. A token this shell cannot read is still a token, and leaving it behind
/// would mean signing out here and finding yourself signed in over there.
fn forget_token() {
    store::forget();

    if let Some(path) = settings::profile_dir().map(|dir| dir.join("session.bin")) {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether this machine has a session it could resume.
///
/// Read from the store rather than from what has been resumed, because a launch asks this before
/// it has had time to reach the server — and the question is whether somebody signed in here,
/// not whether the server can be reached right now.
#[must_use]
pub fn signed_in_before() -> bool {
    stored_token().is_some()
}

/// The operating system's own secret store.
///
/// A token is a credential: whoever holds it is the account until it expires. So it goes into the
/// keychain rather than into a file beside the window size, and on a system this has no way to
/// reach one, nothing is written at all.
///
/// The Electron shell reached the same store through `safeStorage`, under a key of its own. This
/// one keeps its own item rather than trying to open that, so the two shells do not share a
/// session even though they share a profile — somebody moving between them signs in once more.
#[cfg(target_os = "macos")]
mod store {
    use std::ffi::{c_char, c_void};
    use std::ptr;

    /// What the item is filed under, matching the profile directory the settings share.
    const SERVICE: &str = "@prism/client";

    /// The name within that service.
    const ACCOUNT: &str = "session";

    /// What every one of these calls returns when it worked.
    const OK: i32 = 0;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        /// Adds an item, failing if one is already filed under the same two names.
        fn SecKeychainAddGenericPassword(
            keychain: *mut c_void,
            service_name_length: u32,
            service_name: *const c_char,
            account_name_length: u32,
            account_name: *const c_char,
            password_length: u32,
            password_data: *const c_void,
            item: *mut *mut c_void,
        ) -> i32;

        /// Finds an item, writing out its data, its reference, or neither.
        fn SecKeychainFindGenericPassword(
            keychain_or_array: *const c_void,
            service_name_length: u32,
            service_name: *const c_char,
            account_name_length: u32,
            account_name: *const c_char,
            password_length: *mut u32,
            password_data: *mut *mut c_void,
            item: *mut *mut c_void,
        ) -> i32;

        /// Releases the data a find wrote out.
        fn SecKeychainItemFreeContent(attributes: *mut c_void, data: *mut c_void) -> i32;

        /// Deletes an item a find returned.
        fn SecKeychainItemDelete(item: *mut c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        /// Gives up a reference a find returned.
        fn CFRelease(value: *const c_void);
    }

    /// The length of a name, as this API counts it.
    fn length_of(name: &str) -> u32 {
        u32::try_from(name.len()).unwrap_or(0)
    }

    /// Reads the kept secret, or nothing when there is none.
    pub fn read() -> Option<Vec<u8>> {
        let mut length: u32 = 0;
        let mut data: *mut c_void = ptr::null_mut();

        // SAFETY: the two names are borrowed for the length of the call and the lengths passed
        // are their own. `length` and `data` are live locals this writes once each, and a null
        // item pointer is how this API is told the caller does not want a reference back.
        let status = unsafe {
            SecKeychainFindGenericPassword(
                ptr::null(),
                length_of(SERVICE),
                SERVICE.as_ptr().cast::<c_char>(),
                length_of(ACCOUNT),
                ACCOUNT.as_ptr().cast::<c_char>(),
                &raw mut length,
                &raw mut data,
                ptr::null_mut(),
            )
        };

        if status != OK || data.is_null() {
            return None;
        }

        // SAFETY: the call reported success, so `data` points at `length` bytes it allocated and
        // they stay valid until they are handed back below.
        let secret =
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length as usize) }.to_vec();

        // SAFETY: `data` is what the call above allocated, nothing else has freed it, and no
        // attribute list was asked for.
        unsafe { SecKeychainItemFreeContent(ptr::null_mut(), data) };

        Some(secret)
    }

    /// Keeps a secret, replacing whatever was under the same names.
    ///
    /// Deleted and added rather than edited, because the keychain refuses a second item under
    /// one pair of names and this is one call fewer than asking what is there first.
    pub fn write(secret: &[u8]) {
        forget();

        let Ok(length) = u32::try_from(secret.len()) else {
            return;
        };

        // SAFETY: the names and the secret are all borrowed for the length of the call and the
        // lengths passed are their own. A null keychain is the default one, and a null item
        // pointer is how this API is told the caller does not want a reference back.
        unsafe {
            SecKeychainAddGenericPassword(
                ptr::null_mut(),
                length_of(SERVICE),
                SERVICE.as_ptr().cast::<c_char>(),
                length_of(ACCOUNT),
                ACCOUNT.as_ptr().cast::<c_char>(),
                length,
                secret.as_ptr().cast::<c_void>(),
                ptr::null_mut(),
            )
        };
    }

    /// Removes the kept secret, if there is one.
    pub fn forget() {
        let mut item: *mut c_void = ptr::null_mut();
        let mut length: u32 = 0;
        let mut data: *mut c_void = ptr::null_mut();

        // SAFETY: as in `read`, with a reference asked for as well because that is what deleting
        // an item takes.
        let status = unsafe {
            SecKeychainFindGenericPassword(
                ptr::null(),
                length_of(SERVICE),
                SERVICE.as_ptr().cast::<c_char>(),
                length_of(ACCOUNT),
                ACCOUNT.as_ptr().cast::<c_char>(),
                &raw mut length,
                &raw mut data,
                &raw mut item,
            )
        };

        if status != OK {
            return;
        }

        if !data.is_null() {
            // SAFETY: `data` is what the find allocated and nothing else has freed it.
            unsafe { SecKeychainItemFreeContent(ptr::null_mut(), data) };
        }

        if !item.is_null() {
            // SAFETY: `item` is the reference the find returned, owned by this function, deleted
            // once and then given up once.
            unsafe {
                SecKeychainItemDelete(item);
                CFRelease(item.cast_const());
            }
        }
    }
}

/// The operating system's own secret store.
///
/// There is none reached from here on this platform yet, so nothing is kept and every launch
/// asks for a password again. That is deliberate rather than unfinished in one respect: the
/// alternative is a plain file, which would make the machine with no keyring the one machine
/// where a credential sits in readable text, and that is exactly backwards.
#[cfg(not(target_os = "macos"))]
mod store {
    /// Reads nothing, because nothing was kept.
    pub fn read() -> Option<Vec<u8>> {
        None
    }

    /// Keeps nothing, because there is nowhere safe to keep it.
    pub fn write(_secret: &[u8]) {}

    /// Removes nothing, because nothing was kept.
    pub fn forget() {}
}

// ── Small things the protocol needs ─────────────────────────────────────────────────────────

/// Derives the secret that signs somebody in, and destroys the one that does not.
///
/// Both halves are derived — that is what the hash produces — and only this one survives the
/// call. The wrapping half is overwritten when [`secret::Secrets`] is dropped, at the end of this
/// function, having been read by nothing.
///
/// # Errors
///
/// Fails if the salt is not the length the server should have sent, or if the password is too
/// short to be worth hashing.
fn derive_auth(password: &str, salt: &str) -> Result<String, Refusal> {
    let salt = unhex_array::<SALT_LEN>(salt).ok_or_else(|| {
        Refusal::local("the account server sent a salt this build could not read".to_owned())
    })?;

    let secrets =
        secret::derive(password, &salt).map_err(|error| Refusal::local(error.to_string()))?;

    Ok(hex(&secrets.auth))
}

/// Parses JSON without failing on something that is not JSON.
///
/// A proxy in the way answers with a page rather than a body this understands, and that is a
/// refusal to show rather than a reason to stop.
fn safe_parse(text: &str) -> Value {
    if text.is_empty() {
        return Value::Object(serde_json::Map::new());
    }

    serde_json::from_str(text).unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}

/// Percent-encodes a value for a URL, leaving only what never needs it.
///
/// Not decorative: an address with a `+` in it is a different address once a query string has
/// been read, because that is how a space is written there.
fn escape(value: &str) -> String {
    /// The digits a percent-escape is written with.
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";

    let mut escaped = String::with_capacity(value.len());

    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            escaped.push(char::from(byte));
        } else {
            escaped.push('%');
            escaped.push(char::from(DIGITS[usize::from(byte >> 4)]));
            escaped.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
    }

    escaped
}

/// Renders bytes as lowercase hex, which is the form this protocol writes everything in.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Reads hex into an array of a known size, or nothing if it is not that.
fn unhex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }

    let mut bytes = [0u8; N];

    for (slot, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }

    Some(bytes)
}

// ── What a window calls ─────────────────────────────────────────────────────────────────────

/// Returns what is known about the account.
///
/// The first call of a run is the one that offers the token an earlier run kept, and it waits for
/// the answer rather than saying "signed out" and correcting itself a moment later — which is
/// what a window draws as having been signed out.
///
/// # Errors
///
/// Fails if a thread panicked while holding the account.
#[tauri::command(async)]
pub fn account_state(
    held: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<AccountState, String> {
    held.with(|holder| holder.view(&settings))
}

/// Asks the server to send a signup code to an address.
///
/// # Errors
///
/// Fails if no server is configured, if the address is taken or malformed, or if the code could
/// not be sent. Returns `false` from a server with no mail configured, where registering asks for
/// nothing.
#[tauri::command(async)]
pub fn account_challenge(
    email: String,
    held: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<bool, String> {
    held.with(|holder| holder.challenge(&settings, &email))?
}

/// Creates an account and returns the second factor to set up, once.
///
/// # Errors
///
/// Fails if no server is configured, if the code is wrong or lapsed, if the address is taken, or
/// if the server cannot be reached.
#[tauri::command(async)]
pub fn account_register(
    email: String,
    password: String,
    code: String,
    held: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<Enrolment, String> {
    held.with(|holder| holder.register(&settings, &email, &password, &code))?
}

/// Signs in, registers this machine, and trusts every other machine on the account.
///
/// # Errors
///
/// Fails if no server is configured, or if any of the address, password and code is wrong, which
/// is reported as one failure.
#[tauri::command(async)]
pub fn account_sign_in(
    email: String,
    password: String,
    code: String,
    label: String,
    held: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<AccountState, String> {
    held.with(|holder| holder.sign_in(&settings, &email, &password, &code, &label))?
}

/// Ends the session, here and on the server.
///
/// Machines already trusted stay trusted: they were on the account, and signing out is not a
/// statement that they are not yours.
///
/// # Errors
///
/// Fails if a thread panicked while holding the account. A server that cannot be reached is not a
/// failure: the token is gone from this machine either way.
#[tauri::command(async)]
pub fn account_sign_out(
    held: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<AccountState, String> {
    held.with(|holder| holder.sign_out(&settings))
}

/// Renames this machine on the account.
///
/// The name every other machine on the account sees, which is not the same as the one this one is
/// called here — that is a nickname and stays local.
///
/// # Errors
///
/// Fails if nobody is signed in, or the server refuses.
#[tauri::command(async)]
pub fn account_rename(
    label: String,
    held: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<AccountState, String> {
    held.with(|holder| holder.rename(&settings, &label))?
}

/// Removes a machine from the account.
///
/// # Errors
///
/// Fails if nobody is signed in, or the server refuses.
#[tauri::command(async)]
pub fn account_forget_device(
    public_key: String,
    held: State<'_, Held>,
    settings: State<'_, Chosen>,
) -> Result<AccountState, String> {
    held.with(|holder| holder.forget(&settings, &public_key))?
}

/// Asks the account again who its machines are, and says whether they changed.
///
/// Not a command, and deliberately: no window asks for this. It is asked because a window came
/// forward, which is something the shell observes rather than something a page requests, and the
/// answer is what decides whether every open window has to be told.
///
/// # Errors
///
/// Fails if a thread panicked while holding the account.
pub fn refresh(held: &Held, settings: &Chosen) -> Result<bool, String> {
    held.with(|holder| holder.refresh(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine on the account, told apart from the others only by its key.
    fn device(public_key: &str, label: &str) -> Device {
        Device {
            public_key: public_key.to_owned(),
            label: label.to_owned(),
        }
    }

    #[test]
    fn an_address_survives_being_put_in_a_query() {
        // A `+` written literally reads as a space once a query string has been parsed, which
        // makes `a+work@example.com` a different address from the one somebody typed.
        assert_eq!(escape("a+work@example.com"), "a%2Bwork%40example.com");
        assert_eq!(escape("plain@example.com"), "plain%40example.com");
    }

    #[test]
    fn a_trailing_slash_is_not_a_different_server() {
        // Left in, this is a client rebuilt on every call — and a client rebuilt is a session
        // thrown away, on a machine where nothing changed.
        assert_eq!(
            Client::new("https://rv.example.com").base,
            "https://rv.example.com"
        );
        assert_eq!(
            Client::new("https://rv.example.com//").base,
            "https://rv.example.com"
        );
    }

    #[test]
    fn something_that_is_not_json_is_read_as_an_empty_answer() {
        assert!(
            safe_parse("<html>gateway timeout</html>")
                .get("error")
                .is_none()
        );
        assert!(safe_parse("").get("error").is_none());
        assert_eq!(
            safe_parse(r#"{"error":"That code is wrong or has expired."}"#)
                .get("error")
                .and_then(Value::as_str),
            Some("That code is wrong or has expired.")
        );
    }

    #[test]
    fn the_machine_in_front_of_you_is_marked_as_such() {
        let mine = "aa".repeat(KEY_LEN);
        let shown = views(
            &[device(&mine, "mac"), device(&"bb".repeat(KEY_LEN), "desk")],
            &mine,
        );

        assert!(shown[0].is_this_machine);
        assert!(!shown[1].is_this_machine);
        assert_eq!(shown[1].label, "desk");
    }

    #[test]
    fn hex_survives_a_round_trip() {
        let salt = [
            0x00u8, 0x0f, 0xa5, 0xff, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ];

        assert_eq!(unhex_array::<SALT_LEN>(&hex(&salt)), Some(salt));
        assert_eq!(unhex_array::<SALT_LEN>("nonsense"), None);
        assert_eq!(unhex_array::<SALT_LEN>(&hex(&salt)[..30]), None);
    }
}
