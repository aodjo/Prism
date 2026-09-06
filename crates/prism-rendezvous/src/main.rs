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

use clap::Parser;
use prism_core::net::rendezvous::{MAX_MESSAGE_LEN, Message, challenge};

use prism_rendezvous::registry::{Proved, Registry};

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

    let mut registry = Registry::new();
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    let mut reply = [0u8; MAX_MESSAGE_LEN];

    let mut swept = Instant::now();
    let mut reported = Instant::now();

    loop {
        let now = Instant::now();

        if now.duration_since(swept) >= SWEEP_INTERVAL {
            registry.expire(now);
            swept = now;
        }

        if now.duration_since(reported) >= REPORT_INTERVAL {
            println!(
                "prism-rendezvous: {} hosts registered, {} challenges outstanding",
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

        handle(&socket, &mut registry, &mut reply, message, from, now);
    }
}

/// Acts on one message.
fn handle(
    socket: &UdpSocket,
    registry: &mut Registry,
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

        // Messages the server sends rather than receives. Arriving here means a peer is
        // confused or someone is probing; either way there is nothing to answer.
        Message::Challenge { .. }
        | Message::Registered { .. }
        | Message::Incoming { .. }
        | Message::Found { .. }
        | Message::UnknownHost => {}
    }
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
    }
}

/// Returns whether an error is the read timeout rather than a real failure.
fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
