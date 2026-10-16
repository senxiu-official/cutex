//! The access gate through the production Management dispatcher, on a
//! private instance (no live state, no daemon).
use crate::config::instance::{self, Selection};
use crate::management::{
    client_tokens::ClientTokenStore,
    listeners::{self, ConnectionInfo},
    server::{handle_management_request, ManagementRequestContext},
    users::{Grant, Projects, Scope, UserStore, OWNER},
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

const ROOT: &str = "access-http-test-root-credential";

struct PrivateInstance {
    root: PathBuf,
    previous: Selection,
}

impl PrivateInstance {
    fn new() -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("access-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = instance::selected().unwrap();
        instance::set_test_selection(Selection::Custom(root.join(".cutex")));
        Self { root, previous }
    }
}

impl Drop for PrivateInstance {
    fn drop(&mut self) {
        listeners::set_current_connection(None);
        listeners::set_current_client_token(None);
        crate::management::users::set_current_principal(None);
        instance::set_test_selection(self.previous.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn context() -> ManagementRequestContext {
    // Denied and gate-answered requests never reach native runtime callbacks.
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
        revert_session_history: |_, _| panic!("unexpected revert"),
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

fn exchange(method: &str, path: &str, bearer: Option<&str>, body: &Value) -> (u16, Value) {
    let (status, _, body) = exchange_raw(method, path, bearer, body, "");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// `exchange` with extra request headers; returns the response head and raw body.
fn exchange_raw(method: &str, path: &str, bearer: Option<&str>, body: &Value, extra: &str) -> (u16, String, Vec<u8>) {
    exchange_with(context(), method, path, bearer, body.to_string().into_bytes(), "application/json", extra)
}

/// The full form: own context, raw body and content type.
fn exchange_with(
    context: ManagementRequestContext,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Vec<u8>,
    content_type: &str,
    extra: &str,
) -> (u16, String, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    ).into_bytes();
    if let Some(bearer) = bearer {
        request.extend_from_slice(format!("Authorization: Bearer {bearer}\r\n").as_bytes());
    }
    request.extend_from_slice(extra.as_bytes());
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(&body);
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        stream.write_all(&request).unwrap();
        let mut response = vec![];
        stream.read_to_end(&mut response).unwrap();
        response
    });
    let (mut server, _) = listener.accept().unwrap();
    listeners::set_current_connection(Some(ConnectionInfo {
        remote: false,
        peer: "127.0.0.1:43210".parse().unwrap(),
    }));
    listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    crate::http::server::set_extra_response_headers(String::new());
    crate::http::server::set_accept_gzip(false);
    let handled = handle_management_request(&mut server, None, Some(ROOT), context);
    listeners::set_current_connection(None);
    listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    drop(server);
    handled.unwrap();
    let response = client.join().unwrap();
    let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, headers.to_string(), response[split + 4..].to_vec())
}

fn code(value: &Value) -> &str {
    value.pointer("/error/code").and_then(Value::as_str).unwrap_or("")
}

#[test]
fn owner_user_and_pending_tokens_get_exactly_their_rights() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    // Existing token before the first users read: migrated to owner.
    let owner = tokens.issue("owner-desktop").unwrap();
    let users = UserStore::open_default().unwrap();
    assert_eq!(users.principal_for_token(&owner.token_id), crate::management::users::Principal::Owner);
    let pending = tokens.issue("ssh-device").unwrap();
    let user2 = users
        .create_user("User 2", None, Grant { projects: Projects::Only(vec!["p1".into()]), standalone_agents: vec![], send: false }, "root")
        .unwrap();
    let device = tokens.issue("device-c").unwrap();
    users.bind_token(&device.token_id, Some(&user2.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    // Capabilities: every principal, with its own user.
    let (status, caps) = exchange("GET", "/v2/capabilities", Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(caps["user"]["isOwner"], true);
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "users"));
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(ROOT), &json!({}));
    assert_eq!(caps["user"]["id"], OWNER);
    let (status, caps) = exchange("GET", "/v2/capabilities", Some(&pending.token), &json!({}));
    assert_eq!((status, &caps["user"]["pending"]), (200, &json!(true)));
    let (status, caps) = exchange("GET", "/v2/capabilities", Some(&device.token), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(caps["user"]["id"], user2.id.as_str());
    assert_eq!(caps["user"]["grant"]["projects"], json!(["p1"]));
    let (status, host) = exchange("GET", "/v2/host", Some(&device.token), &json!({}));
    assert_eq!(status, 200);
    assert!(host.get("executable").is_none());

    // Pending: nothing else.
    for (method, path) in [("GET", "/v2/sessions"), ("GET", "/v2/settings"), ("GET", "/v2/summary")] {
        let (status, value) = exchange(method, path, Some(&pending.token), &json!({}));
        assert_eq!((status, code(&value)), (403, "access_pending"), "{method} {path}");
    }

    // Non-owner: owner routes refused, invisible objects 404, users API closed.
    for (method, path, body) in [
        ("GET", "/v2/settings", json!({})),
        ("GET", "/v2/users", json!({})),
        ("POST", "/v2/users", json!({"name": "escalate"})),
        ("PUT", &format!("/v2/users/token-bindings/{}", device.token_id) as &str, json!({"userId": "owner"})),
        ("PATCH", &format!("/v2/users/{}", user2.id) as &str, json!({"grant": {"projects": "all", "send": true}})),
        ("POST", "/v2/pairing-codes", json!({"userId": "owner"})),
        ("GET", "/v2/client-tokens", json!({})),
        ("PATCH", &format!("/v2/client-tokens/{}", device.token_id) as &str, json!({"name": "mine"})),
        ("GET", "/v2/peers", json!({})),
        ("GET", "/v2/fs/file?path=/etc/passwd", json!({})),
        ("POST", "/v2/terminals", json!({})),
        ("GET", "/v2/sessions?includeHidden=true", json!({})),
        ("GET", "/v2/sessions?lifecycle=retired", json!({})),
        ("GET", "/v2/brand-new-route", json!({})),
    ] {
        let (status, value) = exchange(method, path, Some(&device.token), &body);
        assert_eq!((status, code(&value)), (403, "owner_required"), "{method} {path}");
    }
    for (method, path, body) in [
        ("GET", "/v2/sessions/cutex.hidden", json!({})),
        ("GET", "/v2/sessions/cutex.hidden/bootstrap", json!({})),
        ("GET", "/v2/sessions/cutex.hidden/history", json!({})),
        ("POST", "/v2/sessions/cutex.hidden/cutex/requests", json!({"requestId": "r", "method": "cutex/session/get", "params": {}})),
        ("POST", "/v2/sessions/cutex.hidden/app-server/requests", json!({"requestId": "r", "native": {"message": {"id": 1, "method": "thread/read"}}})),
        ("GET", "/v2/events?cutexSessionId=cutex.hidden", json!({})),
    ] {
        let (status, value) = exchange(method, path, Some(&device.token), &body);
        assert_eq!((status, code(&value)), (404, "session_not_found"), "{method} {path}");
    }
    let (status, value) = exchange("GET", "/v2/agent-management/projects/p2", Some(&device.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "project_not_found"));

    // The owner keeps everything, including the users API.
    let (status, listing) = exchange("GET", "/v2/users", Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    let tokens_view = listing["tokens"].as_array().unwrap();
    let find = |id: &str| tokens_view.iter().find(|t| t["tokenId"] == id).unwrap().clone();
    assert_eq!(find(&owner.token_id)["userId"], OWNER);
    assert_eq!(find(&pending.token_id)["pending"], true);
    assert_eq!(find(&device.token_id)["userId"], user2.id.as_str());
    assert!(!listing.to_string().contains("cxm1."), "no secrets in the listing");
    let (status, _) = exchange("GET", "/v2/users", Some(ROOT), &json!({}));
    assert_eq!(status, 200);

    // Owner assigns the pending device; it is then the user's.
    let (status, bound) = exchange(
        "PUT",
        &format!("/v2/users/token-bindings/{}", pending.token_id),
        Some(&owner.token),
        &json!({"userId": user2.id}),
    );
    assert_eq!((status, &bound["pending"]), (200, &json!(false)));
    std::thread::sleep(Duration::from_millis(30));
    let (status, value) = exchange("GET", "/v2/settings", Some(&pending.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
}

#[test]
fn rebinding_or_regranting_ends_open_streams_but_revocation_is_distinct() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let issued = tokens.issue("desktop").unwrap();
    users.bind_token(&issued.token_id, Some(OWNER), "root").unwrap();
    let mut request = crate::http::server::SimpleHttpRequest {
        method: "GET".into(),
        path: "/v2/events/stream".into(),
        headers: [("authorization".to_string(), format!("Bearer {}", issued.token))].into_iter().collect(),
        body: Vec::new(),
    };
    let verified = crate::management::client_tokens::authenticate_management_request_with(&mut request, Some(ROOT), &tokens).unwrap();
    assert!(verified.still_valid_in(&tokens));
    let user = users.create_user("C", None, Grant::default(), "root").unwrap();
    users.bind_token(&issued.token_id, Some(&user.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert!(!verified.still_valid_in(&tokens), "a rebound owner token must end its streams");

    // The access-changed frame (token still issued) differs from revoked.
    let mut out = Vec::new();
    listeners::write_revoked_frame(&mut out, &issued.token_id).unwrap();
    assert!(String::from_utf8_lossy(&out).starts_with("event: cutex/access/changed"));
    tokens.revoke(&issued.token_id).unwrap();
    let mut out = Vec::new();
    listeners::write_revoked_frame(&mut out, &issued.token_id).unwrap();
    assert!(String::from_utf8_lossy(&out).starts_with("event: cutex/device/revoked"));
}

#[test]
fn scoped_sessions_check_methods_send_and_input_types() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let mut store = crate::session::model::CutexSessionStore::default();
    for id in ["cutex.solo", "cutex.other"] {
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.to_string(),
            None,
            "test-host".to_string(),
            "/tmp".to_string(),
            None,
        )
        .unwrap();
        record.exposed_to_backend = true;
        store.sessions.insert(id.to_string(), record);
    }
    let mut hidden = crate::session::model::CutexSessionRecord::new(
        "cutex.hidden".to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
    )
    .unwrap();
    hidden.exposed_to_backend = false;
    hidden.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    store.sessions.insert("cutex.hidden".to_string(), hidden);
    let mut local = crate::session::model::CutexSessionRecord::new(
        "cutex.local-hidden".to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
    )
    .unwrap();
    local.exposed_to_backend = false;
    store.sessions.insert("cutex.local-hidden".to_string(), local);
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let reader = users
        .create_user("Reader", None, Grant { standalone_agents: vec!["cutex.solo".into()], ..Grant::default() }, "root")
        .unwrap();
    let sender = users
        .create_user("Sender", None, Grant { standalone_agents: vec!["cutex.solo".into()], send: true, ..Grant::default() }, "root")
        .unwrap();
    let read_token = tokens.issue("reader").unwrap();
    let send_token = tokens.issue("sender").unwrap();
    users.bind_token(&read_token.token_id, Some(&reader.id), "root").unwrap();
    users.bind_token(&send_token.token_id, Some(&sender.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let scope = Scope::load(&Grant { standalone_agents: vec!["cutex.solo".into()], ..Grant::default() }).unwrap();
    assert!(scope.sees_session("cutex.solo") && !scope.sees_session("cutex.other"));
    // The retired IM exposure flag no longer hides a durable agent: the grant decides.
    let unexposed_grant = Grant { standalone_agents: vec!["cutex.hidden".into()], ..Grant::default() };
    let unexposed = Scope::load(&unexposed_grant).unwrap();
    assert!(unexposed.sees_session("cutex.hidden"), "a granted unexposed durable agent is visible");
    assert!(!unexposed.sees_session("cutex.solo"), "grants still scope the user");
    let local_grant = Grant { standalone_agents: vec!["cutex.local-hidden".into()], ..Grant::default() };
    assert!(
        !Scope::load(&local_grant).unwrap().sees_session("cutex.local-hidden"),
        "an unexposed local-only session stays the owner's"
    );
    let all = Scope::load(&Grant { projects: Projects::All, ..Grant::default() }).unwrap();
    assert!(!all.sees_session("cutex.solo"), "\"all\" projects never includes standalone agents");

    let cutex = |method: &str, params: Value| json!({"requestId": "r-1", "method": method, "params": params});
    let text = json!({"clientUserMessageId": "m", "origin": {"kind": "desktop", "clientId": "c"}, "strategy": "queue", "input": [{"type": "text", "text": "hi"}]});
    let image = json!({"clientUserMessageId": "m", "origin": {"kind": "desktop", "clientId": "c"}, "strategy": "queue", "input": [{"type": "localImage", "path": "/etc/shadow"}]});
    let path = "/v2/sessions/cutex.solo/cutex/requests";
    let cases: Vec<(&str, Value, u16, &str)> = vec![
        (read_token.token.as_str(), cutex("cutex/session/retire", json!({})), 403, "owner_required"),
        (send_token.token.as_str(), cutex("cutex/session/delete", json!({})), 403, "owner_required"),
        (send_token.token.as_str(), cutex("cutex/ask/create", json!({})), 403, "owner_required"),
        (send_token.token.as_str(), cutex("cutex/runtime/online", json!({})), 403, "owner_required"),
        (read_token.token.as_str(), cutex("cutex/userInput/submit", text.clone()), 403, "send_not_granted"),
        (send_token.token.as_str(), cutex("cutex/userInput/submit", image), 403, "attachment_not_owned"),
    ];
    for (token, body, status, expected) in cases {
        let (got, value) = exchange("POST", path, Some(token), &body);
        assert_eq!((got, code(&value)), (status, expected), "{body}");
    }
    let native = |method: &str| json!({"requestId": "r", "native": {"message": {"id": 1, "method": method, "params": {"threadId": "t"}}}});
    let native_path = "/v2/sessions/cutex.solo/app-server/requests";
    for (token, method, expected) in [
        (send_token.token.as_str(), "turn/start", "owner_required"),
        (send_token.token.as_str(), "turn/steer", "owner_required"),
        (send_token.token.as_str(), "thread/settings/update", "owner_required"),
        (read_token.token.as_str(), "turn/interrupt", "send_not_granted"),
    ] {
        let (got, value) = exchange("POST", native_path, Some(token), &native(method));
        assert_eq!((got, code(&value)), (403, expected), "{method}");
    }
    let (got, value) = exchange("POST", "/v2/sessions/cutex.solo/app-server/server-request-responses", Some(&read_token.token), &json!({}));
    assert_eq!((got, code(&value)), (403, "send_not_granted"));
    let (got, value) = exchange("POST", "/v2/sessions/cutex.other/cutex/requests", Some(&send_token.token), &cutex("cutex/userInput/submit", text));
    assert_eq!((got, code(&value)), (404, "session_not_found"));
}

fn png(side: u32) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(side, side).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
fn self_service_profiles_and_avatar_visibility() {
    use base64::Engine;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let mut store = crate::session::model::CutexSessionStore::default();
    for id in ["cutex.profile-shared", "cutex.profile-apart"] {
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
        )
        .unwrap();
        record.exposed_to_backend = true;
        store.sessions.insert(id.to_string(), record);
    }
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let owner = tokens.issue("owner-desktop").unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let pending = tokens.issue("pending").unwrap();
    let shared = |send| Grant { standalone_agents: vec!["cutex.profile-shared".into()], send, ..Grant::default() };
    let alice = users.create_user("Alias A", None, shared(true), "root").unwrap();
    let bob = users.create_user("Alias B", None, shared(false), "root").unwrap();
    let carol = users
        .create_user("Alias C", None, Grant { standalone_agents: vec!["cutex.profile-apart".into()], ..Grant::default() }, "root")
        .unwrap();
    let a = tokens.issue("alice-laptop").unwrap();
    let b = tokens.issue("bob-laptop").unwrap();
    let c = tokens.issue("carol-laptop").unwrap();
    users.bind_token(&a.token_id, Some(&alice.id), "root").unwrap();
    users.bind_token(&b.token_id, Some(&bob.id), "root").unwrap();
    users.bind_token(&c.token_id, Some(&carol.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let avatar = |side| json!({"contentType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png(side))});

    // Everyone assigned reads and edits only their own record.
    let (status, me) = exchange("GET", "/v2/users/me", Some(&a.token), &json!({}));
    assert_eq!(status, 200);
    assert_eq!(me["user"], json!({"id": alice.id, "name": "Alias A", "isOwner": false, "displayName": null, "avatarEtag": null}));
    crate::management::v2::schema_export::assert_valid("users", "selfProfile", &me);
    let (status, me) = exchange("PATCH", "/v2/users/me", Some(&a.token), &json!({"displayName": "Alice", "avatar": avatar(128)}));
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["user"]["displayName"], "Alice");
    let alice_etag = me["user"]["avatarEtag"].as_str().unwrap().to_string();
    assert_eq!(alice_etag.len(), 16);
    let (status, me) = exchange("PATCH", "/v2/users/me", Some(&owner.token), &json!({"displayName": "Riley", "avatar": avatar(64)}));
    assert_eq!((status, &me["user"]["id"]), (200, &json!(OWNER)));
    let (status, me) = exchange("GET", "/v2/users/me", Some(ROOT), &json!({}));
    assert_eq!((status, &me["user"]["displayName"]), (200, &json!("Riley")), "root credential = owner");

    for (body, expected) in [
        (json!({"grant": {"projects": "all", "send": true}}), "invalid_request"),
        (json!({"name": "rename my alias"}), "invalid_request"),
        (json!({"displayName": ""}), "invalid_request"),
        (json!({"avatar": {"contentType": "image/webp", "data": base64::engine::general_purpose::STANDARD.encode(png(128))}}), "invalid_avatar"),
        (json!({"avatar": avatar(600)}), "invalid_avatar"),
    ] {
        let (status, value) = exchange("PATCH", "/v2/users/me", Some(&b.token), &body);
        assert_eq!((status, code(&value)), (400, expected), "{body}");
    }
    let (status, value) = exchange("PATCH", "/v2/users/me", Some(&pending.token), &json!({"displayName": "Mallory"}));
    assert_eq!((status, code(&value)), (403, "access_pending"));
    let (status, value) = exchange("GET", &format!("/v2/users/{}/avatar", alice.id), Some(&pending.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "access_pending"));
    // Another user's record or the users API stay the owner's.
    let (status, value) = exchange("PATCH", &format!("/v2/users/{}", bob.id), Some(&a.token), &json!({"name": "x"}));
    assert_eq!((status, code(&value)), (403, "owner_required"));

    // Capabilities and the owner's listing carry the profile.
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&a.token), &json!({}));
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "users-self-profile"));
    assert_eq!((&caps["user"]["displayName"], &caps["user"]["avatarEtag"]), (&json!("Alice"), &json!(alice_etag)));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &caps["user"]);
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&owner.token), &json!({}));
    assert_eq!(caps["user"]["displayName"], "Riley");
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&pending.token), &json!({}));
    assert!(caps["user"].get("displayName").is_none());
    let (status, listing) = exchange("GET", "/v2/users", Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    crate::management::v2::schema_export::assert_valid("users", "userList", &listing);
    let row = |id: &str| listing["users"].as_array().unwrap().iter().find(|u| u["id"] == id).unwrap().clone();
    assert_eq!((row(&alice.id)["name"].clone(), row(&alice.id)["displayName"].clone()), (json!("Alias A"), json!("Alice")));
    assert_eq!(row(&bob.id)["avatarEtag"], Value::Null);

    // Avatars: owner sees everyone, a user the owner, itself and users sharing an agent.
    let avatar_path = |id: &str| format!("/v2/users/{id}/avatar");
    let (status, head, bytes) = exchange_raw("GET", &avatar_path(&alice.id), Some(&owner.token), &json!({}), "");
    assert_eq!(status, 200);
    assert!(head.contains("Content-Type: image/png") && head.contains(&format!("ETag: \"{alice_etag}\"")), "{head}");
    assert_eq!(bytes, png(128));
    let (status, _, bytes) = exchange_raw("GET", &avatar_path(&alice.id), Some(&b.token), &json!({}), &format!("If-None-Match: \"{alice_etag}\"\r\n"));
    assert_eq!((status, bytes.len()), (304, 0), "shared agent → visible, ETag honoured");
    let (status, _, _) = exchange_raw("GET", &avatar_path(OWNER), Some(&c.token), &json!({}), "");
    assert_eq!(status, 200, "everyone sees the owner");
    let (status, value) = exchange("GET", &avatar_path(&alice.id), Some(&c.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "avatar_not_found"), "no shared agent → invisible");
    let (status, value) = exchange("GET", &avatar_path(&bob.id), Some(&a.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "avatar_not_found"), "visible but no avatar");

    // actingUser of a non-owner send carries the profile.
    crate::management::users::set_current_principal(Some(users.principal_for_token(&a.token_id)));
    let acting = crate::management::users::current_acting_user().unwrap();
    crate::management::users::set_current_principal(None);
    assert_eq!(acting, json!({"id": alice.id, "name": "Alias A", "displayName": "Alice", "avatarEtag": alice_etag}));
    crate::management::v2::schema_export::assert_valid("users", "actingUser", &acting);

    // Clearing; deleting a user drops its profile.
    let (status, me) = exchange("PATCH", "/v2/users/me", Some(&a.token), &json!({"avatar": null}));
    assert_eq!((status, &me["user"]["avatarEtag"]), (200, &Value::Null));
    users.bind_token(&c.token_id, None, "root").unwrap();
    let (status, _) = exchange("PATCH", "/v2/users/me", Some(&b.token), &json!({"displayName": "Bob"}));
    assert_eq!(status, 200);
    users.bind_token(&b.token_id, None, "root").unwrap();
    let (status, _) = exchange("DELETE", &format!("/v2/users/{}", bob.id), Some(&owner.token), &json!({}));
    assert_eq!(status, 200);
    let profiles = crate::management::user_profiles::ProfileStore::open_default().unwrap();
    assert!(profiles.snapshot().unwrap().profile(&bob.id).is_none());
}

