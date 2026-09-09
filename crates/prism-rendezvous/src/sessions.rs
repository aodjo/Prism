//! Sessions that outlive the process holding them.
//!
//! A session is what lets an application start signed in. Keeping them in memory made every
//! restart of this server — a deployment, a reboot, a crash — sign every person out of every
//! machine at once, which turns "stay signed in" into a promise that holds only until the next
//! time somebody updates the server.
//!
//! # What is on disk
//!
//! Not the tokens. A token is a bearer credential: whoever reads one is the account until it
//! expires, so a file of them would be as good as a file of passwords. What is stored is
//! SHA-256 of each token, which is enough to recognise one that comes back and no use at all
//! to somebody who reads the file. A slow hash would buy nothing here — a token is 32 bytes of
//! randomness, so there is no smaller space to search than the whole one.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One signed-in session, as it is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    /// The account it belongs to.
    name: String,
    /// When it stops being good, in seconds since the epoch.
    expires_unix: u64,
}

/// The file's shape.
#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    /// Keyed by the hash of the token, never by the token.
    sessions: HashMap<String, Stored>,
}

/// Every session this server will still recognise.
#[derive(Debug)]
pub struct Sessions {
    by_hash: HashMap<String, Stored>,
    path: PathBuf,
}

impl Sessions {
    /// Opens the store, dropping anything in it that has already expired.
    ///
    /// A file that cannot be read or parsed is treated as an empty one. That is the opposite
    /// of the choice the account store makes, and deliberately: losing a session costs somebody
    /// one sign-in, while losing an account costs them the account, so refusing to start over a
    /// damaged sessions file would trade a small loss for a total outage.
    ///
    /// # Errors
    ///
    /// Never. The signature returns a store either way and the type is kept simple for it.
    #[must_use]
    pub fn open(path: impl Into<PathBuf>, now_unix: u64) -> Self {
        let path = path.into();

        let file = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<File>(&bytes).ok())
            .unwrap_or_default();

        Self {
            by_hash: file
                .sessions
                .into_iter()
                .filter(|(_, session)| session.expires_unix > now_unix)
                .collect(),
            path,
        }
    }

    /// How many sessions are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Records a token as belonging to a name until a moment.
    ///
    /// # Errors
    ///
    /// Returns the error from writing the file. The session is remembered in memory either
    /// way, so a server whose disk is full still works until it is restarted.
    pub fn insert(&mut self, token: &str, name: &str, expires_unix: u64) -> io::Result<()> {
        self.by_hash.insert(
            fingerprint(token),
            Stored {
                name: name.to_owned(),
                expires_unix,
            },
        );

        self.save()
    }

    /// Returns the name a token belongs to, if it is still good.
    ///
    /// An expired session is dropped as it is found rather than swept on a timer, and the file
    /// is left alone until something else writes it — a removal that is not persisted costs
    /// nothing, because the next open filters by expiry anyway.
    #[must_use]
    pub fn whose(&mut self, token: &str, now_unix: u64) -> Option<String> {
        let hash = fingerprint(token);

        match self.by_hash.get(&hash) {
            Some(session) if session.expires_unix > now_unix => Some(session.name.clone()),
            Some(_) => {
                self.by_hash.remove(&hash);
                None
            }
            None => None,
        }
    }

    /// Forgets one session.
    ///
    /// # Errors
    ///
    /// Returns the error from writing the file.
    pub fn remove(&mut self, token: &str) -> io::Result<()> {
        if self.by_hash.remove(&fingerprint(token)).is_none() {
            return Ok(());
        }

        self.save()
    }

    /// Forgets every session belonging to one account.
    ///
    /// What deleting an account has to do as well as deleting the account: a token outlives the
    /// record it was issued against, and one still being honoured for a name that no longer
    /// exists is a signed-in machine nobody can sign out.
    ///
    /// # Errors
    ///
    /// Returns the error from writing the file.
    pub fn remove_for(&mut self, name: &str) -> io::Result<usize> {
        let before = self.by_hash.len();
        self.by_hash.retain(|_, session| session.name != name);

        let dropped = before - self.by_hash.len();

        if dropped > 0 {
            self.save()?;
        }

        Ok(dropped)
    }

    /// Writes the store, moving it into place so a reader never sees a half-written file.
    fn save(&self) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(&File {
            sessions: self.by_hash.clone(),
        })?;

        prism_core::store::replace(&self.path, &json)
    }
}

/// Turns a token into what is stored for it.
fn fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());

    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("prism-sessions-{label}.json"))
    }

    #[test]
    fn a_session_is_recognised_after_the_store_is_reopened() {
        let path = path("reopen");
        let _ = std::fs::remove_file(&path);

        let mut sessions = Sessions::open(&path, 100);
        sessions
            .insert("abc123", "someone", 1_000)
            .expect("written");

        let mut reopened = Sessions::open(&path, 100);

        assert_eq!(reopened.whose("abc123", 100).as_deref(), Some("someone"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_token_itself_is_not_in_the_file() {
        // The property the whole module exists for. A file of tokens would be a file of
        // passwords, and this is the test that notices if one ever starts being written.
        let path = path("nottoken");
        let _ = std::fs::remove_file(&path);

        let mut sessions = Sessions::open(&path, 100);
        sessions
            .insert("a-very-recognisable-token", "someone", 1_000)
            .expect("written");

        let text = std::fs::read_to_string(&path).expect("readable");

        assert!(!text.contains("a-very-recognisable-token"));
        assert!(text.contains(&fingerprint("a-very-recognisable-token")));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_expired_session_is_not_loaded_and_not_accepted() {
        let path = path("expired");
        let _ = std::fs::remove_file(&path);

        let mut sessions = Sessions::open(&path, 100);
        sessions.insert("stale", "someone", 200).expect("written");

        assert!(sessions.whose("stale", 300).is_none());
        assert!(Sessions::open(&path, 300).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_damaged_file_is_an_empty_store_rather_than_a_refusal_to_start() {
        let path = path("damaged");
        std::fs::write(&path, b"{not json").expect("written");

        assert!(Sessions::open(&path, 100).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn signing_out_stops_the_token_working() {
        let path = path("signout");
        let _ = std::fs::remove_file(&path);

        let mut sessions = Sessions::open(&path, 100);
        sessions.insert("tok", "someone", 1_000).expect("written");
        sessions.remove("tok").expect("written");

        assert!(sessions.whose("tok", 100).is_none());
        assert!(Sessions::open(&path, 100).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
