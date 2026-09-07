//! The Prism rendezvous server.
//!
//! One UDP socket on a machine both peers can reach. Hosts register under their public key
//! and are told where the server sees them; clients ask for a host by key and both sides are
//! told where the other is. From there they punch through their own routers and the server is
//! out of the way — it carries no video, no input, and no keys.
//!
//! # What it is trusted with
//!
//! Nothing. It cannot read a session and cannot forge one: the Noise handshake between the two
//! peers proves who is at each end regardless of what the server said. A hostile server can
//! refuse to introduce two peers or introduce them to a wrong address, and a wrong address
//! simply fails to handshake. That is the entire threat model, and it is why running this on a
//! cheap VPS is not a compromise.
//!
//! # Running it
//!
//! ```sh
//! prism-rendezvous --bind 0.0.0.0:47300
//! ```
//!
//! It keeps no files, needs no database, and holds a few tens of bytes per registered host.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex};

use clap::Parser;
use prism_core::net::packet::MAX_PACKET_SIZE;
use prism_core::net::rendezvous::{MAX_MESSAGE_LEN, Message, RELAY_TOKEN_LEN, challenge};

use prism_rendezvous::registry::{Proved, Registry};
use prism_rendezvous::relay::{Forward, Relays};

/// How often expired entries are swept out.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// How long a receive waits before the loop checks the clock.
///
/// Short enough that the sweep happens roughly on time on a quiet server, long enough that a
/// quiet server is not spinning.
const RECV_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the server prints what it is holding.
const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Command line for the rendezvous server.
#[derive(Debug, Parser)]
#[command(name = "prism-rendezvous", version, about = "Prism rendezvous server")]
struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:47300")]
    bind: SocketAddr,

    /// Print a line for every message, rather than a summary every minute.
    #[arg(long)]
    verbose: bool,

    /// Refuse to carry traffic for peers that could not reach each other directly.
    ///
    /// Relaying costs this machine's bandwidth and adds its distance to every round trip, so
    /// an operator who does not want to pay either can turn it off. Peers behind two symmetric
    /// NATs then cannot connect at all, which is the honest outcome.
    #[arg(long)]
    no_relay: bool,
}

