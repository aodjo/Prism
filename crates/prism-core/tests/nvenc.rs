//! Tests for the NVENC capability probe.
//!
//! Everything here is a capability rather than a requirement. A machine with no NVIDIA
//! driver steps aside; a machine with one has its answers printed, because the answers
//! decide what the host can do rather than the other way round. That discipline is not
//! theoretical — Apple's encoder declares both a slice size limit and long-term references
//! in its headers and refuses both at run time.

#![cfg(target_os = "windows")]

use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::core::Interface;

use prism_core::encode::nvenc::{Capabilities, H264, HEVC, Nvenc};

/// Loads NVENC and opens a Direct3D device, or returns `None` when this machine has neither.
fn session() -> Option<(Nvenc, ID3D11Device)> {
    let nvenc = match Nvenc::load() {
        Ok(nvenc) => nvenc,
        Err(err) => {
            eprintln!("skipping: {err}");
            return None;
        }
    };

    let mut device: Option<ID3D11Device> = None;

    // SAFETY: every pointer argument is null or a live local, and the output is read only
    // when the call reports success.
    let result = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    };

    if result.is_err() {
        eprintln!("skipping: no hardware Direct3D device");
        return None;
    }

    Some((nvenc, device?))
}

/// Queries one codec's capabilities.
fn capabilities(codec: prism_core::encode::nvenc::CodecGuid) -> Option<Capabilities> {
    let (nvenc, device) = session()?;

    // SAFETY: the device was created above and is alive for the duration of the call.
    match unsafe { nvenc.capabilities(device.as_raw(), codec) } {
        Ok(capabilities) => Some(capabilities),
        Err(err) => {
            eprintln!("skipping: {err}");
            None
        }
    }
}

#[test]
fn the_driver_reports_an_api_version_this_build_can_negotiate() {
    let Some((nvenc, _)) = session() else { return };

    let (major, minor) = nvenc.driver_api_version();
    println!("NVENC driver API {major}.{minor}");

    assert!(
        major >= 9,
        "an API older than 9 predates everything this design needs, got {major}.{minor}"
    );
}

#[test]
fn h264_offers_what_the_latency_design_depends_on() {
    // Each of these is a plan decision that Apple's encoder cannot honour, which is why the
    // Windows host exists. If any becomes false on some future driver, the host has to know
    // rather than silently lose the property.
    let Some(caps) = capabilities(H264) else {
        return;
    };
    println!("H.264 {caps:?}");

    assert!(
        caps.reference_invalidation,
        "recovering from loss without a keyframe needs reference invalidation"
    );
    assert!(
        caps.max_ltr_frames > 0,
        "reference invalidation is useless without long-term references to fall back to"
    );
    assert!(
        caps.dynamic_slice_mode,
        "slice-level streaming needs the encoder to cut frames on demand"
    );
    assert!(
        caps.dynamic_bitrate,
        "congestion control has no actuator without a runtime bitrate"
    );
    assert!(
        caps.async_encode,
        "polling for completion costs latency the event does not"
    );
    assert!(
        caps.intra_refresh,
        "intra refresh is what removes the keyframe bitrate spike"
    );
}

#[test]
fn hevc_is_offered_too_so_the_codec_can_be_negotiated() {
    let Some(caps) = capabilities(HEVC) else {
        return;
    };
    println!("HEVC {caps:?}");

    assert!(caps.max_width >= 1920 && caps.max_height >= 1080);
}

#[test]
fn the_frame_rate_ceiling_is_reported_rather_than_assumed() {
    // The number that decides whether a resolution and rate are reachable at all. It is
    // reported rather than asserted against a target, because it is a property of whichever
    // card the host runs on.
    let Some(caps) = capabilities(H264) else {
        return;
    };

    for (width, height, label) in [
        (1280, 720, "720p"),
        (1920, 1080, "1080p"),
        (2560, 1440, "1440p"),
    ] {
        println!(
            "H.264 ceiling at {label}: {} fps",
            caps.max_fps_for(width, height)
        );
    }

    assert!(
        caps.max_fps_for(1920, 1080) >= 60,
        "an encoder that cannot hold 1080p60 cannot run this pipeline at all"
    );
}

#[test]
fn a_resolution_larger_than_the_encoder_accepts_reports_no_rate() {
    // Zero rather than a number the caller might act on. An encoder that cannot take the
    // frame at all has no frame rate for it.
    let caps = Capabilities {
        max_width: 4096,
        max_height: 4096,
        dynamic_bitrate: true,
        intra_refresh: true,
        dynamic_slice_mode: true,
        reference_invalidation: true,
        max_ltr_frames: 8,
        async_encode: true,
        max_macroblocks_per_second: 983_040,
        encoder_engines: 1,
    };

    assert_eq!(caps.max_fps_for(7680, 4320), 0);
    assert_eq!(caps.max_fps_for(0, 1080), 0);
    assert_eq!(caps.max_fps_for(1920, 0), 0);
}
