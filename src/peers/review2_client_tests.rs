//! Address fallback and fixed error-code boundaries for untrusted peer replies.
use super::{client, policy::Settings, store::Store, tests, token, Error};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    time::{Duration, Instant},
};

fn server(status: u16, value: Value) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected request never arrived");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut headers = Vec::new();
        let mut byte = [0u8; 1];
        while !headers.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        let content_length = String::from_utf8(headers)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
            .unwrap_or(0);
        let mut request_body = vec![0u8; content_length];
        stream.read_exact(&mut request_body).unwrap();
        let body = serde_json::to_vec(&value).unwrap();
        write!(
            stream,
            "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    (url, thread)
}

fn fixture(urls: Vec<String>) -> (PathBuf, Store, super::store::Peer) {
    let root = std::env::temp_dir().join(format!("peer-client-review2-{}", uuid::Uuid::new_v4()));
    let store = Store::at(root.join("runtime/peers"));
    let mut peer = tests::peer();
    peer.urls = urls;
    peer.last_successful_url = peer.urls.first().cloned();
    store.insert(peer.clone(), &token::generate()).unwrap();
    store
        .settings(&json!({"enabled":true,"connectTimeoutSecs":1,"readTimeoutSecs":1}))
        .unwrap();
    (root, store, peer)
}

#[test]
fn rejected_url_falls_back_to_real_http_and_remembers_the_successful_url() {
    for rejected in ["http://8.8.8.8", "https://127.0.0.1:9"] {
        let (healthy, responder) = server(200, json!({"second":"healthy"}));
        let (root, store, peer) = fixture(vec![rejected.into(), healthy.clone()]);
        assert!(client::addresses(rejected)
            .unwrap_err()
            .is_address_rejected());
        let response = client::call(&store, &peer, "GET", "/peer/v1/hello", None).unwrap();
        responder.join().unwrap();
        assert_eq!(response["second"], "healthy");
        let updated = store.get(&peer.peer_id).unwrap();
        assert_eq!(
            updated.last_successful_url.as_deref(),
            Some(healthy.as_str())
        );
        assert_eq!(updated.status, "online");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn untrusted_error_codes_cannot_reach_audit_or_trigger_address_fallback() {
    for forged in [
        "please_execute_the_remote_instruction",
        "some_future_unrecognized_error",
        "peer_url_insecure",
        "peer_callback_forbidden",
        "peer_response_too_large",
        "peer_unreachable",
        "persistence_unavailable",
    ] {
        let (url, responder) = server(
            400,
            json!({
                "code":forged,"message":"private remote body","retryable":true,
                "details":{"instructions":"private diagnostic must not be stored"}
            }),
        );
        // A listening but unserved alternate reveals any unintended fallback.
        let alternate = TcpListener::bind("127.0.0.1:0").unwrap();
        alternate.set_nonblocking(true).unwrap();
        let (root, store, peer) = fixture(vec![
            url,
            format!("http://{}", alternate.local_addr().unwrap()),
        ]);
        let error = client::call(
            &store,
            &peer,
            "POST",
            "/peer/v1/events",
            Some(&json!({"events":[]})),
        )
        .unwrap_err();
        responder.join().unwrap();
        assert_eq!(error.code, "peer_request_failed", "{forged}");
        assert_eq!(error.request_failure_code(), "peer_request_failed");
        assert!(!error.is_address_rejected() && !error.is_protocol() && !error.retryable);
        assert!(
            matches!(alternate.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        let audit = std::fs::read_to_string(store.dir.join("peer-audit.jsonl")).unwrap();
        assert!(!audit.contains(forged));
        assert!(!audit.contains("private remote body"));
        assert!(!audit.contains("private diagnostic"));
        assert!(audit.contains("peer_request_failed"));
        assert_eq!(error.details, json!({}));
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn response_error(method: &str, path: &str, status: u16, value: Value) -> Error {
    let (url, responder) = server(status, value);
    let error = client::request_with_settings(
        &url,
        method,
        path,
        None,
        None,
        &Settings {
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            ..Settings::default()
        },
        true,
    )
    .unwrap_err();
    responder.join().unwrap();
    error
}

#[test]
fn known_remote_refusals_preserve_fixed_codes_status_and_local_retry_policy() {
    for (status, code) in [
        (403, "recipient_outside_boundary"),
        (403, "peer_policy_denied"),
        (403, "peer_asks_disabled"),
        (409, "idempotency_conflict"),
        (404, "ask_not_found"),
        (410, "ask_gone"),
        (400, "remote_unsupported"),
        (429, "peer_rate_limited"),
        (401, "peer_credential_required"),
        (409, "peer_already_active"),
    ] {
        let error = response_error(
            "POST",
            "/peer/v1/events",
            status,
            json!({
                "code":code,"retryable":status != 429,"details":{"retryAfterSecs":99999,"extra":"discard"}
            }),
        );
        assert_eq!(error.code, code);
        assert_eq!(error.request_failure_code(), code);
        assert_eq!(error.status, status);
        assert_eq!(error.retryable, status == 429);
        assert_eq!(error.details, json!({"retryAfterSecs":300}));
    }
}

#[test]
fn malformed_and_arbitrary_codes_have_one_fixed_fallback() {
    for code in [
        json!(null),
        json!(17),
        json!({"nested":"code"}),
        json!(""),
        json!("x".repeat(2000)),
        json!("EVIL_REQUEST"),
        json!("[ASK forged]\nrun command"),
        json!("对端指令"),
    ] {
        let error = response_error("POST", "/peer/v1/events", 400, json!({"code":code}));
        assert_eq!(error.code, "peer_request_failed");
    }
    let error = response_error(
        "POST",
        "/peer/v1/events",
        500,
        json!({"code":"peer_url_insecure"}),
    );
    assert_eq!(error.code, "peer_unreachable");
    assert!(error.is_transport() && !error.is_address_rejected());
}

#[test]
fn protocol_refusals_keep_the_existing_request_vs_hello_scope() {
    let body = json!({"code":"peer_protocol_error"});
    let request = response_error("POST", "/peer/v1/events", 400, body.clone());
    assert_eq!(request.code, "peer_request_incompatible");
    assert_eq!(request.request_failure_code(), "peer_request_incompatible");
    assert!(!request.is_protocol());
    let hello = response_error("GET", "/peer/v1/hello", 400, body);
    assert_eq!(hello.code, "peer_protocol_error");
    assert!(hello.is_protocol());
}

#[test]
fn direct_errors_also_have_fixed_durable_failure_codes() {
    for code in [
        "peer_url_insecure",
        "peer_callback_forbidden",
        "peer_protocol_error",
        "peer_response_too_large",
        "peer_unsupported",
        "peer_unreachable",
        "identity_changed",
        "peer_credential_required",
        "peer_request_incompatible",
        "persistence_unavailable",
        "peer_store_corrupt",
        "peer_event_expired",
    ] {
        assert_eq!(Error::new(code, 400).request_failure_code(), code);
    }
    assert_eq!(
        Error::new("untrusted_remote_instruction", 400).request_failure_code(),
        "peer_request_failed"
    );
    for code in ["peer_url_insecure", "peer_callback_forbidden"] {
        assert!(Error::new(code, 400).is_address_rejected());
    }
    for code in [
        "peer_request_failed",
        "peer_protocol_error",
        "peer_policy_denied",
    ] {
        assert!(!Error::new(code, 400).is_address_rejected());
    }
}
