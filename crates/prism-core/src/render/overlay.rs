//! Drawing statistics over the video.
//!
//! Every milestone in this project is judged by a p99 number, and a number you have to
//! stop the session to read is a number you will not read often enough. The overlay puts
//! the same figures on top of the picture while it is running.
//!
//! Text is rasterised on the CPU with CoreText into a small bitmap and uploaded as a
//! texture. That sounds expensive and is not: the bitmap is a few hundred kilobytes,
//! redrawn about ten times a second, while the video path underneath runs at sixty or a
//! hundred and twenty. Rasterising per frame would be waste; rasterising per update is
//! nothing.

use core::ffi::c_void;
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFRetained, CFString, CGPoint, CGRect, CGSize,
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_graphics::{CGColor, CGColorSpace, CGContext};
use objc2_core_text::{CTFont, CTLine, kCTFontAttributeName, kCTForegroundColorAttributeName};
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

/// Bytes per pixel in the overlay bitmap.
const BYTES_PER_PIXEL: usize = 4;

/// Red, green, blue and alpha of the panel drawn behind the text.
///
/// Dark and mostly opaque, because the picture underneath is arbitrary and the numbers
/// have to stay readable over all of it.
const BACKDROP: [f64; 4] = [0.0, 0.0, 0.0, 0.55];

/// How far the overlay sits from the top left of the target, in pixels.
const MARGIN: usize = 16;

// SAFETY: `CGBitmapContextCreate` is a stable CoreGraphics entry point. The crate binds
// only the newer block-based variant, so it is declared here directly.
unsafe extern "C-unwind" {
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const CGColorSpace,
        bitmap_info: u32,
    ) -> *mut CGContext;
}

/// A block of text drawn over the video.
pub struct TextOverlay {
    width: usize,
    height: usize,
    pixels: Box<[u8]>,
    context: CFRetained<CGContext>,
    /// The font and colour are not held separately: the attribute dictionary is built
    /// with the CoreFoundation type callbacks, so it retains both for as long as it lives.
    attributes: CFRetained<CFDictionary>,
    backdrop: CFRetained<CGColor>,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    line_height: f64,
}

