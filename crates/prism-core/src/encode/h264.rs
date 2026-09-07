//! Writing the H.264 parameter sets by hand.
//!
//! Nothing here touches a GPU: it is the bitstream syntax and no more, which is why it lives
//! outside the backend that needs it and is tested everywhere rather than only where VAAPI
//! builds.
//!
//! VideoToolbox and NVENC hand back a bitstream with its parameter sets already in it. VAAPI
//! does not: `VAConfigAttribEncPackedHeaders` reports which headers the driver expects to be
//! given, and on the Intel part measured here it reports every one of them. So the sequence
//! and picture parameter sets are written here, bit by bit, and handed to the driver to place
//! ahead of the slice.
//!
//! # These have to agree with the buffers, exactly
//!
//! The same facts are stated twice: once in `VAEncSequenceParameterBufferH264` for the
//! hardware, and once here in the bitstream for the decoder. Nothing checks that the two
//! agree. A frame number wider in one than the other, or a crop in one and not the other,
//! produces a stream the encoder is happy with and the decoder reads as garbage — with no
//! error anywhere, which is this project's least favourite kind of failure. That is why the
//! two are built from one description in [`Sps`] rather than written out separately.

/// A bitstream being written most-significant bit first.
///
/// H.264's syntax is not byte aligned — a parameter set is a run of variable length fields —
/// so writing one means writing bits.
#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits already written into the byte being filled, from zero to seven.
    used: u32,
}

impl BitWriter {
    /// Starts an empty bitstream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes the low `count` bits of `value`, most significant first.
    ///
    /// # Panics
    ///
    /// Panics if `count` exceeds 32, which would silently drop the bits above it.
    pub fn bits(&mut self, value: u32, count: u32) {
        assert!(count <= 32, "a u32 has no {count}th bit");

        for shift in (0..count).rev() {
            let bit = (value >> shift) & 1;

            if self.used == 0 {
                self.bytes.push(0);
            }

            if bit == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 1 << (7 - self.used);
            }

            self.used = (self.used + 1) % 8;
        }
    }

    /// Writes one flag.
    pub fn flag(&mut self, set: bool) {
        self.bits(u32::from(set), 1);
    }

    /// Writes an unsigned Exp-Golomb code, which H.264 calls `ue(v)`.
    ///
    /// Values are stored as their own length in leading zeroes followed by the value plus one,
    /// so small numbers cost few bits — which is why the syntax uses it for nearly everything.
    pub fn ue(&mut self, value: u32) {
        let coded = value + 1;
        let width = 32 - coded.leading_zeros();

        self.bits(0, width - 1);
        self.bits(coded, width);
    }

    /// Writes a signed Exp-Golomb code, which H.264 calls `se(v)`.
    ///
    /// Signed values are folded onto unsigned ones alternating either side of zero: 0, 1, -1,
    /// 2, -2 becomes 0, 1, 2, 3, 4.
    pub fn se(&mut self, value: i32) {
        let folded = if value > 0 {
            (value as u32) * 2 - 1
        } else {
            (-value as u32) * 2
        };

        self.ue(folded);
    }

    /// Closes the stream with a stop bit and pads to a byte, as `rbsp_trailing_bits` requires.
    ///
    /// The stop bit is what tells a decoder where the meaningful bits end, since the padding
    /// after it is indistinguishable from data without it.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.flag(true);
        while self.used != 0 {
            self.flag(false);
        }

        self.bytes
    }

    /// How many bits have been written.
    #[must_use]
    pub fn len_bits(&self) -> usize {
        let whole = self.bytes.len() * 8;

        if self.used == 0 {
            whole
        } else {
            whole - (8 - self.used as usize)
        }
    }
}

/// The four byte start code that separates NAL units in an Annex B stream.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Everything both the hardware and the decoder need to be told about the sequence.
///
/// One description, used to fill the driver's buffer and to write the bitstream, because the
/// two must agree and nothing downstream checks that they do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    /// Picture width in pixels, as the session agreed it.
    pub width: u32,
    /// Picture height in pixels, as the session agreed it.
    pub height: u32,
    /// Frames per second, written into the timing information.
    pub fps: u32,
    /// How many frames the encoder may reference.
    pub max_ref_frames: u32,
}

/// Macroblock size in pixels, for both dimensions.
pub const MACROBLOCK: u32 = 16;

/// H.264's `profile_idc` for High, which is what this encoder configures.
const PROFILE_HIGH: u32 = 100;

/// H.264's `level_idc` for level 4.1, which covers 1080p60 and 1440p at this bitrate.
const LEVEL_4_1: u32 = 41;

/// Width of the frame number field, as `log2_max_frame_num_minus4`.
///
/// Stated in both the bitstream and the driver's sequence buffer, from this one constant.
pub const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;

