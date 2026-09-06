//! Rebuilding frames from received packets.
//!
//! Packets arrive out of order, duplicated, or not at all. The reassembler keeps a
//! bounded set of in-flight frames, fills them in as packets land, and hands a frame to
//! the decoder the moment it is whole.
//!
//! The bound is a latency decision rather than a memory one. A frame that is still
//! missing packets after several newer frames have arrived is never going to be useful:
//! waiting for it would stall the decoder for longer than the frame is worth. So the
//! oldest in-flight frame is evicted to make room rather than held.
//!
//! Buffers are recycled across frames, so a steady stream does not allocate.

use crate::net::fec::{FecCodec, ParityBlock};
use crate::net::packet::{FLAG_IDR, FLAG_LAST_OF_FRAME, FecPacket, MAX_VIDEO_PAYLOAD, VideoPacket};

/// Largest `slice_id` the reassembler will accept.
///
/// Encoders in this pipeline use between one and eight slices per frame; the ceiling is
/// well clear of that and stops a corrupt header from sizing an enormous table.
pub const MAX_SLICES_PER_FRAME: usize = 64;

/// What happened to a packet handed to [`FrameReassembler::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Packet was stored and its frame is still incomplete.
    Accepted,

    /// Packet completed its frame, which is now available from
    /// [`FrameReassembler::take_completed`].
    FrameComplete,

    /// Packet repeats one already stored and was discarded.
    Duplicate,

    /// Packet belongs to a frame that was already delivered or evicted.
    Stale,

    /// Packet contradicts the format and was discarded.
    ///
    /// Covers an impossible index, an empty payload, a short packet in the middle of a
    /// slice, a `slice_id` beyond [`MAX_SLICES_PER_FRAME`], and a `pkt_count` that
    /// disagrees with packets already seen for the same slice.
    Invalid,
}

/// Running counters describing what the receive path has seen.
///
/// These feed the stats HUD and, from M4, the congestion controller. `dropped_incomplete`
/// is the number that matters: a non-zero rate means frames are being abandoned, which
/// the encoder should learn about through long-term reference invalidation rather than
/// by being asked for an IDR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    /// Packets stored into a frame.
    pub accepted: u64,
    /// Packets discarded as repeats.
    pub duplicates: u64,
    /// Packets discarded for belonging to a finished or evicted frame.
    pub stale: u64,
    /// Packets discarded as malformed.
    pub invalid: u64,
    /// Frames delivered whole.
    pub completed: u64,
    /// Frames evicted before they ever completed.
    pub dropped_incomplete: u64,
    /// Slices rebuilt from parity rather than lost.
    ///
    /// The number M4 is judged by. Every one of these is a frame that would otherwise have
    /// been thrown away, and a keyframe the encoder did not have to send.
    pub recovered: u64,
}

/// A frame that arrived whole, borrowed from the reassembler's storage.
///
/// The borrow lasts only until the reassembler is used again, which is what lets the
/// buffer be recycled for the next frame without copying the bytes out first.
#[derive(Debug)]
pub struct ReassembledFrame<'a> {
    /// Frame counter this bitstream belongs to.
    pub frame_id: u32,
    /// Host clock at capture time, in microseconds, copied from the packets.
    pub capture_ts_us: u64,
    /// Whether the frame is an IDR.
    pub is_idr: bool,
    /// The complete encoded frame, slices concatenated in order.
    pub data: &'a [u8],
}

/// Reception state for one slice of one frame.
#[derive(Debug, Default)]
struct SliceState {
    seen: bool,
    pkt_count: u16,
    received: u16,
    present: Vec<bool>,
    data: Vec<u8>,
    len: usize,
    parity: ParityBlock,
}

impl SliceState {
    /// Prepares the slice to receive `pkt_count` packets, reusing existing buffers.
    fn begin(&mut self, pkt_count: u16) {
        self.seen = true;
        self.pkt_count = pkt_count;
        self.received = 0;
        self.len = 0;

        self.present.clear();
        self.present.resize(usize::from(pkt_count), false);

        self.data.clear();
        self.data
            .resize(usize::from(pkt_count) * MAX_VIDEO_PAYLOAD, 0);

        self.parity.reset(0, MAX_VIDEO_PAYLOAD);
    }

