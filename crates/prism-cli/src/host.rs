//! Host side: produces frames and sends them.
//!
//! Two sources exist. The synthetic one emits bytes shaped like video and exercises the
//! transport alone. The encoded one runs the real hardware encoder, which is what makes
//! the client's latency figure describe the pipeline rather than just the network.

use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

use prism_core::clock::now_us;
use prism_core::net::handshake::KEY_LEN;
use prism_core::net::sender::SliceSender;

// The core owns what a host session is. The command line and the tray application configure
// the same thing and open it the same way; two descriptions of it would drift, which is how
// this file's own copy came to have no relay fallback until a test caught it.
pub use prism_core::control::host::{HostConfig, HostKeys};

/// What the command line adds to a session, on top of what a session is.
///
/// Every field here exists to measure something rather than to stream anything: a synthetic
/// source that produces bytes shaped like video without an encoder, and a seeded loss injector
/// so a recovery run reproduces exactly. None of it belongs in the core's idea of a session,
/// because none of it is something a person would ever ask for.
#[derive(Debug, Clone)]
pub struct HostRun {
    /// The session itself.
    pub session: HostConfig,
    /// Encoded bytes per frame, for the synthetic source.
    pub frame_bytes: usize,
    /// Slices per frame, for the synthetic source.
    pub slices: usize,
    /// Video packets to drop on the way out, in parts per million.
    ///
    /// How a run is made lossy without an operating system traffic shaper, so the recovery
    /// machinery can be judged reproducibly and in a test.
    pub loss_ppm: u32,
    /// Seed for the loss injector, so a failing run repeats exactly.
    pub loss_seed: u64,
}

/// Opens a session, printing what the core learned along the way.
///
/// The connecting itself lives in the core, because the tray application does exactly the same
/// thing and two copies of "find a peer, punch, fall back to the relay" would drift apart —
/// which is how this one came to have no relay fallback at all until a test caught it.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if no paired client connects, and the underlying
/// [`io::Error`] for a socket or server failure.
fn open(config: HostConfig, keys: &HostKeys) -> io::Result<SliceSender> {
    // Never set. The command line runs one session and exits, so there is nothing to cancel;
    // the flag exists for the application, which has a person who can click stop.
    let cancelled = std::sync::atomic::AtomicBool::new(false);

    let mut waiting = |reachable: prism_core::control::host::Reachable| {
        println!("host: listening on {}", reachable.local);
        if let Some(observed) = reachable.observed {
            println!("host: registered, reachable at {observed}");
        }
        println!("host: waiting for a paired client");
    };

    let opened = prism_core::control::host::connect(&config, keys, &cancelled, &mut waiting)?;

    println!(
        "host: session opened by {} {}",
        hex(&opened.sender.peer()),
        if opened.relayed {
            "through the rendezvous relay"
        } else {
            "directly"
        }
    );

    if let Some(agreed) = opened.sender.agreed() {
        println!(
            "host: agreed {:?}, client shows {}, {} fps, {:.1} Mbps, audio {}",
            agreed.codec,
            if agreed.width >= u16::MAX - 1 {
                "any size".to_string()
            } else {
                format!("up to {}x{}", agreed.width, agreed.height)
            },
            agreed.fps,
            f64::from(agreed.bitrate_bps) / 1e6,
            if agreed.audio { "on" } else { "off" },
        );
    }

    // Held until the process exits. Dropping it would end the keepalive the moment a client
    // connected, unregistering a host that is still streaming; and this command runs one
    // session and then exits, so there is no later session for the socket to be freed for.
    std::mem::forget(opened.keepalive);

    Ok(opened.sender)
}

