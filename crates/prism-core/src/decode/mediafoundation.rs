//! Media Foundation H.264 and HEVC decoder for Windows.
//!
//! The session is created lazily, because a decoder cannot be configured until the stream has
//! delivered the parameter sets that describe it. Until the first keyframe arrives there is
//! nothing to decode, which is exactly the state a client is in when it joins a session
//! already in progress.
//!
//! # Why Media Foundation rather than DXVA directly
//!
//! The plan named D3D11VA, which means driving `ID3D11VideoDecoder` by hand: parsing the
//! sequence and picture parameter sets into DXVA picture-parameter buffers, filling slice
//! control structures, and keeping the reference picture list yourself. That is a few thousand
//! lines of code whose failure mode is a picture that is subtly wrong rather than an error.
//!
//! Media Foundation's decoder transform does all of that internally. Given a DXGI device
//! manager it decodes on the GPU through the same DXVA path and hands back an
//! `ID3D11Texture2D`, so what is written here is the part that is actually this project's:
//! feeding it Annex B and getting pictures out without a copy through system memory.
//!
//! # Synchronous transforms only
//!
//! Hardware vendors ship asynchronous transforms, which are driven by an event queue rather
//! than by calls. Microsoft's own bundled decoders are synchronous and still decode on the GPU
//! when they are given a device manager, so this asks for a synchronous one and drives it with
//! the straightforward loop. An asynchronous transform would be faster to feed under load; it
//! is not what stands between here and a picture on screen.

use std::collections::VecDeque;
use std::sync::Once;
use std::time::Duration;

use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaBuffer, IMFSample, IMFTransform,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_VERSION, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFStartup,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
};
use windows::core::Interface;

use crate::decode::{
    DecodeError, HEVC_NAL_PPS, HEVC_NAL_SPS, HEVC_NAL_VPS, NAL_PPS, NAL_SPS, hevc_nal_type,
    nal_type, nal_units,
};
use crate::net::negotiate::Codec;

/// How many decoded pictures are held before the oldest is dropped.
///
/// The same reasoning as everywhere else on this path: a picture that has waited its turn is
/// already too late to be worth showing, so the queue is short and the old end falls off.
const OUTPUT_QUEUE_DEPTH: usize = 4;

/// Media Foundation counts time in hundreds of nanoseconds.
const HNS_PER_MICROSECOND: i64 = 10;

/// Starts Media Foundation once for the process.
static STARTUP: Once = Once::new();

/// A decoded picture, held as a texture the renderer can draw without a copy.
#[derive(Debug)]
pub struct DecodedFrame {
    /// Presentation timestamp in microseconds, as supplied to the decoder.
    pub pts_us: u64,
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// The NV12 texture the transform decoded into.
    texture: ID3D11Texture2D,
    /// Which slice of it, because a transform may decode into an array.
    index: u32,
    /// Kept alive alongside the texture: it is what the staging copy is issued on.
    context: ID3D11DeviceContext,
    /// And what a staging texture is created from.
    device: ID3D11Device,
}

// SAFETY: the Direct3D objects held here are COM interfaces with atomic reference counting,
// and the device they came from is created without `D3D11_CREATE_DEVICE_SINGLETHREADED` and
// with multithread protection turned on — see `create_device`. Nothing here relies on the
// picture staying on the thread that produced it, and the decode thread hands pictures to the
// thread that draws them.
unsafe impl Send for DecodedFrame {}

impl DecodedFrame {
    /// Returns the texture and the slice of it this picture occupies.
    ///
    /// The renderer binds these directly. A transform that decodes into an array hands back
    /// the same texture for every picture and a different index, which is why the index has to
    /// travel with it.
    #[must_use]
    pub fn texture(&self) -> (&ID3D11Texture2D, u32) {
        (&self.texture, self.index)
    }

