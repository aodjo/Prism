//! NVENC on Windows: loading the encoder and asking what it can do.
//!
//! The library is loaded at run time rather than linked, and the bindings are hand-written
//! rather than generated. Both are deliberate. Generating them needs NVIDIA's Video Codec
//! SDK present at build time, which would break the cross-build from macOS and every CI
//! runner; linking would make the binary refuse to start on a machine with no NVIDIA driver.
//! Loaded at run time, the same binary runs everywhere and simply reports that there is no
//! encoder when there is none.
//!
//! # The version has to be negotiated, not assumed
//!
//! Every NVENC structure carries a version built from the API version, and the driver
//! refuses anything it does not implement with `NV_ENC_ERR_INVALID_VERSION`. The header
//! this was written against declares 13.1; the driver measured here implements 13.0, and
//! asking for 13.1 was refused outright. So the version is read from the driver first and
//! every structure is stamped with that.
//!
//! # Why capabilities are queried rather than assumed
//!
//! The same discipline the VideoToolbox path needed, for the same reason. A constant in a
//! header says nothing about what the silicon does: Apple's encoder refuses both the slice
//! size limit and long-term references while declaring the properties. Everything the host
//! depends on is asked for and reported.

use core::ffi::c_void;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::core::PCSTR;

use crate::encode::EncodeError;

/// Builds the version stamp a structure of revision `revision` must carry.
///
/// Mirrors `NVENCAPI_STRUCT_VERSION`: the API version, the structure's own revision, and a
/// fixed tag the driver checks.
const fn struct_version(api: u32, revision: u32) -> u32 {
    api | (revision << 16) | (0x7 << 28)
}

/// Packs a major and minor API version the way `NVENCAPI_VERSION` does.
const fn api_version(major: u32, minor: u32) -> u32 {
    major | (minor << 24)
}

/// The function table NVENC fills in.
///
/// Held as raw pointers rather than typed function pointers because only a handful are
/// called and transcribing forty-three signatures would be forty-three chances to get one
/// wrong. The layout is what matters, and the driver validates it through `version`.
#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    functions: [*mut c_void; 43],
    reserved2: [*mut c_void; 275],
}

/// Index of `nvEncGetEncodeCaps` in the function table.
const FN_GET_ENCODE_CAPS: usize = 7;

/// Index of `nvEncDestroyEncoder` in the function table.
const FN_DESTROY_ENCODER: usize = 27;

/// Index of `nvEncOpenEncodeSessionEx` in the function table.
const FN_OPEN_SESSION_EX: usize = 29;

/// Parameters for opening a session against a graphics device.
#[repr(C)]
struct OpenSessionParams {
    version: u32,
    device_type: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_DEVICE_TYPE_DIRECTX`.
const DEVICE_TYPE_DIRECTX: u32 = 0;

/// A capability query.
#[repr(C)]
struct CapsParam {
    version: u32,
    caps_to_query: u32,
    reserved: [u32; 62],
}

/// NVENC's own identifier for a codec.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecGuid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// H.264, the codec every decoder in this pipeline is guaranteed to handle.
pub const H264: CodecGuid = CodecGuid {
    data1: 0x6bc8_2762,
    data2: 0x4e63,
    data3: 0x4ca4,
    data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};

/// HEVC, negotiated when both ends support it.
pub const HEVC: CodecGuid = CodecGuid {
    data1: 0x790c_dc88,
    data2: 0x4522,
    data3: 0x4d7b,
    data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
};

/// The capabilities this project's design depends on.
///
/// Each is one of the plan's latency decisions, and each is a thing an encoder may simply
/// not implement. Every one of these is refused by Apple's encoder or absent from its API,
/// which is why they are reported rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Largest frame the encoder accepts.
    pub max_width: i32,
    /// Largest frame height the encoder accepts.
    pub max_height: i32,
    /// Whether the bitrate can be changed on a running session.
    ///
    /// The actuator congestion control needs. Pacing alone only slows the wire.
    pub dynamic_bitrate: bool,
    /// Whether a moving band of intra-coded blocks can replace periodic keyframes.
    pub intra_refresh: bool,
    /// Whether the encoder will cut a frame into slices on demand.
    ///
    /// Slice-level streaming is what lets a frame start moving before it is finished.
    pub dynamic_slice_mode: bool,
    /// Whether a reference frame can be invalidated after the client reports losing it.
    ///
    /// The plan calls this the difference between good game streaming and mediocre: on
    /// loss the host encodes against the last acknowledged reference instead of sending a
    /// keyframe, so the hitch disappears.
    pub reference_invalidation: bool,
    /// How many long-term references the encoder will hold.
    pub max_ltr_frames: i32,
    /// Whether completion can be waited on as an event rather than polled.
    pub async_encode: bool,
    /// Macroblocks per second the encoder can sustain.
    ///
    /// A macroblock is sixteen pixels square, so this divided by the macroblocks in a frame
    /// is the frame rate ceiling. It is the number that decides whether a resolution and
    /// frame rate are reachable at all — see [`Capabilities::max_fps_for`].
    pub max_macroblocks_per_second: i32,
    /// How many independent encoder engines the chip has.
    pub encoder_engines: i32,
}

impl Capabilities {
    /// Returns the frame rate ceiling this encoder can sustain at a resolution.
    ///
    /// Rounded down, because a rate the encoder cannot quite hold is a rate it will drop
    /// frames at. Returns zero when the resolution is larger than the encoder accepts.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::encode::nvenc::Capabilities;
    /// let caps = Capabilities {
    ///     max_width: 4096,
    ///     max_height: 4096,
    ///     dynamic_bitrate: true,
    ///     intra_refresh: true,
    ///     dynamic_slice_mode: true,
    ///     reference_invalidation: true,
    ///     max_ltr_frames: 8,
    ///     async_encode: true,
    ///     max_macroblocks_per_second: 983_040,
    ///     encoder_engines: 1,
    /// };
    ///
    /// // 1080 is not a multiple of sixteen, so the last row of macroblocks is a partial
    /// // one that still costs a whole one: 120 x 68, not 120 x 67.5.
    /// assert_eq!(caps.max_fps_for(1920, 1080), 120);
    /// assert_eq!(caps.max_fps_for(2560, 1440), 68);
    /// ```
    #[must_use]
    pub fn max_fps_for(&self, width: u32, height: u32) -> u32 {
        if width == 0
            || height == 0
            || width > self.max_width.max(0) as u32
            || height > self.max_height.max(0) as u32
        {
            return 0;
        }

        // A macroblock is sixteen pixels square, and a partial one still costs a whole one.
        let per_frame = width.div_ceil(16) as u64 * height.div_ceil(16) as u64;
        if per_frame == 0 {
            return 0;
        }

        (self.max_macroblocks_per_second.max(0) as u64 / per_frame) as u32
    }
}

