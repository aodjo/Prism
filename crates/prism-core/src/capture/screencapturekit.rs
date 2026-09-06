//! ScreenCaptureKit capture for macOS.
//!
//! Frames arrive as IOSurface-backed `CVPixelBuffer`s in NV12, which is the encoder's
//! native format, so a captured frame reaches VideoToolbox without ever being copied or
//! converted.
//!
//! ScreenCaptureKit delivers frames on its own dispatch queue through an Objective-C
//! protocol, so the output handler here is a real Objective-C class defined in Rust. It
//! forwards frames over a bounded channel and drops them when the channel is full: an
//! encoder that has fallen behind is better served by the newest frame than by a queue of
//! old ones.
//!
//! **This requires Screen Recording permission.** The first attempt raises the system
//! prompt; a refusal has to be undone by hand in Privacy settings, which is why the error
//! for it is reported distinctly.

use core::ptr::NonNull;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};

use crate::capture::{CaptureConfig, CaptureError};

/// Four character code for the NV12 pixel format the encoder wants.
const NV12: u32 = u32::from_be_bytes(*b"420v");

/// How long to wait for ScreenCaptureKit to answer an asynchronous request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A captured screen frame, ready to hand to the encoder.
#[derive(Debug)]
pub struct CapturedFrame {
    /// Host clock when the frame was captured, in microseconds.
    pub capture_ts_us: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    buffer: CFRetained<CVPixelBuffer>,
}

// SAFETY: `CVPixelBuffer` is a CoreFoundation type with atomic reference counting, and
// ScreenCaptureKit hands frames over on its own queue with the expectation that they are
// consumed elsewhere.
unsafe impl Send for CapturedFrame {}

impl CapturedFrame {
    /// Returns the underlying pixel buffer for the encoder or renderer to use.
    #[must_use]
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }
}

/// Moves an Objective-C object between threads.
///
/// ScreenCaptureKit answers asynchronously on a queue of its own choosing, so the result
/// has to cross back to the caller. The objects involved are not main-thread bound, which
/// is what makes this sound.
struct Portable<T>(T);

// SAFETY: the objects carried here — `SCShareableContent` and `NSError` — are ordinary
// reference counted Objective-C objects with no thread affinity.
unsafe impl<T> Send for Portable<T> {}

/// Instance variables of the stream output handler.
struct HandlerIvars {
    frames: SyncSender<CapturedFrame>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `StreamOutput` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = HandlerIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        /// Receives one frame from ScreenCaptureKit on its dispatch queue.
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }

            // SAFETY: ScreenCaptureKit passes a live sample buffer for this call.
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                return;
            };

            // SAFETY: a screen sample's image buffer is always a pixel buffer.
            let buffer = unsafe {
                CFRetained::retain(NonNull::new_unchecked(
                    CFRetained::as_ptr(&image).as_ptr().cast::<CVPixelBuffer>(),
                ))
            };

            let capture_ts_us = crate::clock::now_us();

            let (width, height) = (
                objc2_core_video::CVPixelBufferGetWidth(&buffer) as u32,
                objc2_core_video::CVPixelBufferGetHeight(&buffer) as u32,
            );

            // Dropped rather than queued: an encoder that has fallen behind wants the
            // newest frame, not a backlog of old ones.
            let _ = self.ivars().frames.try_send(CapturedFrame {
                capture_ts_us,
                width,
                height,
                buffer,
            });
        }
    }
);

impl StreamOutput {
    /// Creates a handler that forwards frames to `frames`.
    fn new(frames: SyncSender<CapturedFrame>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(HandlerIvars { frames });
        // SAFETY: `NSObject`'s designated initialiser takes no arguments.
        unsafe { msg_send![super(this), init] }
    }
}

