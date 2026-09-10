//! VideoToolbox H.264 encoder for macOS.
//!
//! The session is configured for the lowest latency the hardware will give: real time,
//! no frame reordering so there are no B-frames, a data rate ceiling of a single frame
//! so the encoder cannot emit a burst that takes several frame times to transmit, and a
//! slice size limit so a frame arrives as several NAL units that can start moving before
//! the frame is finished.
//!
//! VideoToolbox hands frames back on its own thread through a C callback. Encoded frames
//! travel from there to the caller over a channel, and the buffers travel back the same
//! way, so a running session does not allocate.

use core::ffi::c_void;
use core::ptr::{NonNull, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use objc2_core_foundation::{CFRetained, CFString, CFType, kCFBooleanFalse, kCFBooleanTrue};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferMetalCompatibilityKey,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_EnableLTR, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxH264SliceBytes, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel,
};

use crate::encode::{EncodeError, EncodedFrame, EncoderConfig};
use crate::net::negotiate::Codec;

/// Four character code for the NV12 pixel format VideoToolbox encodes natively.
const NV12: u32 = u32::from_be_bytes(*b"420v");

/// Keyframe interval, in frames, requested from the encoder.
///
/// Deliberately enormous. Periodic IDR frames are a bitrate spike and a latency spike, and
/// this pipeline recovers from loss with forward error correction instead, which repairs
/// the packets rather than resending the picture.
///
/// The plan's alternative was long-term reference invalidation, and on this platform that
/// is not available: Apple Silicon's hardware H.264 encoder refuses `EnableLTR`, the same
/// way it refuses the slice size limit. See [`VideoToolboxEncoder::ltr_supported`]. A host
/// that needs reference-based recovery needs a different encoder.
///
/// A decoder still has to be able to start, which the repeated parameter sets take care of
/// — see [`PARAMETER_SET_INTERVAL`]. The host can also ask for an IDR explicitly.
const KEYFRAME_INTERVAL: i32 = 100_000;

/// How often the parameter sets are repeated, in frames.
///
/// A decoder cannot start without SPS and PPS, and VideoToolbox emits them only alongside
/// an IDR. With the keyframe interval set as high as it is, that means once at the start of
/// the session and never again — so a client that joins late, or loses the first frame,
/// waits forever on a black window with nothing reporting an error.
///
/// Sixty frames is once a second at the rates this pipeline runs, and the two parameter
/// sets together are a few dozen bytes. Against a stream measured in megabits it is free,
/// and it is the difference between a recoverable stream and one that has exactly one
/// chance to be understood.
const PARAMETER_SET_INTERVAL: u64 = 60;

/// Status VideoToolbox returns when an encoder does not implement a property.
///
/// `kVTPropertyNotSupportedErr`. Apple Silicon's hardware H.264 encoder returns this for
/// the slice size limit, so slicing has to be treated as a capability rather than a
/// requirement — and an Intel Mac returns it for the speed-over-quality preference, so that
/// is one too. Which encoder has which knob is a fact about the machine, not about this file.
const PROPERTY_NOT_SUPPORTED: i32 = -12900;

/// How many encoded frames may queue up before the encoder thread blocks.
///
/// Small on purpose: if the sender cannot keep up, the right response is back pressure
/// rather than a growing queue of frames that are already too late to be useful.
const OUTPUT_QUEUE_DEPTH: usize = 4;

/// An NV12 frame in memory that VideoToolbox can encode.
///
/// NV12 is the encoder's native format, so filling one of these avoids the colour
/// conversion an RGB source would need. Real capture supplies its own buffers; this type
/// exists so a synthetic source can drive the encoder over the same path.
#[derive(Debug)]
pub struct Nv12Frame {
    buffer: CFRetained<CVPixelBuffer>,
    width: u32,
    height: u32,
}

