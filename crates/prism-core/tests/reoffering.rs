//! A host that has served one session can serve the next one.
//!
//! Sharing is a state rather than an attempt: a machine that has been shared stays available
//! after a client comes and goes. That only works if each session lets go of everything it took
//! — and what it takes that matters most is the port, because a host binds one of its own and
//! the next turn of the loop binds it again.
//!
//! This broke and was not caught, because nothing exercised the second turn. The return path
//! ran on a thread that outlived its session holding a duplicate of its socket, so the port
//! stayed taken, every later bind failed with `AddrInUse`, and a machine answered exactly one
//! session per launch. From the outside that is a device that is listed, is registered, and
//! does not answer the handshake.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use prism_core::net::handshake::Identity;
use prism_core::net::negotiate::{H264, HostAbility, Offer};
use prism_core::net::sender::SliceSender;
use prism_core::net::transport::UdpTransport;

/// How long either side waits for the other before giving up.
const PATIENCE: Duration = Duration::from_secs(5);

/// What the host says it can produce.
fn ability() -> HostAbility {
    HostAbility {
        codecs: H264,
        width: 640,
        height: 360,
        fps: 30,
        bitrate_bps: 1_000_000,
        audio: false,
    }
}

/// What the client says it can show.
fn offer() -> Offer {
    Offer {
        codecs: H264,
        max_width: u16::MAX,
        max_height: u16::MAX,
        max_fps: u16::MAX,
        audio: false,
    }
}

/// Finds a port nothing is using, by taking one and giving it straight back.
fn spare_port() -> u16 {
    let taken = UdpSocket::bind("127.0.0.1:0").expect("a port is available");
    let port = taken.local_addr().expect("it has an address").port();

    drop(taken);

    port
}

/// Runs one session on `at`, from handshake to hanging up.
///
/// Returns once both ends have finished with it, which is the moment the next turn of a real
/// host's loop would come round and bind the same port again.
fn one_session(at: SocketAddr, host: &Identity, client: &Identity) {
    let dialling = {
        let client = client.clone();
        let host_key = *host.public();

        std::thread::spawn(move || {
            let transport =
                UdpTransport::bind("127.0.0.1:0".parse().expect("a valid bind address"))
                    .expect("the client binds");

            let established = prism_core::control::session::dial(
                &transport,
                at,
                &client,
                &host_key,
                offer(),
                PATIENCE,
            )
            .expect("the client opens the session");

            // Held only long enough for the host to finish its own half. What this test is
            // about is what the host does afterwards.
            drop(established);
            drop(transport);
        })
    };

    let transport = UdpTransport::bind(at).expect("the host binds");
    let mut sender = SliceSender::serve_on(
        transport,
        host,
        vec![*client.public()],
        ability(),
        PATIENCE,
        &AtomicBool::new(false),
    )
    .expect("the host opens the session");

    sender
        .serve_return_path(false, None)
        .expect("the return path starts");

    dialling.join().expect("the client thread does not panic");

    // The session is over. Everything it took has to go with it.
    drop(sender);
}

#[test]
fn a_second_session_can_bind_the_port_the_first_one_used() {
    let port = spare_port();
    let at: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("a valid address");

    let host = Identity::generate().expect("a key pair");
    let client = Identity::generate().expect("a key pair");

    one_session(at, &host, &client);

    // The return path is a thread, and a thread told to stop is not a thread that has stopped.
    // A real host takes far longer than this to come round to binding again.
    std::thread::sleep(Duration::from_millis(900));

    assert!(
        UdpSocket::bind(at).is_ok(),
        "the port the last session used is still held, so the machine can never be shared again"
    );
}
