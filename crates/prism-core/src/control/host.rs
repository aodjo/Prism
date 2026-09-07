//! A host session that can be started, watched, and stopped from outside.
//!
//! The headless command line runs a host to a frame budget and prints a summary at the end.
//! An application cannot: it starts a session when a person clicks, keeps it running for
//! hours, shows what it is doing while it runs, and stops it when they click again. This is
//! that shape.
//!
//! # What crosses the boundary
//!
//! A configuration going in and a [`Snapshot`] coming out, and nothing else. The snapshot is
//! a handful of counters and an address; it is read at a few hertz by whatever is drawing a
//! window, and it is the only thing the Node-API surface is ever given. No frame, no packet,
//! and no buffer reaches this far — the capture, encode and send path runs entirely on the
//! thread this spawns and never returns anything up.
//!
//! # Why the thread starts before there is a peer
//!
//! Opening a session means binding a socket, registering with a rendezvous server, waiting
//! for a client to call, and completing a handshake — minutes of waiting, in the case where a
//! person has not opened the other machine yet. [`HostService::start`] returns as soon as the
//! thread exists, and [`Snapshot::phase`] says where it has got to.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Only the platforms with a capture loop measure elapsed time.
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::time::Instant;

use crate::net::handshake::{Identity, KEY_LEN};
use crate::net::negotiate::{Codecs, H264, HostAbility};
use crate::net::sender::SliceSender;

/// How the host should behave.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Address to listen on. Port zero lets the operating system choose, which is right when
    /// a rendezvous server is being used and wrong when a port has been forwarded by hand.
    pub bind: SocketAddr,
    /// Rendezvous server to register with, or `None` to be reachable only directly.
    pub rendezvous: Option<SocketAddr>,
    /// How long to wait for a client before giving up.
    pub patience: Duration,
    /// Frames per second to capture at.
    pub fps: u32,
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Stop after this many frames, or run until stopped.
    ///
    /// A measurement run wants a budget so the numbers describe a fixed amount of work. A
    /// session a person started wants to run until they end it.
    pub frames: Option<u32>,
    /// Spread packets across the frame interval at this rate, or send at line rate.
    pub pace_bps: Option<u32>,
    /// Let the congestion controller drive the pacing rate and the encoder's bitrate.
    pub adaptive: bool,
    /// Send Reed-Solomon parity sized for this much loss, or none.
    pub parity_loss: Option<f32>,
    /// Whether to inject the client's input events into this machine.
    pub inject_input: bool,
    /// Codecs this machine can encode.
    ///
    /// What the client offers is intersected with this and the best of what remains is used.
    /// A host that names a codec it cannot actually produce agrees to a stream it then fails
    /// to send, so this is a statement about the hardware rather than a wish.
    pub codecs: Codecs,
    /// Send the machine's audio, or `None` to stream picture only.
    ///
    /// The value is the bitrate. A hundred and twenty-eight kilobits is transparent for
    /// desktop audio and is under half a percent of what the picture costs, so this is on by
    /// default and off only when somebody has a reason.
    pub audio_bitrate_bps: Option<u32>,
}

impl Default for HostConfig {
    /// The settings a session started from a window uses when nobody has changed anything.
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:0".parse().expect("a valid address"),
            rendezvous: None,
            patience: Duration::from_secs(600),
            fps: 60,
            bitrate_bps: 24_000_000,
            frames: None,
            pace_bps: None,
            adaptive: false,
            parity_loss: Some(0.05),
            inject_input: true,
            audio_bitrate_bps: Some(128_000),
            codecs: host_codecs(),
        }
    }
}

/// Returns what this machine can encode.
///
/// A statement about the hardware, not a wish: a host that named a codec its encoder refuses
/// would agree a session it then fails to send.
///
/// Public because the command line builds its configuration field by field rather than from
/// the default, and a second answer to "what can this machine encode" is a second answer that
/// drifts.
#[must_use]
pub fn host_codecs() -> Codecs {
    #[cfg(target_os = "macos")]
    {
        // VideoToolbox encodes both in hardware on every Mac this runs on.
        Codecs::none().with(H264).with(crate::net::negotiate::HEVC)
    }

    // NVENC reports HEVC and this project has measured that it offers it, but the encoder
    // here is still configured through the H.264 half of NVENC's union — different offsets
    // and a different structure. Advertising it before that is written would agree a session
    // whose first frame fails.
    #[cfg(not(target_os = "macos"))]
    {
        Codecs::none().with(H264)
    }
}

