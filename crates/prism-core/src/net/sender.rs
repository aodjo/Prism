//! The host's send path: slices in, sealed and paced packets out.
//!
//! Shared by every host mode so the synthetic source and the real encoder put identical
//! packets on the network — the only difference between them is where the bytes came from.
//!
//! Everything that shapes traffic converges here: packetisation, forward error correction,
//! send pacing, congestion control, and the seal. There is one place a byte leaves the socket
//! and it is [`SliceSender::emit`], which is what makes it possible to say with confidence
//! that nothing is sent unsealed, unpaced, or uncounted.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::now_us;
use crate::input::{Injector, PlatformInjector};
use crate::net::ack::{is_newer, missing_in_history};
use crate::net::cc::{CongestionConfig, CongestionController, DelaySample};
use crate::net::fec::{FecCodec, ParityBlock, max_data_shards_for, parity_shards_for};
use crate::net::handshake::{Identity, KEY_LEN, PeerPolicy};
use crate::net::loss::LossInjector;
use crate::net::negotiate::{Accept, HostAbility};
use crate::net::packet::{
    AudioPacket, CLOCK_PONG_LEN, Channel, ClockPing, ClockPong, CursorPosition,
    FEEDBACK_WANTS_KEYFRAME, FLAG_IDR, FLAG_LAST_OF_FRAME, FecPacket, FeedbackPacket, InputEvent,
    InputPacket, MAX_PACKET_SIZE, MAX_PLAINTEXT_SIZE, MAX_VIDEO_PAYLOAD, channel_of,
};
use crate::net::packetize::SlicePacketizer;
use crate::net::transfer::{Files, Landed};
use crate::net::seal::Opener;
use crate::net::secure::SecureSender;
use crate::net::sendpace::{PacerConfig, SPREAD_PERCENT, SendPacer};
use crate::net::transport::UdpTransport;
use crate::stats::LatencyRecorder;

/// How many recent frames the host remembers the capture time of.
///
/// Comfortably more than the thirty-two a feedback report can describe, so a report about
/// the oldest frame in its own window still finds its timestamp.
const CAPTURE_HISTORY: usize = 64;

/// The most bytes one wire slice may carry, for a given parity setting.
///
/// A free function as well as a method because it is the whole of the decision and there is
/// no way to stand a real session in front of a test.
///
/// # Examples
///
/// ```
/// # use prism_core::net::sender::max_slice_bytes;
/// assert_eq!(max_slice_bytes(None), usize::MAX);
/// assert!(max_slice_bytes(Some(0.05)) < usize::MAX);
/// ```
#[must_use]
pub fn max_slice_bytes(parity_loss: Option<f32>) -> usize {
    parity_loss.map_or(usize::MAX, |loss| {
        max_data_shards_for(loss) * MAX_VIDEO_PAYLOAD
    })
}

/// How long the file thread waits when a transfer has nothing to send.
///
/// Long enough that an idle session is an idle thread, short enough that a person who has just
/// chosen a file does not notice the wait before it starts moving.
const FILE_IDLE: Duration = Duration::from_millis(20);

/// The gap between two file packets, which is what holds a transfer under the picture.
///
/// Roughly eight megabits a second at a full packet. A file is never the thing somebody is
/// waiting on in a session about a screen, so it takes what is left rather than competing:
/// this is a tenth of what the video is configured for and it cannot grow.
const FILE_PACE: Duration = Duration::from_micros(1_100);

/// Starts the thread that moves a file while the session runs.
///
/// Paced rather than driven by the return path, and on a handle of its own so that a chunk
/// waiting to go never sits behind a frame that is already being written.
pub fn spawn_files(files: Arc<Mutex<Files>>, mut socket: SecureSender) {
    std::thread::spawn(move || {
        let mut buf = [0u8; MAX_PLAINTEXT_SIZE];

        loop {
            let written = match files.lock() {
                Ok(mut files) => files.step(&mut buf).unwrap_or(0),
                // Another thread died holding it, which means this session's file state is
                // gone. Ending quietly: there is nothing left to move.
                Err(_) => return,
            };

            if written == 0 {
                std::thread::sleep(FILE_IDLE);

                continue;
            }

            if socket.send(&buf[..written]).is_err() {
                return;
            }

            std::thread::sleep(FILE_PACE);
        }
    });
}

/// The shortest gap between two keyframes the host will produce because it was asked to.
///
/// Two frames at sixty a second, four at a hundred and twenty. Short, because the usual
/// reason to space keyframes out does not apply here: the encoder runs CBR against a
/// one-frame VBV, so a keyframe is not a larger frame, it is a worse-looking one of the
/// same size. Measured over five seconds at 720p60 and two percent loss with no parity,
/// answering every request and answering none faster than this both produce 6.3 Mbps —
/// and 72% against 70% of frames decoded, where a quarter-second gap produces 28%.
///
/// So the limit is not there to protect the bitrate. A client only asks when it cannot
/// decode, and one keyframe answers every outstanding complaint at once, which already
/// holds the rate down on its own — forty-two keyframes for fifty-six broken frames in
/// that run. What this bounds is a peer that ignores that and sets the bit on every
/// report: without a limit it could make the host encode every single frame intra and
/// watch the picture fall apart, from one bit on the return path.
pub const KEYFRAME_REQUEST_INTERVAL_US: u64 = 33_000;

