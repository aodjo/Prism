//! Uploading the client's own cursor as a Metal texture.
//!
//! The shape itself is drawn in [`crate::render::arrow`], which is arithmetic rather than
//! CoreGraphics so that the Windows client shows the same cursor and so that the shape can
//! be tested without a GPU. All that is left here is handing those pixels to Metal.

use core::ffi::c_void;
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLSize, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};

use crate::render::arrow::{BYTES_PER_PIXEL, SIZE, bitmap};
use crate::render::metal::Quad;
use crate::render::{RenderError, place};

/// An arrow cursor the client draws over the video.
pub struct CursorOverlay {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
}

impl CursorOverlay {
    /// Draws the arrow once and uploads it as a texture.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if Metal will not allocate the texture.
    pub fn new(device: &ProtocolObject<dyn MTLDevice>) -> Result<Self, RenderError> {
        let pixels = bitmap();

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
