//! Prism data plane.
//!
//! Everything on the latency-critical path lives here: capture, encode, packetise,
//! send, receive, decode, present, and input. None of it is reachable from JavaScript.
//! The Node-API surface in `prism-napi` exposes only control calls and low-rate stats,
//! because a video frame that crosses into V8 has already cost more than the entire
//! latency budget.
//!
//! See `docs/wire-format.md` for the protocol this crate implements.

pub mod capture;
pub mod clock;
pub mod decode;
pub mod encode;
pub mod input;
pub mod net;
pub mod render;
pub mod stats;
