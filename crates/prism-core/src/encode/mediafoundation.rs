//! Media Foundation H.264 encoder: what a Windows machine without NVENC encodes with.
//!
//! NVENC is the only encoder this project drives directly on Windows, and it exists on NVIDIA's
//! cards alone. Until this was written, a machine with an AMD or Intel GPU, a Snapdragon, or no
//! GPU at all could turn sharing on and then fail every session the moment somebody connected.
//! Media Foundation is the one interface every vendor's encoder is also published through, so
//! it is what is asked when NVENC is not there.
//!
//! # Two ways a picture reaches the transform
//!
//! A vendor's hardware transform takes the picture as the texture it already is, through a
//! DXGI device manager, and nothing leaves the GPU. That is tried first.
//!
//! Microsoft's own software transform cannot, so the picture is copied to a staging texture
//! and read into system memory. That is the one place in this project where a captured frame
//! is brought down to the CPU, against the rule the rest of the pipeline is built on, and it
//! is here on purpose: the machines that reach it have no hardware encoder, and what the rule
//! would give them instead is not a faster path but no picture at all. A virtual machine is
//! the everyday case. It is said on standard error when it happens, so that nobody measures
//! it and takes the numbers for the real ones.
//!
//! # Asynchronous and synchronous transforms behind one call
//!
//! Hardware transforms are asynchronous: they say when they want a frame and when they have
//! one, through an event queue. The software one is synchronous and is simply called. Both are
//! driven here as "hand over a frame, then take whatever is finished", which is what the
//! pump's loop already is. The event queue is polled against a deadline rather than waited on
//! without one, because a driver that never answers would otherwise hold the host's thread for
//! ever, and with it the machine's ability to stop sharing.
//!
//! # What has not been run
//!
//! The hardware path is written against the documented contract and has not been exercised:
//! the machine this was developed against has no hardware encoder. A transform that fails
//! anywhere while it is being set up is stepped over for the next, and the software transform
//! is the floor underneath all of them.