/// user-own-token / user-own-devices: every connection sees its own token;
/// owner, root and members list / revoke only their own devices; pending
/// devices get 403 user_pending.
#[test]
fn own_token_and_own_devices_for_owner_member_pending_and_root() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let (owner, _) = tokens.issue_device("owner-desktop", "Owner PC", "windows").unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let (owner_phone, _) = tokens.issue_device("owner-phone", "Owner Phone", "android").unwrap();
    users.bind_token(&owner_phone.token_id, Some(OWNER), "root").unwrap();
    let alice = users.create_user("Alice", None, Grant::default(), "root").unwrap();
    let bob = users.create_user("Bob", None, Grant::default(), "root").unwrap();
    let (a1, _) = tokens.issue_device("alice-laptop", "Alice Laptop", "linux").unwrap();
    let a2 = tokens.issue("alice-cli").unwrap();
    let (b1, _) = tokens.issue_device("bob-laptop", "Bob Laptop", "macos").unwrap();
    let (pending, _) = tokens.issue_device("stranger", "Stranger PC", "windows").unwrap();
    users.bind_token(&a1.token_id, Some(&alice.id), "root").unwrap();
    users.bind_token(&a2.token_id, Some(&alice.id), "root").unwrap();
    users.bind_token(&b1.token_id, Some(&bob.id), "root").unwrap();
    tokens.rename(&a1.token_id, Some("Alice's work laptop")).unwrap();
    std::thread::sleep(Duration::from_millis(30));

    // C1: capabilities user.token, for every kind of connection.
    let caps = |bearer: &str| exchange("GET", "/v2/capabilities", Some(bearer), &json!({})).1;
    let value = caps(&a1.token);
    assert!(value["featureList"].as_array().unwrap().iter().any(|f| f == "user-own-token"));
    assert!(value["featureList"].as_array().unwrap().iter().any(|f| f == "user-own-devices"));
    assert_eq!(value["user"]["token"], json!({
        "id": a1.token_id, "label": "alice-laptop", "deviceName": "Alice Laptop",
        "ownerName": "Alice's work laptop", "platform": "linux",
    }));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);
    let value = caps(&a2.token);
    assert_eq!(value["user"]["token"], json!({
        "id": a2.token_id, "label": "alice-cli", "deviceName": null, "ownerName": null, "platform": null,
    }));
    let value = caps(&owner.token);
    assert_eq!((&value["user"]["isOwner"], &value["user"]["token"]["id"]), (&json!(true), &json!(owner.token_id)));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);
    let value = caps(&pending.token);
    assert_eq!((&value["user"]["pending"], &value["user"]["token"]["deviceName"]), (&json!(true), &json!("Stranger PC")));
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);
    let value = caps(ROOT);
    assert_eq!(value["user"]["token"], Value::Null, "root credential: no client token");
    crate::management::v2::schema_export::assert_valid("users", "capabilityUser", &value["user"]);

    // C2: listing — a member sees exactly its own tokens, current flagged.
    let devices = |bearer: &str| exchange("GET", "/v2/users/me/devices", Some(bearer), &json!({}));
    let ids = |value: &Value| {
        let mut ids: Vec<(String, bool)> = value["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d["tokenId"].as_str().unwrap().to_string(), d["current"].as_bool().unwrap()))
            .collect();
        ids.sort();
        ids
    };
    let sorted = |mut v: Vec<(String, bool)>| { v.sort(); v };
    let (status, value) = devices(&a1.token);
    assert_eq!(status, 200, "{value}");
    crate::management::v2::schema_export::assert_valid("users", "ownDevices", &value);
    assert_eq!(value["userId"], json!(alice.id));
    assert_eq!(ids(&value), sorted(vec![(a1.token_id.clone(), true), (a2.token_id.clone(), false)]));
    let item = value["devices"].as_array().unwrap().iter().find(|d| d["tokenId"] == a1.token_id).unwrap().clone();
    assert_eq!((&item["ownerName"], &item["deviceName"], &item["kind"], &item["pending"], &item["userId"]),
        (&json!("Alice's work laptop"), &json!("Alice Laptop"), &json!("device"), &json!(false), &json!(alice.id)));
    let (_, value) = devices(&b1.token);
    assert_eq!(ids(&value), vec![(b1.token_id.clone(), true)]);
    // Owner token and root: the owner user's devices.
    let (status, value) = devices(&owner.token);
    assert_eq!((status, &value["userId"]), (200, &json!(OWNER)));
    assert_eq!(ids(&value), sorted(vec![(owner.token_id.clone(), true), (owner_phone.token_id.clone(), false)]));
    let (status, value) = devices(ROOT);
    assert_eq!(status, 200);
    assert_eq!(ids(&value), sorted(vec![(owner.token_id.clone(), false), (owner_phone.token_id.clone(), false)]));
    // Pending: 403 user_pending on both routes.
    let (status, value) = devices(&pending.token);
    assert_eq!((status, code(&value)), (403, "user_pending"));
    let path = |id: &str| format!("/v2/users/me/devices/{id}");
    let (status, value) = exchange("DELETE", &path(&pending.token_id), Some(&pending.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "user_pending"));

    // Removal: another user's / the owner's / an unknown token reads as 404.
    for target in [&b1.token_id, &owner.token_id, &pending.token_id, &"mct-doesnotexist".to_string()] {
        let (status, value) = exchange("DELETE", &path(target), Some(&a1.token), &json!({}));
        assert_eq!((status, code(&value)), (404, "client_token_not_found"), "{target}");
    }
    let (status, value) = exchange("DELETE", &path(&a2.token_id), Some(&owner.token), &json!({}));
    assert_eq!((status, code(&value)), (404, "client_token_not_found"), "the owner's me = the owner user only");
    // Other methods stay closed for members.
    let (status, value) = exchange("POST", "/v2/users/me/devices", Some(&a1.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    let (status, value) = exchange("GET", &path(&a2.token_id), Some(&a1.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    let (status, value) = exchange("POST", "/v2/users/me/devices", Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (405, "method_not_allowed"));

    // A member removes its other device, then itself (= signing out).
    let (status, value) = exchange("DELETE", &path(&a2.token_id), Some(&a1.token), &json!({}));
    assert_eq!(status, 200, "{value}");
    assert_eq!(value, json!({"tokenId": a2.token_id, "revoked": true, "current": false}));
    crate::management::v2::schema_export::assert_valid("users", "ownDeviceRevoked", &value);
    assert!(tokens.list().unwrap().iter().all(|t| t.token_id != a2.token_id));
    let (status, _) = devices(&a2.token);
    assert_eq!(status, 401, "a revoked token is refused");
    let (status, value) = exchange("DELETE", &path(&a1.token_id), Some(&a1.token), &json!({}));
    assert_eq!((status, &value["current"]), (200, &json!(true)));
    std::thread::sleep(Duration::from_millis(30));
    let (status, _) = devices(&a1.token);
    assert_eq!(status, 401, "self-removal signs the device out");
    // The owner removes its own phone; root sees the rest.
    let (status, _) = exchange("DELETE", &path(&owner_phone.token_id), Some(ROOT), &json!({}));
    assert_eq!(status, 200);
    let (_, value) = devices(&owner.token);
    assert_eq!(ids(&value), vec![(owner.token_id.clone(), true)]);
    // Bob and the pending device were untouched.
    let remaining: Vec<String> = tokens.list().unwrap().into_iter().map(|t| t.token_id).collect();
    assert!(remaining.contains(&b1.token_id) && remaining.contains(&pending.token_id));
}

/// Person identity through the dispatcher: challenge / claim for every
/// principal (pending included), auto-binding, owner-only key management
/// and the decorated listing.
#[test]
fn person_identity_claims_and_owner_key_management() {
    use base64::Engine;
    use ring::signature::KeyPair;
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let person = |seed: u8| ring::signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap();
    let (me, colleague) = (person(11), person(12));
    let public = |pair: &ring::signature::Ed25519KeyPair| b64(pair.public_key().as_ref());
    let challenge = |bearer: &str| {
        let (status, value) = exchange("GET", "/v2/identity/challenge", Some(bearer), &json!({}));
        assert_eq!(status, 200, "{value}");
        crate::management::v2::schema_export::assert_valid("users", "identityChallenge", &value);
        value
    };
    let signed = |pair: &ring::signature::Ed25519KeyPair, challenge: &Value| {
        let message = crate::management::persons::signing_message(
            challenge["hostId"].as_str().unwrap(),
            challenge["nonce"].as_str().unwrap(),
            challenge["tokenId"].as_str().unwrap(),
        );
        json!({"publicKey": public(pair), "signature": b64(pair.sign(message.as_bytes()).as_ref()), "nonce": challenge["nonce"]})
    };

    let owner_device = tokens.issue("owner-desktop").unwrap();
    users.snapshot().unwrap();
    // The local desktop (root credential) records its person on owner.
    let c = challenge(ROOT);
    assert_eq!(c["tokenId"], "root");
    let (status, result) = exchange("POST", "/v2/identity/claim", Some(ROOT), &signed(&me, &c));
    assert_eq!((status, &result["outcome"]), (200, &json!("ownerKeyAdded")), "{result}");
    crate::management::v2::schema_export::assert_valid("users", "identityClaimResult", &result);
    // Each nonce works once.
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(ROOT), &signed(&me, &c));
    assert_eq!((status, code(&value)), (400, "challenge_invalid"));

    // A pending device of the same person: bound to owner at once.
    let laptop = tokens.issue("laptop").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(users.principal_for_token(&laptop.token_id), crate::management::users::Principal::Pending);
    let c = challenge(&laptop.token);
    assert_eq!(c["tokenId"], laptop.token_id.as_str());
    // A wrong signature spends the nonce and changes nothing.
    let mut forged = signed(&colleague, &c);
    forged["publicKey"] = json!(public(&me));
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &forged);
    assert_eq!((status, code(&value)), (400, "invalid_signature"));
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &signed(&me, &c));
    assert_eq!((status, code(&value)), (400, "challenge_invalid"));
    // A claim signed for another token does not verify for this caller.
    let c = challenge(&laptop.token);
    let mut other_token = c.clone();
    other_token["tokenId"] = json!(owner_device.token_id);
    let (status, value) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &signed(&me, &other_token));
    assert_eq!((status, code(&value)), (400, "invalid_signature"));
    // Without `nonce` the caller's live nonces are tried.
    let c = challenge(&laptop.token);
    let mut body = signed(&me, &c);
    body.as_object_mut().unwrap().remove("nonce");
    let (status, result) = exchange("POST", "/v2/identity/claim", Some(&laptop.token), &body);
    assert_eq!(status, 200, "{result}");
    assert_eq!((&result["outcome"], &result["userId"], &result["pending"]), (&json!("autoBound"), &json!(OWNER), &json!(false)));
    std::thread::sleep(Duration::from_millis(30));
    let (status, _) = exchange("GET", "/v2/users", Some(&laptop.token), &json!({}));
    assert_eq!(status, 200, "an auto-bound owner device has owner rights");

    // An unknown person on a pending device stays pending, with its name.
    let guest = tokens.issue("guest").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let c = challenge(&guest.token);
    let mut body = signed(&colleague, &c);
    body["displayName"] = json!("Colleague");
    let (status, result) = exchange("POST", "/v2/identity/claim", Some(&guest.token), &body);
    assert_eq!((status, &result["outcome"], &result["pending"]), (200, &json!("pending"), &json!(true)), "{result}");
    let (status, value) = exchange("GET", "/v2/settings", Some(&guest.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "access_pending"));
    // Key management is the owner's.
    let user = users.create_user("Colleague", None, Grant::default(), "root").unwrap();
    let keys_path = format!("/v2/users/{}/person-keys", user.id);
    for bearer in [&guest.token] {
        let (status, value) = exchange("PUT", &keys_path, Some(bearer), &json!({"personKeys": []}));
        assert_eq!((status, code(&value)), (403, "access_pending"));
    }
    let (status, value) = exchange("PUT", &keys_path, Some(&laptop.token), &json!({"personKeys": [{"publicKey": public(&me)}]}));
    assert_eq!((status, code(&value)), (409, "person_key_in_use"));
    let (status, value) = exchange("PUT", &keys_path, Some(&laptop.token), &json!({"personKeys": [{"publicKey": public(&colleague), "label": "Colleague"}]}));
    assert_eq!(status, 200, "{value}");
    crate::management::v2::schema_export::assert_valid("users", "personKeysResult", &value);
    let (status, _) = exchange("PATCH", "/v2/users/owner/person-keys", Some(&laptop.token), &json!({}));
    assert_eq!(status, 405);
    // The colleague's next claim on that pending device binds it.
    let c = challenge(&guest.token);
    let (_, result) = exchange("POST", "/v2/identity/claim", Some(&guest.token), &signed(&colleague, &c));
    assert_eq!((&result["outcome"], &result["userId"]), (&json!("autoBound"), &json!(user.id)));
    // A bound device presenting another person is only recorded.
    let c = challenge(&guest.token);
    let (_, result) = exchange("POST", "/v2/identity/claim", Some(&guest.token), &signed(&me, &c));
    assert_eq!((&result["outcome"], &result["userId"], &result["personUserId"]), (&json!("recorded"), &json!(user.id), &json!(OWNER)));

    // The owner's listing.
    let (status, listing) = exchange("GET", "/v2/users", Some(&laptop.token), &json!({}));
    assert_eq!(status, 200);
    crate::management::v2::schema_export::assert_valid("users", "userList", &listing);
    let row = |id: &str| listing["users"].as_array().unwrap().iter().find(|u| u["id"] == id).unwrap().clone();
    assert_eq!(row(OWNER)["personKeys"][0]["publicKey"], public(&me));
    assert_eq!(row(&user.id)["personKeys"][0]["label"], "Colleague");
    assert_eq!(listing["tokenPersons"][&laptop.token_id], public(&me));
    let token = listing["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == guest.token_id.as_str()).unwrap();
    assert_eq!(token["person"]["publicKey"], public(&me));
    assert_eq!(token["person"]["previousPublicKey"], public(&colleague));
    assert_eq!(token["person"]["displayName"], "Colleague");
    assert!(!listing.to_string().contains("cxm1."), "no secrets in the listing");
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&guest.token), &json!({}));
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "person-identity"));
    // Other methods on the identity routes are not for non-owners.
    let (status, value) = exchange("DELETE", "/v2/identity/claim", Some(&guest.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));

    // Acknowledging the change (owner only) clears previousPublicKey.
    let ack = format!("/v2/users/token-persons/{}/previous", guest.token_id);
    let (status, value) = exchange("DELETE", &ack, Some(&guest.token), &json!({}));
    assert_eq!((status, code(&value)), (403, "owner_required"));
    let (status, value) = exchange("DELETE", &ack, Some(&laptop.token), &json!({}));
    assert_eq!((status, &value["cleared"]), (200, &json!(true)), "{value}");
    crate::management::v2::schema_export::assert_valid("users", "tokenPersonPreviousCleared", &value);
    let (_, listing) = exchange("GET", "/v2/users", Some(ROOT), &json!({}));
    crate::management::v2::schema_export::assert_valid("users", "userList", &listing);
    let token = listing["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == guest.token_id.as_str()).unwrap();
    assert_eq!((&token["person"]["previousPublicKey"], &token["person"]["publicKey"]), (&Value::Null, &json!(public(&me))));
    assert_eq!(users.principal_for_token(&guest.token_id), crate::management::users::Principal::User {
        id: user.id.clone(), name: "Colleague".into(), grant: Grant::default(),
    });
    let (status, value) = exchange("DELETE", "/v2/users/token-persons/mct-missing/previous", Some(ROOT), &json!({}));
    assert_eq!((status, code(&value)), (404, "client_token_not_found"));
}

