//! Reed-Solomon forward error correction tests.
//!
//! The tests here check properties rather than numbers copied out of the implementation:
//! any `k` of `n` shards rebuild the block, a block that has lost more than its parity is
//! refused without touching the caller's bytes, and the parity ratio never leaves its
//! documented band. The one number they do pin is the GF(2^8) shard ceiling, because that
//! is a property of the field rather than of this code.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use prism_core::net::fec::{
    FecCodec, FecError, MAX_DATA_SHARDS, MAX_PARITY_PERCENT, MAX_TOTAL_SHARDS, MIN_PARITY_PERCENT,
    ParityBlock, max_data_shards_for, parity_shards_for,
};
use prism_core::net::packet::MAX_VIDEO_PAYLOAD;

thread_local! {
    /// Heap allocations made on this thread while [`CountingAllocator`] is installed.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// Forwards every allocation to the system allocator, counting the ones that take new
/// memory so a test can prove the frame path stops allocating once it is warm.
struct CountingAllocator;

// SAFETY: every method forwards its arguments unchanged to `System`, which is a correct
// `GlobalAlloc`, so all of that implementation's guarantees carry over verbatim. The
// counter is a `Cell<u64>` in a `const`-initialised thread local, so touching it neither
// allocates nor registers a destructor and cannot re-enter the allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: `layout` is forwarded exactly as the caller supplied it.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, which hands out `System`'s pointers,
        // and `layout` is the one it was allocated with.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: `ptr` and `layout` are as required by the caller's contract, and
        // `new_size` is forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `body` and reports how many heap allocations it made on this thread.
fn count_allocations(body: impl FnOnce()) -> u64 {
    let before = ALLOCATIONS.with(Cell::get);
    body();
    ALLOCATIONS.with(Cell::get) - before
}

/// Builds a deterministic pseudo-random bitstream.
///
/// A counter would make a zeroed or duplicated shard hard to notice; these bytes make any
/// shard that is not exactly the right one stand out.
fn bitstream(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 33) as u8
        })
        .collect()
}

/// Lays a bitstream out as the receiver's shard matrix would hold it, zero-padded.
fn data_matrix(block: &[u8], shard_len: usize) -> Vec<u8> {
    let shards = block.len().div_ceil(shard_len);
    let mut matrix = vec![0u8; shards * shard_len];
    matrix[..block.len()].copy_from_slice(block);
    matrix
}

/// One block encoded and ready to be damaged, mirroring what the receiver would hold.
struct Block {
    block: Vec<u8>,
    shard_len: usize,
    data_shards: usize,
    parity_shards: usize,
    matrix: Vec<u8>,
    parity: ParityBlock,
}

impl Block {
    /// Encodes a fresh block of `len` bytes with `parity_shards` parity shards.
    fn encode(codec: &mut FecCodec, len: usize, shard_len: usize, parity_shards: usize) -> Self {
        let block = bitstream(len);
        let mut parity = ParityBlock::new();
        codec
            .encode(&block, shard_len, parity_shards, &mut parity)
            .expect("block should encode");

        Self {
            matrix: data_matrix(&block, shard_len),
            data_shards: block.len().div_ceil(shard_len),
            block,
            shard_len,
            parity_shards,
            parity,
        }
    }

    /// Returns the receive-side state with the shards in `lost` missing.
    ///
    /// Shard indices run over the whole block: `0..data_shards` are data shards and the
    /// rest are parity, exactly as the codec numbers them.
    fn receive(&self, lost: &[usize]) -> (Vec<u8>, Vec<bool>, ParityBlock) {
        let mut matrix = self.matrix.clone();
        let mut present = vec![true; self.data_shards];
        let mut parity = self.parity.clone();

        for &shard in lost {
            if shard < self.data_shards {
                present[shard] = false;
                let start = shard * self.shard_len;
                matrix[start..start + self.shard_len].fill(0);
            } else {
                let index = shard - self.data_shards;
                parity.set_present(index, false);
                parity
                    .shard_mut(index)
                    .expect("parity index in range")
                    .fill(0);
            }
        }

        (matrix, present, parity)
    }

    /// Total shards in the block, data plus parity.
    fn total_shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }
}

