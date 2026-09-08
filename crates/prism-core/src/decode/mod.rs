//! Video decoding.
//!
//! The decoder takes the Annex B frames the reassembler produces and turns them back
//! into pictures. Like the encoder, every backend is configured for the same thing:
//! decode immediately, hold nothing back, and hand the result over in a form the
//! renderer can draw without copying it through system memory.

#[cfg(target_os = "windows")]
pub mod mediafoundation;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

/// Four-byte Annex B start code, matching what the encoder emits.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// NAL unit type carried by a sequence parameter set.
pub const NAL_SPS: u8 = 7;

/// NAL unit type carried by a picture parameter set.
pub const NAL_PPS: u8 = 8;

/// NAL unit type carried by an IDR slice.
pub const NAL_IDR: u8 = 5;

/// NAL unit type carried by an HEVC video parameter set.
///
/// HEVC has a third parameter set ahead of the other two, and reads its type from a different
/// place: bits one to six of the first byte rather than the low five. A reader that assumed
/// H.264 sees these as slice types and hands them to the decoder as pictures.
pub const HEVC_NAL_VPS: u8 = 32;

/// NAL unit type carried by an HEVC sequence parameter set.
pub const HEVC_NAL_SPS: u8 = 33;

/// NAL unit type carried by an HEVC picture parameter set.
pub const HEVC_NAL_PPS: u8 = 34;

/// Reason a frame could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// A decoder session could not be created for the stream's parameters.
    #[error("could not create the decoder session: {reason} (status {status})")]
    SessionCreate {
        /// What was being attempted.
        reason: &'static str,
        /// Platform status code.
        status: i32,
    },

    /// The stream began without the parameter sets a decoder needs to start.
    #[error("stream has no parameter sets yet, waiting for a keyframe")]
    NoParameterSets,

    /// The bitstream did not parse as Annex B.
    #[error("bitstream is not valid Annex B: {reason}")]
    Bitstream {
        /// What went wrong.
        reason: &'static str,
    },

    /// The platform decoder rejected a frame.
    #[error("decoding a frame failed (status {status})")]
    Decode {
        /// Platform status code.
        status: i32,
    },

    /// A decoded picture could not be read back.
    #[error("could not read the decoded picture: {reason}")]
    Picture {
        /// What went wrong.
        reason: &'static str,
    },
}

/// Iterates the NAL units of an Annex B bitstream.
///
/// Accepts both three- and four-byte start codes, because a stream may be concatenated
/// from sources that differ, and yields each NAL unit without its start code.
///
/// # Examples
///
/// ```
/// # use prism_core::decode::nal_units;
/// let stream = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce];
/// let nals: Vec<&[u8]> = nal_units(&stream).collect();
///
/// assert_eq!(nals, vec![&[0x67u8, 0x42][..], &[0x68, 0xce][..]]);
/// ```
pub fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    NalUnits { stream, offset: 0 }
}

/// Returns the NAL unit type of a NAL that has had its start code removed.
///
/// # Examples
///
/// ```
/// # use prism_core::decode::{NAL_SPS, nal_type};
/// assert_eq!(nal_type(&[0x67, 0x42]), Some(NAL_SPS));
/// assert_eq!(nal_type(&[]), None);
/// ```
#[must_use]
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|&byte| byte & 0x1f)
}

/// Returns the type of an HEVC NAL unit.
///
/// A separate function rather than a parameter on [`nal_type`], because the two numbering
/// schemes do not overlap in meaning and a caller that got the wrong one would read a
/// parameter set as a slice without anything failing.
///
/// # Examples
///
/// ```
/// # use prism_core::decode::{HEVC_NAL_VPS, hevc_nal_type};
/// assert_eq!(hevc_nal_type(&[0x40, 0x01]), Some(HEVC_NAL_VPS));
/// assert_eq!(hevc_nal_type(&[]), None);
/// ```
#[must_use]
pub fn hevc_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|&byte| (byte >> 1) & 0x3f)
}

