//! A client hears the host leave, rather than waiting to notice it has gone.
//!
//! A host that stopped sharing, or quit, used to simply stop sending. The window watching it
//! could not tell that from a slow network, so it went on showing the last picture for its whole
//! idle timeout with nothing on screen to say the machine had gone.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prism_core::control::client::{self, ClientConfig, ClientHooks, Departure, Report, Reporter};
use prism_core::net::handshake::Identity;
use prism_core::net::negotiate::{H264, HostAbility, Offer};
use prism_core::net::sender::SliceSender;
use prism_core::net::transport::UdpTransport;

/// How long either side waits for the other to open the session.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long the client waits without a packet before deciding the host has gone.
///
/// Far longer than the test gives it, so a client that only ends here fails.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How soon after the host goes the client has to have ended.
const PROMPTLY: Duration = Duration::from_secs(3);

#[test]
fn a_client_ends_as_soon_as_the_host_says_goodbye() {
    let host = Identity::generate().expect("a key pair");
    let viewer = Identity::generate().expect("a key pair");

    let transport = UdpTransport::bind("127.0.0.1:0".parse().expect("a valid bind address"))
        .expect("the host binds");
    let at = transport.local_addr().expect("the host has an address");

    let heard: Arc<Mutex<Vec<Report>>> = Arc::new(Mutex::new(Vec::new()));

    let watching = {
        let heard = Arc::clone(&heard);
        let config = ClientConfig {
            host: Some(at),
            rendezvous: None,
            offer: Offer {
                codecs: H264,
                max_width: u16::MAX,
                max_height: u16::MAX,
                max_fps: u16::MAX,
                audio: false,
            },
            force_relay: false,
            frames: None,
            idle_timeout: IDLE_TIMEOUT,
            report_every: 0,
            in_flight: 4,
            decode: false,
            identity: viewer.clone(),
            peer_key: *host.public(),
        };
        let hooks = ClientHooks {
            report: Some(Reporter::new(move |report| {
                if let Ok(mut heard) = heard.lock() {
                    heard.push(report);
                }
            })),
            ..ClientHooks::default()
        };

        std::thread::spawn(move || client::run(config, hooks))
    };

    let sender = SliceSender::serve_on(
        transport,
        &host,
        vec![*viewer.public()],
        HostAbility {
            codecs: H264,
            width: 640,
            height: 360,
            fps: 30,
            bitrate_bps: 1_000_000,
            audio: false,
        },
        PATIENCE,
        &AtomicBool::new(false),
    )
    .expect("the host opens the session");

    // Long enough for the client to be in its receive loop, which is where a real one is when
    // somebody stops sharing.
    std::thread::sleep(Duration::from_millis(300));

    let left = Instant::now();
    drop(sender);

    let ended = watching.join().expect("the client thread does not panic");
    let took = left.elapsed();

    assert!(ended.is_ok(), "the client failed: {ended:?}");
    assert!(
        took < PROMPTLY,
        "the client took {took:?} to notice the host had gone"
    );

    let heard = heard.lock().expect("nothing panicked holding it");
    assert!(
        heard
            .iter()
            .any(|report| matches!(report, Report::Gone(Departure::Left))),
        "the client never said the host left"
    );
}
