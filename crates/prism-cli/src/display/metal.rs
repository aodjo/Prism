//! The macOS half of the window: a Metal layer on an SDL window.
//!
//! SDL makes the window and attaches a `CAMetalLayer` to it; everything below that is the
//! renderer's. What this module adds is the two overlays and the order they are drawn in, so
//! the loop in the parent module never has to know which platform it is running on.

use std::error::Error;

use objc2_metal::MTLPixelFormat;
use objc2_quartz_core::CAMetalLayer;
use prism_core::render::RenderError;
use prism_core::render::cursor::CursorOverlay;
use prism_core::render::metal::MetalRenderer;
use prism_core::render::overlay::TextOverlay;
use sdl3::video::Window;
use sdl3_sys::metal::{
    SDL_Metal_CreateView, SDL_Metal_DestroyView, SDL_Metal_GetLayer, SDL_MetalView,
};

use crate::display::{HUD_FONT_SIZE, HUD_HEIGHT, HUD_WIDTH};

/// A decoded picture on this platform.
pub type Picture = prism_core::decode::videotoolbox::DecodedFrame;

/// Returns the timestamp a picture was decoded from.
pub fn pts_of(picture: &Picture) -> u64 {
    picture.pts_us
}

/// The window's drawing surface and everything drawn onto it.
pub struct Surface {
    view: SDL_MetalView,
    renderer: MetalRenderer,
    overlay: TextOverlay,
    cursor: CursorOverlay,
}

impl Surface {
    /// Attaches a Metal layer to the window and builds the renderer for it.
    ///
    /// # Errors
    ///
    /// Returns an error if SDL will not make a Metal view, or if Metal will not build the
    /// renderer, the statistics panel or the cursor.
    pub fn new(window: &Window, width: u32, height: u32) -> Result<Self, Box<dyn Error>> {
        // SAFETY: the window outlives this surface, which is dropped before it.
        let view = unsafe { SDL_Metal_CreateView(window.raw()) };
        if view.is_null() {
            return Err("could not create a Metal view for the window".into());
        }

        let renderer = MetalRenderer::new(MTLPixelFormat::BGRA8Unorm)?;
        let overlay = TextOverlay::new(renderer.device(), HUD_WIDTH, HUD_HEIGHT, HUD_FONT_SIZE)?;
        let cursor = CursorOverlay::new(renderer.device())?;

        let surface = Self {
            view,
            renderer,
            overlay,
            cursor,
        };
        surface
            .renderer
            .configure_layer(surface.layer(), width as usize, height as usize);

        Ok(surface)
    }

    /// Describes how this surface reaches the screen.
    pub fn describe(&self) -> String {
        "Metal, display sync off".to_owned()
    }

    /// Follows the window to a new size in pixels.
    ///
    /// # Errors
    ///
    /// Never fails on this platform; the signature matches the other backend, where resizing
    /// a swap chain can be refused.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        self.renderer
            .configure_layer(self.layer(), width as usize, height as usize);

        Ok(())
    }

    /// Redraws the statistics panel.
    pub fn update_hud(&mut self, lines: &[String]) {
        self.overlay.update(lines);
    }

    /// Draws one picture with the overlays on top and presents it.
    ///
    /// Returns `false` when the layer had no drawable available, which happens when the
    /// display is behind; the frame is dropped rather than queued because a frame shown late
    /// is worse than one not shown at all.
    ///
    /// # Errors
    ///
    /// Returns whatever the renderer reports about binding the picture or recording the draw.
    pub fn present(
        &mut self,
        picture: &Picture,
        cursor_at: Option<(f32, f32)>,
        target: (usize, usize),
    ) -> Result<bool, RenderError> {
        // A fixed array rather than a vector: this runs once per displayed frame, and the
        // frame path does not allocate.
        //
        // The cursor comes after the statistics so it draws on top of them. It is the thing
        // being pointed with, and it should never vanish behind a panel.
        let mut quads = [self.overlay.quad(target.0, target.1); 2];
        let count = match cursor_at {
            Some(at) => {
                quads[1] = self.cursor.quad(at, target.0, target.1);
                2
            }
            None => 1,
        };

        // Taken from the view rather than through `&self`, so the renderer is free to be
        // borrowed mutably for the draw that follows.
        let layer = layer_of(self.view);
        self.renderer
            .present(picture.pixel_buffer(), layer, &quads[..count])
    }

    /// Returns the layer SDL attached to the window.
    fn layer(&self) -> &CAMetalLayer {
        layer_of(self.view)
    }
}

/// Returns the `CAMetalLayer` behind a Metal view.
///
/// # Safety note
///
/// The layer belongs to the view and lives exactly as long as it does, which is as long as
/// the surface holding it.
fn layer_of<'a>(view: SDL_MetalView) -> &'a CAMetalLayer {
    // SAFETY: SDL returns the view's own layer, and the caller holds the view for at least as
    // long as it uses the result.
    unsafe { &*(SDL_Metal_GetLayer(view).cast::<CAMetalLayer>()) }
}

impl Drop for Surface {
    /// Releases the Metal view, which SDL does not reference count.
    fn drop(&mut self) {
        // SAFETY: the view was created in the constructor, nothing else refers to it, and the
        // window it belongs to outlives this surface.
        unsafe { SDL_Metal_DestroyView(self.view) };
    }
}

impl core::fmt::Debug for Surface {
    /// Describes the surface without reaching into platform objects.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Surface").finish_non_exhaustive()
    }
}
