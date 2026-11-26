//! `cutex/agentBus/wake` (agent messaging v2): the owner wakes a session so
//! it reads and handles the notes other agents left it. It submits one owner
//! input (`auto`, 023 Q2): an idle session starts a turn now, a busy one reads
//! it at its next step like typed input (older Claude adapters: after the
//! turn), an offline one is brought online first (as `startIfOffline`). Every
//! pending note is read into that turn's context.

use serde_json::json;
use serde_json::Value;

use super::server::ValidatedCutexRequest;
use super::user_input::UserInputExecutionError;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::management::server::ManagementRequestContext;

pub const METHOD: &str = "cutex/agentBus/wake";
pub const FEATURE: &str = "agent-bus-read-notes-v1";
/// The owner input the wake sends.
pub const WAKE_TEXT: &str = "Read and handle the unread notes other agents left for you.";
const CLIENT_ID: &str = "cutex-agent-bus-wake";
/// Private retry-payload metadata, never forwarded as user/native input.
pub const READ_NOTES_FIELD: &str = "_cutexReadNotes";

fn check_ordered_reservations(explicit: Option<&Value>, available: &[String], reserved: &[String]) -> Result<(), UserInputExecutionError> {
    let blocked = explicit.and_then(Value::as_array).map_or(available.is_empty() && !reserved.is_empty(), |values|
        values.iter().filter_map(Value::as_str).any(|id| reserved.iter().any(|item| item == id)));
    if blocked {
        return Err(super::input_ledger::route_error("ordered_batch_pending",
            "The selected note has an unconfirmed ordered batch receipt. Dismiss is still available.".into(), false));
    }
    Ok(())
}

fn note_ids(value: &Value) -> anyhow::Result<Vec<String>> {
    let ids: Vec<String> = serde_json::from_value(value.clone())?;
    anyhow::ensure!(ids.len() < super::input_ledger::MAX_BATCH_ITEMS
        && ids.iter().all(|id| !id.trim().is_empty() && id.len() <= 512)
        && ids.iter().collect::<std::collections::BTreeSet<_>>().len() == ids.len(),
        "inputIds must contain at most 99 distinct nonempty note ids");
    Ok(ids)
}

pub fn selected_note_ids(ledger: &super::input_ledger::InputLedger, session: &str, owner: &str) -> anyhow::Result<Option<Vec<String>>> {
    ledger.payload(session, owner)?.and_then(|payload| payload.get(READ_NOTES_FIELD).cloned())
        .map(|value| note_ids(&value)).transpose()
}

/// The first request freezes the set. Replay cannot absorb later notes or
/// change the set after its owner request may already have reached native.
fn capture_read_set(
    ledger: &super::input_ledger::InputLedger, session: &str, input: &ValidatedCutexRequest,
    explicit: Option<&Value>, pending: &[String],
) -> anyhow::Result<Vec<String>> {
    let owner = input.params["clientUserMessageId"].as_str().expect("wake owner id");
    if ledger.payload(session, owner)?.is_none() {
        let ids = match explicit { Some(value) => note_ids(value)?, None => pending.to_vec() };
        note_ids(&json!(ids))?;
        anyhow::ensure!(ids.iter().all(|id| pending.contains(id)), "selected input is not a pending note of this session");
        let mut payload = input.params.clone();
        // offline_submit consumes this routing flag before logical submit;
        // retaining it would make the normal owner retry parser reject it.
        payload.as_object_mut().expect("wake input params").remove("startIfOffline");
        payload[READ_NOTES_FIELD] = json!(ids);
        ledger.put_initial_payload(session, owner, &payload)?;
    }
    let ids = selected_note_ids(ledger, session, owner)?.unwrap_or_default();
    if let Some(value) = explicit {
        let mut requested = note_ids(value)?;
        let mut saved = ids.clone();
        requested.sort(); saved.sort();
        anyhow::ensure!(requested == saved, "read-now request identity already names another note set");
    }
    Ok(ids)
}