/// Width of the picture order count field, as `log2_max_pic_order_cnt_lsb_minus4`.
pub const LOG2_MAX_POC_LSB_MINUS4: u32 = 4;

impl Sps {
    /// The picture size in whole macroblocks, which is what the bitstream is written in.
    ///
    /// A height that is not a multiple of sixteen is coded as the next whole macroblock and
    /// cropped back down, which is why 1080 works at all: it is 67.5 macroblocks.
    #[must_use]
    pub fn macroblocks(self) -> (u32, u32) {
        (
            self.width.div_ceil(MACROBLOCK).max(1),
            self.height.div_ceil(MACROBLOCK).max(1),
        )
    }

    /// How many luma rows the coded picture has beyond the real one.
    ///
    /// Cropping is expressed in chroma samples for 4:2:0, so the number written is half this.
    #[must_use]
    pub fn crop_rows(self) -> u32 {
        let (_, mbh) = self.macroblocks();

        mbh * MACROBLOCK - self.height
    }

    /// Writes the sequence parameter set as an Annex B NAL unit.
    ///
    /// Emulation prevention is left to the driver, which inserts it when told the data has
    /// none — doing it here as well would escape the escapes.
    #[must_use]
    pub fn to_nal(self) -> Vec<u8> {
        let (mbw, mbh) = self.macroblocks();
        let mut w = BitWriter::new();

        // nal_ref_idc 3, nal_unit_type 7 (sequence parameter set).
        w.bits(0, 1);
        w.bits(3, 2);
        w.bits(7, 5);

        w.bits(PROFILE_HIGH, 8);
        // Every constraint_set flag off, then two reserved bits.
        w.bits(0, 8);
        w.bits(LEVEL_4_1, 8);
        w.ue(0);

        // High profile carries the chroma format and bit depths that the baseline does not.
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(false);

        w.ue(LOG2_MAX_FRAME_NUM_MINUS4);
        // Picture order count type zero: the order is carried explicitly per picture, which is
        // the only one that works without knowing the frame pattern in advance.
        w.ue(0);
        w.ue(LOG2_MAX_POC_LSB_MINUS4);
        w.ue(self.max_ref_frames);
        w.flag(false);
        w.ue(mbw - 1);
        w.ue(mbh - 1);
        // Frames only, no fields. Interlaced content does not reach this encoder.
        w.flag(true);
        w.flag(true);

        let crop = self.crop_rows();
        w.flag(crop > 0);
        if crop > 0 {
            w.ue(0);
            w.ue(0);
            w.ue(0);
            // In chroma samples, which 4:2:0 subsamples by two vertically.
            w.ue(crop / 2);
        }

        // Video usability information, present only to carry the frame rate. Every flag in it
        // has to be written even when it is off: a decoder reads them positionally, so
        // stopping early has it read the trailing bits as the fields that were left out. That
        // is not a subtle failure — ffmpeg reports a nonsense buffer count and refuses the
        // stream — but it is one nothing in the encoder would have noticed.
        w.flag(true);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);

        // Timing, so a player knows the rate without being told separately. The tick is half
        // a frame because the standard counts fields.
        w.flag(true);
        w.bits(1, 32);
        w.bits(self.fps.max(1) * 2, 32);
        w.flag(true);

        // Neither hardware reference decoder model is described, so no picture timing is sent
        // and no bitstream restriction is claimed.
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);

        let mut nal = START_CODE.to_vec();
        nal.extend_from_slice(&w.finish());

        nal
    }
}

/// Everything the decoder needs to be told about the picture layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pps {
    /// The quantiser the slices are coded around.
    ///
    /// Slices carry their own delta from this, so it only has to be close.
    pub init_qp: u8,
    /// Whether the slices are coded with CABAC rather than CAVLC.
    ///
    /// CABAC is worth a few percent of bitrate and every decoder this project targets has it.
    pub cabac: bool,
}

impl Default for Pps {
    /// The picture layer a session starts with.
    fn default() -> Self {
        Self {
            init_qp: 26,
            cabac: true,
        }
    }
}

impl Pps {
    /// Writes the picture parameter set as an Annex B NAL unit.
    #[must_use]
    pub fn to_nal(self) -> Vec<u8> {
        let mut w = BitWriter::new();

        // nal_ref_idc 3, nal_unit_type 8 (picture parameter set).
        w.bits(0, 1);
        w.bits(3, 2);
        w.bits(8, 5);

        w.ue(0);
        w.ue(0);
        w.flag(self.cabac);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.bits(0, 2);
        w.se(i32::from(self.init_qp) - 26);
        w.se(0);
        w.se(0);
        // The deblocking filter's controls are present, because the slice header carries them.
        w.flag(true);
        w.flag(false);
        w.flag(false);

        // High profile's extension: eight by eight transforms are worth having and cost only
        // this flag, since no scaling matrix is sent with them.
        w.flag(true);
        w.flag(false);
        w.se(0);

        let mut nal = START_CODE.to_vec();
        nal.extend_from_slice(&w.finish());

        nal
    }
}