use std::mem::ManuallyDrop;
use std::sync::Once;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_FAIL, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQualityVsSpeed,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, METransformHaveOutput,
    METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{CoIncrementMTAUsage, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::Interface;

use crate::decode::{NAL_IDR, NAL_SPS, nal_type, nal_units};
use crate::encode::{EncodeError, EncodedFrame, EncoderConfig};
use crate::net::negotiate::Codec;

/// Media Foundation counts time in hundreds of nanoseconds.
const HNS_PER_MICROSECOND: i64 = 10;

/// Hundreds of nanoseconds in a second, for turning a frame rate into a frame's duration.
const HNS_PER_SECOND: i64 = 10_000_000;

/// How many input samples are kept, and handed over in turn.
///
/// A transform may hold on to a sample after it has been given one — an asynchronous one
/// certainly does, until the frame is finished. With no reordering there is only ever one
/// frame inside it, so four is room to spare, and what it buys is that the picture being
/// converted for the next turn is never the one still being read for this one.
const RING: usize = 4;

/// How long a transform is given to say something before the session gives up on it.
///
/// Generous beside a frame time because what it guards against is not slowness but silence: a
/// driver that has stopped answering altogether. Without a limit that is a host thread which
/// never returns, and a machine that can no longer be unshared.
const PATIENCE: Duration = Duration::from_secs(2);

/// How long to sleep between looks at an empty event queue.
const LOOK_EVERY: Duration = Duration::from_micros(500);

/// How many frames pass between keyframes nobody asked for.
///
/// An hour at sixty a second, which is to say never. Every frame here is a reference and a
/// keyframe is sent when the client says it has lost its place, so a periodic one is only a
/// frame several times the size of its neighbours arriving on a schedule. Not the largest
/// number the property holds, because a driver that checks its range should not be given a
/// reason to refuse this and fall back to a keyframe every second or two.
const FRAMES_BETWEEN_KEYFRAMES: u32 = 216_000;

/// Starts Media Foundation once for the process.
static STARTUP: Once = Once::new();

/// How a picture gets from the conversion target into the transform.
enum Feed {
    /// As a texture, through the device manager. Nothing leaves the GPU.
    Gpu {
        /// The textures the picture is copied into, each wrapped once in the sample that
        /// carries it.
        ring: Vec<(ID3D11Texture2D, IMFSample)>,
        /// Kept alive for as long as the transform holds the pointer it was given.
        _manager: IMFDXGIDeviceManager,
    },
    /// Through system memory, for a transform that cannot be handed a texture.
    Cpu {
        /// Where the picture is copied so that it can be mapped.
        staging: ID3D11Texture2D,
        /// The buffers the mapped picture is copied into, each in the sample that carries it.
        ring: Vec<(IMFMediaBuffer, IMFSample)>,
    },
}

/// An H.264 encoder behind Media Foundation, on the GPU where the machine has one.
pub struct MediaFoundationEncoder {
    transform: IMFTransform,
    /// The event queue, for a transform that is driven by one.
    events: Option<IMFMediaEventGenerator>,
    /// The codec's own settings, for a transform that exposes them.
    tuning: Option<ICodecAPI>,
    feed: Feed,
    next: usize,
    context: ID3D11DeviceContext,
    source: ID3D11Texture2D,
    /// The sample output is written into, when the transform does not bring its own.
    output: Option<IMFSample>,
    /// The parameter sets as the output type states them, for a transform that leaves them
    /// out of the stream.
    header: Vec<u8>,
    /// Frames the transform has asked for and not yet been given.
    asked: u32,
    /// Frames the transform has finished and not yet handed over.
    ready: u32,
    /// A keyframe that was asked for on a turn that could not submit a frame.
    owed_keyframe: bool,
    /// The first frame's timestamp, which the transform's own times are counted from.
    origin: Option<u64>,
    config: EncoderConfig,
    frame: EncodedFrame,
}

impl MediaFoundationEncoder {
    /// Opens an encoder for the picture that is drawn into `texture`.
    ///
    /// Every hardware transform the machine lists is tried before the software one, and one
    /// that fails anywhere in being set up is stepped over for the next.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] if the codec is not H.264, or if no transform
    /// on this machine can be set up for the picture.
    pub fn new(
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        config: EncoderConfig,
    ) -> Result<Self, EncodeError> {
        if config.codec != Codec::H264 {
            return Err(EncodeError::SessionCreate {
                reason: "only H.264 is encoded through Media Foundation",
                status: E_FAIL.0,
            });
        }

        STARTUP.call_once(|| {
            // Listing transforms and activating one are COM calls, and the thread a host runs
            // on has not joined an apartment. The capture happens to have made the process one
            // by the time this runs, through the same call; asking again here is what stops an
            // encoder depending on something a different file does first. Never given back,
            // because there is no moment in a process that shares its screen when it is safe to
            // say nothing will want COM again.
            //
            // SAFETY: both are called once for the process and take nothing that could dangle.
            // A failure in either is left to surface at the first call that needs it, which
            // reports where it happened.
            unsafe {
                let _ = CoIncrementMTAUsage();
                let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
            }
        });

        // The transform works on threads of its own against the device the capture and the
        // conversion are also using, and Direct3D allows that only with this turned on.
        if let Ok(guard) = device.cast::<ID3D11Multithread>() {
            // SAFETY: the interface came from the device it protects. What it returns is the
            // previous setting, which is of no interest: this turns protection on.
            let _ = unsafe { guard.SetMultithreadProtected(true) };
        }

        let mut refused = EncodeError::SessionCreate {
            reason: "this machine has no H.264 encoder",
            status: E_FAIL.0,
        };

        for transform in transforms(MFT_ENUM_FLAG_HARDWARE) {
            match Self::open(device, texture, config, transform, true) {
                Ok(encoder) => return Ok(encoder),
                Err(err) => refused = err,
            }
        }

        for transform in transforms(MFT_ENUM_FLAG_SYNCMFT) {
            match Self::open(device, texture, config, transform, false) {
                Ok(encoder) => {
                    eprintln!(
                        "host: no hardware encoder on this machine; encoding on the processor, \
                         through system memory"
                    );

                    return Ok(encoder);
                }
                Err(err) => refused = err,
            }
        }

        Err(refused)
    }

    /// Sets one transform up, on the GPU or through memory.
    fn open(
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
        config: EncoderConfig,
        transform: IMFTransform,
        on_gpu: bool,
    ) -> Result<Self, EncodeError> {
        let asynchronous = unlock_if_asynchronous(&transform);

        // A synchronous transform is driven by calls and an asynchronous one by its queue;
        // each is asked for under the flag that is supposed to bring only its own kind, and
        // one that turns up under the other's is stepped over rather than driven wrongly.
        if asynchronous != on_gpu {
            return Err(EncodeError::SessionCreate {
                reason: "the transform is not driven the way it was listed as being",
                status: E_FAIL.0,
            });
        }

        let manager = if on_gpu {
            Some(attach_device(&transform, device)?)
        } else {
            None
        };

        let tuning = transform.cast::<ICodecAPI>().ok();

        if let Some(tuning) = tuning.as_ref() {
            tune(tuning, config, on_gpu);
        }

        // SAFETY: the transform is alive, and the attribute store it returns belongs to it.
        unsafe {
            if let Ok(attributes) = transform.GetAttributes() {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }

        // The output first: an encoder decides what input it can take from what it has been
        // asked to produce, and refuses an input type offered before it knows.
        set_output_type(&transform, config)?;
        set_input_type(&transform, config, on_gpu)?;

        let events = if asynchronous {
            Some(transform.cast::<IMFMediaEventGenerator>().map_err(|err| {
                EncodeError::SessionCreate {
                    reason: "an asynchronous transform has no event queue",
                    status: err.code().0,
                }
            })?)
        } else {
            None
        };

        let feed = match manager {
            Some(manager) => Feed::Gpu {
                ring: texture_ring(device, source)?,
                _manager: manager,
            },
            None => Feed::Cpu {
                staging: staging_for(device, source)?,
                ring: memory_ring(config)?,
            },
        };

        let output = output_sample(&transform, config)?;
        let header = sequence_header(&transform);

        // SAFETY: the transform is alive and has both types set.
        unsafe {
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }

        // SAFETY: the device is alive.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(|err| EncodeError::SessionCreate {
                reason: "the Direct3D device has no context",
                status: err.code().0,
            })?;

        Ok(Self {
            transform,
            events,
            tuning,
            feed,
            next: 0,
            context,
            source: source.clone(),
            output,
            header,
            asked: 0,
            ready: 0,
            owed_keyframe: false,
            origin: None,
            config,
            frame: EncodedFrame::default(),
        })
    }

    /// Whether the picture stays on the GPU on its way into the encoder.
    ///
    /// False is the software transform, and with it the copy through system memory that the
    /// module's note explains.
    #[must_use]
    pub fn on_gpu(&self) -> bool {
        matches!(self.feed, Feed::Gpu { .. })
    }

    /// Changes the target bitrate of a running encoder.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Property`] if the transform exposes no settings or refuses the
    /// new rate. The session carries on at the old one.
    pub fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        if bitrate_bps == 0 || bitrate_bps == self.config.bitrate_bps {
            return Ok(());
        }

        let refused = |status: i32| EncodeError::Property {
            property: "mean bitrate",
            status,
        };

        let tuning = self.tuning.as_ref().ok_or(refused(E_FAIL.0))?;

        set(
            tuning,
            &CODECAPI_AVEncCommonMeanBitRate,
            &number(bitrate_bps),
        )
        .map_err(|err| refused(err.code().0))?;

        self.config.bitrate_bps = bitrate_bps;

        Ok(())
    }

    /// Encodes the picture that is in the texture this encoder was opened on.
    ///
    /// Returns nothing on a turn where the transform took the frame and has not finished one
    /// — which a hardware transform is allowed to do, and which the caller treats as a frame
    /// that was not sent rather than as a failure.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::InputBuffer`] if the picture cannot be put into a sample, and
    /// [`EncodeError::Encode`] if the transform refuses the frame, fails to hand one back, or
    /// stops answering.
    pub fn encode(
        &mut self,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Option<&EncodedFrame>, EncodeError> {
        self.owed_keyframe |= force_idr;

        if self.events.is_some() {
            // Whatever has been said since last turn, and then — if it has neither asked for a
            // frame nor finished one — a wait until it does one or the other.
            self.listen(|encoder| encoder.asked > 0 || encoder.ready > 0)?;
        }

        // An asynchronous transform that has a frame finished and has not asked for another
        // is waiting for that one to be taken, so this turn takes it and submits nothing.
        // The keyframe somebody asked for is still owed, and goes in with the next frame.
        let submit = self.events.is_none() || self.asked > 0;

        if submit {
            self.submit(pts_us)?;
        }

        if self.events.is_some() && self.ready == 0 {
            self.listen(|encoder| encoder.ready > 0 || encoder.asked > 0)?;

            if self.ready == 0 {
                return Ok(None);
            }
        }

        if self.collect()? {
            Ok(Some(&self.frame))
        } else {
            Ok(None)
        }
    }

    /// Hands the transform the picture, with a keyframe asked for if one is owed.
    fn submit(&mut self, pts_us: u64) -> Result<(), EncodeError> {
        let slot = self.next % RING;
        let sample = self.fill(slot)?;

        // Counted from the first frame rather than from whenever the clock began. A transform
        // paces itself by these, and one handed the time since 1970 in hundreds of nanoseconds
        // is being asked to do arithmetic a long way from where anybody tested it.
        let origin = *self.origin.get_or_insert(pts_us);
        let since = i64::try_from(pts_us.saturating_sub(origin)).unwrap_or(i64::MAX);

        let time = since.saturating_mul(HNS_PER_MICROSECOND);
        let duration = HNS_PER_SECOND / i64::from(self.config.fps.max(1));

        // SAFETY: the sample is alive.
        unsafe {
            let _ = sample.SetSampleTime(time);
            let _ = sample.SetSampleDuration(duration);
        }

        if self.owed_keyframe
            && let Some(tuning) = self.tuning.as_ref()
        {
            // A refusal is not reported. The frame still goes in, and a client that does not
            // get the keyframe it asked for asks again.
            let _ = set(tuning, &CODECAPI_AVEncVideoForceKeyFrame, &number(1));
        }

        // SAFETY: the transform and the sample are both alive.
        let given = unsafe { self.transform.ProcessInput(0, &sample, 0) };

        match given {
            Ok(()) => {}
            // A synchronous transform that is full wants what it has finished taken first.
            // Whatever comes out is this turn's frame, and the picture waits for the next.
            Err(err) if err.code() == MF_E_NOTACCEPTING && self.events.is_none() => {
                return Ok(());
            }
            Err(err) => {
                return Err(EncodeError::Encode {
                    status: err.code().0,
                });
            }
        }

        self.next = self.next.wrapping_add(1);
        self.asked = self.asked.saturating_sub(1);
        self.owed_keyframe = false;

        Ok(())
    }

    /// Puts the picture into the sample at `slot`, and returns that sample.
    fn fill(&self, slot: usize) -> Result<IMFSample, EncodeError> {
        match &self.feed {
            Feed::Gpu { ring, .. } => {
                let (texture, sample) = &ring[slot];

                // SAFETY: both textures are alive, share a device, and were created from one
                // description, which is what a whole-resource copy requires.
                unsafe { self.context.CopyResource(texture, &self.source) };

                Ok(sample.clone())
            }
            Feed::Cpu { staging, ring } => {
                let (buffer, sample) = &ring[slot];

                // SAFETY: as above; the staging texture differs from the source only in how
                // it may be used, which a copy does not look at.
                unsafe { self.context.CopyResource(staging, &self.source) };

                read_into(&self.context, staging, buffer, self.config)?;

                Ok(sample.clone())
            }
        }
    }

    /// Reads the event queue until `done` says so, or until the transform has been silent for
    /// longer than [`PATIENCE`].
    ///
    /// Everything already queued is always read first, whether or not `done` is satisfied
    /// before any of it is, so the two counters are current when this returns.
    fn listen(&mut self, done: impl Fn(&Self) -> bool) -> Result<(), EncodeError> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };

        let started = Instant::now();

        loop {
            // SAFETY: the queue is alive, and asked not to wait.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    // SAFETY: the event is alive.
                    let kind = unsafe { event.GetType() }.unwrap_or_default();

                    if kind == METransformNeedInput.0 as u32 {
                        self.asked += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        self.ready += 1;
                    }
                }
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if done(self) {
                        return Ok(());
                    }

                    if started.elapsed() >= PATIENCE {
                        return Err(EncodeError::Encode {
                            status: err.code().0,
                        });
                    }

                    std::thread::sleep(LOOK_EVERY);
                }
                Err(err) => {
                    return Err(EncodeError::Encode {
                        status: err.code().0,
                    });
                }
            }
        }
    }

    /// Takes one finished frame out of the transform and into [`Self::frame`].
    ///
    /// Returns whether there was one.
    fn collect(&mut self) -> Result<bool, EncodeError> {
        // Twice at most: once more after the transform has been allowed to restate its output
        // type, which is what it is asking for when it reports a stream change.
        for _ in 0..2 {
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
            buffers[0].pSample = ManuallyDrop::new(self.output.clone());

            let mut flags = 0u32;

            // SAFETY: the transform is alive. The one entry carries either the sample this
            // encoder allocated for the purpose or nothing, for a transform that brings its
            // own, and both that and the event collection are taken back out below.
            let status = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut flags) };

            let sample = buffers[0].pSample.take();
            drop(buffers[0].pEvents.take());

            match status {
                Ok(()) => {}
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    set_output_type(&self.transform, self.config)?;
                    self.header = sequence_header(&self.transform);

                    continue;
                }
                Err(err) => {
                    return Err(EncodeError::Encode {
                        status: err.code().0,
                    });
                }
            }

            self.ready = self.ready.saturating_sub(1);

            let Some(sample) = sample else {
                return Ok(false);
            };

            self.take(&sample)?;

            return Ok(!self.frame.slices.is_empty());
        }

        Ok(false)
    }

    /// Splits a finished sample into NAL units in [`Self::frame`].
    fn take(&mut self, sample: &IMFSample) -> Result<(), EncodeError> {
        let failed = |err: windows::core::Error| EncodeError::Encode {
            status: err.code().0,
        };

        // SAFETY: the sample is alive. With the one buffer an encoder's output has, this is
        // that buffer rather than a copy of it.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(failed)?;

        // SAFETY: the sample is alive; the time is in hundreds of nanoseconds.
        let time = unsafe { sample.GetSampleTime() }.unwrap_or_default();

        let mut bytes: *mut u8 = core::ptr::null_mut();
        let mut length = 0u32;

        // SAFETY: the buffer is alive, and the pointer it hands back is valid until `Unlock`.
        unsafe { buffer.Lock(&mut bytes, None, Some(&mut length)) }.map_err(failed)?;

        self.frame.reset();
        self.frame.pts_us = self.origin.unwrap_or_default()
            + u64::try_from(time / HNS_PER_MICROSECOND).unwrap_or_default();

        if !bytes.is_null() {
            // SAFETY: locked immediately above, which gives read access to `length` bytes
            // until the unlock below.
            let stream = unsafe { core::slice::from_raw_parts(bytes, length as usize) };

            let keyframe = nal_units(stream).any(|nal| nal_type(nal) == Some(NAL_IDR));
            let carries_sets = nal_units(stream).any(|nal| nal_type(nal) == Some(NAL_SPS));

            // A client joining, or one that has lost its place, can decode nothing until it
            // has the parameter sets, and some transforms state them once on the output type
            // and never in the stream. Those get them put in front of every keyframe.
            if keyframe && !carries_sets {
                for nal in nal_units(&self.header) {
                    self.frame.push_nal(nal);
                }
            }

            for nal in nal_units(stream) {
                self.frame.push_nal(nal);
            }

            self.frame.is_idr = keyframe;
        }

        // SAFETY: locked above, and nothing has released it since. The length is put back to
        // nothing so that a sample this encoder owns is empty when it is next handed over.
        unsafe {
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(0);
        }

        Ok(())
    }
}

