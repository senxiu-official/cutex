use super::*;
use crate::agent_bus::native_subagent as child;

#[test]
fn native_child_http_reads_require_management_proof_and_current_generation() {
    use crate::config::instance::{set_test_selection, Selection};
    let _home = crate::config::paths::test_home_env_lock();
    let previous = crate::config::instance::selected().unwrap();
    let path = std::env::temp_dir().join(format!("native-child-http-{}", uuid::Uuid::new_v4()));
    set_test_selection(Selection::Custom(path.clone()));
    let session = "cutex.018e0000-0000-7000-8000-0000b0009ae1";
    let root = "018e0000-0000-7000-8000-0000b0009ae4";
    let runtime = "child-parent-runtime";
    let mut record = active_route_session(session, root, runtime, 1, 7);
    record.registration_class = AgentRegistrationClass::Persistent;
    let mut sessions = CutexSessionStore::default();
    sessions.sessions.insert(session.into(), record);
    crate::session::store::save_cutex_session_store(&sessions).unwrap();
    let state = route_state();
    let mut roster = active_route_roster(runtime, root);
    roster.cutex_session_id = Some(session.into());
    state.lock().unwrap().agents.insert(runtime.into(), roster);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = crate::config::store::load_codez_config();
    config.management_port = Some(port);
    config.management_api_token = Some(crate::profiles::model::ManagementApiToken::new(
        "child-proof-root",
    ));
    crate::config::store::save_codez_config(&config).unwrap();
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = stopped.clone();
    let proofs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = proofs.clone();
    let management = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let request =
                        crate::http::server::read_simple_http_request(&mut stream).unwrap();
                    assert_eq!(request.path, child::PROOF_PATH);
                    assert_eq!(
                        request.headers.get("authorization").map(String::as_str),
                        Some("Bearer child-proof-root")
                    );
                    let principal: child::ChildPrincipal =
                        serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(principal.parent_session_id, session);
                    assert_eq!(principal.generation, 7);
                    count.fetch_add(1, Ordering::SeqCst);
                    let verified = matches!(
                        principal.child_id.as_str(),
                        "child" | "nested-child" | "swapped"
                    );
                    let mut principal = principal;
                    if principal.child_id == "swapped" {
                        principal.generation += 1;
                    }
                    write_json_response(
                        &mut stream,
                        200,
                        "OK",
                        &serde_json::json!({"verified":verified,"principal":principal}),
                    )
                    .unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(2))
                }
                Err(error) => panic!("proof listener: {error}"),
            }
        }
    });
    let token = crate::agent_bus::agent_auth::agent_credential("route-token", runtime, session);
    let call_with_token = |credential: &str, route: &str, child_id: Option<&str>, generation: u64, tool: &str| {
        let header = child_id
            .map(|id| format!("X-Cutex-Mcp-Child-Id: {id}\r\n"))
            .unwrap_or_default();
        let mut body = serde_json::json!({"tool":tool,"arguments":{}});
        if child_id.is_none() {
            body["action_id"] = serde_json::json!("parent-read-test");
        }
        let body = body.to_string();
        let raw=format!("POST {route} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {credential}\r\nX-Cutex-Agent-Id: {runtime}\r\nX-Cutex-Mcp-Thread-Id: {root}\r\nX-Cutex-Mcp-Generation: {generation}\r\n{header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
        invoke_route(raw.into_bytes(), state.clone())
    };
    let call = |route: &str, child_id: Option<&str>, generation: u64, tool: &str| {
        call_with_token(&token,route,child_id,generation,tool)
    };
    // Agent credentials keep their exact-route restriction. A legacy root
    // bridge request that passes admission must still enter child dispatch.

    let query_route=format!("{}?fixture=1",child::PATH);
    let denied=call(&query_route,Some("child"),7,"agent_list");
    assert!(denied.starts_with(b"HTTP/1.1 403 "));
    let value=http_json(&call_with_token("route-token",&query_route,Some("child"),7,"agent_list"));
    assert_eq!(value["ok"],true,"{value}");
    let value=http_json(&call_with_token("route-token",&query_route,Some("forged"),7,"agent_list"));
    assert_eq!(value["code"],"native_child_unverified");
    let before_query_write=proofs.load(Ordering::SeqCst);
    let value=http_json(&call_with_token("route-token",&query_route,Some("child"),7,"agent_create"));
    assert_eq!(value["code"],"native_subagent_read_only");
    assert_eq!(proofs.load(Ordering::SeqCst),before_query_write);
    for id in ["child", "nested-child"] {
        let response = call(child::PATH, Some(id), 7, "agent_list");
        let value = http_json(&response);
        assert_eq!(value["ok"], true, "{}", String::from_utf8_lossy(&response));
    }
    for tool in [
        "artifact_list",
        "service_list",
        "owner_pins",
        "agent_name_history",
    ] {
        let value = http_json(&call(child::PATH, Some("child"), 7, tool));
        assert_ne!(value["code"], "native_child_unverified", "{tool}: {value}");
        assert_ne!(
            value["code"], "native_subagent_read_only",
            "{tool}: {value}"
        );
    }
    let value = http_json(&call(child::PATH, Some("forged"), 7, "agent_list"));
    assert_eq!(value["code"], "native_child_unverified");
    let value = http_json(&call(child::PATH, Some("swapped"), 7, "agent_list"));
    assert_eq!(value["code"], "native_child_unverified");
    let before = proofs.load(Ordering::SeqCst);
    let value = http_json(&call(child::PATH, Some("child"), 6, "agent_list"));
    assert_eq!(value["code"], "unauthorized");
    for tool in [
        "agent_tell",
        "agent_create",
        "agent_resume",
        "service_register",
        "artifact_publish",
    ] {
        let value = http_json(&call(child::PATH, Some("child"), 7, tool));
        assert_eq!(value["code"], "native_subagent_read_only");
    }
    assert_eq!(
        proofs.load(Ordering::SeqCst),
        before,
        "management tools must not enter proof or dispatch"
    );
    let response = call(
        "/api/agent-management/v1/actions",
        Some("child"),
        7,
        "agent_create",
    );
    assert!(response.starts_with(b"HTTP/1.1 403 "));
    assert_eq!(http_json(&response)["detail"], child::REFUSAL);
    let parent = http_json(&call(crate::asks::tools::PATH, None, 7, "agent_list"));
    assert_eq!(parent["ok"], true, "{parent}");
    assert_eq!(
        proofs.load(Ordering::SeqCst),
        before,
        "parent must bypass child proof"
    );
    stopped.store(true, Ordering::SeqCst);
    management.join().unwrap();
    set_test_selection(previous);
}
