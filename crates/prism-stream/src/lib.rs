//! The window another machine is shown in, and the words the shell and that window exchange.
//!
//! Two things live here and they are deliberately separable. [`ipc`] is what the shell needs
//! and nothing more — the message it sends to start a stream and the messages that come back —
//! and it compiles anywhere, with no window and no SDL. [`display`] is the window itself, which
//! exists only where there is a client.
//!
//! # Why the stream is still a process of its own
//!
//! A window has to be driven from the process's main thread on macOS, and the shell's own
//! event loop already owns that thread. But the arrangement earns itself beyond that: the
//! decode and present path runs where the interface cannot stall it, in a process that can
//! crash without taking the window with it, and the rule that a frame never reaches the shell
//! holds by construction rather than by discipline.
//!
//! What that is *not* is a command line. The shell does not build an argument list or read
//! sentences back — it sends one [`ipc::Start`] and receives [`ipc::Event`]s, which is an
//! interface that can be changed without both sides having to agree on a wording.

#[cfg(all(feature = "window", any(target_os = "macos", target_os = "windows")))]
mod audio;
#[cfg(all(feature = "window", any(target_os = "macos", target_os = "windows")))]
pub mod display;
#[cfg(target_os = "macos")]
pub mod drawer;
pub mod ipc;
