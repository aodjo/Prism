//! The synthetic picture the host encodes when there is no real capture yet.
//!
//! The pattern has to actually move, because a static image compresses to almost nothing
//! and would make both the bitrate and the latency figures meaningless.

#[cfg(target_os = "macos")]
use prism_core::encode::videotoolbox::Nv12Frame;

/// Paints a moving test pattern into an NV12 frame.
///
/// # Errors
///
/// Returns an error if the frame cannot be locked for writing.
#[cfg(target_os = "macos")]
pub fn paint(frame: &mut Nv12Frame, phase: usize) -> Result<(), Box<dyn std::error::Error>> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;

    frame.fill(|luma, luma_stride, chroma, chroma_stride| {
        for y in 0..height {
            let row = &mut luma[y * luma_stride..y * luma_stride + width];
            for (x, pixel) in row.iter_mut().enumerate() {
                let bar = ((x + phase * 7) / 64) % 2;
                let gradient = ((x + y + phase * 3) % 256) as u8;
                *pixel = if bar == 0 { gradient } else { 255 - gradient };
            }
        }

        for y in 0..height / 2 {
            let row = &mut chroma[y * chroma_stride..y * chroma_stride + width];
            for (x, pixel) in row.iter_mut().enumerate() {
                *pixel = if x % 2 == 0 {
                    (128 + ((y + phase) % 64) as i32 - 32) as u8
                } else {
                    (128 + ((x + phase) % 64) as i32 - 32) as u8
                };
            }
        }
    })?;

    Ok(())
}