impl core::fmt::Debug for MediaFoundationEncoder {
    /// Describes the encoder without reaching into COM objects.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MediaFoundationEncoder")
            .field("on_gpu", &self.on_gpu())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Lists the machine's H.264 encoders of one kind, best first.
///
/// Nothing at all when Windows will not answer, which reads to the caller as a machine with
/// none of that kind — and that is what it amounts to.
fn transforms(kind: MFT_ENUM_FLAG) -> Vec<IMFTransform> {
    let produces = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut activates: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0u32;

    // SAFETY: the type info outlives the call, and the two out parameters are live. The array
    // the call allocates is freed below.
    let listed = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            kind | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&produces),
            &mut activates,
            &mut count,
        )
    };

    let mut found = Vec::new();

    if listed.is_err() || activates.is_null() {
        return found;
    }

    // SAFETY: the call wrote `count` entries into the array it allocated, and documents the
    // caller as the owner of the array and of every entry in it.
    unsafe {
        for entry in core::slice::from_raw_parts_mut(activates, count as usize) {
            if let Some(activate) = entry.take()
                && let Ok(transform) = activate.ActivateObject::<IMFTransform>()
            {
                found.push(transform);
            }
        }

        CoTaskMemFree(Some(activates.cast()));
    }

    found
}

