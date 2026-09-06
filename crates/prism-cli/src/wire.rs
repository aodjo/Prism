//! Sending encoded slices over the wire.
//!
//! Shared by both host modes so the synthetic source and the real encoder put identical
//! packets on the network — the only difference between them is where the bytes came
//! from.

use std::io;

use prism_core::clock::now_us;
use prism_core::net::packet::{
    CLOCK_PONG_LEN, Channel, ClockPing, ClockPong, FLAG_IDR, FLAG_LAST_OF_FRAME, MAX_PACKET_SIZE,
    channel_of,
};
use prism_core::net::packetize::SlicePacketizer;
use prism_core::net::transport::UdpTransport;

/// Owns the socket and the reusable send buffer for one session.
#[derive(Debug)]
pub struct SliceSender {
    transport: UdpTransport,
    buffer: [u8; MAX_PACKET_SIZE],
    packets: u64,
    bytes: u64,
}

impl SliceSender {
    /// Binds an ephemeral local port and connects it to `peer`.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the socket cannot be bound or connected.
    pub fn connect(peer: std::net::SocketAddr) -> io::Result<Self> {
        let transport = UdpTransport::bind("0.0.0.0:0".parse().expect("valid bind address"))?;
        transport.connect(peer)?;

        Ok(Self {
            transport,
            buffer: [0; MAX_PACKET_SIZE],
            packets: 0,
            bytes: 0,
        })
    }

    /// Cuts one slice into packets and sends them.
    ///
    /// `last` marks the slice that ends the frame, which is how the receiver knows the
    /// frame is complete rather than still arriving.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if a packet cannot be sent.
    ///
    /// # Panics
    ///
    /// Panics if `data` is empty or larger than a slice can be, neither of which a real
    /// encoder produces.
    pub fn send_slice(
        &mut self,
        frame_id: u32,
        slice_id: u16,
        data: &[u8],
        capture_ts_us: u64,
        idr: bool,
        last: bool,
    ) -> io::Result<()> {
        let mut flags = 0;
        if idr {
            flags |= FLAG_IDR;
        }
        if last {
            flags |= FLAG_LAST_OF_FRAME;
        }

        let packetizer = SlicePacketizer::new(frame_id, slice_id, flags, capture_ts_us, data)
            .expect("an encoded slice is always packetisable");

        for packet in packetizer {
            let len = packet
                .encode_into(&mut self.buffer)
                .expect("packet fits the send buffer");
            self.transport.send(&self.buffer[..len])?;
            self.packets += 1;
            self.bytes += len as u64;
        }

        Ok(())
    }

    /// Returns how many packets have been sent.
    #[must_use]
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Returns how many bytes have been sent, including packet headers.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Starts a thread that answers clock synchronisation pings on this session's socket.
    ///
    /// The reply has to come from the socket the video is already flowing out of, so the
    /// client can pair it with the session; a second socket would answer from a different
    /// port. The thread runs until the process exits, which is fine for a tool whose
    /// sessions last exactly as long as the process.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the socket cannot be duplicated.
    pub fn answer_clock_pings(&self) -> io::Result<()> {
        let transport = self.transport.try_clone()?;

        std::thread::spawn(move || {
            let mut recv_buf = [0u8; MAX_PACKET_SIZE];
            let mut send_buf = [0u8; CLOCK_PONG_LEN];

            loop {
                let Ok(bytes) = transport.recv_into(&mut recv_buf) else {
                    return;
                };

                let t2_us = now_us();
                if channel_of(bytes) != Ok(Channel::Control) {
                    continue;
                }
                let Ok(ping) = ClockPing::decode(bytes) else {
                    continue;
                };

                // The socket is connected to the client, so the answer goes back with
                // `send`. `send_to` fails outright here — a connected UDP socket rejects
                // it with EISCONN on macOS and the BSDs.
                let pong = ClockPong {
                    t1_us: ping.t1_us,
                    t2_us,
                    t3_us: now_us(),
                };
                if pong.encode_into(&mut send_buf).is_ok() {
                    let _ = transport.send(&send_buf);
                }
            }
        });

        Ok(())
    }
}
