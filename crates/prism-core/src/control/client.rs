//! Client side: reassembles frames and, when asked, decodes them.
//!
//! Receiving and decoding run on separate threads, and that separation is not an
//! optimisation. A single-threaded client stops reading the socket while it decodes, so
//! packets queue in the kernel buffer and every frame behind the one being decoded is
//! measured as late. Collapsing the two inflated p99 from 5.9 ms to 51.8 ms in an early
//! version of this file while the pipeline itself was unchanged.
//!
//! The receive thread therefore does nothing but read, reassemble, and hand off. When the
//! decoder falls behind, frames are dropped rather than queued: a frame that has waited
//! behind another is already too late to be worth showing.
//!
//! # Why nothing here prints
//!
//! This ran as a command for long enough that its progress was sentences on standard output,
//! and the shell that drove it read them back — watching for the words "session established"
//! to decide a person was looking at a screen. That is a contract nobody declared and any
//! rewording breaks. What a caller wants to show now arrives as [`Report`], and the wording
//! belongs to whoever is doing the showing.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::clock::now_us;
use crate::net::ack::AckTracker;
use crate::net::clocksync::ClockSync;
use crate::net::handshake::{Identity, KEY_LEN};
use crate::net::negotiate::Offer;
use crate::net::packet::{
    AudioPacket, CLOCK_PING_LEN, Channel, ClockPing, ClockPong, ControlType, CursorPosition,
    FEEDBACK_PACKET_LEN, FEEDBACK_WANTS_KEYFRAME, FecPacket, INPUT_PACKET_LEN, InputEvent,
    InputPacket, MAX_PACKET_SIZE, VideoPacket, channel_of, control_type_of,
};
use crate::net::reassemble::{FrameReassembler, PushOutcome};
use crate::net::secure::{SecureReceiver, SecureSender};
use crate::net::transport::UdpTransport;

use crate::stats::{LatencyRecorder, LatencySummary};

/// Something that can play the audio arriving with a stream.
///
/// A trait because the thing that plays sound is the thing that owns a window, and this crate
/// owns none. A session with no window leaves it out and the frames are counted and dropped.
pub trait Playback: fmt::Debug + Send + Sync {
    /// Passes one decoded-order frame to playback.
    ///
    /// Called from the receive thread, so it must not block: a sink that waits here stalls
    /// video as well as sound.
    fn push(&self, sequence: u32, payload: &[u8], arrived_us: u64);
}

/// Where decoded pictures go when the client is showing them.
///
/// The payload type differs by platform because a decoded picture is whatever that platform's
/// decoder produced and stays in its own GPU's memory; the alias keeps the signatures below
/// identical everywhere.
#[cfg(target_os = "macos")]
pub type PictureSink = SyncSender<crate::decode::videotoolbox::DecodedFrame>;

/// Where decoded pictures go when the client is showing them.
#[cfg(target_os = "windows")]
pub type PictureSink = SyncSender<crate::decode::mediafoundation::DecodedFrame>;

/// Where decoded pictures would go on a platform with no decoder yet.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub type PictureSink = SyncSender<()>;

/// The GPU a client decodes onto, when something is drawing the pictures.
///
/// Windows only, because it is the one platform where the decoder is told which device to use
/// rather than finding one for itself. Handing over the renderer's device is what keeps a
/// decoded picture on the GPU the window is already on.
#[cfg(target_os = "windows")]
pub type Gpu = (
    windows::Win32::Graphics::Direct3D11::ID3D11Device,
    windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
);

/// How many frames may wait for the decoder before the newest is dropped.
///
/// Deeper than it looks like it needs to be, and deliberately. Every frame this pipeline
/// sends is a reference for the ones after it, so a frame dropped here is not one late
/// picture — it is every picture until the next keyframe. Eight covers a decoder that stalls
/// for a hundred milliseconds, which is far longer than any stall measured here, and costs
/// nothing when it does not.
const DECODE_QUEUE_DEPTH: usize = 8;

/// How often to ask the host for a clock synchronisation exchange.
///
/// Frequent at first would be better, but the estimate only improves when a round trip
/// happens to be faster than every one before it, and a quarter second is often enough to
/// find a good one early without adding meaningful traffic.
const PING_INTERVAL: Duration = Duration::from_millis(250);

/// How often the running client says what it is doing.
///
/// Once a second, which is fast enough for a number a person is watching and far under the cap
/// the interface side is held to. What crosses here is four integers on a line — the frames
/// themselves never leave this process, and this is the only thing about them that does.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Sentinel for "no clock offset has been established yet".
///
/// Shared with whoever else needs to convert host timestamps — the display thread reads
/// the same value to work out how old each picture is.
pub const OFFSET_UNKNOWN: i64 = i64::MIN;

