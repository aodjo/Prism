//! The socket, with every datagram sealed on the way out and opened on the way in.
//!
//! This exists so that no part of the pipeline above it has to remember to encrypt. A
//! packetiser that builds a video packet hands it here and it leaves the machine sealed;
//! a receive loop asks here for a packet and what it gets back has already been proved to
//! come from the peer. There is deliberately no way to send in the clear.
//!
//! # Why receiving skips rather than fails
//!
//! Anyone who can find the port can send a datagram to it. If a bad packet were an error the
//! caller had to handle, the natural way to write the receive loop would end the session on
//! the first one — a session anybody could kill with a single forged packet from anywhere on
//! the internet. So [`SecureReceiver::recv_into`] drops what does not open and keeps waiting,
//! and the count is available for the statistics rather than as a failure.

use std::io;
use std::net::SocketAddr;

use crate::net::packet::MAX_PACKET_SIZE;
use crate::net::seal::{COUNTER_LEN, Opener, Sealer};
use crate::net::transport::UdpTransport;

/// How many unopenable packets one receive call will skip before returning to the caller.
///
/// Without a ceiling, a flood of junk would keep the call inside its own loop indefinitely
/// and the caller would never get its timeout back. Sixty-four is far more than a healthy
/// path ever produces and small enough that the caller stays in control under a flood.
const REJECT_BUDGET: u32 = 64;

/// The sending half of a sealed session.
///
/// Owns its send buffer, so a running session does not allocate.
pub struct SecureSender {
    transport: UdpTransport,
    sealer: Sealer,
    buffer: Box<[u8; MAX_PACKET_SIZE]>,
}

impl SecureSender {
    /// Wraps a transport with the key for the outgoing direction.
    #[must_use]
    pub fn new(transport: UdpTransport, sealer: Sealer) -> Self {
        Self {
            transport,
            sealer,
            buffer: Box::new([0; MAX_PACKET_SIZE]),
        }
    }

    /// Returns a second sender onto the same socket and the same direction.
    ///
    /// The host sends video from the encoder thread and clock synchronisation replies from
    /// the return path thread. Both are the same direction under the same key, so they share
    /// one nonce counter — see [`Sealer::split`] for why that sharing is not optional.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the socket cannot be duplicated.
    pub fn split(&self) -> io::Result<Self> {
        Ok(Self {
            transport: self.transport.try_clone()?,
            sealer: self.sealer.split(),
            buffer: Box::new([0; MAX_PACKET_SIZE]),
        })
    }

    /// Builds the receiving half on a duplicate of this socket.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the socket cannot be duplicated.
    pub fn receiver(&self, opener: Opener) -> io::Result<SecureReceiver> {
        Ok(SecureReceiver::new(self.transport.try_clone()?, opener))
    }

    /// Seals `plaintext` and sends it to the connected peer.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if the plaintext exceeds
    /// [`MAX_PLAINTEXT_SIZE`](crate::net::packet::MAX_PLAINTEXT_SIZE), and the underlying
    /// [`io::Error`] if the packet cannot be sent.
    pub fn send(&mut self, plaintext: &[u8]) -> io::Result<usize> {
        let len = self.seal(plaintext)?;
        self.transport.send(&self.buffer[..len])
    }

    /// Seals `plaintext` and sends it to a specific address.
    ///
    /// # Errors
    ///
    /// As [`Self::send`].
    pub fn send_to(&mut self, plaintext: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let len = self.seal(plaintext)?;
        self.transport.send_to(&self.buffer[..len], addr)
    }

    /// Returns how many packets this direction has sealed.
    #[must_use]
    pub fn sealed(&self) -> u64 {
        self.sealer.sent()
    }

    /// Returns the socket underneath, for addresses and timeouts.
    ///
    /// Deliberately not a way to send: the transport's own send would put a packet on the
    /// wire in the clear.
    #[must_use]
    pub fn transport(&self) -> &UdpTransport {
        &self.transport
    }

    /// Seals into the owned buffer and returns the sealed length.
    fn seal(&mut self, plaintext: &[u8]) -> io::Result<usize> {
        self.sealer
            .seal(plaintext, self.buffer.as_mut_slice())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
    }
}

impl core::fmt::Debug for SecureSender {
    /// Describes the sender without exposing its key.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SecureSender")
            .field("sealed", &self.sealer.sent())
            .finish_non_exhaustive()
    }
}

/// The receiving half of a sealed session.
pub struct SecureReceiver {
    transport: UdpTransport,
    opener: Opener,
}

impl SecureReceiver {
    /// Wraps a transport with the key for the incoming direction.
    #[must_use]
    pub fn new(transport: UdpTransport, opener: Opener) -> Self {
        Self { transport, opener }
    }

    /// Receives the next packet that opens, skipping any that do not.
    ///
    /// `buf` must be at least [`MAX_PACKET_SIZE`] bytes. The returned slice borrows the part
    /// of `buf` the plaintext was decrypted into.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] or [`io::ErrorKind::TimedOut`] when the read
    /// timeout expires or the reject budget for one call is spent, and the underlying
    /// [`io::Error`] otherwise. A packet that fails to open is never an error.
    pub fn recv_into<'a>(&mut self, buf: &'a mut [u8]) -> io::Result<&'a [u8]> {
        // The borrow checker cannot see that a rejected packet ends the borrow, so the loop
        // works in lengths and the slice is taken once, after the decision.
        let mut budget = REJECT_BUDGET;

        loop {
            let len = self.transport.recv_into(buf)?.len();

            match self.opener.open(&mut buf[..len]) {
                Ok(plaintext) => {
                    let end = COUNTER_LEN + plaintext.len();
                    return Ok(&buf[COUNTER_LEN..end]);
                }
                Err(_) => {
                    budget -= 1;
                    if budget == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "too many unopenable packets in one receive",
                        ));
                    }
                }
            }
        }
    }

    /// Receives the next packet that opens, also reporting where it came from.
    ///
    /// # Errors
    ///
    /// As [`Self::recv_into`].
    pub fn recv_from_into<'a>(&mut self, buf: &'a mut [u8]) -> io::Result<(&'a [u8], SocketAddr)> {
        let mut budget = REJECT_BUDGET;

        loop {
            let (len, from) = {
                let (bytes, from) = self.transport.recv_from_into(buf)?;
                (bytes.len(), from)
            };

            match self.opener.open(&mut buf[..len]) {
                Ok(plaintext) => {
                    let end = COUNTER_LEN + plaintext.len();
                    return Ok((&buf[COUNTER_LEN..end], from));
                }
                Err(_) => {
                    budget -= 1;
                    if budget == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "too many unopenable packets in one receive",
                        ));
                    }
                }
            }
        }
    }

    /// Returns how many packets failed authentication.
    ///
    /// Zero on a healthy path. Anything else is corruption or someone writing packets at this
    /// port, and it belongs in the statistics rather than in a log line per packet.
    #[must_use]
    pub fn forged(&self) -> u64 {
        self.opener.forged()
    }

    /// Returns how many authentic packets were refused as replays.
    #[must_use]
    pub fn replayed(&self) -> u64 {
        self.opener.replayed()
    }

    /// Returns the socket underneath, for addresses and timeouts.
    #[must_use]
    pub fn transport(&self) -> &UdpTransport {
        &self.transport
    }
}

impl core::fmt::Debug for SecureReceiver {
    /// Describes the receiver without exposing its key.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SecureReceiver")
            .field("forged", &self.opener.forged())
            .field("replayed", &self.opener.replayed())
            .finish_non_exhaustive()
    }
}