    /// Stores one parity shard and the block shape it describes.
    ///
    /// A parity packet may be the first thing seen of a slice, so this establishes the
    /// slice's shape as readily as a data packet does. It also carries the slice's true
    /// byte length, which is the only reason recovery of the final packet is correct: the
    /// length otherwise arrives only on that packet, so recovering it would leave a slice
    /// of length zero and hand the decoder an empty bitstream with nothing reporting it.
    ///
    /// Returns whether the packet was consistent with what is already known. A parity
    /// packet describing a different block than the data packets did is refused rather than
    /// mixed in, because reconstruction driven by the wrong counts produces wrong bytes.
    fn accept_parity(&mut self, packet: &FecPacket<'_>) -> bool {
        let data_count = u16::from(packet.data_count);

        if !self.seen {
            self.begin(data_count);
        } else if self.pkt_count != data_count {
            return false;
        }

        if packet.payload.len() != MAX_VIDEO_PAYLOAD {
            return false;
        }

        if self.parity.shard_count() != usize::from(packet.parity_count) {
            self.parity
                .reset(usize::from(packet.parity_count), MAX_VIDEO_PAYLOAD);
        }

        let index = usize::from(packet.shard_index);
        let Some(shard) = self.parity.shard_mut(index) else {
            return false;
        };
        shard.copy_from_slice(packet.payload);
        self.parity.set_present(index, true);

        self.len = packet.slice_len();

        true
    }

    /// Rebuilds the missing packets of this slice from parity, if there are enough shards.
    ///
    /// Returns whether the slice is whole afterwards. Cheap to call on a slice that is
    /// already complete or still hopeless: the codec returns immediately in both cases.
    fn try_recover(&mut self, codec: &mut FecCodec) -> bool {
        if self.is_complete() || self.parity.present_count() == 0 {
            return false;
        }

        let available = usize::from(self.received) + self.parity.present_count();
        if available < usize::from(self.pkt_count) {
            return false;
        }

        match codec.reconstruct(
            &mut self.data,
            &mut self.present,
            &mut self.parity,
            MAX_VIDEO_PAYLOAD,
        ) {
            Ok(rebuilt) if rebuilt > 0 => {
                self.received = self.pkt_count;
                true
            }
            _ => false,
        }
    }

    /// Returns whether every packet of the slice has arrived.
    fn is_complete(&self) -> bool {
        self.seen && self.received == self.pkt_count
    }

    /// Returns the slice bitstream, valid once [`Self::is_complete`] holds.
    fn bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    /// Clears the slice for reuse without releasing its buffers.
    fn reset(&mut self) {
        self.seen = false;
        self.pkt_count = 0;
        self.received = 0;
        self.len = 0;
        self.parity.reset(0, MAX_VIDEO_PAYLOAD);
    }
}

/// One in-flight frame.
#[derive(Debug, Default)]
struct FrameSlot {
    occupied: bool,
    frame_id: u32,
    capture_ts_us: u64,
    is_idr: bool,
    last_slice_id: Option<u16>,
    slices: Vec<SliceState>,
    assembled: Vec<u8>,
}

impl FrameSlot {
    /// Claims the slot for a new frame, reusing existing buffers.
    fn begin(&mut self, frame_id: u32, capture_ts_us: u64) {
        self.occupied = true;
        self.frame_id = frame_id;
        self.capture_ts_us = capture_ts_us;
        self.is_idr = false;
        self.last_slice_id = None;
        for slice in &mut self.slices {
            slice.reset();
        }
        self.assembled.clear();
    }

    /// Returns whether the final slice has been seen and every slice up to it is whole.
    fn is_complete(&self) -> bool {
        let Some(last) = self.last_slice_id else {
            return false;
        };

        (0..=usize::from(last)).all(|id| self.slices.get(id).is_some_and(SliceState::is_complete))
    }

