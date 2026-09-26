//! Pictures as planes of luma and chroma, in system memory.
//!
//! The one place this project handles a picture on the processor on purpose. macOS and Windows
//! never do: the compositor hands a texture to a hardware encoder and a hardware decoder hands one
//! to the renderer. Linux is where that stops being true — a desktop captured through PipeWire
//! arrives as bytes, the encoder that every machine has is a software one, and so is the decoder
//! — and this is the shape the picture takes on its way through.
//!
//! I420 because it is what that encoder and decoder read and write: a full plane of luma and two
//! quarter planes of chroma, one after the other.
//!
//! # The colour the other platforms already agreed on
//!
//! BT.709, video range. That is what the Metal and Direct3D renderers undo and what the NV12
//! shader on Windows produces, so a picture made here and shown there — or made there and shown
//! here — comes out the colour it went in. A different matrix would not fail; it would tint
//! every picture slightly, which is worse.
//!
//! Plain Rust and compiled everywhere, like the parameter sets in [`crate::encode::h264`], so
//! that the arithmetic is tested on the machine the tests happen to run on rather than only on
//! the platform that uses it.

/// Where the three colours sit in a four-byte pixel.
///
/// PipeWire offers several orders and the compositor chooses one, so the conversion is told
/// rather than assuming. The fourth byte is padding or alpha, and either way it is not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelOrder {
    /// The byte holding red.
    pub red: usize,
    /// The byte holding green.
    pub green: usize,
    /// The byte holding blue.
    pub blue: usize,
}

impl PixelOrder {
    /// Blue, green, red, then padding: what most compositors hand over.
    pub const BGRX: Self = Self {
        red: 2,
        green: 1,
        blue: 0,
    };

    /// Red, green, blue, then padding.
    pub const RGBX: Self = Self {
        red: 0,
        green: 1,
        blue: 2,
    };

    /// Padding, then red, green, blue.
    pub const XRGB: Self = Self {
        red: 1,
        green: 2,
        blue: 3,
    };

    /// Padding, then blue, green, red.
    pub const XBGR: Self = Self {
        red: 3,
        green: 2,
        blue: 1,
    };
}

/// One picture as three planes, with no padding at the end of any row.
///
/// The buffers are allocated once, when the picture is made, and written into again for every
/// frame after it. A picture of a different size is a different `I420`.
#[derive(Clone, PartialEq, Eq)]
pub struct I420 {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420 {
    /// Makes a black picture of the given size, rounded down to even.
    ///
    /// Even because chroma covers two pixels in each direction, and a picture with half a
    /// chroma sample at its edge is one no encoder takes.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::yuv::I420;
    /// let picture = I420::new(1921, 1081);
    ///
    /// assert_eq!((picture.width(), picture.height()), (1920, 1080));
    /// assert_eq!(picture.y().len(), 1920 * 1080);
    /// assert_eq!(picture.u().len(), 960 * 540);
    /// ```
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width.max(2) & !1, height.max(2) & !1);
        let luma = width as usize * height as usize;

        Self {
            width,
            height,
            y: vec![16; luma],
            u: vec![128; luma / 4],
            v: vec![128; luma / 4],
        }
    }

    /// The picture's width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The picture's height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The luma plane, one byte a pixel, `width` bytes a row.
    #[must_use]
    pub fn y(&self) -> &[u8] {
        &self.y
    }

    /// The blue-difference plane, one byte for every two by two pixels.
    #[must_use]
    pub fn u(&self) -> &[u8] {
        &self.u
    }

    /// The red-difference plane, laid out like [`Self::u`].
    #[must_use]
    pub fn v(&self) -> &[u8] {
        &self.v
    }

    /// All three planes, to be written into.
    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        (&mut self.y, &mut self.u, &mut self.v)
    }

    /// Fills the picture from a four-byte-a-pixel image, scaling it to fit.
    ///
    /// The source may be any size; it is sampled to this picture's own, nearest pixel for
    /// luma and the average of each two by two block for chroma. Nearest rather than filtered
    /// because this runs on the processor for every frame and a desktop is text and edges, which
    /// a filter blurs — and because the common case is no scaling at all, where the two agree.
    ///
    /// A source shorter than `stride * height` is a frame that arrived torn, and only the rows
    /// it holds are read.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::yuv::{I420, PixelOrder};
    /// let white = [255u8; 4 * 4 * 4];
    /// let mut picture = I420::new(4, 4);
    ///
    /// picture.fill_from(&white, 16, 4, 4, PixelOrder::BGRX);
    ///
    /// assert!(picture.y().iter().all(|&y| y == 235));
    /// assert!(picture.u().iter().all(|&u| u == 128));
    /// ```
    pub fn fill_from(
        &mut self,
        source: &[u8],
        stride: usize,
        width: u32,
        height: u32,
        order: PixelOrder,
    ) {
        let (across, down) = (self.width as usize, self.height as usize);
        let (source_width, source_height) = (width.max(1) as usize, height.max(1) as usize);
        let rows = (source.len() / stride.max(1)).min(source_height);

        if rows == 0 || stride < source_width * 4 {
            return;
        }

        let sample = |x: usize, y: usize| -> (i32, i32, i32) {
            let sx = (x * source_width / across).min(source_width - 1);
            let sy = (y * source_height / down).min(rows - 1);
            let at = sy * stride + sx * 4;

            (
                i32::from(source[at + order.red]),
                i32::from(source[at + order.green]),
                i32::from(source[at + order.blue]),
            )
        };

        for row in (0..down).step_by(2) {
            for column in (0..across).step_by(2) {
                let mut blue_sum = 0;
                let mut red_sum = 0;

                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (r, g, b) = sample(column + dx, row + dy);

                    self.y[(row + dy) * across + column + dx] = luma(r, g, b);
                    blue_sum += blue_difference(r, g, b);
                    red_sum += red_difference(r, g, b);
                }

                let at = (row / 2) * (across / 2) + column / 2;

                self.u[at] = clamp((blue_sum + 2) / 4 + 128);
                self.v[at] = clamp((red_sum + 2) / 4 + 128);
            }
        }
    }
}

