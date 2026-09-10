//! What the codec tests share.
//!
//! A directory rather than a file beside them, because `tests/*.rs` each become a test binary
//! of their own and `tests/common/mod.rs` does not — it is the one place two of them can agree
//! on something without a second copy of it.

/// How long a test waits for a session to hand back what it was given.
///
/// Generous, because this is a test's patience and not a latency budget: what is being checked
/// is that a frame survives the round trip, never that it survives it quickly. The pipeline's
/// real deadlines are measured against a running session, not here.
///
/// Ten seconds rather than the five it was. These tests run in parallel, and on a machine with
/// no media engine — GitHub's Intel macOS runner is a virtual machine without one — several
/// concurrent software encodes starve each other badly enough that five is not enough.
///
/// It bounds what a *missing* frame costs; it does not promise one arrives. Raising it alone
/// fixed nothing: at thirty seconds the round trip still failed, four minutes later, because a
/// `RealTime` session is allowed to drop a frame rather than fall behind and this had been
/// written as though it never would.
#[cfg(target_os = "macos")]
pub const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);
