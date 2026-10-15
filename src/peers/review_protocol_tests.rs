//! Protocol rejections, additive settings and default-off audit regression cases.
use super::{audit, client, policy::Settings, store::Store, Error};
use crate::profiles::model::CodezConfig;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

fn exchange(method: &str, path: &str, status: u16, body: &str) -> super::Result<Value> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = body.to_string();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "expected HTTP request");
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
        write!(
            stream,
            "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let result = client::request_with_settings(
        &url,
        method,
        path,
        None,
        None,
        &Settings {
            connect_timeout_secs: 1,
            read_timeout_secs: 2,
            ..Settings::default()
        },
        true,
    );
    server.join().unwrap();
    result
}

const ASK: &str = "/peer/v1/asks/ask-00000000000000000000000000000001";
const OP: &str = "/peer/v1/asks/ask-00000000000000000000000000000001/ops";

#[test]
fn mutation_schema_rejection_is_an_ordinary_request_failure() {
    for (method, path) in [("PUT", ASK), ("POST", OP), ("POST", "/peer/v1/events")] {
        let error = exchange(
            method,
            path,
            400,
            r#"{"code":"peer_protocol_error","retryable":true}"#,
        )
        .unwrap_err();
        assert_eq!(error.code, "peer_request_incompatible", "{method} {path}");
        assert_eq!(error.status, 400);
        assert!(!error.retryable && !error.is_protocol() && !error.is_transport());
    }
}

