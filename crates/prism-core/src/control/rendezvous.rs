//! Finding a peer through the rendezvous server, and punching a hole to it.
//!
//! # Why there is more than one server
//!
//! A relayed session costs the server's distance on every round trip, so a pair in Seoul
//! should not be introduced — and certainly not relayed — through a machine in Frankfurt. The
//! answer is a server in each region a user might be in, and a way of choosing between them.
//!
//! The awkward part is that the two sides have to choose the *same* one: a registry lives in
//! one server's memory and servers do not talk to each other, so a host registered in Seoul is
//! a host that Frankfurt has never heard of. What resolves it is an asymmetry in what the two
//! sides can afford. A host registers with **every** server, which costs it one datagram per
//! server per keepalive interval and nothing else; a client then asks **all** of them at once
//! and uses whichever answers first, which is a measurement of the round trip rather than a
//! guess about geography.
//!
//! So there is no probe phase and no extra round trip. The request that finds the host is the
//! same request that measures which server is nearest, and the server that answered is the one
//! the pair will relay through if punching fails.
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
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::net::handshake::{Identity, KEY_LEN};
use crate::net::rendezvous::{MAX_MESSAGE_LEN, Message, REGION_LEN, RELAY_TOKEN_LEN, answer};
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

/// How long the keepalive thread sleeps between looking at its stop flag.
const KEEPALIVE_TICK: Duration = Duration::from_millis(100);

/// How many datagrams a host sends at a caller to open its own router.
///
/// More than one because any of them may be lost, and losing all of them costs a whole
/// connection attempt. They carry nothing: their only effect is on the router in between.
const PUNCHES: u32 = 5;

/// How long between them.
const PUNCH_INTERVAL: Duration = Duration::from_millis(50);

/// The rendezvous servers this machine may use, resolved from one name.
///
/// One hostname with several address records is what makes a region something the operator
/// adds by starting a machine and editing a zone file, rather than something every installed
/// client has to be updated to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Servers {
    addresses: Vec<SocketAddr>,
}

impl Servers {
    /// Resolves `name`, which is a host and port such as `rv.example.com:47300`.
    ///
    /// Every address the name resolves to is a candidate, so a name with one record behaves
    /// exactly as a single server always did.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the name cannot be resolved or resolves to
    /// nothing.
    pub fn resolve(name: &str) -> io::Result<Self> {
        let addresses: Vec<SocketAddr> = name.to_socket_addrs()?.collect();

        if addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the rendezvous name resolved to no addresses",
            ));
        }

        Ok(Self { addresses })
    }

    /// Returns the candidates.
    #[must_use]
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    /// Returns whether an address is one of the candidates.
    ///
    /// How a datagram from a server is told from session traffic: a sealed packet looks like
    /// nothing in particular by construction, so the source address is the only thing that
    /// can decide.
    #[must_use]
    pub fn holds(&self, address: SocketAddr) -> bool {
        self.addresses.contains(&address)
    }

    /// Returns how many candidates there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.addresses.len()
    }

    /// Returns whether there are no candidates, which [`Self::resolve`] never produces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }
}

impl From<SocketAddr> for Servers {
    /// Wraps a single address, for a caller that already has one and is not resolving a name.
    fn from(address: SocketAddr) -> Self {
        Self {
            addresses: vec![address],
        }
    }
}

/// A registration that succeeded, and where.
#[derive(Debug, Clone)]
pub struct Registration {
    /// Where the servers saw this machine.
    ///
    /// Taken from the first that answered. They should all agree — it is this socket's mapping
    /// and there is one of it — and a router that gives each destination a different mapping
    /// is one that cannot be punched to anyway.
    pub observed: SocketAddr,
    /// The servers that took the registration, which is where clients will find this machine.
    pub servers: Servers,
}

