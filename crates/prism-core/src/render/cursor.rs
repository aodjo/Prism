//! Drawing the cursor the host is not sending.
//!
//! The host excludes its cursor from the captured video so the client can draw one that
//! answers the mouse immediately rather than a video frame later. That leaves the client
//! needing a cursor bitmap, and until the control channel carries real cursor shapes there
//! is one: a plain arrow, drawn once at startup and blended over every frame.
//!
//! Drawn rather than embedded because a path is a dozen numbers and a PNG is a decoder.

use core::ffi::c_void;
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGColorSpace, CGContext, CGLineJoin, CGPathDrawingMode};
use objc2_metal::{
    MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLSize, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};

use crate::render::RenderError;
use crate::render::metal::{Quad, place};

/// Bitmap flags for premultiplied RGBA, which is what the blend expects.
///
/// `kCGImageAlphaPremultipliedLast` is 1 and `kCGBitmapByteOrder32Big` is 4 << 12.
const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);

/// Bytes per pixel in the cursor bitmap.
const BYTES_PER_PIXEL: usize = 4;

/// Width and height of the cursor bitmap in pixels.
///
/// Sized for the drawable, which is in physical pixels, so this is roughly the size a
/// system cursor occupies on a Retina display.
const SIZE: usize = 40;

/// The arrow outline, in pixels measured right and down from the tip.
///
/// The tip is the hotspot and sits at the origin, so placing the bitmap's top left corner
/// at the pointer position puts the tip exactly where the pointer is.
const ARROW: [(f64, f64); 7] = [
    (0.0, 0.0),
    (0.0, 26.0),
    (6.5, 19.5),
    (10.8, 30.0),
    (15.4, 28.0),
    (11.2, 18.0),
    (19.0, 18.0),
];

/// How thick the outline is, in pixels.
const OUTLINE_WIDTH: f64 = 1.6;

/// An arrow cursor the client draws over the video.
pub struct CursorOverlay {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
}

impl CursorOverlay {
    /// Draws the arrow once and uploads it as a texture.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if CoreGraphics will not allocate a bitmap context or
    /// Metal will not allocate the texture.
    pub fn new(device: &ProtocolObject<dyn MTLDevice>) -> Result<Self, RenderError> {
        let mut pixels = vec![0u8; SIZE * SIZE * BYTES_PER_PIXEL].into_boxed_slice();

        let space = CGColorSpace::new_device_rgb().ok_or(RenderError::Setup {
            reason: "no device RGB colour space",
        })?;

        // SAFETY: the buffer is `SIZE * SIZE * 4` bytes and outlives the context, which is
        // dropped at the end of this function.
        let context = unsafe {
            let raw = bitmap_context(
                pixels.as_mut_ptr().cast::<c_void>(),
                SIZE,
                SIZE,
                8,
                SIZE * BYTES_PER_PIXEL,
                &*space,
                RGBA_PREMULTIPLIED,
            );

            NonNull::new(raw)
                .map(|raw| CFRetained::from_raw(raw))
                .ok_or(RenderError::Setup {
                    reason: "could not create a bitmap context",
                })?
        };

        draw_arrow(&context);

        // SAFETY: the constructor takes plain values and returns a fresh descriptor.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                SIZE,
                SIZE,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(MTLStorageMode::Shared);

        let texture = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or(RenderError::Setup {
                reason: "could not allocate the cursor texture",
            })?;

        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: SIZE,
                height: SIZE,
                depth: 1,
            },
        };

        // SAFETY: the bitmap holds exactly the bytes the region covers, and the texture uses
        // shared storage so the CPU may write it directly.
        unsafe {
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(pixels.as_ptr().cast::<c_void>().cast_mut())
                    .expect("the bitmap is non-null"),
                SIZE * BYTES_PER_PIXEL,
            );
        }

        Ok(Self { texture })
    }

    /// Returns the texture the renderer samples.
    #[must_use]
    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }

    /// Returns the quad that draws the cursor with its tip at `at`.
    ///
    /// `at` is a fraction of the target in both axes, which is what the tracker reports,
    /// because the client's window is rarely the size of the host's screen.
    #[must_use]
    pub fn quad(&self, at: (f32, f32), target_width: usize, target_height: usize) -> Quad<'_> {
        Quad {
            texture: &self.texture,
            rect: place(at, SIZE, SIZE, target_width, target_height),
        }
    }
}

impl core::fmt::Debug for CursorOverlay {
    /// Describes the overlay without dumping its pixels.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CursorOverlay")
            .field("size", &SIZE)
            .finish_non_exhaustive()
    }
}

/// Traces the arrow and paints it, filled and outlined.
///
/// The path is built in a coordinate space measured down from the tip, then flipped: a
/// bitmap context puts user-space y zero at the bottom, while the first row in memory — and
/// so the top of the texture — is the highest y.
fn draw_arrow(context: &CGContext) {
    let flip = |y: f64| SIZE as f64 - y;

    // Colours are set as `CGColor`s, which carry their own colour space, rather than as
    // loose component arrays. A component array is read against whatever space the context
    // currently has, and a fresh bitmap context does not necessarily have the one the
    // caller has in mind — four RGBA components read as grey plus alpha silently produce a
    // fully transparent black, which is a stroke that does not appear at all.
    let fill = CGColor::new_srgb(1.0, 1.0, 1.0, 1.0);
    let outline = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);

    CGContext::clear_rect(
        Some(context),
        CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: SIZE as f64,
                height: SIZE as f64,
            },
        },
    );

    CGContext::set_line_join(Some(context), CGLineJoin::Miter);
    CGContext::set_line_width(Some(context), OUTLINE_WIDTH);
    CGContext::set_fill_color_with_color(Some(context), Some(&fill));
    CGContext::set_stroke_color_with_color(Some(context), Some(&outline));

    CGContext::begin_path(Some(context));
    CGContext::move_to_point(Some(context), ARROW[0].0, flip(ARROW[0].1));
    for point in &ARROW[1..] {
        CGContext::add_line_to_point(Some(context), point.0, flip(point.1));
    }
    CGContext::close_path(Some(context));

    // Filled and stroked in one pass so the outline sits half inside the shape rather than
    // doubling its apparent size.
    CGContext::draw_path(Some(context), CGPathDrawingMode::FillStroke);
}

// SAFETY: `CGBitmapContextCreate` is a stable CoreGraphics entry point. The crate binds
// only the newer block-based variant, so it is declared here directly.
unsafe extern "C-unwind" {
    #[link_name = "CGBitmapContextCreate"]
    fn bitmap_context(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const CGColorSpace,
        bitmap_info: u32,
    ) -> *mut CGContext;
}
