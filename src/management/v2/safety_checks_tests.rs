use super::*;
use crate::owner_notify::store::State;

const AGENT: &str = "cutex.agent-a";

fn temp() -> (OwnerNotifyStore, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("cutex-safety-checks-{}", uuid::Uuid::new_v4().simple()));
    (OwnerNotifyStore::at(dir.join("owner-notify")), dir)
}

fn wire(block_id: &str, state: &str) -> Value {
    json!({
        "blockId": block_id, "threadId": "thread-1", "turnId": "turn-1", "itemId": "call-1",
        "toolName": "Bash", "toolInput": {"command": "rm -rf $S/$v", "description": "clean"},
        "command": "rm -rf $S/$v", "reason": "Dangerous rm operation on possibly-empty variable path: $S/$v",
        "source": {"kind": "agent", "id": "cutex.director", "name": "director"},
        "state": state, "blockedAt": "2026-10-16T01:00:00.000Z", "decidedAt": null,
        "decidedBy": null, "expiresAt": null, "usedAt": null, "grantSeconds": 1800,
    })
}

fn observe(store: &OwnerNotifyStore, method: &str, block: Value) -> bool {
    let settings = OwnerNotifySettings::default();
    let evaluator = Evaluator::with_owner(&settings);
    store
        .with_state(|state| {
            apply_notification(state, AGENT, Some("Builder".into()), None, method, &json!({"threadId": "thread-1", "block": block}),
                &settings, &evaluator, Utc::now())
        })
        .unwrap()
}

fn request(method: &str, params: Value) -> ValidatedCutexRequest {
    ValidatedCutexRequest { request_id: "req-1".into(), method: method.into(), params }
}

fn record(store: &OwnerNotifyStore, block_id: &str) -> OwnerNotification {
    store.snapshot().unwrap().safety_record(AGENT, block_id).cloned().unwrap()
}

type Calls = std::cell::RefCell<Vec<(String, Value)>>;

fn runtime_ok<'a>(calls: &'a Calls, state: &'a str) -> impl FnOnce(&SafetyBlock, &str, Value) -> Result<Value, RuntimeCallError> + 'a {
    move |_, method, params| {
        calls.borrow_mut().push((method.to_string(), params.clone()));
        let mut block = wire(params["blockId"].as_str().unwrap(), state);
        block["decidedAt"] = json!("2026-10-16T01:05:00.000Z");
        block["expiresAt"] = json!("2026-10-16T01:35:00.000Z");
        Ok(json!({ "block": block }))
    }
}

fn never_runtime(_: &SafetyBlock, _: &str, _: Value) -> Result<Value, RuntimeCallError> {
    unreachable!("the runtime must not be called")
}

fn never_submit(_: &ValidatedCutexRequest) -> Result<Value, UserInputExecutionError> {
    unreachable!("no owner input expected")
}

