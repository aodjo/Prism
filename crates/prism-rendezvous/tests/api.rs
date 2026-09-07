//! Tests for the account API.
//!
//! Driven through the router rather than through a socket, so what is checked is what a
//! request actually does and not whether a port could be bound. Every one of these is about a
//! way the API could be wrong that nothing downstream would notice: an endpoint that answers
//! without a token, a failure that says more than it should, a name that can be discovered by
//! how the server refuses it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use prism_core::account::secret::{SALT_LEN, SECRET_LEN};
use prism_core::account::totp;
use prism_core::net::handshake::KEY_LEN;
use prism_rendezvous::accounts::Accounts;
use prism_rendezvous::api::{Service, routes};
use prism_rendezvous::sessions::Sessions;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A router over a store in a file that goes away with the test.
fn service(label: &str) -> (axum::Router, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "prism-api-{label}-{}-{:?}.json",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);

    let accounts = Accounts::open(&path).expect("opens");

    // Beside the accounts, the way the server puts it, and cleared first so that a session
    // left by an earlier run of the suite cannot be mistaken for one this test issued.
    let sessions_path = path.with_file_name(format!("sessions-{label}.json"));
    let _ = std::fs::remove_file(&sessions_path);

    let accounts_path = path.clone();
    let sessions = Sessions::open(sessions_path, 0);

    (
        routes(Service::new(
            accounts,
            sessions,
            "rv.example.com:47300".to_owned(),
        )),
        accounts_path,
    )
}

/// Sends one request and returns the status and the parsed body.
async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("the router answers");

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");

    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };

    (status, value)
}

/// Builds a JSON request.
fn post(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request")
}

/// Builds a JSON request carrying a token.
fn authed(method: &str, path: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("a request")
}

