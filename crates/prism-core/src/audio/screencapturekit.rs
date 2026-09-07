//! Capturing whatever a Mac is playing.
//!
//! ScreenCaptureKit will hand over the system audio mix alongside the screen, which is the
//! same thing WASAPI's loopback mode gives on Windows: everything the person at the other end
//! would have heard, from every application, without asking any of them to cooperate. There is
//! no other supported way to do it — CoreAudio has no loopback device, and the usual answer of
//! installing a virtual one is not something a remote desktop can ask of the machine it is
//! streaming.
//!
//! # This is a second stream, not the capture's
//!
//! It runs its own [`SCStream`] rather than adding an audio output to the screen capture's.
//! Audio has to work when the host is not capturing the screen at all — the synthetic source
//! streams without a display — and tying the two together would make a five millisecond
//! cadence wait on a sixteen millisecond one, which is the arrangement the plan puts audio on
//! its own thread specifically to avoid. The cost is a video stream nobody reads, held to two
//! pixels at one frame a second.
//!
//! # Nothing is excluded from the mix
//!
//! `excludesCurrentProcessAudio` is deliberately off. It reads as harmless — a host plays no
//! sound of its own, so there should be nothing of its own to exclude — but "current process"
//! here means the *responsible* process, the application at the top of the launching chain,
//! not this executable. Everything started from the same place is excluded with it.
//!
//! Measured: with the flag on, a tone played by another program launched from the same shell
//! is captured as samples of exactly zero. Turning it off, and changing nothing else, captures
//! it at -28.8 dBFS and encodes 82-byte Opus frames. A remote desktop exists to send what the
//! machine is playing, and this flag silently decides that some of that does not count.
//!
//! The cost is a feedback loop when host and client run on one machine, since the client's
//! playback is then part of the mix the host captures. The flag never prevented that anyway —
//! the client is a separate process — and the answer to it is not to run both ends on one
//! machine with sound on.

#![cfg(target_os = "macos")]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};

use crate::audio::{CHANNELS, FRAME_INTERLEAVED, Pulled, SAMPLE_RATE, SystemAudio};
use crate::capture::CaptureError;
use crate::capture::screencapturekit::{shareable_content, start_capture, stop_capture};

/// Width and height of the video nobody reads.
///
/// A stream needs a picture size even when only its audio is wanted. Two pixels rather than
/// zero because a zero-sized configuration is refused, and rather than the display's own size
/// because that would have the compositor scaling and delivering a full screen of pixels for
/// a stream that never looks at them.
const UNUSED_PICTURE: usize = 2;

/// How many samples the shared buffer holds before the oldest are dropped.
///
/// A quarter of a second of stereo. Reached only if nothing polls, which means the audio
/// thread has stopped; keeping more would turn that into growing latency instead of a
/// dropout, and a dropout is the honest symptom.
const BACKLOG_LIMIT: usize = SAMPLE_RATE as usize / 4 * CHANNELS;

/// Most audio buffers one sample can arrive in.
///
/// One per channel when the format is planar, which is what ScreenCaptureKit delivers, and
/// one in total when it is interleaved. Both are handled, because the format is the system's
/// choice rather than this code's.
const MAX_BUFFERS: usize = CHANNELS;

/// An [`AudioBufferList`] with room for every buffer a sample can arrive in.
///
/// The system type ends in a one-element array standing for a variable-length one, so a list
/// that holds more than a single buffer cannot be spelled with it directly. The layout is the
/// same: a count, then that many buffers.
#[repr(C)]
struct BufferList {
    count: u32,
    buffers: [AudioBuffer; MAX_BUFFERS],
}

impl BufferList {
    /// Returns an empty list for the system to fill in.
    fn empty() -> Self {
        Self {
            count: MAX_BUFFERS as u32,
            buffers: [(); MAX_BUFFERS].map(|()| AudioBuffer {
                mNumberChannels: 0,
                mDataByteSize: 0,
                mData: null_mut::<c_void>(),
            }),
        }
    }
}

/// What the capture thread and the stream's callback share.
struct Shared {
    samples: Mutex<VecDeque<f32>>,
    arrived: Condvar,
}