/// What this machine is able and willing to send, as the negotiation sees it.
///
/// The codec is a real statement about this machine's hardware and is what the agreement turns
/// on. The picture is not, yet: a host sends its screen at the size the screen is, and that
/// size is not known until capture starts — which is after the handshake. So it claims no
/// ceiling of its own and the agreement records the client's, which becomes binding the day
/// there is a scaler to honour it with. Claiming a size here that capture then contradicted
/// would be worse than claiming none.
fn ability(config: &HostConfig) -> HostAbility {
    HostAbility {
        codecs: config.codecs,
        width: u16::MAX,
        height: u16::MAX,
        fps: u16::try_from(config.fps).unwrap_or(u16::MAX),
        bitrate_bps: config.bitrate_bps,
        audio: config.audio_bitrate_bps.is_some(),
    }
}

/// The keys a host session runs under.
#[derive(Debug, Clone)]
pub struct HostKeys {
    /// This machine's long-term key.
    pub identity: Identity,
    /// Every client key pairing has recorded.
    pub allowed: Vec<[u8; KEY_LEN]>,
}

/// How far a host session has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Binding the socket and registering, if there is a server to register with.
    Opening,
    /// Reachable, and waiting for a paired client to call.
    Waiting,
    /// A client is connected and frames are going out.
    Streaming,
    /// Finished, either because it was stopped or because a frame budget ran out.
    Stopped,
    /// Ended on an error, which [`Snapshot::error`] describes.
    Failed,
}

/// What a host session is doing, as of a moment ago.
///
/// Every field is cheap to read. This is what an interface polls, at a few hertz and never
/// faster: a statistics panel that updates more often than a person can read costs frames to
/// produce and tells them nothing.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// How far the session has got.
    pub phase: Phase,
    /// Where the rendezvous server sees this machine, once it has said.
    pub observed: Option<SocketAddr>,
    /// The connected client's public key, once one has connected.
    pub peer: Option<[u8; KEY_LEN]>,
    /// Frames captured, encoded and sent.
    pub frames: u64,
    /// Packets put on the wire, parity included.
    pub packets: u64,
    /// Bytes put on the wire, headers included.
    pub bytes: u64,
    /// What the last second of sending worked out to, in bits per second.
    pub bitrate_bps: u64,
    /// Audio frames captured, encoded and sent.
    pub audio_frames: u64,
    /// What went wrong, when the phase is [`Phase::Failed`].
    pub error: Option<String>,
}

/// The counters the session thread writes and a watcher reads.
///
/// Atomics rather than a lock, because the session thread touches these once per frame on the
/// path whose latency is the entire point of the project, and a watcher reads them a few times
/// a second. A lock here would put a watcher's scheduling in front of a frame.
#[derive(Debug, Default)]
struct Shared {
    phase: AtomicU32,
    frames: AtomicU64,
    packets: AtomicU64,
    bytes: AtomicU64,
    bitrate_bps: AtomicU64,
    /// Shared with the audio thread rather than written through this struct, because that
    /// thread outlives no more than the session but is started from two different places.
    audio_frames: Arc<AtomicU64>,
    /// Written once when each becomes known, so a lock costs nothing measurable.
    observed: Mutex<Option<SocketAddr>>,
    peer: Mutex<Option<[u8; KEY_LEN]>>,
    error: Mutex<Option<String>>,
}

impl Shared {
    /// Records which phase the session has reached.
    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u32, Ordering::Relaxed);
    }

    /// Reads the phase back.
    fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            0 => Phase::Opening,
            1 => Phase::Waiting,
            2 => Phase::Streaming,
            3 => Phase::Stopped,
            _ => Phase::Failed,
        }
    }

    /// Records what went wrong and ends the session.
    fn fail(&self, error: impl std::fmt::Display) {
        if let Ok(mut slot) = self.error.lock() {
            *slot = Some(error.to_string());
        }
        self.set_phase(Phase::Failed);
    }
}

/// A running host session.
///
/// Dropping this stops the session: a window that closed without ending its session would
/// leave a machine streaming its own screen to somebody with nothing on screen to say so.
pub struct HostService {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HostService {
    /// Starts a session and returns as soon as its thread exists.
    ///
    /// Everything slow — binding, registering, waiting for a client — happens on that thread,
    /// so this returns immediately and [`Self::snapshot`] says what is happening.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] only if the thread cannot be spawned. Every other
    /// failure is reported through [`Phase::Failed`], because by then there is a session
    /// object a caller is already holding and watching.
    pub fn start(config: HostConfig, keys: HostKeys) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);

