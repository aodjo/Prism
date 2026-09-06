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

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use prism_core::clock::now_us;
use prism_core::net::ack::AckTracker;
use prism_core::net::clocksync::ClockSync;
use prism_core::net::handshake::{Identity, KEY_LEN};
use prism_core::net::packet::{
    CLOCK_PING_LEN, Channel, ClockPing, ClockPong, ControlType, CursorPosition,
    FEEDBACK_PACKET_LEN, FecPacket, INPUT_PACKET_LEN, InputEvent, InputPacket, MAX_PACKET_SIZE,
    VideoPacket, channel_of, control_type_of,
};
use prism_core::net::reassemble::{FrameReassembler, PushOutcome};
use prism_core::net::secure::{SecureReceiver, SecureSender};
use prism_core::net::transport::UdpTransport;

use prism_core::stats::{LatencyRecorder, LatencySummary};

/// Where decoded pictures go when the client is showing them.
///
/// The payload type differs by platform because only macOS has a decoder so far; the
/// alias keeps the signatures below identical everywhere.
#[cfg(target_os = "macos")]
pub type PictureSink = SyncSender<prism_core::decode::videotoolbox::DecodedFrame>;

/// Where decoded pictures would go on a platform with no decoder yet.
#[cfg(not(target_os = "macos"))]
pub type PictureSink = SyncSender<()>;

/// How many frames may wait for the decoder before the newest is dropped.
///
/// Two, because a frame that has queued behind another has already missed its moment.
const DECODE_QUEUE_DEPTH: usize = 2;

/// How often to ask the host for a clock synchronisation exchange.
///
/// Frequent at first would be better, but the estimate only improves when a round trip
/// happens to be faster than every one before it, and a quarter second is often enough to
/// find a good one early without adding meaningful traffic.
const PING_INTERVAL: Duration = Duration::from_millis(250);

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

/// What the caller wants from a client session beyond the counters it prints.
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
}

/// How the receiving client should behave.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Address of the host, when it is directly reachable.
    ///
    /// `None` means ask the rendezvous server, which is what a host behind NAT requires.
    pub host: Option<SocketAddr>,
    /// Rendezvous server to find the host through.
    pub rendezvous: Option<SocketAddr>,
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
fn locate(transport: &UdpTransport, config: &ClientConfig) -> io::Result<SocketAddr> {
    if let Some(host) = config.host {
        return Ok(host);
    }

    let Some(server) = config.rendezvous else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "give either --host or --rendezvous so the host can be found",
        ));
    };

    let address = crate::rendezvous::lookup(
        transport,
        server,
        config.peer_key,
        *config.identity.public(),
    )?;
    println!("client: the host is at {address}");

    // Both sides punch. The handshake message this side is about to send repeatedly is its
    // own punch, but the host's router will only pass it once the host has sent outward here
    // — which the server has just told it to do.
    crate::rendezvous::punch(transport, address)?;

    Ok(address)
}

/// Receives packets until the frame budget or the idle timeout is reached.
///
/// # Errors
///
/// Returns an [`io::Error`] if the socket cannot be bound or read, other than the
/// timeout that ends the run normally.
pub fn run(config: ClientConfig, hooks: ClientHooks) -> io::Result<()> {
    let ClientHooks {
        pictures,
        offset,
        input,
        cursor,
    } = hooks;
    let offset = offset.unwrap_or_else(|| Arc::new(AtomicI64::new(OFFSET_UNKNOWN)));
    let transport = UdpTransport::bind("0.0.0.0:0".parse().expect("valid bind address"))?;
    let host = locate(&transport, &config)?;
    transport.connect(host)?;

    println!(
        "client: connecting to {host} ({} frames in flight, decode {})",
        config.in_flight,
        if config.decode { "on" } else { "off" }
    );

    // Nothing is read as a packet until the handshake completes, and it only completes with
    // the host pairing recorded: the first message is encrypted to that key and no other.
    let established = crate::session::dial(&transport, &config.identity, &config.peer_key)?;
    transport.set_read_timeout(Some(config.idle_timeout))?;

    println!(
        "client: session established with {host} ({})",
        crate::identity::to_hex(&established.session.peer_static)
    );

    let (frames_tx, frames_rx) = sync_channel::<FrameBuf>(DECODE_QUEUE_DEPTH);
    let (recycle_tx, recycle_rx) = channel::<FrameBuf>();
    let decoder = config
        .decode
        .then(|| spawn_decoder(frames_rx, recycle_tx, pictures, Arc::clone(&offset)));

    let mut reassembler = FrameReassembler::new(config.in_flight);
    let mut arrival = LatencyRecorder::new(4096);
    let mut recv_buf = [0u8; MAX_PACKET_SIZE];
    let mut malformed = 0u64;
    let mut frames = 0u32;
    let mut behind = 0u32;
    let mut unsynced = 0u32;
    let mut sync = ClockSync::new();
    let mut last_ping = Instant::now() - PING_INTERVAL;
    let mut pings_sent = 0u32;
    let mut pongs_seen = 0u32;
    let mut ping_buf = [0u8; CLOCK_PING_LEN];
    let mut acks = AckTracker::new();
    let mut reports_sent = 0u64;
    let mut reports_failed = 0u64;

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
                    Err(err) => eprintln!("client: ping to {host} failed: {err}"),
                }
            }
        }

        let bytes = match receiver.recv_into(&mut recv_buf) {
            Ok(bytes) => bytes,
            Err(err) if is_timeout(&err) => break,
            Err(err) => return Err(err),
        };

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

        let Some(frame) = reassembler.take_completed() else {
            continue;
        };

        // Acknowledged before anything else is done with the frame. The report is what lets
        // the host reference this frame instead of sending a keyframe when the next one is
        // lost, and every microsecond it waits is a microsecond the encoder spends choosing
        // a reference it did not have to.
        acks.received(frame.frame_id);
        if let Some(report) = acks.report(now_us()) {
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
            report("arrival ", &mut arrival);
        }
        if config.frames.is_some_and(|target| frames >= target) {
            break;
        }
    }

    drop(frames_tx);
    let decode_report = decoder.map(|handle| handle.join().unwrap_or_default());

    println!("\nclient: {frames} frames reassembled, {malformed} packets unparseable");
    match (sync.offset_us(), sync.round_trip_us()) {
        (Some(offset_us), Some(round_trip_us)) => println!(
            "clock: host is {:+.2} ms from this machine, best round trip {:.2} ms ({} samples, {} refused)",
            offset_us as f64 / 1000.0,
            round_trip_us as f64 / 1000.0,
            sync.accepted(),
            sync.rejected()
        ),
        _ => println!(
            "clock: never synchronised ({pings_sent} pings sent, {pongs_seen} answers seen), \
             so cross-machine latency is unmeasurable"
        ),
    }
    if unsynced > 0 {
        println!(
            "client: {unsynced} frames could not be timed — the host stamp was in the future \
             even after correcting for the clock offset"
        );
    }
    report("arrival ", &mut arrival);

    if let Some(decode_report) = decode_report {
        println!(
            "client: {} frames decoded, {behind} dropped at the decoder, {} produced no picture in time",
            decode_report.decoded, decode_report.starved
        );
        print_summary("decode  ", decode_report.stage);
        print_summary("outlag  ", decode_report.lag);
        print_summary("pipeline", decode_report.summary);
        if !decode_report.errors.is_empty() {
            println!(
                "client: decoder reported {} failures: {:?}",
                decode_report.errors.len(),
                decode_report.errors
            );
        }
    }

    let stats = reassembler.stats();
    println!(
        "packets  accepted {}  duplicate {}  stale {}  invalid {}",
        stats.accepted, stats.duplicates, stats.stale, stats.invalid
    );
    println!(
        "frames   completed {}  dropped incomplete {}",
        stats.completed, stats.dropped_incomplete
    );
    if stats.recovered > 0 {
        println!("recovery {} slices rebuilt from parity", stats.recovered);
    }
    println!("feedback sent {reports_sent}  failed to send {reports_failed}");

    Ok(())
}