/// Every combination of `k` values drawn from `0..n`.
fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    if k == 0 {
        return vec![Vec::new()];
    }

    let mut out = Vec::new();
    for first in 0..n {
        if n - first < k {
            break;
        }
        for mut rest in combinations(n, k - 1) {
            if rest.first().is_some_and(|&next| next <= first) {
                continue;
            }
            rest.insert(0, first);
            out.push(rest);
        }
    }

    out
}

#[test]
fn losing_any_single_shard_recovers_the_block() {
    let mut codec = FecCodec::new();
    // A block whose length is deliberately not a multiple of the stride, so the last data
    // shard is the short, zero-padded one and gets dropped like all the others.
    let block = Block::encode(&mut codec, 7 * 300 + 91, 300, 3);

    for shard in 0..block.total_shards() {
        let (mut matrix, mut present, mut parity) = block.receive(&[shard]);
        let recovered = codec
            .reconstruct(&mut matrix, &mut present, &mut parity, block.shard_len)
            .unwrap_or_else(|e| panic!("shard {shard} should recover: {e}"));

        let expected = usize::from(shard < block.data_shards);
        assert_eq!(recovered, expected, "shard {shard}");
        assert_eq!(
            &matrix[..block.block.len()],
            &block.block[..],
            "shard {shard} left the block wrong"
        );
        assert!(present.iter().all(|p| *p), "shard {shard}");
    }
}

#[test]
fn losing_exactly_the_parity_count_recovers_from_any_combination() {
    let mut codec = FecCodec::new();
    let block = Block::encode(&mut codec, 6 * 64, 64, 3);
    let combos = combinations(block.total_shards(), block.parity_shards);
    assert_eq!(combos.len(), 84, "expected every 3-of-9 combination");

    for lost in &combos {
        let (mut matrix, mut present, mut parity) = block.receive(lost);
        codec
            .reconstruct(&mut matrix, &mut present, &mut parity, block.shard_len)
            .unwrap_or_else(|e| panic!("losing {lost:?} should recover: {e}"));

        assert_eq!(matrix, block.matrix, "losing {lost:?} rebuilt wrong bytes");
    }
}

#[test]
fn losing_one_more_than_the_parity_count_fails_without_writing() {
    let mut codec = FecCodec::new();
    let block = Block::encode(&mut codec, 6 * 64, 64, 3);

    for lost in combinations(block.total_shards(), block.parity_shards + 1) {
        let (mut matrix, mut present, mut parity) = block.receive(&lost);
        let before_matrix = matrix.clone();
        let before_present = present.clone();

        let err = codec
            .reconstruct(&mut matrix, &mut present, &mut parity, block.shard_len)
            .expect_err("losing more than the parity count must fail");

        assert!(
            matches!(err, FecError::Unrecoverable { .. }),
            "losing {lost:?} gave {err:?}"
        );
        assert_eq!(
            matrix, before_matrix,
            "losing {lost:?} wrote bytes into a block it could not rebuild"
        );
        assert_eq!(present, before_present, "losing {lost:?}");
    }
}

#[test]
fn a_realistic_slice_survives_its_full_parity_budget() {
    let mut codec = FecCodec::new();
    let block = Block::encode(
        &mut codec,
        64 * MAX_VIDEO_PAYLOAD - 17,
        MAX_VIDEO_PAYLOAD,
        parity_shards_for(64, 0.2),
    );
    assert_eq!(block.parity_shards, 13);

    // Lose the parity budget spread across data and parity shards, back to back the way a
    // burst on a real path would take them.
    let lost: Vec<usize> = (30..30 + block.parity_shards).collect();
    let (mut matrix, mut present, mut parity) = block.receive(&lost);

    let recovered = codec
        .reconstruct(&mut matrix, &mut present, &mut parity, block.shard_len)
        .expect("a full-budget burst should recover");

    assert_eq!(recovered, block.parity_shards);
    assert_eq!(&matrix[..block.block.len()], &block.block[..]);
}

