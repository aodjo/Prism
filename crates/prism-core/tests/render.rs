//! Tests for the Metal renderer.
//!
//! The renderer's job is a colour conversion, so the test does the only thing that can
//! prove it: paint a picture in NV12, render it, read the result back, and check the
//! pixels are the colours they should be.

#![cfg(target_os = "macos")]

use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLRegion, MTLSize, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};
use prism_core::encode::videotoolbox::Nv12Frame;
use prism_core::render::metal::MetalRenderer;

const WIDTH: usize = 64;
const HEIGHT: usize = 64;

/// Fills a frame with one BT.709 video-range colour.
fn fill(frame: &mut Nv12Frame, luma: u8, cb: u8, cr: u8) {
    frame
        .fill(|y_plane, y_stride, uv_plane, uv_stride| {
            for row in 0..HEIGHT {
                for column in 0..WIDTH {
                    y_plane[row * y_stride + column] = luma;
                }
            }
            for row in 0..HEIGHT / 2 {
                for column in 0..WIDTH / 2 {
                    uv_plane[row * uv_stride + column * 2] = cb;
                    uv_plane[row * uv_stride + column * 2 + 1] = cr;
                }
            }
        })
        .expect("a freshly created buffer can be locked");
}

/// Renders a solid colour and returns the BGRA pixel at the centre of the result.
fn render_solid(luma: u8, cb: u8, cr: u8) -> [u8; 4] {
    let mut renderer = MetalRenderer::new(MTLPixelFormat::BGRA8Unorm).expect("renderer starts");
    let mut frame = Nv12Frame::new(WIDTH as u32, HEIGHT as u32).expect("buffer is allocatable");
    fill(&mut frame, luma, cb, cr);

    // SAFETY: the descriptor constructor takes plain values and returns a new object.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            WIDTH,
            HEIGHT,
            false,
        )
    };
    descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
    descriptor.setStorageMode(MTLStorageMode::Shared);

    let target = renderer
        .device()
        .newTextureWithDescriptor(&descriptor)
        .expect("the target texture is allocatable");

    renderer
        .draw(frame.pixel_buffer(), &target)
        .expect("the picture draws");

    let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
    // SAFETY: the buffer holds exactly the bytes the requested region covers, and the
    // texture uses shared storage so the CPU may read it after the GPU has finished.
    unsafe {
        target.getBytes_bytesPerRow_fromRegion_mipmapLevel(
            core::ptr::NonNull::new(pixels.as_mut_ptr().cast()).expect("buffer is non-null"),
            WIDTH * 4,
            MTLRegion {
                origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize {
                    width: WIDTH,
                    height: HEIGHT,
                    depth: 1,
                },
            },
            0,
        );
    }

    let centre = ((HEIGHT / 2) * WIDTH + WIDTH / 2) * 4;
    [
        pixels[centre],
        pixels[centre + 1],
        pixels[centre + 2],
        pixels[centre + 3],
    ]
}

/// Asserts a BGRA pixel is the expected RGB within the tolerance a lossy conversion needs.
fn assert_rgb(actual: [u8; 4], expected: [u8; 3], label: &str) {
    let rgb = [actual[2], actual[1], actual[0]];
    for channel in 0..3 {
        let difference = i32::from(rgb[channel]).abs_diff(i32::from(expected[channel]));
        assert!(
            difference <= 4,
            "{label}: expected rgb {expected:?}, got {rgb:?} (channel {channel} off by {difference})"
        );
    }
    assert_eq!(actual[3], 255, "{label}: alpha must be opaque");
}

#[test]
fn video_range_white_becomes_white() {
    assert_rgb(render_solid(235, 128, 128), [255, 255, 255], "white");
}

#[test]
fn video_range_black_becomes_black() {
    assert_rgb(render_solid(16, 128, 128), [0, 0, 0], "black");
}

#[test]
fn video_range_grey_becomes_grey() {
    assert_rgb(render_solid(126, 128, 128), [128, 128, 128], "grey");
}

#[test]
fn a_chroma_shift_produces_colour_rather_than_grey() {
    let reddish = render_solid(81, 90, 240);
    let greenish = render_solid(145, 54, 34);
    let blueish = render_solid(41, 240, 110);

    assert!(
        reddish[2] > reddish[1] && reddish[2] > reddish[0],
        "expected red, got {reddish:?}"
    );
    assert!(
        greenish[1] > greenish[2] && greenish[1] > greenish[0],
        "expected green, got {greenish:?}"
    );
    assert!(
        blueish[0] > blueish[1] && blueish[0] > blueish[2],
        "expected blue, got {blueish:?}"
    );
}
