//! Probes for macOS system audio capture.
//!
//! Ignored by default: this reads what the machine is actually playing, so it needs a person
//! to make a sound and a machine that is not muted. It exists because the failure it catches
//! is invisible from anywhere else — a capture that opens, delivers buffers on schedule, and
//! fills every one of them with zeroes looks exactly like a quiet room, and the only symptom
//! downstream is that Opus produces five-byte frames forever.

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use prism_core::audio::screencapturekit::SystemAudioCapture;
use prism_core::audio::{FRAME_INTERLEAVED, Pulled, SystemAudio};

/// Reports what two seconds of system audio actually contained.
///
/// Run it with something playing:
///
/// ```sh
/// afplay tone.wav &
/// cargo test -p prism-core --test system_audio -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reads the machine's own audio output; needs a sound playing"]
fn what_the_machine_is_playing_reaches_the_capture() {
    let mut capture = SystemAudioCapture::start().expect("system audio opens");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut frames = 0u32;
    let mut silent = 0u32;
    let mut peak = 0.0f32;
    let mut sum_squares = 0.0f64;
    let mut samples = 0u64;

    while Instant::now() < deadline {
        match capture.poll(Duration::from_millis(50)) {
            Pulled::Frame(frame) => {
                assert_eq!(frame.len(), FRAME_INTERLEAVED, "a frame is a whole frame");
                frames += 1;

                for &sample in frame {
                    peak = peak.max(sample.abs());
                    sum_squares += f64::from(sample) * f64::from(sample);
                }
                samples += frame.len() as u64;
            }
            Pulled::Silence => silent += 1,
            Pulled::Stopped => break,
        }
    }

    let rms = (sum_squares / samples.max(1) as f64).sqrt();
    println!("frames {frames}, nothing-in-time {silent}, peak {peak:.4}, rms {rms:.4}");

    assert!(
        frames > 0,
        "no audio frame arrived in two seconds; the stream opened but delivered nothing"
    );
    assert!(
        peak > 0.001,
        "audio arrived but every sample was zero. Either nothing was playing, or the capture \
         has come adrift from the mix — which is what `excludesCurrentProcessAudio` does when \
         the sound comes from anything sharing this process's responsible process"
    );
}

/// Reports what the encoder makes of what the capture heard.
///
/// The two halves are probed together because their failure looks identical from outside: a
/// capture delivering zeroes and an encoder refusing to spend bits both produce five-byte
/// Opus frames, and only the numbers in between tell them apart.
#[test]
#[ignore = "reads the machine's own audio output; needs a sound playing"]
fn what_the_capture_heard_survives_the_encoder() {
    use prism_core::audio::codec::AudioEncoder;

    let mut capture = SystemAudioCapture::start().expect("system audio opens");
    let mut encoder = AudioEncoder::new(128_000).expect("the encoder starts");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut frames = 0u32;
    let mut peak = 0.0f32;
    let mut bytes = 0usize;
    let mut largest = 0usize;

    while Instant::now() < deadline {
        let Pulled::Frame(frame) = capture.poll(Duration::from_millis(50)) else {
            continue;
        };

        for &sample in frame {
            peak = peak.max(sample.abs());
        }

        let packet = encoder.encode(frame).expect("the frame encodes");
        bytes += packet.len();
        largest = largest.max(packet.len());
        frames += 1;
    }

    println!(
        "frames {frames}, peak {peak:.4}, mean {} bytes, largest {largest}",
        bytes / frames.max(1) as usize
    );

    assert!(frames > 0, "nothing was captured");
    assert!(
        largest > 20,
        "every Opus packet was tiny, which is what silence encodes to — the capture heard a \
         peak of {peak:.4}"
    );
}