/// Frame identifier reserved to mean "this slot holds nothing usable".
///
/// A real frame with this identifier simply yields no delay sample, once every four
/// billion frames — over a year at a hundred and twenty a second.
const NO_FRAME: u32 = u32::MAX;

/// When one recent frame was captured.
///
/// Two atomics rather than a lock. The send loop writes and the return-path thread reads,
/// and that thread also injects input, which the plan requires to be the lowest-latency
/// path in the system — it must never wait on the sender. The identifier is invalidated
/// before the timestamp changes and restored after, so a reader that sees the same
/// identifier either side of the timestamp knows the pair belongs together.
#[derive(Debug)]
struct CaptureSlot {
    frame_id: AtomicU32,
    capture_ts_us: AtomicU64,
}

impl Default for CaptureSlot {
    /// Starts empty rather than claiming to hold frame zero.
    fn default() -> Self {
        Self {
            frame_id: AtomicU32::new(NO_FRAME),
            capture_ts_us: AtomicU64::new(0),
        }
    }
}

/// What the host has heard back from the client, and what it needs to interpret it.
///
/// Written by the return-path thread and read by whoever prints the summary, so plain
/// atomics rather than a lock: the writer must never block on the path that also injects
/// input, and a reader that sees a slightly stale count is reporting, not deciding.
#[derive(Debug)]
struct ReturnPath {
    reports: AtomicU64,
    newest_acked: AtomicU32,
    missing_in_last: AtomicU32,
    ever: AtomicBool,
    /// Capture times of recent frames, so a report can be turned into a one-way delay.
    captures: [CaptureSlot; CAPTURE_HISTORY],
    /// The bitrate the congestion controller currently wants, in bits per second.
    ///
    /// Zero until a controller is running, which is how the send loop knows to leave the
    /// pacer at whatever rate it was configured with.
    target_bps: AtomicU32,
    /// How many times the controller has changed its mind.
    rate_changes: AtomicU64,
    /// Whether the client has said it can no longer decode what it is being sent.
    keyframe_wanted: AtomicBool,
    /// When the last request was answered, so the answers can be spaced out.
    ///
    /// Zero means none has been, which is how the first request is answered immediately.
    keyframe_granted_us: AtomicU64,
    /// How many reports asked for a keyframe, including the ones the interval refused.
    keyframe_requests: AtomicU64,
    /// How many keyframes the host produced because it was asked to.
    keyframes_forced: AtomicU64,
}

impl Default for ReturnPath {
    /// Starts with an empty capture history and no rate opinion.
    ///
    /// Written out rather than derived because a fixed-size array of a type without a
    /// `Copy` default has no derived `Default` beyond thirty-two elements.
    fn default() -> Self {
        Self {
            reports: AtomicU64::new(0),
            newest_acked: AtomicU32::new(0),
            missing_in_last: AtomicU32::new(0),
            ever: AtomicBool::new(false),
            captures: core::array::from_fn(|_| CaptureSlot::default()),
            target_bps: AtomicU32::new(0),
            rate_changes: AtomicU64::new(0),
            keyframe_wanted: AtomicBool::new(false),
            keyframe_granted_us: AtomicU64::new(0),
            keyframe_requests: AtomicU64::new(0),
            keyframes_forced: AtomicU64::new(0),
        }
    }
}

impl ReturnPath {
    /// Records when a frame was captured, evicting whatever the slot held before.
    fn remember_capture(&self, frame_id: u32, capture_ts_us: u64) {
        let slot = &self.captures[frame_id as usize % CAPTURE_HISTORY];

        slot.frame_id.store(NO_FRAME, Ordering::Release);
        slot.capture_ts_us.store(capture_ts_us, Ordering::Release);
        slot.frame_id.store(frame_id, Ordering::Release);
    }

    /// Returns when a frame was captured, if it is still remembered.
    ///
    /// The identifier is read either side of the timestamp: if it changed, the sender was
    /// mid-write and the pair cannot be trusted, so the sample is skipped rather than used.
    /// Skipping costs nothing — another report follows in a frame's time.
    fn capture_of(&self, frame_id: u32) -> Option<u64> {
        let slot = &self.captures[frame_id as usize % CAPTURE_HISTORY];

        let before = slot.frame_id.load(Ordering::Acquire);
        let capture_ts_us = slot.capture_ts_us.load(Ordering::Acquire);
        let after = slot.frame_id.load(Ordering::Acquire);

        (before == frame_id && after == frame_id).then_some(capture_ts_us)
    }

    /// Takes the client's outstanding keyframe request, if the interval allows answering it.
    ///
    /// Consuming rather than reading: the request describes a moment, and answering it is
    /// what makes it stale. A client that still cannot decode says so again in a frame's
    /// time, which is how a lost keyframe turns into a second one rather than a stall.
    fn take_keyframe_request(&self, now_us: u64) -> bool {
        if !self.keyframe_wanted.load(Ordering::Relaxed) {
            return false;
        }

        let granted = self.keyframe_granted_us.load(Ordering::Relaxed);
        if granted != 0 && now_us.saturating_sub(granted) < KEYFRAME_REQUEST_INTERVAL_US {
            return false;
        }

        self.keyframe_wanted.store(false, Ordering::Relaxed);
        self.keyframe_granted_us.store(now_us, Ordering::Relaxed);
        self.keyframes_forced.fetch_add(1, Ordering::Relaxed);

        true
    }
}

