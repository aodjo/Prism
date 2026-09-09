//! Replaying a recorded bitstream through this machine's decoder.
//!
//! The client can be told to write every frame it hands the decoder, with
//! `PRISM_DUMP_BITSTREAM`. This reads one of those files back and decodes it, with no socket,
//! no window and no renderer in the way.
//!
//! # Why that is worth a subcommand
//!
//! A new decoder backend fails in one of two ways: it refuses the stream, or it produces
//! nothing anybody can see. Told apart only by the picture on screen, those are the same bug —
//! and the screen needs a renderer, which on a new platform is the other half of the work that
//! is not written yet.
//!
//! So this exists to answer the decoder's half on its own: given bytes that a working decoder
//! elsewhere accepted, does this one produce pictures, of the right size, in the right number.
//! A dump taken on a Mac and replayed on Windows is the same bytes through two decoders, which
//! is the only comparison that says which of them is wrong.

use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

use prism_core::decode::{access_units, detect_codec};
use prism_core::net::negotiate::Codec;

/// How long to wait on a picture before deciding the decoder has none.
///
/// Generous compared with the live path, which cannot afford to wait. Nothing here is racing a
/// display, and a decoder that pipelines several frames deep would otherwise look like one
/// that produced nothing.
const PATIENCE: Duration = Duration::from_millis(200);

/// What a replay came to.
#[derive(Debug, Default)]
struct Report {
    /// Frames the file was split into.
    submitted: u32,
    /// Pictures the decoder handed back.
    decoded: u32,
    /// Frames refused because the stream had not described itself yet.
    waiting: u32,
    /// Frames the decoder rejected outright.
    refused: u32,
    /// The size of the first picture, once there is one.
    size: Option<(u32, u32)>,
    /// How many frames went in before the first one came out.
    ///
    /// This is the decoder's reordering depth, measured rather than asked about, and it is the
    /// delay that no amount of network tuning gets back: at sixty frames a second every frame
    /// of it is sixteen milliseconds. One means the decoder hands each picture over as it
    /// arrives; more means it is holding some back in case an earlier picture is still to come.
    ///
    /// What decides it is the stream, not the decoder — a sequence parameter set that does not
    /// say `max_num_reorder_frames` leaves a decoder no choice but to assume the worst its
    /// level allows. Which makes this the number to watch when changing what the encoder
    /// declares.
    reorder_depth: Option<u32>,
}

/// Decodes a recorded bitstream and reports what came out.
///
/// # Errors
///
/// Returns an error if the file cannot be read, or if the platform has no decoder for the
/// codec. A frame the decoder rejects is counted rather than returned: one bad frame in a
/// recording is a thing to measure, not a reason to stop.
pub fn run(path: &Path, codec: Option<Codec>, verify: bool) -> Result<(), Box<dyn Error>> {
    let stream = std::fs::read(path)?;

    // Read out of the recording unless somebody insisted. Being told the wrong codec is the
    // one mistake here that says nothing: the stream simply yields no frames, which looks
    // exactly like a decoder that cannot read it.
    let codec = match codec.or_else(|| detect_codec(&stream)) {
        Some(codec) => codec,
        None => return Err("the file has no parameter sets, so it says no codec".into()),
    };

    let frames = access_units(&stream, codec);

    println!(
        "replay: {} bytes from {} split into {} frames of {codec:?}",
        stream.len(),
        path.display(),
        frames.len(),
    );

    if frames.is_empty() {
        return Err("the file holds no frames this codec recognises".into());
    }

    let mut decoder = Decoder::new(codec);
    let mut report = Report::default();
    let mut luma = Vec::new();
    let started = Instant::now();

    for (at, frame) in frames.iter().enumerate() {
        report.submitted += 1;

        // The timestamp is the frame's position rather than anything real. Nothing here
        // measures latency: what is being asked is whether pictures come out at all, and a
        // made-up clock that counts up is enough to tell them apart.
        match decoder.decode(frame, at as u64 * 1_000) {
            Ok(()) => {}
            Err(prism_core::decode::DecodeError::NoParameterSets) => {
                report.waiting += 1;
            }
            Err(err) => {
                report.refused += 1;

                if report.refused <= 3 {
                    eprintln!("replay: frame {at} refused: {err}");
                }
            }
        }

        // One wait, then whatever else is already there. Waiting again on the way out of the
        // loop would pay the whole timeout on every frame, because the last call is always the
        // one with nothing left to hand back.
        let ready = decoder
            .poll(PATIENCE)
            .into_iter()
            .chain(core::iter::from_fn(|| decoder.poll(Duration::ZERO)))
            .collect::<Vec<_>>();

        for picture in ready {
            report.decoded += 1;
            report.size.get_or_insert((picture.width, picture.height));
            // How many frames had gone in by the time the first came out, which is the depth
            // the decoder decided to hold. Recorded once, because after the pipeline has
            // filled every later frame comes out one submission behind whatever it started at.
            report.reorder_depth.get_or_insert(report.submitted);

            if verify {
                picture.copy_luma(&mut luma)?;

                // A plane of one value is what a decoder that produced nothing looks like, and
                // it is indistinguishable from a real black frame by the counters alone.
                let flat = luma
                    .first()
                    .is_some_and(|&first| luma.iter().all(|&sample| sample == first));

                if flat && report.decoded <= 3 {
                    eprintln!(
                        "replay: picture {} is a flat {} — the decoder may be producing nothing",
                        report.decoded,
                        luma.first().copied().unwrap_or_default(),
                    );
                }
            }
        }
    }

    // A decoder runs several pictures behind what it has been given, because it needs the
    // frames that follow one before it can finish it. At the end of a recording those never
    // arrive, so it has to be told none are coming before the last few come out.
    decoder.finish();

    while let Some(picture) = decoder.poll(PATIENCE) {
        report.decoded += 1;
        report.size.get_or_insert((picture.width, picture.height));

        if report.decoded >= report.submitted {
            break;
        }
    }

    let elapsed = started.elapsed();

    println!(
        "replay: {} submitted, {} decoded, {} waiting for parameter sets, {} refused",
        report.submitted, report.decoded, report.waiting, report.refused,
    );

    if let Some((width, height)) = report.size {
        println!("replay: pictures are {width} x {height}");
    }

    if let Some(depth) = report.reorder_depth {
        println!(
            "replay: the first picture came out after {depth} frame{}, which is {:.1} ms of \
             reordering at 60 fps",
            if depth == 1 { "" } else { "s" },
            f64::from(depth.saturating_sub(1)) * 1000.0 / 60.0,
        );
    }

    println!(
        "replay: {:.1} ms total, {:.2} ms a frame",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e3 / f64::from(report.submitted.max(1)),
    );

    for status in decoder.take_errors() {
        eprintln!("replay: the decoder reported status {status}");
    }

    if report.decoded == 0 {
        return Err("the decoder produced no pictures at all".into());
    }

    Ok(())
}

/// This machine's decoder, whichever one that is.
#[cfg(target_os = "macos")]
type Decoder = prism_core::decode::videotoolbox::VideoToolboxDecoder;

/// This machine's decoder, whichever one that is.
#[cfg(target_os = "windows")]
type Decoder = prism_core::decode::mediafoundation::MediaFoundationDecoder;
