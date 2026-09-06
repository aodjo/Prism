//! Getting from a bound socket to a live sealed session.
//!
//! One side dials and one side answers, and which is which follows the direction video
//! travels: the host connects to the client, so the host speaks first. Both sides still check
//! the key they end up facing, so being the one who dialled buys no authority.
//!
//! # Who dials
//!
//! The client. The host listens, because over the internet it has no way to learn a client's
//! address until that client speaks — and the two roles line up with the handshake's own: the
//! side that dials is the side that already knows the other's static key, which is exactly
//! what pairing gave the client.
//!
//! # Losing a handshake message
//!
//! Either message can be lost, and each loss has a different remedy. A lost first message is
//! the dialler's problem and it resends. A lost answer looks to the dialler exactly the same,
//! so it resends again — and the answering side has to reply with the bytes it sent before
//! rather than starting over, which [`Responder`] handles. What the answering side cannot know
//! is when to stop being ready to do that, so it stays ready until a sealed packet arrives:
//! at that point the dialler plainly received the answer.

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::net::handshake::{
    Answer, Established, Identity, Initiator, KEY_LEN, MAX_HANDSHAKE_PAYLOAD, PeerPolicy,
    RESPONSE_OVERHEAD, Responder,
};
use crate::net::packet::MAX_PACKET_SIZE;
use crate::net::transport::UdpTransport;

/// How long to wait for an answer before sending the first message again.
///
/// Short enough that a single loss costs a barely noticeable pause at startup, long enough
/// that a slow path is not mistaken for a lost packet.
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// How long to keep trying before giving up on the peer entirely.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest reply the answering side will write.
const REPLY_CAPACITY: usize = RESPONSE_OVERHEAD + MAX_HANDSHAKE_PAYLOAD;

/// Runs the dialling side of a handshake on a connected socket.
///
/// The socket's read timeout is left set to [`RETRY_INTERVAL`]; the caller sets whatever it
/// wants for the session that follows.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if the peer never answers, [`io::ErrorKind::
/// PermissionDenied`] if it answers with a key other than the expected one, and the
/// underlying [`io::Error`] for a socket failure.
pub fn dial(
    transport: &UdpTransport,
    identity: &Identity,
    peer_key: &[u8; KEY_LEN],
) -> io::Result<Established> {
    let mut initiator =
        Initiator::new(identity, peer_key, &[]).map_err(|err| io::Error::other(err.to_string()))?;

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;

    let give_up = Instant::now() + DIAL_TIMEOUT;
    let mut buf = [0u8; MAX_PACKET_SIZE];

    while Instant::now() < give_up {
        transport.send(initiator.first_message())?;

        let retry_at = Instant::now() + RETRY_INTERVAL;
        while Instant::now() < retry_at {
            let bytes = match transport.recv_into(&mut buf) {
                Ok(bytes) => bytes,
                Err(err) if is_timeout(&err) => break,
                Err(err) => return Err(err),
            };

            if !initiator.accept(bytes) {
                continue;
            }

            let established = initiator
                .take()
                .ok_or_else(|| io::Error::other("the handshake completed without keys"))?;

            // The answer proves the peer holds the key that was dialled, because only that
            // key could have read the first message. Checking it again here is cheap and
            // makes the guarantee local to the code that depends on it.
            if established.session.peer_static != *peer_key {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the peer answered with a different key than the one dialled",
                ));
            }

            return Ok(established);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the peer did not answer the handshake",
    ))
}

/// The answering side of a handshake, kept alive until the dialler is plainly satisfied.
///
/// After the answer is sent this stays in the receive loop, because a lost answer is
/// indistinguishable from a lost first message and the dialler will ask again. It retires the
/// moment a sealed packet arrives.
pub struct Listener {
    responder: Responder,
    reply: Box<[u8; REPLY_CAPACITY]>,
}

impl Listener {
    /// Creates a listener that will answer only the peers `policy` names.
    #[must_use]
    pub fn new(identity: Identity, policy: PeerPolicy) -> Self {
        Self {
            responder: Responder::new(identity, policy),
            reply: Box::new([0; REPLY_CAPACITY]),
        }
    }

    /// Offers a datagram that did not open as a sealed packet.
    ///
    /// Answers it if it is a handshake message, and returns the session the first time one
    /// completes. Anything else is ignored, and ignoring is the whole point: a port on the
    /// internet receives scans, and none of them may draw a reply.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the answer cannot be sent.
    pub fn offer(
        &mut self,
        transport: &UdpTransport,
        datagram: &[u8],
        from: SocketAddr,
    ) -> io::Result<Option<Established>> {
        let Answer::Reply(len) = self
            .responder
            .accept(datagram, &[], self.reply.as_mut_slice())
        else {
            return Ok(None);
        };

        transport.send_to(&self.reply[..len], from)?;

        Ok(self.responder.take())
    }
}

impl core::fmt::Debug for Listener {
    /// Describes the listener without exposing any key material.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Listener")
            .field("responder", &self.responder)
            .finish_non_exhaustive()
    }
}

/// Returns whether an error is the read timeout rather than a real failure.
fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Waits on a bound socket for a paired peer to complete a handshake.
///
/// Answers rather than dials, which is what a host has to do: over the internet it cannot
/// know the client's address until the client speaks, so a listening host is not a
/// convenience but the only arrangement that works at all.
///
/// The socket is connected to whoever completed the handshake before returning, so from that
/// point the kernel drops datagrams from anywhere else.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if nobody completes a handshake before `patience`
/// elapses, and the underlying [`io::Error`] for a socket failure.
pub fn serve(
    transport: &UdpTransport,
    identity: Identity,
    policy: PeerPolicy,
    patience: Duration,
) -> io::Result<(Established, SocketAddr, Listener)> {
    let mut listener = Listener::new(identity, policy);
    let mut buf = [0u8; MAX_PACKET_SIZE];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + patience;

    while Instant::now() < give_up {
        let (len, from) = match transport.recv_from_into(&mut buf) {
            Ok((bytes, from)) => (bytes.len(), from),
            Err(err) if is_timeout(&err) => continue,
            Err(err) => return Err(err),
        };

        if let Some(established) = listener.offer(transport, &buf[..len], from)? {
            transport.connect(from)?;
            return Ok((established, from, listener));
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "no paired client connected",
    ))
}