/// Sends input events straight to the host.
///
/// Handed to the thread that captures input so it can write to the socket itself. Passing
/// events to the receive thread instead would cost up to a packet interval of waiting,
/// which is exactly the delay this path exists to avoid.
///
/// Nothing uses it away from macOS yet, because the window that captures input is the
/// only caller and only macOS has one. It is built everywhere regardless so the wire
/// side stays compiled and tested on every platform.
#[derive(Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct InputSender {
    /// Behind a lock because this is shared with the window thread and sealing needs the
    /// send buffer. The contention is nil — input is at most a thousand events a second and
    /// nothing else uses this handle — and the alternative, a second key for this direction,
    /// would mean a second nonce counter under the same key.
    sender: Mutex<SecureSender>,
    offset: Arc<AtomicI64>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl InputSender {
    /// Sends one input event immediately.
    ///
    /// No pacing and no batching: this is the one path where a millisecond is felt rather
    /// than seen.
    ///
    /// The timestamp is converted into the host's clock before sending, because this side
    /// is the one that knows the offset. Sending a raw local time would leave the host
    /// measuring the clock difference and calling it input latency.
    ///
    /// Returns the host-clock timestamp it stamped on the event, which is what lets the
    /// caller tell later whether a cursor reading from the host already accounts for this
    /// movement or predates it.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the packet cannot be sent.
    pub fn send(&self, event: InputEvent) -> io::Result<u64> {
        let offset = self.offset.load(Ordering::Relaxed);
        let now = now_us();
        let origin_ts_us = if offset == OFFSET_UNKNOWN {
            now
        } else {
            (i128::from(now) + i128::from(offset)).max(0) as u64
        };

        let packet = InputPacket {
            origin_ts_us,
            event,
        };

        let mut buf = [0u8; INPUT_PACKET_LEN];
        if packet.encode_into(&mut buf).is_ok() {
            let mut sender = self
                .sender
                .lock()
                .map_err(|_| io::Error::other("the input sender was poisoned"))?;
            sender.send(&buf)?;
        }

        Ok(origin_ts_us)
    }
}

/// The host's most recent pointer reading, for whoever is drawing the cursor.
///
/// A lock rather than an atomic because the reading is four fields and a timestamp that
/// have to be read together — a position paired with the wrong timestamp would replay the
/// wrong movements on top of it. It is taken once per drawn frame and held for a struct
/// copy, which is not the kind of work the no-locks rule exists to keep off this path.
pub type CursorSink = Arc<Mutex<Option<CursorPosition>>>;

/// What this build can decode and therefore offers a host.
///
/// Naming a codec with no decoder behind it would agree a session that never shows a frame, so
/// this is the one place that answer is written down and both callers ask it rather than
/// keeping a list of their own.
#[must_use]
pub fn decodable() -> crate::net::negotiate::Codecs {
    use crate::net::negotiate::{Codecs, H264};

    #[cfg(target_os = "macos")]
    {
        // Both: VideoToolbox decodes each in hardware, and which is used is whichever the host
        // can also produce.
        Codecs::none().with(H264).with(crate::net::negotiate::HEVC)
    }

    // Every other platform decodes nothing yet, so it offers the floor and gets a session it
    // can at least reassemble and measure.
    #[cfg(not(target_os = "macos"))]
    {
        Codecs::none().with(H264)
    }
}

/// The counters a running session publishes once a second.
///
/// Everything a person watching would want to see, and nothing that has to be parsed out of a
/// sentence to get at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Counters {
    /// Round trip to the host, in microseconds, as the clock exchange last measured it.
    pub round_trip_us: u64,
    /// Frames completed per second over the last interval.
    pub fps: f64,
    /// Everything arriving, in kilobits per second over the last interval.
    pub kbps: f64,
    /// Frames completed since the session opened.
    pub frames: u32,
}

/// What a running client has to say.
///
/// [`Report::Note`] is prose and may be reworded at any time; everything else is the state a
/// caller is allowed to act on.
#[derive(Debug, Clone)]
pub enum Report {
    /// Something worth a line in a log: progress, and what went wrong.
    Note(String),
    /// The handshake completed, and the session runs against this address.
    Established(SocketAddr),
    /// What the two sides settled on, once the session is open.
    Terms(crate::net::negotiate::Accept),
    /// The counters, once every [`STATS_INTERVAL`].
    Counters(Counters),
}

/// Where a client's reports go.
///
/// Called from the receive thread, so what it does with a report has to be cheap: this is the
/// thread that must never stop reading the socket.
#[derive(Clone)]
pub struct Reporter(Arc<dyn Fn(Report) + Send + Sync>);

impl Reporter {
    /// Wraps a function that takes reports.
    #[must_use]
    pub fn new(to: impl Fn(Report) + Send + Sync + 'static) -> Self {
        Self(Arc::new(to))
    }

    /// Sends one report.
    pub fn send(&self, report: Report) {
        (self.0)(report);
    }

    /// Sends a line of prose.
    pub fn note(&self, line: impl Into<String>) {
        self.send(Report::Note(line.into()));
    }
}

impl fmt::Debug for Reporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reporter")
    }
}

/// A reporter that may not be there, so the calling code does not say so at every site.
#[derive(Debug, Clone, Default)]
struct Say(Option<Reporter>);

impl Say {
    /// Sends one report, if anyone is listening.
    fn send(&self, report: Report) {
        if let Some(Reporter(to)) = &self.0 {
            to(report);
        }
    }

    /// Sends a line of prose.
    fn note(&self, line: String) {
        self.send(Report::Note(line));
    }
}

