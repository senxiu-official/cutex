//! Owner batch mutations of archived sessions. Each item owns its receipt.
use std::{collections::HashSet, net::TcpStream};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    http::server::{write_json_response, SimpleHttpRequest},
    management::{control_plane::HumanManagementPrincipal, server::ManagementRequestContext},
};

use crate::management::v2::{
    model::MAX_SAFE_SEQUENCE,
    request_idempotency::{
        request_idempotency_repository, BeginRequest, RequestIdempotencyRepository,
    },
    server::{
        cutex_request_lock, http_reason_for, human_agent_error_status, write_v2_error,
        ValidatedCutexRequest,
    },
    session::retired_session_resource,
    session_delete,
    user_input::UserInputExecutionError,
};

pub const PATH: &str = "/v2/sessions/archive-batch";
const MAX_ITEMS: usize = 20;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Action {
    Restore,
    Delete,
}

impl Action {
    fn method(self) -> &'static str {
        match self {
            Self::Restore => "cutex/session/restore",
            Self::Delete => session_delete::METHOD,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Batch {
    request_id: String,
    action: Action,
    confirmation: Option<String>,
    items: Vec<Item>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Item {
    session_id: String,
    expected_revision: u64,
}

fn parse(body: &[u8]) -> Result<Batch, &'static str> {
    if body.len() > 16 * 1024 {
        return Err("batch body exceeds 16 KiB");
    }
    let batch: Batch = serde_json::from_slice(body).map_err(|_| "invalid archive batch request")?;
    if !(1..=MAX_ITEMS).contains(&batch.items.len()) {
        return Err("archive batches require 1 to 20 items");
    }
    if batch.request_id.is_empty()
        || batch.request_id.len() > 80
        || !batch
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err("invalid requestId");
    }
    let mut unique = HashSet::new();
    for item in &batch.items {
        if item.session_id.len() > 192
            || item
                .session_id
                .strip_prefix("cutex.")
                .is_none_or(str::is_empty)
            || item
                .session_id
                .chars()
                .any(|c| c.is_control() || c == '/' || c == char::from(92))
            || !unique.insert(&item.session_id)
            || !(1..=MAX_SAFE_SEQUENCE).contains(&item.expected_revision)
        {
            return Err(
                "each item needs a unique sessionId and a positive JSON-safe expectedRevision",
            );
        }
    }
    match batch.action {
        Action::Delete => {
            let confirmation = format!("DELETE {} ARCHIVED AGENTS", batch.items.len());
            if batch.confirmation.as_deref() != Some(confirmation.as_str()) {
                return Err("typed delete confirmation is required");
            }
        }
        Action::Restore if batch.confirmation.is_some() => {
            return Err("restore does not accept confirmation")
        }
        Action::Restore => {}
    }
    Ok(batch)
}

pub(in crate::management::v2) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    if request.method != "POST" {
        return write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use POST",
            false,
            json!({}),
        );
    }
    let batch = match parse(&request.body) {
        Ok(batch) => batch,
        Err(detail) => {
            return write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                detail,
                false,
                json!({}),
            )
        }
    };
    let events = crate::management::v2::repository::management_v2_repository()?;
    crate::management::v2::server::materialize_active_stream_reset(events, context)?;
    let receipts = request_idempotency_repository()?;
    let items = batch
        .items
        .iter()
        .map(|item| {
            execute_item(
                receipts,
                &batch,
                item,
                retired_session_resource,
                |session, request| match batch.action {
                    Action::Restore => super::dispatch_session_restore(
                        events,
                        context.mutate_session,
                        &item.session_id,
                        session,
                        request,
                    ),
                    Action::Delete => session_delete::dispatch(
                        events,
                        context.delete_session,
                        Some(&HumanManagementPrincipal::authenticated()),
                        &item.session_id,
                        session,
                        request,
                        &|| unreachable!("batch delete cannot stop an active session"),
                    ),
                },
            )
        })
        .collect::<Vec<_>>();
    write_json_response(
        stream,
        200,
        "OK",
        &json!({"contractVersion":2, "requestId":batch.request_id, "items":items}),
    )
}