impl Shared {
    /// Appends interleaved samples and wakes whoever is waiting for a frame.
    fn push(&self, samples: &[f32]) {
        let Ok(mut pending) = self.samples.lock() else {
            return;
        };

        pending.extend(samples);

        // Dropped from the front rather than the back: if anything is going to be heard
        // again it is the newest audio, and a listener who has already fallen a quarter of a
        // second behind is not going to be helped by hearing what they missed.
        while pending.len() > BACKLOG_LIMIT {
            pending.pop_front();
        }

        self.arrived.notify_one();
    }
}

/// Instance variables of the audio output handler.
struct AudioIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `AudioOutput` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = AudioIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        /// Receives one buffer of system audio on the stream's dispatch queue.
        ///
        /// Not a realtime callback — ScreenCaptureKit delivers on the queue supplied when the
        /// output was added — so taking the lock here is sound. It is held for a copy of at
        /// most a few milliseconds of audio and nothing else.
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Audio {
                return;
            }

            let mut list = BufferList::empty();
            let mut block: *mut CMBlockBuffer = null_mut();

            // SAFETY: the sample is live for this call, the list is a local sized exactly as
            // reported, and the block buffer pointer is a live local the call writes once.
            let status = unsafe {
                sample.audio_buffer_list_with_retained_block_buffer(
                    null_mut(),
                    std::ptr::from_mut(&mut list).cast::<AudioBufferList>(),
                    size_of::<BufferList>(),
                    None,
                    None,
                    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
                    &raw mut block,
                )
            };

            // The block buffer is retained on success and must be released whatever happens
            // next, so it is taken first and dropped at the end of the scope.
            let retained = NonNull::new(block).map(|block| {
                // SAFETY: the call retained this block buffer and nothing else owns it.
                unsafe { CFRetained::from_raw(block) }
            });

            if status != 0 {
                return;
            }

            let count = (list.count as usize).min(MAX_BUFFERS);
            let mut interleaved = [0.0f32; FRAME_INTERLEAVED];

            if count == 1 {
                // Already interleaved: one buffer carrying both channels.
                let buffer = &list.buffers[0];
                // SAFETY: the system reported this pointer and length together, and the data
                // lives as long as the retained block buffer.
                let samples = unsafe { as_samples(buffer) };
                self.ivars().shared.push(samples);
            } else if count >= CHANNELS {
                // SAFETY: as above, once per channel.
                let left = unsafe { as_samples(&list.buffers[0]) };
                // SAFETY: as above.
                let right = unsafe { as_samples(&list.buffers[1]) };
                let frames = left.len().min(right.len());

                // In chunks, so a long buffer does not need a heap allocation to interleave.
                for chunk in (0..frames).step_by(FRAME_INTERLEAVED / CHANNELS) {
                    let end = (chunk + FRAME_INTERLEAVED / CHANNELS).min(frames);
                    let taken = end - chunk;

                    for (index, frame) in (chunk..end).enumerate() {
                        interleaved[index * CHANNELS] = left[frame];
                        interleaved[index * CHANNELS + 1] = right[frame];
                    }

                    self.ivars().shared.push(&interleaved[..taken * CHANNELS]);
                }
            }

            drop(retained);
        }
    }
);

/// Reads one audio buffer as float samples.
///
/// # Safety
///
/// The buffer's pointer and byte size must describe live memory, which is what
/// `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` guarantees for as long as the
/// block buffer it retained is held.
unsafe fn as_samples(buffer: &AudioBuffer) -> &[f32] {
    let Some(data) = NonNull::new(buffer.mData.cast::<f32>()) else {
        return &[];
    };

    // SAFETY: the caller guarantees the pointer and length belong together, and the format
    // was configured as float, so the byte size is a whole number of samples.
    unsafe { std::slice::from_raw_parts(data.as_ptr(), buffer.mDataByteSize as usize / 4) }
}

/// A running capture of this machine's audio output.
pub struct SystemAudioCapture {
    stream: Retained<SCStream>,
    shared: Arc<Shared>,
    frame: [f32; FRAME_INTERLEAVED],
    _handler: Retained<AudioOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl SystemAudioCapture {
    /// Starts capturing what this machine is playing.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::PermissionDenied`] when Screen Recording has not been granted —
    /// system audio is behind the same grant as the screen — [`CaptureError::NoDisplay`] when
    /// there is nothing to attach a stream to, and [`CaptureError::Start`] if ScreenCaptureKit
    /// refuses the session.
    pub fn start() -> Result<Self, CaptureError> {
        let content = shareable_content()?;

        // SAFETY: the content object is alive and its display list is immutable.
        let displays = unsafe { content.displays() };
        let display = displays.firstObject().ok_or(CaptureError::NoDisplay)?;

        let empty: Retained<NSArray<_>> = NSArray::new();
        // SAFETY: the display and the empty exclusion list both outlive the call.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &empty,
            )
        };