/// Renders a key as hex, for the line that names who connected.
fn hex(key: &[u8; KEY_LEN]) -> String {
    key.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The audio thread, for as long as a run wants one.
///
/// Every host mode needs the same three things — start it if the session agreed to it, count
/// what it sent, stop it at the end — so they are here once rather than in each loop. A mode
/// that forgot the last one would leave a thread reading the machine's sound after the run
/// that asked for it had printed its summary and returned.
struct HostAudio {
    sent: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HostAudio {
    /// Starts capturing and sending this machine's audio, if the session agreed to carry it.
    ///
    /// Refusing to start is not an error: a host with no audio source still has a screen. The
    /// core says so on the way past, so a silent session is never silent about why.
    fn start(sender: &SliceSender, config: &HostConfig) -> Self {
        let sent = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Whether the client agreed to carry audio is checked by the core, alongside every
        // other reason a source might not start.
        let thread = config.audio_bitrate_bps.and_then(|bitrate| {
            prism_core::control::host::spawn_audio(sender, bitrate, &sent, &stop)
                .ok()
                .flatten()
        });

        if thread.is_some() {
            println!(
                "host: also sending this machine's audio at {} kbps",
                config.audio_bitrate_bps.unwrap_or(0) / 1000
            );
        }

        Self { sent, stop, thread }
    }

    /// Stops the thread and says how much sound went out.
    fn finish(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
            let sent = self.sent.load(std::sync::atomic::Ordering::Relaxed);
            println!(
                "audio   : {sent} frames sent ({:.1}s of sound)",
                sent as f64 * f64::from(prism_core::audio::FRAME_US) / 1e6
            );
        }
    }
}

/// Sends `config.frames` synthetic frames and reports what was transmitted.
///
/// Frames are paced to the requested rate by sleeping to each frame's deadline. Packets
/// within a frame go out back to back; spreading them across the interval is the send
/// pacer's job and arrives in M4.
///
/// # Errors
///
/// Returns an [`io::Error`] if the socket cannot be bound, connected, or written to.
///
/// # Panics
///
/// Panics if `run.slices` is zero or `run.frame_bytes` is smaller than
/// `run.slices`, since neither describes a frame an encoder could produce.
pub fn run(run: HostRun, keys: &HostKeys) -> io::Result<()> {
    let config = &run.session;
    // A measurement run always has a budget; the command line supplies one by default. A
    // session with none runs until it is stopped, which on the command line means until the
    // process is.
    let budget = config.frames.unwrap_or(u32::MAX);
    assert!(run.slices > 0, "a frame needs at least one slice");
    assert!(
        run.frame_bytes >= run.slices,
        "every slice needs at least one byte"
    );

    let mut sender = open(run.session.clone(), keys)?;
    // Pacing, the return path and parity are set up by the core when the session opens.
    // Loss injection is not: it exists only to make a measurement reproducible.
    if run.loss_ppm > 0 {
        sender.inject_loss(run.loss_ppm, run.loss_seed);
    }
    let slices = build_slices(run.frame_bytes, run.slices);
    let interval = frame_interval(config.fps);
    let audio = HostAudio::start(&sender, config);

    println!(
        "host: sending {} synthetic frames of {} bytes in {} slices at {} fps",
        budget, run.frame_bytes, run.slices, config.fps
    );

    let start = Instant::now();

    for frame_id in 0..budget {
        pace(start, interval, frame_id);
        // Ahead of the frame's own packets, so eighteen bytes the cursor depends on are
        // not queued behind a whole frame of video.
        sender.send_cursor()?;
        let capture_ts_us = now_us();
        sender.note_capture(frame_id, capture_ts_us);

        // The synthetic source follows the controller the way a real encoder would, by
        // producing less. Sending a shorter prefix of each slice rather than rebuilding it
        // keeps the frame path free of allocation.
        let budget = frame_budget(sender.target_bps(), config.fps, run.frame_bytes);
        // No encoder here, so nothing actually changes about the bytes. The flag is still
        // answered, because this is the path the loss gate runs on and a request that goes
        // unanswered here would look like the client asking into the void.
        let is_idr = frame_id == 0 || sender.take_keyframe_request();

        for (slice_id, slice) in slices.iter().enumerate() {
            let bytes = slice_prefix(slice, slice_id, slices.len(), budget);
            sender.send_slice(
                frame_id,
                bytes,
                capture_ts_us,
                is_idr,
                slice_id + 1 == slices.len(),
            )?;
        }
    }

    audio.finish();
    report(&sender, start.elapsed());
    Ok(())
}

/// Encodes synthetic pictures with the hardware encoder and sends the result.
///
/// The capture timestamp is taken before the frame enters the encoder, so the latency
/// the client measures covers encode, network, reassembly, and decode together.
///
/// # Errors
///
/// Returns an error if the encoder cannot be created, a frame cannot be encoded, or the
/// socket cannot be written to.
#[cfg(target_os = "macos")]
pub fn run_encoded(
    run: HostRun,
    keys: &HostKeys,
    encoder_config: prism_core::encode::EncoderConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = &run.session;
    // A measurement run always has a budget; the command line supplies one by default. A
    // session with none runs until it is stopped, which on the command line means until the
    // process is.
    let budget = config.frames.unwrap_or(u32::MAX);
    use prism_core::encode::videotoolbox::{Nv12Frame, VideoToolboxEncoder};

    let mut sender = open(run.session.clone(), keys)?;
    // Pacing, the return path and parity are set up by the core when the session opens.
    // Loss injection is not: it exists only to make a measurement reproducible.
    if run.loss_ppm > 0 {
        sender.inject_loss(run.loss_ppm, run.loss_seed);
    }
    // Built for the codec the session agreed, not the one the command line guessed. A host
    // that encodes one thing and says another produces a client that decodes nothing and
    // reports no error, because a decoder looking for parameter sets it will never see has
    // nothing to complain about.
    let encoder_config = prism_core::encode::EncoderConfig {
        codec: sender
            .agreed()
            .map_or(prism_core::net::negotiate::Codec::H264, |agreed| {
                agreed.codec
            }),
        ..encoder_config
    };

    let mut encoder = VideoToolboxEncoder::new(encoder_config)?;
    // One picture per frame that may be in flight. Painting into a buffer the encoder has not
    // finished reading does not fail; it produces a stream of frames nobody drew.
    let mut pictures = (0..ENCODE_IN_FLIGHT)
        .map(|_| Nv12Frame::new(encoder_config.width, encoder_config.height))
        .collect::<Result<Vec<_>, _>>()?;
    let interval = frame_interval(config.fps);
    let audio = HostAudio::start(&sender, config);

    println!(
        "host: encoding {} frames at {}x{} {} fps, {} kbps",
        budget,
        encoder_config.width,
        encoder_config.height,
        config.fps,
        encoder_config.bitrate_bps / 1000
    );
    if !encoder.slicing_supported() {
        println!(
            "host: this encoder emits one slice per frame, so transmission cannot start early"
        );
    }

    let start = Instant::now();
    let mut dropped = 0u32;
    let mut emitted = 0u32;

    // The first picture is painted before the loop, so that inside it a frame is always
    // submitted before anything else happens. A frame painted in the same breath as it is
    // submitted holds the previous one — already encoded, already waiting — behind a paint
    // it has nothing to do with, and that shows up as latency in every frame of the run.
    crate::pattern::paint(&mut pictures[0], 0)?;

    for frame_id in 0..budget {
        pace(start, interval, frame_id);
        sender.send_cursor()?;

        follow_target(&mut encoder, &sender);

        // Taken here rather than at the paint, because the paint happened an interval ago and
        // the wait since was this harness pacing itself, not a camera holding a picture. What
        // the number has to describe is everything from the encoder onwards.
        let capture_ts_us = now_us();
        let force_idr = frame_id == 0 || sender.take_keyframe_request();
        encoder.encode(
            pictures[frame_id as usize % ENCODE_IN_FLIGHT].pixel_buffer(),
            capture_ts_us,
            force_idr,
        )?;

        // Whatever finished while this frame was waiting its turn goes out now, immediately
        // after the submission that keeps the encoder busy through it.
        if frame_id + 1 >= ENCODE_IN_FLIGHT as u32 {
            if !drain_one(&mut encoder, &mut sender, emitted)? {
                dropped += 1;
            }
            emitted += 1;
        }

        // The next picture, painted while the encoder works on this one. Its buffer is the
        // one the frame just drained was using, which is why the drain comes first.
        let next = frame_id + 1;
        if next < budget {
            crate::pattern::paint(
                &mut pictures[next as usize % ENCODE_IN_FLIGHT],
                next as usize,
            )?;
        }
    }

    for _ in 1..ENCODE_IN_FLIGHT {
        if !drain_one(&mut encoder, &mut sender, emitted)? {
            dropped += 1;
        }
        emitted += 1;
    }

    audio.finish();
    report(&sender, start.elapsed());
    if dropped > 0 {
        println!("host: {dropped} frames produced nothing within the encode deadline");
    }

    Ok(())
}

/// How many frames may be inside the encoder at once.
///
/// Two, and the second one is worth a great deal. Submitting a frame and waiting for it
/// before painting the next leaves the hardware idle through the paint and the paint idle
/// through the encode, so the achievable rate is the two added together. Measured at 1440p
/// with HEVC on Apple Silicon: one in flight encodes 600 frames in 9.05 s (66 fps), two in
/// 3.05 s (197 fps), and the per-frame latency does not move (p50 6.14 → 6.17 ms).
///
/// It stops at two because the frames beyond that are queued rather than overlapped, and a
/// queue inside the encoder is latency with no name on it: three in flight costs 3.83 ms of
/// p50 for 6 fps, and six costs 15 ms for 59. That is the same trade the one-frame VBV
/// exists to refuse.
#[cfg(target_os = "macos")]
const ENCODE_IN_FLIGHT: usize = 2;

/// Sends one finished frame, and says whether there was one.
///
/// Split out because the loop drains one frame per iteration and then drains what is still
/// inside the encoder after the last submission, and those must send identically — a tail
/// that packetised differently from the body would be a bug visible only in the last frames
/// of a run.
///
/// # Errors
///
/// Returns an error if the socket cannot be written to.
#[cfg(target_os = "macos")]
fn drain_one(
    encoder: &mut prism_core::encode::videotoolbox::VideoToolboxEncoder,
    sender: &mut SliceSender,
    frame_id: u32,
) -> io::Result<bool> {
    // The session forbids frame reordering and emits no B-frames, so the nth frame out is the
    // nth frame in and the caller's counter is the right identifier for it. A poll that times
    // out means the encoder has stopped rather than fallen behind — two hundred milliseconds
    // is thirty times the measured latency — so the count moves on rather than waiting.
    let Some(frame) = encoder.poll(Duration::from_millis(200)) else {
        return Ok(false);
    };

    let capture_ts_us = frame.pts_us;
    let is_idr = frame.is_idr;
    let last = frame.slices.len() - 1;

    sender.note_capture(frame_id, capture_ts_us);

    for slice_id in 0..frame.slices.len() {
        let data = frame.slice(slice_id).expect("slice index is in range");
        sender.send_slice(frame_id, data, capture_ts_us, is_idr, slice_id == last)?;
    }

    Ok(true)
}

/// Captures the screen, encodes it, and sends the result.
///
/// The pipeline is [`prism_core::encode::pump::ScreenPump`], the same one the window
/// application drives. This function is what the command line adds around it: a frame budget
/// so a run describes a fixed amount of work, and a summary at the end.
///
/// # Errors
///
/// Returns an error if capture cannot start — most often because Screen Recording has not
/// been granted — or if the encoder or socket fails.
#[cfg(target_os = "macos")]
pub fn run_captured(
    run: HostRun,
    keys: &HostKeys,
    bitrate_bps: u32,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    use prism_core::encode::pump::{PumpConfig, Pumped, ScreenPump};

    let config = &run.session;
    // A measurement run always has a budget; the command line supplies one by default. A
    // session with none runs until it is stopped, which on the command line means until the
    // process is.
    let budget = config.frames.unwrap_or(u32::MAX);

    // The session opens before the pipeline starts, because what the encoder is built for is
    // what the two machines agreed.
    let mut sender = open(run.session.clone(), keys)?;
    // Pacing, the return path and parity are set up by the core when the session opens.
    // Loss injection is not: it exists only to make a measurement reproducible.
    if run.loss_ppm > 0 {
        sender.inject_loss(run.loss_ppm, run.loss_seed);
    }

    let codec = sender
        .agreed()
        .map_or(prism_core::net::negotiate::Codec::H264, |agreed| {
            agreed.codec
        });
    let mut pump = ScreenPump::start(PumpConfig {
        fps: config.fps,
        bitrate_bps,
        width,
        height,
        codec,
    })?;
    let (width, height) = pump.size();
    let audio = HostAudio::start(&sender, config);

    println!(
        "host: capturing the screen at {width}x{height} {} fps, {} kbps",
        config.fps,
        bitrate_bps / 1000
    );
    if !pump.slicing_supported() {
        println!(
            "host: this encoder emits one slice per frame, so transmission cannot start early"
        );
    }

    let start = Instant::now();
    let mut dropped = 0u32;
    let mut idle = 0u32;

    while pump.sent() < budget {
        match pump.pump(&mut sender, config.adaptive)? {
            Pumped::Idle => {
                idle += 1;
                if idle > 20 {
                    return Err("the compositor stopped delivering frames".into());
                }
            }
            Pumped::Dropped => {
                idle = 0;
                dropped += 1;
            }
            Pumped::PeerGone => {
                println!("host: the client disconnected");
                break;
            }
            Pumped::Filling | Pumped::Sent => idle = 0,
        }
    }

    audio.finish();
    report(&sender, start.elapsed());
    if dropped > 0 {
        println!("host: {dropped} frames produced nothing within the encode deadline");
    }

    Ok(())
}

/// Returns the interval between frames at the requested rate.
fn frame_interval(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))
}

