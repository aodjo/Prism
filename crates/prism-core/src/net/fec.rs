//! Reed-Solomon forward error correction over slice packets.
//!
//! A lost packet is worth repairing only if the repair beats a retransmission, and on a
//! LAN at 120 fps a round trip costs more than the frame it would rescue. So loss is
//! covered ahead of time: every slice is sent with a few extra packets that let the
//! receiver rebuild whichever originals went missing, without asking for anything back.
//!
//! # Shard layout
//!
//! This module never chooses the shard layout, it inherits it.
//! [`SlicePacketizer`](crate::net::packetize::SlicePacketizer) already cuts a slice into
//! fixed-stride fragments of [`MAX_VIDEO_PAYLOAD`](crate::net::packet::MAX_VIDEO_PAYLOAD)
//! bytes, and [`FrameReassembler`](crate::net::reassemble::FrameReassembler) already
//! stores the received fragments in one contiguous matrix at that same stride with a
//! parallel present-bitmap. That is exactly a Reed-Solomon shard matrix, so both sides of
//! this module take the matrix as it already exists and never copy it into a shard-of-
//! shards structure.
//!
//! Reed-Solomon requires every shard to be the same length. A slice is almost never an
//! exact multiple of the stride, so the final data shard is zero-padded to `shard_len`.
//! The padding is never transmitted: the sender's last packet is short, and the receiver
//! stores it into a matrix that is already zeroed.
//!
//! # What FEC cannot recover
//!
//! The parity shards restore *bytes*, not *framing*. A recovered final shard comes back
//! zero-padded to `shard_len` because the codec has no way to know where the real slice
//! ended, so the caller must learn the block's true byte length from somewhere other than
//! this module. See [`FecCodec::reconstruct`].
//!
//! # Block size
//!
//! Reed-Solomon over GF(2^8) cannot address more shards than the field has elements, so a
//! block has a hard ceiling ([`MAX_TOTAL_SHARDS`]). A single-slice 1440p IDR can exceed
//! it. This module **refuses** such a block rather than silently splitting it, because
//! splitting is a wire-format decision: each parity packet would have to name the block it
//! belongs to, and this module deliberately owns no wire format. Callers size their blocks
//! with [`max_data_shards_for`] and split the slice themselves.
//!
//! # Allocation
//!
//! Nothing here allocates per block once warmed up. Every buffer is caller-owned and
//! grows only when a block is larger than any block that buffer has held before. The
//! shard reference tables the codec needs are fixed-size stack arrays, bounded by
//! [`MAX_TOTAL_SHARDS`]. The exceptions are called out on [`FecCodec::encode`] and
//! [`FecCodec::reconstruct`].

use reed_solomon_erasure::galois_8::ReedSolomon;
use thiserror::Error;

/// Hard ceiling on the shards in one block, data plus parity.
///
/// Reed-Solomon over GF(2^8) can address at most 256 shards, one per field element.
/// Prism stops one short so a block's shard counts always fit in a `u8`: a total of 256
/// is not representable in a byte, 255 is. A slice needing more shards than this must be
/// split into several blocks by the caller; see [`max_data_shards_for`].
pub const MAX_TOTAL_SHARDS: usize = 255;

/// Largest data-shard count in a block, leaving room for the one parity shard that makes
/// the block worth encoding at all.
pub const MAX_DATA_SHARDS: usize = MAX_TOTAL_SHARDS - 1;

/// Floor of the adaptive parity ratio, as a percentage of the data-shard count.
///
/// Parity never drops below this even when the path reports no loss at all. A measured
/// loss rate is always stale by at least one round trip, and the first packets of a burst
/// are lost before any estimator can react to them, so the floor buys the burst that the
/// estimate has not seen yet.
pub const MIN_PARITY_PERCENT: u32 = 10;

/// Ceiling of the adaptive parity ratio, as a percentage of the data-shard count.
///
/// Above this, parity competes with the video bitrate for the same congested path and
/// makes the loss it is trying to cover worse. Loss beyond what this ratio covers is a
/// job for the encoder's reference handling, not for FEC.
pub const MAX_PARITY_PERCENT: u32 = 20;