impl core::fmt::Debug for I420 {
    /// Names the size, not the bytes.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("I420")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// BT.709 luma, video range, from gamma-encoded red, green and blue.
fn luma(r: i32, g: i32, b: i32) -> u8 {
    clamp(((47 * r + 157 * g + 16 * b + 128) >> 8) + 16)
}

/// BT.709 blue difference, video range, centred on zero.
fn blue_difference(r: i32, g: i32, b: i32) -> i32 {
    (-26 * r - 87 * g + 112 * b + 128) >> 8
}

/// BT.709 red difference, video range, centred on zero.
fn red_difference(r: i32, g: i32, b: i32) -> i32 {
    (112 * r - 102 * g - 10 * b + 128) >> 8
}

/// Keeps a sample inside a byte.
fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::{I420, PixelOrder};

    /// A solid image of one colour, in the given order.
    fn solid(width: usize, height: usize, rgb: [u8; 3], order: PixelOrder) -> Vec<u8> {
        let mut image = vec![0u8; width * height * 4];

        for pixel in image.chunks_exact_mut(4) {
            pixel[order.red] = rgb[0];
            pixel[order.green] = rgb[1];
            pixel[order.blue] = rgb[2];
        }

        image
    }

    #[test]
    fn black_and_white_land_on_the_edges_of_video_range() {
        let mut picture = I420::new(8, 8);

        picture.fill_from(
            &solid(8, 8, [0, 0, 0], PixelOrder::BGRX),
            32,
            8,
            8,
            PixelOrder::BGRX,
        );
        assert!(picture.y().iter().all(|&y| y == 16));

        picture.fill_from(
            &solid(8, 8, [255; 3], PixelOrder::BGRX),
            32,
            8,
            8,
            PixelOrder::BGRX,
        );
        assert!(picture.y().iter().all(|&y| y == 235));
        assert!(picture.u().iter().chain(picture.v()).all(|&c| c == 128));
    }

    #[test]
    fn red_reads_as_red_whichever_order_it_arrived_in() {
        for order in [
            PixelOrder::BGRX,
            PixelOrder::RGBX,
            PixelOrder::XRGB,
            PixelOrder::XBGR,
        ] {
            let mut picture = I420::new(4, 4);

            picture.fill_from(&solid(4, 4, [255, 0, 0], order), 16, 4, 4, order);

            // BT.709 red: Y 63, Cb 102, Cr 240.
            assert!(
                picture.y().iter().all(|&y| (62..=64).contains(&y)),
                "{order:?}"
            );
            assert!(
                picture.u().iter().all(|&u| (101..=103).contains(&u)),
                "{order:?}"
            );
            assert!(
                picture.v().iter().all(|&v| (239..=241).contains(&v)),
                "{order:?}"
            );
        }
    }

    #[test]
    fn a_larger_source_is_scaled_down_to_the_picture() {
        let mut picture = I420::new(4, 2);
        let mut image = solid(8, 4, [0, 0, 0], PixelOrder::BGRX);

        // The right half white.
        for row in 0..4 {
            for column in 4..8 {
                image[(row * 8 + column) * 4..][..3].copy_from_slice(&[255, 255, 255]);
            }
        }

        picture.fill_from(&image, 32, 8, 4, PixelOrder::BGRX);

        assert_eq!(&picture.y()[..4], &[16, 16, 235, 235]);
    }

    #[test]
    fn padded_rows_are_read_at_their_stride() {
        let mut picture = I420::new(2, 2);
        let mut image = vec![0u8; 2 * 64];

        image[..8].copy_from_slice(&[255; 8]);
        image[64..72].copy_from_slice(&[255; 8]);

        picture.fill_from(&image, 64, 2, 2, PixelOrder::BGRX);

        assert!(picture.y().iter().all(|&y| y == 235));
    }

    #[test]
    fn a_torn_frame_is_read_as_far_as_it_goes() {
        let mut picture = I420::new(4, 4);

        picture.fill_from(&[255u8; 16], 16, 4, 4, PixelOrder::BGRX);

        assert!(picture.y().iter().all(|&y| y == 235));
    }

    #[test]
    fn an_odd_size_rounds_down_to_even() {
        let picture = I420::new(7, 5);

        assert_eq!((picture.width(), picture.height()), (6, 4));
        assert_eq!(picture.u().len(), 6);
    }
}
