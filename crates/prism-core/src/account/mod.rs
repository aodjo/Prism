//! Accounts: signing in, and carrying a key between machines.
//!
//! Everything here is control plane and none of it is on the frame path. It exists so that a
//! person with two machines does not have to pair them by reading six digits off one screen
//! and typing them into another, and so that the machines they have paired follow them.
//!
//! # What the server is trusted with, and what it is not
//!
//! The rendezvous server holds accounts. It learns who is signing in, which machines belong to
//! them, and whether they may use the relay. It does not learn their private key.
//!
//! That distinction is the whole design, and it is [`secret`] that keeps it: one password
//! becomes two independent secrets, and only one of them is ever sent. A server that has been
//! taken over completely can lock somebody out, but it cannot watch their screen.
//!
//! The cost of that is real and worth stating: the key is only as safe as the password, and a
//! password that is guessed offline against a stolen database opens it. That is what the
//! memory-hard hash in [`secret`] is priced against.

pub mod secret;
pub mod totp;
pub mod vault;
