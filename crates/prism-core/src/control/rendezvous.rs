//! Finding a peer through the rendezvous server, and punching a hole to it.
//!
//! # One socket, from beginning to end
//!
//! Everything here happens on the socket the session will use. That is not an optimisation,
//! it is the whole mechanism: a router's mapping belongs to a specific local port, so the
//! address the server observes is only useful if the peer sends to it *and this port is the
//! one listening*. Registering from a second socket would publish an address that leads
//! nowhere.
//!
//! Rendezvous traffic and session traffic therefore share a port. They are told apart by
//! source address — the server is at a known address and the peer is not — rather than by
//! looking at the bytes, because a sealed packet is indistinguishable from anything else by
//! construction.
//!
//! # The punch
//!
//! Two routers, each of which will pass an inbound packet only after something inside has
//! sent outward to that same address. So both sides send first and neither waits. The
//! client's Noise handshake message is its punch — it has to be sent repeatedly anyway, in
//! case it is lost — and the host answers an introduction with a few empty datagrams whose
//! only job is to make its own router willing to receive.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::net::handshake::{Identity, KEY_LEN};
use crate::net::rendezvous::{MAX_MESSAGE_LEN, Message, RELAY_TOKEN_LEN, answer};
use crate::net::transport::UdpTransport;

/// How long to wait for the server before asking again.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// How long to keep asking the server before giving up on it.
const SERVER_TIMEOUT: Duration = Duration::from_secs(15);

/// How often a registered host reminds the server it is there.
///
/// Well under the ninety seconds the server holds a registration, so several lost keepalives
/// in a row still leave the host reachable. It is also what holds the router's mapping open:
/// a NAT binding lapses after tens of seconds of silence, and a host that spoke only at
/// startup would quietly become unreachable with nothing appearing to be wrong.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How many datagrams a host sends at a caller to open its own router.
///
/// More than one because any of them may be lost, and losing all of them costs a whole
/// connection attempt. They carry nothing: their only effect is on the router in between.
const PUNCHES: u32 = 5;

/// How long between them.
const PUNCH_INTERVAL: Duration = Duration::from_millis(50);

/// Registers this machine under its public key and returns where the server sees it.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if the server does not answer, and
/// [`io::ErrorKind::PermissionDenied`] if it issues a challenge this machine cannot answer,
/// which means it was aimed at a different key.
pub fn register(
    transport: &UdpTransport,
    server: SocketAddr,
    identity: &Identity,
) -> io::Result<SocketAddr> {
    let mut out = [0u8; MAX_MESSAGE_LEN];
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + SERVER_TIMEOUT;

    let claim = Message::Register {
        host: *identity.public(),
    };

    while Instant::now() < give_up {
        send(transport, &mut out, &claim, server)?;

        let retry_at = Instant::now() + RETRY_INTERVAL;
        while Instant::now() < retry_at {
            let Some(message) = recv_from_server(transport, &mut buf, server)? else {
                break;
            };

            match message {
                Message::Challenge { ephemeral, sealed } => {
                    let secret = answer(identity.private(), &ephemeral, &sealed).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "the rendezvous server challenged a key this machine does not hold",
                        )
                    })?;

                    send(
                        transport,
                        &mut out,
                        &Message::Prove {
                            host: *identity.public(),
                            secret,
                        },
                        server,
                    )?;
                }
                Message::Registered { observed } => return Ok(observed),
                _ => continue,
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the rendezvous server did not answer",
    ))
}

/// Keeps a registration and a router mapping alive until `stop` is set.
///
/// Runs on a duplicate of the session socket, because the mapping being held open is that
/// socket's. Failures are silent: a keepalive that does not arrive costs nothing until
/// several in a row do, and by then the host has stopped being listed anyway.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the socket cannot be duplicated.
pub fn spawn_keepalive(
    transport: &UdpTransport,
    server: SocketAddr,
    host: [u8; KEY_LEN],
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let transport = transport.try_clone()?;

    std::thread::spawn(move || {
        let mut out = [0u8; MAX_MESSAGE_LEN];
        let message = Message::Keepalive { host };

        while !stop.load(Ordering::Relaxed) {
            let _ = send(&transport, &mut out, &message, server);
            std::thread::sleep(KEEPALIVE_INTERVAL);
        }
    });

    Ok(())
}

/// Where a host is, and where the server sees this machine.
///
/// The second half is not needed to connect. It is what a person looking at a diagnostics
/// panel wants, and what says whether this machine's router hands out a stable mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    /// Where to send to reach the host.
    pub address: SocketAddr,
    /// Where the server saw this machine.
    pub observed: SocketAddr,
}

/// Asks the server where a host is and opens this side's router towards it.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] if the server knows no such host, which means it is
/// not running rather than that the key is wrong, and [`io::ErrorKind::TimedOut`] if the
/// server does not answer at all.
pub fn lookup(
    transport: &UdpTransport,
    server: SocketAddr,
    host: [u8; KEY_LEN],
    client: [u8; KEY_LEN],
) -> io::Result<Located> {
    let mut out = [0u8; MAX_MESSAGE_LEN];
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + SERVER_TIMEOUT;

    let request = Message::Connect { host, client };

    while Instant::now() < give_up {
        send(transport, &mut out, &request, server)?;

        let retry_at = Instant::now() + RETRY_INTERVAL;
        while Instant::now() < retry_at {
            let Some(message) = recv_from_server(transport, &mut buf, server)? else {
                break;
            };

            match message {
                Message::Found { address, observed } => return Ok(Located { address, observed }),
                Message::UnknownHost => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "the host is not registered, so it is probably not running",
                    ));
                }
                _ => continue,
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the rendezvous server did not answer",
    ))
}