            std::thread::Builder::new()
                .name("prism-host".into())
                .spawn(move || run(config, keys, &shared, &stop))?
        };

        Ok(Self {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// Returns what the session is doing.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            phase: self.shared.phase(),
            observed: self.shared.observed.lock().ok().and_then(|slot| *slot),
            peer: self.shared.peer.lock().ok().and_then(|slot| *slot),
            frames: self.shared.frames.load(Ordering::Relaxed),
            packets: self.shared.packets.load(Ordering::Relaxed),
            bytes: self.shared.bytes.load(Ordering::Relaxed),
            bitrate_bps: self.shared.bitrate_bps.load(Ordering::Relaxed),
            audio_frames: self.shared.audio_frames.load(Ordering::Relaxed),
            error: self
                .shared
                .error
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().cloned()),
        }
    }

    /// Asks the session to end, without waiting for it.
    ///
    /// The thread notices between frames, so this takes effect within a frame interval.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Asks the session to end and waits for it.
    pub fn join(mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for HostService {
    /// Ends the session when the handle goes away.
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl core::fmt::Debug for HostService {
    /// Describes the session by what it is doing.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostService")
            .field("phase", &self.shared.phase())
            .finish_non_exhaustive()
    }
}

/// Runs one session to completion on its own thread.
fn run(config: HostConfig, keys: HostKeys, shared: &Arc<Shared>, stop: &Arc<AtomicBool>) {
    shared.set_phase(Phase::Opening);

    let opened = {
        let shared = Arc::clone(shared);
        let mut waiting = move |observed: Option<SocketAddr>| {
            if let Ok(mut slot) = shared.observed.lock() {
                *slot = observed;
            }
            shared.set_phase(Phase::Waiting);
        };

        connect(&config, &keys, stop, &mut waiting)
    };

    let sender = match opened {
        Ok(opened) => {
            if let Ok(mut slot) = shared.peer.lock() {
                *slot = Some(opened.sender.peer());
            }
            opened.sender
        }
        Err(err) => {
            shared.fail(err);
            return;
        }
    };

    shared.set_phase(Phase::Streaming);

    // Audio runs on its own thread and its own clock. Interleaving it with the video loop
    // would tie a five millisecond cadence to a sixteen millisecond one, and whichever waited
    // for the other would be the one a person noticed.
    let audio = config.audio_bitrate_bps.and_then(|bitrate| {
        spawn_audio(&sender, bitrate, &shared.audio_frames, stop)
            .ok()
            .flatten()
    });

    let outcome = stream(&config, sender, shared, stop);

    if let Some(thread) = audio {
        let _ = thread.join();
    }

    if let Err(err) = outcome {
        shared.fail(err);
        return;
    }

    shared.set_phase(Phase::Stopped);
}

/// What opening a session produced.
#[derive(Debug)]
pub struct Opened {
    /// The session, ready to send.
    pub sender: SliceSender,
    /// Where the rendezvous server sees this machine, when there was one.
    pub observed: Option<SocketAddr>,
    /// Whether the session is going through the server rather than directly.
    ///
    /// Worth surfacing: a relayed session has the server's distance added to every round trip,
    /// and a person wondering why the picture feels heavy deserves to know the answer is the
    /// path and not the encoder.
    pub relayed: bool,
}

/// Binds, becomes reachable, and waits for a paired client to open a session.
///
/// Direct first, then through the relay. Punching either works within a couple of round trips
/// or does not work at all — both routers have already been told to send, and one that will
/// pass a packet has already passed one — so waiting longer before falling back would only
/// lengthen a pause somebody is sitting through.
///
/// `waiting` is called once the machine is reachable and is doing nothing but waiting, which
/// is the moment an interface stops saying "starting" and starts saying "ready".
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if no paired client connects, [`io::ErrorKind::
/// Interrupted`] if `cancelled` is set before one does, and the underlying [`io::Error`] for a
/// socket or server failure.
pub fn connect(
    config: &HostConfig,
    keys: &HostKeys,
    cancelled: &AtomicBool,
    waiting: &mut dyn FnMut(Option<SocketAddr>),
) -> io::Result<Opened> {
    use crate::control::session::{DIRECT_PATIENCE, RELAYED_PATIENCE};
    use crate::net::transport::UdpTransport;

    let transport = UdpTransport::bind(config.bind)?;

    let Some(server) = config.rendezvous else {
        // Reachable only where a client can already address this machine: the same network, a
        // virtual one, or a forwarded port. Nothing to punch and nothing to fall back to.
        waiting(None);
        stopped(cancelled)?;

        return Ok(Opened {
            sender: ready(
                SliceSender::serve_on(
                    transport,
                    &keys.identity,
                    keys.allowed.clone(),
                    ability(config),
                    config.patience,
                )?,
                config,
            )?,
            observed: None,
            relayed: false,
        });
    };

    let observed = crate::control::rendezvous::register(&transport, server, &keys.identity)?;

    // Held for the life of the session. Both the registration and the router's mapping lapse
    // in well under a minute of silence, so a host that went quiet would be unreachable for
    // the next client with nothing appearing to have failed.
    let keepalive_stop = Arc::new(AtomicBool::new(false));
    crate::control::rendezvous::spawn_keepalive(
        &transport,
        server,
        *keys.identity.public(),
        Arc::clone(&keepalive_stop),
    )?;
    std::mem::forget(keepalive_stop);

    waiting(Some(observed));

    let (caller, _) =
        crate::control::rendezvous::await_caller(&transport, server, config.patience)?;
    stopped(cancelled)?;

    let direct = SliceSender::serve_on(
        transport.try_clone()?,
        &keys.identity,
        keys.allowed.clone(),
        ability(config),
        DIRECT_PATIENCE,
    );

    let mut relayed = false;

    let sender = match direct {
        Ok(sender) => sender,
        Err(err) if err.kind() == io::ErrorKind::TimedOut => {
            relayed = true;

            // Both routers give every destination a different mapping, so there is no address
            // at which the two can reach each other. The server carries it instead — at the
            // cost of its bandwidth and its distance added to every round trip.
            let relayed = crate::control::rendezvous::relay(
                &transport,
                server,
                *keys.identity.public(),
                caller,
            )?;

            SliceSender::serve_on(
                transport,
                &keys.identity,
                keys.allowed.clone(),
                ability(config),
                RELAYED_PATIENCE,
            )
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("no session opened through the relay at {}", relayed.address),
                )
            })?
        }
        Err(err) => return Err(err),
    };

    Ok(Opened {
        sender: ready(sender, config)?,
        observed: Some(observed),
        relayed,
    })
}