/// How many distinct block shapes keep a prepared codec.
///
/// Building a codec is O(k³) in the data-shard count because it inverts a k×k matrix, far
/// too slow to redo per frame, so shapes are cached. The cache is small on purpose: each
/// prepared codec carries the Reed-Solomon library's own inversion-matrix cache, which
/// grows as distinct loss patterns are seen, and four shapes keeps that bounded.
const CODEC_CACHE_CAPACITY: usize = 4;

/// Reason a block could not be encoded or repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FecError {
    /// Block had no bytes, or the caller declared zero data shards.
    #[error("block is empty")]
    EmptyBlock,

    /// Shard length was zero, which would make every shard indistinguishable.
    #[error("shard length is zero")]
    ZeroShardLen,

    /// Block was asked for no parity, which is not error correction.
    #[error("block was given no parity shards")]
    NoParity,

    /// Block needs more shards than GF(2^8) can address.
    ///
    /// The caller must split the slice into several blocks; see [`max_data_shards_for`].
    #[error(
        "block needs {data_shards} data + {parity_shards} parity shards, exceeds MAX_TOTAL_SHARDS of {MAX_TOTAL_SHARDS}"
    )]
    BlockTooLarge {
        /// Data shards the block would have held.
        data_shards: usize,
        /// Parity shards the block would have held.
        parity_shards: usize,
    },

    /// Data matrix length disagreed with the present-bitmap and the shard length.
    #[error("data matrix is {actual} bytes, expected {expected} for {shards} shards")]
    DataLenMismatch {
        /// Bytes actually supplied.
        actual: usize,
        /// Bytes the shard count and shard length require.
        expected: usize,
        /// Shard count taken from the present-bitmap.
        shards: usize,
    },

    /// Parity shards were sized for a different stride than the data shards.
    #[error("parity shards are {actual} bytes, data shards are {expected}")]
    ShardLenMismatch {
        /// Parity shard length found.
        actual: usize,
        /// Data shard length required.
        expected: usize,
    },

    /// Too much of the block is gone for parity to rebuild it.
    ///
    /// Reed-Solomon needs as many shards present as the block has data shards. This is
    /// reported before anything is written, so the caller's data matrix is untouched and
    /// still holds exactly the shards that genuinely arrived.
    #[error("{missing} data shards missing, only {present} of {needed} shards present")]
    Unrecoverable {
        /// Data shards that were absent.
        missing: usize,
        /// Data plus parity shards that were present.
        present: usize,
        /// Shards that had to be present, equal to the data-shard count.
        needed: usize,
    },

    /// The Reed-Solomon codec rejected the block.
    ///
    /// Unreachable by construction: every shape, length, and presence count the codec
    /// checks is checked here first, and this variant exists so a disagreement between
    /// those checks and the library surfaces as an error rather than a panic. The
    /// payload is the library's own description, kept as a static string so this enum
    /// stays `Copy` and so the codec's error type does not leak into Prism's API.
    #[error("reed-solomon codec rejected an already-validated block: {0}")]
    Codec(&'static str),
}

/// Describes a Reed-Solomon library error without leaking its type into this API.
///
/// Deliberately exhaustive: a library upgrade that adds a failure mode should break this
/// match rather than silently fold the new case into a catch-all.
fn codec_error(err: reed_solomon_erasure::Error) -> FecError {
    use reed_solomon_erasure::Error as RsError;

    FecError::Codec(match err {
        RsError::TooFewShards => "too few shards",
        RsError::TooManyShards => "too many shards",
        RsError::TooFewDataShards => "too few data shards",
        RsError::TooManyDataShards => "too many data shards",
        RsError::TooFewParityShards => "too few parity shards",
        RsError::TooManyParityShards => "too many parity shards",
        RsError::TooFewBufferShards => "too few buffer shards",
        RsError::TooManyBufferShards => "too many buffer shards",
        RsError::IncorrectShardSize => "incorrect shard size",
        RsError::TooFewShardsPresent => "too few shards present",
        RsError::EmptyShard => "empty shard",
        RsError::InvalidShardFlags => "invalid shard flags",
        RsError::InvalidIndex => "invalid shard index",
    })
}