pub(super) fn dispatch(
    event_repository: &super::repository::EventRepository,
    context: ManagementRequestContext,
    owner: Option<HumanManagementPrincipal>,
    cutex_session_id: &str,
    session: &Value,
    request: &ValidatedCutexRequest,
) -> Result<Value, UserInputExecutionError> {
    let selected = std::cell::RefCell::new(Vec::new());
    let pending = || {
        super::agent_bus_state::agent_bus_message_repository()
            .and_then(|repository| repository.read_now_notes(cutex_session_id))
            .map_err(|error| UserInputExecutionError {
                stage: "route".into(),
                code: "event_persistence_unavailable".into(),
                message: format!("agent bus state unavailable: {error:#}"),
                retryable: true,
                details: json!({}),
                outcome_unknown: false,
            }).and_then(|(ids, reserved)| {
                check_ordered_reservations(request.params.get("inputIds"), &ids, &reserved)?;
                let count = ids.len();
                *selected.borrow_mut() = ids;
                Ok(count)
            })
    };
    dispatch_with(owner.is_some(), session, request, pending, |submit| {
        let count = if super::input_ledger::enabled() {
            Some(capture_read_set(super::input_ledger::default_ledger().map_err(|error|
                super::input_ledger::route_error("event_persistence_unavailable", error.to_string(), true))?,
                cutex_session_id, submit, request.params.get("inputIds"), &selected.borrow())
                .map_err(|error| super::input_ledger::route_error("read_notes_unavailable", error.to_string(), false))?.len())
        } else {
            if request.params.get("inputIds").is_some() {
                return Err(super::input_ledger::route_error("read_notes_unsupported", "selected note reads require the input ledger".into(), false));
            }
            None
        };
        let mut result = super::offline_submit::dispatch(event_repository, context, owner, cutex_session_id, session, submit)?;
        if let Some(count) = count { result["selectedNotes"] = json!(count); }
        Ok(result)
    })
}