    /// Concatenates the completed slices into [`Self::assembled`].
    fn assemble(&mut self) {
        let Some(last) = self.last_slice_id else {
            return;
        };

        self.assembled.clear();
        for id in 0..=usize::from(last) {
            self.assembled.extend_from_slice(self.slices[id].bytes());
        }
    }
}

/// Rebuilds frames from packets, holding a bounded number in flight.
///
/// # Examples
///
/// ```
/// # use prism_core::net::packet::FLAG_LAST_OF_FRAME;
/// # use prism_core::net::packetize::SlicePacketizer;
/// # use prism_core::net::reassemble::{FrameReassembler, PushOutcome};
/// let bitstream = vec![7u8; 3000];
/// let mut reassembler = FrameReassembler::new(4);
///
/// let mut outcome = PushOutcome::Accepted;
/// for packet in SlicePacketizer::new(1, 0, FLAG_LAST_OF_FRAME, 99, &bitstream).unwrap() {
///     outcome = reassembler.push(&packet);
/// }
///
/// assert_eq!(outcome, PushOutcome::FrameComplete);
/// let frame = reassembler.take_completed().unwrap();
/// assert_eq!(frame.frame_id, 1);
/// assert_eq!(frame.capture_ts_us, 99);
/// assert_eq!(frame.data, &bitstream[..]);
/// ```
#[derive(Debug)]
pub struct FrameReassembler {
    slots: Vec<FrameSlot>,
    codec: FecCodec,
    completed: Option<usize>,
    last_delivered: Option<u32>,
    stats: ReassemblyStats,
}