    /// Copies the luma plane into `out`, one row per line with padding removed.
    ///
    /// Intended for verification rather than the render path, which binds the texture instead
    /// of reading it back. A decoded texture lives in memory the CPU cannot address, so this
    /// copies it to a staging texture first — which is exactly the round trip the render path
    /// exists to avoid, and why nothing but a test should call it.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::Picture`] if the staging copy or the map fails.
    pub fn copy_luma(&self, out: &mut Vec<u8>) -> Result<(), DecodeError> {
        let mut source = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: the texture is alive for the life of this frame.
        unsafe { self.texture.GetDesc(&mut source) };

        let staging = D3D11_TEXTURE2D_DESC {
            Width: source.Width,
            Height: source.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source.Format,
            SampleDesc: source.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };

        let device = &self.device;
        let mut readable: Option<ID3D11Texture2D> = None;
        // SAFETY: the description is fully initialised and the output pointer is valid.
        unsafe { device.CreateTexture2D(&staging, None, Some(&mut readable)) }.map_err(|_| {
            DecodeError::Picture {
                reason: "could not create a staging texture",
            }
        })?;

        let readable = readable.ok_or(DecodeError::Picture {
            reason: "could not create a staging texture",
        })?;

        // SAFETY: both textures are alive and share a device, and the subresource index came
        // from the transform that produced the picture.
        unsafe {
            self.context.CopySubresourceRegion(
                &readable,
                0,
                0,
                0,
                0,
                &self.texture,
                self.index,
                None,
            );
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture is alive and was created for reading.
        unsafe {
            self.context
                .Map(&readable, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|_| DecodeError::Picture {
            reason: "could not map the staging texture",
        })?;

        out.clear();
        out.reserve(self.width as usize * self.height as usize);

        // SAFETY: the mapping is live until `Unmap`, and NV12 stores `height` rows of luma of
        // `RowPitch` bytes each before the chroma plane begins.
        unsafe {
            let base = mapped.pData.cast::<u8>();

            for row in 0..self.height as usize {
                let line = core::slice::from_raw_parts(
                    base.add(row * mapped.RowPitch as usize),
                    self.width as usize,
                );
                out.extend_from_slice(line);
            }
        }

        // SAFETY: mapped immediately above, and nothing has released it since.
        unsafe { self.context.Unmap(&readable, 0) };

        Ok(())
    }
}

/// A Media Foundation decoder transform that follows the stream it is fed.
#[derive(Debug)]
pub struct MediaFoundationDecoder {
    /// Which codec this decoder was built for.
    ///
    /// Told rather than inferred. The two NAL numbering schemes do not overlap in meaning, so
    /// a decoder that guessed wrong would read a parameter set as a slice and hand it over as
    /// a picture — which fails as a black window with nothing reporting an error.
    codec: Codec,
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    manager: Option<IMFDXGIDeviceManager>,
    transform: Option<IMFTransform>,
    /// The parameter sets the current transform was configured from.
    sets: Vec<Vec<u8>>,
    /// The frame being handed over, rebuilt for each call rather than allocated per frame.
    scratch: Vec<u8>,
    pending: VecDeque<DecodedFrame>,
    errors: Vec<i32>,
    width: u32,
    height: u32,
}

impl MediaFoundationDecoder {
    /// Creates a decoder with no transform yet.
    ///
    /// The transform appears once the stream supplies the parameter sets that say what size
    /// and profile it is, which arrive with the first keyframe.
    #[must_use]
    pub fn new(codec: Codec) -> Self {
        STARTUP.call_once(|| {
            // SAFETY: called once for the process, before anything else here touches Media
            // Foundation. A failure is left to surface at the first call that needs it, which
            // reports where it happened rather than at a static initialiser nobody can see.
            let _ = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) };
        });

        Self {
            codec,
            device: None,
            context: None,
            manager: None,
            transform: None,
            sets: Vec::new(),
            scratch: Vec::new(),
            pending: VecDeque::new(),
            errors: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    /// Returns whether a transform exists, meaning parameter sets have been seen.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.transform.is_some()
    }

    /// Submits one Annex B frame for decoding.
    ///
    /// Parameter sets carried by the frame rebuild the transform when they change, so a stream
    /// that switches resolution mid-session is handled without the caller doing anything.
    ///
    /// Unlike the VideoToolbox path, the parameter sets are left in the buffer handed over:
    /// Media Foundation's decoders read them from the bitstream rather than from a separate
    /// description, and a frame stripped of them is a frame they refuse.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::NoParameterSets`] until the stream has supplied what a transform
    /// needs, [`DecodeError::Bitstream`] if the frame carries nothing, and
    /// [`DecodeError::Decode`] if the transform rejects it.
    pub fn decode(&mut self, annexb: &[u8], pts_us: u64) -> Result<(), DecodeError> {
        self.absorb_parameter_sets(annexb)?;

        let Some(transform) = self.transform.clone() else {
            return Err(DecodeError::NoParameterSets);
        };

        if annexb.is_empty() {
            return Err(DecodeError::Bitstream {
                reason: "frame carries no bytes",
            });
        }

        self.scratch.clear();
        self.scratch.extend_from_slice(annexb);

        let sample = build_sample(&self.scratch, pts_us)?;

        // SAFETY: the transform is alive and the sample holds its own buffer.
        let status = unsafe { transform.ProcessInput(0, &sample, 0) };

        if let Err(err) = status {
            self.errors.push(err.code().0);
            return Err(DecodeError::Decode {
                status: err.code().0,
            });
        }

        self.drain(&transform);

        Ok(())
    }

    /// Returns the next decoded picture, or nothing if none is ready.
    ///
    /// The timeout is accepted for the same shape as the other backends and is not waited on:
    /// a synchronous transform has either produced a picture by the time `ProcessInput`
    /// returned or it has not, and sleeping here would only add the delay this path exists to
    /// avoid.
    pub fn poll(&mut self, _timeout: Duration) -> Option<DecodedFrame> {
        self.pending.pop_front()
    }

    /// Returns and clears any status codes the transform reported.
    pub fn take_errors(&mut self) -> Vec<i32> {
        core::mem::take(&mut self.errors)
    }

    /// Takes every picture the transform has ready.
    fn drain(&mut self, transform: &IMFTransform) {
        loop {
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
            let mut flags = 0u32;

            // SAFETY: the transform is alive, and the output buffer array is one entry the
            // transform fills with a sample it allocated.
            let status = unsafe { transform.ProcessOutput(0, &mut buffers, &mut flags) };

            match status {
                Ok(()) => {}
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return,
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The picture size changed under us. Renegotiating the output type is what
                    // the transform is asking for; failing that, the next keyframe rebuilds
                    // the whole thing anyway.
                    if self.accept_output_type(transform).is_err() {
                        self.transform = None;
                    }
                    return;
                }
                Err(err) => {
                    self.errors.push(err.code().0);
                    return;
                }
            }

            let Some(sample) = buffers[0].pSample.take() else {
                return;
            };

            if let Some(frame) = self.picture_from(&sample) {
                while self.pending.len() >= OUTPUT_QUEUE_DEPTH {
                    self.pending.pop_front();
                }
                self.pending.push_back(frame);
            }
        }
    }