/// What the caller wants from a client session beyond the counters it reports.
#[derive(Debug, Default)]
pub struct ClientHooks {
    /// Where decoded pictures go, when someone is showing them.
    pub pictures: Option<PictureSink>,
    /// Published as the clock offset is learned, for anyone converting host timestamps.
    pub offset: Option<Arc<AtomicI64>>,
    /// Filled in once the host's address is known, so input can be sent to it.
    pub input: Option<Arc<OnceLock<InputSender>>>,
    /// Updated as the host reports where its pointer is.
    pub cursor: Option<CursorSink>,
    /// Where arriving audio frames go, when this machine can play them.
    ///
    /// Absent when nothing is showing the stream, because a session with no window is a
    /// measurement run and playing its audio out loud would be a surprise.
    pub audio: Option<Arc<dyn Playback>>,
    /// Where the session says what it is doing.
    ///
    /// Absent for a run whose caller only wants the return value.
    pub report: Option<Reporter>,
    /// The GPU to decode onto, when a renderer already has one.
    ///
    /// Absent for a run with no window, which decodes onto whichever device the decoder finds
    /// for itself because nothing is going to draw the result.
    #[cfg(target_os = "windows")]
    pub gpu: Option<Gpu>,
}

/// How the receiving client should behave.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Address of the host, when it is directly reachable.
    ///
    /// `None` means ask the rendezvous server, which is what a host behind NAT requires.
    pub host: Option<SocketAddr>,
    /// Rendezvous to find the host through, as a name and port.
    ///
    /// A name rather than an address: every record it resolves to is a region, all of them are
    /// asked at once, and whichever answers first is both the nearest and the one the pair
    /// will relay through if punching fails.
    pub rendezvous: Option<String>,
    /// What this machine can decode and present.
    ///
    /// Sent in the message that opens the session, so the host has chosen a codec by the time
    /// the session is live.
    pub offer: Offer,
    /// Go through the relay without trying a direct path first.
    ///
    /// For measuring what relaying costs, and for a person on a path where punching succeeds
    /// and then stops working — which looks like a session that opens and dies rather than one
    /// that never opens.
    pub force_relay: bool,
    /// Stop after this many frames, or run until idle if `None`.
    pub frames: Option<u32>,
    /// Give up after this long with no packets.
    pub idle_timeout: Duration,
    /// Print a running summary every this many frames.
    pub report_every: u32,
    /// How many frames may be in flight before the oldest is abandoned.
    pub in_flight: usize,
    /// Decode the reassembled frames rather than only counting them.
    pub decode: bool,
    /// This machine's long-term key.
    pub identity: Identity,
    /// The host's public key, as pairing recorded it.
    ///
    /// This side dials, so this is the key it encrypts its very first message to. A host that
    /// does not hold the matching private key cannot read that message at all, which is what
    /// makes standing in the middle useless rather than merely detectable.
    pub peer_key: [u8; KEY_LEN],
}

/// A reassembled frame on its way from the receive thread to the decode thread.
///
/// The buffer is recycled back to the receive thread after decoding, so a running
/// session does not allocate.
#[derive(Debug, Default)]
struct FrameBuf {
    capture_ts_us: u64,
    data: Vec<u8>,
}

/// What the decode thread reports when it finishes.
#[derive(Debug, Default)]
struct DecodeReport {
    decoded: u32,
    /// Submissions that produced no picture before the poll timeout expired.
    starved: u32,
    /// End to end, from just before the encoder to just after the decoder.
    summary: Option<LatencySummary>,
    /// The decode stage alone, from submitting a frame to holding its picture.
    stage: Option<LatencySummary>,
    /// How far the picture that came out lags the frame that went in.
    lag: Option<LatencySummary>,
    errors: Vec<i32>,
}

/// Describes an agreed picture size for a person to read.
///
/// A size at the ceiling means neither side constrained the other, which reads as a number in
/// the sixty-thousands and means nothing. Saying so is more use than printing it.
fn describe_size(width: u16, height: u16) -> String {
    if width >= u16::MAX - 1 && height >= u16::MAX - 1 {
        return "whatever the host's screen is".to_string();
    }

    format!("up to {width}x{height}")
}