/// Fails if the session was stopped before it opened.
fn stopped(stop: &AtomicBool) -> io::Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "the session was stopped before a client connected",
        ));
    }

    Ok(())
}

/// Configures a sender whose handshake has completed, and starts its return path.
fn ready(mut sender: SliceSender, config: &HostConfig) -> io::Result<SliceSender> {
    if let Some(bitrate) = config.pace_bps {
        sender.enable_pacing(bitrate, config.adaptive);
    }
    sender.serve_return_path(config.inject_input)?;
    if let Some(loss) = config.parity_loss {
        sender.enable_parity(loss);
    }

    Ok(sender)
}

/// Loudest sample still counted as digital silence.
///
/// Not zero, because a real mix that nobody is listening to still carries dither and the
/// last bit of a fade. Well below anything a person would call quiet.
#[cfg(any(target_os = "windows", target_os = "macos"))]
const SILENCE_FLOOR: f32 = 0.0001;

/// How many frames of unbroken silence pass before the host says so.
///
/// Two hundred a second, so this is ten seconds. Long enough that a stream started before
/// anybody made a sound does not accuse the machine of being broken, short enough to arrive
/// while whoever started the session is still watching.
#[cfg(any(target_os = "windows", target_os = "macos"))]
const SILENCE_PATIENCE_FRAMES: u64 = 2_000;

