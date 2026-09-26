//! Putting a decoded picture on screen on Linux, through SDL's renderer.
//!
//! The other two surfaces take a picture that is already on the GPU and draw it there. This one
//! is handed a picture in system memory — the software decoder is the one every Linux machine
//! has — so its first step is an upload: the three planes into one YUV texture, which the
//! renderer converts to RGB as it draws. Whatever SDL chose to draw with does that conversion,
//! Vulkan or OpenGL or its own software rasteriser, and the texture is told it is BT.709 video
//! range so the conversion is the one the rest of this project assumes.
//!
//! The cursor is the same arrow the other clients draw, and the statistics panel is SDL's own
//! eight-pixel debug font: Linux has no one text API to reach for the way macOS and Windows
//! do, and a panel of numbers read by whoever is measuring a session does not need one.

use std::error::Error;
use std::ffi::{CStr, CString};

use prism_core::render::{RenderError, arrow, fit};
use sdl3::video::Window;
use sdl3_sys::blendmode::{SDL_BLENDMODE_BLEND, SDL_BLENDMODE_BLEND_PREMULTIPLIED};
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::pixels::{
    SDL_COLORSPACE_BT709_LIMITED, SDL_PIXELFORMAT_ABGR8888, SDL_PIXELFORMAT_IYUV,
};
use sdl3_sys::properties::{SDL_CreateProperties, SDL_DestroyProperties, SDL_SetNumberProperty};
use sdl3_sys::rect::SDL_FRect;
use sdl3_sys::render::{
    SDL_CreateRenderer, SDL_CreateTextureWithProperties, SDL_DEBUG_TEXT_FONT_CHARACTER_SIZE,
    SDL_DestroyRenderer, SDL_DestroyTexture, SDL_GetRendererName,
    SDL_PROP_TEXTURE_CREATE_ACCESS_NUMBER, SDL_PROP_TEXTURE_CREATE_COLORSPACE_NUMBER,
    SDL_PROP_TEXTURE_CREATE_FORMAT_NUMBER, SDL_PROP_TEXTURE_CREATE_HEIGHT_NUMBER,
    SDL_PROP_TEXTURE_CREATE_WIDTH_NUMBER, SDL_RenderClear, SDL_RenderDebugText, SDL_RenderFillRect,
    SDL_RenderPresent, SDL_RenderTexture, SDL_Renderer, SDL_SetRenderDrawBlendMode,
    SDL_SetRenderDrawColor, SDL_SetRenderScale, SDL_SetTextureBlendMode, SDL_TEXTUREACCESS_STATIC,
    SDL_TEXTUREACCESS_STREAMING, SDL_Texture, SDL_UpdateTexture, SDL_UpdateYUVTexture,
};

use crate::display::hud_measure;

/// What the decoder hands this surface.
pub type Picture = prism_core::decode::openh264::Picture;

/// How dark the statistics panel's backdrop is, out of 255.
const BACKDROP_ALPHA: u8 = 190;

/// Space between the panel's edge and its text, in the panel's own units.
const TEXT_INSET: f32 = 6.0;

/// Returns the presentation timestamp a picture was decoded with.
pub fn pts_of(picture: &Picture) -> u64 {
    picture.pts_us
}

/// Returns the picture's size in pixels.
pub fn size_of(picture: &Picture) -> (u32, u32) {
    (picture.width(), picture.height())
}

/// A texture the renderer owns, destroyed with it.
struct Texture {
    raw: *mut SDL_Texture,
    width: u32,
    height: u32,
}

impl Drop for Texture {
    /// Gives the texture back to the renderer that made it.
    fn drop(&mut self) {
        // SAFETY: the texture was created by a renderer that outlives it — `Surface` drops its
        // textures before its renderer — and nothing else destroys it.
        unsafe { SDL_DestroyTexture(self.raw) };
    }
}

/// The window's renderer and what it draws with.
pub struct Surface {
    renderer: *mut SDL_Renderer,
    /// The picture's texture, made for the size of the first picture and again whenever the
    /// size changes.
    picture: Option<Texture>,
    cursor: Texture,
    lines: Vec<String>,
    stats: bool,
    /// The panel's size in pixels and its text size, for this window's pixel density.
    panel: (usize, usize, f64),
}

impl Surface {
    /// Makes a renderer for the window.
    ///
    /// # Errors
    ///
    /// Fails if SDL has nothing to render this window with, or cannot make the cursor texture.
    pub fn new(
        window: &Window,
        _width: u32,
        _height: u32,
        scale: f64,
    ) -> Result<Self, Box<dyn Error>> {
        // SAFETY: the window is live for the duration of the call, and a null name asks SDL to
        // choose the best renderer it has. The renderer is destroyed in `Drop`, which runs before
        // the window's — the display loop declares the surface after the window.
        let renderer = unsafe { SDL_CreateRenderer(window.raw(), core::ptr::null()) };

        if renderer.is_null() {
            return Err(format!("SDL has no renderer for this window: {}", last_error()).into());
        }

        let cursor = match make_cursor(renderer) {
            Ok(cursor) => cursor,
            Err(err) => {
                // SAFETY: made immediately above and not yet handed to anything else.
                unsafe { SDL_DestroyRenderer(renderer) };

                return Err(err);
            }
        };

        Ok(Self {
            renderer,
            picture: None,
            cursor,
            lines: Vec::new(),
            stats: false,
            panel: hud_measure(scale),
        })
    }