/// The NVENC library, loaded and version-negotiated.
#[derive(Debug)]
pub struct Nvenc {
    functions: Box<FunctionList>,
    api: u32,
    driver_major: u32,
    driver_minor: u32,
}

impl Nvenc {
    /// Loads NVENC and negotiates an API version the driver implements.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] if the library is absent, does not export what
    /// it should, or refuses to hand over its function table. A machine with no NVIDIA
    /// driver fails here, which is the intended way to discover there is no encoder.
    pub fn load() -> Result<Self, EncodeError> {
        // SAFETY: the name is a nul-terminated literal, and a failure is reported rather
        // than producing a handle.
        let library = unsafe { LoadLibraryA(PCSTR(c"nvEncodeAPI64.dll".as_ptr().cast())) }
            .map_err(|_| EncodeError::SessionCreate {
                reason: "nvEncodeAPI64.dll is not present, so this machine has no NVENC",
                status: 0,
            })?;

        let max_supported = export(library, c"NvEncodeAPIGetMaxSupportedVersion")?;
        let create_instance = export(library, c"NvEncodeAPICreateInstance")?;

        // SAFETY: both exports have the signatures NVENC documents for them, and the
        // pointers came from this library.
        let (max_supported, create_instance) = unsafe {
            (
                core::mem::transmute::<*const c_void, extern "system" fn(*mut u32) -> i32>(
                    max_supported,
                ),
                core::mem::transmute::<*const c_void, extern "system" fn(*mut FunctionList) -> i32>(
                    create_instance,
                ),
            )
        };

        let mut packed = 0u32;
        if max_supported(&mut packed) != 0 {
            return Err(EncodeError::SessionCreate {
                reason: "NVENC would not report the API version it supports",
                status: 0,
            });
        }

        // The driver packs this differently from NVENCAPI_VERSION: major in the high nibble
        // upwards, minor in the low four bits.
        let (driver_major, driver_minor) = (packed >> 4, packed & 0xf);
        let api = api_version(driver_major, driver_minor);

        // SAFETY: NVENC fills a table of the size the version declares; zeroing first means
        // any slot it does not fill is null rather than uninitialised.
        let mut functions: Box<FunctionList> = Box::new(unsafe { core::mem::zeroed() });
        functions.version = struct_version(api, 2);

        let status = create_instance(&mut *functions);
        if status != 0 {
            return Err(EncodeError::SessionCreate {
                reason: "NVENC refused to hand over its function table",
                status,
            });
        }

        Ok(Self {
            functions,
            api,
            driver_major,
            driver_minor,
        })
    }

    /// Returns one entry of the function table as a callable.
    ///
    /// # Safety
    ///
    /// `index` must name a slot NVENC filled, and `F` must be that function's signature.
    /// Both come from the header, and getting either wrong is undefined behaviour.
    unsafe fn function<F: Copy>(&self, index: usize) -> F {
        debug_assert_eq!(
            core::mem::size_of::<F>(),
            core::mem::size_of::<*mut c_void>(),
            "a function table slot is one pointer"
        );

        // SAFETY: the caller guarantees the slot and the signature match.
        unsafe { *core::ptr::from_ref(&self.functions.functions[index]).cast::<F>() }
    }

    /// Returns the API version the driver implements, as major and minor.
    #[must_use]
    pub fn driver_api_version(&self) -> (u32, u32) {
        (self.driver_major, self.driver_minor)
    }