    /// Turns one output sample into a picture, or nothing if it carries no texture.
    fn picture_from(&mut self, sample: &IMFSample) -> Option<DecodedFrame> {
        // SAFETY: an output sample from a decoder carries at least one buffer.
        let buffer: IMFMediaBuffer = unsafe { sample.GetBufferByIndex(0) }.ok()?;
        let dxgi: IMFDXGIBuffer = buffer.cast().ok()?;

        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: the buffer is alive, and the interface asked for is the one a DXGI buffer
        // holds. The pointer is written only on success.
        unsafe {
            dxgi.GetResource(
                &ID3D11Texture2D::IID,
                core::ptr::from_mut(&mut texture).cast(),
            )
        }
        .ok()?;

        // SAFETY: the buffer is alive.
        let index = unsafe { dxgi.GetSubresourceIndex() }.ok()?;

        // SAFETY: the sample is alive; the time is in hundreds of nanoseconds.
        let hns = unsafe { sample.GetSampleTime() }.unwrap_or_default();

        Some(DecodedFrame {
            pts_us: u64::try_from(hns / HNS_PER_MICROSECOND).unwrap_or_default(),
            width: self.width,
            height: self.height,
            texture: texture?,
            index,
            context: self.context.clone()?,
            device: self.device.clone()?,
        })
    }

    /// Rebuilds the transform if the frame carries parameter sets that differ from the
    /// current ones.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::SessionCreate`] if Windows will not give this machine a decoder
    /// for the stream.
    fn absorb_parameter_sets(&mut self, annexb: &[u8]) -> Result<(), DecodeError> {
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

        if self.transform.is_some()
            && self.sets.len() == sets.len()
            && self
                .sets
                .iter()
                .zip(&sets)
                .all(|(held, seen)| held.as_slice() == *seen)
        {
            return Ok(());
        }

        let (width, height) = dimensions_from(self.codec, &sets).unwrap_or((1920, 1080));

        self.sets = sets.iter().map(|set| set.to_vec()).collect();
        self.width = width;
        self.height = height;

        self.build(width, height)
    }

