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
    SCContentFilter, SCRunningApplication, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};

use crate::capture::{CaptureConfig, CaptureError};

/// Four character code for the NV12 pixel format the encoder wants.
const NV12: u32 = u32::from_be_bytes(*b"420v");

/// How long to wait for ScreenCaptureKit to answer an asynchronous request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A captured screen frame, ready to hand to the encoder.
///
/// Cloning retains the same pixel buffer rather than copying the picture, which is what makes
/// it cheap enough to keep the newest frame beside the ones the encoder is still reading.
#[derive(Debug, Clone)]
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

/// Returns how many pixels a display has, as opposed to how many points it is laid out in.
///
/// `SCDisplay` reports points. On a Retina display that is half the pixels in each direction,
/// and capturing at that size sent a quarter of the picture the screen was actually showing —
/// soft text on the far end, however large the window it arrived in. The display's current mode
/// knows its backing size; the points are what is left when it will not say.
fn native_pixels(display: &objc2_screen_capture_kit::SCDisplay) -> (u32, u32) {
    // SAFETY: the display belongs to shareable content that is alive for this call, and these
    // three getters take no arguments and return plain values.
    let (points_wide, points_high, id) = unsafe {
        (
            display.width() as u32,
            display.height() as u32,
            display.displayID(),
        )
    };

    let Some(mode) = objc2_core_graphics::CGDisplayCopyDisplayMode(id) else {
        return (points_wide, points_high);
    };

    let wide = objc2_core_graphics::CGDisplayMode::pixel_width(Some(&mode)) as u32;
    let high = objc2_core_graphics::CGDisplayMode::pixel_height(Some(&mode)) as u32;

    if wide == 0 || high == 0 {
        (points_wide, points_high)
    } else {
        (wide, high)
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

        // The main display, which is the one input is placed on and the one the pointer is
        // reported against. ScreenCaptureKit lists displays in an order of its own, and on a
        // machine with several the first of them was a portrait monitor off to one side —
        // so the picture was one screen and the clicks landed on another.
        let main = objc2_core_graphics::CGMainDisplayID();
        let display = displays
            .iter()
            // SAFETY: each display belongs to the content just fetched.
            .find(|one| unsafe { one.displayID() } == main)
            .or_else(|| displays.firstObject())
            .ok_or(CaptureError::NoDisplay)?;

        let (width, height) =
            crate::capture::fit_within(native_pixels(&display), (config.width, config.height));

        // Everything on the display except this application's own windows. What it puts on the
        // screen while it is being watched is for the person sitting here — the handle that
        // ends the session above all — and drawn into the picture it would be a second handle
        // on the other machine's screen, one that does nothing there.
        //
        // Applications rather than windows, because the handle is made after the capture
        // starts and a list of windows is only the ones that existed when it was taken. A
        // process with no windows at all, the command line's, is not in the list and leaves
        // nothing out.
        let me = std::process::id();
        // SAFETY: the content object is alive and its application list is immutable.
        let own: Vec<Retained<SCRunningApplication>> = unsafe { content.applications() }
            .iter()
            // SAFETY: each application belongs to the content just fetched.
            .filter(|app| u32::try_from(unsafe { app.processID() }).is_ok_and(|pid| pid == me))
            .collect();
        let own = NSArray::from_retained_slice(&own);
        let none: Retained<NSArray<_>> = NSArray::new();

        // SAFETY: the display and both lists outlive the call.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &own,
                &none,
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
    /// Stops the stream and waits for the system to say it has, before returning.
    fn drop(&mut self) {
        stop_capture(&self.stream);
    }
}

/// Fetches the list of capturable displays, blocking until ScreenCaptureKit answers.
///
/// # Errors
///
/// Returns [`CaptureError::PermissionDenied`] if the request fails, which is what
/// happens when Screen Recording has not been granted, and [`CaptureError::Start`] if it
/// does not answer at all.
pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
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

/// Stops a stream, blocking until ScreenCaptureKit says it has finished doing so.
pub(crate) fn stop_capture(stream: &SCStream) {
    let (tx, rx) = std::sync::mpsc::channel();

    let handler = RcBlock::new(move |_error: *mut NSError| {
        let _ = tx.send(());
    });

    // SAFETY: the stream is alive and the block is copied by ScreenCaptureKit.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };

    // Waited for rather than fired and forgotten, which is what this used to do. Stopping is
    // asynchronous and the capture daemon outlives this process, so a program that exits the
    // instant it has asked leaves the system tearing down a stream on behalf of something
    // that is already gone. Starting is waited for; there is no reason stopping should not
    // be, and every reason to hand the daemon back a stream it has finished with.
    //
    // A timeout rather than an unbounded wait because this runs in a destructor: if the
    // system will not answer, carrying on is better than never returning.
    let _ = rx.recv_timeout(REQUEST_TIMEOUT);
}

/// Starts a stream, blocking until ScreenCaptureKit says it has started or refused.
///
/// # Errors
///
/// Returns [`CaptureError::Start`] with what the system said, or with a note that it said
/// nothing at all.
pub(crate) fn start_capture(stream: &SCStream) -> Result<(), CaptureError> {
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