/// Starts the thread that captures, encodes and sends this machine's audio.
///
/// Public because the command line drives its own send loop rather than going through
/// [`HostService`], and audio that only one of the two could start would be audio only one of
/// them could ever be shown to carry.
///
/// `sent` counts frames put on the wire, for whoever reports on the session. `stop` ends the
/// thread; it is also ended by the source failing, which is reported and not retried.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the socket cannot be duplicated for the audio
/// thread's own sender.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn spawn_audio(
    sender: &SliceSender,
    bitrate_bps: u32,
    sent: &Arc<AtomicU64>,
    stop: &Arc<AtomicBool>,
) -> io::Result<Option<JoinHandle<()>>> {
    use crate::audio::codec::AudioEncoder;
    use crate::audio::{Pulled, SystemAudio};

    let mut sender = sender.audio_sender()?;
    let sent = Arc::clone(sent);
    let stop = Arc::clone(stop);

    let thread = std::thread::Builder::new()
        .name("prism-host-audio".into())
        .spawn(move || {
            let mut capture = match start_system_audio() {
                Ok(capture) => capture,
                Err(reason) => {
                    // Said out loud rather than swallowed. The session has already agreed to
                    // carry audio at this point, so a source that will not open leaves a
                    // client waiting for sound that is never coming — and the only other
                    // symptom is silence, which is also what a quiet machine sounds like.
                    eprintln!("host: no audio will be sent — {reason}");
                    return;
                }
            };
            let Ok(mut encoder) = AudioEncoder::new(bitrate_bps) else {
                return;
            };

            let silence = [0.0f32; crate::audio::FRAME_INTERLEAVED];
            let mut sequence = 0u32;
            let mut heard = false;
            let mut mute_warned = false;

            while !stop.load(Ordering::Relaxed) {
                // A silent machine may deliver nothing at all, not zeroes. Sending silence in
                // its place keeps the stream continuous, which is what stops the client's
                // jitter buffer from having to fill from empty the moment something makes a
                // sound.
                let frame = match capture.poll(Duration::from_millis(20)) {
                    Pulled::Frame(samples) => samples,
                    Pulled::Silence => &silence,
                    Pulled::Stopped => return,
                };

                heard |= frame.iter().any(|sample| sample.abs() > SILENCE_FLOOR);

                // Said once, when a source that opened has produced nothing but digital
                // silence for long enough that it is no longer plausibly a quiet moment.
                // Every way this goes wrong — a refused Screen Recording grant, an output
                // routed somewhere the mix is not tapped — reaches the client as a continuous
                // stream of correctly encoded silence, with nothing anywhere reporting a
                // fault. Without this line the only symptom is that nobody can hear anything.
                if !heard && !mute_warned && u64::from(sequence) > SILENCE_PATIENCE_FRAMES {
                    mute_warned = true;
                    eprintln!(
                        "host: audio is being captured but every sample so far is silence. \
                         If the machine is not simply quiet, check that this binary holds the \
                         Screen Recording grant, and that the output is not routed to a \
                         device whose mix is not tapped."
                    );
                }

                let Ok(packet) = encoder.encode(frame) else {
                    continue;
                };

                if sender
                    .send_audio(sequence, packet, crate::clock::now_us())
                    .is_err()
                {
                    return;
                }

                sequence = sequence.wrapping_add(1);
                sent.store(u64::from(sequence), Ordering::Relaxed);
            }
        })?;

    Ok(Some(thread))
}

/// Opens this machine's system audio.
///
/// The two platforms that have a source differ only here: the thread that captures, encodes
/// and sends is the same code for both. A failure ends the audio thread and nothing else — a
/// stream with no sound is a lesser session, not a failed one — but it is reported, because
/// the alternative symptom is silence and a quiet machine sounds exactly the same.
///
/// # Errors
///
/// Returns what the platform said, already rendered, since there is nothing above this that
/// could act on one kind of failure differently from another.
#[cfg(target_os = "windows")]
fn start_system_audio() -> Result<impl crate::audio::SystemAudio, String> {
    crate::audio::wasapi::LoopbackCapture::start().map_err(|err| err.to_string())
}

/// Opens this machine's system audio.
///
/// See the Windows twin above.
///
/// # Errors
///
/// Returns what ScreenCaptureKit said, most often that Screen Recording is not granted.
#[cfg(target_os = "macos")]
fn start_system_audio() -> Result<impl crate::audio::SystemAudio, String> {
    crate::audio::screencapturekit::SystemAudioCapture::start().map_err(|err| err.to_string())
}

/// Returns no audio thread, on a platform with no system audio capture yet.
///
/// Public for the same reason its twin is: the command line calls it too.
///
/// Linux captures through PipeWire, which is not written yet, so those hosts stream picture
/// without sound. A stream with no audio is a lesser session, not a failed one.
///
/// # Errors
///
/// Never fails. The signature matches the platforms that can fail so the caller has one shape
/// to handle rather than two.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn spawn_audio(
    _sender: &SliceSender,
    _bitrate_bps: u32,
    _sent: &Arc<AtomicU64>,
    _stop: &Arc<AtomicBool>,
) -> io::Result<Option<JoinHandle<()>>> {
    Ok(None)
}

