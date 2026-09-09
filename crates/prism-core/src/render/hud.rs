//! The two things the Windows client draws over the video.
//!
//! Both are the same operation seen twice: an RGBA bitmap with premultiplied alpha, uploaded
//! as a texture and blended as a rectangle. The cursor's bitmap comes from
//! [`crate::render::arrow`], which is arithmetic and shared with the Mac; the statistics
//! panel's is rasterised here with GDI, which is this platform's answer to the CoreText the
//! Mac uses.
//!
//! # Why the text is drawn on the CPU
//!
//! It sounds expensive and is not: the bitmap is a few hundred kilobytes, redrawn about ten
//! times a second, while the video path underneath runs at sixty or a hundred and twenty.
//! Rasterising per frame would be waste; rasterising per update is nothing.
//!
//! # Why the alpha is reconstructed afterwards
//!
//! GDI has no concept of an alpha channel. It writes colour into the three low bytes of each
//! pixel and leaves the fourth exactly as it found it, so text drawn into a cleared bitmap
//! comes out fully transparent. Drawing white on black and then reading the coverage back out
//! of the brightness is what turns it into the premultiplied bitmap the blend expects — and it
//! is why the font is asked for grey antialiasing rather than ClearType, whose coloured
//! subpixel fringes would read back as three different coverages for one pixel.

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11ShaderResourceView};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, FF_MODERN, FIXED_PITCH, FW_NORMAL, HBITMAP, HDC, HFONT, OUT_TT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};
use windows::core::PCWSTR;

use crate::render::arrow::{self, SIZE};
use crate::render::d3d11::{D3d11Renderer, Quad};
use crate::render::{RenderError, place};

/// Bytes per pixel in either bitmap.
const BYTES_PER_PIXEL: usize = 4;

/// How opaque the panel behind the text is, out of 255.
///
/// Dark and mostly opaque, because the picture underneath is arbitrary and the numbers have to
/// stay readable over all of it.
const BACKDROP_ALPHA: u32 = 140;

/// How far the panel sits from the top left of the target, in pixels.
const MARGIN: usize = 16;

/// Where the text starts, in pixels from the panel's left and top edges.
///
/// `TextOutW` positions a line by the top left of its character cell rather than by its
/// baseline, so this is the gap above the first line rather than a distance down to it.
const TEXT_INSET: i32 = 6;

/// The typeface the numbers are set in.
///
/// Monospaced, so a figure that grows by a digit does not shift the ones beside it, and
/// present on every Windows since Vista.
const FACE: &str = "Consolas";

/// A block of text drawn over the video.
pub struct TextOverlay {
    width: usize,
    height: usize,
    /// The pixels GDI draws into, owned by the bitmap below rather than allocated here.
    pixels: *mut u8,
    /// The bitmap the alpha channel is rebuilt into before it is uploaded.
    staged: Box<[u8]>,
    dc: HDC,
    bitmap: HBITMAP,
    font: HFONT,
    view: ID3D11ShaderResourceView,
    context: ID3D11DeviceContext,
    line_height: f64,
}