/// Starts the decode thread.
///
/// The decoder is created inside the thread and never leaves it, which keeps every
/// platform session object on the one thread that owns it.
#[cfg(target_os = "macos")]
fn spawn_decoder(
    frames: Receiver<FrameBuf>,
    recycle: Sender<FrameBuf>,
    pictures: Option<PictureSink>,
    offset: Arc<AtomicI64>,
) -> thread::JoinHandle<DecodeReport> {
    use prism_core::decode::DecodeError;
    use prism_core::decode::videotoolbox::VideoToolboxDecoder;

    /// How long the decode thread waits for a picture before moving on.
    ///
    /// Short on purpose. Blocking here stalls the whole decode thread, so every frame
    /// behind the one being waited for is measured as late and may be dropped. Roughly
    /// two frame intervals is long enough to absorb the decoder's own pipelining and
    /// short enough that a stall cannot cascade.
    const POLL_TIMEOUT: Duration = Duration::from_millis(8);

    thread::spawn(move || {
        let mut decoder = VideoToolboxDecoder::new();
        let mut latency = LatencyRecorder::new(4096);
        let mut stage = LatencyRecorder::new(4096);
        let mut decoded = 0u32;
        let mut starved = 0u32;
        let mut lag = LatencyRecorder::new(4096);

        while let Ok(buf) = frames.recv() {
            let started = std::time::Instant::now();

            match decoder.decode(&buf.data, buf.capture_ts_us) {
                Ok(()) | Err(DecodeError::NoParameterSets) => {}
                Err(err) => eprintln!("client: {err}"),
            }

            if let Some(picture) = decoder.poll(POLL_TIMEOUT) {
                lag.record(
                    buf.capture_ts_us
                        .saturating_sub(picture.pts_us)
                        .min(u64::from(u32::MAX)) as u32,
                );
                if let Some(age) = age_of(picture.pts_us, offset.load(Ordering::Relaxed)) {
                    latency.record(age);
                }
                stage.record(started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
                decoded += 1;

                if let Some(sink) = pictures.as_ref() {
                    // Dropped rather than queued: a picture that waits its turn is
                    // already too late to be worth showing.
                    let _ = sink.try_send(picture);
                }
            } else {
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
    })
}

/// Starts a decode thread on a platform with no decoder yet.
///
/// Drains the channel so the receive thread never blocks handing frames over.
#[cfg(not(target_os = "macos"))]
fn spawn_decoder(
    frames: Receiver<FrameBuf>,
    recycle: Sender<FrameBuf>,
    pictures: Option<PictureSink>,
    offset: Arc<AtomicI64>,
) -> thread::JoinHandle<DecodeReport> {
    let _ = (pictures, offset);
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

/// Prints a latency summary under the given label.
fn report(label: &str, recorder: &mut LatencyRecorder) {
    print_summary(label, recorder.summarize());
}

/// Prints an already computed summary, or a placeholder when there is none.
fn print_summary(label: &str, summary: Option<LatencySummary>) {
    let Some(summary) = summary else {
        println!("{label}: no frames measured");
        return;
    };

    println!(
        "{label}: n={} min {:.2} p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms",
        summary.count,
        ms(summary.min_us),
        ms(summary.p50_us),
        ms(summary.p95_us),
        ms(summary.p99_us),
        ms(summary.max_us)
    );
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
