//! Cheap "which turn is running" lookup for submit / stop / interrupt.
//!
//! `thread/read includeTurns:true` returns every turn with every item, which
//! for a long thread is megabytes on each Stop press. The newest turn
//! (`thread/turns/list`, desc, limit 1, summary items) answers the same
//! question; runtimes without `thread/turns/list` fall back to the full read.

use serde_json::json;
use serde_json::Value;

use cutex::app_server::client::AppServerClientError;

/// The id of the newest turn when it is in progress.
pub(crate) fn active_turn_id(
    cutex_session_id: &str,
    thread_id: &str,
) -> anyhow::Result<Option<String>> {
    let handle =
        super::app_server_user_input::session_manager(cutex_session_id).handle(cutex_session_id)?;
    active_turn_id_with(thread_id, |method, params| handle.request(method, params))
}

pub(crate) fn active_turn_id_with(
    thread_id: &str,
    call: impl Fn(&str, Value) -> Result<Value, AppServerClientError>,
) -> anyhow::Result<Option<String>> {
    match call(
        "thread/turns/list",
        json!({ "threadId": thread_id, "sortDirection": "desc", "limit": 1, "itemsView": "summary" }),
    ) {
        Ok(page) => {
            let turns = page
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("thread/turns/list returned no data"))?;
            Ok(turns.first().and_then(in_progress_id))
        }
        // Older Codex servers deserialize the method-tagged request enum
        // before dispatch and report an unknown method as InvalidRequest,
        // unlike adapters that use MethodNotFound. Only this exact missing
        // method diagnostic permits the expensive compatibility fallback.
        Err(AppServerClientError::Rpc(error))
            if error.code == -32601
                || (error.code == -32600 && error.message.starts_with(
                    "Invalid request: unknown variant `thread/turns/list`, expected ",
                )) =>
        {
            let thread = call(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": true }),
            )?;
            Ok(thread
                .pointer("/thread/turns")
                .and_then(Value::as_array)
                .and_then(|turns| turns.iter().rev().find_map(in_progress_id)))
        }
        Err(error) => Err(error.into()),
    }
}

fn in_progress_id(turn: &Value) -> Option<String> {
    (turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .then(|| turn.get("id").and_then(Value::as_str).map(str::to_string))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn ordinary_rpc_failure_does_not_trigger_a_full_history_read() {
        let calls = RefCell::new(Vec::new());
        let error = active_turn_id_with("t", |method, _| {
            calls.borrow_mut().push(method.to_string());
            Err(AppServerClientError::Rpc(
                cutex::app_server::protocol::RpcError {
                    code: -32000,
                    message: "thread unavailable".into(),
                    data: None,
                },
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("thread unavailable"));
        assert_eq!(*calls.borrow(), ["thread/turns/list"]);
    }

    #[test]
    fn legacy_codex_unknown_summary_method_falls_back_but_invalid_requests_do_not() {
        for (message, missing_method) in [
            ("Invalid request: unknown variant `thread/turns/list`, expected one of `thread/read`, `turn/start`", true),
            ("Invalid request: unknown variant `another/method`, expected `thread/read`", false),
            ("Invalid request: missing field `threadId`", false),
            ("Invalid request: unknown field `itemsView`, expected `threadId`", false),
        ] {
            let calls = RefCell::new(Vec::new());
            let result = active_turn_id_with("t", |method, _| {
                calls.borrow_mut().push(method.to_string());
                if method == "thread/turns/list" {
                    Err(AppServerClientError::Rpc(cutex::app_server::protocol::RpcError {
                        code: -32600, message: message.into(), data: None,
                    }))
                } else {
                    assert_eq!(method, "thread/read");
                    Ok(json!({"thread": {"turns": [{"id": "legacy-turn", "status": "inProgress"}]}}))
                }
            });
            if missing_method {
                assert_eq!(result.unwrap().as_deref(), Some("legacy-turn"));
                assert_eq!(*calls.borrow(), ["thread/turns/list", "thread/read"]);
            } else {
                assert!(result.is_err());
                assert_eq!(*calls.borrow(), ["thread/turns/list"]);
            }
        }
    }

    #[test]
    fn malformed_summary_is_not_mistaken_for_an_idle_thread() {
        let calls = RefCell::new(Vec::new());
        assert!(active_turn_id_with("t", |method, _| {
            calls.borrow_mut().push(method.to_string());
            Ok(json!({"thread": {"turns": []}}))
        })
        .is_err());
        assert_eq!(*calls.borrow(), ["thread/turns/list"]);
    }

    #[test]
    fn newest_listed_turn_decides_and_full_read_is_the_fallback() {
        let calls = RefCell::new(Vec::new());
        let running = active_turn_id_with("t", |method, params| {
            calls.borrow_mut().push((method.to_string(), params));
            Ok(json!({ "data": [{ "id": "turn-2", "status": "inProgress" }] }))
        })
        .unwrap();
        assert_eq!(running.as_deref(), Some("turn-2"));
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0].0, "thread/turns/list");
        assert_eq!(calls.borrow()[0].1["limit"], 1);
        assert_eq!(calls.borrow()[0].1["sortDirection"], "desc");

        let idle = active_turn_id_with("t", |_, _| {
            Ok(json!({ "data": [{ "id": "turn-2", "status": "completed" }] }))
        })
        .unwrap();
        assert_eq!(idle, None);
        let empty = active_turn_id_with("t", |_, _| Ok(json!({ "data": [] }))).unwrap();
        assert_eq!(empty, None);

        let fallback = active_turn_id_with("t", |method, _| match method {
            "thread/turns/list" => Err(AppServerClientError::Rpc(
                cutex::app_server::protocol::RpcError {
                    code: -32601,
                    message: "unknown method".into(),
                    data: None,
                },
            )),
            _ => Ok(json!({ "thread": { "turns": [
                { "id": "turn-1", "status": "completed" },
                { "id": "turn-2", "status": "inProgress" },
            ] } })),
        })
        .unwrap();
        assert_eq!(fallback.as_deref(), Some("turn-2"));
    }
}