#[test]
fn a_recovered_tail_shard_comes_back_zero_padded() {
    let mut codec = FecCodec::new();
    let shard_len = 300;
    let block = Block::encode(&mut codec, 4 * shard_len + 11, shard_len, 2);

    let last = block.data_shards - 1;
    let (mut matrix, mut present, mut parity) = block.receive(&[last]);
    codec
        .reconstruct(&mut matrix, &mut present, &mut parity, shard_len)
        .expect("the tail shard should recover");

    // The real bytes come back, and the codec pads the rest: it has no idea the block
    // ended at 11 bytes into this shard. Whoever consumes the block has to know the true
    // length from elsewhere, because nothing here can tell padding from payload.
    assert_eq!(&matrix[..block.block.len()], &block.block[..]);
    assert!(matrix[block.block.len()..].iter().all(|b| *b == 0));
    assert_eq!(matrix.len(), block.data_shards * shard_len);
}

#[test]
fn an_intact_block_is_a_no_op() {
    let mut codec = FecCodec::new();
    let block = Block::encode(&mut codec, 5 * 128, 128, 2);
    let (mut matrix, mut present, mut parity) = block.receive(&[]);

    // Even with every parity shard lost, an intact block needs nothing.
    for index in 0..block.parity_shards {
        parity.set_present(index, false);
    }

    assert_eq!(
        codec
            .reconstruct(&mut matrix, &mut present, &mut parity, block.shard_len)
            .expect("an intact block never fails"),
        0
    );
    assert_eq!(matrix, block.matrix);
}

#[test]
fn a_block_at_the_shard_ceiling_encodes_and_a_larger_one_is_refused() {
    let mut codec = FecCodec::new();
    let shard_len = 4;
    let mut parity = ParityBlock::new();

    // Exactly MAX_TOTAL_SHARDS: the largest block GF(2^8) can address.
    let at_limit = bitstream(MAX_DATA_SHARDS * shard_len);
    codec
        .encode(&at_limit, shard_len, 1, &mut parity)
        .expect("a block of exactly MAX_TOTAL_SHARDS shards must encode");
    assert_eq!(at_limit.len() / shard_len + 1, MAX_TOTAL_SHARDS);

    // One data shard more is one shard past the field.
    let over_limit = bitstream((MAX_DATA_SHARDS + 1) * shard_len);
    assert_eq!(
        codec.encode(&over_limit, shard_len, 1, &mut parity),
        Err(FecError::BlockTooLarge {
            data_shards: MAX_DATA_SHARDS + 1,
            parity_shards: 1,
        })
    );

    // So is the same data with one parity shard too many.
    assert_eq!(
        codec.encode(&at_limit, shard_len, 2, &mut parity),
        Err(FecError::BlockTooLarge {
            data_shards: MAX_DATA_SHARDS,
            parity_shards: 2,
        })
    );
}

#[test]
fn a_block_at_the_ceiling_still_recovers() {
    let mut codec = FecCodec::new();
    let shard_len = 4;
    let block = Block::encode(&mut codec, MAX_DATA_SHARDS * shard_len, shard_len, 1);
    assert_eq!(block.total_shards(), MAX_TOTAL_SHARDS);

    let (mut matrix, mut present, mut parity) = block.receive(&[MAX_DATA_SHARDS - 1]);
    assert_eq!(
        codec
            .reconstruct(&mut matrix, &mut present, &mut parity, shard_len)
            .expect("the largest legal block must still recover"),
        1
    );
    assert_eq!(matrix, block.matrix);
}

#[test]
fn max_data_shards_for_is_the_largest_count_that_fits() {
    for loss in [0.0, 0.05, 0.1, 0.12, 0.15, 0.2, 0.5, 1.0] {
        // A block of exactly 100 data shards is never capped, so its parity count reads
        // back the whole-percent ratio the loss estimate resolved to. That lets this test
        // state the rule independently rather than re-deriving it from the code.
        let percent = parity_shards_for(100, loss);
        assert!((MIN_PARITY_PERCENT as usize..=MAX_PARITY_PERCENT as usize).contains(&percent));
        let uncapped = |shards: usize| (shards * percent).div_ceil(100);

        let limit = max_data_shards_for(loss);
        assert!(limit <= MAX_DATA_SHARDS, "loss {loss}");

        // At the limit the full ratio fits, so nothing is shaved off it.
        assert!(limit + uncapped(limit) <= MAX_TOTAL_SHARDS, "loss {loss}");
        assert_eq!(
            parity_shards_for(limit, loss),
            uncapped(limit),
            "loss {loss}: the limit must not need capping"
        );

        // One shard past it, the full ratio no longer fits.
        assert!(
            limit == MAX_DATA_SHARDS || limit + 1 + uncapped(limit + 1) > MAX_TOTAL_SHARDS,
            "loss {loss}: {} data shards should not fit at the full ratio",
            limit + 1
        );
    }

    // Heavier parity leaves room for less payload.
    assert!(max_data_shards_for(0.2) < max_data_shards_for(0.0));
}

