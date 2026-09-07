//! VideoToolbox H.264 decoder for macOS.
//!
//! The session is created lazily, because a decoder cannot exist until the stream has
//! delivered the parameter sets that describe it. Until the first keyframe arrives there
//! is nothing to decode, which is exactly the state a client is in when it joins a
//! session already in progress.
//!
//! Decoded pictures come back as `CVPixelBuffer`s. Those are IOSurface backed, so the
//! renderer can bind one directly as a Metal texture rather than copying pixels through
//! system memory.

use core::ffi::{c_int, c_void};
use core::ptr::{NonNull, null, null_mut};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use objc2_core_foundation::{CFRetained, CFString, CFType, kCFBooleanTrue};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTSession, VTSessionSetProperty, kVTDecompressionPropertyKey_RealTime,
};

use crate::decode::{
    DecodeError, HEVC_NAL_PPS, HEVC_NAL_SPS, HEVC_NAL_VPS, NAL_PPS, NAL_SPS, hevc_nal_type,
    nal_type, nal_units,
};
use crate::net::negotiate::Codec;

/// How many decoded pictures may queue up before the decoder thread blocks.
const OUTPUT_QUEUE_DEPTH: usize = 4;

/// Length prefix size used when handing NAL units to VideoToolbox.
const NAL_LENGTH_SIZE: c_int = 4;

/// A decoded picture, backed by an IOSurface the renderer can draw directly.
#[derive(Debug)]
pub struct DecodedFrame {
    /// Presentation timestamp in microseconds, as supplied to the decoder.
    pub pts_us: u64,
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    buffer: CFRetained<CVPixelBuffer>,
}

// SAFETY: `CVPixelBuffer` is a CoreFoundation type with atomic reference counting, and
// nothing here relies on the buffer staying on the thread that produced it. VideoToolbox
// is documented to hand decoded buffers to arbitrary threads.
unsafe impl Send for DecodedFrame {}

impl DecodedFrame {
    /// Returns the underlying pixel buffer for the renderer to bind.
    #[must_use]
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }

    /// Copies the luma plane into `out`, one row per line with padding removed.
    ///
    /// Intended for verification rather than the render path, which binds the buffer as
    /// a texture instead of reading it back.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::Picture`] if the buffer cannot be locked.
    ///
    /// # Panics
    ///
    /// Panics if CoreVideo reports a null plane for a buffer it locked.
    pub fn copy_luma(&self, out: &mut Vec<u8>) -> Result<(), DecodeError> {
        // SAFETY: the buffer is alive, and the lock is released before returning.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(&self.buffer, CVPixelBufferLockFlags::ReadOnly) };
        if status != 0 {
            return Err(DecodeError::Picture {
                reason: "could not lock the decoded buffer",
            });
        }

        out.clear();
        out.reserve(self.width as usize * self.height as usize);

        // SAFETY: the buffer is locked, so the luma plane is valid until it is unlocked,
        // and it covers `height` rows of `stride` bytes.
        unsafe {
            let base = CVPixelBufferGetBaseAddressOfPlane(&self.buffer, 0).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&self.buffer, 0);
            assert!(!base.is_null(), "a locked planar buffer has a luma plane");

            for row in 0..self.height as usize {
                let line = core::slice::from_raw_parts(base.add(row * stride), self.width as usize);
                out.extend_from_slice(line);
            }
        }

        // SAFETY: the buffer was locked immediately above with the same flags.
        unsafe {
            CVPixelBufferUnlockBaseAddress(&self.buffer, CVPixelBufferLockFlags::ReadOnly);
        }

        Ok(())
    }
}

/// State the decoder callback writes into, reached through the session's ref con.
#[derive(Debug)]
struct CallbackContext {
    output: SyncSender<DecodedFrame>,
    errors: Mutex<Vec<i32>>,
}

/// A VideoToolbox H.264 decompression session that follows the stream it is fed.
#[derive(Debug)]
pub struct VideoToolboxDecoder {
    /// Which codec this decoder was built for.
    ///
    /// Told rather than inferred. The two numbering schemes do not overlap in meaning, so a
    /// decoder that guessed wrong would read a parameter set as a slice and hand it over as a
    /// picture — which fails as a black window with nothing reporting an error.
    codec: Codec,
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    /// The parameter sets in the order the format description wants them.
    ///
    /// Two for H.264 and three for HEVC, whose video parameter set comes first.
    sets: Vec<Vec<u8>>,
    avcc: Vec<u8>,
    output: Receiver<DecodedFrame>,
    context: Box<CallbackContext>,
}