/// Unlocks a transform that is driven by events, and says whether it is one.
///
/// An asynchronous transform refuses every call until it has been told the caller knows what
/// it is dealing with, which is all the unlock is.
fn unlock_if_asynchronous(transform: &IMFTransform) -> bool {
    // SAFETY: the transform is alive, and the attribute store it hands back belongs to it.
    unsafe {
        let Ok(attributes) = transform.GetAttributes() else {
            return false;
        };

        let asynchronous = attributes
            .GetUINT32(&MF_TRANSFORM_ASYNC)
            .is_ok_and(|value| value != 0);

        if asynchronous {
            let _ = attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
        }

        asynchronous
    }
}

/// Gives a transform the device the picture is on, so that it can be handed textures.
///
/// # Errors
///
/// Returns [`EncodeError::SessionCreate`] if the transform does not take Direct3D 11 textures
/// or refuses the device. Either makes it a transform this path cannot feed.
fn attach_device(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> Result<IMFDXGIDeviceManager, EncodeError> {
    let refused = |reason: &'static str, status: i32| EncodeError::SessionCreate { reason, status };

    // SAFETY: the transform is alive, and the attribute store it hands back belongs to it.
    let aware = unsafe {
        transform
            .GetAttributes()
            .and_then(|attributes| attributes.GetUINT32(&MF_SA_D3D11_AWARE))
            .is_ok_and(|value| value != 0)
    };

    if !aware {
        return Err(refused(
            "the hardware transform does not take Direct3D 11 textures",
            E_FAIL.0,
        ));
    }

    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;

    // SAFETY: both out parameters are live for the call.
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
        .map_err(|err| refused("could not create a DXGI device manager", err.code().0))?;

    let manager = manager.ok_or(refused("could not create a DXGI device manager", E_FAIL.0))?;

    // SAFETY: the device and manager are both alive, and the token is the one just issued.
    unsafe { manager.ResetDevice(device, token) }.map_err(|err| {
        refused(
            "the device manager refused the Direct3D device",
            err.code().0,
        )
    })?;

    // SAFETY: the transform is alive, and the message carries the manager as an integer-sized
    // parameter, which is what this message is defined to take. The manager is kept by the
    // encoder for as long as the transform is.
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .map_err(|err| {
            refused(
                "the hardware transform refused the device manager",
                err.code().0,
            )
        })?;

    Ok(manager)
}

/// Asks the codec for what every encoder in this project is asked for.
///
/// Constant bitrate, no B-frames, no lookahead, and no keyframes nobody asked for. Each is
/// asked separately and a refusal is ignored: which of these a given vendor's transform
/// exposes is not knowable ahead of time, and an encoder that honours some of them is a
/// picture, where one that was abandoned for refusing any is not.
///
/// A single frame's VBV window, which the other backends set, is deliberately not asked for
/// here. What a transform does when it cannot fit a frame in one differs by vendor — the
/// software one is documented as free to drop the frame — and nothing here can see which. A
/// large frame now and then is the smaller harm.
fn tune(tuning: &ICodecAPI, config: EncoderConfig, on_gpu: bool) {
    let _ = set(
        tuning,
        &CODECAPI_AVEncCommonRateControlMode,
        &number(eAVEncCommonRateControlMode_CBR.0 as u32),
    );
    let _ = set(
        tuning,
        &CODECAPI_AVEncCommonMeanBitRate,
        &number(config.bitrate_bps),
    );
    let _ = set(tuning, &CODECAPI_AVLowLatencyMode, &truth());
    let _ = set(tuning, &CODECAPI_AVEncMPVDefaultBPictureCount, &number(0));
    let _ = set(
        tuning,
        &CODECAPI_AVEncMPVGOPSize,
        &number(FRAMES_BETWEEN_KEYFRAMES),
    );

    if !on_gpu {
        // As fast as it goes. A processor encoding a screen in real time has no time to spend
        // on anything else, and a frame that arrives late is worth less than a coarser one
        // that arrives.
        let _ = set(tuning, &CODECAPI_AVEncCommonQualityVsSpeed, &number(0));
    }
}

/// Sets one codec property.
fn set(
    tuning: &ICodecAPI,
    property: &windows::core::GUID,
    value: &VARIANT,
) -> windows::core::Result<()> {
    // SAFETY: the interface is alive, and both pointers are to values that outlive the call.
    unsafe { tuning.SetValue(property, value) }
}

/// A number, the way a codec property takes one.
fn number(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();

    // SAFETY: a zeroed variant is an empty one, and writing the tag together with the member
    // it names is how one is made to hold a value. Nothing here owns memory, so nothing needs
    // clearing afterwards.
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;

        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }

    variant
}

