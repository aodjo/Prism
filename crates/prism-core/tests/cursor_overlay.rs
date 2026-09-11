//! Tests for the bitmaps drawn over the video.
//!
//! Both of them are CoreGraphics drawing into a bitmap that becomes a texture, and both
//! failure modes here are silent. An arrow that is upside down still compiles, still
//! uploads, and still draws — it just points the wrong way, because a bitmap context puts
//! user-space y zero at the bottom while laying row zero out as the top of the image. And a
//! colour set from a loose component array is read against whatever colour space the
//! context happens to have, so four RGBA components can quietly become transparent black.
//!
//! Neither shows up anywhere except in the pixels, so these read the pixels.

#![cfg(target_os = "macos")]

use core::ptr::NonNull;

use objc2_metal::{MTLCreateSystemDefaultDevice, MTLOrigin, MTLRegion, MTLSize, MTLTexture};
use prism_core::render::cursor::CursorOverlay;
use prism_core::render::overlay::TextOverlay;

/// Width and height of the cursor bitmap, matching the renderer's own.
const SIZE: usize = 40;

/// Reads the cursor texture back as RGBA bytes.
fn cursor_pixels() -> Vec<u8> {
    let device = MTLCreateSystemDefaultDevice().expect("a Metal device is available");
    let overlay = CursorOverlay::new(&device).expect("the cursor is drawable");

    let mut pixels = vec![0u8; SIZE * SIZE * 4];

    // SAFETY: the buffer holds exactly the bytes the region covers, and the texture uses
    // shared storage, so the CPU may read it directly.
    unsafe {
        overlay
            .texture()
            .getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(pixels.as_mut_ptr().cast()).expect("buffer is non-null"),
                SIZE * 4,
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width: SIZE,
                        height: SIZE,
                        depth: 1,
                    },
                },
                0,
            );
    }

    pixels
}

/// Returns the alpha of one pixel, where `y` counts down from the top of the image.
fn alpha_at(pixels: &[u8], x: usize, y: usize) -> u8 {
    pixels[(y * SIZE + x) * 4 + 3]
}

#[test]
fn the_tip_is_at_the_top_left_where_the_hotspot_is() {
    let pixels = cursor_pixels();

    // Not the very corner pixel: the tip is a point, so the pixel straddling it is half
    // covered. Three rows down the wedge has opened enough to be solid.
    assert!(
        alpha_at(&pixels, 1, 3) > 200,
        "the arrow's tip is the hotspot, and the hotspot is the bitmap's top left corner"
    );
}

#[test]
fn the_far_corners_are_empty() {
    let pixels = cursor_pixels();

    for (x, y, corner) in [
        (SIZE - 1, SIZE - 1, "bottom right"),
        (SIZE - 1, 0, "top right"),
        (0, SIZE - 1, "bottom left"),
    ] {
        assert_eq!(
            alpha_at(&pixels, x, y),
            0,
            "the {corner} corner is outside the arrow and must be transparent"
        );
    }
}

#[test]
fn the_arrow_points_down_and_right_rather_than_up() {
    // The left edge of the arrow runs straight down from the tip, so there is ink well
    // below the tip and none well above where the tip already is. Flipping the bitmap
    // vertically would swap exactly these two.
    let pixels = cursor_pixels();

    assert!(
        alpha_at(&pixels, 1, 20) > 200,
        "the arrow's left edge runs downward from the tip"
    );
    assert_eq!(
        alpha_at(&pixels, 1, SIZE - 5),
        0,
        "and stops before the bottom of the bitmap"
    );
}

#[test]
fn the_fill_is_light_and_the_edge_is_dark() {
    // White on black, so the cursor is visible whatever is behind it. Interior pixels are
    // the fill; the outline is what makes it readable against a light picture.
    let pixels = cursor_pixels();
    let interior = (10 * SIZE + 3) * 4;

    assert!(
        pixels[interior] > 200 && pixels[interior + 1] > 200 && pixels[interior + 2] > 200,
        "the inside of the arrow is filled light, got {:?}",
        &pixels[interior..interior + 4]
    );

    let darkest = (0..SIZE)
        .flat_map(|y| (0..SIZE).map(move |x| (x, y)))
        .filter(|&(x, y)| alpha_at(&pixels, x, y) > 200)
        .map(|(x, y)| pixels[(y * SIZE + x) * 4])
        .min()
        .expect("the arrow has opaque pixels");

    assert!(
        darkest < 80,
        "some opaque pixel is the dark outline, darkest was {darkest}"
    );
}

#[test]
fn the_backdrop_behind_the_statistics_is_actually_painted() {
    // It was not, for as long as the overlay existed. The panel colour was handed over as
    // four raw components, which the bitmap context read against a colour space that wanted
    // two, giving a black at zero alpha — invisible, and invisible in exactly the case the
    // panel exists for, white text over a bright picture.
    let device = MTLCreateSystemDefaultDevice().expect("a Metal device is available");
    let mut overlay = TextOverlay::new(&device, 64, 32, 12.0).expect("the overlay is creatable");
    overlay.update(&["x".to_owned()]);

    let (width, height) = (overlay.width(), overlay.height());
    let mut pixels = vec![0u8; width * height * 4];

    // SAFETY: the buffer holds exactly the bytes the region covers, and the texture uses
    // shared storage, so the CPU may read it directly.
    unsafe {
        overlay
            .texture()
            .getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(pixels.as_mut_ptr().cast()).expect("buffer is non-null"),
                width * 4,
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize {
                        width,
                        height,
                        depth: 1,
                    },
                },
                0,
            );
    }

    // The far end of the line's own row, so what is there is the panel and nothing else. The
    // panel covers the lines there are rather than the whole bitmap, which is why this samples
    // beside the text rather than in the corner furthest from it.
    let beside = (width + (width - 2)) * 4;
    assert!(
        pixels[beside + 3] > 100,
        "the panel behind the text should be mostly opaque, got rgba {:?}",
        &pixels[beside..beside + 4]
    );

    // And below the last line there is nothing, so a session with little to say does not put
    // a black box over the picture the size of the most it could ever say.
    let under = ((height - 2) * width + (width - 2)) * 4;
    assert_eq!(
        pixels[under + 3],
        0,
        "the bitmap past the last line should be clear, got rgba {:?}",
        &pixels[under..under + 4]
    );
}