impl VideoToolboxDecoder {
    /// Creates a decoder with no session yet.
    ///
    /// The session appears once the stream supplies a sequence and picture parameter set,
    /// which arrive with the first keyframe.
    #[must_use]
    pub fn new(codec: Codec) -> Self {
        let (tx, rx) = sync_channel(OUTPUT_QUEUE_DEPTH);

        Self {
            codec,
            session: None,
            format: None,
            sets: Vec::new(),
            avcc: Vec::new(),
            output: rx,
            context: Box::new(CallbackContext {
                output: tx,
                errors: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Returns whether a session exists, meaning parameter sets have been seen.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.session.is_some()
    }

    /// Submits one Annex B frame for decoding.
    ///
    /// Parameter sets carried by the frame reconfigure the session when they change, so a
    /// stream that switches resolution mid-session is handled without the caller doing
    /// anything.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::NoParameterSets`] if the stream has not yet supplied the
    /// parameter sets a session needs, [`DecodeError::Bitstream`] if the frame carries no
    /// decodable NAL units, and [`DecodeError::Decode`] if VideoToolbox rejects it.
    pub fn decode(&mut self, annexb: &[u8], pts_us: u64) -> Result<(), DecodeError> {
        self.absorb_parameter_sets(annexb)?;

        let Some(session) = self.session.as_ref() else {
            return Err(DecodeError::NoParameterSets);
        };
        let Some(format) = self.format.as_ref() else {
            return Err(DecodeError::NoParameterSets);
        };

        self.avcc.clear();
        for nal in nal_units(annexb) {
            if self.is_parameter_set(nal) {
                continue;
            }
            self.avcc
                .extend_from_slice(&(nal.len() as u32).to_be_bytes());
            self.avcc.extend_from_slice(nal);
        }

        if self.avcc.is_empty() {
            return Err(DecodeError::Bitstream {
                reason: "frame carries no slice NAL units",
            });
        }

        let block = create_block_buffer(&self.avcc)?;
        let sample = create_sample_buffer(&block, format, self.avcc.len(), pts_us)?;

        // SAFETY: the session and sample buffer are both alive, and no source ref con is
        // used so a null pointer is correct there.
        let status = unsafe {
            session.decode_frame(&sample, VTDecodeFrameFlags::empty(), null_mut(), null_mut())
        };

        if status != 0 {
            return Err(DecodeError::Decode { status });
        }

        Ok(())
    }

    /// Waits up to `timeout` for the next decoded picture.
    ///
    /// Ownership passes to the caller so the picture can be handed to another thread for
    /// display. The underlying buffer is reference counted, so this is a retain rather
    /// than a copy of the pixels.
    pub fn poll(&mut self, timeout: Duration) -> Option<DecodedFrame> {
        self.output.recv_timeout(timeout).ok()
    }

    /// Returns and clears any decode errors reported asynchronously by VideoToolbox.
    ///
    /// Frames fail on the decoder's own thread, so a failure cannot be returned from
    /// [`Self::decode`]. Draining this is how the receive loop notices a stream it cannot
    /// decode rather than silently showing nothing.
    pub fn take_errors(&mut self) -> Vec<i32> {
        self.context
            .errors
            .lock()
            .map(|mut e| core::mem::take(&mut *e))
            .unwrap_or_default()
    }

    /// Rebuilds the session if the frame carries parameter sets that differ from the
    /// current ones.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::SessionCreate`] if VideoToolbox will not describe or decode
    /// the stream.
    fn absorb_parameter_sets(&mut self, annexb: &[u8]) -> Result<(), DecodeError> {
        // In the order the format description wants them: for HEVC that is video, sequence,
        // picture, and getting it wrong is refused by CoreMedia rather than silently wrong.
        let wanted: &[u8] = match self.codec {
            Codec::Hevc => &[HEVC_NAL_VPS, HEVC_NAL_SPS, HEVC_NAL_PPS],
            _ => &[NAL_SPS, NAL_PPS],
        };

        let mut found: Vec<Option<&[u8]>> = vec![None; wanted.len()];

        for nal in nal_units(annexb) {
            let Some(kind) = self.type_of(nal) else {
                continue;
            };

            if let Some(at) = wanted.iter().position(|&want| want == kind) {
                found[at] = Some(nal);
            }
        }

        let Some(sets) = found.into_iter().collect::<Option<Vec<_>>>() else {
            // Not every set is here. A frame between keyframes carries none of them, which is
            // the ordinary case rather than a problem.
            return Ok(());
        };

        if self.session.is_some()
            && self.sets.len() == sets.len()
            && self
                .sets
                .iter()
                .zip(&sets)
                .all(|(held, seen)| held.as_slice() == *seen)
        {
            return Ok(());
        }

        self.sets = sets.iter().map(|set| set.to_vec()).collect();

        let format = create_format_description(self.codec, &self.sets)?;
        let session = create_session(&format, &self.context)?;

        self.format = Some(format);
        self.session = Some(session);

        Ok(())
    }

    /// Returns a NAL unit's type in this decoder's codec.
    fn type_of(&self, nal: &[u8]) -> Option<u8> {
        match self.codec {
            Codec::Hevc => hevc_nal_type(nal),
            _ => nal_type(nal),
        }
    }

    /// Returns whether a NAL unit is a parameter set rather than picture data.
    fn is_parameter_set(&self, nal: &[u8]) -> bool {
        match (self.codec, self.type_of(nal)) {
            (Codec::Hevc, Some(kind)) => (HEVC_NAL_VPS..=HEVC_NAL_PPS).contains(&kind),
            (_, Some(kind)) => matches!(kind, NAL_SPS | NAL_PPS),
            _ => false,
        }
    }
}

impl Drop for VideoToolboxDecoder {
    /// Tears the session down before the callback context it points at is freed.
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: the session is alive here, and invalidating it guarantees no
            // further callback can run against the ref con about to be dropped.
            unsafe { session.invalidate() };
        }
    }
}

/// Builds a format description from a sequence and picture parameter set.
///
/// # Errors
///
/// Returns [`DecodeError::SessionCreate`] if CoreMedia cannot parse the parameter sets.
fn create_format_description(
    codec: Codec,
    sets: &[Vec<u8>],
) -> Result<CFRetained<CMFormatDescription>, DecodeError> {
    let pointers: Vec<NonNull<u8>> = sets
        .iter()
        .map(|set| NonNull::from(set.as_slice()).cast::<u8>())
        .collect();
    let sizes: Vec<usize> = sets.iter().map(Vec::len).collect();
    let mut raw: *const CMFormatDescription = null();

    // SAFETY: every parameter set outlives the call, both arrays have the declared length,
    // and CoreMedia writes a retained description into `raw` on success.
    let status = unsafe {
        match codec {
            Codec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                sets.len(),
                NonNull::from(pointers.as_slice()).cast(),
                NonNull::from(sizes.as_slice()).cast(),
                NAL_LENGTH_SIZE,
                None,
                NonNull::from(&mut raw),
            ),
            _ => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                sets.len(),
                NonNull::from(pointers.as_slice()).cast(),
                NonNull::from(sizes.as_slice()).cast(),
                NAL_LENGTH_SIZE,
                NonNull::from(&mut raw),
            ),
        }
    };

    if status != 0 || raw.is_null() {
        return Err(DecodeError::SessionCreate {
            reason: "CMVideoFormatDescriptionCreateFromParameterSets",
            status,
        });
    }

    // SAFETY: CoreMedia created the description, so ownership transfers here.
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw.cast_mut())) })
}