/// Sleeps until the deadline for `frame_id`.
///
/// Pacing against a fixed origin rather than the previous frame keeps the average rate
/// exact, so a late frame does not push every frame after it.
fn pace(start: Instant, interval: Duration, frame_id: u32) {
    let deadline = start + interval * frame_id;
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        sleep(remaining);
    }
}

/// Prints what was transmitted.
fn report(sender: &SliceSender, elapsed: Duration) {
    println!(
        "host: sent {} packets, {:.1} MB in {:.2}s ({:.1} Mbps)",
        sender.packets(),
        sender.bytes() as f64 / 1e6,
        elapsed.as_secs_f64(),
        sender.bytes() as f64 * 8.0 / elapsed.as_secs_f64() / 1e6
    );
    sender.report_pacing();
    if sender.parity_sent() > 0 {
        println!("parity  : {} shards sent", sender.parity_sent());
    }
    sender.report_loss();
    sender.report_feedback();
}

/// Points the encoder at whatever bitrate the congestion controller currently wants.
///
/// The pacer follows the controller on its own, but pacing is not an actuator: slowing the
/// wire while the encoder keeps producing the same bytes moves the queue into the host
/// instead of removing it. This is the half that changes how much there is to send.
///
/// A refusal is reported once and then ignored. A session that keeps running at the old
/// rate is a worse picture than asked for; a session that stops is no picture at all.
#[cfg(target_os = "macos")]
fn follow_target(
    encoder: &mut prism_core::encode::videotoolbox::VideoToolboxEncoder,
    sender: &SliceSender,
) {
    let Some(target) = sender.target_bps() else {
        return;
    };

    if let Err(err) = encoder.set_bitrate_bps(target) {
        eprintln!("host: the encoder would not take {target} bps: {err}");
    }
}