/// Owns the socket and the reusable send buffer for one session.
#[derive(Debug)]
pub struct SliceSender {
    sender: SecureSender,
    /// The key for the client's half of the session, until the return path takes it.
    ///
    /// Held rather than used here because opening is the receiving thread's job, and there
    /// is exactly one of those. Two openers on one direction would each keep their own replay
    /// window and each reject what the other had already accepted.
    opener: Option<Opener>,
    /// The client's static key, as the handshake proved it.
    peer: [u8; KEY_LEN],
    /// Where that client is.
    peer_address: std::net::SocketAddr,
    /// What the two agreed the stream would be.
    agreed: Option<Accept>,
    buffer: [u8; MAX_PACKET_SIZE],
    packets: u64,
    bytes: u64,
    feedback: Arc<ReturnPath>,
    /// Drops a fraction of video packets on the way out, when a run is testing recovery.
    ///
    /// Applies to parity as well as picture data. Exempting parity would make recovery look
    /// better than it is on a real path, where the repair is as losable as the thing it
    /// repairs. Control, feedback and input are left alone: losing those tests something
    /// else entirely.
    loss: Option<LossInjector>,
    /// The loss estimate parity is sized against, or `None` when parity is switched off.
    parity_loss: Option<f32>,
    codec: FecCodec,
    parity: ParityBlock,
    parity_sent: u64,
    /// The frame the wire slice numbering currently belongs to.
    ///
    /// Numbering restarts at zero for each frame, which is what the client expects when it
    /// concatenates a frame's slices in order.
    slicing_frame: Option<u32>,
    /// The number the next wire slice will carry.
    next_slice_id: u16,
    /// Spreads a frame's packets across the interval instead of blasting them at line rate.
    pacer: Option<SendPacer>,
    /// Whether the congestion controller is allowed to drive the pacer's rate.
    adaptive: bool,
    /// How many audio frames have gone out, counted separately because they are a different
    /// kind of traffic: unpaced, unprotected, and two hundred a second regardless of the video.
    audio_frames: u64,
}

impl SliceSender {
    /// Waits on an already bound socket for a paired client to open a session.
    ///
    /// The socket is passed in rather than bound here because a host behind NAT has to
    /// register with the rendezvous server from the very socket the session will use: a
    /// router's mapping belongs to one local port, and an address published from a different
    /// port leads nowhere.
    ///
    /// Returns only once the session is sealed. There is no path through this that produces a
    /// sender able to put a packet on the wire in the clear.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::TimedOut`] if no paired client connects within `patience`,
    /// [`io::ErrorKind::Interrupted`] if `cancelled` is set while it waits, and the underlying
    /// [`io::Error`] for a socket failure.
    pub fn serve_on(
        transport: UdpTransport,
        identity: &Identity,
        allowed: Vec<[u8; KEY_LEN]>,
        ability: HostAbility,
        patience: std::time::Duration,
        cancelled: &AtomicBool,
    ) -> io::Result<Self> {
        let (established, peer, listener) = crate::control::session::serve(
            &transport,
            identity.clone(),
            PeerPolicy::Paired(allowed),
            ability,
            patience,
            cancelled,
        )?;
        transport.set_read_timeout(None)?;

        Ok(Self {
            sender: SecureSender::new(transport, established.session.sealer),
            opener: Some(established.session.opener),
            peer: established.session.peer_static,
            peer_address: peer,
            agreed: listener.agreed(),
            buffer: [0; MAX_PACKET_SIZE],
            packets: 0,
            bytes: 0,
            feedback: Arc::new(ReturnPath::default()),
            loss: None,
            parity_loss: None,
            codec: FecCodec::new(),
            parity: ParityBlock::new(),
            parity_sent: 0,
            slicing_frame: None,
            next_slice_id: 0,
            pacer: None,
            adaptive: false,
            audio_frames: 0,
        })
    }

    /// Starts dropping the given fraction of outgoing video packets.
    ///
    /// `per_million` is parts per million, so the five percent M4 is judged at is 50_000.
    /// The seed makes a failing run reproducible, which is the whole reason this is here
    /// rather than an operating system traffic shaper.
    pub fn inject_loss(&mut self, per_million: u32, seed: u64) {
        self.loss = Some(LossInjector::new(per_million, seed));
        println!(
            "host: dropping {:.2}% of outgoing video packets (seed {seed})",
            f64::from(per_million) / 10_000.0
        );
    }

    /// Turns on Reed-Solomon parity, sized for the given loss estimate.
    ///
    /// The estimate is a fraction, so five percent is 0.05. It sets how much of the
    /// bitrate is spent on repair; the codec clamps it to the plan's ten to twenty percent
    /// band, so a wild estimate cannot spend everything or nothing.
    pub fn enable_parity(&mut self, loss: f32) {
        self.parity_loss = Some(loss);
        println!(
            "host: parity sized for {:.1}% loss ({} data shards per block at most)",
            f64::from(loss) * 100.0,
            max_data_shards_for(loss)
        );
    }

    /// Returns how many parity packets have been sent.
    #[must_use]
    pub fn parity_sent(&self) -> u64 {
        self.parity_sent
    }