/// Works out where the host is: the address given, or the one the rendezvous server reports.
///
/// The lookup happens on the session socket, because the address the server observes is only
/// reachable at the port that created it.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] if neither an address nor a server was given,
/// [`io::ErrorKind::NotFound`] if the server knows no such host, and the underlying
/// [`io::Error`] for a socket failure.
fn open(
    transport: &UdpTransport,
    config: &ClientConfig,
    say: &Say,
) -> io::Result<(crate::net::handshake::Established, SocketAddr)> {
    use crate::control::rendezvous;
    use crate::control::session::{DIRECT_PATIENCE, LOCAL_PATIENCE, RELAYED_PATIENCE, dial};

    // An address given by hand is one somebody has arranged to be reachable, so there is
    // nothing to fall back to and nothing to punch.
    if let Some(host) = config.host {
        let established = dial(
            transport,
            host,
            &config.identity,
            &config.peer_key,
            config.offer,
            RELAYED_PATIENCE,
        )?;

        return Ok((established, host));
    }

    let Some(name) = config.rendezvous.as_deref() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "give either --host or --rendezvous so the host can be found",
        ));
    };

    // Every address the name resolves to is a region to ask. Resolved now rather than when the
    // settings were written, so a region added since is one this session already knows about.
    let servers = rendezvous::Servers::resolve(name)?;

    let me = *config.identity.public();
    let found = rendezvous::lookup(transport, &servers, config.peer_key, me)?;
    if servers.len() > 1 {
        say.note(format!(
            "asked {} rendezvous servers, {} answered first",
            servers.len(),
            found.server
        ));
    }
    say.note(format!(
        "the host is at {}, and this machine appears at {}",
        found.address, found.observed
    ));

    // One public address for both machines means one router between them and the internet,
    // and two machines behind one router usually cannot reach each other at that address —
    // the packet leaves, the router has no reason to send it back in, and the connection
    // fails on the same network where it should be fastest. The host reported where it is on
    // that network when it registered, so it is tried before anything else.
    if !config.force_relay
        && found.address.ip() == found.observed.ip()
        && found.local.ip() != found.address.ip()
    {
        say.note(format!("the host is on this network at {}", found.local));

        match dial(
            transport,
            found.local,
            &config.identity,
            &config.peer_key,
            config.offer,
            LOCAL_PATIENCE,
        ) {
            Ok(established) => {
                say.note(format!("connected across the network to {}", found.local));

                return Ok((established, found.local));
            }
            Err(err) if err.kind() != io::ErrorKind::TimedOut => return Err(err),
            Err(_) => {}
        }
    }

    // Both sides punch. The handshake message about to be sent repeatedly is this side's own
    // punch, but the host's router will only pass it once the host has sent outward here —
    // which the server has just told it to do.
    // Skipping the punch as well as the dial: a punch is only useful to a path that is about
    // to be tried.
    if config.force_relay {
        say.note("skipping the direct path because it was asked to".to_owned());
    } else {
        rendezvous::punch(transport, found.address)?;

        match dial(
            transport,
            found.address,
            &config.identity,
            &config.peer_key,
            config.offer,
            DIRECT_PATIENCE,
        ) {
            Ok(established) => {
                say.note(format!("connected directly to {}", found.address));

                return Ok((established, found.address));
            }
            Err(err) if err.kind() != io::ErrorKind::TimedOut => return Err(err),
            Err(_) => {}
        }
    }

    // Punching failed, which means both routers hand out a different mapping for every
    // destination. There is no address to reach the host at, so the server carries it — at the
    // cost of its bandwidth and its distance added to every round trip, which is why this is
    // reached rather than chosen.
    say.note("no direct path opened; asking the rendezvous server to relay".to_owned());

    // Through the one that answered the lookup. A relay pairs two peers presenting the same
    // token, so it has to be a server they are both registered with — and that one has just
    // proved both that it knows the host and that it is the nearest of them to here.
    let relayed = rendezvous::relay(transport, found.server, config.peer_key, me)?;
    let established = dial(
        transport,
        relayed.address,
        &config.identity,
        &config.peer_key,
        config.offer,
        RELAYED_PATIENCE,
    )?;

    say.note(format!("relaying through {}", relayed.address));

    Ok((established, relayed.address))
}

