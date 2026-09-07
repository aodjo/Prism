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
use crate::net::negotiate::{Accept, HostAbility, OFFER_LEN, Offer, decide};
use crate::net::packet::MAX_PACKET_SIZE;
use crate::net::transport::UdpTransport;

/// How long to wait for an answer before sending the first message again.
///
/// Short enough that a single loss costs a barely noticeable pause at startup, long enough
/// that a slow path is not mistaken for a lost packet.
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// How long to try a direct path before deciding it will not open.
///
/// Punching either works within a couple of round trips or does not work at all: both routers
/// have already been told to send, and one that will pass a packet has already passed one.
/// Waiting longer only delays the fallback a person is waiting through.
pub const DIRECT_PATIENCE: Duration = Duration::from_secs(4);

/// How long to keep trying once a path is known to work.
///
/// Longer, because by this point the only thing that can go wrong is loss, and a relay that
/// has opened is a path that will carry the next attempt.
pub const RELAYED_PATIENCE: Duration = Duration::from_secs(10);

/// Largest reply the answering side will write.
const REPLY_CAPACITY: usize = RESPONSE_OVERHEAD + MAX_HANDSHAKE_PAYLOAD;

/// Reads what the host decided out of a finished handshake.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidData`] if the host answered with something this build
/// cannot read, which means it is running a version this one cannot talk to.
pub fn agreed(established: &Established) -> io::Result<Accept> {
    Accept::decode(&established.peer_payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Runs the dialling side of a handshake against `peer`.
///
/// The socket is left unconnected. That matters: if the handshake times out the caller may
/// want to try again through a relay, and a connected socket refuses to send anywhere else —
/// so the socket is connected only once it is settled where the session will run.
///
/// The read timeout is left set to [`RETRY_INTERVAL`]; the caller sets whatever it wants for
/// the session that follows.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if the peer never answers, [`io::ErrorKind::
/// PermissionDenied`] if it answers with a key other than the expected one, and the
/// underlying [`io::Error`] for a socket failure.
pub fn dial(
    transport: &UdpTransport,
    peer: SocketAddr,
    identity: &Identity,
    peer_key: &[u8; KEY_LEN],
    offer: Offer,
    patience: Duration,
) -> io::Result<Established> {
    // What this machine can decode rides in the message that opens the session, so by the time
    // the session is live it is already configured. Negotiating separately would put a second
    // round trip in front of every connection to move twelve bytes.
    let mut payload = [0u8; OFFER_LEN];
    offer
        .encode_into(&mut payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

    let mut initiator = Initiator::new(identity, peer_key, &payload)
        .map_err(|err| io::Error::other(err.to_string()))?;

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;

    let give_up = Instant::now() + patience;
    let mut buf = [0u8; MAX_PACKET_SIZE];

    while Instant::now() < give_up {
        transport.send_to(initiator.first_message(), peer)?;

        let retry_at = Instant::now() + RETRY_INTERVAL;
        while Instant::now() < retry_at {
            let (len, from) = match transport.recv_from_into(&mut buf) {
                Ok((bytes, from)) => (bytes.len(), from),
                Err(err) if is_timeout(&err) => break,
                Err(err) => return Err(err),
            };

            // Only the address being dialled. Anything else is a stray packet or a scan, and
            // the handshake would refuse it anyway — but refusing it here keeps the count of
            // what the handshake rejected honest.
            if from != peer {
                continue;
            }

            if !initiator.accept(&buf[..len]) {
                continue;
            }

            let established = initiator
                .take()
                .ok_or_else(|| io::Error::other("the handshake completed without keys"))?;

            // The host's decision came back in the message that completed the handshake, so
            // the session is configured before it carries anything. A host that answered with
            // something this build cannot read is a host running a version this one cannot
            // talk to, which is a refusal rather than a stream to attempt.
            Accept::decode(&established.peer_payload)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

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
    /// What this machine is able and willing to send.
    ability: HostAbility,
    /// What was agreed, once a client has said what it can do.
    agreed: Option<Accept>,
}

impl Listener {
    /// Creates a listener that will answer only the peers `policy` names.
    #[must_use]
    pub fn new(identity: Identity, policy: PeerPolicy, ability: HostAbility) -> Self {
        Self {
            responder: Responder::new(identity, policy),
            reply: Box::new([0; REPLY_CAPACITY]),
            ability,
            agreed: None,
        }
    }

    /// Returns what was agreed, once a session has opened.
    #[must_use]
    pub fn agreed(&self) -> Option<Accept> {
        self.agreed
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
        // What the session will be is decided from what the client said it can do, and the
        // decision travels in the same message that completes the handshake.
        let ability = self.ability;
        let mut decided = None;

        let mut answer = |offered: &[u8], out: &mut [u8]| {
            // A client this build cannot agree with gets silence, as any other message that
            // cannot be acted on does: an explanation would tell a scan it found a host.
            let agreed = Offer::decode(offered)
                .and_then(|offer| decide(ability, offer))
                .ok()?;

            let written = agreed.encode_into(out).ok()?;
            decided = Some(agreed);

            Some(written)
        };

        let outcome = self
            .responder
            .accept(datagram, &mut answer, self.reply.as_mut_slice());

        let Answer::Reply(len) = outcome else {
            return Ok(None);
        };

        transport.send_to(&self.reply[..len], from)?;

        let established = self.responder.take();
        if established.is_some() {
            // Recorded only once a session actually opened. A repeat of an already answered
            // message replays the cached bytes without running the decision again, so this
            // holds what those bytes said.
            self.agreed = decided.or(self.agreed);
        }

        Ok(established)
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
    ability: HostAbility,
    patience: Duration,
) -> io::Result<(Established, SocketAddr, Listener)> {
    let mut listener = Listener::new(identity, policy, ability);
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
