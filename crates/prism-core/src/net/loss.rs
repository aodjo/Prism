//! Deliberately dropping packets, to prove that recovery works.
//!
//! M4 is judged on a number — zero keyframe hitches at five percent packet loss — and a
//! number needs a way to produce the loss. The alternative is the operating system's own
//! traffic shaper (dummynet, clumsy, netem), which exercises the real socket path but needs
//! administrator rights, differs on all three platforms, and cannot run in CI. This runs
//! anywhere, is seeded so a failure reproduces exactly, and is the difference between a
//! regression test and a thing someone remembers to try by hand.
//!
//! What it does not test is the kernel path: real loss arrives with reordering, bursts and
//! queue delay that dropping at the application layer does not reproduce. Both are worth
//! having. This one is the one that can run on every commit.

/// Decides which packets to drop, reproducibly.
///
/// The generator is a plain xorshift rather than a dependency. Nothing here needs
/// cryptographic quality — it needs to be the same sequence on every machine and every run
/// for a given seed, which a hand-written generator guarantees and a crate's internal
/// changes do not.
#[derive(Debug, Clone)]
pub struct LossInjector {
    state: u64,
    threshold: u32,
    considered: u64,
    dropped: u64,
}

impl LossInjector {
    /// Creates an injector that drops roughly `per_million` packets in every million.
    ///
    /// Parts per million rather than a float so the configuration is exact and the decision
    /// is integer comparison. Five percent, the figure M4 is judged at, is `50_000`.
    ///
    /// A seed of zero is replaced, because an all-zero xorshift state produces nothing but
    /// zeroes and would drop every packet or none forever.
    #[must_use]
    pub fn new(per_million: u32, seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
            threshold: per_million.min(1_000_000),
            considered: 0,
            dropped: 0,
        }
    }

    /// Returns whether this packet should be thrown away.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::loss::LossInjector;
    /// let mut injector = LossInjector::new(0, 1);
    /// assert!(!injector.should_drop(), "a rate of zero never drops");
    /// ```
    pub fn should_drop(&mut self) -> bool {
        self.considered += 1;

        if self.threshold == 0 {
            return false;
        }

        let drop = self.next() % 1_000_000 < u64::from(self.threshold);
        if drop {
            self.dropped += 1;
        }

        drop
    }

    /// Returns how many packets were considered and how many were dropped.
    ///
    /// Reported rather than assumed: the achieved rate on a short run differs from the
    /// configured one, and a measurement that quietly used a different loss rate than it
    /// claimed would make the whole M4 verdict meaningless.
    #[must_use]
    pub fn tally(&self) -> (u64, u64) {
        (self.considered, self.dropped)
    }

    /// Returns the loss actually applied, in parts per million.
    #[must_use]
    pub fn achieved_per_million(&self) -> u32 {
        if self.considered == 0 {
            return 0;
        }

        ((self.dropped * 1_000_000) / self.considered) as u32
    }

    /// Advances the xorshift64 state and returns it.
    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }
}
