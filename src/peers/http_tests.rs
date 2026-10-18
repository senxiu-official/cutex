//! HTTP exchanges through the production Management dispatcher and peer routes.
//! The accepted socket is local; ConnectionInfo injects the remote listener
//! context, so authentication is exercised without depending on a tailnet.
use super::{pair, store::Store, tests::peer, token};
use crate::config::instance::{self, Selection};
use crate::management::{
    client_tokens::ClientTokenStore,
    listeners::{self, ConnectionInfo},
    server::{handle_management_request, ManagementRequestContext},
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

const ROOT: &str = "peer-http-test-root-credential";
const ASK: &str = "ask-00000000000000000000000000000001";

struct PrivateInstance {
    root: PathBuf,
    previous: Selection,
}

impl PrivateInstance {
    fn new() -> Self {
        Self::with_enabled(true)
    }

    fn with_enabled(enabled: bool) -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("peer-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = instance::selected().unwrap();
        instance::set_test_selection(Selection::Custom(root.join(".cutex")));
        Store::open()
            .unwrap()
            .settings(&json!({"enabled":enabled}))
            .unwrap();
        Self { root, previous }
    }
}

impl Drop for PrivateInstance {
    fn drop(&mut self) {
        listeners::set_current_connection(None);
        listeners::set_current_client_token(None);
        instance::set_test_selection(self.previous.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn context() -> ManagementRequestContext {
    // Peer and authentication routes must never invoke native runtime callbacks.
    ManagementRequestContext {
        explicit_launch_action: |_, _| panic!("unexpected runtime action"),
        adopt_saved_native: |_, _| panic!("unexpected native adoption"),
        review_agent_archive: |_, _| panic!("unexpected archive review"),
        execute_agent_archive: |_, _| panic!("unexpected archive"),
        durable_agent_candidates: |_| panic!("unexpected agent candidates"),
        import_durable_agent: |_, _| panic!("unexpected agent import"),
        load_registry: || panic!("unexpected registry load"),
        load_runtime_status: |_| panic!("unexpected runtime status"),
        forward_native_request: |_, _| panic!("unexpected native request"),
        respond_native_server_request: |_, _, _| panic!("unexpected native response"),
        handle_user_input: |_| panic!("unexpected user input"),
        flush_user_input_queue: |_, _, _| panic!("unexpected queue flush"),
        steer_user_input_queue: |_, _, _| panic!("unexpected queue steer"),
        interrupt_active_turn: |_, _| panic!("unexpected interrupt"),
        load_bootstrap_state: |_, _| panic!("unexpected bootstrap"),
        load_latest_plan: |_| panic!("unexpected plan"),
        load_latest_token_usage: |_| panic!("unexpected usage"),
        revert_session_history: |_, _, _| panic!("unexpected revert"),
        delete_session: |_, _| panic!("unexpected session delete"),
        fork_session: |_, _, _| panic!("unexpected fork"),
        side_chat: |_, _, _| panic!("unexpected side chat"),
        open_offline_history: |_| panic!("unexpected history"),
        mutate_session: |_, _, _| panic!("unexpected session mutation"),
        online_session: |_, _, _| panic!("unexpected online request"),
        bind_project_authority: |_| panic!("unexpected project bind"),
        import_legacy_director_ownership: |_| panic!("unexpected ownership import"),
        reconcile_agent_reservation: |_| panic!("unexpected reservation"),
        list_management_projects: |_| panic!("unexpected project list"),
        read_management_project: |_, _| panic!("unexpected project read"),
        update_management_project_presentation: |_, _| panic!("unexpected project update"),
        execute_management_operator_action: |_, _| panic!("unexpected operator action"),
        execute_management_project_mutation: |_, _| panic!("unexpected project mutation"),
        create_human_agent: |_, _| panic!("unexpected agent create"),
        create_human_session: |_, _| panic!("unexpected session create"),
    }
}

fn exchange(
    remote: bool,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    instance_header: Option<&str>,
    body: &Value,
) -> (u16, Value) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.to_string();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(bearer) = bearer {
        request.push_str(&format!("Authorization: Bearer {bearer}\r\n"));
    }
    if let Some(id) = instance_header {
        request.push_str(&format!("X-Cutex-Peer-Instance: {id}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = vec![];
        stream.read_to_end(&mut response).unwrap();
        response
    });
    let (mut server, _) = listener.accept().unwrap();
    listeners::set_current_connection(Some(ConnectionInfo {
        remote,
        peer: if remote {
            "100.64.0.12:43210".parse().unwrap()
        } else {
            "127.0.0.1:43210".parse().unwrap()
        },
    }));
    listeners::set_current_client_token(None);
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    crate::http::server::set_extra_response_headers(String::new());
    crate::http::server::set_accept_gzip(false);
    let handled = handle_management_request(&mut server, None, Some(ROOT), context());
    listeners::set_current_connection(None);
    listeners::set_current_client_token(None);
    drop(server);
    handled.unwrap();
    let response = client.join().unwrap();
    let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    let value = serde_json::from_slice(&response[split + 4..]).unwrap();
    (status, value)
}

#[test]
fn http_settings_read_does_not_create_or_lock_the_peer_store() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::with_enabled(false);
    let store = Store::open().unwrap();
    assert!(!store.dir.exists());
    for _ in 0..2 {
        let (status, value) = exchange(false, "GET", "/v2/settings", Some(ROOT), None, &json!({}));
        assert_eq!(status, 200);
        assert_eq!(value["peers"]["enabled"], false);
        assert_eq!(value["peers"]["connectTimeoutSecs"], 5);
        assert_eq!(value["peers"]["readTimeoutSecs"], 30);
        assert!(!store.dir.exists());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Client,
    ActivePeer,
    PendingPeer,
    RefusedPeer,
    NoCredential,
}

struct Credential {
    name: &'static str,
    kind: Kind,
    bearer: Option<String>,
    instance_header: Option<String>,
}

fn error_code(value: &Value) -> Option<&str> {
    // Management's listener gate uses the standard v2 error envelope; peer
    // route semantic errors use the versioned peer protocol's flat envelope.
    value
        .get("code")
        .or_else(|| value.pointer("/error/code"))
        .and_then(Value::as_str)
}

fn insert_peer(store: &Store, label: &str, pending: bool) -> (String, String) {
    let mut peer = peer();
    peer.label = label.into();
    let inbound = token::generate();
    peer.inbound_token_sha256 = token::hash(&inbound);
    if pending {
        peer.pending_until = Some((chrono::Utc::now() + chrono::Duration::minutes(2)).to_rfc3339());
    }
    store.insert(peer.clone(), &token::generate()).unwrap();
    (peer.peer_id, inbound)
}

#[test]
fn management_and_peer_routes_enforce_the_http_credential_matrix() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let store = Store::open().unwrap();
    let (active_id, active_token) = insert_peer(&store, "active", false);
    let (pending_id, pending_token) = insert_peer(&store, "pending", true);
    let (revoked_id, revoked_token) = insert_peer(&store, "revoked", false);
    store.remove(&revoked_id).unwrap();
    let management_token = ClientTokenStore::open_default()
        .unwrap()
        .issue("peer-http-matrix")
        .unwrap()
        .token;
    let credentials = [
        Credential {
            name: "root",
            kind: Kind::Root,
            bearer: Some(ROOT.into()),
            instance_header: None,
        },
        Credential {
            name: "valid-cxm1",
            kind: Kind::Client,
            bearer: Some(management_token),
            instance_header: None,
        },
        Credential {
            name: "valid-cxp1",
            kind: Kind::ActivePeer,
            bearer: Some(active_token.clone()),
            instance_header: Some(active_id.clone()),
        },
        Credential {
            name: "wrong-instance",
            kind: Kind::RefusedPeer,
            bearer: Some(active_token),
            instance_header: Some(pending_id.clone()),
        },
        Credential {
            name: "revoked-cxp1",
            kind: Kind::RefusedPeer,
            bearer: Some(revoked_token),
            instance_header: Some(revoked_id),
        },
        Credential {
            name: "pending-cxp1",
            kind: Kind::PendingPeer,
            bearer: Some(pending_token),
            instance_header: Some(pending_id),
        },
        Credential {
            name: "wrong-cxp1",
            kind: Kind::RefusedPeer,
            bearer: Some(token::generate()),
            instance_header: Some(active_id.clone()),
        },
        Credential {
            name: "absent",
            kind: Kind::NoCredential,
            bearer: None,
            instance_header: None,
        },
    ];
    let owner_write = format!("/v2/peers/{active_id}");
    let ask_read = format!("/peer/v1/asks/{ASK}");
    let ask_op = format!("{ask_read}/ops");
    let routes = [
        ("GET", "/v2/peers", json!({}), 200, ""),
        ("PATCH", owner_write.as_str(), json!({}), 200, ""),
        ("GET", "/peer/v1/hello", json!({}), 200, ""),
        ("GET", "/peer/v1/agents", json!({}), 200, ""),
        ("GET", ask_read.as_str(), json!({}), 404, "ask_not_found"),
        (
            "PUT",
            ask_read.as_str(),
            json!({}),
            400,
            "peer_protocol_error",
        ),
        (
            "POST",
            ask_op.as_str(),
            json!({"opId":"pop-http-matrix","op":"stop"}),
            404,
            "ask_not_found",
        ),
        (
            "POST",
            "/peer/v1/events",
            json!({"fromInstanceId":active_id,"events":[]}),
            200,
            "",
        ),
    ];
    let mut count = 0;
    for remote in [false, true] {
        for credential in &credentials {
            for (method, path, body, route_status, route_code) in &routes {
                let is_peer = path.starts_with("/peer/");
                let (expected_status, expected_code) = if is_peer {
                    match credential.kind {
                        Kind::ActivePeer => (*route_status, *route_code),
                        Kind::PendingPeer if *path == "/peer/v1/hello" => (200, ""),
                        Kind::PendingPeer => (409, "pair_incomplete"),
                        _ => (401, "peer_credential_required"),
                    }
                } else {
                    match credential.kind {
                        Kind::Root if remote => (401, "remote_credential_refused"),
                        Kind::Root => (200, ""),
                        Kind::Client if remote && *method != "GET" => (403, "loopback_only"),
                        Kind::Client => (200, ""),
                        Kind::NoCredential => (401, "unauthorized"),
                        _ => (401, "remote_credential_refused"),
                    }
                };
                let (status, value) = exchange(
                    remote,
                    method,
                    path,
                    credential.bearer.as_deref(),
                    credential.instance_header.as_deref(),
                    body,
                );
                assert_eq!(
                    status, expected_status,
                    "credential={} remote={remote} {method} {path}",
                    credential.name
                );
                if !expected_code.is_empty() {
                    assert_eq!(
                        error_code(&value),
                        Some(expected_code),
                        "credential={} remote={remote} {method} {path}",
                        credential.name
                    );
                }
                count += 1;
            }
        }
    }
    assert_eq!(count, 128);
    assert!(store.get(&active_id).unwrap().enabled);
}

#[test]
fn http_pair_callback_abort_and_unpair_follow_the_connection_and_lifecycle() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let store = Store::open().unwrap();
    for remote in [true, false] {
        let invitation = pair::invite(&json!({})).unwrap();
        let newcomer = peer();
        let (status, value) = exchange(
            remote,
            "POST",
            "/peer/v1/pair",
            None,
            None,
            &json!({
                "code":invitation["code"], "instanceId":newcomer.peer_id,
                "hostId":"callback-test", "label":"callback-test",
                "url":"http://127.0.0.1:26270", "tokenForYou":token::generate()
            }),
        );
        if remote {
            assert_eq!(status, 403);
            assert_eq!(value["code"], "peer_callback_forbidden");
            assert!(store.get(&newcomer.peer_id).is_err());
        } else {
            assert_eq!(status, 200);
            let pending = store.get(&newcomer.peer_id).unwrap();
            assert!(pending.allow_loopback);
            assert!(pending.pending_until.is_some());
            let (status, _) = exchange(
                false,
                "POST",
                "/peer/v1/abort",
                value["tokenForYou"].as_str(),
                Some(&newcomer.peer_id),
                &json!({}),
            );
            assert_eq!(status, 200);
            assert!(store.get(&newcomer.peer_id).is_err());
        }
    }
    let (active_id, active_token) = insert_peer(&store, "active", false);
    for remote in [false, true] {
        let before = store.get(&active_id).unwrap();
        let (status, value) = exchange(
            remote,
            "POST",
            "/peer/v1/confirm",
            Some(&active_token),
            Some(&active_id),
            &json!({}),
        );
        assert_eq!(status, 200);
        assert_eq!(value["confirmed"], true);
        assert_eq!(store.get(&active_id).unwrap(), before);
        let (status, value) = exchange(
            remote,
            "POST",
            "/peer/v1/abort",
            Some(&active_token),
            Some(&active_id),
            &json!({}),
        );
        assert_eq!(status, 409);
        assert_eq!(value["code"], "peer_already_active");
        assert!(store.get(&active_id).unwrap().enabled);
    }
    let (status, _) = exchange(
        true,
        "POST",
        "/peer/v1/unpair",
        Some(&active_token),
        Some(&active_id),
        &json!({}),
    );
    assert_eq!(status, 200);
    let removed = store.get(&active_id).unwrap();
    assert_eq!(removed.status, "removedByPeer");
    assert!(!removed.enabled);
    assert!(store.secret(&active_id).is_err());
    let (status, value) = exchange(
        true,
        "GET",
        "/peer/v1/hello",
        Some(&active_token),
        Some(&active_id),
        &json!({}),
    );
    assert_eq!(status, 401);
    assert_eq!(value["code"], "peer_credential_required");
}

/// Person identity (P2 §4): a peer whose remote owner key belongs to a local
/// user is bound to that user when the invite left the user to the default;
/// an explicit invite user always wins; the response carries our owner keys.
#[test]
fn http_pair_binds_a_known_remote_owner_person_unless_the_invite_names_a_user() {
    use base64::Engine;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let store = Store::open().unwrap();
    let users = crate::management::users::UserStore::open_default().unwrap();
    let key = |seed: u8| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([seed; 32]);
    let colleague = users
        .create_user("Colleague", None, crate::management::users::Grant::default(), "root")
        .unwrap();
    users.set_person_keys(&colleague.id, vec![(key(21), None)], "root").unwrap();
    users.set_person_keys("owner", vec![(key(22), None)], "root").unwrap();
    for (n, (invite, expected)) in [(json!({}), colleague.id.clone()), (json!({"userId": "owner"}), "owner".to_string())].into_iter().enumerate() {
        let invitation = pair::invite(&invite).unwrap();
        let newcomer = peer();
        let (status, value) = exchange(
            false,
            "POST",
            "/peer/v1/pair",
            None,
            None,
            &json!({
                "code":invitation["code"], "instanceId":newcomer.peer_id,
                "hostId":"person-test", "label":format!("person-test-{n}"),
                "url":"http://127.0.0.1:26270", "tokenForYou":token::generate(),
                "ownerPersonKeys":[key(21), "junk"]
            }),
        );
        assert_eq!(status, 200, "{value}");
        assert_eq!(value["ownerPersonKeys"], json!([key(22)]));
        assert_eq!(store.get(&newcomer.peer_id).unwrap().user_id, expected, "{invite}");
        let persons = users.persons_snapshot().unwrap();
        assert_eq!(persons.peer_persons[&newcomer.peer_id], vec![key(21)]);
    }
    let audit = std::fs::read_to_string(users.dir().join("audit.jsonl")).unwrap();
    assert_eq!(audit.matches("peerAutoBound").count(), 1);
}
