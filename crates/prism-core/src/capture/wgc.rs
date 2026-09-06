//! Screen capture on Windows through Windows.Graphics.Capture.
//!
//! The compositor hands over each frame as a Direct3D 11 texture that already lives in GPU
//! memory. That is the whole reason this API is used rather than the older desktop
//! duplication or a GDI blit: the texture goes straight to the encoder without ever being
//! read into system memory, which at 1440p120 would be four hundred megabytes a second of
//! pure waste.
//!
//! Frames arrive on a WinRT event thread. They are moved to the caller over a channel and
//! the oldest is dropped when the caller falls behind, because a frame that has queued is
//! already too late to be worth encoding.
//!
//! The cursor is excluded. The client draws its own at its native refresh rate, so a cursor
//! baked into the video would inherit the video's latency — the single most noticeable way
//! a remote desktop feels remote.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Duration;

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::Interface;

use crate::capture::{CaptureConfig, CaptureError};

/// Driver type used when no graphics adapter will provide one.
///
/// A virtual machine without GPU passthrough reports a basic display adapter, and hardware
/// device creation fails on it. WARP is Direct3D's software rasteriser: far too slow to
/// stream from, but it makes the capture path itself testable on a machine that has no GPU,
/// which is the difference between developing this on real hardware only and developing it
/// anywhere.
const WARP: D3D_DRIVER_TYPE = D3D_DRIVER_TYPE(5);

/// One captured frame, still on the GPU.
///
/// Holds the WinRT frame alive because the texture belongs to it. Dropping this returns the
/// surface to the compositor's pool, so a caller that holds frames rather than releasing
/// them starves the capture.
pub struct CapturedFrame {
    frame: Direct3D11CaptureFrame,
    capture_ts_us: u64,
}

// SAFETY: the frame is moved from the WinRT callback thread to the caller and used by one
// thread at a time. WinRT objects are agile unless marked otherwise, and the capture frame
// is not marked otherwise.
unsafe impl Send for CapturedFrame {}

impl CapturedFrame {
    /// Returns the frame's texture, without copying it.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Start`] if the surface does not expose a Direct3D texture,
    /// which would mean the compositor handed back something other than what it promised.
    pub fn texture(&self) -> Result<ID3D11Texture2D, CaptureError> {
        let surface = self.frame.Surface().map_err(start_error)?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(start_error)?;

        // SAFETY: the surface came from the capture pool, so it wraps a DXGI interface, and
        // the type parameter names the interface a capture surface actually implements.
        unsafe { access.GetInterface::<ID3D11Texture2D>() }.map_err(start_error)
    }

    /// Returns when the compositor produced this frame, in microseconds.
    ///
    /// Taken from the frame's own relative time rather than the moment it was received, so
    /// the latency chain measures the compositor's delivery rather than the delay before
    /// this process happened to look.
    #[must_use]
    pub fn capture_ts_us(&self) -> u64 {
        self.capture_ts_us
    }

    /// Returns the frame's size in pixels.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Start`] if the frame will not report its size.
    pub fn size(&self) -> Result<(u32, u32), CaptureError> {
        let size = self.frame.ContentSize().map_err(start_error)?;

        Ok((size.Width.max(0) as u32, size.Height.max(0) as u32))
    }
}

/// A running Windows.Graphics.Capture session for one display.
pub struct ScreenCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    frames: Receiver<CapturedFrame>,
    device: ID3D11Device,
    width: u32,
    height: u32,
    hardware: bool,
}

