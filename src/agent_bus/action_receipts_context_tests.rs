use super::*;
use crate::agent_management::{AgentActionRecord, AgentManagementSnapshot};

const CALLER: &str = "cutex.00000000-0000-0000-0000-0000000000a1";
const CHILD: &str = "cutex.00000000-0000-0000-0000-0000000000a2";

fn fixture() -> (AgentManagementSnapshot, Query) {
    let record: AgentActionRecord = serde_json::from_value(json!({
        "action_id":"partial-create", "request_sha256":"0".repeat(64),
        "operation":"create", "project_id":"p", "caller_cutex_session":CALLER,
        "phase":"configured", "phase_sequence":6,
        "known_native_session_id":"captured-native", "known_successor_cutex_session":CHILD,
        "reserved_agent_cwd":"private-cwd-not-for-receipt",
        "created_at":"2026-10-26T00:00:00Z", "updated_at":"2026-10-26T00:00:01Z"
    })).unwrap();
    let mut snapshot = AgentManagementSnapshot::empty();
    snapshot.actions.insert(record.action_id.clone(), record);
    for (id, action, project, operation) in [
        ("failure-one", "partial-create", "p", "create"),
        ("failure-two", "partial-create", "p", "create"),
        ("other-action", "other", "p", "create"),
        ("other-project", "partial-create", "other", "create"),
        ("other-operation", "partial-create", "p", "online"),
    ] {
        snapshot.failure_events.insert(id.into(), serde_json::from_value(json!({
            "schema":"cutex/agent-management-failure/v1", "event_id":id,
            "action_id":action, "project_id":project, "operation":operation,
            "code":"owner_action_required", "detail":"private observation diagnostic",
            "routing_status":"unrouted", "created_at":"2026-10-26T00:00:02Z"
        })).unwrap());
    }
    (snapshot, Query {tool:"agent_create".into(), action_id:"partial-create".into()})
}

#[test]
fn unfinished_creation_exposes_only_scoped_durable_context_without_effects() {
    let (snapshot, q) = fixture();
    let before = serde_json::to_value(&snapshot).unwrap();
    let cache: SharedCache = Default::default();
    let schema = crate::management::v2::schema_export::validator("tool-action-receipt", "receipt").unwrap();
    for _ in 0..3 {
        let value = query_in(&cache, CALLER, &q, || management_query_with(&snapshot, CALLER, &q,
            |_| panic!("unfinished creation must not query a child")));
        assert_eq!(value["status"], "unknown");
        assert_eq!(value["outcomeUnknown"], true);
        assert!(valid_response(&value, &q.tool, &q.action_id));
        assert!(schema.is_valid(&value), "{value}");
        assert_eq!(value["result"]["durableContext"], json!({
            "phase":"configured", "phaseSequence":6,
            "knownSuccessorCutexSession":CHILD, "knownNativeSessionId":"captured-native",
            "failureCount":2,"failures":[
                {"eventId":"failure-one", "code":"owner_action_required", "createdAt":"2026-10-26T00:00:02Z"},
                {"eventId":"failure-two", "code":"owner_action_required", "createdAt":"2026-10-26T00:00:02Z"}
            ]
        }));
        assert!(!value.to_string().contains("private"));
        assert!(value.get("retryAdmission").is_none());
    }
    assert_eq!(serde_json::to_value(&snapshot).unwrap(), before);
    assert!(cache.lock().unwrap().rows.is_empty(), "read cannot admit a worker");
    assert_eq!(management_query_with(&snapshot, "new-director", &q, |_| panic!("wrong caller"))
        .unwrap(), envelope(&q, "notFound", true, None));
}

#[test]
fn unfinished_creation_without_identity_or_failure_stays_unknown() {
    let (mut snapshot, q) = fixture();
    snapshot.failure_events.clear();
    let first = snapshot.actions.keys().next().unwrap().clone();
    let action = snapshot.actions.get_mut(&first).unwrap();
    action.phase = crate::agent_management::AgentActionPhase::Prepared;
    action.phase_sequence = 0;
    action.known_native_session_id = None;
    action.known_successor_cutex_session = None;
    let value = management_query_with(&snapshot, CALLER, &q, |_| panic!("no child lookup")).unwrap();
    assert_eq!(value["status"], "unknown");
    assert_eq!(value["outcomeUnknown"], true);
    assert_eq!(value["result"]["durableContext"]["phase"], "prepared");
    assert_eq!(value["result"]["durableContext"]["knownNativeSessionId"], Value::Null);
    assert_eq!(value["result"]["durableContext"]["failures"], json!([]));
}