#[cfg(test)]
mod tests {
    use super::{BitWriter, Pps, Sps};

    #[test]
    fn bits_are_written_most_significant_first() {
        let mut w = BitWriter::new();
        w.bits(0b101, 3);
        w.bits(0b11, 2);

        assert_eq!(w.len_bits(), 5);
        assert_eq!(w.finish(), vec![0b1011_1100]);
    }

    #[test]
    fn exp_golomb_matches_the_standard_examples() {
        // The values H.264's own table gives, which is the cheapest way to know the codes are
        // right: a transposed one produces a parameter set that reads as different numbers.
        for (value, bits, width) in [
            (0u32, 0b1u32, 1u32),
            (1, 0b010, 3),
            (2, 0b011, 3),
            (3, 0b00100, 5),
            (4, 0b00101, 5),
            (5, 0b00110, 5),
            (6, 0b00111, 5),
            (7, 0b0001000, 7),
        ] {
            let mut w = BitWriter::new();
            w.ue(value);

            assert_eq!(w.len_bits(), width as usize, "ue({value}) width");

            let mut expected = BitWriter::new();
            expected.bits(bits, width);
            assert_eq!(w.finish(), expected.finish(), "ue({value}) bits");
        }
    }

    #[test]
    fn signed_exp_golomb_alternates_either_side_of_zero() {
        for (value, folded) in [(0i32, 0u32), (1, 1), (-1, 2), (2, 3), (-2, 4), (3, 5)] {
            let mut signed = BitWriter::new();
            signed.se(value);

            let mut unsigned = BitWriter::new();
            unsigned.ue(folded);

            assert_eq!(signed.finish(), unsigned.finish(), "se({value})");
        }
    }

    #[test]
    fn the_trailing_bits_stop_and_pad() {
        let mut w = BitWriter::new();
        w.bits(0b1, 1);

        // One bit of data, one stop bit, six of padding.
        assert_eq!(w.finish(), vec![0b1100_0000]);
    }

    #[test]
    fn a_sequence_set_starts_the_way_a_decoder_looks_for_one() {
        let nal = Sps {
            width: 1920,
            height: 1080,
            fps: 60,
            max_ref_frames: 1,
        }
        .to_nal();

        assert_eq!(&nal[..4], &[0, 0, 0, 1], "no start code");
        // nal_ref_idc 3, type 7.
        assert_eq!(nal[4], 0b0110_0111, "not a sequence parameter set");
        assert_eq!(nal[5], 100, "not High profile");
        assert_eq!(nal[7], 41, "not level 4.1");
    }

    #[test]
    fn a_picture_set_starts_the_way_a_decoder_looks_for_one() {
        let nal = Pps::default().to_nal();

        assert_eq!(&nal[..4], &[0, 0, 0, 1]);
        // nal_ref_idc 3, type 8.
        assert_eq!(nal[4], 0b0110_1000);
    }

    #[test]
    fn a_height_that_is_not_whole_macroblocks_is_cropped_back() {
        // 1080 is 67.5 macroblocks. Coded as 68 and cropped, which is the only way the
        // commonest picture size in the world is representable at all.
        let sps = Sps {
            width: 1920,
            height: 1080,
            fps: 60,
            max_ref_frames: 1,
        };

        assert_eq!(sps.macroblocks(), (120, 68));
        assert_eq!(sps.crop_rows(), 8);
    }

    #[test]
    fn a_height_that_is_whole_macroblocks_is_not_cropped() {
        let sps = Sps {
            width: 1280,
            height: 720,
            fps: 60,
            max_ref_frames: 1,
        };

        assert_eq!(sps.macroblocks(), (80, 45));
        assert_eq!(sps.crop_rows(), 0);
    }

    #[test]
    fn the_two_picture_sizes_a_session_uses_round_the_same_way() {
        // 1440p and 720p are both whole macroblocks; 1080p is the odd one. Getting this wrong
        // encodes a picture of a different size than the one that was agreed, and the client
        // shows it stretched rather than failing.
        for (width, height, mbw, mbh) in [(2560, 1440, 160, 90), (1280, 720, 80, 45)] {
            let sps = Sps {
                width,
                height,
                fps: 120,
                max_ref_frames: 1,
            };

            assert_eq!(sps.macroblocks(), (mbw, mbh));
            assert_eq!(sps.crop_rows(), 0, "{width}x{height} should not crop");
        }
    }
}