impl Nv12Frame {
    /// Allocates an NV12 frame of the given size.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if CoreVideo will not allocate the buffer.
    pub fn new(width: u32, height: u32) -> Result<Self, EncodeError> {
        let attributes = surface_backed_attributes();
        let mut raw: *mut CVPixelBuffer = null_mut();

        // SAFETY: `raw` is a valid, writable pointer for the duration of the call, and
        // CoreVideo either writes a retained buffer into it or returns a failure status.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width as usize,
                height as usize,
                NV12,
                Some(&attributes),
                NonNull::from(&mut raw),
            )
        };

        if status != 0 || raw.is_null() {
            return Err(EncodeError::InputBuffer {
                reason: "CVPixelBufferCreate failed",
            });
        }

        // SAFETY: CoreVideo returned a buffer it created, so ownership transfers here and
        // `CFRetained::from_raw` is the correct way to take it.
        let buffer = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

        Ok(Self {
            buffer,
            width,
            height,
        })
    }

    /// Returns the frame's width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Returns the frame's height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Returns the underlying pixel buffer.
    ///
    /// Exposed so a synthetic picture can be handed to the renderer on the same path a
    /// decoded one takes, which is what lets the colour conversion be tested without a
    /// full encode and decode round trip.
    #[must_use]
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }

    /// Locks the frame and hands its two planes to `fill`.
    ///
    /// `fill` receives the luma plane with its stride and the interleaved chroma plane
    /// with its stride. Strides are usually larger than the width because CoreVideo
    /// aligns rows, so writing row by row rather than as one block is required.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if the buffer cannot be locked.
    ///
    /// # Panics
    ///
    /// Panics if CoreVideo reports a null plane address for a buffer it locked, which
    /// would mean the buffer is not the planar format it was created as.
    /// Hands the frame's two planes to `read`, without changing them.
    ///
    /// The same locking and plane arithmetic as [`Nv12Frame::fill`], and used for the same
    /// reason in reverse: writing the frames that went in alongside the stream that came out
    /// is what makes two codecs comparable against the thing they were both reproducing.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if the buffer cannot be locked, and whatever
    /// `read` returned otherwise.
    ///
    /// # Panics
    ///
    /// Panics if CoreVideo reports a null plane address for a buffer it locked.
    pub fn read<T>(
        &self,
        read: impl FnOnce(&[u8], usize, &[u8], usize) -> T,
    ) -> Result<T, EncodeError> {
        // SAFETY: the buffer is alive for the duration of this call, and the lock is released
        // before returning on every path.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(&self.buffer, CVPixelBufferLockFlags::ReadOnly) };
        if status != 0 {
            return Err(EncodeError::InputBuffer {
                reason: "could not lock the pixel buffer",
            });
        }

        // SAFETY: the buffer is locked, so the plane pointers are valid until it is unlocked.
        // NV12 has exactly two planes, the luma one covering `height` rows and the chroma one
        // covering `height / 2`, so the lengths below are within the allocation.
        let out = unsafe {
            let y_ptr = CVPixelBufferGetBaseAddressOfPlane(&self.buffer, 0).cast::<u8>();
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&self.buffer, 0);
            let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(&self.buffer, 1).cast::<u8>();
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&self.buffer, 1);

            assert!(
                !y_ptr.is_null() && !uv_ptr.is_null(),
                "CoreVideo locked a buffer and then reported a null plane"
            );

            let height = self.height as usize;
            let luma = core::slice::from_raw_parts(y_ptr, y_stride * height);
            let chroma = core::slice::from_raw_parts(uv_ptr, uv_stride * height / 2);

            read(luma, y_stride, chroma, uv_stride)
        };

        // SAFETY: the buffer was locked above with the same flags.
        unsafe {
            let _ = CVPixelBufferUnlockBaseAddress(&self.buffer, CVPixelBufferLockFlags::ReadOnly);
        }

        Ok(out)
    }

    /// Locks the frame and hands its two planes to `fill`.
    ///
    /// `fill` receives the luma plane with its stride and the interleaved chroma plane
    /// with its stride. Strides are usually larger than the width because CoreVideo
    /// aligns rows, so writing row by row rather than as one block is required.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if the buffer cannot be locked.
    ///
    /// # Panics
    ///
    /// Panics if CoreVideo reports a null plane address for a buffer it locked, which
    /// would mean the buffer is not the planar format it was created as.
    pub fn fill(
        &mut self,
        fill: impl FnOnce(&mut [u8], usize, &mut [u8], usize),
    ) -> Result<(), EncodeError> {
        // SAFETY: the buffer is alive for the duration of this call, and the lock is
        // released before returning on every path.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(&self.buffer, CVPixelBufferLockFlags::empty()) };
        if status != 0 {
            return Err(EncodeError::InputBuffer {
                reason: "could not lock the pixel buffer",
            });
        }

        // SAFETY: the buffer is locked, so the plane pointers are valid until it is
        // unlocked. NV12 has exactly two planes, the luma plane covering `height` rows
        // and the chroma plane covering `height / 2`, so the lengths below are within the
        // allocation CoreVideo made.
        unsafe {
            let y_ptr = CVPixelBufferGetBaseAddressOfPlane(&self.buffer, 0).cast::<u8>();
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&self.buffer, 0);
            let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(&self.buffer, 1).cast::<u8>();
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&self.buffer, 1);

            assert!(
                !y_ptr.is_null() && !uv_ptr.is_null(),
                "a locked NV12 buffer has two planes"
            );

            let y = core::slice::from_raw_parts_mut(y_ptr, y_stride * self.height as usize);
            let uv = core::slice::from_raw_parts_mut(uv_ptr, uv_stride * self.height as usize / 2);

            fill(y, y_stride, uv, uv_stride);
        }

        // SAFETY: the buffer was locked immediately above with the same flags.
        unsafe {
            CVPixelBufferUnlockBaseAddress(&self.buffer, CVPixelBufferLockFlags::empty());
        }

        Ok(())
    }
}

