//! Playing the host's sound on the client.
//!
//! SDL asks for audio when its device needs it, on a thread of its own, at whatever interval
//! the device runs at. Everything below happens inside that request: take the next frame from
//! the jitter buffer, decode it, hand over the samples. If the frame did not arrive, invent one
//! rather than leave a hole.
//!
//! # No lock on this path
//!
//! The jitter buffer, the decoder and the receiving end of the packet channel all live inside
//! the callback and are touched by nothing else. The network thread only sends. That is not an
//! optimisation for its own sake: the audio callback runs on a deadline the operating system
//! sets, and a callback that waits on a lock held by a thread the scheduler has parked produces
//! exactly the dropout the jitter buffer exists to prevent.
//!
//! # Why the queue is kept shallow
//!
//! SDL keeps a queue of its own, and anything sitting in it is delay on top of the jitter
//! buffer's. So the callback puts in what was asked for and no more: the depth this session
//! runs at is the buffer's decision, made from measurements, not the sum of two independent
//! guesses.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use prism_core::audio::codec::AudioDecoder;
use prism_core::audio::jitter::{JitterBuffer, Pull};
use prism_core::audio::{CHANNELS, FRAME_INTERLEAVED, SAMPLE_RATE};
use sdl3::audio::{AudioCallback, AudioFormat, AudioSpec, AudioStream, AudioStreamWithCallback};

/// How many frames may be waiting in the channel between the network and the audio thread.
///
/// Deliberately small. This channel is not a buffer — the jitter buffer is — and a deep one
/// here would hide arrival timing from the very code that measures it. Sixty-four frames is a
/// third of a second, far more than the audio thread can fall behind without something else
/// being badly wrong.
const CHANNEL_DEPTH: usize = 64;

/// One arriving audio frame, on its way to the audio thread.
struct Arrival {
    sequence: u32,
    arrived_us: u64,
    payload: Vec<u8>,
}

/// What playback has been doing, for the statistics line.
#[derive(Debug, Default)]
pub struct PlaybackStats {
    /// Frames handed to the device.
    pub played: AtomicU64,
    /// Frames the decoder had to invent because they never arrived.
    pub concealed: AtomicU64,
    /// Times the device asked for audio and there was none ready.
    pub starved: AtomicU64,
    /// Frames dropped between the network and the audio thread.
    ///
    /// Non-zero means the audio thread is not keeping up, which is a different problem from a
    /// lossy network and wants a different answer.
    pub dropped: AtomicU64,
    /// How deep the jitter buffer is currently running, in frames.
    pub depth: AtomicU64,
}

/// Hands arriving frames to the audio thread.
///
/// Cloneable and cheap to hold. Sending never blocks: if the audio thread has fallen far
/// enough behind to fill the channel, the newest frame is dropped and counted, because
/// blocking the receive loop to wait for audio would stall video too.
#[derive(Clone)]
pub struct AudioSink {
    frames: SyncSender<Arrival>,
    stats: Arc<PlaybackStats>,
}

