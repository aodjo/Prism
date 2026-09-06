//! Tests for the Direct3D BGRA to NV12 conversion.
//!
//! A colour conversion that is subtly wrong produces a picture that looks fine until it is
//! compared against something, so this compares it against something. The expected values
//! are BT.709 video range, computed from the standard rather than from this shader, and
//! three of them are the exact values the Metal renderer is independently tested to decode
//! back into white, black and grey. Two shaders written from opposite directions agreeing
//! on the same numbers is evidence; either one agreeing with itself is not.

#![cfg(target_os = "windows")]

use prism_core::encode::nv12::{Bgra2Nv12, Nv12Texture};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

/// Direct3D's software rasteriser, used when no adapter provides a hardware device.
const WARP: D3D_DRIVER_TYPE = D3D_DRIVER_TYPE(5);

/// Size of the test frame. Small, because the conversion is per-pixel and a larger frame
/// would prove nothing more.
const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;

/// How far a converted sample may sit from the reference before it counts as wrong.
///
/// The shader works in floating point and the result is quantised to a byte, so a value
/// either side of the reference is expected. Anything further is a different matrix, a
/// different range, or a linearisation that should not be there.
const TOLERANCE: i32 = 2;

/// Creates a Direct3D device, or returns `None` if this machine has none.
fn device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, WARP] {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        // SAFETY: every pointer argument is null or a live local, and the outputs are read
        // only when the call reports success.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };

        if result.is_ok() {
            if let (Some(device), Some(context)) = (device, context) {
                return Some((device, context));
            }
        }
    }

    eprintln!("skipping: no Direct3D 11 device is available");
    None
}

/// Builds a BGRA texture filled with one colour.
fn solid_source(device: &ID3D11Device, red: u8, green: u8, blue: u8) -> ID3D11Texture2D {
    let pixels: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|_| [blue, green, red, 255])
        .collect();

    let desc = D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
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
        SysMemPitch: WIDTH * 4,
        SysMemSlicePitch: 0,
    };

    let mut texture = None;

    // SAFETY: the description and the initial data agree on the size, the pixel buffer
    // outlives the call, and the output is a live local.
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }
        .expect("a BGRA texture is allocatable");

    texture.expect("Direct3D produced the texture it reported")
}

/// Copies an NV12 texture to system memory and returns one luma and one chroma sample.
///
/// Reading back is exactly what the conversion exists to avoid on the frame path. It is
/// what a test has to do, and only a test does it.
fn sample(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    target: &Nv12Texture,
) -> (u8, u8, u8) {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: target.width(),
        Height: target.height(),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };

    let mut staging = None;

    // SAFETY: the description is fully initialised and the output is a live local.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
        .expect("a staging texture is allocatable");
    let staging = staging.expect("Direct3D produced the staging texture it reported");

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();

    // SAFETY: both textures share a description, the map is released before returning, and
    // the mapped pointer is read only within the rows the pitch describes.
    let (luma, cb, cr) = unsafe {
        context.CopyResource(&staging, target.texture());
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .expect("a staging texture is mappable");

        let base = mapped.pData.cast::<u8>();
        let pitch = mapped.RowPitch as usize;

        // The middle of the frame, well away from any edge the sampler clamps at.
        let row = target.height() as usize / 2;
        let column = target.width() as usize / 2;
        let luma = *base.add(row * pitch + column);

        // The chroma plane follows the luma plane, which occupies one pitch per row.
        let chroma_base = base.add(target.height() as usize * pitch);
        let chroma_row = row / 2;
        let chroma_column = (column / 2) * 2;
        let cb = *chroma_base.add(chroma_row * pitch + chroma_column);
        let cr = *chroma_base.add(chroma_row * pitch + chroma_column + 1);

        context.Unmap(&staging, 0);

        (luma, cb, cr)
    };

    (luma, cb, cr)
}

/// Converts one solid colour and returns the Y, Cb and Cr it produced.
fn convert(red: u8, green: u8, blue: u8) -> Option<(u8, u8, u8)> {
    let (device, context) = device()?;

    let source = solid_source(&device, red, green, blue);
    let target = Nv12Texture::new(&device, WIDTH, HEIGHT).expect("an NV12 texture is allocatable");
    let converter = Bgra2Nv12::new(&device).expect("the conversion shaders compile");

    converter
        .convert(&source, &target)
        .expect("a bound source converts");

    Some(sample(&device, &context, &target))
}

/// Asserts a converted colour matches a reference triple within tolerance.
fn assert_ycbcr(actual: (u8, u8, u8), expected: (u8, u8, u8), label: &str) {
    let pairs = [
        ("Y", actual.0, expected.0),
        ("Cb", actual.1, expected.1),
        ("Cr", actual.2, expected.2),
    ];

    for (name, got, want) in pairs {
        let difference = i32::from(got) - i32::from(want);
        assert!(
            difference.abs() <= TOLERANCE,
            "{label}: {name} was {got}, expected {want} (off by {difference}); \
             got {actual:?} against {expected:?}"
        );
    }
}

#[test]
fn white_becomes_the_value_the_metal_renderer_decodes_back_to_white() {
    // 235, 128, 128 is the exact triple `video_range_white_becomes_white` feeds the Metal
    // shader and gets pure white out of. Two shaders written from opposite directions
    // meeting on the same number is the check worth having.
    let Some(actual) = convert(255, 255, 255) else {
        return;
    };

    assert_ycbcr(actual, (235, 128, 128), "white");
}

#[test]
fn black_becomes_the_value_the_metal_renderer_decodes_back_to_black() {
    let Some(actual) = convert(0, 0, 0) else {
        return;
    };

    assert_ycbcr(actual, (16, 128, 128), "black");
}

#[test]
fn mid_grey_lands_where_the_metal_renderer_expects_it() {
    let Some(actual) = convert(128, 128, 128) else {
        return;
    };

    assert_ycbcr(actual, (126, 128, 128), "grey");
}

#[test]
fn the_primaries_match_bt709_video_range() {
    // Computed from the standard, not from the shader: Y' = 16 + 219*Y,
    // Cb' = 128 + 224*(B - Y)/1.8556, Cr' = 128 + 224*(R - Y)/1.5748, with the BT.709
    // luma weights 0.2126, 0.7152 and 0.0722. These are what separate BT.709 from BT.601,
    // which is the mistake this test exists to catch.
    for (red, green, blue, expected, label) in [
        (255, 0, 0, (63, 102, 240), "red"),
        (0, 255, 0, (173, 42, 26), "green"),
        (0, 0, 255, (32, 240, 118), "blue"),
    ] {
        let Some(actual) = convert(red, green, blue) else {
            return;
        };

        assert_ycbcr(actual, expected, label);
    }
}

#[test]
fn a_frame_with_an_odd_dimension_is_refused_rather_than_half_converted() {
    // NV12 subsamples chroma by two in both directions, so an odd dimension has no
    // representation. Accepting one would silently drop a row or a column.
    let Some((device, _)) = device() else { return };

    assert!(Nv12Texture::new(&device, 63, 64).is_err());
    assert!(Nv12Texture::new(&device, 64, 63).is_err());
    assert!(Nv12Texture::new(&device, 0, 64).is_err());
}
