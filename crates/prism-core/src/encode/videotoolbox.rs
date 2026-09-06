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
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use objc2_core_foundation::{CFRetained, CFString, CFType, kCFBooleanFalse, kCFBooleanTrue};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags, kCMVideoCodecType_H264};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferMetalCompatibilityKey,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxH264SliceBytes,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
};

use crate::encode::{EncodeError, EncodedFrame, EncoderConfig};

/// Four character code for the NV12 pixel format VideoToolbox encodes natively.
const NV12: u32 = u32::from_be_bytes(*b"420v");

/// Keyframe interval, in frames, requested from the encoder.
///
/// Deliberately enormous. Periodic IDR frames are a bitrate spike and a latency spike,
/// and this pipeline recovers from loss through long-term reference invalidation instead
/// (M4). Until that lands, the host asks for an IDR explicitly when it needs one.
const KEYFRAME_INTERVAL: i32 = 100_000;

/// Status VideoToolbox returns when an encoder does not implement a property.
///
/// `kVTPropertyNotSupportedErr`. Apple Silicon's hardware H.264 encoder returns this for
/// the slice size limit, so slicing has to be treated as a capability rather than a
/// requirement.
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
    output: SyncSender<EncodedFrame>,
    spare: Mutex<Vec<EncodedFrame>>,
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
            output: tx,
            spare: Mutex::new(Vec::new()),
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
                kCMVideoCodecType_H264,
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
        };
        encoder.slicing = encoder.configure()?;

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
    /// Returns as soon as VideoToolbox accepts the frame; the encoded result arrives
    /// through [`Self::poll`].
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] if VideoToolbox rejects the frame.
    pub fn encode(
        &mut self,
        frame: &Nv12Frame,
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
                &*(CFRetained::as_ptr(&frame.buffer)
                    .as_ptr()
                    .cast::<CVImageBuffer>()),
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
            self.set_bool(
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
                Some(kVTProfileLevel_H264_High_AutoLevel.as_ref()),
            )?;

            if self.config.max_slice_bytes == 0 {
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

    // SAFETY: reading the buffer's contents is valid for the lifetime of this call.
    if unsafe { fill_from_sample(&mut frame, sample) }.is_none() {
        return;
    }

    let _ = context.output.try_send(frame);
}

/// Converts one VideoToolbox sample buffer into an Annex B frame.
///
/// Returns `None` if the sample carries no data, which happens for the dropped frames
/// VideoToolbox reports when it falls behind.
///
/// # Safety
///
/// `sample` must be a live sample buffer for the duration of the call.
unsafe fn fill_from_sample(frame: &mut EncodedFrame, sample: &CMSampleBuffer) -> Option<()> {
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
        // SAFETY: the description belongs to an H.264 sample produced by this session.
        unsafe { push_parameter_sets(frame, &description) };
    }

    push_avcc_nals(frame, avcc);
    frame.is_idr = frame.slices.iter().any(|range| {
        frame
            .data
            .get(range.start + crate::encode::START_CODE.len())
            .is_some_and(|&b| b & 0x1f == 5)
    });

    if !frame.is_idr {
        strip_parameter_sets(frame);
    }

    Some(())
}

/// Prepends the SPS and PPS carried in a format description.
///
/// They are emitted ahead of every frame and removed again unless the frame turns out to
/// be an IDR, because a decoder needs them before the first slice it can start from and
/// nowhere else.
///
/// # Safety
///
/// `description` must describe an H.264 stream.
unsafe fn push_parameter_sets(
    frame: &mut EncodedFrame,
    description: &objc2_core_media::CMFormatDescription,
) {
    use objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex;

    let mut count = 0usize;

    // SAFETY: querying index zero is how the parameter set count is discovered.
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description,
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            &mut count,
            core::ptr::null_mut(),
        )
    };
    if status != 0 {
        return;
    }

    for index in 0..count {
        let mut ptr: *const u8 = core::ptr::null();
        let mut size = 0usize;

        // SAFETY: `index` is below the reported count and both out pointers are valid.
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                description,
                index,
                &mut ptr,
                &mut size,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        if status != 0 || ptr.is_null() || size == 0 {
            continue;
        }

        // SAFETY: CoreMedia reported `size` readable bytes at `ptr`.
        frame.push_nal(unsafe { core::slice::from_raw_parts(ptr, size) });
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
fn strip_parameter_sets(frame: &mut EncodedFrame) {
    let keep_from = frame
        .slices
        .iter()
        .position(|range| {
            frame
                .data
                .get(range.start + crate::encode::START_CODE.len())
                .is_some_and(|&b| !matches!(b & 0x1f, 7 | 8))
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