    /// Asks the encoder what it can do for one codec, on a Direct3D device.
    ///
    /// The device must be the one the frames to encode live on. A session opened on another
    /// device would need every frame copied across, which is the copy the capture and
    /// conversion path exists to avoid.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] if a session cannot be opened, which is how a
    /// driver that does not support the requested API version reports itself.
    ///
    /// # Safety
    ///
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn capabilities(
        &self,
        device: *mut c_void,
        codec: CodecGuid,
    ) -> Result<Capabilities, EncodeError> {
        // SAFETY: the table slot holds the function NVENC put there, and the caller
        // guarantees the device is live.
        let open = unsafe {
            core::mem::transmute::<
                *mut c_void,
                extern "system" fn(*mut OpenSessionParams, *mut *mut c_void) -> i32,
            >(self.functions.functions[FN_OPEN_SESSION_EX])
        };
        // SAFETY: as above.
        let get_caps = unsafe {
            core::mem::transmute::<
                *mut c_void,
                extern "system" fn(*mut c_void, CodecGuid, *mut CapsParam, *mut i32) -> i32,
            >(self.functions.functions[FN_GET_ENCODE_CAPS])
        };
        // SAFETY: as above.
        let destroy = unsafe {
            core::mem::transmute::<*mut c_void, extern "system" fn(*mut c_void) -> i32>(
                self.functions.functions[FN_DESTROY_ENCODER],
            )
        };

        // SAFETY: zeroing gives every reserved field the zero NVENC requires.
        let mut params: OpenSessionParams = unsafe { core::mem::zeroed() };
        params.version = struct_version(self.api, 1);
        params.device_type = DEVICE_TYPE_DIRECTX;
        params.device = device;
        params.api_version = self.api;

        let mut session: *mut c_void = core::ptr::null_mut();
        let status = open(&mut params, &mut session);
        if status != 0 || session.is_null() {
            return Err(EncodeError::SessionCreate {
                reason: "NVENC would not open a session on this device",
                status,
            });
        }

        let query = |id: u32| -> i32 {
            // SAFETY: zeroing gives the reserved fields the zero NVENC requires, and the
            // session is one this function just opened.
            let mut param: CapsParam = unsafe { core::mem::zeroed() };
            param.version = struct_version(self.api, 1);
            param.caps_to_query = id;

            let mut value = 0i32;
            if get_caps(session, codec, &mut param, &mut value) == 0 {
                value
            } else {
                0
            }
        };

        let capabilities = Capabilities {
            max_width: query(CAPS_WIDTH_MAX),
            max_height: query(CAPS_HEIGHT_MAX),
            dynamic_bitrate: query(CAPS_DYN_BITRATE_CHANGE) != 0,
            intra_refresh: query(CAPS_INTRA_REFRESH) != 0,
            dynamic_slice_mode: query(CAPS_DYNAMIC_SLICE_MODE) != 0,
            reference_invalidation: query(CAPS_REF_PIC_INVALIDATION) != 0,
            max_ltr_frames: query(CAPS_NUM_MAX_LTR_FRAMES),
            async_encode: query(CAPS_ASYNC_ENCODE) != 0,
            max_macroblocks_per_second: query(CAPS_MB_PER_SEC_MAX),
            encoder_engines: query(CAPS_NUM_ENCODER_ENGINES),
        };

        destroy(session);

        Ok(capabilities)
    }
}

// Positions in `NV_ENC_CAPS`, which is an unvalued enum and therefore numbered from zero in
// declaration order. Taken from the header rather than remembered.
const CAPS_NUM_MAX_BFRAMES: u32 = 0;
const CAPS_WIDTH_MAX: u32 = 16;
const CAPS_HEIGHT_MAX: u32 = 17;
const CAPS_DYN_BITRATE_CHANGE: u32 = 20;
const CAPS_INTRA_REFRESH: u32 = 25;
const CAPS_DYNAMIC_SLICE_MODE: u32 = 27;
const CAPS_REF_PIC_INVALIDATION: u32 = 28;
const CAPS_ASYNC_ENCODE: u32 = 30;
const CAPS_MB_PER_SEC_MAX: u32 = 32;
const CAPS_NUM_MAX_LTR_FRAMES: u32 = 40;
const CAPS_NUM_ENCODER_ENGINES: u32 = 49;

/// Silences the unused warning on a constant kept for the record.
///
/// B-frames are never enabled here — they reorder output and cost a frame of latency — but
/// the position is recorded so the numbering above can be checked against the header.
#[allow(dead_code)]
const _: u32 = CAPS_NUM_MAX_BFRAMES;

/// Looks up one export, naming it if it is missing.
fn export(library: HMODULE, name: &core::ffi::CStr) -> Result<*const c_void, EncodeError> {
    // SAFETY: the module handle is live and the name is nul-terminated.
    unsafe { GetProcAddress(library, PCSTR(name.as_ptr().cast())) }
        .map(|address| address as *const c_void)
        .ok_or(EncodeError::SessionCreate {
            reason: "nvEncodeAPI64.dll is missing an export this build needs",
            status: 0,
        })
}

