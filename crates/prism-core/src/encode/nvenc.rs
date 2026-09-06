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
