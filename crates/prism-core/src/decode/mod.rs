//! Video decoding.
//!
//! The decoder takes the Annex B frames the reassembler produces and turns them back
//! into pictures. Like the encoder, every backend is configured for the same thing:
//! decode immediately, hold nothing back, and hand the result over in a form the
//! renderer can draw without copying it through system memory.

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
