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

/// Where a picture sits inside a target of another shape.
///
/// As large as it fits whole, and centred. The rest of the target is left as it was cleared,
/// which is black: a far screen of one shape stretched into a window of another draws every
/// circle on it as an ellipse and puts every click a little off where it was aimed.
///
/// In the target's own units, whatever they are — pixels for the renderer, points for the
/// window the pointer moves in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fitted {
    /// From the left of the target to the left of the picture.
    pub left: f32,
    /// From the top of the target to the top of the picture.
    pub top: f32,
    /// How wide the picture is drawn.
    pub width: f32,
    /// How tall the picture is drawn.
    pub height: f32,
}

impl Fitted {
    /// The whole of a target, for when there is no picture yet to fit.
    #[must_use]
    pub fn whole(target: (f32, f32)) -> Self {
        Self {
            left: 0.0,
            top: 0.0,
            width: target.0,
            height: target.1,
        }
    }

    /// Returns where a fraction of the picture lands, as a fraction of the target.
    ///
    /// For something drawn over the picture in the target's coordinates, such as the far
    /// pointer, which is known as a place on the far screen.
    #[must_use]
    pub fn to_target(&self, at: (f32, f32), target: (f32, f32)) -> (f32, f32) {
        (
            at.0.mul_add(self.width, self.left) / target.0.max(1.0),
            at.1.mul_add(self.height, self.top) / target.1.max(1.0),
        )
    }

    /// Returns where a place in the target is, as a fraction of the picture.
    ///
    /// Clamped to the picture's edges, so a place on the bars around it is the nearest place
    /// on the picture: the pointer held against the edge of the far screen rather than off it.
    #[must_use]
    pub fn to_picture(&self, x: f32, y: f32) -> (f32, f32) {
        let across = (self.width - 1.0).max(1.0);
        let down = (self.height - 1.0).max(1.0);

        (
            ((x - self.left) / across).clamp(0.0, 1.0),
            ((y - self.top) / down).clamp(0.0, 1.0),
        )
    }
}

/// Fits a picture of `picture` pixels into a target of `target` units, keeping its shape.
///
/// Whole units on every side, so the picture's edges fall on pixels rather than blending half
/// a pixel of it into the bars. A picture with no area, or a target with none, is the whole
/// target: there is nothing to keep the shape of.
#[must_use]
pub fn fit(picture: (u32, u32), target: (f32, f32)) -> Fitted {
    let (across, down) = (picture.0 as f32, picture.1 as f32);

    if across < 1.0 || down < 1.0 || target.0 < 1.0 || target.1 < 1.0 {
        return Fitted::whole(target);
    }

    let scale = (target.0 / across).min(target.1 / down);
    let width = (across * scale).round().min(target.0);
    let height = (down * scale).round().min(target.1);

    Fitted {
        left: ((target.0 - width) / 2.0).floor(),
        top: ((target.1 - height) / 2.0).floor(),
        width,
        height,
    }
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
    use super::{Fitted, fit, place};

    #[test]
    fn a_wide_picture_in_a_tall_window_has_bars_above_and_below() {
        let fitted = fit((1920, 1080), (1000.0, 1000.0));

        assert_eq!(fitted.left, 0.0);
        assert_eq!(fitted.width, 1000.0);
        assert_eq!(fitted.height, 563.0, "1000 × 1080 / 1920, rounded");
        assert_eq!(
            fitted.top, 218.0,
            "the rest shared out, the odd unit at the bottom"
        );
    }

    #[test]
    fn a_tall_picture_in_a_wide_window_has_bars_either_side() {
        let fitted = fit((1080, 1920), (1920.0, 1080.0));

        assert_eq!(fitted.top, 0.0);
        assert_eq!(fitted.height, 1080.0);
        assert_eq!(fitted.width, 608.0);
        assert_eq!(fitted.left, 656.0);
    }

    #[test]
    fn a_picture_the_shape_of_its_window_fills_it() {
        assert_eq!(
            fit((2560, 1440), (1280.0, 720.0)),
            Fitted::whole((1280.0, 720.0))
        );
    }

    #[test]
    fn nothing_to_fit_is_the_whole_target() {
        assert_eq!(fit((0, 0), (800.0, 600.0)), Fitted::whole((800.0, 600.0)));
    }

    #[test]
    fn a_place_on_the_bars_is_the_nearest_place_on_the_picture() {
        let fitted = fit((1920, 1080), (1000.0, 1000.0));

        assert_eq!(fitted.to_picture(500.0, 10.0).1, 0.0, "above the picture");
        assert_eq!(fitted.to_picture(500.0, 990.0).1, 1.0, "below it");

        let middle = fitted.to_picture(500.0, 218.0 + 281.0);
        assert!((middle.0 - 0.5).abs() < 0.01 && (middle.1 - 0.5).abs() < 0.01);
    }

    #[test]
    fn the_far_pointer_lands_on_the_picture_rather_than_across_the_bars() {
        let fitted = fit((1920, 1080), (1000.0, 1000.0));
        let (x, y) = fitted.to_target((0.0, 0.0), (1000.0, 1000.0));

        assert_eq!(x, 0.0);
        assert!(
            (y - 0.218).abs() < 1e-6,
            "the picture's top, not the window's"
        );
    }

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
