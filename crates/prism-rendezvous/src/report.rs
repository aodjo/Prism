//! What a region tells the account server about itself.
//!
//! A region pushes; nothing asks it. That decision is not about taste — it is what keeps
//! adding a region to "a machine, a port, and an address record". Polling would need every
//! region to answer HTTPS from the internet, which means a hostname each, a certificate each,
//! a proxy each, and two more ports open on a box whose whole attack surface today is one UDP
//! port and SSH. Pushing needs none of that: the connection is outbound, and a region that has
//! nothing to say is a region nobody has to reach.
//!
//! It also means the dashboard reads numbers out of its own database instead of waiting on five
//! machines scattered around the world before it can draw anything.
//!
//! # What the numbers mean
//!
//! Everything counted here is **since this server started**. A region holds no state and is
//! meant to be disposable — `docs/rendezvous.md` says destroying one costs the sessions in
//! flight on it and nothing else — so there is nowhere for a monthly total to live, and
//! pretending otherwise would put a number in front of an operator that resets without warning
//! on every deploy. `uptime_seconds` travels beside the totals so that whoever reads them knows
//! what window they cover.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use serde::Serialize;

/// How long to wait for the account server before giving up on one report.
///
/// A report is worthless late: the next one is already being counted. Short enough that a
/// server which has stopped answering costs one thread a few seconds rather than a minute.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How many reports may wait to be sent.
///
/// One. If the sender is still working through the last one, the newer report replaces nothing
/// and is dropped — a report that has been queued behind another is already out of date, and a
/// queue here would turn a slow account server into unbounded memory on a region.
const QUEUE_DEPTH: usize = 1;

/// One relayed session, as an operator sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Carried {
    /// The host's public key, in hex.
    pub host: String,
    /// The client's public key, in hex.
    pub client: String,
    /// The relay's token, in hex, which is what names it in a log.
    pub token: String,
    /// When both sides arrived, in seconds since the epoch.
    pub since_unix: u64,
    /// Bytes carried for it so far, both directions together.
    pub bytes: u64,
}

/// One pair this server introduced.
#[derive(Debug, Clone, Serialize)]
pub struct Introduced {
    /// The host's public key, in hex.
    pub host: String,
    /// The client's public key, in hex.
    pub client: String,
    /// When the introduction was made, in seconds since the epoch.
    pub at_unix: u64,
    /// Whether the pair came back asking to be relayed.
    ///
    /// False does not mean they connected: it means they did not ask this server for a relay.
    /// A pair that gave up looks exactly the same from here, which is why nothing downstream
    /// may read this as a success rate.
    pub relayed: bool,
}

/// Everything a region has to say about itself.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// What an operator calls this region, which has to match the name in the database.
    pub region: String,
    /// The version of the server that is running.
    pub build: String,
    /// How long since it started, in seconds.
    pub uptime_seconds: u64,
    /// Hosts registered and reachable here.
    pub hosts: u64,
    /// Challenges issued and not yet answered.
    pub outstanding: u64,
    /// Relayed sessions being carried.
    pub carrying: u64,
    /// Relays with one side present and waiting for the other.
    pub waiting: u64,
    /// What those sessions are costing the link right now, in megabits per second.
    pub now_mbps: f64,
    /// The most they have cost it since this server started.
    pub peak_mbps: f64,
    /// What the link will carry, in megabits per second, as the operator configured it.
    ///
    /// Configuration rather than measurement: a server cannot discover what its plan allows.
    pub link_mbps: f64,
    /// Bytes relayed since this server started.
    pub carried_bytes: u64,
    /// Every relay being carried right now.
    pub sessions: Vec<Carried>,
    /// The pairs most recently introduced, newest last.
    pub introduced: Vec<Introduced>,
}

/// Hands reports to a thread that sends them.
///
/// Cloneable and cheap to hold. Sending never blocks: a report that cannot be handed over at
/// once is dropped, because the loop that produces it is the loop that must never wait.
#[derive(Debug, Clone)]
pub struct Reporter(SyncSender<Report>);

impl Reporter {
    /// Offers one report, dropping it if the sender is still busy with the last.
    ///
    /// Returns whether it was taken, which is worth knowing only for a test: a dropped report
    /// is a normal thing that happens when the account server is slow, not a failure.
    pub fn offer(&self, report: Report) -> bool {
        !matches!(self.0.try_send(report), Err(TrySendError::Full(_)))
    }
}

/// Starts the thread that sends reports, and returns the handle to give it work.
///
/// `to` is an origin such as `https://accounts.presm.kr`; the path is appended here so that
/// the two ends cannot disagree about it. `token` authorises the report — without it anybody
/// who can reach the account server could claim to be a region and write whatever numbers they
/// liked into an operator's dashboard.
///
/// The thread owns a single-threaded runtime of its own. The server has no runtime otherwise
/// unless it is keeping accounts, and the receive loop must not acquire one.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the thread cannot be started.
pub fn spawn(to: String, token: String) -> std::io::Result<Reporter> {
    let (sender, receiver) = sync_channel(QUEUE_DEPTH);

    std::thread::Builder::new()
        .name("prism-report".to_owned())
        .spawn(move || send_until_closed(&to, &token, &receiver))?;

    Ok(Reporter(sender))
}