/// Parses the command line and runs the server.
fn main() -> ExitCode {
    let cli = Cli::parse();

    match serve(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("prism-rendezvous: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the receive loop until the socket fails.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the socket cannot be bound, or fails in a way that
/// is not a timeout. A datagram that cannot be understood is never an error: most of what
/// arrives at an open port is not this protocol.
fn serve(cli: &Cli) -> io::Result<()> {
    let socket = UdpSocket::bind(cli.bind)?;
    socket.set_read_timeout(Some(RECV_TIMEOUT))?;

    println!("prism-rendezvous: listening on {}", socket.local_addr()?);

    // The relay lives on its own port and its own thread. Its own port so a sealed packet
    // never has to be told apart from a signalling message — see `relay` for why guessing
    // would be a bug rather than an inefficiency — and its own thread because it carries tens
    // of megabits while this loop handles a message every few minutes.
    let relays = Arc::new(Mutex::new(Relays::new()));
    let relay_port = if cli.no_relay {
        println!(
            "prism-rendezvous: relaying is off; peers behind two symmetric NATs cannot connect"
        );
        None
    } else {
        let port = spawn_relay(cli.bind, Arc::clone(&relays), cli.verbose)?;
        println!("prism-rendezvous: relaying on port {port}");
        Some(port)
    };

    let mut registry = Registry::new();
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    let mut reply = [0u8; MAX_MESSAGE_LEN];

    let mut swept = Instant::now();
    let mut reported = Instant::now();

    loop {
        let now = Instant::now();

        if now.duration_since(swept) >= SWEEP_INTERVAL {
            registry.expire(now);
            if let Ok(mut relays) = relays.lock() {
                relays.expire(now);
            }
            swept = now;
        }

        if now.duration_since(reported) >= REPORT_INTERVAL {
            let (open, waiting) = relays
                .lock()
                .map_or((0, 0), |relays| (relays.open(), relays.waiting()));
            println!(
                "prism-rendezvous: {} hosts registered, {} challenges outstanding, \
                 {open} relays open ({waiting} waiting)",
                registry.registered(),
                registry.outstanding()
            );
            reported = now;
        }

        let (len, from) = match socket.recv_from(&mut buf) {
            Ok(received) => received,
            Err(err) if is_timeout(&err) => continue,
            Err(err) => return Err(err),
        };

        // A datagram longer than any message this protocol defines is not one, and truncating
        // it into something that parses is exactly the mistake to avoid.
        if len >= MAX_MESSAGE_LEN {
            continue;
        }

        let Ok(message) = Message::decode(&buf[..len]) else {
            continue;
        };

        if cli.verbose {
            println!("prism-rendezvous: {from} sent {}", name_of(&message));
        }

        handle(
            &socket,
            &mut registry,
            &relays,
            relay_port,
            &mut reply,
            message,
            from,
            now,
        );
    }
}

/// Acts on one message.
#[allow(clippy::too_many_arguments)]
fn handle(
    socket: &UdpSocket,
    registry: &mut Registry,
    relays: &Mutex<Relays>,
    relay_port: Option<u16>,
    reply: &mut [u8],
    message: Message,
    from: SocketAddr,
    now: Instant,
) {
    match message {
        Message::Register { host } => {
            let Ok((message, secret)) = challenge(&host) else {
                return;
            };

            registry.challenge_issued(from, host, secret, now);
            send(socket, reply, &message, from);
        }

        Message::Prove { host, secret } => match registry.prove(from, &host, &secret, now) {
            Proved::Registered => {
                send(socket, reply, &Message::Registered { observed: from }, from);
            }
            // Silence for everything else. A caller that failed to prove a key learns
            // nothing about why, and a caller that never asked gets no reply to a message it
            // did not send — which is what keeps this from answering forged source addresses.
            Proved::NoChallenge | Proved::Wrong | Proved::Full => {}
        },

        Message::Keepalive { host } => {
            if registry.refresh(from, &host, now) {
                send(socket, reply, &Message::Registered { observed: from }, from);
            }
        }

        Message::Connect { host, client } => {
            if !registry.may_connect(from, now) {
                return;
            }

            let Some(address) = registry.lookup(&host, now) else {
                send(socket, reply, &Message::UnknownHost, from);
                return;
            };

            // The host is told first. Its answer to that address is what opens its own
            // router's mapping, and it needs to be on its way before the client starts
            // sending, or the client's first packets arrive at a closed door.
            send(
                socket,
                reply,
                &Message::Incoming {
                    client,
                    address: from,
                },
                address,
            );
            send(
                socket,
                reply,
                &Message::Found {
                    address,
                    observed: from,
                },
                from,
            );
        }

        Message::Relay { host, client } => {
            let Some(port) = relay_port else {
                return;
            };
            if !registry.may_connect(from, now) {
                return;
            }

            // Only a pair the server has already introduced. Without this anyone could ask it
            // to carry traffic for two keys it has never heard of, which is a machine
            // volunteering its bandwidth to strangers.
            let Some(address) = registry.lookup(&host, now) else {
                send(socket, reply, &Message::UnknownHost, from);
                return;
            };

            let Ok(fresh) = relay_token() else {
                return;
            };

            // The same token for both asks about the same pair. Each peer discovers on its own
            // that punching failed and asks separately; two tokens would leave each waiting at
            // the relay for a partner that was never coming.
            let Some(token) = relays
                .lock()
                .ok()
                .and_then(|mut relays| relays.token_for(host, client, fresh, now))
            else {
                return;
            };

            // Both sides are told at once. They present the token at the relay port and the
            // first two addresses to do so are paired, which is why the token has to be
            // unguessable: it is the only thing that says which two.
            let offer = Message::Relaying { port, token };
            send(socket, reply, &offer, from);
            send(socket, reply, &offer, address);
        }

        // Messages the server sends rather than receives. Arriving here means a peer is
        // confused or someone is probing; either way there is nothing to answer.
        Message::Challenge { .. }
        | Message::Registered { .. }
        | Message::Incoming { .. }
        | Message::Found { .. }
        | Message::Relaying { .. }
        | Message::UnknownHost => {}
    }
}

/// Starts the thread that carries relayed traffic.
///
/// Binds the port after the signalling one, on the same address, and returns it so peers can
/// be told where to send.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the port cannot be bound, which usually means
/// something else already has it.
fn spawn_relay(bind: SocketAddr, relays: Arc<Mutex<Relays>>, verbose: bool) -> io::Result<u16> {
    let mut relay_bind = bind;
    relay_bind.set_port(bind.port().wrapping_add(1));

    let socket = UdpSocket::bind(relay_bind)?;
    let port = socket.local_addr()?.port();
    socket.set_read_timeout(Some(RECV_TIMEOUT))?;

    std::thread::Builder::new()
        .name("prism-relay".into())
        .spawn(move || {
            // A full packet, because what arrives here is a session's own traffic rather than
            // a signalling message.
            let mut buf = [0u8; MAX_PACKET_SIZE];

            loop {
                let Ok((len, from)) = socket.recv_from(&mut buf) else {
                    continue;
                };

                let now = Instant::now();
                let Ok(mut held) = relays.lock() else {
                    return;
                };

                let outcome = held.accept(from, &buf[..len], now);

                if verbose && len < 64 {
                    // Only the short datagrams, which are token presentations. Logging a line
                    // per forwarded packet would print tens of thousands a second and be the
                    // slowest thing the relay does.
                    println!("prism-relay: {from} presented {len} bytes -> {outcome:?}");
                }

                match outcome {
                    // Forwarded byte for byte. No header, no rewriting, no length change: a
                    // relayed session and a direct one are the same session.
                    Forward::To(peer) => {
                        drop(held);
                        let _ = socket.send_to(&buf[..len], peer);
                    }
                    Forward::Registered => {
                        drop(held);
                        let _ = socket.send_to(&buf[..len], from);
                    }
                    Forward::Opened(other) => {
                        drop(held);
                        // Both sides are told, so the one that was already waiting stops
                        // presenting its token and starts sending.
                        let _ = socket.send_to(&buf[..len], from);
                        let _ = socket.send_to(&buf[..len], other);
                    }
                    Forward::Ignored => {}
                }
            }
        })?;

    Ok(port)
}

/// Makes a token for one relay session.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the platform has no usable randomness, which is a
/// condition no session should be allocated under.
fn relay_token() -> io::Result<[u8; RELAY_TOKEN_LEN]> {
    let mut token = [0u8; RELAY_TOKEN_LEN];
    getrandom::fill(&mut token).map_err(|err| io::Error::other(err.to_string()))?;

    Ok(token)
}

/// Sends one message, dropping it if the socket refuses.
///
/// A failed send is not worth ending the server over: the peer will ask again, and a server
/// that exited because one datagram could not be delivered would be trivially killable.
fn send(socket: &UdpSocket, buf: &mut [u8], message: &Message, to: SocketAddr) {
    if let Ok(len) = message.encode_into(buf) {
        let _ = socket.send_to(&buf[..len], to);
    }
}

/// Names a message for the verbose log.
fn name_of(message: &Message) -> &'static str {
    match message {
        Message::Register { .. } => "register",
        Message::Challenge { .. } => "challenge",
        Message::Prove { .. } => "prove",
        Message::Registered { .. } => "registered",
        Message::Connect { .. } => "connect",
        Message::Incoming { .. } => "incoming",
        Message::Found { .. } => "found",
        Message::UnknownHost => "unknown-host",
        Message::Keepalive { .. } => "keepalive",
        Message::Relay { .. } => "relay",
        Message::Relaying { .. } => "relaying",
    }
}

/// Returns whether an error is the read timeout rather than a real failure.
fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