/// Receives packets until the frame budget or the idle timeout is reached.
///
/// # Errors
///
/// Returns an [`io::Error`] if the socket cannot be bound or read, other than the
/// timeout that ends the run normally.
pub fn run(config: ClientConfig, hooks: ClientHooks) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    let gpu = hooks.gpu.clone();
    let ClientHooks {
        pictures,
        offset,
        input,
        cursor,
        audio,
        report,
        ..
    } = hooks;
    let say = Say(report);
    let offset = offset.unwrap_or_else(|| Arc::new(AtomicI64::new(OFFSET_UNKNOWN)));
    let transport = UdpTransport::bind("0.0.0.0:0".parse().expect("valid bind address"))?;

    say.note(format!(
        "connecting ({} frames in flight, decode {})",
        config.in_flight,
        if config.decode { "on" } else { "off" }
    ));

    // Nothing is read as a packet until the handshake completes, and it only completes with
    // the host pairing recorded: the first message is encrypted to that key and no other.
    let (established, host) = open(&transport, &config, &say)?;
    let agreed = crate::control::session::agreed(&established)?;

    // Connected only now that it is settled where the session runs. Doing it earlier would
    // have made the fallback to a relay impossible: a connected socket refuses to send
    // anywhere else.
    transport.connect(host)?;
    transport.set_read_timeout(Some(config.idle_timeout))?;

    say.note(format!(
        "session established with {host} ({})",
        crate::identity::to_hex(&established.session.peer_static)
    ));
    say.note(format!(
        "agreed {:?}, {}, {} fps, {:.1} Mbps, audio {}",
        agreed.codec,
        describe_size(agreed.width, agreed.height),
        agreed.fps,
        f64::from(agreed.bitrate_bps) / 1e6,
        if agreed.audio { "on" } else { "off" },
    ));
    // The state, after the prose. These two are what a caller acts on, and they are the reason
    // nothing outside this file has to recognise a sentence.
    say.send(Report::Established(host));
    say.send(Report::Terms(agreed));

    let (frames_tx, frames_rx) = sync_channel::<FrameBuf>(DECODE_QUEUE_DEPTH);
    let (recycle_tx, recycle_rx) = channel::<FrameBuf>();
    let decoder = config.decode.then(|| {
        spawn_decoder(
            frames_rx,
            recycle_tx,
            pictures,
            Arc::clone(&offset),
            agreed.codec,
            say.clone(),
            #[cfg(target_os = "windows")]
            gpu,
        )
    });

    let mut reassembler = FrameReassembler::new(config.in_flight);
    let mut arrival = LatencyRecorder::new(4096);
    let mut recv_buf = [0u8; MAX_PACKET_SIZE];
    let mut malformed = 0u64;
    let mut frames = 0u32;
    let mut behind = 0u32;
    let mut unsynced = 0u32;
    let mut sync = ClockSync::new();
    let mut last_ping = Instant::now() - PING_INTERVAL;
    let mut last_stats = Instant::now();
    let mut window_frames = 0u32;
    let mut window_bytes = 0u64;
    let mut pings_sent = 0u32;
    let mut pongs_seen = 0u32;
    let mut ping_buf = [0u8; CLOCK_PING_LEN];
    let mut acks = AckTracker::new();
    let mut reports_sent = 0u64;
    let mut reports_failed = 0u64;
    let mut audio_frames = 0u64;
    let mut audio_bytes = 0u64;
    // Set when this side has lost a frame, and cleared once the host has answered with a
    // keyframe. Until then every report carries the request, because a single one can be lost
    // and the stream stays black until one arrives.
    let mut wants_keyframe = false;
    let mut lost_frames = 0u64;
    let mut queued_behind = 0u32;

    let mut sender = SecureSender::new(transport.try_clone()?, established.session.sealer);
    let mut receiver = SecureReceiver::new(transport.try_clone()?, established.session.opener);

    if let Some(slot) = input.as_ref() {
        if let Ok(split) = sender.split() {
            let _ = slot.set(InputSender {
                sender: Mutex::new(split),
                offset: Arc::clone(&offset),
            });
        }
    }

    loop {
        if last_ping.elapsed() >= PING_INTERVAL {
            last_ping = Instant::now();
            let ping = ClockPing { t1_us: now_us() };
            if ping.encode_into(&mut ping_buf).is_ok() {
                match sender.send(&ping_buf) {
                    Ok(_) => pings_sent += 1,
                    Err(err) => say.note(format!("ping to {host} failed: {err}")),
                }
            }
        }

        let bytes = match receiver.recv_into(&mut recv_buf) {
            Ok(bytes) => bytes,
            Err(err) if is_timeout(&err) => break,
            Err(err) => return Err(err),
        };

        window_bytes += bytes.len() as u64;

        // Emitted here rather than where a frame completes, so that a stream which has stopped
        // producing pictures still says so with zeroes instead of going quiet — which is
        // indistinguishable, from the outside, from a client that has died.
        if last_stats.elapsed() >= STATS_INTERVAL {
            let seconds = last_stats.elapsed().as_secs_f64();
            last_stats = Instant::now();

            say.send(Report::Counters(Counters {
                round_trip_us: sync.round_trip_us().unwrap_or(0),
                fps: f64::from(window_frames) / seconds,
                kbps: (window_bytes as f64 * 8.0 / 1000.0) / seconds,
                frames,
            }));

            window_frames = 0;
            window_bytes = 0;
        }

        if channel_of(bytes) == Ok(Channel::Control) {
            let t4_us = now_us();

            match control_type_of(bytes) {
                Ok(ControlType::ClockPong) => {
                    pongs_seen += 1;
                    if let Ok(pong) = ClockPong::decode(bytes) {
                        if sync.observe(&pong, t4_us).is_some() {
                            offset.store(
                                sync.offset_us().unwrap_or(OFFSET_UNKNOWN),
                                Ordering::Relaxed,
                            );
                        }
                    }
                }
                Ok(ControlType::CursorPosition) => {
                    if let (Some(sink), Ok(reading)) =
                        (cursor.as_ref(), CursorPosition::decode(bytes))
                    {
                        if let Ok(mut slot) = sink.lock() {
                            *slot = Some(reading);
                        }
                    }
                }
                _ => {}
            }

            continue;
        }

        // Audio has its own path from here on: no reassembly, no parity, and a jitter buffer
        // of its own. Sharing the video path's machinery would make every one of its decisions
        // wrong for sound, which is five millisecond frames rather than sixteen and a
        // concealed gap rather than a repaired one.
        if channel_of(bytes) == Ok(Channel::Audio) {
            match AudioPacket::decode(bytes) {
                Ok(packet) => {
                    audio_frames += 1;
                    audio_bytes += packet.payload.len() as u64;

                    // Counted whether or not there is anywhere to play it. A run with no
                    // window still says whether sound crossed the wire, which is the only way
                    // to tell "the host is not capturing" from "this machine is not playing".
                    if let Some(sink) = audio.as_ref() {
                        sink.push(packet.sequence, packet.payload, now_us());
                    }
                }
                Err(_) => malformed += 1,
            }

            continue;
        }

        // Parity goes to the same reassembler, which is what lets it repair a slice the
        // moment it has enough shards rather than at some later reconciliation step.
        let outcome = if channel_of(bytes) == Ok(Channel::Fec) {
            match FecPacket::decode(bytes) {
                Ok(packet) => reassembler.push_fec(&packet),
                Err(_) => {
                    malformed += 1;
                    continue;
                }
            }
        } else {
            match VideoPacket::decode(bytes) {
                Ok(packet) => reassembler.push(&packet),
                Err(_) => {
                    malformed += 1;
                    continue;
                }
            }
        };

        if outcome != PushOutcome::FrameComplete {
            continue;
        }

        if reassembler.stats().dropped_incomplete > lost_frames || behind > queued_behind {
            lost_frames = reassembler.stats().dropped_incomplete;
            queued_behind = behind;
            wants_keyframe = true;
        }

        let Some(frame) = reassembler.take_completed() else {
            continue;
        };

        // Acknowledged before anything else is done with the frame. The report is what lets
        // the host reference this frame instead of sending a keyframe when the next one is
        // lost, and every microsecond it waits is a microsecond the encoder spends choosing
        // a reference it did not have to.
        acks.received(frame.frame_id);
        // A frame that never completed is a frame every later one refers to. Nothing decodes
        // again until a keyframe arrives, so this is where it is asked for. Read before the
        // completed frame is taken, because taking it borrows the reassembler.
        if frame.is_idr {
            wants_keyframe = false;
        }

        let flags = if wants_keyframe {
            FEEDBACK_WANTS_KEYFRAME
        } else {
            0
        };

        if let Some(report) = acks.report(now_us(), flags) {
            let mut buf = [0u8; FEEDBACK_PACKET_LEN];
            if report.encode_into(&mut buf).is_ok() {
                match sender.send(&buf) {
                    Ok(_) => reports_sent += 1,
                    Err(_) => reports_failed += 1,
                }
            }
        }

        match age_of(frame.capture_ts_us, offset.load(Ordering::Relaxed)) {
            Some(age) => arrival.record(age),
            None => unsynced += 1,
        }
        frames += 1;
        window_frames += 1;

        if config.decode {
            let mut buf = recycle_rx.try_recv().unwrap_or_default();
            buf.capture_ts_us = frame.capture_ts_us;
            buf.data.clear();
            buf.data.extend_from_slice(frame.data);

            if frames_tx.try_send(buf).is_err() {
                behind += 1;
            }
        }

        if config.report_every > 0 && frames % config.report_every == 0 {
            summarize(&say, "arrival ", &mut arrival);
        }
        if config.frames.is_some_and(|target| frames >= target) {
            break;
        }
    }

    drop(frames_tx);
    let decode_report = decoder.map(|handle| handle.join().unwrap_or_default());

    say.note(format!(
        "{frames} frames reassembled, {malformed} packets unparseable"
    ));
    match (sync.offset_us(), sync.round_trip_us()) {
        (Some(offset_us), Some(round_trip_us)) => say.note(format!(
            "clock: host is {:+.2} ms from this machine, best round trip {:.2} ms ({} samples, {} refused)",
            offset_us as f64 / 1000.0,
            round_trip_us as f64 / 1000.0,
            sync.accepted(),
            sync.rejected()
        )),
        _ => say.note(format!(
            "clock: never synchronised ({pings_sent} pings sent, {pongs_seen} answers seen), \
             so cross-machine latency is unmeasurable"
        )),
    }
    if unsynced > 0 {
        say.note(format!(
            "{unsynced} frames could not be timed — the host stamp was in the future even \
             after correcting for the clock offset"
        ));
    }
    summarize(&say, "arrival ", &mut arrival);

    if let Some(decode_report) = decode_report {
        say.note(format!(
            "{} frames decoded, {behind} dropped at the decoder, {} produced no picture in time",
            decode_report.decoded, decode_report.starved
        ));
        stage(&say, "decode  ", decode_report.stage);
        stage(&say, "outlag  ", decode_report.lag);
        stage(&say, "pipeline", decode_report.summary);
        if !decode_report.errors.is_empty() {
            say.note(format!(
                "decoder reported {} failures: {:?}",
                decode_report.errors.len(),
                decode_report.errors
            ));
        }
    }

    let stats = reassembler.stats();
    say.note(format!(
        "packets  accepted {}  duplicate {}  stale {}  invalid {}",
        stats.accepted, stats.duplicates, stats.stale, stats.invalid
    ));
    say.note(format!(
        "frames   completed {}  dropped incomplete {}",
        stats.completed, stats.dropped_incomplete
    ));
    if stats.recovered > 0 {
        say.note(format!(
            "recovery {} slices rebuilt from parity",
            stats.recovered
        ));
    }
    say.note(format!(
        "feedback sent {reports_sent}  failed to send {reports_failed}"
    ));

    if audio_frames > 0 {
        // Five milliseconds a frame, so the count is also how long the sound was. Reported
        // against the run's own length because the useful question is whether it was
        // continuous, not how much of it there was.
        say.note(format!(
            "audio    {audio_frames} frames ({:.1}s of sound), {:.1} kB, mean {} bytes{}",
            audio_frames as f64 * f64::from(crate::audio::FRAME_US) / 1e6,
            audio_bytes as f64 / 1e3,
            audio_bytes / audio_frames,
            if audio.is_some() {
                ", played"
            } else {
                ", not played (no window)"
            }
        ));
    } else if agreed.audio {
        say.note(
            "audio    none arrived, though the session agreed to it. The host has no system \
             audio capture on its platform, or it was started without audio."
                .to_owned(),
        );
    }

    Ok(())
}