    /// Says what is drawing, for the log.
    pub fn describe(&self) -> String {
        // SAFETY: the renderer is live; SDL owns the returned string and keeps it for the
        // renderer's lifetime.
        let name = unsafe { SDL_GetRendererName(self.renderer) };

        if name.is_null() {
            return "SDL renderer".to_owned();
        }

        // SAFETY: a non-null name from SDL is a terminated string.
        let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();

        format!("SDL renderer ({name}), YUV uploaded from the software decoder")
    }

    /// Follows the window to a new size, which SDL's renderer does by itself.
    ///
    /// # Errors
    ///
    /// Never; the signature is the one every surface shares.
    pub fn resize(&mut self, _width: u32, _height: u32) -> Result<(), RenderError> {
        Ok(())
    }

    /// Replaces what the statistics panel says.
    pub fn update_hud(&mut self, lines: &[String]) {
        self.lines.clear();
        self.lines.extend_from_slice(lines);
    }

    /// Shows or hides the statistics panel.
    pub fn show_stats(&mut self, on: bool) {
        self.stats = on;
    }

    /// Draws a picture, the far cursor over it, and the panel if it is showing.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if the picture's texture cannot be made or filled.
    pub fn present(
        &mut self,
        picture: &Picture,
        cursor_at: Option<(f32, f32)>,
        target: (usize, usize),
    ) -> Result<bool, RenderError> {
        let texture = self.picture_texture(picture.width(), picture.height())?;
        let planes = picture.planes();
        let across = i32::try_from(planes.width()).unwrap_or(i32::MAX);

        // SAFETY: the texture is live and was made for exactly this size and format, and each
        // plane holds `height` (or half of it) rows of the pitch given, which is how `I420`
        // lays them out.
        let filled = unsafe {
            SDL_UpdateYUVTexture(
                texture,
                core::ptr::null(),
                planes.y().as_ptr(),
                across,
                planes.u().as_ptr(),
                across / 2,
                planes.v().as_ptr(),
                across / 2,
            )
        };

        if !filled {
            return Err(RenderError::Setup {
                reason: "SDL would not take the picture into its texture",
            });
        }

        let whole = (target.0 as f32, target.1 as f32);
        let fitted = fit((picture.width(), picture.height()), whole);
        let placed = SDL_FRect {
            x: fitted.left,
            y: fitted.top,
            w: fitted.width,
            h: fitted.height,
        };

        // SAFETY: the renderer and the texture are live, and every rectangle is a local that
        // outlives the call it is passed to.
        unsafe {
            SDL_SetRenderDrawColor(self.renderer, 0, 0, 0, 255);
            SDL_RenderClear(self.renderer);
            SDL_RenderTexture(self.renderer, texture, core::ptr::null(), &raw const placed);
        }

        if let Some(at) = cursor_at {
            let (x, y) = fitted.to_target(at, whole);
            let arrow = SDL_FRect {
                x: x * whole.0,
                y: y * whole.1,
                w: arrow::SIZE as f32,
                h: arrow::SIZE as f32,
            };

            // SAFETY: as above.
            unsafe {
                SDL_RenderTexture(
                    self.renderer,
                    self.cursor.raw,
                    core::ptr::null(),
                    &raw const arrow,
                );
            }
        }

        if self.stats {
            self.draw_panel();
        }

        // SAFETY: the renderer is live.
        Ok(unsafe { SDL_RenderPresent(self.renderer) })
    }