/// Returns the parity-shard count for a block of `data_shards` at a measured loss rate.
///
/// `loss` is a fraction, so 0.05 means five percent. The rule quantises to whole percent
/// immediately and does the rest in integers, so it is exactly reproducible and so the
/// block shapes it produces are stable enough for [`FecCodec`] to keep hitting its codec
/// cache:
///
/// 1. The loss estimate is converted to the **nearest** whole percent and clamped into
///    [`MIN_PARITY_PERCENT`]..=[`MAX_PARITY_PERCENT`]. A negative, zero, or non-finite
///    estimate is treated as no measured loss and takes the floor rather than the
///    ceiling: a broken estimator should not be able to spend 20% of the bitrate.
/// 2. The parity count is `ceil(data_shards * percent / 100)`, in integer arithmetic.
/// 3. The count is reduced if the block would otherwise exceed [`MAX_TOTAL_SHARDS`].
///
/// Step 1 rounds to nearest rather than up because these fractions are not exactly
/// representable in `f32`: `0.15` multiplies out to `15.000000954`, and rounding that up
/// would hand a caller who asked for 15% a 16% ratio. Nearest is stable against that
/// error, which is under a millionth of a percent, and the band's floor already supplies
/// the conservative margin that rounding up was meant to.
///
/// Step 2 rounds **up**, and that is what makes FEC do anything at all on small frames:
/// three shards at the 10% floor is 0.3 shards, which becomes one parity shard rather than
/// none. Since it is integer arithmetic on a percentage of at least [`MIN_PARITY_PERCENT`],
/// the result is at least 1 for any block with at least one data shard.
///
/// Step 3 means a block close to the shard ceiling gets *less* than the nominal ratio. A
/// caller that wants the full ratio splits the block instead; see [`max_data_shards_for`].
///
/// # Examples
///
/// ```
/// # use prism_core::net::fec::parity_shards_for;
/// // A tiny block still gets protection.
/// assert_eq!(parity_shards_for(3, 0.0), 1);
/// // The floor applies below 10%.
/// assert_eq!(parity_shards_for(100, 0.01), 10);
/// // Between the bounds the estimate is used as measured.
/// assert_eq!(parity_shards_for(100, 0.15), 15);
/// // The ceiling applies above 20%.
/// assert_eq!(parity_shards_for(100, 0.90), 20);
/// ```
#[must_use]
pub fn parity_shards_for(data_shards: usize, loss: f32) -> usize {
    if data_shards == 0 || data_shards >= MAX_TOTAL_SHARDS {
        return 0;
    }

    let uncapped = raw_parity_shards(data_shards, loss_percent(loss));
    uncapped.min(MAX_TOTAL_SHARDS - data_shards)
}

/// Returns the largest data-shard count that still fits one block at this loss rate.
///
/// A slice longer than this many shards has to be cut into several blocks, each encoded
/// separately, because [`parity_shards_for`] would otherwise have to shave the ratio down
/// to fit the GF(2^8) ceiling. The answer depends on the loss rate: heavier parity leaves
/// room for fewer data shards.
///
/// # Examples
///
/// ```
/// # use prism_core::net::fec::{max_data_shards_for, parity_shards_for, MAX_TOTAL_SHARDS};
/// let limit = max_data_shards_for(0.20);
/// assert!(limit + parity_shards_for(limit, 0.20) <= MAX_TOTAL_SHARDS);
/// // A lighter loss estimate leaves room for more payload.
/// assert!(max_data_shards_for(0.0) > limit);
/// ```
#[must_use]
pub fn max_data_shards_for(loss: f32) -> usize {
    let percent = loss_percent(loss);
    let mut data_shards = MAX_DATA_SHARDS;

    while data_shards > 1
        && data_shards + raw_parity_shards(data_shards, percent) > MAX_TOTAL_SHARDS
    {
        data_shards -= 1;
    }

    data_shards
}

/// Converts a loss fraction into the nearest whole-percent parity ratio, clamped to the
/// band. Non-finite and non-positive estimates take the floor.
fn loss_percent(loss: f32) -> u32 {
    if !loss.is_finite() || loss <= 0.0 {
        return MIN_PARITY_PERCENT;
    }

    let percent = (loss * 100.0).round();
    if percent <= MIN_PARITY_PERCENT as f32 {
        MIN_PARITY_PERCENT
    } else if percent >= MAX_PARITY_PERCENT as f32 {
        MAX_PARITY_PERCENT
    } else {
        percent as u32
    }
}

/// Returns `ceil(data_shards * percent / 100)` without the shard-ceiling cap.
fn raw_parity_shards(data_shards: usize, percent: u32) -> usize {
    (data_shards * percent as usize).div_ceil(100)
}

