//! What the shell and the stream process say to each other.
//!
//! One [`Start`] goes in, a run of [`Event`]s comes back, and both travel as a four-byte
//! big-endian length followed by that many bytes of JSON. The length is what makes a partial
//! write recoverable: a reader that has the length knows whether it has the whole message, and
//! a message it cannot parse is one message lost rather than a stream out of step forever.
//!
//! JSON rather than the packed encoding `packages/protocol` defines, because nothing here is on
//! the frame path — one message to start and a handful a second afterwards — and a field added
//! to one side is a field the other simply ignores.
//!
//! Standard output carries these and nothing else. Anything a process writes for a person, its
//! panics included, goes to standard error, so a stray print cannot be mistaken for a message.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Longest message either side will read.
///
/// Far past anything these carry. A length prefix read off a pipe is a number a broken writer
/// controls, and without a ceiling it is an allocation that side chooses.
pub const MAX_MESSAGE_LEN: usize = 64 * 1024;

/// What the shell asks the stream process to do.
///
/// Sent once, before anything else. Everything the run needs is here, because a process that
/// had to ask questions back would need a protocol going the other way to answer them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Start {
    /// The host to watch, as pairing recorded it: a stored name or a hex public key.
    pub host: String,
    /// Where the host can be reached directly, when that is known.
    ///
    /// `None` means find it through the rendezvous server, which is what a host behind a
    /// router requires.
    #[serde(default)]
    pub address: Option<String>,
    /// The rendezvous server to find the host through, as a name and port.
    #[serde(default)]
    pub rendezvous: Option<String>,
    /// Whether this end may type and click on the other machine.
    pub control: bool,
    /// Prefer smoothness over immediacy when pacing what is drawn.
    pub smooth: bool,
    /// Give up after this long with no packets.
    pub idle_timeout_ms: u64,
    /// How large to open the window.
    pub width: u32,
    /// How large to open the window.
    pub height: u32,
}

/// What the stream process says while it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// A line worth keeping, which is what explains a failure afterwards.
    ///
    /// Prose, and reworded whenever it reads better. Nothing may be decided from it.
    Note {
        /// The line.
        line: String,
    },
    /// The handshake completed and a person is now looking at another machine.
    Established {
        /// Where the session runs, which is a relay's address when one is carrying it.
        address: String,
    },
    /// What the two sides settled on.
    Terms {
        /// The codec in use.
        codec: String,
        /// Width in pixels, or zero when the client took whatever the host's screen is.
        width: u32,
        /// Height in pixels, or zero when the client took whatever the host's screen is.
        height: u32,
        /// Frames per second.
        fps: u32,
        /// Whether sound is part of the session.
        audio: bool,
    },
    /// The counters, once a second.
    Counters {
        /// Round trip to the host in microseconds.
        round_trip_us: u64,
        /// Frames completed per second.
        fps: f64,
        /// Everything arriving, in kilobits per second.
        kbps: f64,
        /// Frames completed since the session opened.
        frames: u32,
    },
    /// The run is over.
    ///
    /// Sent before the process exits, so the shell knows why rather than only that it did.
    Ended {
        /// What went wrong, or `None` if the stream simply ended.
        error: Option<String>,
    },
}

/// Writes one message and flushes it.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the message cannot be serialised or written.
pub fn write<T: Serialize>(to: &mut impl Write, message: &T) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;

    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the message is too long"))?;

    to.write_all(&len.to_be_bytes())?;
    to.write_all(&body)?;
    to.flush()
}

/// Reads one message, or `None` once the other side has closed the pipe.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] if the length is past [`MAX_MESSAGE_LEN`] or the body
/// is not what was expected, and the underlying [`io::Error`] if the pipe fails. A message that
/// ends mid-body is [`io::ErrorKind::UnexpectedEof`] rather than a clean end: the writer died
/// partway, which is not the same as having nothing more to say.
pub fn read<T: for<'de> Deserialize<'de>>(from: &mut impl Read) -> io::Result<Option<T>> {
    let mut header = [0u8; 4];

    match from.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }

    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_MESSAGE_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {len} byte message is past the ceiling"),
        ));
    }

    let mut body = vec![0u8; len];
    from.read_exact(&mut body)?;

    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_survives_the_round_trip() {
        let mut pipe = Vec::new();
        write(
            &mut pipe,
            &Event::Established {
                address: "203.0.113.9:47000".to_owned(),
            },
        )
        .expect("writes");

        let back: Option<Event> = read(&mut pipe.as_slice()).expect("reads");

        assert!(
            matches!(back, Some(Event::Established { address }) if address == "203.0.113.9:47000")
        );
    }

    #[test]
    fn messages_keep_their_boundaries() {
        // The whole point of the length. Two messages in one pipe have to come back as two,
        // whatever the reader's buffer happened to hold.
        let mut pipe = Vec::new();
        write(
            &mut pipe,
            &Event::Note {
                line: "one".to_owned(),
            },
        )
        .expect("writes");
        write(
            &mut pipe,
            &Event::Note {
                line: "two".to_owned(),
            },
        )
        .expect("writes");

        let mut source = pipe.as_slice();
        let first: Option<Event> = read(&mut source).expect("reads");
        let second: Option<Event> = read(&mut source).expect("reads");
        let third: Option<Event> = read(&mut source).expect("reads");

        assert!(matches!(first, Some(Event::Note { line }) if line == "one"));
        assert!(matches!(second, Some(Event::Note { line }) if line == "two"));
        assert!(third.is_none(), "the pipe kept answering after it ran out");
    }

    #[test]
    fn an_empty_pipe_is_the_end_rather_than_an_error() {
        let empty: [u8; 0] = [];
        let nothing: Option<Event> = read(&mut empty.as_slice()).expect("reads");

        assert!(nothing.is_none());
    }

    #[test]
    fn a_length_past_the_ceiling_is_refused_before_it_is_allocated() {
        // The length comes off a pipe, so it is a number the other side chooses. Without this
        // it is an allocation the other side chooses.
        let mut pipe = u32::MAX.to_be_bytes().to_vec();
        pipe.push(0);

        let refused: io::Result<Option<Event>> = read(&mut pipe.as_slice());

        assert_eq!(
            refused
                .expect_err("a four gigabyte message was accepted")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_message_that_stops_partway_is_not_a_clean_end() {
        let mut pipe = 32u32.to_be_bytes().to_vec();
        pipe.extend_from_slice(b"{\"kind\":\"note\"");

        let cut: io::Result<Option<Event>> = read(&mut pipe.as_slice());

        assert_eq!(
            cut.expect_err("a half-written message read as the end")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
