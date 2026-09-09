//! The cursor bitmap the client draws for itself.
//!
//! The host excludes its cursor from the captured video so the client can draw one that
//! answers the mouse immediately rather than a video frame later. That leaves the client
//! needing a cursor bitmap, and until the control channel carries real cursor shapes there
//! is one: a plain arrow.
//!
//! Drawn here in arithmetic rather than by each platform's drawing library, for two reasons.
//! The two clients then show the same cursor by construction instead of by inspection, and
//! the shape can be tested anywhere rather than only on the machine whose drawing library
//! produced it.

/// Width and height of the cursor bitmap in pixels.
///
/// Sized for the drawable, which is in physical pixels, so this is roughly the size a system
/// cursor occupies on a display with a two-times backing scale.
pub const SIZE: usize = 40;

/// Bytes per pixel in the cursor bitmap.
pub const BYTES_PER_PIXEL: usize = 4;

/// The arrow outline, in pixels measured right and down from the tip.
///
/// The tip is the hotspot and sits at the origin, so placing the bitmap's top left corner at
/// the pointer position puts the tip exactly where the pointer is.
const ARROW: [(f32, f32); 7] = [
    (0.0, 0.0),
    (0.0, 26.0),
    (6.5, 19.5),
    (10.8, 30.0),
    (15.4, 28.0),
    (11.2, 18.0),
    (19.0, 18.0),
];

/// How thick the outline is, in pixels.
///
/// Centred on the path, half inside the shape and half outside, so the outline does not
/// enlarge the arrow the way an outward stroke would.
const OUTLINE_WIDTH: f32 = 1.6;

/// How many samples per pixel per axis are taken to find the edges.
///
/// Four, so sixteen per pixel. The bitmap is built once at startup, which is why this can be
/// brute force rather than clever: the whole thing is under two hundred thousand distance
/// computations and never runs again.
const SUPERSAMPLE: usize = 4;

/// Draws the arrow into a fresh RGBA bitmap with premultiplied alpha.
///
/// Rows run top down, so the first row of the returned buffer is the top of the image and
/// the tip sits at its top left corner. Premultiplied because that is what both renderers'
/// blend expects: white text and a white arrow are stored with their colour already scaled
/// by their coverage.
#[must_use]
pub fn bitmap() -> Vec<u8> {
    let mut pixels = vec![0u8; SIZE * SIZE * BYTES_PER_PIXEL];
    let step = 1.0 / SUPERSAMPLE as f32;
    let half_width = OUTLINE_WIDTH / 2.0;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut inside = 0u32;
            let mut edge = 0u32;

            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let at = (
                        x as f32 + (sx as f32 + 0.5) * step,
                        y as f32 + (sy as f32 + 0.5) * step,
                    );
                    let distance = signed_distance(at);

                    if distance <= -half_width {
                        inside += 1;
                    } else if distance <= half_width {
                        edge += 1;
                    }
                }
            }

            let samples = (SUPERSAMPLE * SUPERSAMPLE) as u32;
            if inside + edge == 0 {
                continue;
            }

            // The fill is white and the outline black, both fully opaque where they cover.
            // Premultiplied, so the colour channels carry the white coverage alone while
            // alpha carries both.
            let white = to_byte(inside, samples);
            let alpha = to_byte(inside + edge, samples);
            let at = (y * SIZE + x) * BYTES_PER_PIXEL;

            pixels[at] = white;
            pixels[at + 1] = white;
            pixels[at + 2] = white;
            pixels[at + 3] = alpha;
        }
    }

    pixels
}

/// Scales a sample count to a byte, rounding to nearest.
fn to_byte(covered: u32, total: u32) -> u8 {
    ((covered * 255 + total / 2) / total) as u8
}

/// Returns the distance from a point to the arrow's outline, negative inside it.
fn signed_distance(at: (f32, f32)) -> f32 {
    let mut nearest = f32::MAX;

    for index in 0..ARROW.len() {
        let from = ARROW[index];
        let to = ARROW[(index + 1) % ARROW.len()];
        nearest = nearest.min(distance_to_segment(at, from, to));
    }

    if contains(at) { -nearest } else { nearest }
}

