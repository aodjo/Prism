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
    subject: String,
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

/// Sends signup codes for one deployment.
#[derive(Debug, Clone)]
pub struct Mailer {
    /// The provider's key. Never logged and never stored.
    key: String,
    /// The address the message comes from, which the provider must already hold the domain for.
    from: String,
    client: reqwest::Client,
}

impl Mailer {
    /// Builds a mailer.
    ///
    /// Nothing here needs to know where this server can be reached. What is sent is a code
    /// somebody types back into the application they are already looking at, so there is no
    /// link, no public name to configure, and nothing to get wrong.
    ///
    /// # Errors
    ///
    /// Returns the underlying error if an HTTP client cannot be built, which means the
    /// platform has no usable TLS.
    pub fn new(key: String, from: String) -> Result<Self, MailError> {
        let client = reqwest::Client::builder()
            .timeout(PATIENCE)
            .build()
            .map_err(|err| MailError::Unreachable(err.to_string()))?;

        Ok(Self { key, from, client })
    }

    /// Sends somebody the code that proves the address is theirs.
    ///
    /// A code rather than a link. A link opens a browser, which is not where the person is —
    /// they are in front of the application, halfway through signing up — and it would leave
    /// the application with no way to know it had been opened. A code walks back to where the
    /// flow is. It is also the safer of the two: a link is something a stranger can get an
    /// address's owner to click, and a code is something they have to be told.
    ///
    /// # Errors
    ///
    /// Returns [`MailError::Unreachable`] if the provider cannot be reached and
    /// [`MailError::Refused`] if it answers with anything but success. Both are worth
    /// reporting rather than swallowing: a code that was never sent is a signup that cannot
    /// finish, and saying so lets the person try again.
    pub async fn send_code(&self, to: &str, code: &str) -> Result<(), MailError> {
        let response = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.key)
            .json(&Outgoing {
                from: &self.from,
                to: [to],
                subject: format!("{code} is your Prism code"),
                text: text_of(code),
                html: html_of(code),
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
fn text_of(code: &str) -> String {
    format!(
        "{code}\n\n\
         Type this into Prism to finish making your account.\n\n\
         It works once and stops working in fifteen minutes. If you did not ask for it, \
         nothing has been created and there is nothing to do — no account exists for this \
         address until somebody types this code, so ignoring it leaves the address free for \
         you.\n"
    )
}

/// The same message, for a reader that shows markup.
///
/// Deliberately plain. A verification message that looks like marketing is one people have
/// been taught to distrust, and everything here has to survive being read by somebody deciding
/// whether it is a phishing attempt.
fn html_of(code: &str) -> String {
    format!(
        "<div style=\"font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,\
         sans-serif;color:#1a1a1f;max-width:34em\">\
         <p style=\"font:600 34px/1 ui-monospace,SFMono-Regular,Menlo,monospace;\
         letter-spacing:.18em;margin:0 0 .5em\">{code}</p>\
         <p style=\"margin:0\">Type this into Prism to finish making your account.</p>\
         <p style=\"color:#6a6a78;font-size:13px\">It works once and stops working in fifteen \
         minutes. If you did not ask for it, nothing has been created and there is nothing to \
         do — no account exists for this address until somebody types this code, so ignoring \
         it leaves the address free for you.</p></div>"
    )
}