#[test]
fn a_blocked_check_becomes_a_needs_you_notice_once() {
    let (store, dir) = temp();
    assert!(observe(&store, BLOCKED_NOTIFICATION, wire("sb-1", "blocked")));
    // Two observing processes may both see it: one record.
    assert!(!observe(&store, BLOCKED_NOTIFICATION, wire("sb-1", "blocked")));
    let state = store.snapshot().unwrap();
    assert_eq!(state.notifications.len(), 1);
    let notice = record(&store, "sb-1");
    assert_eq!(notice.system_kind.as_deref(), Some("safetyBlock"));
    assert_eq!(notice.notification_event.as_deref(), Some("system.safetyBlock"));
    assert_eq!(notice.agent_name.as_deref(), Some("Builder"));
    assert!(notice.pending(), "counts in 需要你 by default: {notice:?}");
    assert!(!notice.needs_reply && !notice.is_final());
    let block = notice.safety_block.as_ref().unwrap();
    assert_eq!((block.tool_name.as_str(), block.command.as_str(), block.state.as_str()), ("Bash", "rm -rf $S/$v", "blocked"));
    assert_eq!(block.tool_input["command"], "rm -rf $S/$v");
    assert!(notice.text.contains("rm -rf $S/$v") && notice.text.contains("possibly-empty"));
    // The live event carries the block.
    let created = state.notices.iter().find(|n| n.method == crate::owner_notify::CREATED_EVENT).unwrap();
    assert_eq!(created.params["notification"]["safetyBlock"]["blockId"], "sb-1");
    assert_eq!(state.notices.iter().filter(|n| n.method == crate::owner_notify::CREATED_EVENT).count(), 1);
    // Reading the agent does not decide for the owner.
    store.with_state(|state| state.mark_agent_read(AGENT, Utc::now())).unwrap();
    assert!(record(&store, "sb-1").pending());
    assert_eq!(list(&store.snapshot().unwrap(), "open", None, 10).len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn only_the_root_credential_decides() {
    let (store, dir) = temp();
    observe(&store, BLOCKED_NOTIFICATION, wire("sb-1", "blocked"));
    let session = json!({"runtime": {"activeTurnId": null}});
    for method in METHODS {
        let denied = dispatch_with(false, &store, AGENT, &session, &request(method, json!({"blockId": "sb-1"})),
            never_runtime, never_submit).unwrap_err();
        assert_eq!(denied.code, "cutex_method_denied", "{method}");
    }
    assert!(record(&store, "sb-1").pending());
    // Another agent's session cannot reach it either.
    let other = dispatch_with(true, &store, "cutex.agent-b", &session, &request(GRANT_METHOD, json!({"blockId": "sb-1"})),
        never_runtime, never_submit).unwrap_err();
    assert_eq!(other.code, "safety_block_not_found");
    let extra = dispatch_with(true, &store, AGENT, &session, &request(GRANT_METHOD, json!({"blockId": "sb-1", "x": 1})),
        never_runtime, never_submit).unwrap_err();
    assert_eq!(extra.code, "invalid_request");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn grant_reaches_the_runtime_closes_the_item_and_tells_the_agent_once() {
    let (store, dir) = temp();
    observe(&store, BLOCKED_NOTIFICATION, wire("sb-1", "blocked"));
    let session = json!({"runtime": {"activeTurnId": "turn-9"}});
    let calls = Calls::default();
    let mut submitted = None;
    let result = dispatch_with(true, &store, AGENT, &session, &request(GRANT_METHOD, json!({"blockId": "sb-1"})),
        runtime_ok(&calls, "granted"),
        |input| {
            submitted = Some(input.clone());
            Ok(json!({"disposition": "queued"}))
        })
        .unwrap();
    let calls = calls.into_inner();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "thread/safetyCheck/grant");
    assert_eq!(calls[0].1, json!({"threadId": "thread-1", "blockId": "sb-1", "decidedBy": {"kind": "owner"}}));
    assert_eq!(result["block"]["state"], "granted");
    assert_eq!(result["submit"]["disposition"], "queued");
    let input = submitted.unwrap();
    assert_eq!(input.method, "cutex/userInput/submit");
    assert_eq!(input.params["strategy"], "queue", "busy: after the current turn");
    assert_eq!(input.params["clientUserMessageId"], "safety-grant-sb-1");
    assert_eq!(input.params["input"][0]["text"], "主人已批准一次：rm -rf $S/$v（30 分钟内有效，仅此一次）");
    let notice = record(&store, "sb-1");
    assert!(!notice.pending() && notice.is_final());
    assert_eq!(notice.state, State::Acknowledged);
    assert_eq!(notice.safety_block.as_ref().unwrap().expires_at.as_deref(), Some("2026-10-16T01:35:00.000Z"));
    // A second grant never reaches the runtime.
    let again = dispatch_with(true, &store, AGENT, &session, &request(GRANT_METHOD, json!({"blockId": "sb-1"})),
        never_runtime, never_submit).unwrap_err();
    assert_eq!(again.code, "safety_block_closed");
    // The runtime reports the grant used: the record follows.
    let mut used = wire("sb-1", "used");
    used["usedAt"] = json!("2026-10-16T01:10:00.000Z");
    assert!(observe(&store, UPDATED_NOTIFICATION, used));
    assert_eq!(record(&store, "sb-1").safety_block.unwrap().state, "used");
    assert!(list(&store.snapshot().unwrap(), "open", None, 10).is_empty());
    assert_eq!(list(&store.snapshot().unwrap(), "all", Some(AGENT), 10).len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dismiss_closes_without_a_grant_even_when_the_runtime_is_offline() {
    let (store, dir) = temp();
    observe(&store, BLOCKED_NOTIFICATION, wire("sb-1", "blocked"));
    let session = json!({"runtime": {"activeTurnId": null}});
    let result = dispatch_with(true, &store, AGENT, &session, &request(DISMISS_METHOD, json!({"blockId": "sb-1"})),
        |_, method, _| {
            assert_eq!(method, "thread/safetyCheck/dismiss");
            Err(RuntimeCallError::Offline)
        },
        never_submit)
        .unwrap();
    assert_eq!(result["block"]["state"], "dismissed");
    assert!(result.get("submit").is_none());
    assert!(!record(&store, "sb-1").pending());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn grant_needs_a_running_runtime_that_supports_it() {
    let (store, dir) = temp();
    observe(&store, BLOCKED_NOTIFICATION, wire("sb-1", "blocked"));
    let session = json!({"runtime": {"activeTurnId": null}});
    let offline = dispatch_with(true, &store, AGENT, &session, &request(GRANT_METHOD, json!({"blockId": "sb-1"})),
        |_, _, _| Err(RuntimeCallError::Offline), never_submit).unwrap_err();
    assert_eq!(offline.code, "session_offline");
    let old = dispatch_with(true, &store, AGENT, &session, &request(GRANT_METHOD, json!({"blockId": "sb-1"})),
        |_, _, _| Err(RuntimeCallError::Unsupported), never_submit).unwrap_err();
    assert_eq!(old.code, "safety_grants_unsupported");
    assert!(record(&store, "sb-1").pending(), "still open after failed grants");
    // The attached TUI decided first: take the runtime's state.
    let raced = dispatch_with(true, &store, AGENT, &session, &request(GRANT_METHOD, json!({"blockId": "sb-1"})),
        |_, _, _| Err(RuntimeCallError::Refused {
            code: "safety_block_closed".into(), message: "safety block already dismissed".into(),
            data: json!({"code": "safety_block_closed", "block": wire("sb-1", "dismissed")}),
        }),
        never_submit).unwrap_err();
    assert_eq!(raced.code, "safety_block_closed");
    assert_eq!(record(&store, "sb-1").safety_block.unwrap().state, "dismissed");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn grant_text_uses_the_grant_window() {
    let block = block_from_wire("claude", &wire("sb-1", "granted")).unwrap();
    assert_eq!(grant_text(&block), "主人已批准一次：rm -rf $S/$v（30 分钟内有效，仅此一次）");
    assert!(block_from_wire("claude", &json!({"blockId": "x"})).is_none());
    assert!(block_from_wire("claude", &wire("sb-1", "weird")).is_none());
}

#[test]
fn a_reconnecting_observer_catches_up_from_the_runtime_list() {
    let (store, dir) = temp();
    observe(&store, BLOCKED_NOTIFICATION, wire("sb-1", "blocked"));
    let settings = OwnerNotifySettings::default();
    let evaluator = Evaluator::with_owner(&settings);
    // Newest first: sb-3 waits (missed while nobody observed), sb-2 was decided
    // before cutex ever saw it (not recorded), sb-1 was granted meanwhile.
    let mut other_thread = wire("sb-4", "blocked");
    other_thread["threadId"] = json!("thread-9");
    let listed = vec![wire("sb-3", "blocked"), wire("sb-2", "dismissed"), wire("sb-1", "granted"), other_thread];
    let changed = store
        .with_state(|state| apply_snapshot(state, AGENT, None, "thread-1", &listed, &settings, &evaluator, Utc::now()))
        .unwrap();
    assert!(changed);
    let state = store.snapshot().unwrap();
    assert!(state.safety_record(AGENT, "sb-3").unwrap().pending());
    assert!(state.safety_record(AGENT, "sb-2").is_none());
    assert!(state.safety_record(AGENT, "sb-4").is_none());
    assert_eq!(state.safety_record(AGENT, "sb-1").unwrap().safety_block.as_ref().unwrap().state, "granted");
    // Applying the same list again changes nothing.
    assert!(!store
        .with_state(|state| apply_snapshot(state, AGENT, None, "thread-1", &listed, &settings, &evaluator, Utc::now()))
        .unwrap());
    let _ = std::fs::remove_dir_all(dir);
}