#[test]
fn parity_ratio_stays_inside_its_band_for_every_block_size() {
    let losses = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        -1.0,
        0.0,
        0.001,
        0.05,
        0.1,
        0.101,
        0.15,
        0.199,
        0.2,
        0.201,
        0.75,
        1.0,
    ];

    for &loss in &losses {
        for data_shards in 1..=MAX_DATA_SHARDS {
            let parity = parity_shards_for(data_shards, loss);

            assert!(
                parity >= 1,
                "loss {loss}, {data_shards} shards: FEC must never be a no-op"
            );
            assert!(
                data_shards + parity <= MAX_TOTAL_SHARDS,
                "loss {loss}, {data_shards} shards: block must fit GF(2^8)"
            );

            // Never more than the ceiling ratio, rounded up.
            let ceiling = (data_shards * MAX_PARITY_PERCENT as usize).div_ceil(100);
            assert!(parity <= ceiling, "loss {loss}, {data_shards} shards");

            // Never less than the floor ratio, except where the shard ceiling forced it
            // down, which is exactly when the block should have been split instead.
            let floor = (data_shards * MIN_PARITY_PERCENT as usize).div_ceil(100);
            assert!(
                parity >= floor || data_shards + floor > MAX_TOTAL_SHARDS,
                "loss {loss}, {data_shards} shards"
            );
        }
    }
}

#[test]
fn parity_ratio_clamps_to_the_band_and_rounds_shards_up() {
    // The requirement that makes FEC worth having on small frames: three shards at the
    // floor still buys a parity shard rather than rounding away to nothing.
    assert_eq!(parity_shards_for(3, 0.0), 1);
    assert_eq!(parity_shards_for(1, 0.0), 1);

    // Below the floor and above the ceiling, the band wins.
    assert_eq!(parity_shards_for(100, 0.0), 10);
    assert_eq!(parity_shards_for(100, 0.05), 10);
    assert_eq!(parity_shards_for(100, 0.10), 10);
    assert_eq!(parity_shards_for(100, 0.20), 20);
    assert_eq!(parity_shards_for(100, 0.95), 20);

    // In between, the estimate is used as measured, at whole-percent resolution. 0.15 is
    // the case that catches a naive ceil(): in f32 it multiplies out to 15.000000954, so
    // rounding up would quietly charge a caller who asked for 15% a 16% ratio.
    assert_eq!(parity_shards_for(100, 0.15), 15);
    assert_eq!(parity_shards_for(100, 0.144), 14);
    assert_eq!(parity_shards_for(100, 0.146), 15);
    assert_eq!(parity_shards_for(100, 0.199), 20);

    // A broken estimator must not be able to spend the ceiling ratio.
    assert_eq!(parity_shards_for(100, f32::NAN), 10);
    assert_eq!(parity_shards_for(100, f32::INFINITY), 10);
    assert_eq!(parity_shards_for(100, -0.5), 10);

    // Monotone in the loss estimate: more measured loss never buys less parity.
    let mut previous = 0;
    for step in 0..=100 {
        let parity = parity_shards_for(200, step as f32 / 100.0);
        assert!(parity >= previous, "parity fell at loss {step}%");
        previous = parity;
    }

    // A block with no data shards has nothing to protect.
    assert_eq!(parity_shards_for(0, 0.15), 0);
}