/// Returns the codec the two machines agreed on.
///
/// A session opened without negotiating — which the measurement paths do — falls back to
/// H.264, the one codec every machine here can do.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn agreed_codec(sender: &SliceSender) -> crate::net::negotiate::Codec {
    sender
        .agreed()
        .map_or(crate::net::negotiate::Codec::H264, |agreed| agreed.codec)
}

/// Records the counters a watcher reads, once per frame.
///
/// Only compiled where there is a capture loop to call it. A helper left behind a platform
/// that has no host is dead code on every other one, which the cross-target lint refuses.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn note(shared: &Shared, sender: &SliceSender, frames: u64, started: Instant) {
    shared.frames.store(frames, Ordering::Relaxed);
    shared.packets.store(sender.packets(), Ordering::Relaxed);
    shared.bytes.store(sender.bytes(), Ordering::Relaxed);

    let elapsed = started.elapsed().as_secs_f64();
    if elapsed > 0.0 {
        let rate = sender.bytes() as f64 * 8.0 / elapsed;
        shared.bitrate_bps.store(rate as u64, Ordering::Relaxed);
    }
}

/// Returns whether the session should keep going.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn keep_going(config: &HostConfig, stop: &AtomicBool, frames: u64) -> bool {
    if stop.load(Ordering::Relaxed) {
        return false;
    }

    config
        .frames
        .is_none_or(|budget| frames < u64::from(budget))
}

/// Captures, encodes and sends until the session ends.
#[cfg(target_os = "macos")]
fn stream(
    config: &HostConfig,
    mut sender: SliceSender,
    shared: &Shared,
    stop: &AtomicBool,
) -> Result<(), String> {
    use crate::capture::CaptureConfig;
    use crate::capture::screencapturekit::ScreenCapture;
    use crate::encode::videotoolbox::VideoToolboxEncoder;

    let mut capture = ScreenCapture::start(CaptureConfig {
        fps: config.fps,
        ..CaptureConfig::default()
    })
    .map_err(|err| err.to_string())?;

    let encoder_config = crate::encode::EncoderConfig {
        // Whatever the two machines agreed. A host that encoded something else would send a
        // stream the client cannot decode, and the symptom is a black window with no error.
        codec: agreed_codec(&sender),
        width: capture.width(),
        height: capture.height(),
        fps: config.fps,
        bitrate_bps: config.bitrate_bps,
        max_slice_bytes: config.bitrate_bps / 8 / config.fps.max(1) / 4,
    };

    let mut encoder = VideoToolboxEncoder::new(encoder_config).map_err(|err| err.to_string())?;

    let started = Instant::now();
    let mut frames = 0u64;
    let mut idle = 0u32;

    while keep_going(config, stop, frames) {
        let Some(captured) = capture.poll(Duration::from_millis(500)) else {
            idle += 1;
            if idle > 20 {
                return Err("the compositor stopped delivering frames".into());
            }
            continue;
        };
        idle = 0;

        sender.send_cursor().map_err(|err| err.to_string())?;
        if config.adaptive {
            follow_target(&mut encoder, &sender);
        }

        let capture_ts_us = captured.capture_ts_us;
        encoder
            .encode(captured.pixel_buffer(), capture_ts_us, frames == 0)
            .map_err(|err| err.to_string())?;

        let Some(frame) = encoder.poll(Duration::from_millis(200)) else {
            continue;
        };

        let frame_id = frames as u32;
        sender.note_capture(frame_id, capture_ts_us);

        let last = frame.slices.len().saturating_sub(1);
        for slice_id in 0..frame.slices.len() {
            let data = frame.slice(slice_id).expect("slice index is in range");
            sender
                .send_slice(
                    frame_id,
                    slice_id as u16,
                    data,
                    frame.pts_us,
                    frame.is_idr,
                    slice_id == last,
                )
                .map_err(|err| err.to_string())?;
        }

        frames += 1;
        note(shared, &sender, frames, started);
    }

    Ok(())
}