pub(super) fn dispatch_with(
    root: bool,
    _session: &Value,
    request: &ValidatedCutexRequest,
    pending: impl FnOnce() -> Result<usize, UserInputExecutionError>,
    submit: impl FnOnce(&ValidatedCutexRequest) -> Result<Value, UserInputExecutionError>,
) -> Result<Value, UserInputExecutionError> {
    if !root {
        return Err(UserInputExecutionError {
            stage: "route".into(),
            code: "cutex_method_denied".into(),
            message: "cutex/agentBus/wake requires the Management root credential".into(),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        });
    }
    if !request.params.is_null() {
        let object = request.params.as_object().ok_or_else(|| super::server::invalid_user_input_error("wake params must be an object"))?;
        if object.keys().any(|key| key != "inputIds") {
            return Err(super::server::invalid_user_input_error("wake accepts only optional inputIds"));
        }
        if let Some(value) = object.get("inputIds") {
            note_ids(value).map_err(|error| super::server::invalid_user_input_error(&error.to_string()))?;
        }
    }
    let pending_notes = pending()?;
    // The owner's own words: `auto` like typed input (023 Q2). Busy: read at
    // the next step (Codex steer; Claude same-turn steering, else after the
    // turn on older adapters). Idle: now.
    let input = ValidatedCutexRequest {
        request_id: request.request_id.clone(),
        method: "cutex/userInput/submit".into(),
        params: json!({
            "clientUserMessageId": format!("agent-bus-wake-{}", request.request_id),
            "origin": {"kind": "other", "clientId": CLIENT_ID},
            "strategy": "auto",
            "input": [{"type": "text", "text": WAKE_TEXT, "text_elements": []}],
            "startIfOffline": true,
        }),
    };
    let submitted = submit(&input)?;
    let pending_notes = submitted.get("selectedNotes").and_then(Value::as_u64)
        .map(|count| count as usize).unwrap_or(pending_notes);
    let started = matches!(submitted.get("disposition").and_then(Value::as_str), Some("started" | "steered"));
    Ok(json!({"pendingNotes": pending_notes, "started": started, "submit": submitted}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(params: Value) -> ValidatedCutexRequest {
        ValidatedCutexRequest { request_id: "req-1".into(), method: METHOD.into(), params }
    }

    fn session(active_turn: Option<&str>) -> Value {
        json!({"runtime": {"activeTurnId": active_turn, "appServerConnected": true}, "native": {"threadId": "t"}})
    }

    /// Owner decision 2026-10-08 item 5.
    #[test]
    fn wake_is_root_only_and_queues_one_owner_input() {
        let denied = dispatch_with(false, &session(None), &request(json!({})), || Ok(0), |_| unreachable!()).unwrap_err();
        assert_eq!(denied.code, "cutex_method_denied");
        let invalid = dispatch_with(true, &session(None), &request(json!({"x": 1})), || Ok(0), |_| unreachable!()).unwrap_err();
        assert_eq!(invalid.code, "invalid_request");
        // Idle: auto (starts a turn now); offline sessions start first.
        let mut seen = None;
        let result = dispatch_with(true, &session(None), &request(json!({})), || Ok(2), |submit| {
            seen = Some(submit.clone());
            Ok(json!({"disposition": "started"}))
        })
        .unwrap();
        assert_eq!(result["pendingNotes"], 2);
        assert_eq!(result["started"], true);
        let seen = seen.unwrap();
        assert_eq!(seen.method, "cutex/userInput/submit");
        assert_eq!(seen.params["strategy"], "auto");
        assert_eq!(seen.params["startIfOffline"], true);
        assert_eq!(seen.params["input"][0]["text"], WAKE_TEXT);
        assert_eq!(seen.params["clientUserMessageId"], "agent-bus-wake-req-1");
        // Busy: `auto` too (023 Q2): steered into the running turn's next step.
        let result = dispatch_with(true, &session(Some("turn-1")), &request(Value::Null), || Ok(0), |submit| {
            assert_eq!(submit.params["strategy"], "auto");
            Ok(json!({"disposition": "steered"}))
        })
        .unwrap();
        assert_eq!((result["pendingNotes"].as_u64(), result["started"].as_bool()), (Some(0), Some(true)));
    }

    #[test]
    fn read_now_freezes_exact_ids_across_replay_and_reopen() {
        let root = std::env::temp_dir().join(format!("wake-read-set-{}", uuid::Uuid::new_v4()));
        let ledger = super::super::input_ledger::InputLedger::open(root.join("ledger.db"));
        let input = ValidatedCutexRequest { request_id: "wake-1".into(), method: "cutex/userInput/submit".into(),
            params: json!({"clientUserMessageId":"owner-1","origin":{"kind":"other","clientId":CLIENT_ID},
                "strategy":"auto","input":[{"type":"text","text":WAKE_TEXT}],"startIfOffline":true}) };
        assert_eq!(capture_read_set(&ledger, "session", &input, Some(&json!(["note-b"])),
            &["note-a".into(), "note-b".into()]).unwrap(), ["note-b"]);
        drop(ledger);
        let ledger = super::super::input_ledger::InputLedger::open(root.join("ledger.db"));
        assert_eq!(capture_read_set(&ledger, "session", &input, None, &["later-note".into()]).unwrap(), ["note-b"]);
        assert!(capture_read_set(&ledger, "session", &input, Some(&json!(["note-a"])), &[]).is_err());
        assert_eq!(selected_note_ids(&ledger, "session", "owner-1").unwrap().unwrap(), ["note-b"]);
        let mut logical = ledger.payload("session", "owner-1").unwrap().unwrap();
        logical.as_object_mut().unwrap().remove(READ_NOTES_FIELD);
        super::super::user_input::parse_user_input_submit_params(&logical).unwrap();
        drop(ledger);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_now_rejects_invalid_selection_before_submitting() {
        for params in [json!({"inputIds":["a","a"]}), json!({"inputIds":[""]}),
            json!({"inputIds":"a"}), json!({"inputIds":(0..100).map(|n| n.to_string()).collect::<Vec<_>>()})] {
            assert!(dispatch_with(true, &session(None), &request(params), || unreachable!(), |_| unreachable!()).is_err());
        }
        super::super::contract_validation::validate_cutex_request(&json!({"requestId":"wake",
            "method":METHOD,"params":{"inputIds":["note-a"]}})).unwrap();
    }

    #[test]
    fn read_now_reports_an_ordered_reservation_before_submitting() {
        let reserved = ["reserved".into()];
        let available = ["available".into()];
        let selection = request(json!({"inputIds":["available","reserved"]}));
        let error = dispatch_with(true, &session(None), &selection,
            || check_ordered_reservations(selection.params.get("inputIds"), &available, &reserved).map(|()| 2),
            |_| unreachable!()).unwrap_err();
        assert_eq!(error.code, "ordered_batch_pending");
        assert!(check_ordered_reservations(None, &[], &reserved).is_err());
        assert!(check_ordered_reservations(None, &available, &reserved).is_ok());
    }
}