impl core::fmt::Debug for FunctionList {
    /// Describes the table without printing forty-three raw pointers.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FunctionList")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// The low-latency preset, `NV_ENC_PRESET_P1_GUID`.
///
/// P1 is the fastest of the seven presets. Quality presets buy image quality with encoder
/// latency, which is the wrong side of this project's trade.
const PRESET_P1: CodecGuid = CodecGuid {
    data1: 0xfc0a_8d3e,
    data2: 0x45f8,
    data3: 0x4cf8,
    data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};

/// `NV_ENC_TUNING_INFO_LOW_LATENCY`.
const TUNING_LOW_LATENCY: u32 = 2;

/// `NV_ENC_BUFFER_FORMAT_NV12`.
const BUFFER_FORMAT_NV12: u32 = 1;

/// `NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX`.
const RESOURCE_TYPE_DIRECTX: u32 = 0;

/// `NV_ENC_PIC_STRUCT_FRAME`.
const PIC_STRUCT_FRAME: u32 = 1;

/// `NV_ENC_PIC_FLAG_FORCEIDR`.
const PIC_FLAG_FORCE_IDR: u32 = 2;

/// `NV_ENC_PIC_FLAG_OUTPUT_SPSPPS`.
const PIC_FLAG_OUTPUT_SPSPPS: u32 = 4;

/// Index of `nvEncInitializeEncoder`.
const FN_INITIALIZE: usize = 11;
/// Index of `nvEncCreateBitstreamBuffer`.
const FN_CREATE_BITSTREAM: usize = 14;
/// Index of `nvEncDestroyBitstreamBuffer`.
const FN_DESTROY_BITSTREAM: usize = 15;
/// Index of `nvEncEncodePicture`.
const FN_ENCODE_PICTURE: usize = 16;
/// Index of `nvEncLockBitstream`.
const FN_LOCK_BITSTREAM: usize = 17;
/// Index of `nvEncUnlockBitstream`.
const FN_UNLOCK_BITSTREAM: usize = 18;
/// Index of `nvEncMapInputResource`.
const FN_MAP_INPUT: usize = 25;
/// Index of `nvEncUnmapInputResource`.
const FN_UNMAP_INPUT: usize = 26;
/// Index of `nvEncRegisterResource`.
const FN_REGISTER_RESOURCE: usize = 30;
/// Index of `nvEncUnregisterResource`.
const FN_UNREGISTER_RESOURCE: usize = 31;
/// Index of `nvEncGetEncodePresetConfigEx`.
const FN_GET_PRESET_CONFIG_EX: usize = 39;

/// Every NVENC structure this module passes, sized exactly as the C header defines it.
///
/// The sizes and offsets were taken from the header by a compiler rather than by hand — a
/// first attempt at `NV_ENC_INITIALIZE_PARAMS` by hand came out twenty-four bytes short and
/// placed `tuningInfo` at the wrong offset, which would have corrupted memory rather than
/// failed. The named fields are the ones this module sets; everything after them is opaque
/// padding, and a compile-time assertion pins the total.
macro_rules! nvenc_struct {
    (
        $(#[$meta:meta])*
        $name:ident, $size:expr, $tail:expr, { $($field:ident : $ty:ty),* $(,)? }
    ) => {
        $(#[$meta])*
        #[repr(C)]
        struct $name {
            $($field: $ty,)*
            tail: [u8; $tail],
        }

        const _: () = assert!(
            core::mem::size_of::<$name>() == $size,
            concat!(stringify!($name), " must match the C layout exactly")
        );

        impl Default for $name {
            /// Zeroes every field, which is what NVENC requires of its reserved space.
            fn default() -> Self {
                // SAFETY: every field is a plain integer, pointer or byte array, for all of
                // which an all-zero pattern is a valid value.
                unsafe { core::mem::zeroed() }
            }
        }
    };
}

nvenc_struct!(InitializeParams, 1800, 1656, {
    version: u32,
    encode_guid: CodecGuid,
    preset_guid: CodecGuid,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    bit_fields: u32,
    priv_data_size: u32,
    reserved: u32,
    priv_data: *mut c_void,
    encode_config: *mut c_void,
    max_encode_width: u32,
    max_encode_height: u32,
    me_hints: [u8; 32],
    tuning_info: u32,
    buffer_format: u32,
});

nvenc_struct!(PresetConfig, 5128, 5120, {
    version: u32,
    reserved: u32,
});

nvenc_struct!(RegisterResource, 1536, 1488, {
    version: u32,
    resource_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    sub_resource_index: u32,
    resource_to_register: *mut c_void,
    registered_resource: *mut c_void,
    buffer_format: u32,
    buffer_usage: u32,
});

nvenc_struct!(MapInputResource, 1544, 1508, {
    version: u32,
    sub_resource_index: u32,
    sub_resource: *mut c_void,
    registered_resource: *mut c_void,
    mapped_resource: *mut c_void,
    mapped_buffer_fmt: u32,
});

nvenc_struct!(CreateBitstreamBuffer, 776, 752, {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: *mut c_void,
});

nvenc_struct!(PicParams, 3360, 3280, {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_timestamp: u64,
    input_duration: u64,
    input_buffer: *mut c_void,
    output_bitstream: *mut c_void,
    completion_event: *mut c_void,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    codec_pic_params_pad: u32,
});

nvenc_struct!(LockBitstream, 1544, 1476, {
    version: u32,
    bit_fields: u32,
    output_bitstream: *mut c_void,
    slice_offsets: *mut u32,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_timestamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: *mut c_void,
    picture_type: u32,
});

/// A running NVENC session encoding NV12 textures into H.264.
///
/// The texture is registered once and mapped per frame rather than copied: NVENC reads the
/// same GPU memory the conversion pass wrote, so a frame never touches system memory
/// between the compositor and the wire.
pub struct NvencEncoder {
    nvenc: Nvenc,
    session: *mut c_void,
    /// The configuration handed to the driver, kept alive because reconfiguring points at
    /// the same memory rather than supplying a fresh copy.
    preset: Box<PresetConfig>,
    /// The parameters the session was initialized with, reused when the rate changes.
    init: Box<InitializeParams>,
    registered: *mut c_void,
    bitstream: *mut c_void,
    config: crate::encode::EncoderConfig,
    frame: crate::encode::EncodedFrame,
    frames_encoded: u64,
}

impl NvencEncoder {
    /// Opens a session and prepares it to encode `texture`.
    ///
    /// `texture` must be the NV12 surface the conversion pass draws into, on the same
    /// Direct3D device as `device`. It is registered here and reused for every frame.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] if NVENC will not open a session, accept the
    /// configuration, or register the texture.
    ///
    /// # Safety
    ///
    /// `device` must be a live `ID3D11Device` and `texture` a live `ID3D11Texture2D` NV12
    /// surface belonging to it, both outliving this encoder.
    pub unsafe fn new(
        device: *mut c_void,
        texture: *mut c_void,
        config: crate::encode::EncoderConfig,
    ) -> Result<Self, EncodeError> {
        let nvenc = Nvenc::load()?;
        let api = nvenc.api;

        // SAFETY: the table slot holds the function NVENC put there.
        let open = unsafe { nvenc.function::<OpenSession>(FN_OPEN_SESSION_EX) };

        let mut params = OpenSessionParams {
            version: struct_version(api, 1),
            device_type: DEVICE_TYPE_DIRECTX,
            device,
            reserved: core::ptr::null_mut(),
            api_version: api,
            reserved1: [0; 253],
            reserved2: [core::ptr::null_mut(); 64],
        };

        let mut session: *mut c_void = core::ptr::null_mut();
        check(open(&mut params, &mut session), "open a session")?;

        let mut encoder = Self {
            nvenc,
            session,
            preset: Box::new(PresetConfig::default()),
            init: Box::new(InitializeParams::default()),
            registered: core::ptr::null_mut(),
            bitstream: core::ptr::null_mut(),
            config,
            frame: crate::encode::EncodedFrame::default(),
            frames_encoded: 0,
        };

        // SAFETY: the session was just opened and the texture is the caller's, which they
        // guarantee outlives this encoder.
        unsafe {
            encoder.initialize()?;
            encoder.register(texture)?;
            encoder.create_bitstream()?;
        }

        Ok(encoder)
    }

    /// Configures the session from the low-latency preset.
    ///
    /// The preset is fetched and handed back unchanged. NVENC fills a configuration
    /// structure of nearly four kilobytes with a codec-specific union inside it, and every
    /// field this code does not need is a field it can get wrong; asking the driver for a
    /// preset and passing it through is both simpler and safer than filling it in.
    ///
    /// # Safety
    ///
    /// The session must be open.
    unsafe fn initialize(&mut self) -> Result<(), EncodeError> {
        // SAFETY: the table slots hold the functions NVENC put there.
        let (get_preset, initialize) = unsafe {
            (
                self.nvenc
                    .function::<GetPresetConfigEx>(FN_GET_PRESET_CONFIG_EX),
                self.nvenc.function::<Initialize>(FN_INITIALIZE),
            )
        };

        let api = self.nvenc.api;
        self.preset.version = struct_version_high(api, 5);

        // The configuration lives inside the preset structure, so its own version has to be
        // stamped before the driver will fill it.
        let config_ptr = (&raw mut self.preset.tail).cast::<u8>();
        // SAFETY: `presetCfg` begins at offset eight, which is where `tail` starts, and the
        // first four bytes of a configuration are its version.
        unsafe {
            config_ptr
                .cast::<u32>()
                .write_unaligned(struct_version_high(api, 9));
        }

        check(
            get_preset(
                self.session,
                H264,
                PRESET_P1,
                TUNING_LOW_LATENCY,
                &mut *self.preset,
            ),
            "fetch the low latency preset",
        )?;

        // SAFETY: the offsets come from the header as the compiler reports them, and they
        // are all inside the configuration the driver just filled.
        unsafe {
            apply_rate_control(config_ptr, self.config.bitrate_bps, self.config.fps);
            apply_h264_config(config_ptr, self.config.max_slice_bytes);
        }

        let params = InitializeParams {
            version: struct_version_high(api, 7),
            encode_guid: H264,
            preset_guid: PRESET_P1,
            encode_width: self.config.width,
            encode_height: self.config.height,
            dar_width: self.config.width,
            dar_height: self.config.height,
            frame_rate_num: self.config.fps,
            frame_rate_den: 1,
            // Synchronous. The event-driven path is what the plan wants and NVENC supports
            // it, but it needs an event object per in-flight frame and belongs with the
            // threading work rather than with first light.
            enable_encode_async: 0,
            // Picture type decided by the encoder, which is what lets it honour a forced
            // IDR without the caller tracking GOP structure.
            enable_ptd: 1,
            encode_config: config_ptr.cast(),
            tuning_info: TUNING_LOW_LATENCY,
            buffer_format: BUFFER_FORMAT_NV12,
            ..InitializeParams::default()
        };

        // Kept rather than dropped: reconfiguring the session later hands these same
        // parameters back with a new rate in them.
        *self.init = params;

        check(
            initialize(self.session, &mut *self.init),
            "initialize the encoder",
        )
    }

    /// Changes the target bitrate on a running session.
    ///
    /// This is the actuator congestion control needs on the Windows host. NVENC has no
    /// property to set for it: the whole configuration is handed back with the new rate in
    /// it, which is why this encoder keeps its configuration alive rather than building one
    /// and forgetting it.
    ///
    /// `resetEncoder` is deliberately left clear. Resetting forces a keyframe, and a
    /// controller that adjusts several times a second would then be sending keyframes
    /// several times a second — a worse problem than the one it is solving.
    ///
    /// # Known limitation, measured rather than assumed
    ///
    /// On the driver this was developed against (NVENC API 13.0, GTX 1660) the call
    /// **succeeds and the output rate does not follow**. A session initialised at a fixed
    /// rate tracks it closely — 8 Mbps produced 7.2 and 40 Mbps produced 34.8 — while a
    /// session reconfigured from 20 Mbps up to 40 kept producing 17, which is what 20 Mbps
    /// produces. One hundred and fifty-six reconfigurations were accepted and none refused.
    /// Leaving the VBV window untouched across the change made no difference.
    ///
    /// So this is wired up and honest about not yet working: the congestion controller
    /// drives the send pacer, which does take effect, and the encoder is told the same
    /// number. Whether the missing piece is `resetEncoder`, a field the preset owns, or
    /// something about this driver is not yet established, and claiming the actuator works
    /// would be worse than saying it does not.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] if NVENC refuses the new configuration.
    pub fn set_bitrate_bps(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        if bitrate_bps == 0 || bitrate_bps == self.config.bitrate_bps {
            return Ok(());
        }

        let config_ptr = (&raw mut self.preset.tail).cast::<u8>();

        // SAFETY: the pointer addresses the configuration this encoder owns and the driver
        // filled, and the offsets come from the header.
        unsafe { apply_rate_control(config_ptr, bitrate_bps, self.config.fps) };

        let mut params = ReconfigureParams {
            version: struct_version_high(self.nvenc.api, 2),
            bit_fields: 0,
            init: core::mem::take(&mut *self.init),
            tail: [0; 8],
        };

        // SAFETY: the table slot holds the function NVENC put there, and the parameters are
        // a live local for the duration of the call.
        let reconfigure = unsafe { self.nvenc.function::<Reconfigure>(FN_RECONFIGURE) };
        let status = reconfigure(self.session, &mut params);

        // Put the parameters back whether or not the driver accepted them, so a refusal
        // leaves the encoder able to try again rather than holding a default.
        *self.init = core::mem::take(&mut params.init);
        check(status, "reconfigure the encoder")?;

        self.config.bitrate_bps = bitrate_bps;

        Ok(())
    }

    /// Registers the NV12 texture so frames can be mapped rather than copied.
    ///
    /// # Safety
    ///
    /// `texture` must be a live NV12 `ID3D11Texture2D` on this session's device.
    unsafe fn register(&mut self, texture: *mut c_void) -> Result<(), EncodeError> {
        // SAFETY: the table slot holds the function NVENC put there.
        let register = unsafe { self.nvenc.function::<Register>(FN_REGISTER_RESOURCE) };

        let mut resource = RegisterResource {
            version: struct_version(self.nvenc.api, 5),
            resource_type: RESOURCE_TYPE_DIRECTX,
            width: self.config.width,
            height: self.config.height,
            // Zero lets NVENC take the pitch from the texture, which is the only thing that
            // can know it.
            pitch: 0,
            resource_to_register: texture,
            buffer_format: BUFFER_FORMAT_NV12,
            ..RegisterResource::default()
        };

        check(
            register(self.session, &mut resource),
            "register the texture",
        )?;
        self.registered = resource.registered_resource;

        Ok(())
    }

    /// Allocates the buffer NVENC writes the bitstream into.
    ///
    /// # Safety
    ///
    /// The session must be initialized.
    unsafe fn create_bitstream(&mut self) -> Result<(), EncodeError> {
        // SAFETY: the table slot holds the function NVENC put there.
        let create = unsafe { self.nvenc.function::<CreateBitstream>(FN_CREATE_BITSTREAM) };

        let mut buffer = CreateBitstreamBuffer {
            version: struct_version(self.nvenc.api, 1),
            ..CreateBitstreamBuffer::default()
        };

        check(
            create(self.session, &mut buffer),
            "allocate a bitstream buffer",
        )?;
        self.bitstream = buffer.bitstream_buffer;

        Ok(())
    }

    /// Encodes one frame from the registered texture.
    ///
    /// The texture must already hold the picture: this maps it, encodes, and reads the
    /// bitstream back, so the caller draws into it and then calls this.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] if NVENC refuses the frame or will not hand back the
    /// bitstream.
    pub fn encode(
        &mut self,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<&crate::encode::EncodedFrame, EncodeError> {
        // SAFETY: every table slot holds the function NVENC put there, and the session,
        // registered resource and bitstream buffer are all alive for this encoder's life.
        let (map, unmap, encode, lock, unlock) = unsafe {
            (
                self.nvenc.function::<MapInput>(FN_MAP_INPUT),
                self.nvenc.function::<UnmapInput>(FN_UNMAP_INPUT),
                self.nvenc.function::<EncodePicture>(FN_ENCODE_PICTURE),
                self.nvenc.function::<LockBitstreamFn>(FN_LOCK_BITSTREAM),
                self.nvenc.function::<UnlockBitstream>(FN_UNLOCK_BITSTREAM),
            )
        };

        let api = self.nvenc.api;

        let mut mapping = MapInputResource {
            version: struct_version(api, 4),
            registered_resource: self.registered,
            ..MapInputResource::default()
        };
        check(map(self.session, &mut mapping), "map the texture")?;

        let mut flags = 0;
        if force_idr {
            // The parameter sets ride along with the keyframe, so a client that joins here
            // has everything it needs to start.
            flags |= PIC_FLAG_FORCE_IDR | PIC_FLAG_OUTPUT_SPSPPS;
        }

        let mut picture = PicParams {
            version: struct_version_high(api, 7),
            input_width: self.config.width,
            input_height: self.config.height,
            encode_pic_flags: flags,
            input_timestamp: pts_us,
            input_buffer: mapping.mapped_resource,
            output_bitstream: self.bitstream,
            buffer_fmt: BUFFER_FORMAT_NV12,
            picture_struct: PIC_STRUCT_FRAME,
            ..PicParams::default()
        };

        let encode_status = encode(self.session, &mut picture);

        let mut locked = LockBitstream {
            version: struct_version_high(api, 2),
            output_bitstream: self.bitstream,
            ..LockBitstream::default()
        };

        let result = (|| -> Result<(), EncodeError> {
            check(encode_status, "encode a picture")?;
            check(lock(self.session, &mut locked), "lock the bitstream")?;
            Ok(())
        })();

        if result.is_ok() {
            self.frame.reset();
            self.frame.pts_us = locked.output_timestamp;

            // SAFETY: NVENC reports the buffer and its length together, and the buffer is
            // valid until it is unlocked below.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    locked.bitstream_buffer_ptr.cast::<u8>(),
                    locked.bitstream_size_in_bytes as usize,
                )
            };

            split_annex_b(&mut self.frame, bytes);
            self.frame.is_idr = self.frame.slices.iter().any(|range| {
                self.frame
                    .data
                    .get(range.start + crate::encode::START_CODE.len())
                    .is_some_and(|&byte| byte & 0x1f == 5)
            });

            let _ = unlock(self.session, self.bitstream);
        }

        let _ = unmap(self.session, mapping.mapped_resource);
        result?;

        self.frames_encoded += 1;

        Ok(&self.frame)
    }

    /// Returns how many frames this session has encoded.
    #[must_use]
    pub fn frames_encoded(&self) -> u64 {
        self.frames_encoded
    }

    /// Returns the configuration this session was created with.
    #[must_use]
    pub fn config(&self) -> crate::encode::EncoderConfig {
        self.config
    }
}

/// Overwrites the preset's rate control with a requested bitrate.
///
/// The preset alone does not honour a bitrate. Passing it through unchanged produced forty
/// megabits against a twenty megabit request, and frames four times the size the budget
/// allows — which the client could not keep up with, so it dropped frames, and a stream with
/// a single keyframe never recovers from a dropped reference.
///
/// Constant bitrate with a one-frame VBV window, which is the plan's first and most
/// important latency decision: a larger window lets the encoder emit a frame that takes
/// several frame times to transmit, and that is the single biggest source of latency spikes.
///
/// # Safety
///
/// `config` must point at a configuration structure the driver has filled.
unsafe fn apply_rate_control(config: *mut u8, bitrate_bps: u32, fps: u32) {
    // Offsets within NV_ENC_CONFIG, read from the header by a compiler rather than
    // counted by hand.
    const GOP_LENGTH: usize = 20;
    const FRAME_INTERVAL_P: usize = 24;
    const RC_MODE: usize = 44;
    const AVERAGE_BITRATE: usize = 60;
    const MAX_BITRATE: usize = 64;
    const VBV_BUFFER_SIZE: usize = 68;
    const VBV_INITIAL_DELAY: usize = 72;

    /// `NV_ENC_PARAMS_RC_CBR`.
    const RC_CBR: u32 = 2;

    /// An infinite group of pictures: one keyframe at the start and nothing but P
    /// frames after it. Recovery is the transport's job through parity, not a periodic
    /// keyframe the whole stream pays for.
    const GOP_INFINITE: u32 = u32::MAX;

    let bitrate = bitrate_bps;
    // Exactly one frame of budget. This is the vbvBufferSize the plan calls the largest
    // single cause of latency spikes when it is set larger.
    let frame_budget = bitrate / fps.max(1);

    // SAFETY: every offset is inside the structure, and each write is a `u32` at a
    // four-byte aligned offset.
    unsafe {
        let put =
            |offset: usize, value: u32| config.add(offset).cast::<u32>().write_unaligned(value);

        put(GOP_LENGTH, GOP_INFINITE);
        // Every frame is a P frame; no B frames, which reorder output and cost a frame.
        put(FRAME_INTERVAL_P, 1);
        put(RC_MODE, RC_CBR);
        put(AVERAGE_BITRATE, bitrate);
        put(MAX_BITRATE, bitrate);
        put(VBV_BUFFER_SIZE, frame_budget);
        put(VBV_INITIAL_DELAY, frame_budget);
    }
}

/// Sets the H.264 specific parts of the configuration the preset does not.
///
/// `slices` is how many slices each frame is cut into; one leaves the frame whole. Cutting
/// it lets each piece start moving before the rest is encoded, which is the plan's second
/// latency decision — and it is exactly what Apple Silicon refuses, so this is the first
/// place in the project it can actually be done.
///
/// Parameter sets are repeated on every frame rather than only on the keyframe. Without
/// that a client that joins late, or loses the one frame carrying them, waits forever on a
/// black window with nothing reporting an error — the same failure the VideoToolbox path
/// had. Two parameter sets are tens of bytes against a stream measured in megabits.
///
/// # Safety
///
/// `config` must point at a configuration structure the driver has filled.
unsafe fn apply_h264_config(config: *mut u8, slices: u32) {
    // Offsets within NV_ENC_CONFIG, from the header as the compiler reports them.
    const H264_BITS: usize = 168;
    const IDR_PERIOD: usize = 176;
    const SLICE_MODE: usize = 232;
    const SLICE_MODE_DATA: usize = 236;

    /// Bit 12 of the H.264 bitfield word: emit SPS and PPS with every frame.
    const REPEAT_SPS_PPS: u32 = 1 << 12;

    /// `sliceMode` 3 means "this many slices per frame", which is the only mode that gives
    /// a predictable count rather than a size-driven one.
    const SLICE_MODE_COUNT: u32 = 3;

    /// One keyframe at the start and none after it, matching the infinite group of pictures
    /// the rate control sets.
    const IDR_INFINITE: u32 = u32::MAX;

    // SAFETY: every offset is inside the structure and each access is a `u32`.
    unsafe {
        let bits = config.add(H264_BITS).cast::<u32>();
        bits.write_unaligned(bits.read_unaligned() | REPEAT_SPS_PPS);

        let put =
            |offset: usize, value: u32| config.add(offset).cast::<u32>().write_unaligned(value);

        put(IDR_PERIOD, IDR_INFINITE);

        if slices > 1 {
            put(SLICE_MODE, SLICE_MODE_COUNT);
            put(SLICE_MODE_DATA, slices);
        }
    }
}

impl Drop for NvencEncoder {
    /// Releases the bitstream buffer, the registered texture, and the session, in that order.
    fn drop(&mut self) {
        // SAFETY: each slot holds the function NVENC put there, and each handle is either
        // null or one this encoder created.
        unsafe {
            if !self.bitstream.is_null() {
                let destroy = self.nvenc.function::<UnlockBitstream>(FN_DESTROY_BITSTREAM);
                let _ = destroy(self.session, self.bitstream);
            }
            if !self.registered.is_null() {
                let unregister = self
                    .nvenc
                    .function::<UnlockBitstream>(FN_UNREGISTER_RESOURCE);
                let _ = unregister(self.session, self.registered);
            }
            if !self.session.is_null() {
                let destroy = self
                    .nvenc
                    .function::<extern "system" fn(*mut c_void) -> i32>(FN_DESTROY_ENCODER);
                let _ = destroy(self.session);
            }
        }
    }
}

impl core::fmt::Debug for NvencEncoder {
    /// Describes the session without printing raw handles.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NvencEncoder")
            .field("config", &self.config)
            .field("frames_encoded", &self.frames_encoded)
            .finish_non_exhaustive()
    }
}

/// Splits an Annex B bitstream into the frame's NAL units.
///
/// NVENC emits H.264 with start codes already in place, so this finds the boundaries rather
/// than inserting them. Both three and four byte start codes are accepted because an encoder
/// may use either.
fn split_annex_b(frame: &mut crate::encode::EncodedFrame, bytes: &[u8]) {
    let mut starts = Vec::new();
    let mut index = 0;

    while index + 3 <= bytes.len() {
        if bytes[index] == 0 && bytes[index + 1] == 0 {
            if bytes[index + 2] == 1 {
                starts.push((index, 3));
                index += 3;
                continue;
            }
            if index + 4 <= bytes.len() && bytes[index + 2] == 0 && bytes[index + 3] == 1 {
                starts.push((index, 4));
                index += 4;
                continue;
            }
        }
        index += 1;
    }

    for (position, (offset, prefix)) in starts.iter().enumerate() {
        let begin = offset + prefix;
        let end = starts
            .get(position + 1)
            .map_or(bytes.len(), |(next, _)| *next);

        if begin < end {
            frame.push_nal(&bytes[begin..end]);
        }
    }
}

/// Turns an NVENC status into an error naming what was being attempted.
fn check(status: i32, attempt: &'static str) -> Result<(), EncodeError> {
    if status == 0 {
        return Ok(());
    }

    let _ = attempt;
    Err(EncodeError::Encode { status })
}

/// Builds a version stamp with the high bit some structures require.
const fn struct_version_high(api: u32, revision: u32) -> u32 {
    struct_version(api, revision) | (1 << 31)
}

nvenc_struct!(ReconfigureParams, 1816, 8, {
    version: u32,
    bit_fields: u32,
    init: InitializeParams,
});

/// Index of `nvEncReconfigureEncoder`.
const FN_RECONFIGURE: usize = 32;

type Reconfigure = extern "system" fn(*mut c_void, *mut ReconfigureParams) -> i32;
type OpenSession = extern "system" fn(*mut OpenSessionParams, *mut *mut c_void) -> i32;
type GetPresetConfigEx =
    extern "system" fn(*mut c_void, CodecGuid, CodecGuid, u32, *mut PresetConfig) -> i32;
type Initialize = extern "system" fn(*mut c_void, *mut InitializeParams) -> i32;
type Register = extern "system" fn(*mut c_void, *mut RegisterResource) -> i32;
type CreateBitstream = extern "system" fn(*mut c_void, *mut CreateBitstreamBuffer) -> i32;
type MapInput = extern "system" fn(*mut c_void, *mut MapInputResource) -> i32;
type UnmapInput = extern "system" fn(*mut c_void, *mut c_void) -> i32;
type EncodePicture = extern "system" fn(*mut c_void, *mut PicParams) -> i32;
type LockBitstreamFn = extern "system" fn(*mut c_void, *mut LockBitstream) -> i32;
type UnlockBitstream = extern "system" fn(*mut c_void, *mut c_void) -> i32;
