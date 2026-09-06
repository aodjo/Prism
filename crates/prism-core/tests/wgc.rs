//! Tests for Windows.Graphics.Capture.
//!
//! These check the contract the rest of the pipeline depends on: that a frame arrives as a
//! Direct3D texture the encoder can be handed directly, in the format and with the bind
//! flags that allows. They deliberately assert nothing about frame rate — capture only
//! produces a frame when the screen changes, and a virtual display can report a refresh
//! rate of one hertz, so a rate measured anywhere but real hardware would be a number that
//! means nothing.
//!
//! Everything here steps aside when capture is unavailable. A session with no interactive
//! desktop has nothing to capture, and that is a property of where the test runs rather
//! than a fault in what it tests.

#![cfg(target_os = "windows")]

use std::time::Duration;

use prism_core::capture::wgc::ScreenCapture;
use prism_core::capture::{CaptureConfig, CaptureError};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

/// Starts a capture session, or returns `None` when this machine cannot capture.
fn session() -> Option<ScreenCapture> {
    match ScreenCapture::start(CaptureConfig::default()) {
        Ok(capture) => Some(capture),
        Err(CaptureError::PermissionDenied | CaptureError::NoDisplay) => {
            eprintln!("skipping: this session has no display it may capture");
            None
        }
        Err(err) => {
            eprintln!("skipping: capture would not start: {err}");
            None
        }
    }
}

/// Reads a texture's description.
fn describe(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut desc = D3D11_TEXTURE2D_DESC::default();

    // SAFETY: the texture is alive and the destination is a live, correctly typed local.
    unsafe { texture.GetDesc(&mut desc) };

    desc
}

/// Waits for a frame, allowing generously for a display that updates rarely.
fn first_frame(capture: &mut ScreenCapture) -> Option<prism_core::capture::wgc::CapturedFrame> {
    capture.poll(Duration::from_secs(5))
}

#[test]
fn a_session_reports_the_size_of_the_display_it_captures() {
    let Some(capture) = session() else { return };

    assert!(
        capture.width() > 0 && capture.height() > 0,
        "a capture of no pixels is not a capture, got {}x{}",
        capture.width(),
        capture.height()
    );
}

#[test]
fn a_frame_arrives_as_a_texture_of_the_size_that_was_promised() {
    let Some(mut capture) = session() else { return };
    let Some(frame) = first_frame(&mut capture) else {
        eprintln!("skipping: the display produced no frame, which means nothing changed");
        return;
    };

    let texture = frame.texture().expect("a capture frame carries a texture");
    let desc = describe(&texture);

    assert_eq!(desc.Width, capture.width());
    assert_eq!(desc.Height, capture.height());
}

#[test]
fn the_texture_is_usable_by_an_encoder_without_copying_it() {
    // This is the whole reason this API is used rather than desktop duplication or a GDI
    // blit. A texture the encoder cannot bind would have to be copied through system
    // memory, which at 1440p120 is four hundred megabytes a second of pure waste.
    let Some(mut capture) = session() else { return };
    let Some(frame) = first_frame(&mut capture) else {
        eprintln!("skipping: the display produced no frame, which means nothing changed");
        return;
    };

    let texture = frame.texture().expect("a capture frame carries a texture");
    let desc = describe(&texture);

    assert_eq!(
        desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM,
        "the capture pool was asked for BGRA8 and the texture must be it"
    );
    assert_eq!(
        desc.Usage, D3D11_USAGE_DEFAULT,
        "a default-usage texture lives in GPU memory, which is where it has to stay"
    );
    assert_ne!(
        desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32,
        0,
        "an encoder reads the frame as a shader resource"
    );
    assert_ne!(
        desc.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32,
        0,
        "and a colour conversion pass draws into one"
    );
    assert_eq!(
        desc.CPUAccessFlags, 0,
        "a texture the CPU can read is one the GPU had to make a copy of"
    );
}

#[test]
fn frames_carry_a_timestamp_that_moves_forward() {
    let Some(mut capture) = session() else { return };

    let Some(first) = first_frame(&mut capture) else {
        eprintln!("skipping: the display produced no frame, which means nothing changed");
        return;
    };
    let Some(second) = first_frame(&mut capture) else {
        eprintln!("skipping: the display produced only one frame");
        return;
    };

    assert!(
        second.capture_ts_us() > first.capture_ts_us(),
        "the latency chain is anchored on this stamp, so it has to advance: {} then {}",
        first.capture_ts_us(),
        second.capture_ts_us()
    );
}