fn failure(code: &str, message: &str, retryable: bool) -> UserInputExecutionError {
    UserInputExecutionError {
        code: code.into(),
        message: message.into(),
        stage: "route".into(),
        retryable,
        details: json!({}),
        outcome_unknown: false,
    }
}

fn error_result(id: &str, status: u16, error: UserInputExecutionError) -> Value {
    json!({"sessionId":id, "status":status, "error":{
        "code":error.code, "message":error.message, "retryable":error.retryable, "details":error.details
    }})
}

fn execute_item(
    receipts: &RequestIdempotencyRepository,
    batch: &Batch,
    item: &Item,
    lookup: impl FnOnce(&str) -> anyhow::Result<Option<Value>>,
    mutate: impl FnOnce(&Value, &ValidatedCutexRequest) -> Result<Value, UserInputExecutionError>,
) -> Value {
    let id = &item.session_id;
    let reply = |status, code, message, retryable| {
        error_result(id, status, failure(code, message, retryable))
    };
    let unavailable = || {
        reply(
            503,
            "event_persistence_unavailable",
            "archive batch state unavailable",
            true,
        )
    };
    let unknown = || {
        reply(
            409,
            "request_outcome_unknown",
            "the durable outcome is unknown; refresh the archived list",
            false,
        )
    };
    let Ok(lane) = cutex_request_lock(id) else {
        return unavailable();
    };
    let Ok(guard) = lane.reserve() else {
        return unavailable();
    };
    let method = batch.action.method();
    let request = ValidatedCutexRequest {
        request_id: format!("archive-batch-{}", batch.request_id),
        method: method.into(),
        params: json!({"expectedRevision":item.expected_revision}),
    };
    // Claim before checking the archived row, so exact replays still have a
    // receipt after the row was restored or deleted.
    let scope = format!("archive-batch:{id}");
    let body = json!({"method":method, "params":request.params});
    let claim = match receipts.begin(&scope, &batch.request_id, &body) {
        Ok(BeginRequest::Forward(claim)) => claim,
        Ok(BeginRequest::Completed(response)) => return response.body,
        Ok(BeginRequest::InProgress) => {
            return reply(202, "request_in_progress", "this item is in progress", true)
        }
        Ok(BeginRequest::Conflict) => {
            return reply(
                409,
                "idempotency_conflict",
                "requestId already used with a different action or revision for this item",
                false,
            )
        }
        Ok(BeginRequest::OutcomeUnknown) => return unknown(),
        Ok(BeginRequest::CapacityExceeded(_)) => {
            return reply(
                503,
                "idempotency_capacity_exceeded",
                "archive receipt capacity is temporarily unavailable",
                true,
            )
        }
        Err(_) => return unavailable(),
    };
    let operation = (|| {
        let session = lookup(id)
            .map_err(|_| {
                failure(
                    "event_persistence_unavailable",
                    "session state unavailable",
                    true,
                )
            })?
            .ok_or_else(|| {
                failure(
                    "session_not_found",
                    "archived session does not exist on this host",
                    false,
                )
            })?;
        crate::management::v2::model_overload_retry::dispatch_after_cancel_with_reservation(
            id,
            method,
            Some(&guard),
            || mutate(&session, &request),
        )
    })();
    let (status, response) = match operation {
        Ok(result) => {
            if let Some(reason) = crate::management::v2::quota_continue::cancels_pending(method) {
                crate::management::v2::quota_continue::cancel_default(id, reason);
            }
            (200, json!({"sessionId":id,"status":200,"result":result}))
        }
        Err(error) if error.outcome_unknown => {
            let _ = receipts.mark_outcome_unknown(&claim);
            return unknown();
        }
        Err(error) => {
            let status = if error.code == "session_not_found" {
                404
            } else {
                human_agent_error_status(&error.code)
            };
            (status, error_result(id, status, error))
        }
    };
    if receipts
        .complete(&claim, status, http_reason_for(status), response.clone())
        .is_err()
    {
        let _ = receipts.mark_outcome_unknown(&claim);
        return unknown();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn request(action: &str, session_ids: &[&str], confirmation: Option<&str>) -> Value {
        json!({"requestId":"batch-1","action":action,"confirmation":confirmation,
            "items":session_ids.iter().map(|id| json!({"sessionId":id,"expectedRevision":3})).collect::<Vec<_>>()})
    }

    #[test]
    fn bounds_duplicates_and_delete_confirmation_are_validated_before_dispatch() {
        assert_eq!(
            crate::management::v2::access::classify("POST", PATH),
            Some(crate::management::v2::access::Class::OwnerOnly)
        );
        let parse_value = |value: Value| parse(&serde_json::to_vec(&value).unwrap()).is_ok();
        let ids: Vec<_> = (0..21)
            .map(|index| format!("cutex.fixture-{index}"))
            .collect();
        let ids: Vec<_> = ids.iter().map(String::as_str).collect();
        assert!(!parse_value(request("restore", &[], None)));
        assert!(parse_value(request("restore", &ids[..20], None)));
        assert!(!parse_value(request("restore", &ids, None)));
        assert!(!parse_value(request("restore", &[ids[0], ids[0]], None)));
        assert!(!parse_value(request("delete", &ids[..1], None)));
        assert!(!parse_value(request(
            "delete",
            &ids[..1],
            Some("DELETE 2 ARCHIVED AGENTS")
        )));
        assert!(parse_value(request(
            "delete",
            &ids[..1],
            Some("DELETE 1 ARCHIVED AGENTS")
        )));
        let mut invalid = request("restore", &ids[..1], None);
        invalid["items"][0]["expectedRevision"] = json!(MAX_SAFE_SEQUENCE + 1);
        assert!(!parse_value(invalid));
        assert!(parse(&vec![b' '; 16 * 1024 + 1]).is_err());
    }

    #[test]
    fn mixed_receipts_replay_after_delete_and_fence_rebinding() {
        let path = std::env::temp_dir().join(format!("cb-{}", uuid::Uuid::new_v4().simple()));
        let receipts = RequestIdempotencyRepository::open(&path, "test".into()).unwrap();
        let batch = parse(
            &serde_json::to_vec(&request(
                "delete",
                &["cutex.success", "cutex.stale", "cutex.missing"],
                Some("DELETE 3 ARCHIVED AGENTS"),
            ))
            .unwrap(),
        )
        .unwrap();
        let dispatched = Cell::new(0);
        let run = |batch: &Batch| {
            batch
                .items
                .iter()
                .map(|item| {
                    execute_item(
                        &receipts,
                        batch,
                        item,
                        |id| {
                            Ok(match id {
                                "cutex.missing" => None,
                                "cutex.stale" => Some(json!({"revision":4})),
                                _ if dispatched.get() > 0 => None,
                                _ => Some(json!({"revision":3})),
                            })
                        },
                        |session, req| {
                            super::super::require_archive_revision(
                                session,
                                req.params.as_object().unwrap(),
                            )?;
                            dispatched.set(dispatched.get() + 1);
                            Ok(json!({"deleted":true}))
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        let result = run(&batch);
        assert_eq!(result[0]["status"], 200);
        assert_eq!(result[1]["error"]["code"], "revision_conflict");
        assert_eq!(result[2]["status"], 404);
        assert_eq!(run(&batch), result);
        assert_eq!(dispatched.get(), 1);
        let mut changed = batch;
        changed.items[0].expected_revision += 1;
        assert_eq!(run(&changed)[0]["error"]["code"], "idempotency_conflict");
        drop(receipts);
        std::fs::remove_dir_all(path).unwrap();
    }
}