/// Returns how many bytes this frame may carry, given what the controller wants.
///
/// A real encoder is told a bitrate and produces frames that average it. The synthetic
/// source does the same arithmetic directly. Without this the controller has nothing to
/// actuate: pacing alone slows the wire while the source keeps producing at full rate, and
/// the queue simply moves inside the host.
fn frame_budget(target_bps: Option<u32>, fps: u32, configured: usize) -> usize {
    let Some(bps) = target_bps else {
        return configured;
    };

    let wanted = bps as usize / 8 / fps.max(1) as usize;

    // Never more than the source was built to produce, and never so little that a slice
    // ends up empty — an encoder cannot emit a frame of nothing either.
    wanted.clamp(MIN_FRAME_BYTES, configured)
}

/// Returns the prefix of one slice that fits inside a frame budget.
///
/// The budget is split evenly, and the remainder goes to the last slice so the frame's
/// total is exactly the budget rather than a rounding error below it.
fn slice_prefix(slice: &[u8], slice_id: usize, slices: usize, budget: usize) -> &[u8] {
    let base = budget / slices;
    let wanted = if slice_id + 1 == slices {
        base + budget % slices
    } else {
        base
    };

    &slice[..wanted.clamp(1, slice.len())]
}

/// Smallest frame the synthetic source will produce, in bytes.
///
/// One packet per slice at the eight slices the pipeline's encoders use at most. Below this
/// the shape stops resembling a frame and the measurement stops meaning anything.
const MIN_FRAME_BYTES: usize = 8 * 1200;

