//! The Prism rendezvous server, as a library.
//!
//! The binary is a socket and a receive loop around this. Splitting them apart is what lets
//! the tables be tested for the things that actually matter — that a registration cannot be
//! taken over, and that neither table grows without bound — without a socket in the way.
//!
//! See the binary's own documentation for what the server is trusted with, which is nothing.

pub mod accounts;
pub mod api;
pub mod mail;
pub mod registry;
pub mod relay;
pub mod report;
pub mod sessions;