/// Registers this machine under its public key with every server that will have it.
///
/// All of them, rather than the nearest, because a client can only be introduced by a server
/// this machine is registered with — and which server the client will turn out to be nearest
/// to is not something the host can know. The cost is one datagram per server per keepalive
/// interval, which is nothing beside a session.
///
/// Servers that do not answer are left out rather than fatal. One region being unreachable
/// should cost the clients near that region, not every client everywhere.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if no server answers, and
/// [`io::ErrorKind::PermissionDenied`] if one issues a challenge this machine cannot answer,
/// which means it was aimed at a different key.
pub fn register(
    transport: &UdpTransport,
    servers: &Servers,
    identity: &Identity,
) -> io::Result<Registration> {
    let mut out = [0u8; MAX_MESSAGE_LEN];
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + SERVER_TIMEOUT;

    let claim = Message::Register {
        host: *identity.public(),
    };

    let mut registered: Vec<SocketAddr> = Vec::new();
    let mut observed: Option<SocketAddr> = None;

    while Instant::now() < give_up {
        // Asked of every server that has not answered yet, in one pass rather than one after
        // another. Waiting out a dead region before trying a live one would make the slowest
        // server decide how long starting a share takes.
        for &server in &servers.addresses {
            if !registered.contains(&server) {
                send(transport, &mut out, &claim, server)?;
            }
        }

        let retry_at = Instant::now() + RETRY_INTERVAL;
        while Instant::now() < retry_at {
            let Some((message, from)) = recv_from_servers(transport, &mut buf, servers)? else {
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
                        from,
                    )?;
                }
                Message::Registered { observed: at } => {
                    if !registered.contains(&from) {
                        registered.push(from);
                    }
                    observed.get_or_insert(at);
                }
                _ => continue,
            }
        }

        if registered.len() == servers.len() {
            break;
        }
    }

    match observed {
        Some(observed) => Ok(Registration {
            observed,
            servers: Servers {
                addresses: registered,
            },
        }),
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the rendezvous server did not answer",
        )),
    }
}

/// A registration being held open, which stops when this is dropped.
///
/// The thread holds a duplicate of the session socket, so it is not only a registration that
/// outlives its session but a port: until this thread ends, the address is taken and the next
/// session that tries to bind it is refused. Ending it is therefore something that has to
/// happen, not something to leave to the end of the process — so it happens on drop, and the
/// handle is carried alongside the session it belongs to.
#[derive(Debug)]
pub struct Keepalive {
    /// Set to end the thread.
    stop: Arc<AtomicBool>,
    /// The thread, taken when it is joined.
    thread: Option<JoinHandle<()>>,
}