/// Splits a frame budget into slice bitstreams with a distinguishable byte pattern.
///
/// The pattern differs per slice so a reassembly bug that swaps or repeats a slice shows
/// up as wrong bytes rather than passing unnoticed.
fn build_slices(frame_bytes: usize, slices: usize) -> Vec<Vec<u8>> {
    let base = frame_bytes / slices;
    let remainder = frame_bytes % slices;

    (0..slices)
        .map(|slice_id| {
            let len = base + usize::from(slice_id < remainder);
            (0..len)
                .map(|i| (i.wrapping_mul(31).wrapping_add(slice_id) & 0xff) as u8)
                .collect()
        })
        .collect()
}

/// Encodes and sends frames on Windows, capturing the desktop or painting a pattern.
///
/// This is the Windows half of the vertical slice: Windows.Graphics.Capture hands over a
/// Direct3D texture, a shader converts it to NV12 on the GPU, and NVENC encodes it — none
/// of which ever touches system memory. The synthetic path takes the same route from a
/// pre-painted texture, so the only difference between them is where the picture came from.
///
/// # Errors
///
/// Returns an error if Direct3D, the converter, the encoder or capture cannot be created,
/// or if a frame cannot be encoded or sent.
#[cfg(target_os = "windows")]
pub fn run_windows(
    run: HostRun,
    keys: &HostKeys,
    encoder_config: prism_core::encode::EncoderConfig,
    capture: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = &run.session;
    // A measurement run always has a budget; the command line supplies one by default. A
    // session with none runs until it is stopped, which on the command line means until the
    // process is.
    let budget = config.frames.unwrap_or(u32::MAX);
    use prism_core::encode::nv12::{Bgra2Nv12, Nv12Texture};
    use prism_core::encode::nvenc::NvencEncoder;
    use windows::core::Interface;

    let mut source = WindowsSource::new(&encoder_config, capture)?;
    let (width, height) = source.size();

    // The encoder is created on the device the frames already live on, because a second
    // device would mean copying every frame between them.
    let device = source.device();
    let target = Nv12Texture::new(device, width, height)?;
    let converter = Bgra2Nv12::new(device)?;

    // The session opens before the encoder is built, because what the encoder is built for is
    // what the two machines agreed.
    let mut sender = open(run.session.clone(), keys)?;

    let encoder_config = prism_core::encode::EncoderConfig {
        codec: sender
            .agreed()
            .map_or(prism_core::net::negotiate::Codec::H264, |agreed| {
                agreed.codec
            }),
        width,
        height,
        ..encoder_config
    };

    // SAFETY: the device and the texture both outlive the encoder, which is dropped at the
    // end of this function.
    let mut encoder =
        unsafe { NvencEncoder::new(device.as_raw(), target.texture().as_raw(), encoder_config) }?;
    // Pacing, the return path and parity are set up by the core when the session opens.
    // Loss injection is not: it exists only to make a measurement reproducible.
    if run.loss_ppm > 0 {
        sender.inject_loss(run.loss_ppm, run.loss_seed);
    }

    let audio = HostAudio::start(&sender, config);

    println!(
        "host: {} at {width}x{height}, NVENC at {} kbps",
        if capture { "capturing" } else { "painting" },
        encoder_config.bitrate_bps / 1000
    );

    let start = Instant::now();
    let interval = frame_interval(config.fps);
    let mut sent = 0u32;
    let mut idle = 0u32;

    while sent < budget {
        let Some(bgra) = source.next_frame(interval) else {
            idle += 1;
            if idle > 200 {
                return Err("the source stopped producing frames".into());
            }
            continue;
        };
        idle = 0;

        if !capture {
            pace(start, interval, sent);
        }

        let capture_ts_us = now_us();
        sender.note_capture(sent, capture_ts_us);
        sender.send_cursor()?;
        follow_target_nvenc(&mut encoder, &sender);

        converter.convert(&bgra, &target)?;
        let force_idr = sent == 0 || sender.take_keyframe_request();
        let frame = encoder.encode(capture_ts_us, force_idr)?;

        let last = frame.slices.len().saturating_sub(1);
        for index in 0..frame.slices.len() {
            let data = frame.slice(index).expect("slice index is in range");
            sender.send_slice(sent, data, capture_ts_us, frame.is_idr, index == last)?;
        }

        sent += 1;
    }

    audio.finish();
    report(&sender, start.elapsed());
    sender.report_pacing();
    if config.adaptive {
        println!(
            "encoder : {} rate changes accepted, {} refused, settled at {:.1} Mbps",
            ENCODER_RATE_CHANGES.load(std::sync::atomic::Ordering::Relaxed),
            ENCODER_RATE_REFUSALS.load(std::sync::atomic::Ordering::Relaxed),
            f64::from(encoder.config().bitrate_bps) / 1e6,
        );
    }
    if sender.parity_sent() > 0 {
        println!("parity  : {} shards sent", sender.parity_sent());
    }
    sender.report_loss();
    sender.report_feedback();

    Ok(())
}