/// Reads the picture size out of an H.264 sequence parameter set.
///
/// The set is taken with its header byte, as [`nal_units`] yields it. Returns nothing when the
/// bitstream runs out or says something this reader does not follow — a caller that cannot
/// learn the size has other ways to find it out, and guessing one would be worse than not
/// answering.
///
/// # Examples
///
/// ```
/// # use prism_core::decode::h264_dimensions;
/// # use prism_core::encode::h264::Sps;
/// let nal = Sps { width: 1280, height: 720, fps: 60, max_ref_frames: 1 }.to_nal();
///
/// // Past the four byte start code, which `nal_units` would have removed.
/// assert_eq!(h264_dimensions(&nal[4..]), Some((1280, 720)));
/// ```
#[must_use]
pub fn h264_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    /// Profiles that carry the chroma format and bit depths the baseline leaves out.
    const EXTENDED: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

    /// Luma samples across and down one macroblock.
    const MACROBLOCK: u32 = 16;

    let payload = unescape(sps.get(1..)?);
    let mut bits = BitReader::new(&payload);

    let profile = bits.bits(8)?;
    bits.bits(8)?;
    bits.bits(8)?;
    bits.ue()?;

    // 4:2:0 unless the set says otherwise, which is what the subsampling below assumes.
    let mut chroma = 1;

    if EXTENDED.contains(&profile) {
        chroma = bits.ue()?;

        if chroma == 3 {
            bits.flag()?;
        }

        bits.ue()?;
        bits.ue()?;
        bits.flag()?;

        if bits.flag()? {
            // Scaling lists, which are read only to step over: eight for 4:2:0 and twelve when
            // the transform is 8x8 on all three planes.
            let lists = if chroma == 3 { 12 } else { 8 };

            for list in 0..lists {
                if bits.flag()? {
                    bits.skip_scaling_list(if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    bits.ue()?;

    match bits.ue()? {
        0 => {
            bits.ue()?;
        }
        1 => {
            bits.flag()?;
            bits.se()?;
            bits.se()?;

            let cycle = bits.ue()?;
            for _ in 0..cycle.min(256) {
                bits.se()?;
            }
        }
        _ => {}
    }

    bits.ue()?;
    bits.flag()?;

    let across = bits.ue()?.checked_add(1)?;
    let down = bits.ue()?.checked_add(1)?;

    let frames_only = bits.flag()?;
    if !frames_only {
        bits.flag()?;
    }

    bits.flag()?;

    let mut width = across.checked_mul(MACROBLOCK)?;
    let mut height = down
        .checked_mul(if frames_only { 1 } else { 2 })?
        .checked_mul(MACROBLOCK)?;

    if bits.flag()? {
        // Cropping is counted in chroma samples, so how many luma samples each one stands for
        // depends on the subsampling. Vertically it is doubled again for field coding.
        let (across_unit, down_unit) = match chroma {
            0 => (1, 2 - u32::from(frames_only)),
            2 => (2, 2 - u32::from(frames_only)),
            3 => (1, 2 - u32::from(frames_only)),
            _ => (2, (2 - u32::from(frames_only)) * 2),
        };

        let left = bits.ue()?;
        let right = bits.ue()?;
        let top = bits.ue()?;
        let bottom = bits.ue()?;

        width = width.checked_sub(left.checked_add(right)?.checked_mul(across_unit)?)?;
        height = height.checked_sub(top.checked_add(bottom)?.checked_mul(down_unit)?)?;
    }

    (width > 0 && height > 0).then_some((width, height))
}

/// Removes the emulation prevention bytes a bitstream carries.
///
/// A `0x03` after two zero bytes is there so the payload cannot contain a start code, and it
/// is not part of what the syntax describes. Reading the syntax without taking them out lands
/// three bits off for the rest of the set.
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;

    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }

        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }

    out
}

/// Reads the bit-packed syntax a parameter set is written in.
struct BitReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> BitReader<'a> {
    /// Starts at the first bit.
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Reads one bit, or nothing when the stream has run out.
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.bytes.get(self.at / 8)?;
        let shift = 7 - (self.at % 8);
        self.at += 1;

        Some(u32::from((byte >> shift) & 1))
    }

    /// Reads a flag.
    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|bit| bit == 1)
    }

    /// Reads a fixed-width unsigned field, most significant bit first.
    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;

        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }

        Some(value)
    }

    /// Reads an unsigned Exp-Golomb code, which H.264 calls `ue(v)`.
    ///
    /// Refuses anything wider than thirty-two bits rather than wrapping: a set that claims one
    /// is damaged, and reading on from a damaged set produces a plausible size that is wrong.
    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;

        while self.bit()? == 0 {
            leading += 1;

            if leading > 31 {
                return None;
            }
        }

        if leading == 0 {
            return Some(0);
        }

        let rest = self.bits(leading)?;

        Some((1u32 << leading) - 1 + rest)
    }

    /// Reads a signed Exp-Golomb code, which H.264 calls `se(v)`.
    fn se(&mut self) -> Option<i32> {
        let folded = self.ue()?;
        let magnitude = i64::from(folded).div_euclid(2) + i64::from(folded % 2);

        i32::try_from(if folded % 2 == 1 {
            magnitude
        } else {
            -magnitude
        })
        .ok()
    }

    /// Steps over a scaling list without keeping it.
    fn skip_scaling_list(&mut self, size: u32) -> Option<()> {
        let mut last = 8i32;
        let mut next = 8i32;

        for _ in 0..size {
            if next != 0 {
                next = (last + self.se()? + 256) % 256;
            }

            last = if next == 0 { last } else { next };
        }

        Some(())
    }
}

