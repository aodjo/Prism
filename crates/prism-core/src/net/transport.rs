//! UDP transport.
//!
//! One socket carries every channel. Datagram boundaries match packet boundaries, so
//! there is no framing to do and a lost packet costs exactly one packet rather than
//! desynchronising a stream.
//!
//! Sending and receiving both work against caller-owned buffers, so a running session
//! does not allocate.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

/// A bound UDP socket carrying one session.
#[derive(Debug)]
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Binds a socket to the given local address.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the address cannot be bound.
    ///
    /// # Examples
    ///
    /// ```
    /// # use prism_core::net::transport::UdpTransport;
    /// let transport = UdpTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    /// assert!(transport.local_addr().unwrap().port() != 0);
    /// ```
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(addr)?,
        })
    }

    /// Fixes the peer this socket sends to and accepts from.
    ///
    /// Connecting a UDP socket lets the kernel drop datagrams from anywhere else before
    /// they reach the receive loop, which removes a whole class of off-path nuisance
    /// traffic without any work in user space.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the peer cannot be set.
    pub fn connect(&self, peer: SocketAddr) -> io::Result<()> {
        self.socket.connect(peer)
    }

    /// Returns the address the socket is bound to.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Sets how long [`Self::recv_into`] waits before giving up.
    ///
    /// `None` blocks indefinitely. A timeout is what lets the receive loop notice that a
    /// sender has stopped instead of hanging forever.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the timeout cannot be set.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    /// Sends one packet to the connected peer.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`], notably if no peer has been connected.
    pub fn send(&self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.send(bytes)
    }

    /// Receives one packet into `buf` and returns the bytes that were written.
    ///
    /// A datagram longer than `buf` is truncated, so `buf` must be at least
    /// [`MAX_PACKET_SIZE`](crate::net::packet::MAX_PACKET_SIZE) bytes for a well-formed
    /// peer's packets to survive intact.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] or [`io::ErrorKind::TimedOut`] when the read
    /// timeout expires, and the underlying [`io::Error`] otherwise.
    pub fn recv_into<'a>(&self, buf: &'a mut [u8]) -> io::Result<&'a [u8]> {
        let len = self.socket.recv(buf)?;
        Ok(&buf[..len])
    }
}
