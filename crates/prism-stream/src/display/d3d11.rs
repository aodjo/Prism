//! The Windows half of the window: a flip-model swap chain on an SDL window.
//!
//! SDL makes the window and this takes its `HWND`; everything below that is the renderer's.
//! The device is created here rather than by the decoder, because the decoder is the one that
//! has to be told: a picture decoded onto a different device than the window is drawn on would
//! have to be copied across the two, which is the copy the whole path exists to avoid.

use std::error::Error;

use prism_core::control::client::Gpu;
use prism_core::render::RenderError;
use prism_core::render::d3d11::D3d11Renderer;
use prism_core::render::hud::{CursorOverlay, TextOverlay};
use sdl3::video::Window;
use sdl3_sys::properties::SDL_GetPointerProperty;
use sdl3_sys::video::{SDL_GetWindowProperties, SDL_PROP_WINDOW_WIN32_HWND_POINTER};
use windows::Win32::Foundation::HWND;

use crate::display::{HUD_FONT_SIZE, HUD_HEIGHT, HUD_WIDTH};

/// A decoded picture on this platform.
pub type Picture = prism_core::decode::mediafoundation::DecodedFrame;

/// Returns the timestamp a picture was decoded from.
pub fn pts_of(picture: &Picture) -> u64 {
    picture.pts_us
}

/// Returns how large a picture is, in pixels.
pub fn size_of(picture: &Picture) -> (u32, u32) {
    (picture.width, picture.height)
}

/// The window's drawing surface and everything drawn onto it.
pub struct Surface {
    renderer: D3d11Renderer,
    overlay: TextOverlay,
    cursor: CursorOverlay,
    gpu: Gpu,
}

impl Surface {
    /// Builds a swap chain for the window and the renderer that draws into it.
    ///
    /// # Errors
    ///
    /// Returns an error if SDL will not give up the window handle, if no Direct3D device with
    /// video support can be created, or if the renderer, the statistics panel or the cursor
    /// cannot be built.
    pub fn new(window: &Window, width: u32, height: u32) -> Result<Self, Box<dyn Error>> {
        let hwnd = window_handle(window)?;
        let (device, context) = prism_core::decode::mediafoundation::create_device()?;

        // SAFETY: the handle came from the window this surface belongs to, which outlives it.
        let renderer = unsafe { D3d11Renderer::new(&device, &context, hwnd, width, height)? };

        let overlay = TextOverlay::new(&renderer, HUD_WIDTH, HUD_HEIGHT, HUD_FONT_SIZE)?;
        let cursor = CursorOverlay::new(&renderer)?;

        Ok(Self {
            renderer,
            overlay,
            cursor,
            gpu: (device, context),
        })
    }

    /// Returns the device the decoder should decode onto.
    pub fn gpu(&self) -> Option<Gpu> {
        Some(self.gpu.clone())
    }

    /// Describes how this surface reaches the screen.
    pub fn describe(&self) -> String {
        if self.renderer.tearing() {
            "Direct3D 11, flip model, tearing allowed".to_owned()
        } else {
            "Direct3D 11, flip model, waiting for the vertical blank".to_owned()
        }
    }

    /// Follows the window to a new size in pixels.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if DXGI will not resize the swap chain.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        self.renderer.resize(width, height)
    }

    /// Redraws the statistics panel.
    pub fn update_hud(&mut self, lines: &[String]) {
        self.overlay.update(lines);
    }

    /// Draws one picture with the overlays on top and presents it.
    ///
    /// Returns `false` when the display is still busy with the frame before; the frame is
    /// dropped rather than queued because a frame shown late is worse than one not shown at
    /// all.
    ///
    /// # Errors
    ///
    /// Returns whatever the renderer reports about binding the picture or presenting it.
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
        let mut quads = [
            self.overlay.quad(target.0, target.1),
            self.overlay.quad(target.0, target.1),
        ];
        let count = match cursor_at {
            Some(at) => {
                quads[1] = self.cursor.quad(at, target.0, target.1);
                2
            }
            None => 1,
        };

        let (texture, index) = picture.texture();
        self.renderer.present(texture, index, &quads[..count])
    }
}

impl core::fmt::Debug for Surface {
    /// Describes the surface without reaching into platform objects.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Surface")
            .field("renderer", &self.renderer)
            .finish_non_exhaustive()
    }
}

/// Reads the Win32 window handle out of an SDL window.
///
/// # Errors
///
/// Returns an error if SDL does not report one, which on this platform means the window was
/// not made by the Win32 backend and there is nothing to draw into.
fn window_handle(window: &Window) -> Result<HWND, Box<dyn Error>> {
    // SAFETY: the window is alive for the duration of the call, and the property is read as
    // the pointer type SDL documents it to hold.
    let handle = unsafe {
        let properties = SDL_GetWindowProperties(window.raw());
        SDL_GetPointerProperty(
            properties,
            SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            core::ptr::null_mut(),
        )
    };

    if handle.is_null() {
        return Err("the window has no Win32 handle to draw into".into());
    }

    Ok(HWND(handle))
}