    /// Spreads outgoing packets over time instead of blasting them at line rate.
    ///
    /// `adaptive` lets the congestion controller drive the rate from feedback; without it
    /// the pacer stays at `bitrate_bps` for the whole session, which is what a measurement
    /// run wants when the controller is the thing under test.
    pub fn enable_pacing(&mut self, bitrate_bps: u32, adaptive: bool) {
        self.pacer = Some(SendPacer::new(PacerConfig {
            bitrate_bps,
            ..PacerConfig::default()
        }));
        self.adaptive = adaptive;

        println!(
            "host: pacing at {:.1} Mbps over {}% of each interval{}",
            f64::from(bitrate_bps) / 1e6,
            SPREAD_PERCENT,
            if adaptive {
                ", rate driven by feedback"
            } else {
                ", fixed rate"
            }
        );
    }

    /// Returns the bitrate the controller currently wants, or `None` if none is running.
    ///
    /// The pacer already follows this on its own. It is exposed because pacing alone is not
    /// an actuator: slowing the wire while the source keeps producing the same bytes just
    /// moves the queue inside the host. Whatever generates the frames has to follow it too.
    #[must_use]
    pub fn target_bps(&self) -> Option<u32> {
        match self.feedback.target_bps.load(Ordering::Relaxed) {
            0 => None,
            bps => Some(bps),
        }
    }

    /// Whether the next frame should be encoded as a keyframe.
    ///
    /// Every frame here is a reference — there are no B-frames and the keyframe interval is
    /// effectively infinite, because a periodic keyframe is a periodic latency spike. The
    /// cost of that choice is that one frame the client never completes makes every later
    /// frame undecodable, forever, and the client cannot fix it alone. This is the way out:
    /// the client says it is stuck, and the next frame starts the stream over.
    ///
    /// Consuming, and rate limited to one per [`KEYFRAME_REQUEST_INTERVAL_US`]. Call it once
    /// per frame, immediately before encoding, and pass the answer as the force flag.
    #[must_use]
    pub fn take_keyframe_request(&self) -> bool {
        self.feedback.take_keyframe_request(now_us())
    }

    /// How many keyframes the client asked for, and how many it was given.
    #[must_use]
    pub fn keyframe_counts(&self) -> (u64, u64) {
        (
            self.feedback.keyframe_requests.load(Ordering::Relaxed),
            self.feedback.keyframes_forced.load(Ordering::Relaxed),
        )
    }

    /// Records when a frame was captured, so feedback about it can be timed.
    ///
    /// Called once per frame by the send loop. Without it the controller has nothing to
    /// subtract a client timestamp from and takes no delay samples at all.
    pub fn note_capture(&self, frame_id: u32, capture_ts_us: u64) {
        self.feedback.remember_capture(frame_id, capture_ts_us);
    }

    /// Prints what the pacer and the controller did, if either was running.
    pub fn report_pacing(&self) {
        let Some(pacer) = self.pacer.as_ref() else {
            return;
        };

        println!(
            "pacing  : {} packets, {} waits totalling {:.2}s, final rate {:.1} Mbps",
            pacer.packets(),
            pacer.waits(),
            pacer.total_wait().as_secs_f64(),
            f64::from(pacer.bitrate_bps()) / 1e6,
        );

        if self.adaptive {
            println!(
                "control : {} rate changes, controller settled at {:.1} Mbps",
                self.feedback.rate_changes.load(Ordering::Relaxed),
                f64::from(self.feedback.target_bps.load(Ordering::Relaxed)) / 1e6,
            );
        }
    }

    /// Cuts one slice into packets and sends them.
    ///
    /// `last` marks the slice that ends the frame, which is how the receiver knows the
    /// frame is complete rather than still arriving.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if a packet cannot be sent.
    ///
    /// # Panics
    ///
    /// Panics if `data` is empty or larger than a slice can be, neither of which a real
    /// encoder produces.
    pub fn send_slice(
        &mut self,
        frame_id: u32,
        data: &[u8],
        capture_ts_us: u64,
        idr: bool,
        last: bool,
    ) -> io::Result<()> {
        // Wire slices are numbered here rather than by the caller, because one slice from the
        // encoder may become several on the wire and only this side knows when. Callers hand
        // over the encoder's slices in order and say which is the frame's last; the numbering
        // that reaches the client is this function's business.
        if self.slicing_frame != Some(frame_id) {
            self.slicing_frame = Some(frame_id);
            self.next_slice_id = 0;
        }

        let limit = self.max_slice_bytes();
        let chunks = data.len().div_ceil(limit).max(1);

        for (index, chunk) in data.chunks(limit).enumerate() {
            let mut flags = 0;
            if idr {
                flags |= FLAG_IDR;
            }
            // Only the very last packet of the very last piece ends the frame. Setting it on
            // each piece would have the client assembling a frame it has most of.
            if last && index + 1 == chunks {
                flags |= FLAG_LAST_OF_FRAME;
            }

            let slice_id = self.next_slice_id;
            self.next_slice_id = self.next_slice_id.wrapping_add(1);

            let packetizer = SlicePacketizer::new(frame_id, slice_id, flags, capture_ts_us, chunk)
                .expect("an encoded slice is always packetisable");

            for packet in packetizer {
                let len = packet
                    .encode_into(&mut self.buffer)
                    .expect("packet fits the send buffer");
                self.emit(len)?;
            }

            self.send_parity(frame_id, slice_id, chunk, capture_ts_us)?;
        }

        Ok(())
    }