#[test]
fn degenerate_inputs_are_refused() {
    let mut codec = FecCodec::new();
    let mut parity = ParityBlock::new();

    assert_eq!(
        codec.encode(&[1, 2, 3], 0, 1, &mut parity),
        Err(FecError::ZeroShardLen)
    );
    assert_eq!(
        codec.encode(&[], 4, 1, &mut parity),
        Err(FecError::EmptyBlock)
    );
    assert_eq!(
        codec.encode(&[1, 2, 3], 4, 0, &mut parity),
        Err(FecError::NoParity)
    );

    codec.encode(&[1, 2, 3, 4], 2, 1, &mut parity).unwrap();

    let mut matrix = vec![0u8; 4];
    let mut present = vec![false, true];

    assert_eq!(
        codec.reconstruct(&mut matrix, &mut present, &mut parity, 0),
        Err(FecError::ZeroShardLen)
    );
    assert_eq!(
        codec.reconstruct(&mut matrix, &mut [], &mut parity, 2),
        Err(FecError::EmptyBlock)
    );
    assert_eq!(
        codec.reconstruct(&mut [0u8; 3], &mut present, &mut parity, 2),
        Err(FecError::DataLenMismatch {
            actual: 3,
            expected: 4,
            shards: 2,
        })
    );
    assert_eq!(
        codec.reconstruct(&mut matrix, &mut present, &mut ParityBlock::new(), 2),
        Err(FecError::ShardLenMismatch {
            actual: 0,
            expected: 2,
        })
    );

    // A stride the parity was not built for must be caught, not silently misread.
    let mut wrong_stride = ParityBlock::new();
    wrong_stride.reset(1, 3);
    assert_eq!(
        codec.reconstruct(&mut matrix, &mut present, &mut wrong_stride, 2),
        Err(FecError::ShardLenMismatch {
            actual: 3,
            expected: 2,
        })
    );
}

#[test]
fn parity_block_bounds_are_checked_rather_than_panicking() {
    let mut block = ParityBlock::new();
    assert!(block.shard(0).is_none());
    assert!(!block.is_present(0));

    block.reset(2, 8);
    assert_eq!(block.shard_count(), 2);
    assert_eq!(block.shard_len(), 8);
    assert_eq!(block.shard(1).map(<[u8]>::len), Some(8));
    assert!(block.shard(2).is_none());
    assert!(block.shard_mut(2).is_none());

    // Indices arrive off the wire, so an out-of-range one is ignored, not fatal.
    block.set_present(9, true);
    assert_eq!(block.present_count(), 0);

    block.set_present(1, true);
    assert!(block.is_present(1));
    assert_eq!(block.present_count(), 1);

    // Resizing clears the presence map and the bytes with it.
    block.shard_mut(1).unwrap().fill(0xab);
    block.reset(2, 8);
    assert_eq!(block.present_count(), 0);
    assert!(block.shard(1).unwrap().iter().all(|b| *b == 0));
}

#[test]
fn the_codec_cache_keeps_shapes_and_evicts_the_least_recently_used() {
    let mut codec = FecCodec::new();

    for shape in 0..4 {
        codec.prepare(8 + shape, 2).unwrap();
    }
    assert_eq!(codec.prepared_shapes(), 4);

    // Re-preparing a known shape neither grows the cache nor rebuilds anything.
    let rebuilt = count_allocations(|| codec.prepare(8, 2).unwrap());
    assert_eq!(codec.prepared_shapes(), 4);
    assert_eq!(rebuilt, 0, "a cached shape must not rebuild its codec");

    // A fifth shape evicts one, and the cache never grows past its capacity.
    codec.prepare(99, 5).unwrap();
    assert_eq!(codec.prepared_shapes(), 4);

    assert_eq!(codec.prepare(0, 1), Err(FecError::EmptyBlock));
    assert_eq!(codec.prepare(4, 0), Err(FecError::NoParity));
    assert_eq!(
        codec.prepare(MAX_DATA_SHARDS, 2),
        Err(FecError::BlockTooLarge {
            data_shards: MAX_DATA_SHARDS,
            parity_shards: 2,
        })
    );
}