/// State the encoder callback writes into, reached through the session's ref con.
#[derive(Debug)]
struct CallbackContext {
    /// Which codec this session encodes.
    ///
    /// Held here because the callback runs on VideoToolbox's own thread, where the encoder is
    /// not reachable — and it changes how a NAL unit is read: H.264 puts its type in the low
    /// five bits of the first byte, HEVC in bits one to six, and its parameter sets are three
    /// rather than two. A callback that assumed one and got the other produces a stream no
    /// decoder will take.
    codec: Codec,
    output: SyncSender<EncodedFrame>,
    spare: Mutex<Vec<EncodedFrame>>,
    /// Frames handed back so far, which is what decides when parameter sets are repeated.
    ///
    /// Counted here rather than on the encoder because the decision is made on
    /// VideoToolbox's own callback thread, where the encoder is not reachable.
    frames: AtomicU64,
}

/// A configured VideoToolbox H.264 compression session.
#[derive(Debug)]
pub struct VideoToolboxEncoder {
    session: CFRetained<VTCompressionSession>,
    output: Receiver<EncodedFrame>,
    context: Box<CallbackContext>,
    current: Option<EncodedFrame>,
    config: EncoderConfig,
    slicing: bool,
    ltr: bool,
}

impl VideoToolboxEncoder {
    /// Creates a compression session configured for low latency.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] if VideoToolbox will not create the
    /// session, or [`EncodeError::Property`] if it refuses one of the settings the
    /// latency target depends on.
    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        let (tx, rx) = sync_channel(OUTPUT_QUEUE_DEPTH);
        let context = Box::new(CallbackContext {
            codec: config.codec,
            output: tx,
            spare: Mutex::new(Vec::new()),
            frames: AtomicU64::new(0),
        });
        let refcon = (&raw const *context).cast::<c_void>().cast_mut();

        let mut raw: *mut VTCompressionSession = null_mut();