/// Caller-owned storage for one block's parity shards.
///
/// The sender fills it with [`FecCodec::encode`] and transmits the shards; the receiver
/// sizes it with [`ParityBlock::reset`], writes whichever parity packets arrived into
/// [`ParityBlock::shard_mut`], and hands it to [`FecCodec::reconstruct`]. One type serves
/// both because the receiver's parity is just a partly filled version of the sender's.
///
/// Reusing one instance across frames is the point: the backing buffers are only ever
/// grown, never released, so a steady stream stops allocating after the first few frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParityBlock {
    shards: Vec<u8>,
    present: Vec<bool>,
    shard_len: usize,
    count: usize,
}

impl ParityBlock {
    /// Creates an empty block that has not yet been sized.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::fec::ParityBlock;
    /// let block = ParityBlock::new();
    /// assert_eq!(block.shard_count(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resizes the block to `count` shards of `shard_len` bytes, all marked absent.
    ///
    /// Shard bytes are zeroed, so a shard left absent never exposes the previous frame's
    /// content to anything that reads it by mistake. This is where a receiver's parity
    /// storage allocates, and only when `count * shard_len` exceeds the largest block
    /// this instance has ever held.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::fec::ParityBlock;
    /// let mut block = ParityBlock::new();
    /// block.reset(3, 1180);
    /// assert_eq!(block.shard_count(), 3);
    /// assert_eq!(block.present_count(), 0);
    /// ```
    pub fn reset(&mut self, count: usize, shard_len: usize) {
        self.count = count;
        self.shard_len = shard_len;

        self.shards.clear();
        self.shards.resize(count * shard_len, 0);

        self.present.clear();
        self.present.resize(count, false);
    }

    /// Returns how many parity shards this block is sized for.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.count
    }

    /// Returns the length of each shard in bytes, or zero before the first
    /// [`Self::reset`].
    #[must_use]
    pub fn shard_len(&self) -> usize {
        self.shard_len
    }

    /// Returns how many shards are marked present.
    #[must_use]
    pub fn present_count(&self) -> usize {
        self.present.iter().filter(|p| **p).count()
    }

    /// Returns whether the shard at `index` is marked present.
    ///
    /// An out-of-range index reads as absent rather than panicking, because indices on
    /// this path come off the wire.
    #[must_use]
    pub fn is_present(&self, index: usize) -> bool {
        self.present.get(index).copied().unwrap_or(false)
    }

    /// Returns shard `index`, or `None` if the index is out of range.
    ///
    /// The bytes are meaningful only when [`Self::is_present`] holds for the same index.
    #[must_use]
    pub fn shard(&self, index: usize) -> Option<&[u8]> {
        if index >= self.count {
            return None;
        }

        let start = index * self.shard_len;
        Some(&self.shards[start..start + self.shard_len])
    }

    /// Returns shard `index` for writing, or `None` if the index is out of range.
    ///
    /// The receiver must write all `shard_len` bytes; a parity packet that arrives short
    /// is malformed, and storing it partially would leave the rest of the shard zeroed
    /// and quietly corrupt whatever the block reconstructs. Marking the shard present is
    /// a separate step, [`Self::set_present`], so a caller cannot mark a shard it failed
    /// to write.
    pub fn shard_mut(&mut self, index: usize) -> Option<&mut [u8]> {
        if index >= self.count {
            return None;
        }

        let start = index * self.shard_len;
        Some(&mut self.shards[start..start + self.shard_len])
    }

    /// Marks shard `index` present or absent, ignoring an out-of-range index.
    pub fn set_present(&mut self, index: usize, present: bool) {
        if let Some(slot) = self.present.get_mut(index) {
            *slot = present;
        }
    }
}

/// One prepared Reed-Solomon codec and the block shape it was built for.
#[derive(Debug)]
struct CachedCodec {
    data_shards: usize,
    parity_shards: usize,
    codec: ReedSolomon,
}