/// Points the encoder at whatever bitrate the congestion controller currently wants.
///
/// The pacer follows the controller on its own, but pacing is not an actuator: slowing the
/// wire while the encoder keeps producing the same bytes moves the queue into the host instead
/// of removing it. This is the half that changes how much there is to send.
#[cfg(target_os = "macos")]
fn follow_target(
    encoder: &mut crate::encode::videotoolbox::VideoToolboxEncoder,
    sender: &SliceSender,
) {
    // A refusal is ignored rather than reported. A session that keeps running at the old rate
    // is a worse picture than asked for; a session that stops is no picture at all.
    if let Some(target) = sender.target_bps() {
        let _ = encoder.set_bitrate_bps(target);
    }
}

/// Captures, encodes and sends until the session ends.
#[cfg(target_os = "windows")]
fn stream(
    config: &HostConfig,
    mut sender: SliceSender,
    shared: &Shared,
    stop: &AtomicBool,
) -> Result<(), String> {
    use crate::capture::CaptureConfig;
    use crate::capture::wgc::ScreenCapture;
    use crate::encode::nv12::{Bgra2Nv12, Nv12Texture};
    use crate::encode::nvenc::NvencEncoder;
    use windows::core::Interface;

    let mut capture = ScreenCapture::start(CaptureConfig {
        fps: config.fps,
        ..CaptureConfig::default()
    })
    .map_err(|err| err.to_string())?;

    // Even dimensions, because NV12 subsamples chroma by two in both directions.
    let (width, height) = (capture.width() & !1, capture.height() & !1);
    let device = capture.device();

    let target = Nv12Texture::new(device, width, height).map_err(|err| err.to_string())?;
    let converter = Bgra2Nv12::new(device).map_err(|err| err.to_string())?;

    let encoder_config = crate::encode::EncoderConfig {
        codec: agreed_codec(&sender),
        width,
        height,
        fps: config.fps,
        bitrate_bps: config.bitrate_bps,
        // NVENC counts slices rather than bytes, so this is the slice count.
        max_slice_bytes: 4,
    };

    // SAFETY: the device and the texture both outlive the encoder, which is dropped when this
    // function returns.
    let mut encoder =
        unsafe { NvencEncoder::new(device.as_raw(), target.texture().as_raw(), encoder_config) }
            .map_err(|err| err.to_string())?;

    let started = Instant::now();
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(config.fps.max(1)));
    let mut frames = 0u64;
    let mut idle = 0u32;

    while keep_going(config, stop, frames) {
        let Some(captured) = capture.poll(interval) else {
            idle += 1;
            if idle > 200 {
                return Err("the compositor stopped delivering frames".into());
            }
            continue;
        };
        idle = 0;

        let Ok(bgra) = captured.texture() else {
            continue;
        };

        let capture_ts_us = crate::clock::now_us();
        let frame_id = frames as u32;

        sender.note_capture(frame_id, capture_ts_us);
        sender.send_cursor().map_err(|err| err.to_string())?;
        if config.adaptive {
            follow_target(&mut encoder, &sender);
        }

        converter
            .convert(&bgra, &target)
            .map_err(|err| err.to_string())?;
        let frame = encoder
            .encode(capture_ts_us, frames == 0)
            .map_err(|err| err.to_string())?;

        let last = frame.slices.len().saturating_sub(1);
        for index in 0..frame.slices.len() {
            let data = frame.slice(index).expect("slice index is in range");
            sender
                .send_slice(
                    frame_id,
                    index as u16,
                    data,
                    capture_ts_us,
                    frame.is_idr,
                    index == last,
                )
                .map_err(|err| err.to_string())?;
        }

        frames += 1;
        note(shared, &sender, frames, started);
    }

    Ok(())
}

/// Points NVENC at whatever bitrate the congestion controller currently wants.
#[cfg(target_os = "windows")]
fn follow_target(encoder: &mut crate::encode::nvenc::NvencEncoder, sender: &SliceSender) {
    if let Some(target) = sender.target_bps() {
        let _ = encoder.set_bitrate_bps(target);
    }
}

/// Refuses to stream on a platform with no host implementation yet.
///
/// Linux hosts arrive with PipeWire and VAAPI. Until then this fails immediately rather than
/// starting a session that would never produce a frame.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn stream(
    _config: &HostConfig,
    _sender: SliceSender,
    _shared: &Shared,
    _stop: &AtomicBool,
) -> Result<(), String> {
    Err("hosting is not implemented on this platform yet".into())
}