        // SAFETY: the width and height are positive, the callback matches the expected
        // signature, and `refcon` points at a boxed context that this struct keeps alive
        // for at least as long as the session.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                match config.codec {
                    Codec::H264 => kCMVideoCodecType_H264,
                    Codec::Hevc => kCMVideoCodecType_HEVC,
                    // Apple encodes no AV1 in hardware on any machine this runs on, and the
                    // negotiation should never have chosen it. Refusing here rather than
                    // starting a session that produces nothing is what turns a silent black
                    // window into a session that fails to open.
                    Codec::Av1 => {
                        return Err(EncodeError::SessionCreate {
                            reason: "AV1 is not encodable by VideoToolbox",
                            status: 0,
                        });
                    }
                },
                None,
                None,
                None,
                Some(output_callback),
                refcon,
                NonNull::from(&mut raw),
            )
        };

        if status != 0 || raw.is_null() {
            return Err(EncodeError::SessionCreate {
                reason: "VTCompressionSessionCreate",
                status,
            });
        }

        // SAFETY: VideoToolbox created the session, so ownership transfers here.
        let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

        let mut encoder = Self {
            session,
            output: rx,
            context,
            current: None,
            config,
            slicing: false,
            ltr: false,
        };
        encoder.slicing = encoder.configure()?;
        encoder.ltr = encoder.configure_ltr()?;

        Ok(encoder)
    }

    /// Returns the configuration this session was created with.
    #[must_use]
    pub fn config(&self) -> EncoderConfig {
        self.config
    }

    /// Returns whether the encoder honours the configured slice size limit.
    ///
    /// Apple Silicon's hardware H.264 encoder does not implement it, so on those machines
    /// a frame arrives as a single NAL unit and cannot start transmitting until it is
    /// fully encoded. That costs roughly half a frame time and is unavoidable here;
    /// NVENC on the Windows host does support slicing, which is where it matters most.
    #[must_use]
    pub fn slicing_supported(&self) -> bool {
        self.slicing
    }

    /// Submits a frame for encoding.
    ///
    /// Takes the pixel buffer rather than a specific frame type, so a synthetic picture
    /// and a captured one go down exactly the same path.
    ///
    /// Returns as soon as VideoToolbox accepts the frame; the encoded result arrives
    /// through [`Self::poll`].
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] if VideoToolbox rejects the frame.
    pub fn encode(
        &mut self,
        frame: &CVPixelBuffer,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<(), EncodeError> {
        let pts = CMTime {
            value: pts_us as i64,
            timescale: 1_000_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let duration = CMTime {
            value: 1,
            timescale: self.config.fps.max(1) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };

        let properties = force_idr.then(force_keyframe_properties);

        // SAFETY: the pixel buffer outlives the call, the timestamps are valid, and no
        // source ref con is used so a null pointer is correct there.
        let status = unsafe {
            self.session.encode_frame(
                &*(core::ptr::from_ref(frame).cast::<CVImageBuffer>()),
                pts,
                duration,
                properties.as_deref(),
                null_mut(),
                null_mut(),
            )
        };

        if status != 0 {
            return Err(EncodeError::Encode { status });
        }

        Ok(())
    }

    /// Waits up to `timeout` for the next encoded frame.
    ///
    /// The previously returned frame's buffers are recycled at the start of each call,
    /// which is why the borrow cannot outlive the next poll.
    pub fn poll(&mut self, timeout: Duration) -> Option<&EncodedFrame> {
        if let Some(mut used) = self.current.take() {
            used.reset();
            if let Ok(mut spare) = self.context.spare.lock() {
                spare.push(used);
            }
        }

        self.current = self.output.recv_timeout(timeout).ok();
        self.current.as_ref()
    }

    /// Applies the settings the latency target depends on.
    ///
    /// Returns whether the encoder accepted the slice size limit. Every other property
    /// here is mandatory, because a session without them would silently miss the latency
    /// target rather than fail.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Property`] naming the first mandatory property VideoToolbox
    /// refuses.
    fn configure(&self) -> Result<bool, EncodeError> {
        // SAFETY: every key below is a VideoToolbox compression property constant and
        // every value matches the type that property expects.
        unsafe {
            self.set_bool("RealTime", kVTCompressionPropertyKey_RealTime, true)?;
            self.set_bool(
                "AllowFrameReordering",
                kVTCompressionPropertyKey_AllowFrameReordering,
                false,
            )?;
            // A preference, not a requirement, and one that plenty of encoders do not have.
            // An Intel Mac refuses it outright with `kVTPropertyNotSupportedErr`, and this was
            // written as though every encoder had the knob — so on those machines the session
            // could not be created at all and the machine could not be shared. What its absence
            // costs is some encoding speed; what insisting on it cost was the whole feature.
            self.set_bool_if_supported(
                "PrioritizeEncodingSpeedOverQuality",
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                true,
            )?;
            self.set_number(
                "AverageBitRate",
                kVTCompressionPropertyKey_AverageBitRate,
                self.config.bitrate_bps as i64,
            )?;
            self.set_number(
                "ExpectedFrameRate",
                kVTCompressionPropertyKey_ExpectedFrameRate,
                i64::from(self.config.fps),
            )?;
            self.set_number(
                "MaxKeyFrameInterval",
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                i64::from(KEYFRAME_INTERVAL),
            )?;
            self.set_property(
                "ProfileLevel",
                kVTCompressionPropertyKey_ProfileLevel,
                Some(match self.config.codec {
                    Codec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel.as_ref(),
                    // High rather than Baseline: every decoder this streams to reads it, and
                    // it is worth a few percent of bitrate for the same picture.
                    _ => kVTProfileLevel_H264_High_AutoLevel.as_ref(),
                }),
            )?;

            // There is no HEVC equivalent of the H.264 slice ceiling, and the H.264 one is
            // refused on Apple Silicon anyway. Both mean the same thing here: this encoder
            // emits whole frames, and transmission cannot start before one is finished.
            if self.config.max_slice_bytes == 0 || self.config.codec != Codec::H264 {
                return Ok(false);
            }

            match self.set_number(
                "MaxH264SliceBytes",
                kVTCompressionPropertyKey_MaxH264SliceBytes,
                i64::from(self.config.max_slice_bytes),
            ) {
                Ok(()) => Ok(true),
                Err(EncodeError::Property {
                    status: PROPERTY_NOT_SUPPORTED,
                    ..
                }) => Ok(false),
                Err(err) => Err(err),
            }
        }
    }

    /// Returns whether this encoder accepted long-term references.
    ///
    /// A capability, not a guarantee: see [`VideoToolboxEncoder::configure_ltr`].
    #[must_use]
    pub fn ltr_supported(&self) -> bool {
        self.ltr
    }

    /// Asks the encoder for long-term references and reports whether it agreed.
    ///
    /// Probed rather than required. The constant existing in a header says nothing about
    /// whether the silicon implements it, and this project has the precedent already: the
    /// same encoder refuses `MaxH264SliceBytes` on Apple Silicon with the same status. A
    /// host whose encoder declines simply has no reference-based recovery and must fall
    /// back to keyframes.
    ///
    /// Note what VideoToolbox offers is weaker than the NVENC model the plan describes.
    /// There is no equivalent of invalidating a specific reference: the encoder hands out an
    /// opaque token per reference frame, the host hands back the tokens the client has
    /// acknowledged, and on loss the host asks for a refresh — the encoder then picks which
    /// acknowledged reference to use. That still removes the keyframe hitch, with less
    /// control over which frame is chosen.
    fn configure_ltr(&self) -> Result<bool, EncodeError> {
        // SAFETY: `EnableLTR` is a compression property that takes a boolean.
        match unsafe { self.set_bool("EnableLTR", kVTCompressionPropertyKey_EnableLTR, true) } {
            Ok(()) => Ok(true),
            Err(EncodeError::Property {
                status: PROPERTY_NOT_SUPPORTED,
                ..
            }) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Changes the target bitrate on a running session.
    ///
    /// This is the actuator congestion control needs. Pacing alone only slows the wire while
    /// the encoder keeps producing the same bytes, which moves the queue into the host
    /// instead of removing it; the rate the encoder is told is what actually changes how
    /// much there is to send.
    ///
    /// Takes effect from the next frame submitted. A rate the encoder refuses leaves the
    /// session at whatever it had, which is a worse picture than the caller asked for rather
    /// than a broken session.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Property`] if VideoToolbox refuses the new rate.
    pub fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        if bitrate_bps == self.config.bitrate_bps {
            return Ok(());
        }

        // SAFETY: `AverageBitRate` is a compression property that takes a number, and it is
        // documented as settable on a live session.
        unsafe {
            self.set_number(
                "AverageBitRate",
                kVTCompressionPropertyKey_AverageBitRate,
                i64::from(bitrate_bps),
            )?;
        }

        self.config.bitrate_bps = bitrate_bps;

        Ok(())
    }

    /// Sets a boolean session property.
    ///
    /// # Safety
    ///
    /// `key` must be a VideoToolbox property that takes a boolean.
    unsafe fn set_bool(
        &self,
        name: &'static str,
        key: &CFString,
        value: bool,
    ) -> Result<(), EncodeError> {
        // SAFETY: the two boolean singletons are CoreFoundation statics with static
        // lifetime, and every CoreFoundation type can be viewed as a `CFType`.
        let value = unsafe {
            let boolean = if value {
                kCFBooleanTrue
            } else {
                kCFBooleanFalse
            };
            boolean.map(|b| &*(core::ptr::from_ref(b).cast::<CFType>()))
        };
        // SAFETY: the caller guarantees `key` is a property that takes a boolean.
        unsafe { self.set_property(name, key, value) }
    }

    /// Sets a boolean session property, and shrugs if this encoder has never heard of it.
    ///
    /// For preferences rather than requirements: a knob that tunes how an encoder spends its
    /// time, where not having the knob costs some of what it was set for and nothing else.
    ///
    /// Only [`PROPERTY_NOT_SUPPORTED`] is tolerated. A property that exists and was refused —
    /// the wrong type, a dead session — is still an error, because that is a mistake in this
    /// file rather than a fact about the machine.
    ///
    /// # Safety
    ///
    /// `key` must be a VideoToolbox property that takes a boolean.
    unsafe fn set_bool_if_supported(
        &self,
        name: &'static str,
        key: &CFString,
        value: bool,
    ) -> Result<(), EncodeError> {
        // SAFETY: the caller guarantees `key` is a property that takes a boolean.
        match unsafe { self.set_bool(name, key, value) } {
            Err(EncodeError::Property { status, .. }) if status == PROPERTY_NOT_SUPPORTED => Ok(()),
            other => other,
        }
    }

    /// Sets a numeric session property.
    ///
    /// # Safety
    ///
    /// `key` must be a VideoToolbox property that takes a number.
    unsafe fn set_number(
        &self,
        name: &'static str,
        key: &CFString,
        value: i64,
    ) -> Result<(), EncodeError> {
        let number = objc2_core_foundation::CFNumber::new_i64(value);
        // SAFETY: every CoreFoundation type can be viewed as a `CFType`.
        let value = unsafe { &*(core::ptr::from_ref(&*number).cast::<CFType>()) };
        // SAFETY: the caller guarantees `key` is a property that takes a number.
        unsafe { self.set_property(name, key, Some(value)) }
    }

    /// Sets a session property to an arbitrary CoreFoundation value.
    ///
    /// # Safety
    ///
    /// `value` must be of the type `key` expects.
    unsafe fn set_property(
        &self,
        name: &'static str,
        key: &CFString,
        value: Option<&CFType>,
    ) -> Result<(), EncodeError> {
        // SAFETY: the session is alive and the caller guarantees the value type matches.
        // SAFETY: a compression session is a `VTSession`, and both are alive here.
        let session = unsafe { &*(core::ptr::from_ref(&*self.session).cast::<VTSession>()) };
        // SAFETY: the session is alive and the caller guarantees the value type matches.
        let status = unsafe { VTSessionSetProperty(session, key, value) };

        if status != 0 {
            return Err(EncodeError::Property {
                property: name,
                status,
            });
        }

        Ok(())
    }
}

impl Drop for VideoToolboxEncoder {
    /// Tears the session down before the callback context it points at is freed.
    fn drop(&mut self) {
        // SAFETY: the session is alive here, and invalidating it guarantees no further
        // callback can run against the ref con that is about to be dropped.
        unsafe { self.session.invalidate() };
    }
}

/// Builds the attributes that make a pixel buffer IOSurface backed.
///
/// Without these, `CVPixelBufferCreate` returns plain heap memory. That still encodes,
/// but it cannot be bound as a Metal texture and it cannot reach the encoder without a
/// copy. Asking for Metal compatibility implies an IOSurface, which is what keeps both
/// paths zero copy.
fn surface_backed_attributes() -> CFRetained<objc2_core_foundation::CFDictionary> {
    use objc2_core_foundation::{
        CFDictionary, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    };

    // SAFETY: both entries are CoreFoundation constants with static lifetime, and
    // CFDictionaryCreate reads the two arrays without retaining the arrays themselves.
    unsafe {
        let mut keys = [core::ptr::from_ref(kCVPixelBufferMetalCompatibilityKey).cast::<c_void>()];
        let mut values = [kCFBooleanTrue.map_or(core::ptr::null(), |b| {
            core::ptr::from_ref(b).cast::<c_void>()
        })];

        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
        .expect("a one entry dictionary is always constructible")
    }
}

/// Builds the frame properties dictionary that forces a keyframe.
fn force_keyframe_properties() -> CFRetained<objc2_core_foundation::CFDictionary> {
    use objc2_core_foundation::{
        CFDictionary, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    };

    // SAFETY: both entries are CoreFoundation constants with static lifetime, and
    // CFDictionaryCreate reads the two arrays without retaining the arrays themselves.
    unsafe {
        let mut keys =
            [core::ptr::from_ref(kVTEncodeFrameOptionKey_ForceKeyFrame).cast::<c_void>()];
        let mut values = [kCFBooleanTrue.map_or(core::ptr::null(), |b| {
            core::ptr::from_ref(b).cast::<c_void>()
        })];

        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
        .expect("a one entry dictionary is always constructible")
    }
}

/// Receives encoded frames from VideoToolbox.
///
/// # Safety
///
/// Called by VideoToolbox on its own thread. `refcon` is the pointer handed to
/// `VTCompressionSessionCreate`, which points at a [`CallbackContext`] the encoder keeps
/// alive until after the session is invalidated.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    if status != 0 || sample_buffer.is_null() || refcon.is_null() {
        return;
    }

    // SAFETY: the encoder owns the boxed context and outlives every callback, because it
    // invalidates the session before dropping.
    let context = unsafe { &*refcon.cast::<CallbackContext>() };

    // SAFETY: VideoToolbox passes a live sample buffer that is valid for this call.
    let sample = unsafe { &*sample_buffer };

    let mut frame = context
        .spare
        .lock()
        .ok()
        .and_then(|mut spare| spare.pop())
        .unwrap_or_default();
    frame.reset();

    // Parameter sets ride along with every frame the encoder produces and are stripped
    // again unless this frame is one a decoder could start from. Repeating them on a cadence
    // is what lets a client join late or recover, rather than having exactly one chance at
    // the start of the session to understand the stream.
    let nth = context.frames.fetch_add(1, Ordering::Relaxed);
    let keep_parameter_sets = nth % PARAMETER_SET_INTERVAL == 0;

    // SAFETY: reading the buffer's contents is valid for the lifetime of this call.
    if unsafe { fill_from_sample(&mut frame, sample, context.codec, keep_parameter_sets) }.is_none()
    {
        return;
    }

    let _ = context.output.try_send(frame);
}