impl Drop for Keepalive {
    /// Ends the keepalive and waits for its thread, so the socket is free on return.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Keeps a registration and a router mapping alive for as long as the returned handle lives.
///
/// Runs on a duplicate of the session socket, because the mapping being held open is that
/// socket's. Failures are silent: a keepalive that does not arrive costs nothing until
/// several in a row do, and by then the host has stopped being listed anyway.
///
/// One datagram to each server per interval. At a hundred and twenty-eight bytes every fifteen
/// seconds that is under a hundred bits a second per server, which is why a host can afford to
/// be registered everywhere rather than choosing.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the socket cannot be duplicated or the thread
/// cannot be spawned.
pub fn spawn_keepalive(
    transport: &UdpTransport,
    servers: &Servers,
    host: [u8; KEY_LEN],
) -> io::Result<Keepalive> {
    let transport = transport.try_clone()?;
    let stop = Arc::new(AtomicBool::new(false));
    let mine = Arc::clone(&stop);
    let servers = servers.clone();

    let thread = std::thread::Builder::new()
        .name("prism-keepalive".into())
        .spawn(move || {
            let mut out = [0u8; MAX_MESSAGE_LEN];
            let message = Message::Keepalive { host };
            let mut due = Instant::now();

            // Slept in short steps rather than in one long one. What is waited on here is not
            // the next keepalive but the order to stop, and a thread that checks for it every
            // fifteen seconds is a share that takes fifteen seconds to restart.
            while !mine.load(Ordering::Relaxed) {
                if Instant::now() >= due {
                    for &server in servers.addresses() {
                        let _ = send(&transport, &mut out, &message, server);
                    }
                    due = Instant::now() + KEEPALIVE_INTERVAL;
                }

                std::thread::sleep(KEEPALIVE_TICK);
            }
        })?;

    Ok(Keepalive {
        stop,
        thread: Some(thread),
    })
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
    /// Which server answered, and so which one to relay through if punching fails.
    ///
    /// The pair has to relay through one they are both registered with, and this is the one
    /// that just proved it knows the host — and proved, by answering first, that it is the
    /// nearest of them to this machine.
    pub server: SocketAddr,
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
    servers: &Servers,
    host: [u8; KEY_LEN],
    client: [u8; KEY_LEN],
) -> io::Result<Located> {
    let mut out = [0u8; MAX_MESSAGE_LEN];
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + SERVER_TIMEOUT;

    let request = Message::Connect { host, client };
    let mut unknown: Vec<SocketAddr> = Vec::new();

    while Instant::now() < give_up {
        // Asked of every server at once, and the first answer wins. That is the whole of the
        // choice: no probe, no extra round trip, and what decides is the measured time to
        // answer rather than a guess about where the machines are.
        for &server in servers.addresses() {
            if !unknown.contains(&server) {
                send(transport, &mut out, &request, server)?;
            }
        }

        let retry_at = Instant::now() + RETRY_INTERVAL;
        while Instant::now() < retry_at {
            let Some((message, from)) = recv_from_servers(transport, &mut buf, servers)? else {
                break;
            };

            match message {
                Message::Found { address, observed } => {
                    return Ok(Located {
                        address,
                        observed,
                        server: from,
                    });
                }
                // One server not knowing the host is not the host being absent: it may simply
                // be a region the host has not registered with. Only when every server says so
                // does it mean what it sounds like.
                Message::UnknownHost => {
                    if !unknown.contains(&from) {
                        unknown.push(from);
                    }
                }
                _ => continue,
            }
        }

        if unknown.len() == servers.len() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the host is not registered, so it is probably not running",
            ));
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
/// `cancelled` is read once per retry, so a host asked to stop while nobody has called stops
/// within one interval rather than at the end of its patience.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if nobody calls within `patience`, and
/// [`io::ErrorKind::Interrupted`] if `cancelled` is set.
pub fn await_caller(
    transport: &UdpTransport,
    servers: &Servers,
    patience: Duration,
    cancelled: &AtomicBool,
) -> io::Result<Caller> {
    let mut buf = [0u8; MAX_MESSAGE_LEN];

    transport.set_read_timeout(Some(RETRY_INTERVAL))?;
    let give_up = Instant::now() + patience;

    while Instant::now() < give_up {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "the session was stopped while it was waiting",
            ));
        }

        // From any of them. Which server introduces the pair is the client's choice, made by
        // whichever answered it first, and the host finds out by being told.
        let Some((message, from)) = recv_from_servers(transport, &mut buf, servers)? else {
            continue;
        };

        if let Message::Incoming { client, address } = message {
            punch(transport, address)?;
            return Ok(Caller {
                key: client,
                address,
                server: from,
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "no client asked to connect",
    ))
}

/// Who is calling, where from, and which server said so.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    /// The caller's key, as the server believes it.
    ///
    /// Advisory: it says who the server thinks is calling, and the handshake that follows is
    /// what actually decides.
    pub key: [u8; KEY_LEN],
    /// Where to expect the caller.
    pub address: SocketAddr,
    /// Which server introduced them, and so which one to relay through if punching fails.
    pub server: SocketAddr,
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