impl TextOverlay {
    /// Creates an overlay of the given size in pixels.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if GDI will not allocate a bitmap or a font, or if
    /// Direct3D will not allocate the texture.
    pub fn new(
        renderer: &D3d11Renderer,
        width: usize,
        height: usize,
        font_size: f64,
    ) -> Result<Self, RenderError> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
                biWidth: i32::try_from(width).unwrap_or(i32::MAX),
                // Negative, which is how a device independent bitmap is asked for top down.
                // Left positive the first row in memory would be the bottom of the image and
                // the panel would upload upside down.
                biHeight: -i32::try_from(height).unwrap_or(i32::MAX),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        // SAFETY: the description is fully initialised, the pixel pointer is a live local that
        // the call writes the bitmap's address into, and no file mapping is being used.
        let (dc, bitmap, pixels) = unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(RenderError::Setup {
                    reason: "could not create a drawing context for the overlay",
                });
            }

            let mut pixels: *mut core::ffi::c_void = core::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0)
                .map_err(|_| RenderError::Setup {
                    reason: "could not allocate the overlay bitmap",
                })?;

            if pixels.is_null() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return Err(RenderError::Setup {
                    reason: "the overlay bitmap has no pixels",
                });
            }

            SelectObject(dc, bitmap.into());
            (dc, bitmap, pixels.cast::<u8>())
        };

        let face: Vec<u16> = FACE.encode_utf16().chain(core::iter::once(0)).collect();

        // SAFETY: the face name is nul terminated and outlives the call, which copies it.
        let font = unsafe {
            CreateFontW(
                // Negative asks for a character height in pixels rather than in points, which
                // is what the panel is measured in.
                -(font_size as i32),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (FIXED_PITCH.0 | FF_MODERN.0) as u32,
                PCWSTR(face.as_ptr()),
            )
        };

        if font.is_invalid() {
            // SAFETY: both handles were created above and nothing else refers to them.
            unsafe {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
            }
            return Err(RenderError::Setup {
                reason: "could not create the overlay font",
            });
        }

        // SAFETY: every handle here was created above and belongs to this context.
        unsafe {
            SelectObject(dc, font.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00ff_ffff));
        }

        let staged = vec![0u8; width * height * BYTES_PER_PIXEL].into_boxed_slice();
        let view = renderer.upload_bitmap(
            &staged,
            u32::try_from(width).unwrap_or(1),
            u32::try_from(height).unwrap_or(1),
        )?;

        // SAFETY: the renderer's device and its immediate context outlive the overlay, which
        // is dropped with the window that owns both.
        let context = unsafe {
            renderer
                .device()
                .GetImmediateContext()
                .map_err(|_| RenderError::Setup {
                    reason: "the device has no immediate context",
                })?
        };

        Ok(Self {
            width,
            height,
            pixels,
            staged,
            dc,
            bitmap,
            font,
            view,
            context,
            line_height: font_size * 1.35,
        })
    }

    /// Returns the view the renderer samples.
    #[must_use]
    pub fn view(&self) -> &ID3D11ShaderResourceView {
        &self.view
    }

    /// Returns the quad that draws the overlay pinned to the top left of the target.
    ///
    /// Placed at its natural pixel size rather than as a fraction, so the text stays legible
    /// whatever the window is scaled to instead of stretching with it.
    #[must_use]
    pub fn quad(&self, target_width: usize, target_height: usize) -> Quad<'_> {
        let at = (
            MARGIN as f32 / target_width.max(1) as f32,
            MARGIN as f32 / target_height.max(1) as f32,
        );

        Quad {
            texture: &self.view,
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
        let bytes = self.width * self.height * BYTES_PER_PIXEL;
        let base = self.pixels;

        // SAFETY: the bitmap holds exactly this many bytes for as long as this struct does,
        // and nothing else writes them. The pointer is copied out of the struct first so the
        // slice does not borrow it, which is what lets the staging pass below take `&mut self`.
        let drawn = unsafe { core::slice::from_raw_parts_mut(base, bytes) };
        drawn.fill(0);

        for (index, line) in lines.iter().enumerate() {
            let top = self
                .line_height
                .mul_add(index as f64, f64::from(TEXT_INSET));
            if top + self.line_height > self.height as f64 {
                break;
            }
            self.draw_line(line, TEXT_INSET, top as i32);
        }

        stage(&mut self.staged, drawn);
        self.upload();
    }

    /// Draws one line of text with its top edge at a position in the bitmap.
    fn draw_line(&self, text: &str, x: i32, y: i32) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        if wide.is_empty() {
            return;
        }

        // SAFETY: the context holds the bitmap and the font selected in the constructor, and
        // the slice is alive for the duration of the call.
        unsafe {
            let _ = TextOutW(self.dc, x, y, &wide);
        }
    }

    /// Copies the staged bitmap into the texture.
    fn upload(&self) {
        // SAFETY: the view is one the renderer produced for a texture of exactly this size,
        // and the staged bitmap holds exactly the bytes that texture was created for.
        unsafe {
            if let Ok(resource) = self.view.GetResource() {
                self.context.UpdateSubresource(
                    &resource,
                    0,
                    None,
                    self.staged.as_ptr().cast(),
                    u32::try_from(self.width * BYTES_PER_PIXEL).unwrap_or(0),
                    0,
                );
            }
        }
    }
}

impl Drop for TextOverlay {
    /// Releases the GDI objects, which are not reference counted.
    fn drop(&mut self) {
        // SAFETY: all three handles were created in the constructor, nothing else refers to
        // them, and the context is deleted last because it holds the other two.
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
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

/// Turns what GDI drew into a premultiplied bitmap over the panel's backdrop.
///
/// The text is white, so its coverage is its brightness, and premultiplied white is that same
/// value in all three colour channels. The backdrop is black, whose premultiplied colour is
/// zero, which is why only the alpha channel carries it.
fn stage(staged: &mut [u8], drawn: &[u8]) {
    for (out, source) in staged
        .chunks_exact_mut(BYTES_PER_PIXEL)
        .zip(drawn.chunks_exact(BYTES_PER_PIXEL))
    {
        let coverage = u32::from(source[0].max(source[1]).max(source[2]));
        let alpha = coverage + BACKDROP_ALPHA * (255 - coverage) / 255;

        out[0] = coverage as u8;
        out[1] = coverage as u8;
        out[2] = coverage as u8;
        out[3] = alpha.min(255) as u8;
    }
}

/// An arrow cursor the client draws over the video.
pub struct CursorOverlay {
    view: ID3D11ShaderResourceView,
}

impl CursorOverlay {
    /// Draws the arrow once and uploads it as a texture.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Setup`] if Direct3D will not allocate the texture.
    pub fn new(renderer: &D3d11Renderer) -> Result<Self, RenderError> {
        let size = u32::try_from(SIZE).unwrap_or(1);
        let view = renderer.upload_bitmap(&arrow::bitmap(), size, size)?;

        Ok(Self { view })
    }

    /// Returns the view the renderer samples.
    #[must_use]
    pub fn view(&self) -> &ID3D11ShaderResourceView {
        &self.view
    }

    /// Returns the quad that draws the cursor with its tip at `at`.
    ///
    /// `at` is a fraction of the target in both axes, which is what the tracker reports,
    /// because the client's window is rarely the size of the host's screen.
    #[must_use]
    pub fn quad(&self, at: (f32, f32), target_width: usize, target_height: usize) -> Quad<'_> {
        Quad {
            texture: &self.view,
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