/// Converts one VideoToolbox sample buffer into an Annex B frame.
///
/// Returns `None` if the sample carries no data, which happens for the dropped frames
/// VideoToolbox reports when it falls behind.
///
/// `keep_parameter_sets` leaves the SPS and PPS in front of a frame that is not an IDR.
/// They cost a few dozen bytes and are what a client joining mid-session needs before it
/// can decode anything at all.
///
/// # Safety
///
/// `sample` must be a live sample buffer for the duration of the call.
unsafe fn fill_from_sample(
    frame: &mut EncodedFrame,
    sample: &CMSampleBuffer,
    codec: Codec,
    keep_parameter_sets: bool,
) -> Option<()> {
    // SAFETY: the sample is live, so its timestamp and buffers are readable.
    let pts = unsafe { sample.presentation_time_stamp() };
    frame.pts_us = if pts.timescale > 0 {
        (pts.value as i128 * 1_000_000 / i128::from(pts.timescale)) as u64
    } else {
        0
    };

    // SAFETY: the sample is live.
    let block = unsafe { sample.data_buffer() }?;

    let mut length = 0usize;
    let mut data: *mut core::ffi::c_char = null_mut();

    // SAFETY: both out pointers are valid and CoreMedia writes the contiguous range of
    // the block buffer into them.
    let status = unsafe { block.data_pointer(0, &mut length, core::ptr::null_mut(), &mut data) };
    if status != 0 || data.is_null() || length == 0 {
        return None;
    }

    // SAFETY: CoreMedia reported `length` readable bytes at `data`, valid while the
    // sample buffer is alive.
    let avcc = unsafe { core::slice::from_raw_parts(data.cast::<u8>(), length) };

    // SAFETY: the sample is live and carries a video format description.
    if let Some(description) = unsafe { sample.format_description() } {
        // SAFETY: the description belongs to a sample this session produced, in the codec it
        // was created for.
        unsafe { push_parameter_sets(frame, &description, codec) };
    }

    push_avcc_nals(frame, avcc);
    frame.is_idr = frame.slices.iter().any(|range| {
        frame
            .data
            .get(range.start + crate::encode::START_CODE.len())
            .is_some_and(|&byte| is_key_slice(codec, byte))
    });

    if !frame.is_idr && !keep_parameter_sets {
        strip_parameter_sets(frame, codec);
    }

    Some(())
}