impl FrameReassembler {
    /// Creates a reassembler holding at most `capacity` frames in flight.
    ///
    /// Capacity trades tolerance for reordering against how long a doomed frame can
    /// occupy a slot. Four is a reasonable default on a LAN; a path with deep
    /// reordering wants more.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::reassemble::FrameReassembler;
    /// let reassembler = FrameReassembler::new(4);
    /// assert_eq!(reassembler.stats().completed, 0);
    /// ```
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "reassembler capacity must be at least one frame"
        );

        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, FrameSlot::default);

        Self {
            slots,
            codec: FecCodec::new(),
            completed: None,
            last_delivered: None,
            stats: ReassemblyStats::default(),
        }
    }

    /// Returns the running counters for this reassembler.
    #[must_use]
    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// Stores one packet and reports what became of it.
    ///
    /// When the return value is [`PushOutcome::FrameComplete`], the frame is waiting in
    /// [`Self::take_completed`] and must be taken before the next call, or it is
    /// overwritten and counted as dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::packet::VideoPacket;
    /// # use prism_core::net::reassemble::{FrameReassembler, PushOutcome};
    /// let mut reassembler = FrameReassembler::new(2);
    /// let packet = VideoPacket {
    ///     frame_id: 1, slice_id: 0, pkt_idx: 0, pkt_count: 2,
    ///     flags: 0, capture_ts_us: 0, payload: &[1],
    /// };
    ///
    /// // A non-final packet carrying less than a full payload cannot be genuine.
    /// assert_eq!(reassembler.push(&packet), PushOutcome::Invalid);
    /// ```
    pub fn push(&mut self, packet: &VideoPacket<'_>) -> PushOutcome {
        let outcome = self.push_inner(packet);

        match outcome {
            PushOutcome::Accepted | PushOutcome::FrameComplete => self.stats.accepted += 1,
            PushOutcome::Duplicate => self.stats.duplicates += 1,
            PushOutcome::Stale => self.stats.stale += 1,
            PushOutcome::Invalid => self.stats.invalid += 1,
        }

        outcome
    }

    /// Stores a parity shard and rebuilds the slice it repairs, if it now can be.
    ///
    /// Parity may arrive before, between or after the packets it protects, so this both
    /// records the shard and attempts recovery. Recovery attempts are cheap on a slice that
    /// is already whole or still short of shards; both return immediately.
    ///
    /// Returns [`PushOutcome::FrameComplete`] when the repair finished the frame, which is
    /// the whole point: a frame that would have been thrown away is delivered instead, with
    /// no keyframe and no visible interruption.
    pub fn push_fec(&mut self, packet: &FecPacket<'_>) -> PushOutcome {
        let outcome = self.push_fec_inner(packet);

        match outcome {
            PushOutcome::Accepted | PushOutcome::FrameComplete => self.stats.accepted += 1,
            PushOutcome::Duplicate => self.stats.duplicates += 1,
            PushOutcome::Stale => self.stats.stale += 1,
            PushOutcome::Invalid => self.stats.invalid += 1,
        }

        outcome
    }

    /// Stores a parity shard and attempts recovery, without touching the counters.
    fn push_fec_inner(&mut self, packet: &FecPacket<'_>) -> PushOutcome {
        if self
            .last_delivered
            .is_some_and(|last| !is_newer(packet.frame_id, last))
        {
            return PushOutcome::Stale;
        }

        self.discard_pending_if_not(packet.frame_id);

        let Some(idx) = self.slot_for(packet.frame_id, packet.capture_ts_us) else {
            return PushOutcome::Stale;
        };

        let slot = &mut self.slots[idx];
        let slice_id = usize::from(packet.slice_id);

        if slot.slices.len() <= slice_id {
            slot.slices.resize_with(slice_id + 1, SliceState::default);
        }

        let slice = &mut slot.slices[slice_id];
        if !slice.accept_parity(packet) {
            return PushOutcome::Invalid;
        }

        if slice.try_recover(&mut self.codec) {
            self.stats.recovered += 1;
        }

        if slot.is_complete() {
            slot.assemble();
            self.completed = Some(idx);
            PushOutcome::FrameComplete
        } else {
            PushOutcome::Accepted
        }
    }

    /// Takes the frame completed by the most recent [`Self::push`], if there is one.
    ///
    /// Taking a frame frees its slot, so the returned borrow keeps the reassembler
    /// mutably borrowed until it is dropped. Copy or submit the bytes, then let the
    /// borrow end before pushing again.
    ///
    /// Delivering a frame also abandons every in-flight frame older than it. A decoder
    /// cannot use a frame that predates one it has already been given, so holding those
    /// slots open would only delay the frames that still matter.
    pub fn take_completed(&mut self) -> Option<ReassembledFrame<'_>> {
        let idx = self.completed.take()?;
        let frame_id = self.slots[idx].frame_id;

        for (i, slot) in self.slots.iter_mut().enumerate() {
            if i != idx && slot.occupied && !is_newer(slot.frame_id, frame_id) {
                slot.occupied = false;
                self.stats.dropped_incomplete += 1;
            }
        }

        let slot = &mut self.slots[idx];
        slot.occupied = false;
        self.last_delivered = Some(frame_id);
        self.stats.completed += 1;

        Some(ReassembledFrame {
            frame_id: slot.frame_id,
            capture_ts_us: slot.capture_ts_us,
            is_idr: slot.is_idr,
            data: &slot.assembled,
        })
    }

    /// Validates and stores a packet without touching the counters.
    fn push_inner(&mut self, packet: &VideoPacket<'_>) -> PushOutcome {
        if !Self::is_plausible(packet) {
            return PushOutcome::Invalid;
        }

        if self
            .last_delivered
            .is_some_and(|last| !is_newer(packet.frame_id, last))
        {
            return PushOutcome::Stale;
        }

        self.discard_pending_if_not(packet.frame_id);

        let Some(idx) = self.slot_for(packet.frame_id, packet.capture_ts_us) else {
            return PushOutcome::Stale;
        };

        let slot = &mut self.slots[idx];
        let slice_id = usize::from(packet.slice_id);

        if slot.slices.len() <= slice_id {
            slot.slices.resize_with(slice_id + 1, SliceState::default);
        }

        let slice = &mut slot.slices[slice_id];
        if slice.seen && slice.pkt_count != packet.pkt_count {
            return PushOutcome::Invalid;
        }
        if !slice.seen {
            slice.begin(packet.pkt_count);
        }

        let pkt_idx = usize::from(packet.pkt_idx);
        if slice.present[pkt_idx] {
            return PushOutcome::Duplicate;
        }

        let offset = pkt_idx * MAX_VIDEO_PAYLOAD;
        slice.data[offset..offset + packet.payload.len()].copy_from_slice(packet.payload);
        slice.present[pkt_idx] = true;
        slice.received += 1;

        if packet.pkt_idx + 1 == packet.pkt_count {
            slice.len = offset + packet.payload.len();
        }

        if packet.flags & FLAG_IDR != 0 {
            slot.is_idr = true;
        }
        if packet.flags & FLAG_LAST_OF_FRAME != 0 {
            slot.last_slice_id = Some(packet.slice_id);
        }

        // Parity for this slice may already be waiting: it is sent after the slice's own
        // packets but overtaking is ordinary, and this packet may have been the one that
        // brought the shard count up to where recovery becomes possible.
        if slice.try_recover(&mut self.codec) {
            self.stats.recovered += 1;
        }

        if slot.is_complete() {
            slot.assemble();
            self.completed = Some(idx);
            PushOutcome::FrameComplete
        } else {
            PushOutcome::Accepted
        }
    }

    /// Returns whether a packet's header is self-consistent.
    ///
    /// Every packet but the last of its slice must carry a full payload, because that is
    /// the only way the receiver can compute where a packet's bytes belong without
    /// having seen the ones before it.
    fn is_plausible(packet: &VideoPacket<'_>) -> bool {
        packet.pkt_count > 0
            && packet.pkt_idx < packet.pkt_count
            && !packet.payload.is_empty()
            && packet.payload.len() <= MAX_VIDEO_PAYLOAD
            && usize::from(packet.slice_id) < MAX_SLICES_PER_FRAME
            && (packet.pkt_idx + 1 == packet.pkt_count || packet.payload.len() == MAX_VIDEO_PAYLOAD)
    }

    /// Abandons an untaken completed frame when a packet for a different frame arrives.
    fn discard_pending_if_not(&mut self, frame_id: u32) {
        let Some(idx) = self.completed else {
            return;
        };

        if self.slots[idx].frame_id != frame_id {
            self.completed = None;
            self.slots[idx].occupied = false;
            self.stats.dropped_incomplete += 1;
        }
    }

    /// Finds the slot holding `frame_id`, claiming or evicting one if necessary.
    ///
    /// Returns `None` only when every slot holds a frame newer than `frame_id`, which
    /// means this packet arrived too late to be worth keeping.
    fn slot_for(&mut self, frame_id: u32, capture_ts_us: u64) -> Option<usize> {
        if let Some(idx) = self
            .slots
            .iter()
            .position(|s| s.occupied && s.frame_id == frame_id)
        {
            return Some(idx);
        }

        if let Some(idx) = self.slots.iter().position(|s| !s.occupied) {
            self.slots[idx].begin(frame_id, capture_ts_us);
            return Some(idx);
        }

        let oldest = self
            .slots
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                if is_newer(a.frame_id, b.frame_id) {
                    core::cmp::Ordering::Greater
                } else {
                    core::cmp::Ordering::Less
                }
            })
            .map(|(idx, _)| idx)?;

        if is_newer(self.slots[oldest].frame_id, frame_id) {
            return None;
        }

        if self.completed == Some(oldest) {
            self.completed = None;
        }
        self.stats.dropped_incomplete += 1;
        self.slots[oldest].begin(frame_id, capture_ts_us);
        Some(oldest)
    }
}

/// Returns whether `a` is strictly newer than `b` under wrapping frame numbering.
///
/// Frame counters wrap at `u32::MAX`, which at 120 fps takes over a year, but comparing
/// them by distance rather than magnitude costs nothing and removes the failure mode
/// entirely.
fn is_newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}