/// Creates a decompression session for the given stream description.
///
/// # Errors
///
/// Returns [`DecodeError::SessionCreate`] if VideoToolbox will not decode the stream.
fn create_session(
    format: &CMFormatDescription,
    context: &CallbackContext,
) -> Result<CFRetained<VTDecompressionSession>, DecodeError> {
    let record = VTDecompressionOutputCallbackRecord {
        decompressionOutputCallback: Some(output_callback),
        decompressionOutputRefCon: core::ptr::from_ref(context).cast::<c_void>().cast_mut(),
    };

    let mut raw: *mut VTDecompressionSession = null_mut();

    // SAFETY: the format description is alive, the callback matches the expected
    // signature, and the ref con points at a context the decoder keeps alive for at least
    // as long as the session.
    let status = unsafe {
        VTDecompressionSession::create(None, format, None, None, &record, NonNull::from(&mut raw))
    };

    if status != 0 || raw.is_null() {
        return Err(DecodeError::SessionCreate {
            reason: "VTDecompressionSessionCreate",
            status,
        });
    }

    // SAFETY: VideoToolbox created the session, so ownership transfers here.
    let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

    // SAFETY: a decompression session is a `VTSession`, and RealTime takes a boolean.
    unsafe {
        let vt_session = &*(core::ptr::from_ref(&*session).cast::<VTSession>());
        let value = kCFBooleanTrue.map(|b| &*(core::ptr::from_ref(b).cast::<CFType>()));
        let key: &CFString = kVTDecompressionPropertyKey_RealTime;
        VTSessionSetProperty(vt_session, key, value);
    }

    Ok(session)
}

