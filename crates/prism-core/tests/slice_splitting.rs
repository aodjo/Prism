//! Tests for splitting a long slice so that every part of it is protected.
//!
//! A Reed-Solomon block holds 255 shards in all, so a slice longer than the data half of that
//! cannot be covered by one. Apple Silicon's encoder ignores the slice size limit and hands
//! over whole frames, and a keyframe at a real bitrate is several hundred packets — so without
//! splitting, exactly the frame that must not be lost is the one sent unprotected.
//!
//! The receiver is not told any of this. It rebuilds a frame by concatenating its slices in
//! order, which is why splitting is invisible on the far side, and which is the assumption the
//! whole arrangement rests on. That is what the second test here pins down.

use prism_core::net::fec::max_data_shards_for;
use prism_core::net::packet::{FLAG_LAST_OF_FRAME, MAX_VIDEO_PAYLOAD};
use prism_core::net::packetize::SlicePacketizer;
use prism_core::net::reassemble::{FrameReassembler, PushOutcome};
use prism_core::net::sender::max_slice_bytes;

#[test]
fn every_permitted_slice_fits_one_parity_block() {
    // The invariant the parity path now relies on rather than checking for. Across the whole
    // band of loss estimates the codec accepts, a slice of the permitted length must never
    // need more data shards than a block can hold.
    for percent in 0..=100u32 {
        let loss = percent as f32 / 100.0;
        let bytes = max_slice_bytes(Some(loss));
        let shards = bytes.div_ceil(MAX_VIDEO_PAYLOAD);

        assert!(
            shards <= max_data_shards_for(loss),
            "at {percent}% loss a full slice needs {shards} shards, block holds {}",
            max_data_shards_for(loss)
        );
    }
}

#[test]
fn parity_is_off_means_nothing_is_split() {
    // Splitting buys protection. With no parity there is none to buy, and the extra headers
    // would be spent for nothing.
    assert_eq!(max_slice_bytes(None), usize::MAX);
}

#[test]
fn a_frame_sent_as_several_slices_arrives_as_the_bytes_that_went_in() {
    // What makes the split invisible. If the reassembler ever stopped concatenating slices in
    // order, splitting would silently corrupt every frame large enough to be split, and the
    // symptom would be a decoder refusing a bitstream nobody had touched.
    let limit = max_slice_bytes(Some(0.05));
    let bitstream: Vec<u8> = (0..limit * 2 + 1234).map(|i| (i % 251) as u8).collect();

    let mut reassembler = FrameReassembler::new(4);
    let chunks: Vec<&[u8]> = bitstream.chunks(limit).collect();
    let mut outcome = PushOutcome::Accepted;

    for (slice_id, chunk) in chunks.iter().enumerate() {
        let flags = if slice_id + 1 == chunks.len() {
            FLAG_LAST_OF_FRAME
        } else {
            0
        };

        for packet in
            SlicePacketizer::new(7, slice_id as u16, flags, 1_000, chunk).expect("packetises")
        {
            outcome = reassembler.push(&packet);
        }
    }

    assert_eq!(
        outcome,
        PushOutcome::FrameComplete,
        "the frame never completed"
    );

    let frame = reassembler.take_completed().expect("a completed frame");
    assert_eq!(frame.frame_id, 7);
    assert_eq!(
        frame.data,
        &bitstream[..],
        "the reassembled frame is not what was sent"
    );
}
