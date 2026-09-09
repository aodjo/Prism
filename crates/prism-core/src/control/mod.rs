//! Control plane: everything that happens before and around a session, not inside one.
//!
//! Pairing, finding a peer, and opening a session are all exchanges of a handful of datagrams
//! at human speed. None of them is on the frame path, and none of them may ever be: a
//! millisecond here is invisible, while a millisecond in `net` is a frame.
//!
//! They live in the core rather than in whatever drives it because both drivers need them —
//! the headless command line and the Node-API surface — and because only the core can hold a
//! private key. The Node-API surface exposes these and nothing else that touches the network.
//!
//! Nothing here prints. A library that writes to standard output cannot be embedded in a
//! window, so what a caller would want to show is returned instead.

pub mod client;
pub mod host;
/// What the system has to allow before a host can capture or control this machine.
pub mod permissions;
pub mod rendezvous;
pub mod session;