#[test]
fn hello_and_response_decode_errors_remain_peer_protocol_failures() {
    for path in ["/peer/v1/hello", "/peer/v1/agents", ASK] {
        let error = exchange("GET", path, 400, r#"{"code":"peer_protocol_error"}"#).unwrap_err();
        assert_eq!(error.code, "peer_protocol_error");
        assert!(error.is_protocol() && !error.retryable);
    }
    for (method, path) in [("PUT", ASK), ("POST", OP), ("POST", "/peer/v1/events")] {
        for (status, body) in [(200, "not JSON"), (302, "")] {
            let error = exchange(method, path, status, body).unwrap_err();
            assert_eq!(error.code, "peer_protocol_error");
            assert_eq!(error.status, 502);
            assert!(error.is_protocol() && !error.is_transport());
        }
    }
}

#[test]
fn unrelated_http_refusals_and_transport_failures_keep_their_classification() {
    let error = exchange("PUT", ASK, 503, r#"{"code":"peer_protocol_error"}"#).unwrap_err();
    assert_eq!(error.code, "peer_unreachable");
    assert!(error.retryable && error.is_transport());
    let error = exchange("PUT", ASK, 403, r#"{"code":"peer_policy_denied"}"#).unwrap_err();
    assert_eq!(error.code, "peer_policy_denied");
    assert!(!error.retryable && !error.is_protocol());
    let error = exchange("POST", OP, 409, r#"{"code":"peer_unsupported"}"#).unwrap_err();
    assert_eq!(error.code, "peer_unsupported");
    assert!(error.is_protocol());
}

#[test]
fn unsafe_addresses_are_policy_rejections_not_version_mismatches() {
    let forbidden =
        client::validate_callback_addresses(&["127.0.0.1:80".parse().unwrap()], false).unwrap_err();
    assert_eq!(forbidden.code, "peer_callback_forbidden");
    assert_eq!(forbidden.status, 403);
    let insecure = client::addresses("http://8.8.8.8").unwrap_err();
    assert_eq!(insecure.code, "peer_url_insecure");
    assert_eq!(insecure.status, 400);
    for error in [forbidden, insecure] {
        assert!(!error.is_protocol() && !error.is_transport() && !error.retryable);
    }
}

#[test]
fn full_config_accepts_future_peer_fields_without_losing_other_settings() {
    let original = CodezConfig {
        management_port: Some(26470),
        agent_bus_port: Some(26460),
        default_profile: Some("preserve-config-profile".into()),
        peers: Settings {
            enabled: true,
            max_hops: 7,
            ..Settings::default()
        },
        ..CodezConfig::default()
    };
    let mut value = serde_json::to_value(&original).unwrap();
    value["peers"]["futureProtocolOptions"] = json!({"generation":2,"extensions":["new"]});
    value["futureConfigOption"] = json!(true);
    let parsed: CodezConfig = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(parsed.peers, original.peers);
    assert_eq!(parsed.management_port, original.management_port);
    assert_eq!(parsed.agent_bus_port, original.agent_bus_port);
    assert_eq!(parsed.default_profile, original.default_profile);

    value.as_object_mut().unwrap().remove("peers");
    let legacy: CodezConfig = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.peers, Settings::default());
    assert_eq!(legacy.management_port, original.management_port);
}

#[test]
fn sparse_peer_settings_preserve_the_full_config_and_default_omitted_fields() {
    for peers in [
        json!({}),
        json!({"enabled":true}),
        json!({"outboxMaxAgeHours":48}),
        json!({"maxHops":7}),
        json!({"refreshAgentsSecs":120}),
        json!({"connectTimeoutSecs":2}),
        json!({"readTimeoutSecs":8}),
        json!({"enabled":true,"futureSetting":{"value":1}}),
    ] {
        let config: CodezConfig = serde_json::from_value(json!({
            "management_port":26470,
            "agent_bus_port":26460,
            "default_profile":"preserve-sparse-config",
            "peers":peers,
        }))
        .unwrap();
        let mut expected = serde_json::to_value(Settings::default()).unwrap();
        for (key, value) in peers.as_object().unwrap() {
            if expected.get(key).is_some() {
                expected[key] = value.clone();
            }
        }
        assert_eq!(serde_json::to_value(config.peers).unwrap(), expected);
        assert_eq!(config.management_port, Some(26470));
        assert_eq!(config.agent_bus_port, Some(26460));
        assert_eq!(
            config.default_profile.as_deref(),
            Some("preserve-sparse-config")
        );
    }
    for peers in [json!(null), json!({"enabled":null}), json!({"maxHops":"3"})] {
        assert!(serde_json::from_value::<CodezConfig>(json!({"peers":peers})).is_err());
    }
}

#[test]
fn settings_patch_rejects_unknown_keys_and_still_validates_known_values() {
    let settings = Settings::default();
    let patched = settings
        .patched(&json!({
            "enabled":true,"outboxMaxAgeHours":48,"maxHops":5,
            "refreshAgentsSecs":90,"connectTimeoutSecs":2,"readTimeoutSecs":8
        }))
        .unwrap();
    assert!(patched.enabled);
    assert_eq!(patched.outbox_max_age_hours, 48);
    assert_eq!(patched.max_hops, 5);
    assert_eq!(patched.refresh_agents_secs, 90);
    assert_eq!(patched.connect_timeout_secs, 2);
    assert_eq!(patched.read_timeout_secs, 8);
    for patch in [
        json!({"futureProtocolOptions":true}),
        json!({"enabled":true,"readTimoutSecs":5}),
        json!({"enabled":"yes"}),
        json!({"readTimeoutSecs":0}),
        json!({"maxHops":9}),
        json!(null),
        json!([]),
    ] {
        let error = settings.patched(&patch).unwrap_err();
        assert_eq!(error.code, "invalid_request", "{patch}");
        assert_eq!(error.status, 400);
    }
    assert_eq!(settings, Settings::default());
}

#[test]
fn rejected_requests_do_not_initialize_default_off_peer_audit() {
    let root = std::env::temp_dir().join(format!("peer-unused-audit-{}", uuid::Uuid::new_v4()));
    let store = Store::at(root.join("runtime/peers"));
    for (route, code, status) in [
        ("/peer/v1/hello", "peer_credential_required", 401),
        ("/peer/v1/events", "peer_credential_required", 401),
        ("/peer/v1/pair", "invalid_request", 400),
    ] {
        audit::record(
            &store,
            "inbound",
            None,
            route,
            &Err(Error::new(code, status)),
            0,
            None,
        );
        assert!(!root.exists(), "rejected {route} created peer state");
    }
}

#[test]
fn unauthenticated_audit_does_not_initialize_enabled_but_unused_peers() {
    use crate::config::instance::{self, Selection};
    struct Restore(Selection);
    impl Drop for Restore {
        fn drop(&mut self) {
            instance::set_test_selection(self.0.clone());
        }
    }
    let _restore = Restore(instance::selected().unwrap());
    let root = crate::config::paths::home_dir()
        .unwrap()
        .join(format!("peer-enabled-audit-{}", uuid::Uuid::new_v4()));
    instance::set_test_selection(Selection::Custom(root.join(".cutex")));
    let mut config = CodezConfig::default();
    config.peers.enabled = true;
    crate::config::store::save_codez_config(&config).unwrap();
    let store = Store::open().unwrap();
    assert!(store.snapshot().unwrap().settings.enabled);
    assert!(!store.dir.exists());
    audit::record(
        &store,
        "inbound",
        None,
        "/peer/v1/events",
        &Err(Error::new("peer_credential_required", 401)),
        0,
        None,
    );
    assert!(!store.dir.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn established_peer_audit_keeps_rejections_without_logging_request_contents() {
    let root = std::env::temp_dir().join(format!("peer-active-audit-{}", uuid::Uuid::new_v4()));
    let store = Store::at(root.join("runtime/peers"));
    store.settings(&json!({"enabled":true})).unwrap();
    audit::record(
        &store,
        "inbound",
        None,
        "/peer/v1/events",
        &Err(Error::new("peer_credential_required", 401)),
        37,
        Some(&json!({"text":"private-body-must-not-appear"})),
    );
    let text = std::fs::read_to_string(store.dir.join("peer-audit.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(entry["status"], 401);
    assert_eq!(entry["code"], "peer_credential_required");
    assert_eq!(entry["bytes"], 37);
    assert!(!text.contains("private-body-must-not-appear"));
    std::fs::remove_dir_all(root).unwrap();
}