/// Renders bytes as hex, the way every field on this API carries them.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Registers an account and returns its TOTP secret.
async fn register(router: &axum::Router, name: &str) -> Vec<u8> {
    let (status, body) = send(
        router,
        post(
            "/v1/accounts",
            json!({
                "email": name,
                "salt": hex(&[1u8; SALT_LEN]),
                "auth": hex(&[2u8; SECRET_LEN]),
                "sealed_key": hex(&[3u8; 60]),
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "registration failed: {body}");

    let base32 = body["totp_secret"].as_str().expect("a secret").to_owned();
    from_base32(&base32)
}

/// Reads back what the API rendered, so a test uses the same secret an authenticator would.
fn from_base32(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

    let mut out = Vec::new();
    let mut buffer = 0u16;
    let mut bits = 0u32;

    for character in text.bytes() {
        let value = ALPHABET
            .iter()
            .position(|&c| c == character)
            .expect("base32 character") as u16;

        buffer = (buffer << 5) | value;
        bits += 5;

        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    out
}

/// Signs in and returns the token.
async fn sign_in(router: &axum::Router, name: &str, secret: &[u8]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock")
        .as_secs();

    let (status, body) = send(
        router,
        post(
            "/v1/sessions",
            json!({
                "email": name,
                "auth": hex(&[2u8; SECRET_LEN]),
                "code": totp::code_at_time(secret, now),
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "sign-in failed: {body}");

    body["token"].as_str().expect("a token").to_owned()
}

#[tokio::test]
async fn an_account_can_be_made_and_signed_in_to() {
    let (router, path) = service("roundtrip");
    let secret = register(&router, "someone@example.com").await;
    let token = sign_in(&router, "someone@example.com", &secret).await;

    assert!(!token.is_empty());
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn registering_hands_back_something_an_authenticator_can_read() {
    let (router, path) = service("provision");
    let (_, body) = send(
        &router,
        post(
            "/v1/accounts",
            json!({
                "email": "someone@example.com",
                "salt": hex(&[1u8; SALT_LEN]),
                "auth": hex(&[2u8; SECRET_LEN]),
                "sealed_key": hex(&[3u8; 60]),
            }),
        ),
    )
    .await;

    let uri = body["totp_uri"].as_str().expect("a uri");
    assert!(
        uri.starts_with("otpauth://totp/Prism:someone@example.com?"),
        "{uri}"
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn the_sealed_key_comes_back_exactly_as_it_went_in() {
    // A byte lost here is a key lost, and the server cannot check it for itself.
    let (router, path) = service("sealed");
    let secret = register(&router, "someone@example.com").await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock")
        .as_secs();

    let (_, body) = send(
        &router,
        post(
            "/v1/sessions",
            json!({
                "email": "someone@example.com",
                "auth": hex(&[2u8; SECRET_LEN]),
                "code": totp::code_at_time(&secret, now),
            }),
        ),
    )
    .await;

    assert_eq!(body["sealed_key"].as_str().expect("a key"), hex(&[3u8; 60]));
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn a_wrong_code_is_refused() {
    let (router, path) = service("wrongcode");
    register(&router, "someone@example.com").await;

    let (status, _) = send(
        &router,
        post(
            "/v1/sessions",
            json!({
                "email": "someone@example.com",
                "auth": hex(&[2u8; SECRET_LEN]),
                "code": 0,
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn an_unknown_name_is_refused_the_same_way_a_wrong_password_is() {
    // Same status, same body. Anything else is an oracle for which names exist.
    let (router, path) = service("sameway");
    let secret = register(&router, "someone@example.com").await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock")
        .as_secs();
    let code = totp::code_at_time(&secret, now);

    let unknown = send(
        &router,
        post(
            "/v1/sessions",
            json!({ "email": "nobody@example.com", "auth": hex(&[2u8; SECRET_LEN]), "code": code }),
        ),
    )
    .await;

    let wrong = send(
        &router,
        post(
            "/v1/sessions",
            json!({ "email": "someone@example.com", "auth": hex(&[9u8; SECRET_LEN]), "code": code }),
        ),
    )
    .await;

    assert_eq!(unknown.0, wrong.0);
    assert_eq!(unknown.1, wrong.1);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn asking_for_a_salt_does_not_say_whether_the_account_exists() {
    // The reply has to look the same for a name that is there and one that is not, or this
    // endpoint is a way to enumerate accounts.
    let (router, path) = service("saltprobe");
    register(&router, "someone@example.com").await;

    let real = send(
        &router,
        Request::builder()
            .uri("/v1/salt?email=someone")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    let fake = send(
        &router,
        Request::builder()
            .uri("/v1/salt?email=nobody")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(real.0, StatusCode::OK);
    assert_eq!(fake.0, StatusCode::OK);
    assert_eq!(
        real.1["salt"].as_str().map(str::len),
        fake.1["salt"].as_str().map(str::len),
        "the decoy salt is a different length from a real one"
    );
    assert_ne!(real.1["salt"], fake.1["salt"]);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn the_same_name_cannot_be_registered_twice() {
    let (router, path) = service("twice");
    register(&router, "someone@example.com").await;

    let (status, _) = send(
        &router,
        post(
            "/v1/accounts",
            json!({
                "email": "someone@example.com",
                "salt": hex(&[1u8; SALT_LEN]),
                "auth": hex(&[2u8; SECRET_LEN]),
                "sealed_key": hex(&[3u8; 60]),
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn every_signed_in_endpoint_refuses_without_a_token() {
    // The mistake that matters most here, and the easiest one to make by adding a route and
    // forgetting the line that checks who is asking.
    let (router, path) = service("notoken");
    register(&router, "someone@example.com").await;

    let attempts = [
        Request::builder()
            .uri("/v1/session")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .uri("/v1/devices")
            .body(Body::empty())
            .unwrap(),
        post(
            "/v1/devices",
            json!({ "public_key": hex(&[7u8; KEY_LEN]), "label": "x" }),
        ),
        Request::builder()
            .method("DELETE")
            .uri(format!("/v1/devices/{}", hex(&[7u8; KEY_LEN])))
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("PUT")
            .uri("/v1/account/key")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "salt": hex(&[8u8; SALT_LEN]),
                    "auth": hex(&[9u8; SECRET_LEN]),
                    "sealed_key": hex(&[4u8; 60]),
                })
                .to_string(),
            ))
            .unwrap(),
    ];

    for request in attempts {
        let uri = request.uri().to_string();
        let method = request.method().to_string();
        let (status, _) = send(&router, request).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} answered without a token"
        );
    }
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn a_token_kept_from_a_previous_run_still_says_who_it_belongs_to() {
    // What lets an application start signed in rather than asking for a password every time it
    // is opened. It has to hand back the same picture signing in did, or the window would show
    // less after a restart than it did before one.
    let (router, path) = service("resume");
    let secret = register(&router, "someone@example.com").await;
    let token = sign_in(&router, "someone@example.com", &secret).await;

    let key = hex(&[5u8; KEY_LEN]);
    send(
        &router,
        authed(
            "POST",
            "/v1/devices",
            &token,
            json!({ "public_key": key, "label": "a laptop" }),
        ),
    )
    .await;

    let (status, body) = send(&router, authed("GET", "/v1/session", &token, Value::Null)).await;

    assert_eq!(status, StatusCode::OK);
    // Where to register comes back with the session, so that neither end of a stream has to be
    // told by hand where the signalling is.
    assert_eq!(body["rendezvous"], "rv.example.com:47300");
    assert_eq!(body["email"], "someone@example.com");
    assert_eq!(body["relay_allowed"], false);
    assert_eq!(body["devices"][0]["public_key"], key);
    assert_eq!(body["devices"][0]["label"], "a laptop");
    assert!(
        body.get("token").is_none(),
        "resuming sent the token back for no reason"
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn signing_out_stops_the_token_working() {
    let (router, path) = service("signout");
    let secret = register(&router, "someone@example.com").await;
    let token = sign_in(&router, "someone@example.com", &secret).await;

    let (status, _) = send(
        &router,
        authed("DELETE", "/v1/session", &token, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&router, authed("GET", "/v1/session", &token, Value::Null)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn a_made_up_token_is_refused() {
    let (router, path) = service("faketoken");
    register(&router, "someone@example.com").await;

    let (status, _) = send(
        &router,
        authed("GET", "/v1/devices", &"ab".repeat(32), Value::Null),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn devices_can_be_listed_added_and_removed() {
    let (router, path) = service("devices");
    let secret = register(&router, "someone@example.com").await;
    let token = sign_in(&router, "someone@example.com", &secret).await;

    let key = hex(&[7u8; KEY_LEN]);

    let (status, body) = send(
        &router,
        authed(
            "POST",
            "/v1/devices",
            &token,
            json!({ "public_key": key, "label": "laptop" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["devices"].as_array().expect("a list").len(), 1);

    let (_, body) = send(&router, authed("GET", "/v1/devices", &token, Value::Null)).await;
    assert_eq!(body["devices"][0]["label"], "laptop");

    let (status, body) = send(
        &router,
        authed("DELETE", &format!("/v1/devices/{key}"), &token, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["devices"].as_array().expect("a list").is_empty());
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn one_account_cannot_see_or_touch_another_ones_devices() {
    // The failure that turns an account system into a way to read other people's machines.
    let (router, path) = service("isolation");
    let mine = register(&router, "someone@example.com").await;
    let theirs = register(&router, "another@example.com").await;

    let my_token = sign_in(&router, "someone@example.com", &mine).await;
    let their_token = sign_in(&router, "another@example.com", &theirs).await;

    send(
        &router,
        authed(
            "POST",
            "/v1/devices",
            &my_token,
            json!({ "public_key": hex(&[7u8; KEY_LEN]), "label": "mine" }),
        ),
    )
    .await;

    let (_, body) = send(
        &router,
        authed("GET", "/v1/devices", &their_token, Value::Null),
    )
    .await;

    assert!(
        body["devices"].as_array().expect("a list").is_empty(),
        "another account's machines were visible"
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn changing_the_password_takes_effect_and_the_old_one_stops_working() {
    let (router, path) = service("rekey");
    let secret = register(&router, "someone@example.com").await;
    let token = sign_in(&router, "someone@example.com", &secret).await;

    let (status, _) = send(
        &router,
        authed(
            "PUT",
            "/v1/account/key",
            &token,
            json!({
                "salt": hex(&[8u8; SALT_LEN]),
                "auth": hex(&[9u8; SECRET_LEN]),
                "sealed_key": hex(&[4u8; 60]),
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock")
        .as_secs();
    let code = totp::code_at_time(&secret, now);

    let (old, _) = send(
        &router,
        post(
            "/v1/sessions",
            json!({ "email": "someone@example.com", "auth": hex(&[2u8; SECRET_LEN]), "code": code }),
        ),
    )
    .await;
    assert_eq!(
        old,
        StatusCode::UNAUTHORIZED,
        "the old password still works"
    );

    let (new, body) = send(
        &router,
        post(
            "/v1/sessions",
            json!({ "email": "someone@example.com", "auth": hex(&[9u8; SECRET_LEN]), "code": code }),
        ),
    )
    .await;
    assert_eq!(new, StatusCode::OK);
    assert_eq!(body["sealed_key"].as_str().expect("a key"), hex(&[4u8; 60]));
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn malformed_hex_is_refused_rather_than_stored() {
    // A field that is not what it claims reaches the store as something shorter, and a short
    // key is a key that will not open.
    let (router, path) = service("malformed");

    let (status, _) = send(
        &router,
        post(
            "/v1/accounts",
            json!({
                "email": "someone@example.com",
                "salt": "not hex",
                "auth": hex(&[2u8; SECRET_LEN]),
                "sealed_key": hex(&[3u8; 60]),
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn the_relay_is_off_for_a_new_account() {
    // It costs bandwidth somebody pays for.
    let (router, path) = service("relay");
    let secret = register(&router, "someone@example.com").await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock")
        .as_secs();

    let (_, body) = send(
        &router,
        post(
            "/v1/sessions",
            json!({
                "email": "someone@example.com",
                "auth": hex(&[2u8; SECRET_LEN]),
                "code": totp::code_at_time(&secret, now),
            }),
        ),
    )
    .await;

    assert_eq!(body["relay_allowed"], false);
    let _ = std::fs::remove_file(path);
}