/// Yes, the way a codec property takes one.
fn truth() -> VARIANT {
    let mut variant = VARIANT::default();

    // SAFETY: as in `number`.
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;

        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = VARIANT_TRUE;
    }

    variant
}

/// A media type with what the input and the output have in common.
fn picture_type(config: EncoderConfig) -> Result<IMFMediaType, EncodeError> {
    // SAFETY: nothing is borrowed across the call.
    let media = unsafe { MFCreateMediaType() }.map_err(|err| EncodeError::SessionCreate {
        reason: "could not describe the picture",
        status: err.code().0,
    })?;

    // SAFETY: the media type is alive and every attribute is one it accepts.
    unsafe {
        let _ = media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
        let _ = media.SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height));
        let _ = media.SetUINT64(&MF_MT_FRAME_RATE, pack(config.fps.max(1), 1));
        let _ = media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1));
        let _ = media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
    }

    Ok(media)
}

/// Tells the transform what to produce.
fn set_output_type(transform: &IMFTransform, config: EncoderConfig) -> Result<(), EncodeError> {
    let media = picture_type(config)?;

    // Main rather than High: every hardware encoder and every decoder this meets has it, and
    // what High adds is worth little at the rates a screen is sent at.
    //
    // SAFETY: the media type is alive and every attribute is one it accepts.
    unsafe {
        let _ = media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264);
        let _ = media.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps);
        let _ = media.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
    }

    // SAFETY: the transform and the type are both alive.
    unsafe { transform.SetOutputType(0, &media, 0) }.map_err(|err| EncodeError::SessionCreate {
        reason: "the encoder refused to produce H.264 at this size and rate",
        status: err.code().0,
    })
}