/// A context whose session lookups work on the private instance (empty
/// registry, no runtime); user input stops at a marker error.
fn p2_context() -> ManagementRequestContext {
    let mut context = context();
    context.load_registry = || Ok(Default::default());
    context.load_runtime_status = |_| Ok(None);
    context.handle_user_input = |_| {
        Err(crate::management::v2::user_input::UserInputExecutionError {
            stage: "route".into(),
            code: "test_user_input_reached".into(),
            message: "the access gate let the input through".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        })
    };
    context
}

fn p2_exchange(method: &str, path: &str, bearer: &str, body: &Value) -> (u16, Value) {
    let (status, _, body) = exchange_with(p2_context(), method, path, Some(bearer), body.to_string().into_bytes(), "application/json", "");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// P2 access (capability users-p2-access): read-only workspace files of
/// visible agents, own attachments as images, delegation commands on
/// visible asks.
#[test]
fn p2_users_read_workspaces_send_own_images_and_act_on_visible_asks() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let instance = PrivateInstance::new();
    let work = instance.root.join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(instance.root.join("outside-secret.txt"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(instance.root.join("outside-secret.txt"), work.join("escape.txt")).unwrap();
    let has_git = std::process::Command::new("git").args(["init", "-q"]).current_dir(&work).status().is_ok_and(|s| s.success());
    let host = crate::platform::host::current_host_name();
    let mut store = crate::session::model::CutexSessionStore::default();
    for (id, cwd, exposed) in [
        ("cutex.p2-solo", work.clone(), true),
        ("cutex.p2-other", work.clone(), true),
        ("cutex.p2-broad", instance.root.clone(), true),
        ("cutex.p2-unexposed", work.clone(), false),
    ] {
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.to_string(), None, host.clone(), cwd.to_string_lossy().into_owned(), None,
        )
        .unwrap();
        record.exposed_to_backend = exposed;
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        store.sessions.insert(id.to_string(), record);
    }
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    users.snapshot().unwrap();
    let visible = vec!["cutex.p2-solo".to_string(), "cutex.p2-broad".to_string(), "cutex.p2-unexposed".to_string()];
    let reader = users.create_user("Reader", None, Grant { standalone_agents: visible.clone(), ..Grant::default() }, "root").unwrap();
    let sender = users.create_user("Sender", None, Grant { standalone_agents: visible.clone(), send: true, ..Grant::default() }, "root").unwrap();
    let read_token = tokens.issue("p2-reader").unwrap();
    let send_token = tokens.issue("p2-sender").unwrap();
    let send_token_2 = tokens.issue("p2-sender-phone").unwrap();
    users.bind_token(&read_token.token_id, Some(&reader.id), "root").unwrap();
    users.bind_token(&send_token.token_id, Some(&sender.id), "root").unwrap();
    users.bind_token(&send_token_2.token_id, Some(&sender.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let (_, caps) = exchange("GET", "/v2/capabilities", Some(&read_token.token), &json!({}));
    assert!(caps["featureList"].as_array().unwrap().iter().any(|f| f == "users-p2-access"));

    // (a) Read-only files of a visible agent's cwd, read grant suffices.
    let (status, dir) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/dir", &read_token.token, &json!({}));
    assert_eq!(status, 200, "{dir}");
    assert!(dir["entries"].as_array().unwrap().iter().any(|e| e["name"] == "src"), "{dir}");
    let (status, file) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/file?path=src/main.rs", &read_token.token, &json!({}));
    assert_eq!((status, &file["content"]), (200, &json!("fn main() {}\n")), "{file}");
    let (status, _) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/search?q=main", &read_token.token, &json!({}));
    assert_eq!(status, 200);
    let (status, git) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/status", &read_token.token, &json!({}));
    assert_eq!(status, 200, "{git}");
    for (path, expected) in [
        ("/v2/sessions/cutex.p2-solo/fs/file?path=../outside-secret.txt", (403, "path_outside_root")),
        ("/v2/sessions/cutex.p2-solo/fs/file?path=src/../../outside-secret.txt", (403, "path_outside_root")),
        ("/v2/sessions/cutex.p2-broad/fs/dir", (403, "workspace_root_too_broad")),
        ("/v2/sessions/cutex.p2-broad/fs/file?path=.cutex/config.json", (403, "workspace_root_too_broad")),
        ("/v2/sessions/cutex.p2-other/fs/dir", (404, "session_not_found")),
        ("/v2/projects/p-not-granted/fs/dir", (404, "project_not_found")),
    ] {
        let (status, value) = p2_exchange("GET", path, &read_token.token, &json!({}));
        assert_eq!((status, code(&value)), expected, "{path}");
    }
    // A granted agent with the retired IM exposure flag off is visible like any other.
    let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-unexposed/fs/dir", &read_token.token, &json!({}));
    assert_eq!(status, 200, "{value}");
    #[cfg(unix)]
    {
        let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/fs/file?path=escape.txt", &read_token.token, &json!({}));
        assert_eq!((status, code(&value)), (403, "path_outside_root"), "symlink escape");
    }
    if has_git {
        let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/diff?path=../outside-secret.txt", &read_token.token, &json!({}));
        assert_eq!((status, code(&value)), (403, "path_outside_root"));
        let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/diff?path=src/main.rs", &read_token.token, &json!({}));
        assert_eq!(status, 200, "{value}");
        assert!(value.to_string().contains("fn main"), "untracked file diffed against /dev/null: {value}");
        #[cfg(unix)]
        {
            let (status, value) = p2_exchange("GET", "/v2/sessions/cutex.p2-solo/git/diff?path=escape.txt", &read_token.token, &json!({}));
            assert_eq!(status, 200, "{value}");
            assert!(!value.to_string().contains("secret"), "an untracked symlink out of the root is not diffed: {value}");
        }
    }
    // Writes, terminals and host-wide files stay the owner's.
    for (method, path) in [
        ("POST", "/v2/sessions/cutex.p2-solo/terminals"),
        ("GET", "/v2/sessions/cutex.p2-solo/terminals"),
        ("POST", "/v2/sessions/cutex.p2-solo/fs/file"),
        ("GET", "/v2/fs/file?path=/etc/hostname"),
        ("GET", "/v2/fs/dirs"),
    ] {
        let (status, value) = p2_exchange(method, path, &send_token.token, &json!({}));
        assert_eq!((status, code(&value)), (403, "owner_required"), "{method} {path}");
    }
    // The owner keeps the broad root.
    let (status, _) = p2_exchange("GET", "/v2/sessions/cutex.p2-broad/fs/dir", ROOT, &json!({}));
    assert_eq!(status, 200);

    // (b) Image inputs: own uploads only.
    let upload = |bearer: &str, session: &str| {
        let (status, _, body) = exchange_with(
            p2_context(), "POST", &format!("/v2/sessions/{session}/attachments"), Some(bearer), png(8), "image/png", "",
        );
        (status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null))
    };
    let (status, value) = upload(&read_token.token, "cutex.p2-solo");
    assert_eq!((status, code(&value)), (403, "send_not_granted"));
    let (status, value) = upload(&send_token.token, "cutex.p2-other");
    assert_eq!((status, code(&value)), (404, "session_not_found"));
    let (status, attachment) = upload(&send_token.token, "cutex.p2-solo");
    assert_eq!(status, 201, "{attachment}");
    let own_path = attachment["path"].as_str().unwrap().to_string();
    assert!(crate::management::v2::attachments::uploaded_by("cutex.p2-solo", &send_token.token_id, &own_path));
    assert!(!crate::management::v2::attachments::uploaded_by("cutex.p2-solo", &send_token_2.token_id, &own_path));
    let submit = |path: &str| json!({
        "requestId": "r-img", "method": "cutex/userInput/submit",
        "params": {"clientUserMessageId": "m-img", "origin": {"kind": "backend", "clientId": "c"}, "strategy": "queue",
            "input": [{"type": "text", "text": "look"}, {"type": "localImage", "path": path}]},
    });
    let requests = "/v2/sessions/cutex.p2-solo/cutex/requests";
    let (status, value) = p2_exchange("POST", requests, &send_token.token, &submit(&own_path));
    // Past the gate: the handler answers (no runtime in this test).
    assert_ne!(status, 403, "{value}");
    assert!(matches!(code(&value), "session_offline" | "test_user_input_reached"), "{value}");
    for (bearer, path) in [
        (&send_token_2.token, own_path.as_str()),
        (&send_token.token, "/etc/hostname"),
        (&send_token.token, &format!("{own_path}.by-x") as &str),
    ] {
        let (status, value) = p2_exchange("POST", requests, bearer, &submit(path));
        assert_eq!((status, code(&value)), (403, "attachment_not_owned"), "{path}");
    }
    let (status, value) = p2_exchange("POST", requests, &send_token.token, &json!({
        "requestId": "r-url", "method": "cutex/userInput/submit",
        "params": {"clientUserMessageId": "m-url", "origin": {"kind": "backend", "clientId": "c"}, "strategy": "queue",
            "input": [{"type": "image", "url": "https://example.invalid/x.png"}]},
    }));
    assert_eq!((status, code(&value)), (403, "input_type_not_allowed"));

    // (c) Delegation commands on visible, related asks.
    let asks = crate::asks::store::AskStore::open_default().unwrap();
    let agent = |id: &str| crate::asks::model::Requester::Agent { cutex_session_id: id.into(), name: None };
    let mut new = crate::asks::store::tests::new_ask("p2-visible", "please");
    new.requester = agent("cutex.p2-solo");
    new.target_cutex_session_id = "cutex.p2-broad".into();
    let (visible_ask, _) = asks.with_state(|state| state.create_ask(new, chrono::Utc::now())).unwrap().unwrap();
    let mut new = crate::asks::store::tests::new_ask("p2-hidden", "secret");
    new.requester = agent("cutex.p2-solo");
    new.target_cutex_session_id = "cutex.p2-other".into();
    let (hidden_ask, _) = asks.with_state(|state| state.create_ask(new, chrono::Utc::now())).unwrap().unwrap();
    let command = |method: &str, params: Value| json!({"requestId": format!("r-{method}"), "method": method, "params": params});
    for (bearer, session, body, expected) in [
        (&read_token.token, "cutex.p2-solo", command("cutex/ask/stop", json!({"askId": visible_ask.ask_id})), (403, "send_not_granted")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/stop", json!({"askId": hidden_ask.ask_id})), (404, "ask_not_found")),
        (&send_token.token, "cutex.p2-other", command("cutex/ask/stop", json!({"askId": visible_ask.ask_id})), (404, "session_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/respond", json!({"questionId": "q-missing", "answers": {}})), (404, "question_not_found")),
        (&send_token.token, "cutex.p2-solo", command("cutex/ask/create", json!({"text": "x", "actionId": "a"})), (403, "owner_required")),
        (&send_token.token, "cutex.p2-solo", command("cutex/asks/pause", json!({})), (403, "owner_required")),
    ] {
        let (status, value) = p2_exchange("POST", &format!("/v2/sessions/{session}/cutex/requests"), bearer, &body);
        assert_eq!((status, code(&value)), expected, "{body}");
    }
    // A visible ask through either end's path reaches the handler.
    let (status, value) = p2_exchange(
        "POST", "/v2/sessions/cutex.p2-broad/cutex/requests", &send_token.token,
        &command("cutex/ask/stop", json!({"askId": visible_ask.ask_id})),
    );
    assert_eq!(status, 200, "{value}");
    assert_eq!(value.pointer("/cutex/result/ask/askId"), Some(&json!(visible_ask.ask_id)), "{value}");
    assert_eq!(value.pointer("/cutex/result/ask/closed"), Some(&json!("stopped")), "{value}");
    let state = asks.snapshot().unwrap();
    assert!(state.asks[&hidden_ask.ask_id].closed.is_none(), "the refused ask is untouched");
}

/// Safety grants (capability `claude-safety-grants`): only the owner decides.
/// Non-owner users (read or send grant) are refused by the gate, a caller
/// without the root credential (an agent, any other process) by the route.
#[test]
fn safety_check_decisions_and_list_are_the_owners_only() {
    let _home_lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let mut store = crate::session::model::CutexSessionStore::default();
    let mut record = crate::session::model::CutexSessionRecord::new(
        "cutex.solo".to_string(), None, "test-host".to_string(), "/tmp".to_string(), None,
    )
    .unwrap();
    record.exposed_to_backend = true;
    store.sessions.insert("cutex.solo".to_string(), record);
    crate::session::store::save_cutex_session_store(&store).unwrap();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let grant = Grant { standalone_agents: vec!["cutex.solo".into()], send: true, ..Grant::default() };
    let sender = users.create_user("Sender", None, grant.clone(), "root").unwrap();
    let reader = users.create_user("Reader", None, Grant { send: false, ..grant }, "root").unwrap();
    let send_token = tokens.issue("sender").unwrap();
    let read_token = tokens.issue("reader").unwrap();
    users.bind_token(&send_token.token_id, Some(&sender.id), "root").unwrap();
    users.bind_token(&read_token.token_id, Some(&reader.id), "root").unwrap();
    std::thread::sleep(Duration::from_millis(30));

    let path = "/v2/sessions/cutex.solo/cutex/requests";
    for method in ["cutex/safetyCheck/grant", "cutex/safetyCheck/dismiss"] {
        let body = json!({"requestId": "r-1", "method": method, "params": {"blockId": "sb-1"}});
        for token in [send_token.token.as_str(), read_token.token.as_str()] {
            let (status, value) = exchange("POST", path, Some(token), &body);
            assert_eq!((status, code(&value)), (403, "owner_required"), "{method}");
        }
        // No root credential (what an agent or a bridge caller has): refused
        // before any state is read.
        for bearer in [None, Some("agent-bus-token-is-not-a-management-credential")] {
            let (status, value) = exchange("POST", path, bearer, &body);
            assert_eq!((status, code(&value)), (403, "cutex_method_denied"), "{method} {bearer:?}");
        }
    }
    for token in [send_token.token.as_str(), read_token.token.as_str()] {
        let (status, value) = exchange("GET", "/v2/safety-checks", Some(token), &Value::Null);
        assert_eq!((status, code(&value)), (403, "owner_required"));
    }
    for bearer in [None, Some("agent-bus-token-is-not-a-management-credential")] {
        let (status, _) = exchange("GET", "/v2/safety-checks", bearer, &Value::Null);
        assert_eq!(status, 401, "{bearer:?}");
    }
    let (status, value) = exchange("GET", "/v2/safety-checks?state=all", Some(ROOT), &Value::Null);
    assert_eq!((status, &value["blocks"]), (200, &json!([])), "{value}");
}