#[test]
fn a_warm_encode_does_not_allocate() {
    // Without this, a counter that silently observed nothing would make the assertion at
    // the end of this test pass for the wrong reason.
    assert!(
        count_allocations(|| {
            std::hint::black_box(Vec::<u8>::with_capacity(4096));
        }) > 0,
        "the allocation counter is not observing allocations"
    );

    let mut codec = FecCodec::new();
    let mut parity = ParityBlock::new();
    let shard_len = MAX_VIDEO_PAYLOAD;
    // Not a multiple of the stride, so the short-tail path is exercised too.
    let block = bitstream(64 * shard_len - 23);
    let parity_shards = parity_shards_for(64, 0.2);

    // Warm up: this is where the codec, the parity buffer, and the tail scratch are built.
    for _ in 0..2 {
        codec
            .encode(&block, shard_len, parity_shards, &mut parity)
            .unwrap();
    }

    let allocations = count_allocations(|| {
        for _ in 0..16 {
            codec
                .encode(&block, shard_len, parity_shards, &mut parity)
                .unwrap();
        }
    });

    assert_eq!(
        allocations, 0,
        "the frame path must not allocate once its buffers are sized"
    );
}

#[test]
fn a_smaller_block_reuses_the_buffers_a_larger_one_grew() {
    let mut codec = FecCodec::new();
    let mut parity = ParityBlock::new();
    let shard_len = MAX_VIDEO_PAYLOAD;

    let large = bitstream(64 * shard_len - 23);
    let small = bitstream(9 * shard_len - 5);

    codec.encode(&large, shard_len, 13, &mut parity).unwrap();
    codec.encode(&small, shard_len, 2, &mut parity).unwrap();
    codec.encode(&small, shard_len, 2, &mut parity).unwrap();

    let allocations = count_allocations(|| {
        codec.encode(&small, shard_len, 2, &mut parity).unwrap();
    });

    assert_eq!(allocations, 0, "shrinking a block must not reallocate");
    assert_eq!(parity.shard_count(), 2);
    assert_eq!(parity.shard_len(), shard_len);

    // And the smaller block still round-trips after the buffers were reused.
    let mut matrix = data_matrix(&small, shard_len);
    let mut present = vec![true; small.len().div_ceil(shard_len)];
    present[3] = false;
    matrix[3 * shard_len..4 * shard_len].fill(0);

    assert_eq!(
        codec
            .reconstruct(&mut matrix, &mut present, &mut parity, shard_len)
            .unwrap(),
        1
    );
    assert_eq!(&matrix[..small.len()], &small[..]);
}

#[test]
fn parity_shards_are_a_function_of_the_data_only() {
    // Encoding the same block twice must produce byte-identical parity, whatever the
    // codec has done in between: the receiver has no way to tell which encode a parity
    // packet came from.
    let mut codec = FecCodec::new();
    let block = bitstream(11 * 64 + 3);

    let mut first = ParityBlock::new();
    codec.encode(&block, 64, 3, &mut first).unwrap();

    codec
        .encode(&bitstream(1000), 64, 5, &mut ParityBlock::new())
        .unwrap();
    for shape in 0..5 {
        codec.prepare(20 + shape, 4).unwrap();
    }

    let mut second = ParityBlock::new();
    codec.encode(&block, 64, 3, &mut second).unwrap();

    assert_eq!(first, second);
}

#[test]
fn recovery_ignores_parity_shards_that_never_arrived() {
    let mut codec = FecCodec::new();
    let block = Block::encode(&mut codec, 8 * 100, 100, 4);

    // A lost parity shard costs exactly as much of the budget as a lost data shard. Two
    // data and two parity shards gone leaves eight of twelve, which is precisely the
    // eight the block needs, so it still rebuilds.
    let (mut matrix, mut present, mut parity) = block.receive(&[1, 2, 8, 9]);
    assert_eq!(
        codec
            .reconstruct(&mut matrix, &mut present, &mut parity, block.shard_len)
            .expect("eight of twelve shards is exactly enough"),
        2
    );
    assert_eq!(matrix, block.matrix);

    // One more gone anywhere in the block, data or parity, and it is not.
    let (mut matrix, mut present, mut parity) = block.receive(&[1, 2, 3, 8, 9]);
    let before = matrix.clone();
    let err = codec
        .reconstruct(&mut matrix, &mut present, &mut parity, block.shard_len)
        .expect_err("seven of twelve shards cannot rebuild eight data shards");
    assert_eq!(
        err,
        FecError::Unrecoverable {
            missing: 3,
            present: 7,
            needed: 8,
        }
    );
    assert_eq!(matrix, before);
}