/// Returns the distance from a point to a line segment.
fn distance_to_segment(at: (f32, f32), from: (f32, f32), to: (f32, f32)) -> f32 {
    let along = (to.0 - from.0, to.1 - from.1);
    let offset = (at.0 - from.0, at.1 - from.1);
    let length = along.0.mul_add(along.0, along.1 * along.1);

    // A zero-length edge would divide by zero. The polygon has none, but the guard costs
    // nothing and keeps the function total.
    let t = if length > 0.0 {
        (offset.0.mul_add(along.0, offset.1 * along.1) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let closest = (
        along.0.mul_add(t, from.0) - at.0,
        along.1.mul_add(t, from.1) - at.1,
    );

    closest.0.hypot(closest.1)
}

/// Returns whether a point lies inside the arrow, by casting a ray along positive x.
fn contains(at: (f32, f32)) -> bool {
    let mut inside = false;

    for index in 0..ARROW.len() {
        let from = ARROW[index];
        let to = ARROW[(index + 1) % ARROW.len()];

        // Only edges the horizontal ray actually crosses count, and the half-open comparison
        // is what keeps a vertex on the ray from being counted twice.
        if (from.1 > at.1) != (to.1 > at.1) {
            let crossing = (to.0 - from.0).mul_add((at.1 - from.1) / (to.1 - from.1), from.0);
            if at.0 < crossing {
                inside = !inside;
            }
        }
    }

    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads one pixel's red and alpha out of the bitmap.
    fn pixel(pixels: &[u8], x: usize, y: usize) -> (u8, u8) {
        let at = (y * SIZE + x) * BYTES_PER_PIXEL;
        (pixels[at], pixels[at + 3])
    }

    #[test]
    fn the_bitmap_is_the_size_it_says() {
        assert_eq!(bitmap().len(), SIZE * SIZE * BYTES_PER_PIXEL);
    }

    #[test]
    fn the_body_of_the_arrow_is_opaque_white() {
        let pixels = bitmap();

        // Well inside the shaft, clear of the outline on either side.
        let (white, alpha) = pixel(&pixels, 3, 10);
        assert_eq!(alpha, 255, "the arrow's body should be opaque");
        assert_eq!(white, 255, "the arrow's body should be white");
    }

    #[test]
    fn the_outline_is_opaque_and_dark() {
        let pixels = bitmap();

        // The left edge of the shaft runs down x = 0, so the first column is outline.
        let (white, alpha) = pixel(&pixels, 0, 10);
        assert_eq!(alpha, 255, "the outline should be opaque");
        assert!(white < 128, "the outline should be dark, got {white}");
    }

    #[test]
    fn everything_beyond_the_arrow_is_clear() {
        let pixels = bitmap();

        for at in [(SIZE - 1, 0), (SIZE - 1, SIZE - 1), (0, SIZE - 1), (30, 4)] {
            let (_, alpha) = pixel(&pixels, at.0, at.1);
            assert_eq!(alpha, 0, "{at:?} is outside the arrow and should be clear");
        }
    }

    #[test]
    fn the_tip_is_where_the_hotspot_is() {
        let pixels = bitmap();

        // The tip is the origin, so the very first pixel is covered while the one to its
        // right, across the arrow's leading edge, is not.
        assert!(pixel(&pixels, 0, 0).1 > 0, "the tip should be drawn");
        assert_eq!(
            pixel(&pixels, 6, 1).1,
            0,
            "the arrow does not reach right at the tip"
        );
    }

    #[test]
    fn colour_never_exceeds_alpha() {
        let pixels = bitmap();

        // What premultiplied means. A pixel whose colour outran its alpha would brighten
        // whatever it was blended over instead of tinting it.
        for at in (0..pixels.len()).step_by(BYTES_PER_PIXEL) {
            let (white, alpha) = (pixels[at], pixels[at + 3]);
            assert!(
                white <= alpha,
                "pixel {at} has colour {white} over alpha {alpha}"
            );
        }
    }
}
