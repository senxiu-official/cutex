use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};

use cutex::{
    management::server::handle_management_request,
    profiles::pinned_selection::{CODE, UPGRADE_CODE},
    session::{
        model::{CutexSessionRecord, CutexSessionRuntimeBackend},
        store::{cutex_sessions_path, shared_session_row, with_locked_session_rows},
    },
};
use serde_json::{json, Value};

use crate::cli_app::test_home::IsolatedTestHome;

fn request(id: &str, method: &str, params: Value, request_id: &str) -> (u16, Value) {
    const ROOT: &str = "pinned-selection-private-root";
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let body = json!({"requestId": request_id, "method": method, "params": params});
    let body = serde_json::to_vec(&body).unwrap();
    let headers = format!(
        "POST /v2/sessions/{id}/cutex/requests HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {ROOT}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (mut stream, _) = listener.accept().unwrap();
    let handled = handle_management_request(
        &mut stream,
        None,
        Some(ROOT),
        super::management_request_context(),
    );
    drop(stream);
    handled.unwrap();
    let response = client.join().unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

#[test]
fn pinned_selection_conflicts_through_owner_online_and_defaults_http() {
    let home = IsolatedTestHome::new_private("pinned-selection-http").unwrap();
    let id = "cutex.pinned-selection-http";
    let adapter = home.root().join("adapter");
    let state = home.root().join("state");
    fs::create_dir(&state).unwrap();
    let assets = adapter.join("src/cute_claude");
    fs::create_dir_all(&assets).unwrap();
    fs::write(
        assets.join("model_catalog.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "updatedAt": "2026-11-24",
            "sourceUrls": [],
            "models": [{
                "id": "sonnet",
                "displayName": "Sonnet fixture",
                "description": "Fixture model for the launch selection contract",
                "kind": "alias",
                "supportedEffortLevels": ["low", "high"]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut record = CutexSessionRecord::new(
        id.into(),
        None,
        cutex::platform::host::current_host_name(),
        home.root().to_string_lossy().into_owned(),
        None,
    )
    .unwrap();
    record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::Persistent;
    record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
    record.runtime_generation = 7;
    record.claude_launch_selection_pending = true;
    record.model_defaults = Some("sonnet".into());
    record.reasoning_defaults = Some("max".into());
    record.claude_session = Some(
        serde_json::from_value(json!({
            "deployment": {
                "python": home.root().join("unused-python"),
                "adapter_source": adapter,
                "claude_cli": home.root().join("unused-cli"),
                "claude_home": home.root().join("account"),
                "tui_binary": home.root().join("unused-tui")
            },
            "state_dir": state, "thread_id": uuid::Uuid::new_v4().to_string(),
            "native_session_id": uuid::Uuid::new_v4().to_string(), "model": "sonnet", "effort": "max"
        }))
        .unwrap(),
    );
    with_locked_session_rows(&cutex_sessions_path().unwrap(), |rows| {
        rows.insert_session(id, record);
        rows.commit()?;
        Ok(())
    })
    .unwrap();
    let read = || shared_session_row(id).unwrap().unwrap();
    let before = read();
    let generation = before.runtime_generation;
    let revision = before.durable_revision();
    let online = |request_id| {
        request(
            id,
            "cutex/runtime/online",
            json!({"expectedRuntimeGeneration": generation}),
            request_id,
        )
    };
    let update = |request_id, effort| {
        request(
            id,
            "cutex/session/defaults/update",
            json!({"expectedRevision": revision, "patch": {"reasoningEffort": effort}}),
            request_id,
        )
    };
    let expect_conflict = |(status, body): (u16, Value), code: &str, details: Value| {
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["contractVersion"], 2);
        assert_eq!(body["error"]["source"], "cutex");
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["retryable"], false);
        assert_eq!(body["error"]["details"], details);
        assert_eq!(read().as_ref(), before.as_ref());
    };

    let capability = json!({"requiredCapability":"offlineLaunchSelection"});
    expect_conflict(online("launch-old-pin"), UPGRADE_CODE, capability.clone());
    expect_conflict(update("update-old-pin", "low"), UPGRADE_CODE, capability);

    fs::write(
        assets.join("features.json"),
        br#"{"features":{"offlineLaunchSelection":true}}"#,
    )
    .unwrap();
    let unavailable = json!({
        "model":"sonnet", "reasoningEffort":"max",
        "reason":"effortUnavailable", "offeredReasoningEfforts":["low","high"]
    });
    expect_conflict(online("launch-rejected-effort"), CODE, unavailable.clone());
    expect_conflict(update("update-rejected-effort", "max"), CODE, unavailable);

    let (status, body) = update("save-offered-effort", "low");
    assert_eq!(status, 200, "{body}");
    let saved = read();
    let binding = saved.claude_session.as_ref().unwrap();
    let original = before.claude_session.as_ref().unwrap();
    assert_eq!(saved.reasoning_defaults.as_deref(), Some("low"));
    assert_eq!(binding.effort.unwrap().as_str(), "low");
    assert_eq!(binding.thread_id, original.thread_id);
    assert_eq!(binding.native_session_id, original.native_session_id);
    assert!(saved.claude_launch_selection_pending);
    assert_eq!(saved.runtime_generation, generation);
    assert!(saved.runtime_pid.is_none());
}