/// How long the decode thread waits for a picture before moving on.
///
/// Short on purpose. Blocking here stalls the whole decode thread, so every frame behind the
/// one being waited for is measured as late and may be dropped. Roughly two frame intervals is
/// long enough to absorb the decoder's own pipelining and short enough that a stall cannot
/// cascade.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const POLL_TIMEOUT: Duration = Duration::from_millis(8);

/// What the decode loop needs of a platform's decoder.
///
/// Both backends already have this shape. Naming it is what lets one loop drive either, so the
/// counters, the timestamps and the bitstream dump behave identically on the two clients by
/// construction rather than by two people having written the same thing twice.
#[cfg(any(target_os = "macos", target_os = "windows"))]
trait Decoder {
    /// What this decoder hands back, which stays in its own platform's GPU memory.
    type Picture;

    /// Submits one Annex B frame.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports; [`crate::decode::DecodeError::NoParameterSets`] is
    /// the ordinary case of a stream that has not described itself yet.
    fn decode(&mut self, annexb: &[u8], pts_us: u64) -> Result<(), crate::decode::DecodeError>;

    /// Waits up to `timeout` for a picture.
    fn poll(&mut self, timeout: Duration) -> Option<Self::Picture>;

    /// Returns the presentation timestamp a picture was submitted with.
    fn pts_of(picture: &Self::Picture) -> u64;

