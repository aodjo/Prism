//! Drawing decoded pictures.
//!
//! The decoder hands over pictures in the GPU's own memory, and the renderer's job is to
//! get them onto the screen without ever copying them through system memory. On macOS a
//! decoded `CVPixelBuffer` is IOSurface backed, so it can be bound as a Metal texture
//! directly; on Windows a decoded picture is an NV12 `ID3D11Texture2D` bound as two views,
//! one per plane. Either way the colour conversion from NV12 to RGB happens in a fragment
//! shader rather than on the CPU.
//!
//! Both backends draw the same two things over the picture — the statistics panel and the
//! cursor — as textured rectangles with premultiplied alpha, and both place them with
//! [`place`]. What differs between them is the drawing API, not the arithmetic.

pub mod arrow;
#[cfg(target_os = "macos")]
pub mod cursor;
#[cfg(target_os = "windows")]
pub mod d3d11;
#[cfg(target_os = "windows")]
pub mod hud;
#[cfg(target_os = "macos")]
pub mod metal;
#[cfg(target_os = "macos")]
pub mod overlay;
pub mod pacing;

/// Reason a picture could not be drawn.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// No GPU was available, or it refused to create something the renderer needs.
    #[error("could not set up the renderer: {reason}")]
    Setup {
        /// What went wrong.
        reason: &'static str,
    },

    /// The shader source failed to compile.
    #[error("shader compilation failed: {message}")]
    Shader {
        /// What the compiler reported.
        message: String,
    },

    /// A decoded picture could not be bound as a texture.
    #[error("could not bind the picture as a texture (status {status})")]
    Bind {
        /// Platform status code.
        status: i32,
    },
}

/// Places a bitmap of `width` by `height` pixels with its top left corner at a point.
///
/// The point is a fraction of the target, the size is in pixels, and the result is the
/// rectangle's left, top, width and height in normalised device coordinates, where y grows
/// upwards. Sizing in pixels rather than fractions is what keeps text legible and a cursor
/// cursor-sized whatever the window is scaled to, instead of stretching them with it.
///
/// Both renderers use this, and both take the result as four floats their overlay vertex
/// shader turns into a rectangle. Metal and Direct3D agree on clip space, so the arithmetic
/// does not have to know which one it is feeding.
#[must_use]
pub fn place(
    at: (f32, f32),
    width: usize,
    height: usize,
    target_width: usize,
    target_height: usize,
) -> [f32; 4] {
    let target_width = target_width.max(1) as f32;
    let target_height = target_height.max(1) as f32;

    [
        at.0.mul_add(2.0, -1.0),
        at.1.mul_add(-2.0, 1.0),
        (width as f32 / target_width) * 2.0,
        (height as f32 / target_height) * 2.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::place;

    #[test]
    fn the_top_left_corner_is_the_top_left_of_clip_space() {
        let rect = place((0.0, 0.0), 100, 50, 1000, 500);

        assert!((rect[0] - -1.0).abs() < f32::EPSILON, "left edge");
        assert!((rect[1] - 1.0).abs() < f32::EPSILON, "top edge");
    }

    #[test]
    fn a_bitmap_covering_the_target_spans_all_of_clip_space() {
        let rect = place((0.0, 0.0), 800, 600, 800, 600);

        assert!((rect[2] - 2.0).abs() < f32::EPSILON, "full width");
        assert!((rect[3] - 2.0).abs() < f32::EPSILON, "full height");
    }

    #[test]
    fn the_middle_of_the_target_is_the_middle_of_clip_space() {
        let rect = place((0.5, 0.5), 10, 10, 100, 100);

        assert!(rect[0].abs() < f32::EPSILON, "horizontal centre");
        assert!(rect[1].abs() < f32::EPSILON, "vertical centre");
    }

    #[test]
    fn a_bitmap_keeps_its_pixel_size_as_the_target_grows() {
        let small = place((0.0, 0.0), 40, 40, 640, 360);
        let large = place((0.0, 0.0), 40, 40, 1280, 720);

        // Half the fraction of a target twice the size, which is the same forty pixels.
        assert!((small[2] - large[2] * 2.0).abs() < 1e-6, "width in pixels");
        assert!((small[3] - large[3] * 2.0).abs() < 1e-6, "height in pixels");
    }

    #[test]
    fn a_target_with_no_area_does_not_divide_by_zero() {
        let rect = place((0.0, 0.0), 40, 40, 0, 0);

        assert!(rect.iter().all(|value| value.is_finite()));
    }
}