#[test]
fn completed_reply_is_unchanged_even_with_historical_failures() {
    let (mut snapshot, q) = fixture();
    let full = json!({"ok":true,"created":true,"agent":{"cutex_session_id":CHILD}});
    snapshot.extra.insert(RECEIPTS_EXTENSION.into(), json!({q.action_id.clone():{
        "caller":CALLER,"request_sha256":"0".repeat(64),"result":full
    }}));
    assert_eq!(management_query_with(&snapshot, CALLER, &q, |_| panic!("completed reply"))
        .unwrap(), completed(&q, full));
}

#[test]
fn cached_enriched_unknown_refreshes_without_becoming_completed_and_refs_are_bounded() {
    let (mut snapshot, q) = fixture();
    let failure = snapshot.failure_events["failure-one"].clone();
    for i in 0..30 {
        let mut row = failure.clone();
        row.event_id = format!("extra-{i:02}");
        snapshot.failure_events.insert(row.event_id.clone(), row);
    }
    let cache: SharedCache = Default::default();
    cache.lock().unwrap().rows.insert(key(CALLER, &q), Running {
        digest:"digest".into(), generation:1, result:Some(envelope(&q,"unknown",true,None))
    });
    for _ in 0..3 {
        let value = query_in(&cache, CALLER, &q, || management_query_with(&snapshot, CALLER, &q,
            |_| panic!("no child lookup")));
        assert_eq!(value["status"], "unknown");
        assert_eq!(value["outcomeUnknown"], true);
        assert_eq!(completed(&q, value["result"].clone()), value);
        assert_eq!(value["result"]["durableContext"]["failureCount"], 32);
        assert_eq!(value["result"]["durableContext"]["failures"].as_array().unwrap().len(), 16);
        assert_eq!(cache.lock().unwrap().rows[&key(CALLER, &q)].result.as_ref(), Some(&value));
    }
}

#[test]
fn audit_observation_failure_has_allowlisted_reason_without_sensitive_tail() {
    let (mut snapshot, q) = fixture();
    for (code, detail, reason) in [
        ("owner_action_required", "agent_bus_observation_failed: private endpoint/path timeout (external outcome unknown)", true),
        ("owner_action_required", "agent_bus_observation_failed_extra: private tail", false),
        ("owner_action_required", "private error mentioning agent_bus_observation_failed: tail", false),
        ("owner_action_required", "unrecognized_failure: private tail", false),
        ("different_code", "agent_bus_observation_failed: private tail", false),
    ] {
        let failure = snapshot.failure_events.get_mut("failure-one").unwrap();
        failure.code = code.into();
        failure.detail = detail.into();
        let value = management_query_with(&snapshot, CALLER, &q, |_| panic!("no child lookup")).unwrap();
        let reference = &value["result"]["durableContext"]["failures"][0];
        assert_eq!(reference.get("reasonCode"), reason.then_some(&json!("agent_bus_observation_failed")));
        assert!(!value.to_string().contains("private"));
        assert_eq!(value["status"], "unknown");
        assert_eq!(value["outcomeUnknown"], true);
        assert_eq!(value["result"]["durableContext"]["phase"], "configured");
    }
}

/// r276 (EVA 2026-11-04): a create whose runtime start failed on an agent bus
/// timeout says how to finish it; the stored detail carries the class prefix.
#[test]
fn a_failed_runtime_start_names_its_reason_and_the_exact_retry() {
    for (detail, reason, retry) in [
        ("owner_action_required: agent_bus_unavailable: Failed to read agent bus response (external outcome unknown)", Some("agent_bus_unavailable"), true),
        ("owner_action_required: session_online_failed: native start incomplete; private tail", Some("runtime_start_failed"), true),
        ("owner_action_required: agent_bus_observation_failed: private tail", Some("agent_bus_observation_failed"), false),
        ("owner_action_required: unrecognized_failure: private tail", None, false),
    ] {
        let (mut snapshot, q) = fixture();
        snapshot.failure_events.remove("failure-two");
        let failure = snapshot.failure_events.get_mut("failure-one").unwrap();
        failure.detail = detail.into();
        let value = management_query_with(&snapshot, CALLER, &q, |_| panic!("no child lookup")).unwrap();
        let context = &value["result"]["durableContext"];
        assert_eq!(context["failures"][0].get("reasonCode"), reason.map(|r| json!(r)).as_ref(), "{detail}");
        assert_eq!(context["failures"][0].get("retry").is_some(), retry);
        assert_eq!(context.get("retry").is_some(), retry);
        assert_eq!(context.get("hint").is_some(), retry);
        if retry {
            assert_eq!(context["retry"], "exact");
            assert!(context["hint"].as_str().unwrap().contains("same agent_create"));
        }
        assert!(!value.to_string().contains("private"));
        assert_eq!(value["status"], "unknown");
    }
}