    /// The most bytes one wire slice may carry.
    ///
    /// A Reed-Solomon block holds 255 shards in all, so a slice longer than the data half of
    /// that cannot be protected by one block. The encoder does not know or care: Apple
    /// Silicon ignores the slice size limit entirely and hands over whole frames, and a
    /// keyframe at a real bitrate is several hundred packets. Left alone, exactly the frame
    /// that must not be lost is the one frame sent unprotected.
    ///
    /// So a long slice becomes several wire slices, each with its own parity block. The
    /// client already rebuilds a frame by concatenating its slices in order, which is why
    /// this is invisible on the far side and costs nothing but a few extra headers.
    ///
    /// Without parity there is nothing to fit inside, and splitting would only add headers.
    fn max_slice_bytes(&self) -> usize {
        max_slice_bytes(self.parity_loss)
    }

    /// Paces, optionally drops, and sends the first `len` bytes of the send buffer.
    ///
    /// Every video and parity packet leaves through here, so the pacing and the loss
    /// injection are each written once. Control, feedback and input deliberately do not:
    /// the plan gives input priority over everything, and eighteen bytes of cursor position
    /// cannot congest anything.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the packet cannot be sent.
    fn emit(&mut self, len: usize) -> io::Result<()> {
        if let Some(pacer) = self.pacer.as_mut() {
            if self.adaptive {
                let target = self.feedback.target_bps.load(Ordering::Relaxed);
                if target > 0 && target != pacer.bitrate_bps() {
                    pacer.set_bitrate_bps(target);
                }
            }

            let wait = pacer.wait_before(len, now_us().saturating_mul(1_000));
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }

        self.packets += 1;
        self.bytes += len as u64;

        // Counted before the drop, so the packet total describes what the session produced
        // and the loss figure is measured against it.
        if self.loss.as_mut().is_some_and(LossInjector::should_drop) {
            return Ok(());
        }

        self.sender.send(&self.buffer[..len])?;

        Ok(())
    }

    /// Generates parity for a slice and sends it, if parity is switched on.
    ///
    /// Sent after the slice's own packets rather than before. Parity is only useful once
    /// something is missing, so putting it ahead of the data would delay every packet it
    /// protects for no gain.
    ///
    /// Every slice reaching here fits one block, because [`Self::max_slice_bytes`] is what
    /// decided how long it could be.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if a parity packet cannot be sent.
    fn send_parity(
        &mut self,
        frame_id: u32,
        slice_id: u16,
        data: &[u8],
        capture_ts_us: u64,
    ) -> io::Result<()> {
        let Some(loss) = self.parity_loss else {
            return Ok(());
        };

        // Guaranteed by `max_slice_bytes`, which is what decides how long a wire slice may
        // be. Kept as a guard rather than an assertion because the alternative to skipping is
        // a block with one parity shard for two hundred data ones — protection that looks
        // enabled and repairs nothing.
        let data_count = data.len().div_ceil(MAX_VIDEO_PAYLOAD);
        if data_count > max_data_shards_for(loss) {
            return Ok(());
        }

        let parity_count = parity_shards_for(data_count, loss);
        if self
            .codec
            .encode(data, MAX_VIDEO_PAYLOAD, parity_count, &mut self.parity)
            .is_err()
        {
            return Ok(());
        }

        let tail = data.len() - (data_count - 1) * MAX_VIDEO_PAYLOAD;

        for index in 0..parity_count {
            let Some(shard) = self.parity.shard(index) else {
                continue;
            };

            let packet = FecPacket {
                frame_id,
                slice_id,
                data_count: data_count as u8,
                parity_count: parity_count as u8,
                shard_index: index as u8,
                tail_len: tail as u16,
                capture_ts_us,
                payload: shard,
            };

            let Ok(len) = packet.encode_into(&mut self.buffer) else {
                continue;
            };

            self.emit(len)?;
            self.parity_sent += 1;
        }

        Ok(())
    }

    /// Samples the host pointer and tells the client where it is.
    ///
    /// Called once per frame, which is a rate the cursor's smoothness does not depend on:
    /// the client draws the cursor from its own motion the instant that motion happens, and
    /// uses this only to correct for everything it could not know about — the host's own
    /// user, a window warping the pointer, an edge it clamped against.
    ///
    /// Returns whether there was a pointer to report. A host with no desktop has none, and
    /// that is not an error worth stopping a session over.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the message cannot be sent.
    pub fn send_cursor(&mut self) -> io::Result<bool> {
        let Some(sample) = crate::input::pointer() else {
            return Ok(false);
        };

        let cursor = CursorPosition {
            sample_ts_us: now_us(),
            x: sample.x,
            y: sample.y,
            screen_width: sample.screen_width,
            screen_height: sample.screen_height,
        };

        let len = cursor
            .encode_into(&mut self.buffer)
            .expect("a clamped sample always encodes");
        self.sender.send(&self.buffer[..len])?;
        self.packets += 1;
        self.bytes += len as u64;

        Ok(true)
    }

    /// Sends one encoded audio frame.
    ///
    /// Not paced and not protected by parity. Audio is a fraction of a percent of the link and
    /// its frames are five milliseconds apart, so spreading them would delay sound to smooth a
    /// burst that does not exist; and a lost frame is concealed by the decoder, which costs
    /// nothing per frame where parity would cost bandwidth on every one.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the packet cannot be sent, and
    /// [`io::ErrorKind::InvalidInput`] if the frame is larger than one packet carries — which
    /// for Opus at any sane rate it never is.
    pub fn send_audio(
        &mut self,
        sequence: u32,
        payload: &[u8],
        capture_ts_us: u64,
    ) -> io::Result<()> {
        let packet = AudioPacket {
            sequence,
            capture_ts_us,
            payload,
        };

        let len = packet
            .encode_into(&mut self.buffer)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

        self.sender.send(&self.buffer[..len])?;
        self.packets += 1;
        self.bytes += len as u64;
        self.audio_frames += 1;

        Ok(())
    }

