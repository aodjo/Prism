//! The one message this server sends.
//!
//! An address somebody typed into a form is a claim, not a fact. The only thing that turns it
//! into a fact is something arriving at it that only its owner could have received — so this
//! module exists to send exactly that, and nothing else. There is no newsletter, no
//! notification, and no second kind of message to grow into one.
//!
//! # Why an API rather than SMTP
//!
//! A self-hosted server that had to speak SMTP would need a relay, credentials for it, and a
//! reputation good enough that what it sends is not filed as junk. That is a large thing to
//! ask of somebody who wanted a rendezvous server. An HTTP call to a provider that already
//! holds the domain's sending reputation is one credential and no infrastructure.

use std::time::Duration;

use serde::Serialize;

/// Where the messages are handed over.
const ENDPOINT: &str = "https://api.resend.com/emails";

/// How long to wait on the provider before giving up.
///
/// Short, because somebody is sitting in front of a form waiting for this to come back. A
/// registration that hangs for thirty seconds reads as broken whether or not the mail arrives.
const PATIENCE: Duration = Duration::from_secs(10);

/// What the provider is asked to send.
#[derive(Debug, Serialize)]
struct Outgoing<'a> {
    from: &'a str,
    to: [&'a str; 1],
    subject: &'a str,
    text: String,
    html: String,
}

/// Why a message could not be sent.
#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// The provider could not be reached, or did not answer in time.
    #[error("the mail provider could not be reached: {0}")]
    Unreachable(String),
    /// The provider answered, and the answer was a refusal.
    #[error("the mail provider refused to send: {status} {body}")]
    Refused {
        /// The status it answered with.
        status: u16,
        /// What it said, which usually names the problem.
        body: String,
    },
}

/// Sends verification links for one deployment.
#[derive(Debug, Clone)]
pub struct Mailer {
    /// The provider's key. Never logged and never stored.
    key: String,
    /// The address the message comes from, which the provider must already hold the domain for.
    from: String,
    /// Where this server can be reached from wherever somebody reads their mail.
    base: String,
    client: reqwest::Client,
}

impl Mailer {
    /// Builds a mailer.
    ///
    /// `base` is the address the link points at. It is not the address the API is bound to:
    /// this server is meant to sit behind something that terminates TLS, and the link has to
    /// work from a phone on a different network, so only the operator knows what it is.
    ///
    /// # Errors
    ///
    /// Returns the underlying error if an HTTP client cannot be built, which means the
    /// platform has no usable TLS.
    pub fn new(key: String, from: String, base: String) -> Result<Self, MailError> {
        let client = reqwest::Client::builder()
            .timeout(PATIENCE)
            .build()
            .map_err(|err| MailError::Unreachable(err.to_string()))?;

        Ok(Self {
            key,
            from,
            base: base.trim_end_matches('/').to_string(),
            client,
        })
    }

    /// Returns the link a token becomes.
    #[must_use]
    pub fn link(&self, token: &str) -> String {
        format!("{}/v1/accounts/verify?token={token}", self.base)
    }

    /// Sends somebody the link that proves the address is theirs.
    ///
    /// # Errors
    ///
    /// Returns [`MailError::Unreachable`] if the provider cannot be reached and
    /// [`MailError::Refused`] if it answers with anything but success. Both are worth
    /// reporting rather than swallowing: an account whose link was never sent is one nobody
    /// can ever use, and saying so lets the person try again.
    pub async fn send_verification(&self, to: &str, token: &str) -> Result<(), MailError> {
        let link = self.link(token);

        let response = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.key)
            .json(&Outgoing {
                from: &self.from,
                to: [to],
                subject: "Confirm your Prism account",
                text: text_of(&link),
                html: html_of(&link),
            })
            .send()
            .await
            .map_err(|err| MailError::Unreachable(err.to_string()))?;

        let status = response.status();

        if status.is_success() {
            return Ok(());
        }

        Err(MailError::Refused {
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
        })
    }
}

/// The message, for somebody whose mail reader shows text.
fn text_of(link: &str) -> String {
    format!(
        "Somebody registered a Prism account with this address.\n\n\
         Open this link to confirm it is yours:\n\n{link}\n\n\
         The link works once and stops working after a day. Until it is opened the account \
         cannot sign in, so if this was not you there is nothing to do — ignore this and the \
         address stays free for you to use.\n"
    )
}

/// The same message, for a reader that shows markup.
///
/// Deliberately plain. A verification message that looks like marketing is one people have
/// been taught to distrust, and everything here has to survive being read by somebody deciding
/// whether it is a phishing attempt.
fn html_of(link: &str) -> String {
    format!(
        "<div style=\"font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,\
         sans-serif;color:#1a1a1f;max-width:34em\">\
         <p>Somebody registered a Prism account with this address.</p>\
         <p><a href=\"{link}\" style=\"color:#5b3ce0\">Confirm it is yours</a></p>\
         <p style=\"color:#6a6a78;font-size:13px\">Or paste this into a browser:<br>{link}</p>\
         <p style=\"color:#6a6a78;font-size:13px\">The link works once and stops working after \
         a day. Until it is opened the account cannot sign in, so if this was not you there is \
         nothing to do — ignore this and the address stays free for you to use.</p></div>"
    )
}