/// Generates and applies Reed-Solomon parity for slice blocks.
///
/// Holds nothing about any particular block. All it owns is a small cache of prepared
/// codecs and one scratch shard for zero-padding a block's short tail, both of which
/// exist so that the per-frame calls do no allocation and no matrix inversion.
///
/// # Examples
///
/// ```
/// # use prism_core::net::fec::{FecCodec, ParityBlock, parity_shards_for};
/// let bitstream: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
/// let shard_len = 1000;
/// let data_shards = bitstream.len().div_ceil(shard_len);
///
/// let mut codec = FecCodec::new();
/// let mut parity = ParityBlock::new();
/// codec
///     .encode(&bitstream, shard_len, parity_shards_for(data_shards, 0.0), &mut parity)
///     .unwrap();
///
/// // Receive side: the whole block arrived except shard 1.
/// let mut data = vec![0u8; data_shards * shard_len];
/// data[..bitstream.len()].copy_from_slice(&bitstream);
/// data[shard_len..2 * shard_len].fill(0);
/// let mut present = vec![true, false, true];
///
/// let recovered = codec
///     .reconstruct(&mut data, &mut present, &mut parity, shard_len)
///     .unwrap();
/// assert_eq!(recovered, 1);
/// assert_eq!(&data[..bitstream.len()], &bitstream[..]);
/// ```
#[derive(Debug)]
pub struct FecCodec {
    cache: Vec<CachedCodec>,
    tail: Vec<u8>,
}