/// Tells the transform what it is being fed.
fn set_input_type(
    transform: &IMFTransform,
    config: EncoderConfig,
    on_gpu: bool,
) -> Result<(), EncodeError> {
    let media = picture_type(config)?;

    // SAFETY: the media type is alive and every attribute is one it accepts.
    unsafe {
        let _ = media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);

        // Through memory the stride is stated rather than left to be assumed, because rows of
        // exactly the picture's width are what gets written, and a transform that guessed a
        // padded one would read a sheared picture without anything failing. A texture carries
        // its own, and is not told one it might then be held to.
        if !on_gpu {
            let _ = media.SetUINT32(&MF_MT_DEFAULT_STRIDE, config.width);
        }
    }

    // SAFETY: the transform and the type are both alive.
    unsafe { transform.SetInputType(0, &media, 0) }.map_err(|err| EncodeError::SessionCreate {
        reason: "the encoder refused an NV12 picture at this size",
        status: err.code().0,
    })
}

/// The parameter sets, as the transform states them on its output type.
///
/// Empty for a transform that states none there, which is one that puts them in the stream.
fn sequence_header(transform: &IMFTransform) -> Vec<u8> {
    // SAFETY: the transform is alive and has an output type set.
    let Ok(media) = (unsafe { transform.GetOutputCurrentType(0) }) else {
        return Vec::new();
    };

    // SAFETY: the media type is alive.
    let Ok(length) = (unsafe { media.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }) else {
        return Vec::new();
    };

    let mut header = vec![0u8; length as usize];

    // SAFETY: the media type is alive, and the buffer is as long as the blob was just said to
    // be.
    if unsafe { media.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None) }.is_err() {
        header.clear();
    }

    header
}

