// Real ephemeral TCP + production Bus auth/fence/preflight; no live daemon.
use super::*;
use fs2::FileExt;
use serde_json::json;

#[test]
fn slow_tool_preflight_lock_refusal_keeps_http_auth_and_query_read_only() {
    use crate::config::instance::{Selection, set_test_selection};
    let _home = crate::config::paths::test_home_env_lock();
    let previous = crate::config::instance::selected().unwrap();
    let root = std::env::temp_dir().join(format!("rp-{}", uuid::Uuid::new_v4()));
    set_test_selection(Selection::Custom(root));
    let caller = "cutex.preflight-caller";
    let runtime = "preflight-runtime";
    let thread = "018e0000-0000-7000-8000-0000b0009ae4";
    let mut record = active_route_session(caller, thread, runtime, 1, 7);
    record.registration_class = AgentRegistrationClass::Persistent;
    let mut sessions = CutexSessionStore::default();
    sessions.sessions.insert(caller.into(), record);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let state = route_state();
    let mut roster = active_route_roster(runtime, thread);
    roster.registration_class = AgentRegistrationClass::Persistent;
    state.lock().unwrap().agents.insert(runtime.into(), roster);
    let credential = crate::agent_bus::agent_auth::agent_credential("route-token", runtime, caller);
    let store = crate::asks::store::AskStore::open_default().unwrap();
    let before = serde_json::to_value(store.snapshot().unwrap()).unwrap();
    let management = crate::agent_management::AgentManagementStore::open_default().unwrap();
    let management_before = serde_json::to_value(management.snapshot().unwrap()).unwrap();
    let file = std::fs::OpenOptions::new().read(true).write(true).open(store.dir().join("asks-v1.lock")).unwrap();
    file.lock_exclusive().unwrap();
    let call = |path: &str, token: &str, native: &str, generation: u64, body: Value| {
        let body = body.to_string();
        let raw = format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nX-Cutex-Agent-Id: {runtime}\r\nX-Cutex-Mcp-Thread-Id: {native}\r\nX-Cutex-Mcp-Generation: {generation}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        invoke_route(raw.into_bytes(), Arc::clone(&state))
    };
    for tool in ["agent_delegate", "agent_create"] {
        let body = json!({"tool":tool,"arguments":{"target":"Kimi","action_id":"http-prelock"}});
        let at = std::time::Instant::now();
        let response = call(crate::asks::short_ids::PATH, &credential, thread, 7, body.clone());
        let elapsed = at.elapsed();
        assert!(response.starts_with(b"HTTP/1.1 200 "), "{}", String::from_utf8_lossy(&response));
        let value = http_json(&response);
        assert_eq!(value["code"], "id_resolution_busy", "{value}");
        assert_eq!(value["status"], "no_write");
        assert_eq!(value["action_id"], "http-prelock");
        assert!(elapsed >= std::time::Duration::from_secs(2) && elapsed < std::time::Duration::from_secs(4), "{elapsed:?}");
        println!("authenticated {tool} real HTTP preflight under held OS lock: {elapsed:?} no_write");
        for (native, generation) in [(thread, 6), ("018e0000-0000-7000-8000-0000b0009ae5", 7)] {
            let rejected = http_json(&call(crate::asks::short_ids::PATH, &credential, native, generation, body.clone()));
            assert_eq!(rejected["code"], "unauthorized", "{rejected}");
        }
        let wrong = crate::agent_bus::agent_auth::agent_credential("route-token", runtime, "cutex.other-caller");
        let denied = call(crate::asks::short_ids::PATH, &wrong, thread, 7, body);
        assert!(!String::from_utf8_lossy(&denied).contains("id_resolution_busy"), "credential must fail before ID lookup");
    }
    drop(file);
    assert_eq!(serde_json::to_value(store.snapshot().unwrap()).unwrap(), before);
    for tool in ["agent_delegate", "agent_create"] {
        for _ in 0..3 {
            let response = call(crate::asks::tools::PATH, &credential, thread, 7,
                json!({"tool":"action_status","action_id":"query-only","arguments":{"tool":tool,"action_id":"http-prelock"}}));
            let value = http_json(&response);
            assert_eq!(value["status"], "notFound", "{value}");
            assert_eq!(value["outcomeUnknown"], true);
        }
    }
    assert_eq!(serde_json::to_value(store.snapshot().unwrap()).unwrap(), before, "queries must not reserve or run work");
    assert_eq!(serde_json::to_value(management.snapshot().unwrap()).unwrap(), management_before,
        "create queries must not reserve or run Agent Management work");
    set_test_selection(previous);
}