    /// Takes the status codes the backend reported and were not fatal.
    fn take_errors(&mut self) -> Vec<i32>;
}

#[cfg(target_os = "macos")]
impl Decoder for crate::decode::videotoolbox::VideoToolboxDecoder {
    type Picture = crate::decode::videotoolbox::DecodedFrame;

    fn decode(&mut self, annexb: &[u8], pts_us: u64) -> Result<(), crate::decode::DecodeError> {
        Self::decode(self, annexb, pts_us)
    }

    fn poll(&mut self, timeout: Duration) -> Option<Self::Picture> {
        Self::poll(self, timeout)
    }

    fn pts_of(picture: &Self::Picture) -> u64 {
        picture.pts_us
    }

    fn take_errors(&mut self) -> Vec<i32> {
        Self::take_errors(self)
    }
}

#[cfg(target_os = "windows")]
impl Decoder for crate::decode::mediafoundation::MediaFoundationDecoder {
    type Picture = crate::decode::mediafoundation::DecodedFrame;

    fn decode(&mut self, annexb: &[u8], pts_us: u64) -> Result<(), crate::decode::DecodeError> {
        Self::decode(self, annexb, pts_us)
    }

    fn poll(&mut self, timeout: Duration) -> Option<Self::Picture> {
        Self::poll(self, timeout)
    }

    fn pts_of(picture: &Self::Picture) -> u64 {
        picture.pts_us
    }

    fn take_errors(&mut self) -> Vec<i32> {
        Self::take_errors(self)
    }
}

/// Decodes everything the receive thread hands over and reports what came of it.
///
/// The decoder is moved in and never leaves, which keeps every platform session object on the
/// one thread that owns it.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn decode_until_closed<D: Decoder>(
    mut decoder: D,
    frames: &Receiver<FrameBuf>,
    recycle: &Sender<FrameBuf>,
    pictures: Option<SyncSender<D::Picture>>,
    offset: &Arc<AtomicI64>,
    say: &Say,
) -> DecodeReport {
    // A developer affordance: writes exactly what is handed to the decoder, so a stream the
    // decoder refuses can be put in front of an independent one. A bitstream that ffmpeg reads
    // and this decoder does not is a different bug from one neither will touch, and there is no
    // way to tell them apart without the bytes.
    let mut dump =
        std::env::var_os("PRISM_DUMP_BITSTREAM").and_then(|path| std::fs::File::create(path).ok());

    let mut latency = LatencyRecorder::new(4096);
    let mut stage = LatencyRecorder::new(4096);
    let mut lag = LatencyRecorder::new(4096);
    let mut decoded = 0u32;
    let mut starved = 0u32;

    while let Ok(buf) = frames.recv() {
        let started = std::time::Instant::now();

        if let Some(file) = dump.as_mut() {
            use std::io::Write;
            let _ = file.write_all(&buf.data);
        }

        match decoder.decode(&buf.data, buf.capture_ts_us) {
            Ok(()) | Err(crate::decode::DecodeError::NoParameterSets) => {}
            Err(err) => say.note(err.to_string()),
        }

        // Everything the decoder has finished, not just the first of it. Waiting once and
        // taking one picture per frame submitted means a decoder that ever falls behind stays
        // behind for the rest of the session: it is handed one and gives back one, and the
        // gap between the two never closes.
        let mut wait = POLL_TIMEOUT;
        let mut produced = false;

        while let Some(picture) = decoder.poll(wait) {
            wait = Duration::ZERO;
            produced = true;

            let pts_us = D::pts_of(&picture);

            lag.record(
                buf.capture_ts_us
                    .saturating_sub(pts_us)
                    .min(u64::from(u32::MAX)) as u32,
            );
            if let Some(age) = age_of(pts_us, offset.load(Ordering::Relaxed)) {
                latency.record(age);
            }
            stage.record(started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
            decoded += 1;

            if let Some(sink) = pictures.as_ref() {
                // Dropped rather than queued: a picture that waits its turn is already too
                // late to be worth showing.
                let _ = sink.try_send(picture);
            }
        }

        if !produced {
            starved += 1;
        }

        let _ = recycle.send(buf);
    }

    DecodeReport {
        decoded,
        starved,
        summary: latency.summarize(),
        stage: stage.summarize(),
        lag: lag.summarize(),
        errors: decoder.take_errors(),
    }
}

/// Starts the decode thread.
///
/// The decoder is created inside the thread rather than handed to it, so that nothing but the
/// thread that uses it ever holds it.
#[cfg(target_os = "macos")]
fn spawn_decoder(
    frames: Receiver<FrameBuf>,
    recycle: Sender<FrameBuf>,
    pictures: Option<PictureSink>,
    offset: Arc<AtomicI64>,
    codec: crate::net::negotiate::Codec,
    say: Say,
) -> thread::JoinHandle<DecodeReport> {
    use crate::decode::videotoolbox::VideoToolboxDecoder;

    thread::spawn(move || {
        decode_until_closed(
            VideoToolboxDecoder::new(codec),
            &frames,
            &recycle,
            pictures,
            &offset,
            &say,
        )
    })
}

/// Starts the decode thread.
///
/// Decodes onto the renderer's device when there is one, so a picture is already on the GPU
/// the window is on and nothing has to be copied between two of them.
#[cfg(target_os = "windows")]
fn spawn_decoder(
    frames: Receiver<FrameBuf>,
    recycle: Sender<FrameBuf>,
    pictures: Option<PictureSink>,
    offset: Arc<AtomicI64>,
    codec: crate::net::negotiate::Codec,
    say: Say,
    gpu: Option<Gpu>,
) -> thread::JoinHandle<DecodeReport> {
    use crate::decode::mediafoundation::MediaFoundationDecoder;

    thread::spawn(move || {
        let decoder = match gpu {
            Some((device, context)) => MediaFoundationDecoder::with_device(codec, device, context),
            None => MediaFoundationDecoder::new(codec),
        };

        decode_until_closed(decoder, &frames, &recycle, pictures, &offset, &say)
    })
}

/// Starts a decode thread on a platform with no decoder yet.
///
/// Drains the channel so the receive thread never blocks handing frames over.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn spawn_decoder(
    frames: Receiver<FrameBuf>,
    recycle: Sender<FrameBuf>,
    pictures: Option<PictureSink>,
    offset: Arc<AtomicI64>,
    codec: crate::net::negotiate::Codec,
    say: Say,
) -> thread::JoinHandle<DecodeReport> {
    let _ = (pictures, offset, codec, say);
    thread::spawn(move || {
        while let Ok(buf) = frames.recv() {
            let _ = recycle.send(buf);
        }
        DecodeReport::default()
    })
}

