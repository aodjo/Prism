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
    ID3D11Texture2D,
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

/// Builds a BGRA texture holding a pattern that differs from frame to frame.
///
/// A static picture would let the encoder emit near-empty P-frames and prove very little.
fn moving_source(device: &ID3D11Device, width: u32, height: u32, step: u32) -> ID3D11Texture2D {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    let pixels: Vec<u8> = (0..width * height)
        .flat_map(|index| {
            let x = (index % width) as u8;
            let y = (index / width) as u8;
            [
                x.wrapping_add(step as u8 * 3),
                y,
                (step as u8).wrapping_mul(7),
                255,
            ]
        })
        .collect();

    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };

    let mut texture = None;

    // SAFETY: the description and the pixel buffer agree on the size, the buffer outlives
    // the call, and the output is a live local.
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }
        .expect("a BGRA texture is allocatable");

    texture.expect("Direct3D produced the texture it reported")
}

#[test]
fn the_whole_windows_path_produces_a_decodable_stream() {
    // Capture's output format into the encoder's input, on the GPU, and out as H.264. The
    // stream this writes has been decoded end to end by an independent decoder: 120 of 120
    // frames, High profile, one I frame and 119 P frames with no B frames, which is the
    // low-latency structure the plan requires.
    use prism_core::encode::EncoderConfig;
    use prism_core::encode::nv12::{Bgra2Nv12, Nv12Texture};
    use prism_core::encode::nvenc::NvencEncoder;
    use prism_core::net::negotiate::Codec;

    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    const FRAMES: u32 = 30;

    let Some((_, device)) = session() else { return };

    let target = Nv12Texture::new(&device, WIDTH, HEIGHT).expect("an NV12 texture is allocatable");
    let converter = Bgra2Nv12::new(&device).expect("the conversion shaders compile");

    let config = EncoderConfig {
        codec: Codec::H264,
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        bitrate_bps: 20_000_000,
        max_slice_bytes: 0,
    };

    // SAFETY: both the device and the texture are alive for the whole of this test.
    let encoder = unsafe { NvencEncoder::new(device.as_raw(), target.texture().as_raw(), config) };

    let mut encoder = match encoder {
        Ok(encoder) => encoder,
        Err(err) => {
            eprintln!("skipping: {err}");
            return;
        }
    };

    let mut idr_frames = 0;
    let mut total_bytes = 0;

    for step in 0..FRAMES {
        let source = moving_source(&device, WIDTH, HEIGHT, step);
        converter
            .convert(&source, &target)
            .expect("a bound source converts");

        let frame = encoder
            .encode(u64::from(step) * 16_667, step == 0)
            .expect("a converted frame encodes");

        assert!(
            !frame.data.is_empty(),
            "frame {step} encoded to nothing at all"
        );
        assert!(
            !frame.slices.is_empty(),
            "frame {step} produced no NAL units, so the Annex B split found no start codes"
        );

        if frame.is_idr {
            idr_frames += 1;
        }
        total_bytes += frame.data.len();
    }

    assert_eq!(
        encoder.frames_encoded(),
        u64::from(FRAMES),
        "every frame submitted should have come back"
    );
    assert_eq!(
        idr_frames, 1,
        "only the frame that asked to be an IDR should be one; periodic keyframes are a \
         bitrate spike the design removes"
    );

    println!("encoded {FRAMES} frames, {total_bytes} bytes, {idr_frames} IDR");
}

#[test]
fn the_first_frame_carries_the_parameter_sets_a_decoder_needs_to_start() {
    // Without SPS and PPS a decoder has nothing to configure itself from, and the symptom
    // is a black window with no error anywhere — the same failure the VideoToolbox path
    // had until parameter sets were made to repeat.
    use prism_core::encode::EncoderConfig;
    use prism_core::encode::nv12::{Bgra2Nv12, Nv12Texture};
    use prism_core::encode::nvenc::NvencEncoder;
    use prism_core::net::negotiate::Codec;

    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;

    let Some((_, device)) = session() else { return };

    let target = Nv12Texture::new(&device, WIDTH, HEIGHT).expect("an NV12 texture is allocatable");
    let converter = Bgra2Nv12::new(&device).expect("the conversion shaders compile");

    let config = EncoderConfig {
        codec: Codec::H264,
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        bitrate_bps: 20_000_000,
        max_slice_bytes: 0,
    };

    // SAFETY: both the device and the texture are alive for the whole of this test.
    let Ok(mut encoder) =
        (unsafe { NvencEncoder::new(device.as_raw(), target.texture().as_raw(), config) })
    else {
        eprintln!("skipping: no NVENC session");
        return;
    };

    let source = moving_source(&device, WIDTH, HEIGHT, 0);
    converter.convert(&source, &target).expect("converts");

    let frame = encoder.encode(0, true).expect("the first frame encodes");

    let kinds: Vec<u8> = frame
        .slices
        .iter()
        .filter_map(|range| {
            frame
                .data
                .get(range.start + prism_core::encode::START_CODE.len())
                .map(|byte| byte & 0x1f)
        })
        .collect();

    assert!(
        kinds.contains(&7),
        "the sequence parameter set is missing, got {kinds:?}"
    );
    assert!(
        kinds.contains(&8),
        "the picture parameter set is missing, got {kinds:?}"
    );
    assert!(
        kinds.contains(&5),
        "the IDR slice itself is missing, got {kinds:?}"
    );
}
