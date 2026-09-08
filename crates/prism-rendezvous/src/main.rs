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
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex};

use clap::Parser;
use prism_core::net::packet::MAX_PACKET_SIZE;
use prism_core::net::rendezvous::{MAX_MESSAGE_LEN, Message, RELAY_TOKEN_LEN, challenge};

use prism_rendezvous::mail::Mailer;
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

    /// Where accounts are kept, and the port to serve them on.
    ///
    /// Omitted means no account API at all: the server does what it always did, which is
    /// signalling for machines that paired by reading a code off one screen.
    #[arg(long)]
    accounts: Option<PathBuf>,

    /// Address to serve the account API on.
    ///
    /// The loopback by default, because this speaks plain HTTP and is meant to sit behind a
    /// reverse proxy that terminates TLS. Binding it anywhere else is refused unless
    /// `--api-insecure` says the exposure is deliberate: the value that signs somebody in
    /// crosses this connection, and a server that put it on the network in the clear would
    /// look exactly like one that did not.
    #[arg(long, default_value = "127.0.0.1:47380")]
    api_bind: SocketAddr,

    /// Serve the account API in the clear on an address other than the loopback.
    ///
    /// For a network where something else is providing confidentiality — a tunnel, a private
    /// link. Not for the internet.
    #[arg(long)]
    api_insecure: bool,

    /// Where signed-in machines should look for this server's signalling, as `host:port`.
    ///
    /// Handed out when somebody signs in, so that neither end has to be told by hand where to
    /// register. It cannot be worked out from `--bind`: a server behind a forwarded port or a
    /// name knows neither, and the address that matters is the one a machine somewhere else
    /// can reach. Omitted means machines are on their own to find each other, which on a
    /// single network they can.
    #[arg(long)]
    advertise: Option<String>,

    /// The mail provider's key, which is what lets this server prove an address is somebody's.
    ///
    /// Taken from the environment rather than typed on a command line, because a key on a
    /// command line is a key in every process listing and in the shell's history. Without it
    /// this server cannot send anything, so it creates accounts already verified and says so
    /// at startup — which is right for a server one person runs for their own machines and
    /// wrong for one anybody can reach.
    #[arg(long, env = "PRISM_RESEND_KEY", hide_env_values = true)]
    mail_key: Option<String>,

    /// The address confirmations come from, whose domain the provider must already hold.
    ///
    /// Something like `Prism <no-reply@example.com>`.
    #[arg(long, env = "PRISM_MAIL_FROM")]
    mail_from: Option<String>,

    /// Where this server is reachable from wherever somebody reads their mail.
    ///
    /// The base of the confirmation link, so it cannot be worked out from `--api-bind`: that
    /// is usually the loopback behind a proxy, and a link to the loopback opens nothing on the
    /// phone somebody is holding. Something like `https://rv.example.com`.
    #[arg(long, env = "PRISM_PUBLIC_URL")]
    public_url: Option<String>,

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
    // The account API, if this server keeps accounts. On its own thread with its own runtime,
    // because it is the only part of this server that is asynchronous and the loop below is
    // the only part that must never wait on anything.
    if let Some(path) = cli.accounts.clone() {
        spawn_accounts(
            path,
            cli.api_bind,
            cli.api_insecure,
            cli.advertise.clone().unwrap_or_default(),
            Post {
                key: cli.mail_key.clone(),
                from: cli.mail_from.clone(),
                public_url: cli.public_url.clone(),
            },
        )?;
    }

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

/// How this server sends the one message it sends, as the command line gave it.
#[derive(Debug, Clone)]
struct Post {
    key: Option<String>,
    from: Option<String>,
    public_url: Option<String>,
}