        // SAFETY: the constructor takes no arguments and returns a fresh object.
        let stream_config = unsafe { SCStreamConfiguration::new() };
        // SAFETY: every setter below takes a plain value the configuration copies.
        unsafe {
            stream_config.setCapturesAudio(true);
            stream_config.setSampleRate(SAMPLE_RATE as isize);
            stream_config.setChannelCount(CHANNELS as isize);
            // Off deliberately; see the note at the top of this file. On, it silently drops
            // audio from every process sharing this one's responsible process.
            stream_config.setExcludesCurrentProcessAudio(false);

            // The picture this stream will not be read for. One frame a second rather than
            // none, because a stream configured to deliver no video at all is refused.
            stream_config.setWidth(UNUSED_PICTURE);
            stream_config.setHeight(UNUSED_PICTURE);
            stream_config.setMinimumFrameInterval(objc2_core_media::CMTime {
                value: 1,
                timescale: 1,
                flags: objc2_core_media::CMTimeFlags::Valid,
                epoch: 0,
            });
        }

        let shared = Arc::new(Shared {
            samples: Mutex::new(VecDeque::with_capacity(BACKLOG_LIMIT)),
            arrived: Condvar::new(),
        });

        let handler = AudioOutput::alloc().set_ivars(AudioIvars {
            shared: Arc::clone(&shared),
        });
        // SAFETY: the instance variables were set immediately above, so `init` runs on a
        // fully constructed allocation.
        let handler: Retained<AudioOutput> = unsafe { objc2::msg_send![super(handler), init] };
        let queue = DispatchQueue::new("io.prism.audio", None);

        // SAFETY: the filter and configuration outlive the stream, and no delegate is
        // supplied because stream errors surface as audio that stops arriving.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                None,
            )
        };

        // Only the audio output is added. Screen samples are produced regardless — the
        // configuration demands a size — but with nothing listening they are never delivered.
        // SAFETY: the handler and queue are kept alive by the returned struct.
        unsafe {
            SCStream::addStreamOutput_type_sampleHandlerQueue_error(
                &stream,
                ProtocolObject::from_ref(&*handler),
                SCStreamOutputType::Audio,
                Some(&queue),
            )
            .map_err(|err| CaptureError::Start {
                reason: err.localizedDescription().to_string(),
            })?;
        }

        start_capture(&stream)?;

        Ok(Self {
            stream,
            shared,
            frame: [0.0; FRAME_INTERLEAVED],
            _handler: handler,
            _queue: queue,
        })
    }

    /// Waits up to `timeout` for one frame of interleaved samples.
    ///
    /// Never reports [`Pulled::Stopped`]. ScreenCaptureKit signals a stream that has ended by
    /// delivering nothing further, which is indistinguishable from a quiet moment, so this
    /// source keeps producing silence and lets the session end for a reason it can name.
    fn pull(&mut self, timeout: Duration) -> Pulled<'_> {
        let Ok(pending) = self.shared.samples.lock() else {
            return Pulled::Silence;
        };

        let Ok((mut pending, _)) =
            self.shared
                .arrived
                .wait_timeout_while(pending, timeout, |pending| {
                    pending.len() < FRAME_INTERLEAVED
                })
        else {
            return Pulled::Silence;
        };

        if pending.len() < FRAME_INTERLEAVED {
            return Pulled::Silence;
        }

        for slot in &mut self.frame {
            *slot = pending.pop_front().unwrap_or(0.0);
        }

        drop(pending);

        Pulled::Frame(&self.frame)
    }
}

impl SystemAudio for SystemAudioCapture {
    /// See [`SystemAudioCapture::pull`].
    fn poll(&mut self, timeout: Duration) -> Pulled<'_> {
        self.pull(timeout)
    }
}

impl Drop for SystemAudioCapture {
    /// Stops the stream and waits for the system to say it has, before returning.
    fn drop(&mut self) {
        stop_capture(&self.stream);
    }
}

impl std::fmt::Debug for SystemAudioCapture {
    /// Names the type without reaching into the Objective-C objects it holds.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemAudioCapture").finish_non_exhaustive()
    }
}