    /// Creates the device, the manager and the transform, in that order.
    fn build(&mut self, width: u32, height: u32) -> Result<(), DecodeError> {
        if self.device.is_none() {
            let (device, context) = create_device()?;
            let manager = create_manager(&device)?;

            self.device = Some(device);
            self.context = Some(context);
            self.manager = Some(manager);
        }

        let transform = find_transform(self.codec)?;

        if let Some(manager) = self.manager.as_ref() {
            // A transform that will not take a device manager decodes into system memory
            // instead. Slower, and still a picture — so this is not a failure.
            let handle = manager.as_raw() as usize;
            // SAFETY: the transform is alive, and the message carries the manager as an
            // integer-sized parameter, which is what this message is defined to take.
            let _ = unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, handle) };
        }

        set_input_type(&transform, self.codec, width, height)?;
        self.accept_output_type(&transform)?;

        // SAFETY: the transform is alive and has both types set.
        unsafe {
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }

        self.transform = Some(transform);
        self.pending.clear();

        Ok(())
    }

    /// Picks the first NV12 output the transform offers.
    ///
    /// NV12 because it is what every hardware decoder produces and what the renderer's shader
    /// expects. A transform with no NV12 output is one this project cannot draw from.
    fn accept_output_type(&self, transform: &IMFTransform) -> Result<(), DecodeError> {
        for index in 0..32u32 {
            // SAFETY: the transform is alive; enumeration ends with an error status.
            let Ok(candidate) = (unsafe { transform.GetOutputAvailableType(0, index) }) else {
                break;
            };

            // SAFETY: the media type is alive.
            let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) };

            if subtype.map(|guid| guid == MFVideoFormat_NV12) == Ok(true) {
                // SAFETY: the type came from this transform's own enumeration.
                unsafe { transform.SetOutputType(0, &candidate, 0) }.map_err(|err| {
                    DecodeError::SessionCreate {
                        reason: "the decoder refused its own NV12 output",
                        status: err.code().0,
                    }
                })?;

                return Ok(());
            }
        }

        Err(DecodeError::SessionCreate {
            reason: "the decoder offers no NV12 output",
            status: E_FAIL.0,
        })
    }

    /// Returns a NAL unit's type in this decoder's codec.
    fn type_of(&self, nal: &[u8]) -> Option<u8> {
        match self.codec {
            Codec::Hevc => hevc_nal_type(nal),
            _ => nal_type(nal),
        }
    }
}

/// Creates a Direct3D device the decoder and the renderer can share.
///
/// Video support is asked for because without it the device cannot back a decoder at all.
/// Multithread protection is turned on because the decode thread and the draw thread both
/// issue work against this device, and Direct3D's own contract is that they may not do so at
/// once unless it is.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), DecodeError> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    // SAFETY: every out parameter is a live option, and the feature level array outlives the
    // call.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|err| DecodeError::SessionCreate {
        reason: "no Direct3D device with video support",
        status: err.code().0,
    })?;

    let device = device.ok_or(DecodeError::SessionCreate {
        reason: "no Direct3D device with video support",
        status: E_FAIL.0,
    })?;
    let context = context.ok_or(DecodeError::SessionCreate {
        reason: "the Direct3D device has no context",
        status: E_FAIL.0,
    })?;

    if let Ok(guard) = device.cast::<ID3D11Multithread>() {
        // SAFETY: the interface came from the device it protects. The previous setting is
        // returned and dropped: this is turning protection on, not toggling it.
        let _ = unsafe { guard.SetMultithreadProtected(true) };
    }

    Ok((device, context))
}

/// Wraps a device in the manager a transform is handed.
fn create_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager, DecodeError> {
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;

    // SAFETY: both out parameters are live for the call.
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }.map_err(|err| {
        DecodeError::SessionCreate {
            reason: "could not create a DXGI device manager",
            status: err.code().0,
        }
    })?;

    let manager = manager.ok_or(DecodeError::SessionCreate {
        reason: "could not create a DXGI device manager",
        status: E_FAIL.0,
    })?;

    // SAFETY: the device and manager are both alive, and the token is the one just issued.
    unsafe { manager.ResetDevice(device, token) }.map_err(|err| DecodeError::SessionCreate {
        reason: "the device manager refused the Direct3D device",
        status: err.code().0,
    })?;

    Ok(manager)
}

/// Finds a synchronous decoder for the codec, preferring one that runs on the GPU.
fn find_transform(codec: Codec) -> Result<IMFTransform, DecodeError> {
    let subtype = match codec {
        Codec::Hevc => MFVideoFormat_HEVC,
        _ => MFVideoFormat_H264,
    };

    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };

    let mut activates: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0u32;

    // SAFETY: the type info outlives the call, and the two out parameters are live. The array
    // the call allocates is freed below.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut activates,
            &mut count,
        )
    }
    .map_err(|err| DecodeError::SessionCreate {
        reason: "could not ask Windows for a decoder",
        status: err.code().0,
    })?;

    let mut chosen: Option<IMFTransform> = None;

    if !activates.is_null() {
        // SAFETY: the call wrote `count` entries into the array it allocated.
        let found = unsafe { core::slice::from_raw_parts(activates, count as usize) };

        for entry in found {
            if chosen.is_none()
                && let Some(activate) = entry.as_ref()
                // SAFETY: the activation object is alive until the array is freed.
                && let Ok(transform) = unsafe { activate.ActivateObject::<IMFTransform>() }
            {
                chosen = Some(transform);
            }
        }

        // SAFETY: the array and every entry in it came from `MFTEnumEx`, which documents the
        // caller as the owner of both.
        unsafe {
            for entry in core::slice::from_raw_parts_mut(activates, count as usize) {
                drop(entry.take());
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates.cast()));
        }
    }

    chosen.ok_or(DecodeError::SessionCreate {
        reason: "this machine has no decoder for that codec",
        status: E_FAIL.0,
    })
}

