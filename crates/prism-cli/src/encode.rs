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
    /// How many frames may be inside the encoder at once.
    ///
    /// One submits a frame and waits for it before painting the next, so the measured rate is
    /// paint plus encode latency added together and the hardware idles through both. Anything
    /// more overlaps them, which is what the plan means by encoding asynchronously, and is the
    /// difference between a frame rate bounded by latency and one bounded by throughput.
    ///
    /// Each frame in flight is a source picture the encoder may still be reading, so this is
    /// also how many of them have to exist.
    pub in_flight: usize,
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
    let in_flight = config.in_flight.max(1);
    let mut encoder = VideoToolboxEncoder::new(config.encoder)?;
    // One picture per frame that may be in flight. Reusing a single one would have the painter
    // overwriting pixels the encoder has not finished reading, which does not fail — it
    // produces a stream that decodes into frames nobody drew.
    let mut sources = (0..in_flight)
        .map(|_| Nv12Frame::new(config.encoder.width, config.encoder.height))
        .collect::<Result<Vec<_>, _>>()?;
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

    // When each frame was submitted, so latency stays per-frame once several are in flight.
    // The session forbids frame reordering and emits no B-frames, so what comes out is what
    // went in, in order — which is the only reason a queue of start times lines up at all.
    let mut submitted_at = std::collections::VecDeque::with_capacity(in_flight);
    let mut submitted = 0u32;

    for frame_id in 0..config.frames {
        while submitted < config.frames && (submitted - frame_id) < in_flight as u32 {
            let source = &mut sources[submitted as usize % in_flight];
            crate::pattern::paint(source, submitted as usize)?;

            if let Some(writer) = source_out.as_mut() {
                // Planar, exactly as the encoder will see it, and with the row padding removed
                // so the file is what every tool means by NV12 at this size.
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

            submitted_at.push_back(Instant::now());
            encoder.encode(
                source.pixel_buffer(),
                u64::from(submitted) * frame_interval_us,
                submitted == 0,
            )?;
            submitted += 1;
        }

        let started = submitted_at
            .pop_front()
            .expect("a frame was submitted for every one awaited");

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