impl AudioSink {
    /// Passes one frame to playback.
    pub fn push(&self, sequence: u32, payload: &[u8], arrived_us: u64) {
        let arrival = Arrival {
            sequence,
            arrived_us,
            payload: payload.to_vec(),
        };

        match self.frames.try_send(arrival) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Returns the counters, for whatever is reporting them.
    #[must_use]
    pub fn stats(&self) -> &Arc<PlaybackStats> {
        &self.stats
    }
}

/// Lets a session hand its audio to this device.
///
/// Spelled out rather than imported, because this file already has a `Playback` of its own —
/// the SDL callback — and the two are unrelated.
impl prism_core::control::client::Playback for AudioSink {
    fn push(&self, sequence: u32, payload: &[u8], arrived_us: u64) {
        Self::push(self, sequence, payload, arrived_us);
    }
}

impl core::fmt::Debug for AudioSink {
    /// Describes the sink by what playback has done.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioSink")
            .field("played", &self.stats.played.load(Ordering::Relaxed))
            .field("concealed", &self.stats.concealed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Everything the audio callback owns.
struct Playback {
    frames: Receiver<Arrival>,
    buffer: JitterBuffer,
    decoder: AudioDecoder,
    stats: Arc<PlaybackStats>,
    /// Samples decoded but not yet handed over, because the device asked for less than a frame.
    pending: Vec<f32>,
}

impl AudioCallback<f32> for Playback {
    /// Fills the device's request from the jitter buffer.
    ///
    /// `requested` is in samples, both channels together. It is whatever the device wants this
    /// time round and is not a multiple of a frame, so anything decoded beyond it is kept.
    fn callback(&mut self, stream: &mut AudioStream, requested: i32) {
        // Everything that arrived since the last request. Draining here rather than on a timer
        // is what keeps the buffer's own arrival measurements honest — it sees the times the
        // packets actually landed, which the sender recorded.
        while let Ok(arrival) = self.frames.try_recv() {
            self.buffer
                .push(arrival.sequence, &arrival.payload, arrival.arrived_us);
        }

        self.stats
            .depth
            .store(u64::from(self.buffer.depth()), Ordering::Relaxed);

        let wanted = requested.max(0) as usize;

        while self.pending.len() < wanted {
            let decoded = match self.buffer.pull() {
                Pull::Frame { payload, .. } => self.decoder.decode(payload),
                Pull::Missing { .. } => {
                    self.stats.concealed.fetch_add(1, Ordering::Relaxed);
                    self.decoder.conceal()
                }
                Pull::Empty => {
                    // Nothing ready. Silence rather than a stall: the device gets what it asked
                    // for on time, and the buffer fills in the meantime.
                    self.stats.starved.fetch_add(1, Ordering::Relaxed);
                    self.pending.resize(wanted, 0.0);
                    break;
                }
            };

            match decoded {
                Ok(samples) => {
                    self.pending.extend_from_slice(samples);
                    self.stats.played.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    // A frame the codec would not read. One frame of silence is the same cost
                    // as a lost one and there is nothing else to do with it.
                    self.pending
                        .resize(self.pending.len() + FRAME_INTERLEAVED, 0.0);
                }
            }
        }

        let handing = wanted.min(self.pending.len());
        let _ = stream.put_data_f32(&self.pending[..handing]);
        self.pending.drain(..handing);
    }
}

/// Playback, held open for as long as the session lasts.
///
/// Dropping this closes the device. The stream has to be kept rather than let go: SDL stops
/// asking for audio the moment nothing owns it.
pub struct AudioPlayback {
    _stream: AudioStreamWithCallback<Playback>,
}

impl core::fmt::Debug for AudioPlayback {
    /// Describes playback without reaching into SDL.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioPlayback").finish_non_exhaustive()
    }
}

/// Opens the default output device and starts playing whatever is sent to the sink.
///
/// # Errors
///
/// Returns whatever SDL said if there is no output device or it refuses the format. A machine
/// with no sound is a machine that should still stream picture, so the caller is expected to
/// carry on rather than end the session.
pub fn start(
    audio: &sdl3::AudioSubsystem,
) -> Result<(AudioPlayback, AudioSink), Box<dyn std::error::Error>> {
    let (sender, receiver) = sync_channel(CHANNEL_DEPTH);
    let stats = Arc::new(PlaybackStats::default());

    let playback = Playback {
        frames: receiver,
        buffer: JitterBuffer::new(),
        decoder: AudioDecoder::new()?,
        stats: Arc::clone(&stats),
        pending: Vec::with_capacity(FRAME_INTERLEAVED * 4),
    };

    let spec = AudioSpec {
        freq: Some(SAMPLE_RATE as i32),
        channels: Some(CHANNELS as i32),
        format: Some(AudioFormat::f32_sys()),
    };

    let stream = audio.open_playback_stream(&spec, playback)?;
    stream.resume()?;

    Ok((
        AudioPlayback { _stream: stream },
        AudioSink {
            frames: sender,
            stats,
        },
    ))
}