/// Returns whether a NAL unit's first byte begins a parameter set.
///
/// H.264 has two, numbered 7 and 8. HEVC has three — a video parameter set at 32 ahead of the
/// sequence and picture ones at 33 and 34 — and reads its type from a different place.
fn is_parameter_set(codec: Codec, byte: u8) -> bool {
    match codec {
        Codec::Hevc => matches!((byte >> 1) & 0x3f, 32..=34),
        _ => matches!(byte & 0x1f, 7 | 8),
    }
}

/// Returns whether a NAL unit's first byte begins a slice a decoder can start from.
///
/// The two codecs put the type in different places. H.264 uses the low five bits, and type 5
/// is an IDR slice. HEVC uses bits one to six, and types 16 to 21 are all slices a decoder can
/// start from — an IDR is 19 or 20, and the others differ only in whether pictures before them
/// may be discarded.
fn is_key_slice(codec: Codec, byte: u8) -> bool {
    match codec {
        Codec::Hevc => (16..=21).contains(&((byte >> 1) & 0x3f)),
        _ => byte & 0x1f == 5,
    }
}

/// Prepends the parameter sets carried in a format description.
///
/// Two for H.264 and three for HEVC, which adds a video parameter set ahead of the other two.
/// They are emitted ahead of every frame and removed again unless the frame turns out to be a
/// keyframe, because a decoder needs them before the first slice it can start from and nowhere
/// else.
///
/// # Safety
///
/// `description` must describe a stream in `codec`.
unsafe fn push_parameter_sets(
    frame: &mut EncodedFrame,
    description: &objc2_core_media::CMFormatDescription,
    codec: Codec,
) {
    use objc2_core_media::{
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };

    let get = |index: usize, ptr: *mut *const u8, size: *mut usize, count: *mut usize| -> i32 {
        // SAFETY: both calls read a description of their own codec, which is what the caller
        // guarantees, and every pointer argument is null or a live local of the caller.
        unsafe {
            match codec {
                Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    description,
                    index,
                    ptr,
                    size,
                    count,
                    core::ptr::null_mut(),
                ),
                _ => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    description,
                    index,
                    ptr,
                    size,
                    count,
                    core::ptr::null_mut(),
                ),
            }
        }
    };

    let mut count = 0usize;

    // Querying index zero is how the parameter set count is discovered.
    if get(0, core::ptr::null_mut(), core::ptr::null_mut(), &mut count) != 0 {
        return;
    }

    for index in 0..count {
        let mut ptr: *const u8 = core::ptr::null();
        let mut size = 0usize;

        if get(index, &mut ptr, &mut size, core::ptr::null_mut()) != 0 || ptr.is_null() || size == 0
        {
            continue;
        }

        // SAFETY: CoreMedia reported `size` readable bytes at `ptr`.
        let nal = unsafe { core::slice::from_raw_parts(ptr, size) };

        // VideoToolbox writes no video usability information, so its sequence parameter sets
        // never say how far the stream reorders — and a decoder that is not told assumes the
        // worst its level allows. Measured against Media Foundation that was five frames of
        // delay on a stream that reorders nothing. The rewrite adds the missing sentence; a
        // set that already carries one, or a codec whose sets are not H.264's, is left alone.
        let rewritten = match codec {
            Codec::Hevc => None,
            _ => crate::encode::h264::declare_no_reordering(nal),
        };

        frame.push_nal(rewritten.as_deref().unwrap_or(nal));
    }
}

