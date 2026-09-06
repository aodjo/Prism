//! Splitting encoded slices into wire packets.
//!
//! The encoder hands out slices as they complete, and each slice is cut into packets
//! small enough to survive the path MTU. Slicing is what lets a frame start moving
//! before it has finished encoding, which is worth roughly half a frame time at the
//! target frame rates.
//!
//! Nothing here allocates. [`SlicePacketizer`] borrows the encoder's bitstream and
//! yields packets whose payloads are subslices of it; the caller serialises each one
//! into a send buffer it owns and reuses.

use crate::net::packet::{MAX_VIDEO_PAYLOAD, VideoPacket};

/// Largest slice this module can packetise, bounded by the `u16` packet counter.
pub const MAX_SLICE_LEN: usize = u16::MAX as usize * MAX_VIDEO_PAYLOAD;

/// Reason a slice could not be packetised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PacketizeError {
    /// Slice had no bytes. An encoder that emits an empty slice is malfunctioning.
    #[error("slice is empty")]
    EmptySlice,

    /// Slice would need more than `u16::MAX` packets.
    #[error("slice is {actual} bytes, exceeds MAX_SLICE_LEN of {MAX_SLICE_LEN}")]
    SliceTooLarge {
        /// Slice length that was rejected.
        actual: usize,
    },
}

/// Cuts one encoded slice into [`VideoPacket`]s.
///
/// Yields packets in order, each carrying at most [`MAX_VIDEO_PAYLOAD`] bytes, with
/// `pkt_idx` counting up and `pkt_count` fixed for the whole slice so the receiver knows
/// when the slice is complete. Every packet repeats the frame's `capture_ts_us`, which
/// is what lets the receiver compute end-to-end latency from any packet that arrives.
///
/// # Examples
///
/// ```
/// # use prism_core::net::packetize::SlicePacketizer;
/// # use prism_core::net::packet::MAX_VIDEO_PAYLOAD;
/// let bitstream = vec![0u8; MAX_VIDEO_PAYLOAD + 10];
/// let packetizer = SlicePacketizer::new(7, 0, 0x02, 1_000, &bitstream).unwrap();
///
/// assert_eq!(packetizer.packet_count(), 2);
/// let packets: Vec<_> = packetizer.collect();
/// assert_eq!(packets[0].payload.len(), MAX_VIDEO_PAYLOAD);
/// assert_eq!(packets[1].payload.len(), 10);
/// assert!(packets.iter().all(|p| p.frame_id == 7 && p.pkt_count == 2));
/// ```
#[derive(Debug, Clone)]
pub struct SlicePacketizer<'a> {
    frame_id: u32,
    slice_id: u16,
    flags: u8,
    capture_ts_us: u64,
    data: &'a [u8],
    next_idx: u16,
    pkt_count: u16,
}

impl<'a> SlicePacketizer<'a> {
    /// Prepares to cut `data` into packets for the given frame and slice.
    ///
    /// `flags` is applied unchanged to every packet of the slice, so the caller sets
    /// [`FLAG_LAST_OF_FRAME`](crate::net::packet::FLAG_LAST_OF_FRAME) only on the slice
    /// that actually ends the frame.
    ///
    /// # Errors
    ///
    /// Returns [`PacketizeError::EmptySlice`] if `data` is empty, or
    /// [`PacketizeError::SliceTooLarge`] if it would need more than `u16::MAX` packets.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packetize::SlicePacketizer;
    /// assert!(SlicePacketizer::new(1, 0, 0, 0, &[]).is_err());
    /// assert!(SlicePacketizer::new(1, 0, 0, 0, &[1, 2, 3]).is_ok());
    /// ```
    pub fn new(
        frame_id: u32,
        slice_id: u16,
        flags: u8,
        capture_ts_us: u64,
        data: &'a [u8],
    ) -> Result<Self, PacketizeError> {
        if data.is_empty() {
            return Err(PacketizeError::EmptySlice);
        }

        if data.len() > MAX_SLICE_LEN {
            return Err(PacketizeError::SliceTooLarge { actual: data.len() });
        }

        let pkt_count = data.len().div_ceil(MAX_VIDEO_PAYLOAD) as u16;

        Ok(Self {
            frame_id,
            slice_id,
            flags,
            capture_ts_us,
            data,
            next_idx: 0,
            pkt_count,
        })
    }

    /// Returns how many packets this slice will produce.
    ///
    /// Known up front, because the receiver needs `pkt_count` in the very first packet
    /// it sees in order to size its reassembly state.
    #[must_use]
    pub fn packet_count(&self) -> u16 {
        self.pkt_count
    }
}

impl<'a> Iterator for SlicePacketizer<'a> {
    type Item = VideoPacket<'a>;

    /// Yields the next packet of the slice, or `None` once the slice is exhausted.
    fn next(&mut self) -> Option<Self::Item> {
        if self.next_idx >= self.pkt_count {
            return None;
        }

        let start = self.next_idx as usize * MAX_VIDEO_PAYLOAD;
        let end = (start + MAX_VIDEO_PAYLOAD).min(self.data.len());

        let packet = VideoPacket {
            frame_id: self.frame_id,
            slice_id: self.slice_id,
            pkt_idx: self.next_idx,
            pkt_count: self.pkt_count,
            flags: self.flags,
            capture_ts_us: self.capture_ts_us,
            payload: &self.data[start..end],
        };

        self.next_idx += 1;
        Some(packet)
    }

    /// Reports the exact number of packets still to come.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::from(self.pkt_count - self.next_idx);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for SlicePacketizer<'_> {}