/// Sends every report handed over, until the sender is dropped.
fn send_until_closed(to: &str, token: &str, reports: &Receiver<Report>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("prism-rendezvous: no runtime for reporting; this region will not be listed");
        return;
    };

    // Built inside the runtime's context. A `reqwest::Client` reaches for the reactor as it is
    // constructed, and one built outside it panics with "there is no reactor running" on the
    // first report rather than at startup — which is a thread that dies silently an hour after
    // anybody was watching.
    let _guard = runtime.enter();

    // A fresh connection for every report, deliberately. Reports are a minute apart, and a
    // pooled connection that has been idle that long has usually been closed at the other end —
    // which reqwest discovers by writing to it, and cannot retry, because a POST is not safe to
    // repeat. Measured here: the first report went through and the second failed with "error
    // sending request". A handshake once a minute costs nothing next to that.
    let client = match reqwest::Client::builder()
        .timeout(TIMEOUT)
        .pool_max_idle_per_host(0)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            eprintln!("prism-rendezvous: no http client for reporting ({err})");
            return;
        }
    };

    let endpoint = format!("{}/v1/regions/report", to.trim_end_matches('/'));

    while let Ok(report) = reports.recv() {
        // The request is built inside the runtime as well, for the same reason.
        let sending = runtime.block_on(async {
            client
                .post(&endpoint)
                .bearer_auth(token)
                .json(&report)
                .send()
                .await
        });

        // Failures are said once and otherwise shrugged off. A region whose account server is
        // unreachable is still a region carrying sessions perfectly well, and a signalling
        // server that stopped working because a dashboard could not be updated would be the
        // dashboard deciding whether anybody gets to connect.
        match sending {
            Ok(answer) if answer.status().is_success() => {}
            Ok(answer) => {
                eprintln!(
                    "prism-rendezvous: the account server refused a report: {}",
                    answer.status()
                );
            }
            Err(err) => eprintln!("prism-rendezvous: could not send a report ({err})"),
        }
    }
}

/// The current time in whole seconds since the epoch.
///
/// Zero if the clock is set before 1970, which is a machine with no clock rather than a case
/// worth an error path: what it costs is one wrong timestamp on a report.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The pairs this server has introduced lately.
///
/// A ring rather than a log, because a region keeps nothing: this is what the last few minutes
/// looked like, held so a report can carry it, and it is meant to be forgotten. The account
/// server is where an operator goes for history.
#[derive(Debug, Default)]
pub struct Introductions {
    recent: std::collections::VecDeque<Introduced>,
}

/// How many introductions to remember.
///
/// Comfortably more than a report interval's worth on a busy region, and small enough that the
/// memory is not worth thinking about. What overflows is dropped oldest-first; the account
/// server already has it.
const REMEMBERED: usize = 200;

impl Introductions {
    /// Creates an empty ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a client was told where a host is.
    pub fn record(&mut self, host: &[u8], client: &[u8]) {
        self.recent.push_back(Introduced {
            host: hex(host),
            client: hex(client),
            at_unix: unix_now(),
            relayed: false,
        });

        while self.recent.len() > REMEMBERED {
            self.recent.pop_front();
        }
    }

    /// Marks the most recent introduction of a pair as having gone to the relay.
    ///
    /// The newest, because a pair that reconnects is introduced again and it is the current
    /// attempt that ended up relayed. A pair asked about but never introduced — which a
    /// stranger can produce by sending a relay request out of nowhere — marks nothing.
    pub fn relayed(&mut self, host: &[u8], client: &[u8]) {
        let (host, client) = (hex(host), hex(client));

        if let Some(found) = self
            .recent
            .iter_mut()
            .rev()
            .find(|entry| entry.host == host && entry.client == client)
        {
            found.relayed = true;
        }
    }

    /// Copies out what is remembered, oldest first.
    #[must_use]
    pub fn recent(&self) -> Vec<Introduced> {
        self.recent.iter().cloned().collect()
    }
}

/// Writes bytes as lowercase hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_become_lowercase_hex() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa7, 0xff]), "000fa7ff");
    }

    #[test]
    fn a_report_that_cannot_be_handed_over_is_dropped_rather_than_queued() {
        // The property the receive loop depends on: offering a report never blocks it, however
        // slow or dead the account server is.
        let (sender, receiver) = sync_channel(QUEUE_DEPTH);
        let reporter = Reporter(sender);

        let sample = || Report {
            region: "Test".to_owned(),
            build: "0".to_owned(),
            uptime_seconds: 0,
            hosts: 0,
            outstanding: 0,
            carrying: 0,
            waiting: 0,
            now_mbps: 0.0,
            peak_mbps: 0.0,
            link_mbps: 0.0,
            carried_bytes: 0,
            sessions: Vec::new(),
            introduced: Vec::new(),
        };

        assert!(reporter.offer(sample()), "the first report was refused");
        assert!(
            !reporter.offer(sample()),
            "a second report was queued behind the first"
        );

        drop(receiver);
    }
}