/// A running capture of one display.
pub struct ScreenCapture {
    stream: Retained<SCStream>,
    frames: Receiver<CapturedFrame>,
    width: u32,
    height: u32,
    _handler: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl ScreenCapture {
    /// Starts capturing the main display.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::PermissionDenied`] when Screen Recording has not been
    /// granted, [`CaptureError::NoDisplay`] when there is nothing to capture, and
    /// [`CaptureError::Start`] if ScreenCaptureKit refuses the session.
    pub fn start(config: CaptureConfig) -> Result<Self, CaptureError> {
        let content = shareable_content()?;

        // SAFETY: the content object is alive and its display list is immutable.
        let displays = unsafe { content.displays() };
        let display = displays.firstObject().ok_or(CaptureError::NoDisplay)?;

        // SAFETY: the display belongs to the content just fetched.
        let (native_width, native_height) =
            unsafe { (display.width() as u32, display.height() as u32) };
        let width = if config.width > 0 {
            config.width
        } else {
            native_width
        };
        let height = if config.height > 0 {
            config.height
        } else {
            native_height
        };

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
            stream_config.setWidth(width as usize);
            stream_config.setHeight(height as usize);
            stream_config.setPixelFormat(NV12);
            stream_config.setShowsCursor(config.show_cursor);
            stream_config.setQueueDepth(config.queue_depth as isize);
            stream_config.setCapturesAudio(false);
            stream_config.setScalesToFit(true);
            stream_config.setMinimumFrameInterval(objc2_core_media::CMTime {
                value: 1,
                timescale: config.fps.max(1) as i32,
                flags: objc2_core_media::CMTimeFlags::Valid,
                epoch: 0,
            });
        }

        let (frames_tx, frames_rx) = sync_channel(config.queue_depth.max(1));
        let handler = StreamOutput::new(frames_tx);
        let queue = DispatchQueue::new("io.prism.capture", None);

        // SAFETY: the filter and configuration outlive the stream, and no delegate is
        // supplied because stream errors surface as an empty frame channel.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                None,
            )
        };

        // SAFETY: the handler and queue are kept alive by the returned struct.
        unsafe {
            SCStream::addStreamOutput_type_sampleHandlerQueue_error(
                &stream,
                ProtocolObject::from_ref(&*handler),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|err| CaptureError::Start {
                reason: err.localizedDescription().to_string(),
            })?;
        }

        start_capture(&stream)?;

        Ok(Self {
            stream,
            frames: frames_rx,
            width,
            height,
            _handler: handler,
            _queue: queue,
        })
    }

    /// Returns the captured display's width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Returns the captured display's height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Waits up to `timeout` for the next captured frame.
    ///
    /// Returns `None` when nothing arrived, which is normal on a still screen:
    /// ScreenCaptureKit only delivers a frame when something changes.
    pub fn poll(&mut self, timeout: Duration) -> Option<CapturedFrame> {
        self.frames.recv_timeout(timeout).ok()
    }
}

impl Drop for ScreenCapture {
    /// Stops the stream so the compositor stops producing frames for it.
    fn drop(&mut self) {
        // SAFETY: the stream is alive, and passing no completion handler is allowed.
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// Fetches the list of capturable displays, blocking until ScreenCaptureKit answers.
///
/// # Errors
///
/// Returns [`CaptureError::PermissionDenied`] if the request fails, which is what
/// happens when Screen Recording has not been granted, and [`CaptureError::Start`] if it
/// does not answer at all.
fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    let (tx, rx) = std::sync::mpsc::channel();

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes either a live content object or a live error,
            // both valid for the duration of this call, so retaining is required to keep one.
            let result = unsafe {
                if content.is_null() {
                    Err(NonNull::new(error).map(|e| Retained::retain(e.as_ptr()).unwrap()))
                } else {
                    Ok(Retained::retain(content).expect("a non-null object retains"))
                }
            };
            let _ = tx.send(Portable(result));
        },
    );

    // SAFETY: the block lives until this function returns, and ScreenCaptureKit copies it.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    match rx.recv_timeout(REQUEST_TIMEOUT) {
        Ok(Portable(Ok(content))) => Ok(content),
        Ok(Portable(Err(_))) => Err(CaptureError::PermissionDenied),
        Err(_) => Err(CaptureError::Start {
            reason: "ScreenCaptureKit did not answer the content request".to_owned(),
        }),
    }
}

/// Starts the stream, blocking until ScreenCaptureKit reports success or failure.
///
/// # Errors
///
/// Returns [`CaptureError::Start`] with the platform's message if the stream will not
/// start.
fn start_capture(stream: &SCStream) -> Result<(), CaptureError> {
    let (tx, rx) = std::sync::mpsc::channel();

    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: a non-null error is live for the duration of this call.
        let message =
            unsafe { NonNull::new(error).map(|e| e.as_ref().localizedDescription().to_string()) };
        let _ = tx.send(message);
    });

    // SAFETY: the stream is alive and the block is copied by ScreenCaptureKit.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

    match rx.recv_timeout(REQUEST_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(message)) => Err(CaptureError::Start { reason: message }),
        Err(_) => Err(CaptureError::Start {
            reason: "ScreenCaptureKit did not answer the start request".to_owned(),
        }),
    }
}