/// Splits a length-prefixed AVCC buffer into Annex B NAL units.
///
/// VideoToolbox emits four byte big-endian lengths, so anything that does not parse
/// cleanly is treated as the end of the buffer rather than guessed at.
fn push_avcc_nals(frame: &mut EncodedFrame, avcc: &[u8]) {
    let mut offset = 0usize;

    while offset + 4 <= avcc.len() {
        let len = u32::from_be_bytes([
            avcc[offset],
            avcc[offset + 1],
            avcc[offset + 2],
            avcc[offset + 3],
        ]) as usize;
        offset += 4;

        if len == 0 || offset + len > avcc.len() {
            break;
        }

        frame.push_nal(&avcc[offset..offset + len]);
        offset += len;
    }
}

/// Removes leading parameter set NAL units from a non-IDR frame.
fn strip_parameter_sets(frame: &mut EncodedFrame, codec: Codec) {
    let keep_from = frame
        .slices
        .iter()
        .position(|range| {
            frame
                .data
                .get(range.start + crate::encode::START_CODE.len())
                .is_some_and(|&byte| !is_parameter_set(codec, byte))
        })
        .unwrap_or(frame.slices.len());

    if keep_from == 0 {
        return;
    }

    let cut = frame
        .slices
        .get(keep_from)
        .map_or(frame.data.len(), |range| range.start);
    frame.data.drain(..cut);
    frame.slices.drain(..keep_from);
    for range in &mut frame.slices {
        range.start -= cut;
        range.end -= cut;
    }
}