/// Receives one message, ignoring anything that did not come from one of the servers.
///
/// Returns which server sent it, because with more than one candidate the answer is not only
/// what was said but who said it: the server that answers a lookup is the one the pair will
/// relay through, and the one that introduces a caller is the one the host must relay through
/// to meet them.
///
/// `Ok(None)` means the read timed out, which the callers use to decide when to ask again.
/// Datagrams from anywhere else are session traffic or noise and are dropped here; telling
/// them apart by source is the only way, since a sealed packet looks like nothing in
/// particular by design.
fn recv_from_servers(
    transport: &UdpTransport,
    buf: &mut [u8],
    servers: &Servers,
) -> io::Result<Option<(Message, SocketAddr)>> {
    let (len, from) = match transport.recv_from_into(buf) {
        Ok((bytes, from)) => (bytes.len(), from),
        Err(err) if is_timeout(&err) => return Ok(None),
        Err(err) => return Err(err),
    };

    if !servers.holds(from) {
        return Ok(None);
    }

    Ok(Message::decode(&buf[..len])
        .ok()
        .map(|message| (message, from)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_resolves_to_itself() {
        let servers = Servers::resolve("127.0.0.1:47300").expect("a literal address resolves");

        assert_eq!(servers.len(), 1);
        assert!(servers.holds("127.0.0.1:47300".parse().expect("valid address")));
    }

    #[test]
    fn a_name_without_a_port_is_refused() {
        // Rather than guessed at. A rendezvous on the wrong port is a rendezvous that times
        // out, which reads as a server that is down.
        assert!(Servers::resolve("rv.example.com").is_err());
    }

    #[test]
    fn a_name_that_resolves_to_nothing_is_an_error_rather_than_an_empty_list() {
        // An empty list would make every later step succeed at doing nothing: no server to
        // register with, no server to ask, and no error to explain either.
        let outcome = Servers::resolve("no-such-host.invalid:47300");

        assert!(outcome.is_err());
    }

    #[test]
    fn only_the_servers_are_recognised() {
        let servers = Servers::from("10.0.0.1:47300".parse::<SocketAddr>().expect("valid"));

        assert!(servers.holds("10.0.0.1:47300".parse().expect("valid")));
        // Session traffic arrives on the same socket and must not be read as signalling.
        assert!(!servers.holds("10.0.0.2:47300".parse().expect("valid")));
        // The relay runs one port along, and its datagrams are not messages either.
        assert!(!servers.holds("10.0.0.1:47301".parse().expect("valid")));
    }
}

/// One server, as it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    /// Where it is.
    pub address: SocketAddr,
    /// What its operator named it, or empty if it did not say.
    pub region: String,
    /// How long the round trip took, in microseconds.
    pub rtt_us: u32,
}

/// How long to wait for servers to answer a probe.
///
/// Long enough for the far side of the world twice over, short enough that a settings window
/// does not sit blank. A server that has not answered in this is one nobody should be sent to.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Asks every server where it is, and how far away.
///
/// One datagram to each and whatever comes back, on a socket of its own — this is asked by a
/// window showing a list, not by a session, and it must not disturb one that is running.
///
/// The round trip is measured here rather than claimed by the server, so a server cannot make
/// itself look near. The name beside it is the server's own word and is cosmetic: what a person
/// picks between is the number.
///
/// Servers that do not answer are left out. A list is a list of what is there.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] only if a socket cannot be opened at all. A server
/// that fails to answer is an absence, not an error.
pub fn probe(servers: &Servers) -> io::Result<Vec<Sighting>> {
    let transport = UdpTransport::bind("0.0.0.0:0".parse().expect("valid bind address"))?;
    transport.set_read_timeout(Some(Duration::from_millis(100)))?;

    let mut out = [0u8; MAX_MESSAGE_LEN];
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    let mut sent: Vec<(SocketAddr, [u8; 8], Instant)> = Vec::with_capacity(servers.len());

    for (index, &server) in servers.addresses().iter().enumerate() {
        // Distinct per server, so an answer is matched to the question it belongs to rather
        // than to whichever question was asked last.
        let mut nonce = [0u8; 8];
        nonce[..8].copy_from_slice(&(index as u64).to_le_bytes());

        let asked = Instant::now();
        if send(&transport, &mut out, &Message::Where { nonce }, server).is_ok() {
            sent.push((server, nonce, asked));
        }
    }

    let mut seen: Vec<Sighting> = Vec::new();
    let give_up = Instant::now() + PROBE_TIMEOUT;

    while Instant::now() < give_up && seen.len() < sent.len() {
        let Ok((bytes, from)) = transport.recv_from_into(&mut buf) else {
            continue;
        };

        let Ok(Message::Here { nonce, region }) = Message::decode(bytes) else {
            continue;
        };

        let Some(&(server, _, asked)) = sent
            .iter()
            .find(|(server, expected, _)| *server == from && *expected == nonce)
        else {
            continue;
        };

        if seen.iter().any(|sighting| sighting.address == server) {
            continue;
        }

        seen.push(Sighting {
            address: server,
            region: read_region(&region),
            rtt_us: asked.elapsed().as_micros().min(u128::from(u32::MAX)) as u32,
        });
    }

    // Nearest first, which is the order a person reads a list of places to connect through.
    seen.sort_by_key(|sighting| sighting.rtt_us);

    Ok(seen)
}

/// Reads a server's name for itself out of its fixed-width field.
///
/// Trailing zero bytes are padding rather than content, and anything that is not UTF-8 is
/// dropped: a name is shown to a person, and one that arrived damaged is better absent than
/// rendered as replacement characters.
fn read_region(region: &[u8; REGION_LEN]) -> String {
    let end = region
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(REGION_LEN);

    core::str::from_utf8(&region[..end])
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}