/// Returns how long ago a host timestamp was, correcting for the clock offset.
///
/// Both sides stamp against the Unix epoch, which only makes them directly comparable on
/// one machine. `offset_us` is how far the host's clock runs ahead, as measured by the
/// synchronisation exchange; [`OFFSET_UNKNOWN`] means no exchange has succeeded yet and
/// the stamps are compared as they are, which is correct when both ends share a clock.
///
/// Returns `None` when the corrected stamp is still in the future. Reporting that as zero
/// would be worse than reporting nothing: it showed a flawless sub-millisecond pipeline on
/// the first cross-machine run, which was entirely an artefact of the subtraction.
pub fn age_of(host_ts_us: u64, offset_us: i64) -> Option<u32> {
    let local_ts = if offset_us == OFFSET_UNKNOWN {
        i128::from(host_ts_us)
    } else {
        i128::from(host_ts_us) - i128::from(offset_us)
    };

    let now = i128::from(now_us());
    (now >= local_ts).then(|| (now - local_ts).min(i128::from(u32::MAX)) as u32)
}

/// Reports a latency summary under the given label.
fn summarize(say: &Say, label: &str, recorder: &mut LatencyRecorder) {
    stage(say, label, recorder.summarize());
}

/// Reports an already computed summary, or a placeholder when there is none.
fn stage(say: &Say, label: &str, summary: Option<LatencySummary>) {
    let Some(summary) = summary else {
        say.note(format!("{label}: no frames measured"));
        return;
    };

    say.note(format!(
        "{label}: n={} min {:.2} p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms",
        summary.count,
        ms(summary.min_us),
        ms(summary.p50_us),
        ms(summary.p95_us),
        ms(summary.p99_us),
        ms(summary.max_us)
    ));
}

/// Converts microseconds to milliseconds for display.
fn ms(micros: u32) -> f64 {
    f64::from(micros) / 1000.0
}

/// Returns whether an error is the read timeout that ends a run normally.
///
/// The kind differs by platform: Unix reports `WouldBlock` and Windows `TimedOut`.
fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// How often a synthetic run fabricates a pointer report.
///
/// One millisecond is the rate a gaming mouse polls at, which is the rate the input path
/// is designed to carry, so a measurement taken at anything slower would flatter it.
const SYNTHETIC_INPUT_INTERVAL: Duration = Duration::from_millis(1);

/// Returns the fabricated pointer motion at a given point in a measurement run.
///
/// A small back and forth rather than a drift, so a long run does not walk the host's
/// pointer off the screen and start clamping against an edge.
///
/// # Examples
///
/// ```ignore
/// let event = synthetic_motion(0);
/// ```
#[must_use]
pub fn synthetic_motion(sequence: u64) -> InputEvent {
    InputEvent::MouseMove {
        dx: if sequence % 2 == 0 { 2 } else { -2 },
        dy: 0,
    }
}

/// Starts a thread that fabricates pointer motion once the host's address is known.
///
/// This exists so the input path can be measured without a window, which is the only way
/// to measure it against a host whose video this client cannot decode — a Windows host has
/// no encoder yet, and waiting for one before testing input would leave the whole return
/// path unexercised on the platform it matters most on.
///
/// The thread runs until the process exits, which is fine for a tool whose sessions last
/// exactly as long as the process.
pub fn spawn_synthetic_input(slot: Arc<OnceLock<InputSender>>) {
    std::thread::spawn(move || {
        let mut sequence = 0u64;

        loop {
            std::thread::sleep(SYNTHETIC_INPUT_INTERVAL);

            let Some(sender) = slot.get() else {
                continue;
            };

            if sender.send(synthetic_motion(sequence)).is_ok() {
                sequence += 1;
            }
        }
    });
}