/// How many times the encoder actually accepted a new rate.
///
/// Counted rather than assumed: a controller that changes its mind while the encoder
/// ignores it looks identical from the outside to one that is working.
#[cfg(target_os = "windows")]
static ENCODER_RATE_CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times the encoder refused one.
#[cfg(target_os = "windows")]
static ENCODER_RATE_REFUSALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Points NVENC at whatever bitrate the congestion controller currently wants.
#[cfg(target_os = "windows")]
fn follow_target_nvenc(
    encoder: &mut prism_core::encode::nvenc::NvencEncoder,
    sender: &SliceSender,
) {
    let Some(target) = sender.target_bps() else {
        return;
    };

    let before = encoder.config().bitrate_bps;
    match encoder.set_bitrate_bps(target) {
        Ok(()) => {
            let after = encoder.config().bitrate_bps;
            if after != before {
                ENCODER_RATE_CHANGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Err(err) => {
            if ENCODER_RATE_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                eprintln!("host: NVENC would not take {target} bps: {err}");
            }
        }
    }
}

/// Where a Windows host's pictures come from.
///
/// Capture and the synthetic pattern differ only in this; everything downstream is the same
/// path, which is what makes a measurement taken with the pattern say something about the
/// real one.
#[cfg(target_os = "windows")]
enum WindowsSource {
    /// The desktop, through Windows.Graphics.Capture.
    Desktop(Box<prism_core::capture::wgc::ScreenCapture>),
    /// A pre-painted cycle of textures, for a run with no desktop to capture.
    ///
    /// Painted once at startup rather than per frame. A host loop that allocates and uploads
    /// a texture every frame would be measuring that upload as much as the encoder.
    Painted {
        device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
        frames: Vec<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
        next: usize,
        width: u32,
        height: u32,
    },
}

#[cfg(target_os = "windows")]
impl WindowsSource {
    /// Starts capture, or paints a cycle of frames when capture is not wanted.
    fn new(
        config: &prism_core::encode::EncoderConfig,
        capture: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        use prism_core::capture::CaptureConfig;
        use prism_core::capture::wgc::ScreenCapture;

        if capture {
            let capture = ScreenCapture::start(CaptureConfig {
                fps: config.fps,
                ..CaptureConfig::default()
            })?;

            return Ok(Self::Desktop(Box::new(capture)));
        }

        // Even dimensions, because NV12 subsamples chroma by two.
        let (width, height) = (config.width & !1, config.height & !1);
        let device = create_device()?;
        let frames = (0..PAINTED_FRAMES)
            .map(|step| paint_bgra(&device, width, height, step))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self::Painted {
            device,
            frames,
            next: 0,
            width,
            height,
        })
    }

    /// Returns the size of the pictures this source produces.
    fn size(&self) -> (u32, u32) {
        match self {
            // Even, for the same reason as above.
            Self::Desktop(capture) => (capture.width() & !1, capture.height() & !1),
            Self::Painted { width, height, .. } => (*width, *height),
        }
    }

    /// Returns the Direct3D device the pictures live on.
    fn device(&self) -> &windows::Win32::Graphics::Direct3D11::ID3D11Device {
        match self {
            Self::Desktop(capture) => capture.device(),
            Self::Painted { device, .. } => device,
        }
    }

    /// Returns the next picture, waiting up to `timeout` for the compositor.
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
        match self {
            Self::Desktop(capture) => capture.poll(timeout)?.texture().ok(),
            Self::Painted { frames, next, .. } => {
                let texture = frames.get(*next % frames.len())?.clone();
                *next = next.wrapping_add(1);
                Some(texture)
            }
        }
    }
}