/// Waits for the server to say a paired client is calling, and opens the router towards it.
///
/// Returns the caller's key and the address to expect it at. The key is advisory: it says who
/// the server thinks is calling, and the handshake that follows is what actually decides.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if nobody calls within `patience`.
pub fn await_caller(
    transport: &UdpTransport,
    server: SocketAddr,
    patience: Duration,
) -> io::Result<([u8; KEY_LEN], SocketAddr)> {
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + patience;

    while Instant::now() < give_up {
        let Some(message) = recv_from_server(transport, &mut buf, server)? else {
            continue;
        };

        if let Message::Incoming { client, address } = message {
            punch(transport, address)?;
            return Ok((client, address));
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "no client asked to connect",
    ))
}

/// Sends a handful of empty datagrams so this side's router will accept `peer`.
///
/// They carry nothing on purpose. The peer's handshake code ignores anything that is not a
/// handshake message, so an empty datagram costs it a branch and costs the router in between
/// exactly the mapping this needs.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the socket cannot be written to.
pub fn punch(transport: &UdpTransport, peer: SocketAddr) -> io::Result<()> {
    for _ in 0..PUNCHES {
        transport.send_to(&[], peer)?;
        std::thread::sleep(PUNCH_INTERVAL);
    }

    Ok(())
}

/// Receives one message, ignoring anything that did not come from the server.
///
/// `Ok(None)` means the read timed out, which the callers use to decide when to ask again.
/// Datagrams from anywhere else are session traffic or noise and are dropped here; telling
/// them apart by source is the only way, since a sealed packet looks like nothing in
/// particular by design.
fn recv_from_server(
    transport: &UdpTransport,
    buf: &mut [u8],
    server: SocketAddr,
) -> io::Result<Option<Message>> {
    let (len, from) = match transport.recv_from_into(buf) {
        Ok((bytes, from)) => (bytes.len(), from),
        Err(err) if is_timeout(&err) => return Ok(None),
        Err(err) => return Err(err),
    };

    if from != server {
        return Ok(None);
    }

    Ok(Message::decode(&buf[..len]).ok())
}

/// Encodes and sends one message.
fn send(
    transport: &UdpTransport,
    buf: &mut [u8],
    message: &Message,
    to: SocketAddr,
) -> io::Result<()> {
    let len = message
        .encode_into(buf)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    transport.send_to(&buf[..len], to)?;

    Ok(())
}

/// Returns whether an error is the read timeout rather than a real failure.
fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Where a relay was opened and how to reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relayed {
    /// The address to send the session's traffic to from now on.
    pub address: SocketAddr,
}

/// Asks the server to carry this session's traffic, and waits for it to open.
///
/// Called after punching has failed, which is what happens when both ends sit behind a NAT
/// that gives every destination a different mapping. Both peers are told the same token; each
/// presents it at the relay port and the server pairs the first two addresses that do.
///
/// The session that follows is byte for byte the session that would have run directly. The
/// server holds no key and forwards without looking, so relaying costs latency and the
/// server's bandwidth, and costs nothing in what the peers can prove about each other.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] if the server will not relay — it may have been started
/// with relaying off, or be full — and [`io::ErrorKind::TimedOut`] if the relay never opens,
/// which means the other peer never presented its token.
pub fn relay(
    transport: &UdpTransport,
    server: SocketAddr,
    host: [u8; KEY_LEN],
    client: [u8; KEY_LEN],
) -> io::Result<Relayed> {
    let mut out = [0u8; MAX_MESSAGE_LEN];
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + SERVER_TIMEOUT;

    let request = Message::Relay { host, client };
    let mut offered: Option<(SocketAddr, [u8; RELAY_TOKEN_LEN])> = None;

    while Instant::now() < give_up {
        match offered {
            // Still asking for a relay.
            None => send(transport, &mut out, &request, server)?,
            // Presenting the token at the relay port until the server answers, which it does
            // once the other peer has presented the same one.
            Some((relay_address, token)) => {
                transport.send_to(&token, relay_address)?;
            }
        }

        let retry_at = Instant::now() + RETRY_INTERVAL;
        while Instant::now() < retry_at {
            let (len, from) = match transport.recv_from_into(&mut buf) {
                Ok((bytes, from)) => (bytes.len(), from),
                Err(err) if is_timeout(&err) => break,
                Err(err) => return Err(err),
            };

            // The relay answers with the token itself, from the relay port. Nothing else on
            // this socket looks like that, and it is the only thing that says the relay is
            // carrying traffic rather than merely allocated.
            if let Some((relay_address, token)) = offered {
                if from == relay_address && buf[..len] == token {
                    return Ok(Relayed {
                        address: relay_address,
                    });
                }
            }

            if from != server {
                continue;
            }

            match Message::decode(&buf[..len]) {
                Ok(Message::Relaying { port, token }) => {
                    let mut relay_address = server;
                    relay_address.set_port(port);
                    offered = Some((relay_address, token));
                }
                Ok(Message::UnknownHost) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "the rendezvous server will not relay for this pair",
                    ));
                }
                _ => continue,
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the relay never opened, so the other side did not arrive",
    ))
}