/// Wraps AVCC bytes in a block buffer CoreMedia owns.
///
/// The bytes are copied rather than referenced, so the caller is free to reuse its
/// buffer as soon as this returns even though decoding is asynchronous.
///
/// # Errors
///
/// Returns [`DecodeError::Picture`] if CoreMedia cannot allocate or fill the buffer.
fn create_block_buffer(avcc: &[u8]) -> Result<CFRetained<CMBlockBuffer>, DecodeError> {
    let mut raw: *mut CMBlockBuffer = null_mut();

    // SAFETY: passing a null memory block asks CoreMedia to allocate, and
    // `AssureMemoryNow` makes it do so before returning.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            null_mut(),
            avcc.len(),
            None,
            null(),
            0,
            avcc.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut raw),
        )
    };

    if status != 0 || raw.is_null() {
        return Err(DecodeError::Picture {
            reason: "could not allocate a block buffer",
        });
    }

    // SAFETY: CoreMedia created the buffer, so ownership transfers here.
    let block = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

    // SAFETY: the block was allocated with exactly `avcc.len()` bytes.
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(
            NonNull::from(avcc).cast::<c_void>(),
            &block,
            0,
            avcc.len(),
        )
    };

    if status != 0 {
        return Err(DecodeError::Picture {
            reason: "could not fill the block buffer",
        });
    }

    Ok(block)
}

/// Wraps a block buffer as a timed sample the decoder can accept.
///
/// # Errors
///
/// Returns [`DecodeError::Picture`] if CoreMedia cannot create the sample.
fn create_sample_buffer(
    block: &CMBlockBuffer,
    format: &CMFormatDescription,
    size: usize,
    pts_us: u64,
) -> Result<CFRetained<CMSampleBuffer>, DecodeError> {
    let timing = CMSampleTimingInfo {
        duration: CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags::empty(),
            epoch: 0,
        },
        presentationTimeStamp: CMTime {
            value: pts_us as i64,
            timescale: 1_000_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        },
        decodeTimeStamp: CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags::empty(),
            epoch: 0,
        },
    };
    let sizes = [size];
    let mut raw: *mut CMSampleBuffer = null_mut();

    // SAFETY: the block buffer and format description outlive the call, and the timing
    // and size arrays each hold the single entry declared.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(block),
            Some(format),
            1,
            1,
            &timing,
            1,
            sizes.as_ptr(),
            NonNull::from(&mut raw),
        )
    };

    if status != 0 || raw.is_null() {
        return Err(DecodeError::Picture {
            reason: "could not create a sample buffer",
        });
    }

    // SAFETY: CoreMedia created the sample, so ownership transfers here.
    Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) })
}

/// Receives decoded pictures from VideoToolbox.
///
/// # Safety
///
/// Called by VideoToolbox on its own thread. `refcon` is the pointer handed to
/// `VTDecompressionSessionCreate`, which points at a [`CallbackContext`] the decoder
/// keeps alive until after the session is invalidated.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    pts: CMTime,
    _duration: CMTime,
) {
    if refcon.is_null() {
        return;
    }

    // SAFETY: the decoder owns the boxed context and outlives every callback, because it
    // invalidates the session before dropping.
    let context = unsafe { &*refcon.cast::<CallbackContext>() };

    if status != 0 {
        if let Ok(mut errors) = context.errors.lock() {
            errors.push(status);
        }
        return;
    }

    if image_buffer.is_null() {
        return;
    }

    // SAFETY: VideoToolbox hands over a live image buffer; retaining it keeps the picture
    // valid after this call returns.
    let buffer = unsafe {
        let pixels = image_buffer.cast::<CVPixelBuffer>();
        CFRetained::retain(NonNull::new_unchecked(pixels))
    };

    let (width, height) = (
        CVPixelBufferGetWidth(&buffer) as u32,
        CVPixelBufferGetHeight(&buffer) as u32,
    );

    let pts_us = if pts.timescale > 0 {
        (pts.value as i128 * 1_000_000 / i128::from(pts.timescale)) as u64
    } else {
        0
    };

    let _ = context.output.try_send(DecodedFrame {
        pts_us,
        width,
        height,
        buffer,
    });
}