/// Iterator over the NAL units of an Annex B bitstream.
struct NalUnits<'a> {
    stream: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    /// Yields the next NAL unit, skipping its start code.
    fn next(&mut self) -> Option<Self::Item> {
        let start = find_start_code(self.stream, self.offset)?;
        let payload = start.end;
        let next = find_start_code(self.stream, payload);

        let end = next.map_or(self.stream.len(), |range| range.start);
        self.offset = end;

        (payload < end).then(|| &self.stream[payload..end])
    }
}

/// Finds the next start code at or after `from`, returning the range it occupies.
fn find_start_code(stream: &[u8], from: usize) -> Option<core::ops::Range<usize>> {
    let mut i = from;

    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 {
            if stream[i + 2] == 1 {
                return Some(i..i + 3);
            }
            if i + 4 <= stream.len() && stream[i + 2] == 0 && stream[i + 3] == 1 {
                return Some(i..i + 4);
            }
        }
        i += 1;
    }

    None
}

#[cfg(test)]
mod dimension_tests {
    use super::h264_dimensions;
    use crate::encode::h264::Sps;

    /// Every size the encoder can be asked for, read back out of what it wrote.
    ///
    /// A round trip rather than a fixture: the writer beside it is the thing this has to agree
    /// with, and a hand-typed parameter set would only prove that two hand-typed things match.
    #[test]
    fn a_written_parameter_set_reads_back_at_the_size_it_was_written_for() {
        for (width, height) in [
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
            (640, 360),
            (16, 16),
        ] {
            let nal = Sps {
                width,
                height,
                fps: 60,
                max_ref_frames: 1,
            }
            .to_nal();

            assert_eq!(
                h264_dimensions(&nal[4..]),
                Some((width, height)),
                "{width}x{height}",
            );
        }
    }

    /// 1080 is not a whole number of macroblocks, so it is coded taller and cropped back.
    ///
    /// The one case where reading the coded size and reading the real size differ, and the one
    /// most likely to be got wrong.
    #[test]
    fn a_height_that_is_not_whole_macroblocks_is_cropped_back_down() {
        let nal = Sps {
            width: 1920,
            height: 1080,
            fps: 60,
            max_ref_frames: 1,
        }
        .to_nal();

        // 68 macroblocks is 1088 rows, so the set must say to take eight away.
        assert_eq!(h264_dimensions(&nal[4..]), Some((1920, 1080)));
    }

    #[test]
    fn a_truncated_parameter_set_is_refused_rather_than_guessed_at() {
        let nal = Sps {
            width: 1280,
            height: 720,
            fps: 60,
            max_ref_frames: 1,
        }
        .to_nal();

        for cut in 5..nal.len().min(12) {
            assert_eq!(h264_dimensions(&nal[4..cut]), None, "cut at {cut}");
        }

        assert_eq!(h264_dimensions(&[]), None);
        assert_eq!(h264_dimensions(&[0x67]), None);
    }

    /// The escapes a bitstream carries are not part of the syntax and have to come out first.
    #[test]
    fn emulation_prevention_bytes_are_taken_out_before_the_syntax_is_read() {
        assert_eq!(super::unescape(&[0, 0, 3, 1]), vec![0, 0, 1]);
        assert_eq!(super::unescape(&[0, 0, 3, 0, 0, 3, 2]), vec![0, 0, 0, 0, 2]);
        // Only after two zeros. A three anywhere else is data.
        assert_eq!(super::unescape(&[0, 3, 1]), vec![0, 3, 1]);
        assert_eq!(super::unescape(&[]), Vec::<u8>::new());
    }
}