impl ScreenCapture {
    /// Starts capturing the primary display.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::PermissionDenied`] if this build of Windows will not allow
    /// capture, [`CaptureError::NoDisplay`] if there is no primary monitor, and
    /// [`CaptureError::Start`] if Direct3D or the capture API refuses.
    pub fn start(config: CaptureConfig) -> Result<Self, CaptureError> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(CaptureError::PermissionDenied);
        }

        let (device, hardware) = create_device()?;
        let item = capture_primary_monitor()?;

        let size = item.Size().map_err(start_error)?;
        let (width, height) = (size.Width.max(0) as u32, size.Height.max(0) as u32);
        if width == 0 || height == 0 {
            return Err(CaptureError::NoDisplay);
        }

        let winrt_device = winrt_device_for(&device)?;

        // Bgra8 because it is the only format the capture pool accepts. The conversion to
        // the encoder's NV12 happens on the GPU, not here.
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            config.queue_depth.clamp(2, 8) as i32,
            size,
        )
        .map_err(start_error)?;

        let (tx, rx) = sync_channel(config.queue_depth.clamp(1, 8));
        let sender = Mutex::new(tx);

        pool.FrameArrived(&TypedEventHandler::new(
            move |pool: windows_core::Ref<'_, Direct3D11CaptureFramePool>, _| {
                let Some(pool) = pool.as_ref() else {
                    return Ok(());
                };
                let Ok(frame) = pool.TryGetNextFrame() else {
                    return Ok(());
                };

                let capture_ts_us = frame
                    .SystemRelativeTime()
                    .map(|time| (time.Duration / 10) as u64)
                    .unwrap_or_default();

                // Dropped rather than queued when the caller is behind. Releasing the frame
                // immediately also returns its surface to the pool, which is what keeps the
                // compositor from stalling on a slow consumer.
                if let Ok(sender) = sender.lock() {
                    let _ = sender.try_send(CapturedFrame {
                        frame,
                        capture_ts_us,
                    });
                }

                Ok(())
            },
        ))
        .map_err(start_error)?;

        let session = pool.CreateCaptureSession(&item).map_err(start_error)?;

        // The cursor is drawn by the client, so it must not be in the video. Older builds
        // do not expose the property at all, and on those the cursor is baked in — worth
        // knowing about rather than failing over.
        let _ = session.SetIsCursorCaptureEnabled(config.show_cursor);

        session.StartCapture().map_err(start_error)?;

        Ok(Self {
            session,
            pool,
            frames: rx,
            device,
            width,
            height,
            hardware,
        })
    }

    /// Returns the capture width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Returns the capture height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Returns whether a real graphics adapter is behind the capture.
    ///
    /// False means Direct3D fell back to its software rasteriser, which happens on a
    /// virtual machine with no GPU passthrough. Capture still works and can be developed
    /// against; the frame rate it delivers says nothing about real hardware.
    #[must_use]
    pub fn hardware(&self) -> bool {
        self.hardware
    }

    /// Returns the Direct3D device the frames belong to.
    ///
    /// The encoder has to be created on this same device, or every frame would need a copy
    /// across devices — exactly the copy this whole path exists to avoid.
    #[must_use]
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Waits for the next frame, giving up after `timeout`.
    pub fn poll(&mut self, timeout: Duration) -> Option<CapturedFrame> {
        self.frames.recv_timeout(timeout).ok()
    }
}

impl Drop for ScreenCapture {
    /// Stops the session and releases the pool.
    ///
    /// Both explicitly rather than by refcount, because the compositor keeps delivering
    /// into a pool that is still alive and a session left running holds a capture indicator
    /// on the user's screen.
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

impl core::fmt::Debug for ScreenCapture {
    /// Describes the session without reaching into COM objects.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ScreenCapture")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("hardware", &self.hardware)
            .finish_non_exhaustive()
    }
}

/// Creates a Direct3D 11 device, falling back to the software rasteriser.
///
/// Returns the device and whether it is backed by real hardware.
fn create_device() -> Result<(ID3D11Device, bool), CaptureError> {
    for (driver, hardware) in [(D3D_DRIVER_TYPE_HARDWARE, true), (WARP, false)] {
        let mut device: Option<ID3D11Device> = None;

        // SAFETY: every pointer argument is either null or a live local, and the output
        // parameter is only read when the call reports success.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                Default::default(),
                // BGRA support is required by the capture pool's pixel format.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };

        if result.is_ok() {
            if let Some(device) = device {
                return Ok((device, hardware));
            }
        }
    }

    Err(CaptureError::Start {
        reason: "no Direct3D 11 device could be created, with hardware or software".to_owned(),
    })
}

/// Wraps a Direct3D device in the WinRT device the capture pool wants.
fn winrt_device_for(device: &ID3D11Device) -> Result<IDirect3DDevice, CaptureError> {
    let dxgi: IDXGIDevice = device.cast().map_err(start_error)?;

    // SAFETY: the DXGI device came from a live Direct3D device, which is what this call
    // requires; it returns a new WinRT object that owns its own reference.
    let inspectable =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(start_error)?;

    inspectable.cast().map_err(start_error)
}

/// Builds a capture item for the primary monitor.
///
/// The interop interface is the only way to capture a display without a picker dialog,
/// which a headless host cannot show.
fn capture_primary_monitor() -> Result<GraphicsCaptureItem, CaptureError> {
    // SAFETY: the point is a plain value and the flag asks for the primary monitor when it
    // falls on none, so the call cannot fail to name a monitor on a machine that has one.
    let monitor: HMONITOR =
        unsafe { MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY) };
    if monitor.is_invalid() {
        return Err(CaptureError::NoDisplay);
    }

    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(start_error)?;

    // SAFETY: the monitor handle is valid and the type parameter names the interface the
    // interop factory produces for it.
    unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(monitor) }.map_err(start_error)
}

/// Turns a Windows error into a capture failure that names it.
fn start_error(error: windows::core::Error) -> CaptureError {
    CaptureError::Start {
        reason: error.to_string(),
    }
}
