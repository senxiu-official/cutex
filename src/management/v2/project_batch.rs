//! Owner-only, non-atomic project archive/restore. Reuses the single mutation.
use super::request_idempotency::{
    request_idempotency_repository, BeginRequest, RequestIdempotencyRepository,
};
use crate::{
    agent_management::{AgentActionId, AgentManagementError, ProjectId},
    http::server::{write_json_response, SimpleHttpRequest},
    management::{
        control_plane::{
            HumanManagementPrincipal, HumanManagementProjectMutationKind,
            HumanManagementProjectMutationRequest, HumanManagementProjectMutationSchema,
        },
        server::ManagementRequestContext,
    },
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, net::TcpStream};

pub const FEATURE: &str = "project-lifecycle-batch-v1";
pub const PATH: &str = "/v2/agent-management/projects/archive-batch";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Action {
    Archive,
    Restore,
}
impl Action {
    fn operation(self) -> HumanManagementProjectMutationKind {
        match self {
            Self::Archive => HumanManagementProjectMutationKind::Archive,
            Self::Restore => HumanManagementProjectMutationKind::Restore,
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Batch {
    request_id: String,
    action: Action,
    items: Vec<Item>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Item {
    project_id: ProjectId,
    expected_revision: u64,
}

fn parse(body: &[u8]) -> Result<Batch, &'static str> {
    if body.len() > 16 * 1024 {
        return Err("batch body exceeds 16 KiB");
    }
    let batch: Batch = serde_json::from_slice(body).map_err(|_| "invalid project archive batch")?;
    if !(1..=20).contains(&batch.items.len()) {
        return Err("require 1 to 20 projects");
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
    let mut ids = BTreeSet::new();
    for item in &batch.items {
        if !ids.insert(&item.project_id) || item.expected_revision > super::model::MAX_SAFE_SEQUENCE
        {
            return Err(
                "projects must be unique with a JSON-safe expectedRevision (zero permitted)",
            );
        }
    }
    Ok(batch)
}

fn error(id: &ProjectId, status: u16, code: &str, message: &str, retryable: bool) -> Value {
    json!({"projectId":id,"status":status,"error":{"code":code,"message":message,"retryable":retryable}})
}

fn execute_item(
    receipts: &RequestIdempotencyRepository,
    batch: &Batch,
    item: &Item,
    mutate: impl FnOnce(&HumanManagementProjectMutationRequest) -> Result<Value, AgentManagementError>,
) -> Value {
    let id = &item.project_id;
    let fail = |status: u16, code: &str, message: &str, retryable: bool| {
        error(id, status, code, message, retryable)
    };
    let unknown = || {
        fail(
            409,
            "request_outcome_unknown",
            "refresh project state; this item must not be automatically replayed",
            false,
        )
    };
    let scope = format!("project-archive-batch:{}", id.as_str());
    let body =
        json!({"operation":batch.action.operation(),"expectedRevision":item.expected_revision});
    let claim = match receipts.begin(&scope, &batch.request_id, &body) {
        Ok(BeginRequest::Forward(claim)) => claim,
        Ok(BeginRequest::Completed(response)) => return response.body,
        Ok(BeginRequest::InProgress) => {
            return fail(
                202,
                "request_in_progress",
                "this project item is in progress",
                true,
            )
        }
        Ok(BeginRequest::Conflict) => {
            return fail(
                409,
                "idempotency_conflict",
                "requestId already used with a different action or revision for this project",
                false,
            )
        }
        Ok(BeginRequest::OutcomeUnknown) => return unknown(),
        Ok(BeginRequest::CapacityExceeded(_)) => {
            return fail(
                503,
                "idempotency_capacity_exceeded",
                "project receipt capacity unavailable",
                true,
            )
        }
        Err(_) => {
            return fail(
                503,
                "persistence_unavailable",
                "project receipt state unavailable",
                true,
            )
        }
    };
    // The inner single-project receipt remains the authority for mutation;
    // the bounded outer receipt also retains per-item refusals and replay.
    let action_id = format!(
        "project-batch-{:x}",
        Sha256::digest(format!("{}:{}", scope, batch.request_id).as_bytes())
    );
    let request = HumanManagementProjectMutationRequest {
        schema: HumanManagementProjectMutationSchema::V1,
        action_id: AgentActionId::new(action_id).expect("bounded ASCII digest"),
        project_id: id.clone(),
        expected_authority_epoch: None,
        expected_project_revision: Some(item.expected_revision),
        operation: batch.action.operation(),
    };
    let response = match mutate(&request) {
        Ok(result) => json!({"projectId":id,"status":200,"result":result}),
        // Persistence or external errors may follow a committed write. Keep
        // the claim unresolved, just as archive-batch does for unknown effects.
        Err(
            AgentManagementError::PersistenceUnavailable
            | AgentManagementError::External(_)
            | AgentManagementError::InvalidStore,
        ) => {
            let _ = receipts.mark_outcome_unknown(&claim);
            return unknown();
        }
        Err(error) => {
            let (status, _, retryable, details) =
                super::server::agent_management_control_error_parts(&error);
            let mut response = fail(status, error.code(), &error.to_string(), retryable);
            response["error"]["details"] = details;
            response
        }
    };
    let status = response["status"].as_u64().unwrap() as u16;
    if receipts
        .complete(
            &claim,
            status,
            super::server::http_reason_for(status),
            response.clone(),
        )
        .is_err()
    {
        let _ = receipts.mark_outcome_unknown(&claim);
        return unknown();
    }
    response
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    if request.method != "POST" {
        return super::server::write_v2_error(
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
        Err(message) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                message,
                false,
                json!({}),
            )
        }
    };
    let receipts = match request_idempotency_repository() {
        Ok(receipts) => receipts,
        Err(_) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "persistence_unavailable",
                "project receipts unavailable",
                true,
                json!({}),
            )
        }
    };
    let principal = HumanManagementPrincipal::authenticated();
    let items = batch
        .items
        .iter()
        .map(|item| {
            execute_item(receipts, &batch, item, |payload| {
                (context.execute_management_project_mutation)(&principal, payload).and_then(
                    |result| {
                        serde_json::to_value(result)
                            .map_err(|_| AgentManagementError::PersistenceUnavailable)
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    write_json_response(
        stream,
        200,
        "OK",
        &json!({"contractVersion":1,"requestId":batch.request_id,"atomic":false,"items":items}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    fn batch_request(action: &str, items: Value) -> Batch {
        parse(
            &serde_json::to_vec(&json!({"requestId":"batch-1","action":action,"items":items}))
                .unwrap(),
        )
        .unwrap()
    }
    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("project-batch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn project_batch_bounds_owner_gate_and_zero_revisions() {
        let request = |items| {
            serde_json::to_vec(&json!({"requestId":"b","action":"archive","items":items})).unwrap()
        };
        assert!(parse(&request(json!([]))).is_err());
        let items = (0..21)
            .map(|i| json!({"projectId":format!("p-{i}"),"expectedRevision":0}))
            .collect::<Vec<_>>();
        assert!(parse(&request(json!(items[..20]))).is_ok());
        assert!(parse(&request(json!(items))).is_err());
        assert!(parse(&request(json!([items[0], items[0]]))).is_err());
        assert!(parse(&request(
            json!([{"projectId":"p","expectedRevision":super::super::model::MAX_SAFE_SEQUENCE+1}])
        ))
        .is_err());
        for method in ["GET", "POST", "PUT", "DELETE"] {
            assert_eq!(
                super::super::access::classify(method, PATH),
                Some(super::super::access::Class::OwnerOnly)
            );
        }
    }
    #[test]
    fn project_batch_replay_conflicts_partial_failures_and_restart() {
        let root = root();
        let repo = RequestIdempotencyRepository::open(&root, "process-a".into()).unwrap();
        let batch = batch_request(
            "archive",
            json!([{"projectId":"blocked","expectedRevision":3},{"projectId":"good","expectedRevision":0}]),
        );
        let calls = Cell::new(0);
        let results = batch
            .items
            .iter()
            .map(|item| {
                execute_item(&repo, &batch, item, |request| {
                    calls.set(calls.get() + 1);
                    assert_eq!(
                        request.expected_project_revision,
                        Some(item.expected_revision)
                    );
                    assert!(request.expected_authority_epoch.is_none());
                    assert!(matches!(
                        request.operation,
                        HumanManagementProjectMutationKind::Archive
                    ));
                    if request.project_id.as_str() == "blocked" {
                        Err(AgentManagementError::Conflict("project_revision_conflict"))
                    } else {
                        Ok(json!({"project_revision":1}))
                    }
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.get(), 2);
        super::super::schema_export::assert_valid(
            "project-table",
            "batchResponse",
            &json!({"contractVersion":1,"requestId":"batch-1","atomic":false,"items":results}),
        );
        assert_eq!(results[0]["status"], 409);
        assert_eq!(results[1]["status"], 200);
        for (index, item) in batch.items.iter().enumerate() {
            assert_eq!(
                execute_item(&repo, &batch, item, |_| panic!(
                    "exact replay must not mutate"
                )),
                results[index]
            );
        }
        let changed = batch_request(
            "restore",
            json!([{"projectId":"good","expectedRevision":0}]),
        );
        assert_eq!(
            execute_item(&repo, &changed, &changed.items[0], |_| panic!(
                "conflict must not mutate"
            ))["error"]["code"],
            "idempotency_conflict"
        );
        drop(repo);
        let repo = RequestIdempotencyRepository::open(&root, "process-b".into()).unwrap();
        assert_eq!(
            execute_item(&repo, &batch, &batch.items[1], |_| panic!("restart replay")),
            results[1]
        );
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn project_batch_unknown_and_in_progress_never_repeat_the_single_mutation() {
        let root = root();
        let repo = RequestIdempotencyRepository::open(&root, "process-a".into()).unwrap();
        let batch = batch_request("restore", json!([{"projectId":"p","expectedRevision":1}]));
        let scope = "project-archive-batch:p";
        let body = json!({"operation":batch.action.operation(),"expectedRevision":1});
        let BeginRequest::Forward(claim) = repo.begin(scope, "batch-1", &body).unwrap() else {
            panic!()
        };
        assert_eq!(
            execute_item(&repo, &batch, &batch.items[0], |_| panic!("in flight"))["status"],
            202
        );
        repo.mark_outcome_unknown(&claim).unwrap();
        assert_eq!(
            execute_item(&repo, &batch, &batch.items[0], |_| panic!(
                "unknown must not repeat"
            ))["error"]["code"],
            "request_outcome_unknown"
        );
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
}