    /// Builds a sender for the audio thread.
    ///
    /// Its own socket handle and its own buffer, so audio and video never wait on each other,
    /// and a *shared* nonce counter, because both are the same direction under the same key.
    /// Two independent counters would both start at zero and reuse every nonce, which leaks
    /// the authentication key rather than merely weakening the cipher.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the socket cannot be duplicated.
    pub fn audio_sender(&self) -> io::Result<AudioSender> {
        Ok(AudioSender {
            sender: self.sender.split()?,
            buffer: Box::new([0; MAX_PACKET_SIZE]),
            frames: 0,
        })
    }

    /// Returns how many audio frames have been sent.
    #[must_use]
    pub fn audio_frames(&self) -> u64 {
        self.audio_frames
    }

    /// Returns the connected client's public key, as the handshake proved it.
    #[must_use]
    pub fn peer(&self) -> [u8; KEY_LEN] {
        self.peer
    }

    /// Returns where the connected client is.
    #[must_use]
    pub fn peer_address(&self) -> std::net::SocketAddr {
        self.peer_address
    }

    /// Returns what the two machines agreed the stream would be.
    ///
    /// `None` on a session opened without negotiating, which the measurement paths do.
    #[must_use]
    pub fn agreed(&self) -> Option<Accept> {
        self.agreed
    }