/// The textures a picture is copied into on its way to a hardware transform.
///
/// Each is wrapped once, here, in the sample that carries it, so that a running encoder
/// allocates nothing.
fn texture_ring(
    device: &ID3D11Device,
    source: &ID3D11Texture2D,
) -> Result<Vec<(ID3D11Texture2D, IMFSample)>, EncodeError> {
    let mut like = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: the texture is alive and the out parameter is a live local.
    unsafe { source.GetDesc(&mut like) };

    let mut ring = Vec::with_capacity(RING);

    for _ in 0..RING {
        let texture = texture_like(device, &like)?;

        // SAFETY: the texture is alive, and the identifier is the one for the interface it is
        // being passed as.
        let buffer =
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false) }
                .map_err(|err| EncodeError::SessionCreate {
                    reason: "could not wrap a texture for the encoder",
                    status: err.code().0,
                })?;

        // Some transforms read how much of a buffer is in use and take a texture that says
        // none of it is as an empty frame.
        if let Ok(flat) = buffer.cast::<IMF2DBuffer>() {
            // SAFETY: both interfaces are alive, and the length is the buffer's own.
            unsafe {
                if let Ok(length) = flat.GetContiguousLength() {
                    let _ = buffer.SetCurrentLength(length);
                }
            }
        }

        ring.push((texture, sample_around(&buffer)?));
    }

    Ok(ring)
}

/// A texture the picture can be copied to and then read from.
fn staging_for(
    device: &ID3D11Device,
    source: &ID3D11Texture2D,
) -> Result<ID3D11Texture2D, EncodeError> {
    let mut like = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: the texture is alive and the out parameter is a live local.
    unsafe { source.GetDesc(&mut like) };

    like.Usage = D3D11_USAGE_STAGING;
    like.BindFlags = 0;
    like.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    like.MiscFlags = 0;

    texture_like(device, &like)
}

/// Allocates one texture from a description.
fn texture_like(
    device: &ID3D11Device,
    like: &D3D11_TEXTURE2D_DESC,
) -> Result<ID3D11Texture2D, EncodeError> {
    let mut texture: Option<ID3D11Texture2D> = None;

    // SAFETY: the description is fully initialised and the output parameter is a live local
    // that is only read when the call reports success.
    unsafe { device.CreateTexture2D(like, None, Some(&mut texture)) }.map_err(|err| {
        EncodeError::SessionCreate {
            reason: "could not allocate a texture for the encoder's input",
            status: err.code().0,
        }
    })?;

    texture.ok_or(EncodeError::InputBuffer {
        reason: "Direct3D reported success but produced no texture",
    })
}

/// The buffers a picture is read into on its way to the software transform.
fn memory_ring(config: EncoderConfig) -> Result<Vec<(IMFMediaBuffer, IMFSample)>, EncodeError> {
    let mut ring = Vec::with_capacity(RING);

    for _ in 0..RING {
        // SAFETY: nothing is borrowed across the call.
        let buffer = unsafe { MFCreateMemoryBuffer(nv12_bytes(config)) }.map_err(|err| {
            EncodeError::SessionCreate {
                reason: "could not allocate a buffer for the encoder's input",
                status: err.code().0,
            }
        })?;

        let sample = sample_around(&buffer)?;

        ring.push((buffer, sample));
    }

    Ok(ring)
}