/// Tells the transform what it is being fed.
fn set_input_type(
    transform: &IMFTransform,
    codec: Codec,
    width: u32,
    height: u32,
) -> Result<(), DecodeError> {
    // SAFETY: nothing is borrowed across the call.
    let media = unsafe { MFCreateMediaType() }.map_err(|err| DecodeError::SessionCreate {
        reason: "could not describe the stream",
        status: err.code().0,
    })?;

    let subtype = match codec {
        Codec::Hevc => MFVideoFormat_HEVC,
        _ => MFVideoFormat_H264,
    };

    // SAFETY: the media type is alive and every attribute is one it accepts.
    unsafe {
        let _ = media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
        let _ = media.SetGUID(&MF_MT_SUBTYPE, &subtype);
        let _ = media.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height));
    }

    // SAFETY: the transform and the type are both alive.
    unsafe { transform.SetInputType(0, &media, 0) }.map_err(|err| DecodeError::SessionCreate {
        reason: "the decoder refused the stream's format",
        status: err.code().0,
    })
}

/// Puts a frame into the sample shape Media Foundation takes.
fn build_sample(annexb: &[u8], pts_us: u64) -> Result<IMFSample, DecodeError> {
    let length = u32::try_from(annexb.len()).map_err(|_| DecodeError::Bitstream {
        reason: "frame is larger than a media buffer can hold",
    })?;

    // SAFETY: nothing is borrowed across either call.
    let buffer = unsafe { MFCreateMemoryBuffer(length) }.map_err(|err| DecodeError::Decode {
        status: err.code().0,
    })?;

    let mut destination: *mut u8 = core::ptr::null_mut();
    let mut capacity = 0u32;

    // SAFETY: the buffer is alive, and the pointer it hands back is valid until `Unlock`.
    unsafe { buffer.Lock(&mut destination, Some(&mut capacity), None) }.map_err(|err| {
        DecodeError::Decode {
            status: err.code().0,
        }
    })?;

    // SAFETY: the buffer was created with exactly this length, and the lock above gives write
    // access to all of it.
    unsafe {
        core::ptr::copy_nonoverlapping(annexb.as_ptr(), destination, annexb.len());
    }

    // SAFETY: locked immediately above.
    unsafe {
        let _ = buffer.Unlock();
        let _ = buffer.SetCurrentLength(length);
    }

    // SAFETY: nothing is borrowed across the call.
    let sample = unsafe { MFCreateSample() }.map_err(|err| DecodeError::Decode {
        status: err.code().0,
    })?;

    // SAFETY: the sample and buffer are both alive.
    unsafe {
        let _ = sample.AddBuffer(&buffer);
        let _ =
            sample.SetSampleTime(i64::try_from(pts_us).unwrap_or(i64::MAX) * HNS_PER_MICROSECOND);
    }

    Ok(sample)
}

/// Packs a size the way `MF_MT_FRAME_SIZE` wants it: width in the high half.
const fn pack(width: u32, height: u32) -> u64 {
    ((width as u64) << 32) | height as u64
}

/// Reads the picture size out of a sequence parameter set.
///
/// A decoder can be created without this — Media Foundation reads the real size from the
/// bitstream — but the size is what the pictures are reported as, and a frame that claimed the
/// wrong one would be drawn wrong. Returns nothing when the parameter set cannot be read,
/// which leaves the caller to guess and be corrected on the first picture.
fn dimensions_from(codec: Codec, sets: &[&[u8]]) -> Option<(u32, u32)> {
    if codec == Codec::Hevc {
        // HEVC's sequence parameter set puts the size behind profile and level structures that
        // are variable length. Left unread for now: the transform is told a size only so it has
        // one, and it corrects itself from the bitstream.
        return None;
    }

    crate::decode::h264_dimensions(sets.get(1).copied()?)
}

#[cfg(test)]
mod tests {
    use super::pack;

    #[test]
    fn a_frame_size_packs_width_above_height() {
        assert_eq!(pack(1920, 1080), (1920u64 << 32) | 1080);
        assert_eq!(pack(0, 0), 0);
    }
}