/// How many distinct pictures the painted source cycles through.
///
/// Enough that consecutive frames differ, so the encoder has real work to do, and few enough
/// that they are all painted before the run starts.
#[cfg(target_os = "windows")]
const PAINTED_FRAMES: u32 = 16;

/// Creates a Direct3D device for the painted source.
#[cfg(target_os = "windows")]
fn create_device()
-> Result<windows::Win32::Graphics::Direct3D11::ID3D11Device, Box<dyn std::error::Error>> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    };

    let mut device: Option<ID3D11Device> = None;

    // SAFETY: every pointer argument is null or a live local, and the output is read only
    // when the call reports success.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }?;

    device.ok_or_else(|| "Direct3D reported success but produced no device".into())
}

/// Paints one BGRA texture with a pattern that differs per step.
#[cfg(target_os = "windows")]
fn paint_bgra(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    width: u32,
    height: u32,
    step: u32,
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, Box<dyn std::error::Error>> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    let pixels: Vec<u8> = (0..width * height)
        .flat_map(|index| {
            let x = (index % width) as u8;
            let y = (index / width) as u8;
            [
                x.wrapping_add((step as u8).wrapping_mul(3)),
                y,
                (step as u8).wrapping_mul(7),
                255,
            ]
        })
        .collect();

    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };

    let mut texture = None;

    // SAFETY: the description and the pixel buffer agree on the size, the buffer outlives
    // the call, and the output is a live local.
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }?;

    texture.ok_or_else(|| "Direct3D reported success but produced no texture".into())
}
