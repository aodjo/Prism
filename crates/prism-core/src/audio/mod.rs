//! Audio: capture on the host, Opus across the wire, playback on the client.
//!
//! Sound has its own path from end to end, and that is deliberate. Its packets are tiny, its
//! frames are five milliseconds rather than sixteen, and a lost frame is concealed rather than
//! repaired. Sharing the video path's reassembly, its parity and its display pacing would make
//! every one of those decisions wrong for audio.
//!
//! # Why sound is held separately from picture
//!
//! A picture that is a frame late is invisible. A gap in sound is not: the ear notices a
//! two millisecond dropout that the eye would never see. So audio gets its own jitter buffer,
//! sized by what the path actually does, and picture is not held back waiting for it. The two
//! carry the same host clock, which is what lets them be brought back together at the point
//! where it matters — a person watching somebody speak — rather than being locked together
//! everywhere, which would give the worse behaviour of the two to both.
//!
//! # Why Opus, and why a Rust one
//!
//! Opus is the codec designed for exactly this: it reaches down to five millisecond frames,
//! conceals lost packets, and carries its own in-band redundancy. The implementation is a pure
//! Rust port rather than the C reference, because every other dependency in this crate is pure
//! Rust and adding one C library would mean a cross toolchain for every target. Measured on the
//! client machine at 48 kHz stereo, five millisecond frames, 128 kbps: **24 microseconds to
//! encode and decode one frame**, half a percent of that frame's own duration, at a waveform
//! signal to noise ratio of 36 dB and a band energy error of 4.7 dB.

pub mod codec;
pub mod jitter;

/// Samples per second on the wire.
///
/// Fixed at 48 kHz rather than negotiated. It is what Opus works in natively, what every
/// desktop audio system either uses or resamples to, and a rate agreed once is a rate that
/// cannot be got wrong at three in the morning.
pub const SAMPLE_RATE: u32 = 48_000;

/// Channels on the wire.
pub const CHANNELS: usize = 2;

/// Length of one frame, in microseconds.
///
/// Five milliseconds, chosen by measurement. Two and a half costs nearly twice the processing
/// per unit of sound for a slightly worse spectrum, and ten adds five milliseconds of delay to
/// buy a codec improvement no listener would place. Five is where the curve turns.
pub const FRAME_US: u32 = 5_000;

/// Samples per channel in one frame.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_US as usize) / 1_000_000;

/// Interleaved samples in one frame, both channels together.
pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS;