impl TextOverlay {
    /// Creates an overlay of the given size in pixels.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if CoreGraphics will not allocate a bitmap context
    /// or Metal will not allocate the texture.
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        width: usize,
        height: usize,
        font_size: f64,
    ) -> Result<Self, RenderError> {
        let mut pixels = vec![0u8; width * height * BYTES_PER_PIXEL].into_boxed_slice();

        let space = CGColorSpace::new_device_rgb().ok_or(RenderError::Setup {
            reason: "no device RGB colour space",
        })?;

        // SAFETY: the buffer is `width * height * 4` bytes and outlives the context, which
        // this struct owns alongside it.
        let context = unsafe {
            let raw = CGBitmapContextCreate(
                pixels.as_mut_ptr().cast::<c_void>(),
                width,
                height,
                8,
                width * BYTES_PER_PIXEL,
                &*space,
                RGBA_PREMULTIPLIED,
            );

            NonNull::new(raw)
                .map(|raw| CFRetained::from_raw(raw))
                .ok_or(RenderError::Setup {
                    reason: "could not create a bitmap context",
                })?
        };

        let name = CFString::from_str("Menlo");
        // SAFETY: the font name is a valid CFString and no transform is applied.
        let font = unsafe { CTFont::with_name(&name, font_size, core::ptr::null()) };

        let colour = CGColor::new_srgb(1.0, 1.0, 1.0, 1.0);
        let attributes = text_attributes(&font, &colour)?;

        // SAFETY: the constructor takes plain values and returns a fresh descriptor.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                width,
                height,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(MTLStorageMode::Shared);

        let texture = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or(RenderError::Setup {
                reason: "could not allocate the overlay texture",
            })?;

        Ok(Self {
            width,
            height,
            pixels,
            context,
            attributes,
            backdrop: CGColor::new_srgb(BACKDROP[0], BACKDROP[1], BACKDROP[2], BACKDROP[3]),
            texture,
            line_height: font_size * 1.35,
        })
    }

    /// Returns the texture the renderer samples.
    #[must_use]
    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }

    /// Returns the quad that draws the overlay pinned to the top left of the target.
    ///
    /// Placed at its natural pixel size rather than as a fraction, so the text stays
    /// legible whatever the window is scaled to instead of stretching with it.
    #[must_use]
    pub fn quad(&self, target_width: usize, target_height: usize) -> Quad<'_> {
        let at = (
            MARGIN as f32 / target_width.max(1) as f32,
            MARGIN as f32 / target_height.max(1) as f32,
        );

        Quad {
            texture: &self.texture,
            rect: place(at, self.width, self.height, target_width, target_height),
        }
    }

    /// Returns the overlay's width in pixels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Returns the overlay's height in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    /// Redraws the overlay with the given lines and uploads it.
    ///
    /// Lines are drawn from the top. Anything that does not fit is dropped rather than
    /// wrapped: the overlay is a fixed corner of the screen and a stat that grows into the
    /// picture is worse than one that is missing.
    pub fn update(&mut self, lines: &[String]) {
        let full = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: self.width as f64,
                height: self.height as f64,
            },
        };

        // The backdrop is not decoration: white text over a bright, moving picture is
        // unreadable exactly when the numbers matter most.
        //
        // The colour is a `CGColor`, which carries its own colour space, rather than a
        // loose component array. An array is read against whatever space the context
        // currently has, and a fresh bitmap context does not necessarily have the one the
        // caller has in mind — these four components read as grey plus alpha give a fully
        // transparent black, which is a backdrop that never appears.
        CGContext::clear_rect(Some(&self.context), full);
        CGContext::set_fill_color_with_color(Some(&self.context), Some(&self.backdrop));
        CGContext::fill_rect(Some(&self.context), full);

        for (index, line) in lines.iter().enumerate() {
            let baseline = self.height as f64 - self.line_height * (index as f64 + 1.0);
            if baseline < 0.0 {
                break;
            }
            self.draw_line(line, 6.0, baseline + self.line_height * 0.25);
        }

        self.upload();
    }

    /// Draws one line of text at a baseline position in the bitmap.
    fn draw_line(&self, text: &str, x: f64, y: f64) {
        let string = CFString::from_str(text);

        // SAFETY: the string and attributes are alive for the duration of the call, and
        // CoreText copies what it needs from both.
        let line = unsafe {
            let Some(attributed) =
                CFAttributedString::new(None, Some(&string), Some(&self.attributes))
            else {
                return;
            };
            CTLine::with_attributed_string(&attributed)
        };

        // SAFETY: the context is alive and the line was built for it.
        unsafe {
            CGContext::set_text_position(Some(&self.context), x, y);
            line.draw(&self.context);
        }
    }

    /// Copies the bitmap into the Metal texture.
    fn upload(&self) {
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: self.width,
                height: self.height,
                depth: 1,
            },
        };

        // SAFETY: the bitmap holds exactly the bytes the region covers, and the texture
        // uses shared storage so the CPU may write it directly.
        unsafe {
            self.texture
                .replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    region,
                    0,
                    NonNull::new(self.pixels.as_ptr().cast::<c_void>().cast_mut())
                        .expect("the bitmap is non-null"),
                    self.width * BYTES_PER_PIXEL,
                );
        }
    }
}

impl core::fmt::Debug for TextOverlay {
    /// Describes the overlay without dumping its pixels.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TextOverlay")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("line_height", &self.line_height)
            .finish_non_exhaustive()
    }
}

/// Builds the attribute dictionary CoreText draws with.
///
/// # Errors
///
/// Returns [`RenderError::Setup`] if the dictionary cannot be created.
fn text_attributes(
    font: &CTFont,
    colour: &CGColor,
) -> Result<CFRetained<CFDictionary>, RenderError> {
    // SAFETY: both keys are CoreText constants with static lifetime, and the dictionary
    // retains the font and colour through the CoreFoundation callbacks.
    unsafe {
        let mut keys = [
            core::ptr::from_ref(kCTFontAttributeName).cast::<c_void>(),
            core::ptr::from_ref(kCTForegroundColorAttributeName).cast::<c_void>(),
        ];
        let mut values = [
            core::ptr::from_ref(font).cast::<c_void>(),
            core::ptr::from_ref(colour).cast::<c_void>(),
        ];

        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            2,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
        .ok_or(RenderError::Setup {
            reason: "could not build the text attributes",
        })
    }
}
