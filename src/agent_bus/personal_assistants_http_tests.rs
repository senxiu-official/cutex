use super::*;
use crate::personal_assistants::{self as pa, Config, Store};
use serde_json::json;

#[test]
fn personal_assistants_bus_requires_current_session_and_fences_generic_routes() {
    use crate::config::instance::{scoped_test_selection, Selection};
    let _home = crate::config::paths::test_home_env_lock();
    let root = std::env::temp_dir().join(format!("pa-bus-{}", uuid::Uuid::new_v4()));
    let _selection = scoped_test_selection(Selection::Custom(root));
    let caller = format!("cutex.{}", uuid::Uuid::new_v4());
    let runtime = "private-assistant-runtime";
    let thread = uuid::Uuid::new_v4().to_string();
    let mut record = active_route_session(&caller, &thread, runtime, 1, 7);
    record.registration_class = AgentRegistrationClass::Persistent;
    let mut sessions = CutexSessionStore::default();
    sessions.sessions.insert(caller.clone(), record);
    let marked = format!("cutex.{}", uuid::Uuid::new_v4());
    let mut marked_record = active_route_session(
        &marked,
        &uuid::Uuid::new_v4().to_string(),
        "marked-runtime",
        1,
        7,
    );
    marked_record.creation_action = Some("human-agent:personal-assistant-pa-fixture".into());
    sessions.sessions.insert(marked.clone(), marked_record);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let state = route_state();
    let mut roster = active_route_roster(runtime, &thread);
    roster.cutex_session_id = Some(caller.clone());
    roster.registration_class = AgentRegistrationClass::Persistent;
    state.lock().unwrap().agents.insert(runtime.into(), roster);
    let credential =
        crate::agent_bus::agent_auth::agent_credential("route-token", runtime, &caller);
    let call = |path: &str, token: &str, generation: u64, body: Value| {
        let body = body.to_string();
        let raw=format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nX-Cutex-Agent-Id: {runtime}\r\nX-Cutex-Mcp-Thread-Id: {thread}\r\nX-Cutex-Mcp-Generation: {generation}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
        invoke_route(raw.into_bytes(), Arc::clone(&state))
    };
    let args = json!({"tool":"pa_self","arguments":{}});
    let path = pa::tools::PATH;
    // An unreadable optional registry must leave an ordinary agent's tool
    // listing and authenticated read routes available. Marked sessions still
    // require a working registry before they can use assistant tools.
    let root = Store::default_root().unwrap();
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    let pending = rusqlite::Connection::open(root.join("assistants-v1.sqlite3")).unwrap();
    pending.pragma_update(None, "user_version", 99).unwrap();
    let generic = call(
        crate::agent_bus::agent_auth::BROWSER_FENCE_PATH,
        &credential,
        7,
        json!({}),
    );
    assert!(
        generic.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&generic)
    );
    let ordinary = crate::agent_bus::tool_policy::toolset_response(&caller, Default::default());
    assert!(ordinary["tools"]
        .as_array()
        .unwrap()
        .contains(&json!("agent_list")));
    assert_eq!(
        crate::agent_bus::tool_policy::toolset_response(&marked, Default::default())["tools"],
        json!([])
    );
    let unknown = format!("cutex.{}", uuid::Uuid::new_v4());
    assert_eq!(
        crate::agent_bus::tool_policy::toolset_response(&unknown, Default::default())["tools"],
        json!([])
    );
    pending.pragma_update(None, "user_version", 0).unwrap();
    drop(pending);
    assert!(call(path, &credential, 7, args.clone()).starts_with(b"HTTP/1.1 403"));
    let store = Store::open_default().unwrap();
    let row = store
        .begin(
            "owner",
            "new",
            Config {
                name: "Test".into(),
                profile: "private".into(),
                home_instance_id: crate::management::instance::instance_id(),
                project_ids: Default::default(),
            },
            |_| Ok(()),
        )
        .unwrap();
    store.bind(&row.id, &caller).unwrap();
    let accepted = call(path, &credential, 7, args.clone());
    assert!(
        accepted.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&accepted)
    );
    assert_eq!(http_json(&accepted)["result"]["id"], row.id);
    for (token, generation) in [("route-token", 7), (credential.as_str(), 6)] {
        assert!(call(path, token, generation, args.clone()).starts_with(b"HTTP/1.1 403"));
    }
    assert!(call(
        crate::asks::tools::PATH,
        &credential,
        7,
        json!({"tool":"agent_tell","arguments":{}})
    )
    .starts_with(b"HTTP/1.1 403"));
    assert!(call(
        path,
        &credential,
        7,
        json!({"tool":"director_request_list","arguments":{}})
    )
    .starts_with(b"HTTP/1.1 403"));
    let listed = crate::agent_bus::tool_policy::toolset_response(&caller, Default::default());
    assert!(listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| pa::tools::ASSISTANT.contains(&t.as_str().unwrap())));
    assert!(listed["tools"]
        .as_array()
        .unwrap()
        .contains(&json!("pa_self")));
    store
        .configure(
            "owner",
            &row.id,
            "disable",
            2,
            &pa::Update::Enabled { enabled: false },
            |_| Ok(()),
        )
        .unwrap();
    assert!(call(path, &credential, 7, args).starts_with(b"HTTP/1.1 403"));
    assert_eq!(
        crate::agent_bus::tool_policy::toolset_response(&caller, Default::default())["tools"],
        json!([])
    );
}

#[test]
fn personal_assistants_tool_lists_are_empty_when_both_identity_sources_are_unavailable() {
    use crate::config::instance::{scoped_test_selection, Selection};
    let _home = crate::config::paths::test_home_env_lock();
    let _selection = scoped_test_selection(Selection::Custom(
        std::env::temp_dir().join(format!("pa-tool-source-{}", uuid::Uuid::new_v4())),
    ));
    let session = format!("cutex.{}", uuid::Uuid::new_v4());
    assert_eq!(
        crate::agent_bus::tool_policy::toolset_response(&session, Default::default())["tools"],
        json!([])
    );
    // The fixture starts with unavailable session metadata and a future assistant schema.
    std::fs::create_dir_all(crate::session::store::cutex_sessions_path().unwrap()).unwrap();
    let root = Store::default_root().unwrap();
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    let db = rusqlite::Connection::open(root.join("assistants-v1.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    assert!(crate::session::store::shared_session_row(&session).is_err());
    assert!(Store::open_default().is_err());
    assert_eq!(
        crate::agent_bus::tool_policy::toolset_response(&session, Default::default())["tools"],
        json!([])
    );
    assert_eq!(
        crate::agent_bus::tool_policy::tools_list_for_session(&session)["tools"],
        json!([])
    );
}