    /// The texture for pictures of this size, made if there is none or the size has changed.
    fn picture_texture(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<*mut SDL_Texture, RenderError> {
        if let Some(texture) = self
            .picture
            .as_ref()
            .filter(|one| (one.width, one.height) == (width, height))
        {
            return Ok(texture.raw);
        }

        self.picture = None;

        let texture = make_texture(
            self.renderer,
            SDL_PIXELFORMAT_IYUV.0 as i64,
            SDL_TEXTUREACCESS_STREAMING.0 as i64,
            (width, height),
            Some(SDL_COLORSPACE_BT709_LIMITED.0 as i64),
        )
        .ok_or(RenderError::Setup {
            reason: "SDL would not make a YUV texture for the picture",
        })?;

        let raw = texture.raw;
        self.picture = Some(texture);

        Ok(raw)
    }

    /// Draws the statistics panel in the top left corner.
    fn draw_panel(&self) {
        let (width, height, font) = self.panel;
        let zoom = (font / f64::from(SDL_DEBUG_TEXT_FONT_CHARACTER_SIZE)).max(1.0) as f32;
        let backdrop = SDL_FRect {
            x: 0.0,
            y: 0.0,
            w: width as f32 / zoom,
            h: height as f32 / zoom,
        };
        let line_height = SDL_DEBUG_TEXT_FONT_CHARACTER_SIZE as f32 + 4.0;

        // SAFETY: the renderer is live, and the scale is put back before this returns so nothing
        // drawn afterwards is magnified with it.
        unsafe {
            SDL_SetRenderScale(self.renderer, zoom, zoom);
            SDL_SetRenderDrawBlendMode(self.renderer, SDL_BLENDMODE_BLEND);
            SDL_SetRenderDrawColor(self.renderer, 0, 0, 0, BACKDROP_ALPHA);
            SDL_RenderFillRect(self.renderer, &raw const backdrop);
            SDL_SetRenderDrawColor(self.renderer, 255, 255, 255, 255);
        }

        for (index, line) in self.lines.iter().enumerate() {
            let top = TEXT_INSET + index as f32 * line_height;

            if top + line_height > backdrop.h {
                break;
            }

            let Ok(text) = CString::new(line.as_str()) else {
                continue;
            };

            // SAFETY: the renderer is live and the text is a terminated string that outlives
            // the call.
            unsafe { SDL_RenderDebugText(self.renderer, TEXT_INSET, top, text.as_ptr()) };
        }

        // SAFETY: the renderer is live.
        unsafe { SDL_SetRenderScale(self.renderer, 1.0, 1.0) };
    }
}

impl Drop for Surface {
    /// Destroys the textures, then the renderer they belong to.
    fn drop(&mut self) {
        self.picture = None;

        // SAFETY: the cursor texture is destroyed first, by replacing it with nothing that needs
        // destroying, and the renderer — made in `new`, destroyed only here — goes after both.
        unsafe {
            SDL_DestroyTexture(self.cursor.raw);
            self.cursor.raw = core::ptr::null_mut();
            SDL_DestroyRenderer(self.renderer);
        }
    }
}

impl core::fmt::Debug for Surface {
    /// Describes the surface without reaching into SDL.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Surface")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// Makes the texture the far cursor is drawn from, holding the shared arrow.
fn make_cursor(renderer: *mut SDL_Renderer) -> Result<Texture, Box<dyn Error>> {
    let side = arrow::SIZE as u32;
    let cursor = make_texture(
        renderer,
        SDL_PIXELFORMAT_ABGR8888.0 as i64,
        SDL_TEXTUREACCESS_STATIC.0 as i64,
        (side, side),
        None,
    )
    .ok_or_else(|| format!("SDL would not make the cursor texture: {}", last_error()))?;

    let pixels = arrow::bitmap();
    let pitch = (arrow::SIZE * arrow::BYTES_PER_PIXEL) as i32;

    // SAFETY: the texture is live and was made for exactly this size, and the bitmap holds
    // `SIZE` rows of `pitch` bytes — red, green, blue, alpha, which is `ABGR8888` read as a
    // little-endian word. The arrow's colour is premultiplied, and the blend mode says so.
    unsafe {
        SDL_UpdateTexture(cursor.raw, core::ptr::null(), pixels.as_ptr().cast(), pitch);
        SDL_SetTextureBlendMode(cursor.raw, SDL_BLENDMODE_BLEND_PREMULTIPLIED);
    }

    Ok(cursor)
}

/// Makes a texture of a format, an access and a size, in a colour space if one is given.
fn make_texture(
    renderer: *mut SDL_Renderer,
    format: i64,
    access: i64,
    (width, height): (u32, u32),
    colorspace: Option<i64>,
) -> Option<Texture> {
    // SAFETY: a new property set, filled and then destroyed below once the texture that read it
    // has been made; every name is one of SDL's own constants and every value a number.
    let raw = unsafe {
        let properties = SDL_CreateProperties();

        SDL_SetNumberProperty(properties, SDL_PROP_TEXTURE_CREATE_FORMAT_NUMBER, format);
        SDL_SetNumberProperty(properties, SDL_PROP_TEXTURE_CREATE_ACCESS_NUMBER, access);
        SDL_SetNumberProperty(
            properties,
            SDL_PROP_TEXTURE_CREATE_WIDTH_NUMBER,
            i64::from(width),
        );
        SDL_SetNumberProperty(
            properties,
            SDL_PROP_TEXTURE_CREATE_HEIGHT_NUMBER,
            i64::from(height),
        );

        if let Some(space) = colorspace {
            SDL_SetNumberProperty(properties, SDL_PROP_TEXTURE_CREATE_COLORSPACE_NUMBER, space);
        }

        let texture = SDL_CreateTextureWithProperties(renderer, properties);
        SDL_DestroyProperties(properties);

        texture
    };

    (!raw.is_null()).then_some(Texture { raw, width, height })
}

/// What SDL last said went wrong.
fn last_error() -> String {
    // SAFETY: SDL always returns a terminated string, possibly empty, owned by itself.
    unsafe { CStr::from_ptr(SDL_GetError()) }
        .to_string_lossy()
        .into_owned()
}