impl Post {
    /// Builds the mailer, or nothing when this deployment does not prove addresses.
    ///
    /// # Errors
    ///
    /// Returns an error if a key was given without an address to send from — a half-configured
    /// mailer would fail at the moment somebody registers rather than at startup, which is the
    /// wrong end of the day to find out.
    fn into_mailer(self, bind: SocketAddr) -> io::Result<Option<Mailer>> {
        let Some(key) = self.key.filter(|key| !key.trim().is_empty()) else {
            println!(
                "prism-rendezvous: no mail key, so an email address is only ever a name here \
                 — accounts are created already verified and nobody has to prove an address \
                 is theirs. Fine for your own machines; not for a server strangers can reach."
            );

            return Ok(None);
        };

        let Some(from) = self.from.filter(|from| !from.trim().is_empty()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--mail-key was given without --mail-from, so there is no address to send \
                 confirmations from",
            ));
        };

        let base = self
            .public_url
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| format!("http://{bind}"));

        if base.starts_with("http://") {
            println!(
                "prism-rendezvous: confirmation links point at {base}, which is not TLS. They \
                 have to open from wherever mail is read, so set --public-url to the name in \
                 front of this server."
            );
        }

        let mailer = Mailer::new(key, from.clone(), base.clone())
            .map_err(|err| io::Error::other(err.to_string()))?;

        println!("prism-rendezvous: confirmations sent from {from}, links under {base}");

        Ok(Some(mailer))
    }
}

/// Starts the account API on a thread of its own.
///
/// # Errors
///
/// Returns an error if the account store cannot be opened, if the address would put the API
/// in the clear on the network, or if the thread cannot be spawned.
fn spawn_accounts(
    path: PathBuf,
    bind: SocketAddr,
    insecure: bool,
    advertise: String,
    post: Post,
) -> io::Result<()> {
    use prism_rendezvous::accounts::Accounts;
    use prism_rendezvous::api::{Service, routes};
    use prism_rendezvous::sessions::Sessions;

    if !bind.ip().is_loopback() && !insecure {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the account API speaks plain HTTP and {bind} is not the loopback. Put a reverse \
                 proxy in front of it and bind the loopback, or pass --api-insecure if something \
                 else is providing confidentiality. The value that signs somebody in crosses \
                 this connection."
            ),
        ));
    }

    let accounts = Accounts::open(&path)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;

    println!(
        "prism-rendezvous: {} account(s) from {}",
        accounts.len(),
        path.display()
    );

    if bind.ip().is_loopback() {
        println!("prism-rendezvous: accounts on http://{bind} — put TLS in front of it");
    } else {
        println!("prism-rendezvous: accounts on http://{bind} IN THE CLEAR, as asked");
    }

    // Beside the accounts rather than under a flag of its own: it is the same deployment's
    // state, and a server whose sessions and accounts could be pointed at different places
    // would have one more way to be configured into signing everybody out.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sessions = Sessions::open(path.with_file_name("sessions.json"), now);
    println!("prism-rendezvous: {} session(s) still good", sessions.len());

    if advertise.is_empty() {
        println!(
            "prism-rendezvous: no --advertise, so signed-in machines are not told where to \
             register and can only reach each other directly"
        );
    } else {
        println!("prism-rendezvous: telling signed-in machines to register at {advertise}");
    }

    let mailer = post.into_mailer(bind)?;
    let service = Service::new(accounts, sessions, advertise, mailer);

    std::thread::Builder::new()
        .name("prism-accounts".into())
        .spawn(move || {
            // Its own runtime, because this is the only part of the server that is
            // asynchronous and the receive loop is the only part that must never wait.
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("prism-rendezvous: the account API could not start: {err}");
                    return;
                }
            };

            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::bind(bind).await {
                    Ok(listener) => listener,
                    Err(err) => {
                        eprintln!("prism-rendezvous: the account API could not bind {bind}: {err}");
                        return;
                    }
                };

                if let Err(err) = axum::serve(listener, routes(service)).await {
                    eprintln!("prism-rendezvous: the account API stopped: {err}");
                }
            });
        })?;

    Ok(())
}