impl Default for FecCodec {
    /// Creates a codec with an empty cache; identical to [`FecCodec::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl FecCodec {
    /// Creates a codec with no prepared shapes.
    ///
    /// The first block of each shape pays for building its codec. Callers that know their
    /// shapes ahead of time can pay that cost off the frame path with [`Self::prepare`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::fec::FecCodec;
    /// let codec = FecCodec::new();
    /// assert_eq!(codec.prepared_shapes(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: Vec::with_capacity(CODEC_CACHE_CAPACITY),
            tail: Vec::new(),
        }
    }

    /// Returns how many block shapes currently have a prepared codec.
    #[must_use]
    pub fn prepared_shapes(&self) -> usize {
        self.cache.len()
    }

    /// Builds and caches the codec for one block shape.
    ///
    /// Worth calling at session setup for the shapes a stream is expected to use, because
    /// building a codec inverts a `data_shards` × `data_shards` matrix and is the one
    /// genuinely expensive thing in this module. Calling it for an already-prepared shape
    /// costs nothing but a lookup.
    ///
    /// # Errors
    ///
    /// Returns [`FecError::EmptyBlock`] if `data_shards` is zero, [`FecError::NoParity`]
    /// if `parity_shards` is zero, and [`FecError::BlockTooLarge`] if the two together
    /// exceed [`MAX_TOTAL_SHARDS`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::fec::FecCodec;
    /// let mut codec = FecCodec::new();
    /// codec.prepare(10, 2).unwrap();
    /// assert_eq!(codec.prepared_shapes(), 1);
    /// ```
    pub fn prepare(&mut self, data_shards: usize, parity_shards: usize) -> Result<(), FecError> {
        Self::check_shape(data_shards, parity_shards)?;
        self.select_codec(data_shards, parity_shards)
    }

    /// Generates the parity shards for one block.
    ///
    /// `block` is the slice bitstream exactly as the packetizer will cut it: the data
    /// shards are its successive `shard_len`-byte fragments, and the last one is short
    /// unless the length divides evenly. `out` is resized to hold `parity_shards` shards,
    /// filled, and marked entirely present.
    ///
    /// Allocation happens in exactly two places, both of them only on growth: `out`'s
    /// buffers when this block needs more parity bytes than any block it has held before,
    /// and the codec cache when this block's shape is one of the first four shapes seen
    /// (or evicted the shape it replaces). The data shards themselves are borrowed
    /// straight out of `block` and never copied; only a short tail is copied, into a
    /// one-shard scratch buffer that grows at most once.
    ///
    /// # Errors
    ///
    /// Returns [`FecError::ZeroShardLen`] if `shard_len` is zero,
    /// [`FecError::EmptyBlock`] if `block` is empty, [`FecError::NoParity`] if
    /// `parity_shards` is zero, and [`FecError::BlockTooLarge`] if the block would need
    /// more than [`MAX_TOTAL_SHARDS`] shards in total.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::fec::{FecCodec, ParityBlock};
    /// let mut codec = FecCodec::new();
    /// let mut parity = ParityBlock::new();
    ///
    /// codec.encode(&[1, 2, 3, 4, 5], 2, 2, &mut parity).unwrap();
    /// assert_eq!(parity.shard_count(), 2);
    /// assert_eq!(parity.shard_len(), 2);
    /// assert_eq!(parity.present_count(), 2);
    /// ```
    pub fn encode(
        &mut self,
        block: &[u8],
        shard_len: usize,
        parity_shards: usize,
        out: &mut ParityBlock,
    ) -> Result<(), FecError> {
        if shard_len == 0 {
            return Err(FecError::ZeroShardLen);
        }
        if block.is_empty() {
            return Err(FecError::EmptyBlock);
        }

        let data_shards = block.len().div_ceil(shard_len);
        Self::check_shape(data_shards, parity_shards)?;

        let tail_len = block.len() % shard_len;
        if tail_len != 0 {
            self.tail.clear();
            self.tail
                .extend_from_slice(&block[block.len() - tail_len..]);
            self.tail.resize(shard_len, 0);
        }

        self.select_codec(data_shards, parity_shards)?;
        out.reset(parity_shards, shard_len);

        let tail = self.tail.as_slice();
        let full_shards = block.len() / shard_len;
        let data_refs: [&[u8]; MAX_TOTAL_SHARDS] = core::array::from_fn(|i| {
            if i < full_shards {
                &block[i * shard_len..(i + 1) * shard_len]
            } else if i == full_shards && tail_len != 0 {
                tail
            } else {
                Default::default()
            }
        });

        let mut chunks = out.shards.chunks_mut(shard_len);
        let mut parity_refs: [&mut [u8]; MAX_TOTAL_SHARDS] =
            core::array::from_fn(|_| chunks.next().unwrap_or_default());

        self.cache[0]
            .codec
            .encode_sep(&data_refs[..data_shards], &mut parity_refs[..parity_shards])
            .map_err(codec_error)?;

        out.present.fill(true);
        Ok(())
    }

    /// Rebuilds the missing data shards of one block in place.
    ///
    /// `data` is the receiver's contiguous shard matrix, `data.len() / shard_len` shards
    /// long, and `data_present` is its parallel bitmap — the layout
    /// [`FrameReassembler`](crate::net::reassemble::FrameReassembler) already keeps, so
    /// the matrix is repaired where it lies and never copied. `parity` holds whichever
    /// parity shards arrived. On success every entry of `data_present` is set, and the
    /// number of shards actually rebuilt is returned.
    ///
    /// Reconstruction is a no-op returning `Ok(0)` when no data shard is missing, so the
    /// caller can hand every slice through this path and pay nothing on the common one.
    ///
    /// # Recovered bytes are padded, not framed
    ///
    /// A rebuilt final shard is zero-padded out to `shard_len`, because parity carries no
    /// record of where the block's real bytes stopped. A caller that recovers the tail
    /// shard must already know the block's true length; it cannot learn it from here.
    ///
    /// # Allocation
    ///
    /// This module allocates nothing: the shard table it hands the codec is a stack array
    /// bounded by [`MAX_TOTAL_SHARDS`]. The Reed-Solomon library does allocate inside a
    /// call that actually repairs something — it builds and caches the inverse matrix for
    /// each distinct loss pattern, and its internal shard lists spill to the heap above 32
    /// shards. That cost falls only on blocks that lost a packet, never on intact ones.
    ///
    /// # Errors
    ///
    /// Returns [`FecError::ZeroShardLen`] if `shard_len` is zero, [`FecError::EmptyBlock`]
    /// if `data_present` is empty, [`FecError::DataLenMismatch`] if `data` is not exactly
    /// `data_present.len() * shard_len` bytes, [`FecError::ShardLenMismatch`] if the
    /// parity shards use a different stride, [`FecError::NoParity`] if `parity` holds no
    /// shards, [`FecError::BlockTooLarge`] if the block exceeds [`MAX_TOTAL_SHARDS`], and
    /// [`FecError::Unrecoverable`] if fewer shards are present than the block has data
    /// shards.
    ///
    /// On any error, `data` and `data_present` are left exactly as they were passed in.
    /// The block is never partially repaired, so a caller that gets an error still holds
    /// precisely the shards that genuinely arrived and can fall back to dropping the
    /// frame rather than decoding invented bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::fec::{FecCodec, FecError, ParityBlock};
    /// let mut codec = FecCodec::new();
    /// let mut parity = ParityBlock::new();
    /// codec.encode(&[1, 2, 3, 4], 2, 1, &mut parity).unwrap();
    ///
    /// // Both data shards gone but only one parity shard: refuse rather than guess.
    /// let mut data = vec![0u8; 4];
    /// let mut present = vec![false, false];
    /// let err = codec
    ///     .reconstruct(&mut data, &mut present, &mut parity, 2)
    ///     .unwrap_err();
    ///
    /// assert!(matches!(err, FecError::Unrecoverable { .. }));
    /// assert_eq!(data, vec![0, 0, 0, 0]);
    /// ```
    pub fn reconstruct(
        &mut self,
        data: &mut [u8],
        data_present: &mut [bool],
        parity: &mut ParityBlock,
        shard_len: usize,
    ) -> Result<usize, FecError> {
        if shard_len == 0 {
            return Err(FecError::ZeroShardLen);
        }

        let data_shards = data_present.len();
        if data_shards == 0 {
            return Err(FecError::EmptyBlock);
        }

        let expected = data_shards * shard_len;
        if data.len() != expected {
            return Err(FecError::DataLenMismatch {
                actual: data.len(),
                expected,
                shards: data_shards,
            });
        }

        let missing = data_present.iter().filter(|p| !**p).count();
        if missing == 0 {
            return Ok(0);
        }

        let parity_shards = parity.shard_count();
        if parity.shard_len() != shard_len {
            return Err(FecError::ShardLenMismatch {
                actual: parity.shard_len(),
                expected: shard_len,
            });
        }
        Self::check_shape(data_shards, parity_shards)?;

        let present = data_shards - missing + parity.present_count();
        if present < data_shards {
            return Err(FecError::Unrecoverable {
                missing,
                present,
                needed: data_shards,
            });
        }

        self.select_codec(data_shards, parity_shards)?;

        let total_shards = data_shards + parity_shards;
        let ParityBlock {
            shards: parity_bytes,
            present: parity_present,
            ..
        } = parity;

        let mut chunks = data
            .chunks_mut(shard_len)
            .chain(parity_bytes.chunks_mut(shard_len));
        let mut shards: [(&mut [u8], bool); MAX_TOTAL_SHARDS] = core::array::from_fn(|i| {
            let present = if i < data_shards {
                data_present[i]
            } else if i < total_shards {
                parity_present[i - data_shards]
            } else {
                false
            };

            (chunks.next().unwrap_or_default(), present)
        });

        self.cache[0]
            .codec
            .reconstruct_data(&mut shards[..total_shards])
            .map_err(codec_error)?;

        data_present.fill(true);
        Ok(missing)
    }

    /// Rejects a block shape Reed-Solomon cannot represent.
    fn check_shape(data_shards: usize, parity_shards: usize) -> Result<(), FecError> {
        if data_shards == 0 {
            return Err(FecError::EmptyBlock);
        }
        if parity_shards == 0 {
            return Err(FecError::NoParity);
        }
        if data_shards + parity_shards > MAX_TOTAL_SHARDS {
            return Err(FecError::BlockTooLarge {
                data_shards,
                parity_shards,
            });
        }

        Ok(())
    }

    /// Moves the codec for this shape to the front of the cache, building it if new.
    ///
    /// Front-of-cache is where [`Self::encode`] and [`Self::reconstruct`] read it from, so
    /// this is the only place either of them needs to think about the cache at all.
    ///
    /// # Errors
    ///
    /// Returns [`FecError::Codec`] if the Reed-Solomon library refuses the shape, which
    /// [`Self::check_shape`] should already have prevented.
    fn select_codec(&mut self, data_shards: usize, parity_shards: usize) -> Result<(), FecError> {
        if let Some(pos) = self
            .cache
            .iter()
            .position(|c| c.data_shards == data_shards && c.parity_shards == parity_shards)
        {
            self.cache[..=pos].rotate_right(1);
            return Ok(());
        }

        let codec = ReedSolomon::new(data_shards, parity_shards).map_err(codec_error)?;

        if self.cache.len() == CODEC_CACHE_CAPACITY {
            self.cache.pop();
        }
        self.cache.insert(
            0,
            CachedCodec {
                data_shards,
                parity_shards,
                codec,
            },
        );

        Ok(())
    }
}