    /// Returns how many packets have been sent.
    #[must_use]
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Returns how many bytes have been sent, including packet headers.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Starts the thread that handles everything coming back from the client.
    ///
    /// The reply has to come from the socket the video is already flowing out of, so the
    /// client can pair it with the session; a second socket would answer from a different
    /// port. The thread runs until the process exits, which is fine for a tool whose
    /// sessions last exactly as long as the process.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the socket cannot be duplicated, and
    /// [`io::ErrorKind::AlreadyExists`] if a return path is already running.
    pub fn serve_return_path(
        &mut self,
        inject_input: bool,
        files: Option<Arc<Mutex<Files>>>,
    ) -> io::Result<()> {
        let opener = self.opener.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the return path is already running",
            )
        })?;

        let mut receiver = self.sender.receiver(opener)?;
        let mut replies = self.sender.split()?;

        // Its own thread and its own handle on the socket, because a file moves on a clock of
        // its own: this thread wakes when the client says something, and a transfer that only
        // advanced then would run at the rate of the return path rather than at the rate the
        // wire has room for.
        if let Some(files) = files.as_ref() {
            spawn_files(Arc::clone(files), self.sender.split()?);
        }

        let carrier = files;
        let feedback = Arc::clone(&self.feedback);
        let adaptive = self.adaptive;
        let start_bps = self.pacer.as_ref().map_or(0, SendPacer::bitrate_bps);

        std::thread::spawn(move || {
            let mut control = adaptive.then(|| {
                let mut config = CongestionConfig::default();
                if start_bps > 0 {
                    config.start_bps = start_bps.clamp(config.min_bps, config.max_bps);
                }
                CongestionController::new(config)
            });
            let mut last_sampled: Option<u32> = None;

            let mut recv_buf = [0u8; MAX_PACKET_SIZE];
            let mut send_buf = [0u8; CLOCK_PONG_LEN];
            let mut input = HostInput::new(inject_input);
            let mut latency = LatencyRecorder::new(4096);
            let mut injected = 0u64;

            loop {
                let bytes = match receiver.recv_into(&mut recv_buf) {
                    Ok(bytes) => bytes,
                    // A connected UDP socket reports the far machine having nothing listening
                    // as a refused connection, which is what an ordinary disconnection looks
                    // like from here. Ending quietly, because a line of error text after every
                    // normal session is a line nobody reads by the time it matters.
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        return;
                    }
                    Err(err) => {
                        eprintln!("host: return path recv failed: {err} ({:?})", err.kind());
                        return;
                    }
                };
                let arrived_us = now_us();

                match channel_of(bytes) {
                    Ok(Channel::Control) => {
                        let Ok(ping) = ClockPing::decode(bytes) else {
                            continue;
                        };

                        // The socket is connected to the client, so the answer goes back
                        // with `send`. `send_to` fails outright here — a connected UDP
                        // socket rejects it with EISCONN on macOS and the BSDs.
                        let pong = ClockPong {
                            t1_us: ping.t1_us,
                            t2_us: arrived_us,
                            t3_us: now_us(),
                        };
                        if pong.encode_into(&mut send_buf).is_ok() {
                            let _ = replies.send(&send_buf);
                        }
                    }
                    Ok(Channel::Input) => {
                        let Ok(packet) = InputPacket::decode(bytes) else {
                            continue;
                        };

                        // The client already converted its timestamp into this machine's
                        // clock, so the difference is the wire time and nothing else.
                        latency.record(
                            arrived_us
                                .saturating_sub(packet.origin_ts_us)
                                .min(u64::from(u32::MAX)) as u32,
                        );
                        injected += 1;

                        input.inject(packet.event);

                        if injected == 20 {
                            input.report_once();
                        }

                        if injected % 500 == 0 {
                            if let Some(summary) = latency.summarize() {
                                println!(
                                    "input  : {injected} events, wire p50 {:.2} p99 {:.2} ms",
                                    f64::from(summary.p50_us) / 1000.0,
                                    f64::from(summary.p99_us) / 1000.0,
                                );
                            }
                        }
                    }
                    Ok(Channel::Feedback) => {
                        let Ok(report) = FeedbackPacket::decode(bytes) else {
                            continue;
                        };

                        // Only the newest report matters. Each one repeats the whole recent
                        // history, so an older one arriving late says nothing new, and
                        // acting on it would undo what a newer one already established.
                        let previous = feedback.newest_acked.load(Ordering::Relaxed);
                        let first = !feedback.ever.swap(true, Ordering::Relaxed);

                        if first || is_newer(report.last_frame_id, previous) {
                            feedback
                                .newest_acked
                                .store(report.last_frame_id, Ordering::Relaxed);
                            feedback
                                .missing_in_last
                                .store(missing_in_history(report.recv_bitmap), Ordering::Relaxed);

                            // Only recorded from a report that is current. A stale one
                            // asking for a keyframe describes a gap the client has since
                            // been carried past, and a client that still cannot decode
                            // sets the bit again on the very next frame.
                            if report.flags & FEEDBACK_WANTS_KEYFRAME != 0 {
                                feedback.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                                feedback.keyframe_wanted.store(true, Ordering::Relaxed);
                            }
                        }

                        feedback.reports.fetch_add(1, Ordering::Relaxed);

                        // A delay sample is only taken when the report describes a frame
                        // newer than the last one sampled. A report repeating a frame
                        // already measured carries a newer client timestamp against the
                        // same capture time, which reads as delay climbing forever and
                        // would walk the rate to the floor.
                        let Some(cc) = control.as_mut() else {
                            continue;
                        };
                        if last_sampled.is_some_and(|last| !is_newer(report.last_frame_id, last)) {
                            continue;
                        }
                        let Some(capture_ts_us) = feedback.capture_of(report.last_frame_id) else {
                            continue;
                        };
                        last_sampled = Some(report.last_frame_id);

                        let before = cc.target_bps();
                        let after = cc.observe(&DelaySample {
                            // Signed on purpose: the two clocks are unsynchronised here, so
                            // this is routinely negative. The controller only ever takes
                            // differences, in which a constant offset cancels exactly.
                            one_way_delay_us: report.client_ts_us as i64 - capture_ts_us as i64,
                            observed_at_us: arrived_us,
                            frame_loss: missing_in_history(report.recv_bitmap) as f32 / 32.0,
                        });

                        feedback.target_bps.store(after, Ordering::Relaxed);
                        if after != before {
                            feedback.rate_changes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // Offers, answers, chunks and reports. Whatever the state machine wants
                    // to say back goes out from the file thread, which is holding the same
                    // lock this one is about to let go of.
                    Ok(Channel::File) => {
                        let Some(files) = carrier.as_ref() else {
                            continue;
                        };

                        if let Ok(mut files) = files.lock() {
                            match files.arrived(bytes) {
                                Ok(Some(Landed::Received { name, path })) => {
                                    println!("files  : {name} arrived in {}", path.display());
                                }
                                Ok(_) => {}
                                Err(err) => eprintln!("files  : {err}"),
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }

    /// Prints what the injector actually dropped, if one was running.
    ///
    /// The achieved rate rather than the configured one, because on a short run they differ
    /// and a verdict quoting the wrong one would be measuring something it did not do.
    pub fn report_loss(&self) {
        let Some(loss) = self.loss.as_ref() else {
            return;
        };

        let (considered, dropped) = loss.tally();
        println!(
            "loss    : dropped {dropped} of {considered} video packets ({:.2}% achieved)",
            f64::from(loss.achieved_per_million()) / 10_000.0
        );
    }

    /// Prints what the client has been reporting back, and says so loudly if it has not.
    ///
    /// Silence here is the failure mode this project has already been bitten by. The host
    /// socket is connected to its peer, so anything arriving from a different address is
    /// discarded by the kernel with no error and no log — video keeps flowing and only the
    /// return path is dead. That looked like "input is broken" last time and would look
    /// like "the controller does nothing" this time.
    pub fn report_feedback(&self) {
        let reports = self.feedback.reports.load(Ordering::Relaxed);

        if reports == 0 {
            println!(
                "feedback: none received — the client never reported a frame. If video is \
                 flowing, the return path is being dropped: this socket is connected to \
                 --peer, so anything from a different source address is discarded silently. \
                 Check that --peer is the address the client routes out of."
            );
            return;
        }

        println!(
            "feedback: {reports} reports, newest frame acknowledged {}, {} of the last 32 \
             missing",
            self.feedback.newest_acked.load(Ordering::Relaxed),
            self.feedback.missing_in_last.load(Ordering::Relaxed),
        );

        let (asked, forced) = self.keyframe_counts();
        if asked > 0 {
            println!(
                "recovery: {asked} reports asked for a keyframe, {forced} sent \
                 (at most one per {} ms)",
                KEYFRAME_REQUEST_INTERVAL_US / 1000,
            );
        }
    }
}

/// Holds the host's injector and everything that has to be said about it exactly once.
///
/// Injection is optional — a session that only watches is still useful — so a host that
/// cannot control the machine carries no injector rather than refusing to start. The
/// reasons it might fail repeat on every event if left alone: a missing Accessibility
/// grant on macOS, an elevated foreground window on Windows. Each is worth one line.
struct HostInput {
    injector: Option<PlatformInjector>,
    complained: bool,
    confirmed: bool,
}

impl HostInput {
    /// Creates the platform injector, saying why if it cannot.
    fn new(enabled: bool) -> Self {
        let injector = if enabled {
            match PlatformInjector::new() {
                Ok(injector) => Some(injector),
                Err(err) => {
                    eprintln!("host: input will not be injected: {err}");
                    None
                }
            }
        } else {
            None
        };

        Self {
            injector,
            complained: false,
            confirmed: false,
        }
    }

    /// Injects one event, complaining at most once about a kind of failure that repeats.
    fn inject(&mut self, event: InputEvent) {
        let Some(injector) = self.injector.as_mut() else {
            return;
        };

        if let Err(err) = injector.inject(event) {
            if !self.complained {
                eprintln!("host: {err}");
                self.complained = true;
            }
        }
    }

    /// Says once whether injected events are actually reaching the system.
    ///
    /// Worth saying because both platforms can accept an event and do nothing with it:
    /// macOS posts into the void when the process is untrusted, and Windows refuses
    /// outright when a more privileged window holds the foreground.
    fn report_once(&mut self) {
        if self.confirmed || self.injector.is_none() {
            return;
        }
        self.confirmed = true;

        if self
            .injector
            .as_ref()
            .is_some_and(PlatformInjector::injection_is_landing)
        {
            println!("input  : injection confirmed, events are reaching the system");
        } else {
            println!(
                "input  : events are being posted but do not appear to land — check the \
                 permission to control this machine, unless the client is on this same \
                 machine, where its captured pointer holds the cursor still and this check \
                 cannot tell the two apart"
            );
        }
    }
}

/// Sends audio, on the audio thread's own handle.
///
/// Separate from [`SliceSender`] because audio is a different kind of traffic on the same
/// session: two hundred tiny frames a second, unpaced and unprotected, on a thread that must
/// not wait behind a video frame being packetised.
pub struct AudioSender {
    sender: SecureSender,
    buffer: Box<[u8; MAX_PACKET_SIZE]>,
    frames: u64,
}

impl AudioSender {
    /// Sends one encoded audio frame.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the packet cannot be sent, and
    /// [`io::ErrorKind::InvalidInput`] if the frame is larger than one packet carries — which
    /// for Opus at any sane rate it never is.
    pub fn send_audio(
        &mut self,
        sequence: u32,
        payload: &[u8],
        capture_ts_us: u64,
    ) -> io::Result<()> {
        let packet = AudioPacket {
            sequence,
            capture_ts_us,
            payload,
        };

        let len = packet
            .encode_into(self.buffer.as_mut_slice())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

        self.sender.send(&self.buffer[..len])?;
        self.frames += 1;

        Ok(())
    }

    /// Returns how many frames have gone out.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

impl core::fmt::Debug for AudioSender {
    /// Describes the sender by what it has sent.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioSender")
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{KEYFRAME_REQUEST_INTERVAL_US, ReturnPath};
    use std::sync::atomic::Ordering;

    /// A moment far enough from zero that the interval arithmetic is not measuring startup.
    const NOW: u64 = 10_000_000;

    #[test]
    fn nothing_is_forced_until_a_client_asks() {
        let feedback = ReturnPath::default();

        assert!(!feedback.take_keyframe_request(NOW));
    }

    #[test]
    fn the_first_request_is_answered_at_once() {
        // The whole point is that the client is stuck and stays stuck until this arrives.
        // Making it wait out an interval it never started would add a quarter of a second
        // of black to every recovery.
        let feedback = ReturnPath::default();
        feedback.keyframe_wanted.store(true, Ordering::Relaxed);

        assert!(feedback.take_keyframe_request(NOW));
    }

    #[test]
    fn one_request_produces_one_keyframe() {
        let feedback = ReturnPath::default();
        feedback.keyframe_wanted.store(true, Ordering::Relaxed);

        assert!(feedback.take_keyframe_request(NOW));
        assert!(
            !feedback.take_keyframe_request(NOW + KEYFRAME_REQUEST_INTERVAL_US * 10),
            "a request already answered was answered again"
        );
    }

    #[test]
    fn a_client_asking_on_every_frame_does_not_get_a_keyframe_on_every_frame() {
        // The bound on what one bit of the return path can make the host do. A peer that
        // sets it on every report — through a bug or on purpose — would otherwise have
        // every frame encoded intra, and the picture would fall apart with nothing on the
        // host saying why.
        let feedback = ReturnPath::default();

        feedback.keyframe_wanted.store(true, Ordering::Relaxed);
        assert!(feedback.take_keyframe_request(NOW));

        // A frame at a hundred and twenty a second, which is the fastest anything asks.
        for frame in 1..KEYFRAME_REQUEST_INTERVAL_US / 8_000 {
            feedback.keyframe_wanted.store(true, Ordering::Relaxed);
            assert!(
                !feedback.take_keyframe_request(NOW + frame * 8_000),
                "a second keyframe went out {} us after the first",
                frame * 8_000
            );
        }

        feedback.keyframe_wanted.store(true, Ordering::Relaxed);
        assert!(
            feedback.take_keyframe_request(NOW + KEYFRAME_REQUEST_INTERVAL_US),
            "a client still stuck after the interval was never answered again"
        );
        assert_eq!(feedback.keyframes_forced.load(Ordering::Relaxed), 2);
    }
}
