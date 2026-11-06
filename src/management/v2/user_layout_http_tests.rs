use super::*;
use crate::management::client_tokens::DeviceScope;
use crate::management::user_layout;
const PATH: &str = "/v2/users/me/layout";
const SIDEBAR: &str = "/v2/users/me/layout/sidebar";

#[test]
fn user_layout_dispatch_permissions_revisions_pointer_and_delete_user() {
    let _lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let alice = users
        .create_user("Alice", None, Grant::default(), "root")
        .unwrap();
    let bob = users
        .create_user("Bob", None, Grant::default(), "root")
        .unwrap();
    let read = tokens
        .issue_scoped("layout-read", DeviceScope::Read)
        .unwrap();
    let chat = tokens
        .issue_scoped("layout-chat", DeviceScope::Chat)
        .unwrap();
    let bob_token = tokens.issue("layout-bob").unwrap();
    let pending = tokens.issue("layout-pending").unwrap();
    users
        .bind_token(&read.token_id, Some(&alice.id), "root")
        .unwrap();
    users
        .bind_token(&chat.token_id, Some(&alice.id), "root")
        .unwrap();
    users
        .bind_token(&bob_token.token_id, Some(&bob.id), "root")
        .unwrap();
    assert_eq!(exchange("GET", PATH, None, &json!({})).0, 401);
    let caps = exchange("GET", "/v2/capabilities", Some(&chat.token), &json!({})).1;
    assert_eq!(caps["features"]["userLayout"], true);
    assert!(caps["featureList"]
        .as_array()
        .unwrap()
        .contains(&json!("user-layout")));
    assert!(
        !caps["featureList"]
            .as_array()
            .unwrap()
            .contains(&json!("user-layout-switch")),
        "phase 1b is not advertised"
    );
    for (method, path) in [
        ("GET", PATH),
        ("PUT", SIDEBAR),
        ("DELETE", SIDEBAR),
        ("PUT", "/v2/users/me/layout/pointer"),
    ] {
        let (status, v) = exchange(
            method,
            path,
            Some(&pending.token),
            &json!({"revision":0,"data":{}}),
        );
        assert_eq!((status, code(&v)), (403, "user_pending"), "{v}");
    }
    let (status, empty) = exchange("GET", PATH, Some(&read.token), &json!({}));
    assert_eq!(status, 200, "{empty}");
    assert_eq!(empty["docKey"], alice.id);
    crate::management::v2::schema_export::assert_valid("users", "layoutView", &empty);
    for method in ["PUT", "DELETE"] {
        let (status, v) = exchange(
            method,
            SIDEBAR,
            Some(&read.token),
            &json!({"revision":0,"data":{}}),
        );
        assert_eq!((status, code(&v)), (403, "device_scope_denied"));
        assert_eq!(v["error"]["details"]["required"], "chat");
    }
    let data = json!({"members":[{"host":"ci-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"agent","id":"a"}],"unknown":true});
    let put = json!({"revision":0,"data":data});
    let (status, first) = exchange("PUT", SIDEBAR, Some(&chat.token), &put);
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["revision"], 1);
    crate::management::v2::schema_export::assert_valid("users", "layoutMutation", &first);
    let (status, v) = exchange("PUT", SIDEBAR, Some(&chat.token), &put);
    assert_eq!((status, code(&v)), (409, "layout_revision_conflict"));
    assert_eq!(v["error"]["details"]["revision"], 1);
    assert_eq!(
        exchange(
            "PUT",
            SIDEBAR,
            Some(&chat.token),
            &json!({"revision":1,"data":data})
        ),
        (200, first)
    );
    assert_eq!(
        exchange("GET", PATH, Some(&read.token), &json!({})).1["sections"]["sidebar"]["data"],
        data
    );
    assert_eq!(
        exchange("GET", PATH, Some(&bob_token.token), &json!({})).1["sections"],
        json!({})
    );
    assert_eq!(
        exchange("GET", PATH, Some(ROOT), &json!({})).1["sections"],
        json!({})
    );
    for bearer in [&chat.token, ROOT] {
        let (status, v) = exchange(
            "PUT",
            &format!("{SIDEBAR}?as=owner"),
            Some(bearer),
            &json!({"revision":1,"data":{}}),
        );
        assert_eq!((status, code(&v)), (400, "layout_switch_unavailable"));
    }
    let pointer = json!({"memoryHost":"ci-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"host-a"});
    assert_eq!(
        exchange(
            "PUT",
            "/v2/users/me/layout/pointer",
            Some(&read.token),
            &pointer
        )
        .0,
        403
    );
    let (status, saved) = exchange(
        "PUT",
        "/v2/users/me/layout/pointer",
        Some(&chat.token),
        &pointer,
    );
    assert_eq!(status, 200, "{saved}");
    assert!(saved["setAt"].is_string());
    crate::management::v2::schema_export::assert_valid("users", "layoutPointer", &saved);
    let (_, view) = exchange("GET", PATH, Some(&chat.token), &json!({}));
    assert_eq!(view["pointer"], saved);
    assert_eq!(view["sections"]["sidebar"]["revision"], 1);
    let deleted = exchange("DELETE", SIDEBAR, Some(&chat.token), &json!({"revision":1}));
    assert_eq!(deleted.0, 200, "{deleted:?}");
    assert_eq!(deleted.1["revision"], 2);
    let (_, view) = exchange("GET", PATH, Some(&chat.token), &json!({}));
    assert_eq!(view["sections"]["sidebar"]["data"], Value::Null);
    assert_eq!(
        exchange("PUT", SIDEBAR, Some(&bob_token.token), &put).0,
        200
    );
    users.bind_token(&bob_token.token_id, None, "root").unwrap();
    let file = users.dir().join("layout").join(format!("{}.json", bob.id));
    assert!(file.exists());
    assert_eq!(
        exchange(
            "DELETE",
            &format!("/v2/users/{}", bob.id),
            Some(ROOT),
            &json!({})
        )
        .0,
        200
    );
    assert!(!file.exists());
}

#[test]
fn user_layout_events_only_own_document_and_noop_is_silent() {
    let _lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let a = users
        .create_user("A", None, Grant::default(), "root")
        .unwrap();
    let b = users
        .create_user("B", None, Grant::default(), "root")
        .unwrap();
    let ta = tokens.issue_scoped("a", DeviceScope::Chat).unwrap();
    let tb = tokens.issue_scoped("b", DeviceScope::Chat).unwrap();
    users.bind_token(&ta.token_id, Some(&a.id), "root").unwrap();
    users.bind_token(&tb.token_id, Some(&b.id), "root").unwrap();
    let repo = crate::management::v2::repository::management_v2_repository().unwrap();
    let checkpoint = repo.checkpoint().unwrap();
    for bearer in [&ta.token, &tb.token, ROOT] {
        assert_eq!(
            exchange(
                "PUT",
                SIDEBAR,
                Some(bearer),
                &json!({"revision":0,"data":{"x":1}})
            )
            .0,
            200
        );
    }
    assert_eq!(
        exchange(
            "PUT",
            SIDEBAR,
            Some(&ta.token),
            &json!({"revision":1,"data":{"x":1}})
        )
        .0,
        200
    );
    assert_eq!(host_events(&checkpoint, user_layout::CHANGED).len(), 3);
    let since = match &checkpoint.cursor {
        Some(cursor) => format!("streamId={}&after={cursor}", checkpoint.stream_id),
        None => format!("streamId={}", checkpoint.stream_id),
    };
    let path = format!("/v2/events?{since}&limit=1000");
    for (bearer, id) in [
        (ta.token.as_str(), a.id.as_str()),
        (tb.token.as_str(), b.id.as_str()),
        (ROOT, OWNER),
    ] {
        let (status, events) = exchange("GET", &path, Some(bearer), &json!({}));
        assert_eq!(status, 200, "{events}");
        let changed: Vec<_> = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["cutex"]["method"] == user_layout::CHANGED)
            .collect();
        assert_eq!(changed.len(), 1, "{events}");
        assert_eq!(changed[0]["cutex"]["params"]["docKey"], id);
    }
    // Exercise production SSE replay, not just the page predicate. The client
    // disconnects once its own event arrives; the server's watch then exits.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let target = a.id.clone();
    let bearer = ta.token.clone();
    let stream_path = path.replacen("/v2/events?", "/v2/events/stream?", 1);
    let client = std::thread::spawn(move || {
        let mut socket = TcpStream::connect(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(socket,"GET {stream_path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\n\r\n").unwrap();
        let mut out = vec![];
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&out);
            if text.contains(&format!("\"docKey\":\"{target}\"")) && text.ends_with("\n\n") {
                break;
            }
        }
        socket.shutdown(std::net::Shutdown::Both).unwrap();
        String::from_utf8(out).unwrap()
    });
    let (mut server, _) = listener.accept().unwrap();
    listeners::set_current_connection(Some(ConnectionInfo {
        remote: false,
        peer: "127.0.0.1:43210".parse().unwrap(),
    }));
    crate::http::server::set_keep_alive(crate::http::server::KeepAlive::Off);
    handle_management_request(&mut server, None, Some(ROOT), p2_context()).unwrap();
    listeners::set_current_connection(None);
    listeners::set_current_client_token(None);
    crate::management::users::set_current_principal(None);
    drop(server);
    let replay = client.join().unwrap();
    assert!(
        replay.contains("200 OK") && replay.contains(&a.id),
        "{replay}"
    );
    assert!(
        !replay.contains(&b.id),
        "foreign layout event leaked: {replay}"
    );
    assert!(
        !replay.contains("\"docKey\":\"owner\""),
        "owner layout event leaked: {replay}"
    );
}

#[test]
fn user_layout_owner_keys_split_without_changing_owner_rights() {
    use base64::Engine;
    let _lock = crate::config::paths::test_home_env_lock();
    let _instance = PrivateInstance::new();
    let tokens = ClientTokenStore::open_default().unwrap();
    let users = UserStore::open_default().unwrap();
    let a = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8; 32]);
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([2u8; 32]);
    users
        .set_person_keys(OWNER, vec![(a.clone(), None), (b.clone(), None)], "root")
        .unwrap();
    users.set_owner_person(&a, "root").unwrap();
    let ta = tokens.issue("owner-a").unwrap();
    let tb = tokens.issue("owner-b").unwrap();
    let no_key = tokens.issue("owner-no-key").unwrap();
    for t in [&ta, &tb, &no_key] {
        users.bind_token(&t.token_id, Some(OWNER), "root").unwrap();
    }
    // Synthetic persisted identity: routing still authenticates each real test token.
    let person_file = users.dir().join("persons-v1.json");
    let mut persons: Value = serde_json::from_slice(&std::fs::read(&person_file).unwrap()).unwrap();
    persons["tokenPersons"][&ta.token_id] = json!(a);
    persons["tokenPersons"][&tb.token_id] = json!(b);
    crate::config::atomic::write_private_pretty_json_atomic(&person_file, &persons, "test persons")
        .unwrap();
    let split = format!("owner~{}", crate::management::persons::fingerprint(&b));
    for (bearer, key) in [
        (ROOT, "owner"),
        (&ta.token, "owner"),
        (&tb.token, split.as_str()),
        (&no_key.token, "owner"),
    ] {
        let (status, v) = exchange("GET", PATH, Some(bearer), &json!({}));
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["docKey"], key);
        assert_eq!(v["ownerPersonConfirmed"], true);
        assert_eq!(
            exchange("GET", "/v2/users", Some(bearer), &json!({})).0,
            200,
            "owner keeps host rights"
        );
    }
    assert_eq!(
        exchange(
            "PUT",
            SIDEBAR,
            Some(&tb.token),
            &json!({"revision":0,"data":{"person":"b"}})
        )
        .0,
        200
    );
    assert!(users
        .dir()
        .join("layout")
        .join(format!("{}.json", split.replace('~', "--")))
        .exists());
    assert_eq!(
        exchange("GET", PATH, Some(ROOT), &json!({})).1["sections"],
        json!({})
    );
    users.clear_owner_person("root").unwrap();
    let (_, v) = exchange("GET", PATH, Some(&tb.token), &json!({}));
    assert_eq!(v["docKey"], "owner");
    assert_eq!(v["ownerPersonConfirmed"], false);
}