/// A sample holding one buffer.
fn sample_around(buffer: &IMFMediaBuffer) -> Result<IMFSample, EncodeError> {
    // SAFETY: nothing is borrowed across the call.
    let sample = unsafe { MFCreateSample() }.map_err(|err| EncodeError::SessionCreate {
        reason: "could not create a sample",
        status: err.code().0,
    })?;

    // SAFETY: the sample and the buffer are both alive.
    unsafe { sample.AddBuffer(buffer) }.map_err(|err| EncodeError::SessionCreate {
        reason: "could not put a buffer in a sample",
        status: err.code().0,
    })?;

    Ok(sample)
}

/// The sample the transform writes a finished frame into, if it wants to be given one.
///
/// Nothing for a transform that brings its own, which is what the hardware ones do.
fn output_sample(
    transform: &IMFTransform,
    config: EncoderConfig,
) -> Result<Option<IMFSample>, EncodeError> {
    // SAFETY: the transform is alive and has its output type set.
    let about =
        unsafe { transform.GetOutputStreamInfo(0) }.map_err(|err| EncodeError::SessionCreate {
            reason: "the encoder would not describe its output",
            status: err.code().0,
        })?;

    let brings_its_own =
        (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;

    if about.dwFlags & brings_its_own != 0 {
        return Ok(None);
    }

    // Whatever it says it needs, and never less than an uncompressed picture — which no
    // encoded frame exceeds, and which covers a transform that says nothing useful.
    let size = about.cbSize.max(nv12_bytes(config));

    // SAFETY: nothing is borrowed across the call.
    let buffer =
        unsafe { MFCreateMemoryBuffer(size) }.map_err(|err| EncodeError::SessionCreate {
            reason: "could not allocate a buffer for the encoder's output",
            status: err.code().0,
        })?;

    sample_around(&buffer).map(Some)
}

/// Copies the picture out of a staging texture into a buffer, a row at a time.
///
/// A row at a time because the texture's rows are as long as the driver found convenient and
/// the buffer's are exactly the picture's width, which is what the input type said they are.
fn read_into(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    buffer: &IMFMediaBuffer,
    config: EncoderConfig,
) -> Result<(), EncodeError> {
    let (width, height) = (config.width as usize, config.height as usize);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();

    // SAFETY: the staging texture is alive and was created for reading. This waits for the
    // copy into it to finish, which is the cost of this path and the reason it is the last
    // one tried.
    unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.map_err(|_| {
        EncodeError::InputBuffer {
            reason: "could not map the staging texture",
        }
    })?;

    let mut destination: *mut u8 = core::ptr::null_mut();
    let mut capacity = 0u32;

    // SAFETY: the buffer is alive, and the pointer it hands back is valid until `Unlock`.
    let locked = unsafe { buffer.Lock(&mut destination, Some(&mut capacity), None) };

    let fits = locked.is_ok()
        && !destination.is_null()
        && !mapped.pData.is_null()
        && capacity >= nv12_bytes(config);

    if fits {
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData.cast::<u8>().cast_const();

        // SAFETY: the mapping is live until `Unmap` and the lock until `Unlock`. NV12 is
        // `height` rows of luma and then half as many of interleaved chroma, every one of them
        // `RowPitch` bytes apart in the texture and at least `width` long; the buffer holds
        // exactly `width * height * 3 / 2`, which was checked above, and the two never
        // overlap.
        unsafe {
            for row in 0..height + height / 2 {
                core::ptr::copy_nonoverlapping(
                    base.add(row * pitch),
                    destination.add(row * width),
                    width,
                );
            }
        }
    }

    // SAFETY: whichever of the two was taken above is released; the length is what was
    // written.
    unsafe {
        if locked.is_ok() {
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(if fits { nv12_bytes(config) } else { 0 });
        }

        context.Unmap(staging, 0);
    }

    if fits {
        Ok(())
    } else {
        Err(EncodeError::InputBuffer {
            reason: "could not write the picture into the encoder's buffer",
        })
    }
}

/// The bytes in one NV12 picture: a full plane of luma and half of one of chroma.
const fn nv12_bytes(config: EncoderConfig) -> u32 {
    config.width * config.height / 2 * 3
}

/// Packs a pair the way Media Foundation's size, rate and ratio attributes want it: the first
/// in the high half.
const fn pack(first: u32, second: u32) -> u64 {
    ((first as u64) << 32) | second as u64
}

#[cfg(test)]
mod tests {
    use super::{nv12_bytes, pack};
    use crate::encode::EncoderConfig;
    use crate::net::negotiate::Codec;

    #[test]
    fn a_pair_packs_the_first_above_the_second() {
        assert_eq!(pack(1920, 1080), (1920u64 << 32) | 1080);
        assert_eq!(pack(60, 1), 60u64 << 32 | 1);
    }

    #[test]
    fn an_nv12_picture_is_one_and_a_half_planes() {
        let config = EncoderConfig {
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 20_000_000,
            max_slice_bytes: 0,
        };

        assert_eq!(nv12_bytes(config), 1920 * 1080 * 3 / 2);
    }
}
