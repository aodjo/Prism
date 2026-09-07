//! Encodes synthetic frames to an Annex B file.
//!
//! This exists to prove the encoder produces a real elementary stream: the output is a
//! plain `.h264` file that any decoder can open, so a broken bitstream shows up
//! immediately rather than being discovered later inside the client.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use prism_core::encode::EncoderConfig;
use prism_core::encode::videotoolbox::{Nv12Frame, VideoToolboxEncoder};
use prism_core::stats::LatencyRecorder;

/// How the encode probe should run.
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    /// Where to write the Annex B elementary stream.
    pub out: PathBuf,
    /// Frames to encode.
    pub frames: u32,
    /// Encoder settings.
    pub encoder: EncoderConfig,
    /// Where to also write the frames that went in, as raw NV12.
    ///
    /// The pattern is deterministic, so this is what makes a quality comparison possible at
    /// all: two codecs at the same bitrate can only be told apart against the thing they were
    /// both trying to reproduce.
    pub source_out: Option<PathBuf>,
}

/// Encodes `config.frames` synthetic frames and writes the result to disk.
///
/// Reports encode latency per frame and the slice count distribution, which is what says
/// whether the slice size limit is actually cutting frames into pieces that can be sent
/// before the frame is finished.
///
/// # Errors
///
/// Returns an error if the encoder cannot be created, a frame cannot be encoded, or the
/// output file cannot be written.
pub fn run(config: EncodeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = VideoToolboxEncoder::new(config.encoder)?;
    let mut source = Nv12Frame::new(config.encoder.width, config.encoder.height)?;
    let mut out = BufWriter::new(File::create(&config.out)?);
    let mut source_out = config
        .source_out
        .as_ref()
        .map(|path| File::create(path).map(BufWriter::new))
        .transpose()?;

    let mut latency = LatencyRecorder::new(4096);
    let mut bytes_written = 0u64;
    let mut idr_frames = 0u32;
    let mut encoded_frames = 0u32;
    let mut total_slices = 0u64;
    let mut min_slices = usize::MAX;
    let mut max_slices = 0usize;

    println!(
        "encode: {}x{} @ {} fps, {} kbps -> {}",
        config.encoder.width,
        config.encoder.height,
        config.encoder.fps,
        config.encoder.bitrate_bps / 1000,
        config.out.display()
    );
    if encoder.slicing_supported() {
        println!(
            "encode: slice limit {} bytes honoured",
            config.encoder.max_slice_bytes
        );
    } else {
        println!("encode: this encoder does not support a slice size limit");
    }

    let frame_interval_us = 1_000_000 / u64::from(config.encoder.fps.max(1));

    for frame_id in 0..config.frames {
        crate::pattern::paint(&mut source, frame_id as usize)?;

        if let Some(writer) = source_out.as_mut() {
            // Planar, exactly as the encoder will see it, and with the row padding removed so
            // the file is what every tool means by NV12 at this size.
            source.read(|luma, luma_stride, chroma, chroma_stride| {
                let width = config.encoder.width as usize;
                let height = config.encoder.height as usize;

                for y in 0..height {
                    writer.write_all(&luma[y * luma_stride..y * luma_stride + width])?;
                }
                for y in 0..height / 2 {
                    writer.write_all(&chroma[y * chroma_stride..y * chroma_stride + width])?;
                }

                Ok::<(), std::io::Error>(())
            })??;
        }

        let started = Instant::now();
        encoder.encode(
            source.pixel_buffer(),
            u64::from(frame_id) * frame_interval_us,
            frame_id == 0,
        )?;

        let Some(frame) = encoder.poll(Duration::from_secs(2)) else {
            return Err(format!("encoder produced nothing for frame {frame_id}").into());
        };

        latency.record(started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
        out.write_all(&frame.data)?;

        bytes_written += frame.data.len() as u64;
        encoded_frames += 1;
        idr_frames += u32::from(frame.is_idr);
        total_slices += frame.slices.len() as u64;
        min_slices = min_slices.min(frame.slices.len());
        max_slices = max_slices.max(frame.slices.len());
    }

    out.flush()?;

    let summary = latency.summarize().expect("frames were encoded");
    println!(
        "encode: {encoded_frames} frames ({idr_frames} IDR), {:.2} MB, {:.1} kbps average",
        bytes_written as f64 / 1e6,
        bytes_written as f64 * 8.0 * f64::from(config.encoder.fps)
            / f64::from(encoded_frames)
            / 1000.0
    );
    println!(
        "encode: slices per frame min {min_slices} max {max_slices} mean {:.1}",
        total_slices as f64 / f64::from(encoded_frames)
    );
    println!(
        "encode: latency p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms",
        f64::from(summary.p50_us) / 1000.0,
        f64::from(summary.p95_us) / 1000.0,
        f64::from(summary.p99_us) / 1000.0,
        f64::from(summary.max_us) / 1000.0
    );

    Ok(())
}
