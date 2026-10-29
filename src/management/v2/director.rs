//! Owner HTTP boundary for project Director history and explicit handover review.
use crate::{
    asks::store::AskError,
    director_address as da,
    http::server::{write_json_response, SimpleHttpRequest},
};
use da::management::ExtensionStateStore;
pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema =
    super::schema_export::ExportedSchema {
        name: "director",
        title: "Project Director and handover",
        routes: &[
            "GET /v2/projects/{id}/director",
            "GET /v2/projects/{id}/director/handover",
            "POST /v2/projects/{id}/director/handover/{review}/accept|discard",
            "GET /v2/projects/{id}/director/scopes/candidates",
            "POST /v2/projects/{id}/director/scopes",
            "GET /v2/projects/{id}/director/start-reviews",
            "GET /v2/projects/{id}/director/start-reviews/{review}/content",
            "POST /v2/projects/{id}/director/start-reviews/{review}/accept|discard",
        ],
        text: include_str!("schema/export/director.json"),
    };
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::TcpStream;

#[derive(Debug, PartialEq, Eq)]
struct Route {
    project: String,
    tail: Vec<String>,
}
fn route(path: &str) -> Option<Route> {
    let parts: Vec<_> = path.split('?').next()?.split('/').collect();
    if parts.len() < 5 || parts[..3] != ["", "v2", "projects"] || parts[4] != "director" {
        return None;
    }
    let project = super::server::decode_path_segment(parts[3])?;
    if project.is_empty() {
        return None;
    }
    let tail = parts[5..]
        .iter()
        .map(|p| super::server::decode_path_segment(p))
        .collect::<Option<Vec<_>>>()?;
    Some(Route { project, tail })
}
pub fn is_path(path: &str) -> bool {
    route(path).is_some()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Decision {
    expected_epoch: u64,
    expected_revision: u64,
    action_id: Option<String>,
    reason: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scope {
    id: String,
    expected_epoch: u64,
    ask_ids: Vec<String>,
}
fn body<T: serde::de::DeserializeOwned>(request: &SimpleHttpRequest) -> Result<T, AskError> {
    if request.body.len() > 1024 * 1024 {
        return Err(AskError::new(
            "invalid_request",
            "Request body exceeds 1 MiB",
        ));
    }
    serde_json::from_slice(&request.body)
        .map_err(|e| AskError::new("invalid_request", e.to_string()))
}
fn execute(request: &SimpleHttpRequest) -> Result<Value, AskError> {
    let route = route(&request.path)
        .ok_or_else(|| AskError::new("not_found", "Unknown Director resource"))?;
    let project = &route.project;
    let roster = crate::memory::load_agent_management_snapshot().map_err(da_error)?;
    let (director, epoch) = da::project_director_state(&roster, project)?;
    let ownership = da::Store::open_default()?.snapshot()?;
    let extension = da::management::ManagementStore::open_default()
        .map_err(da_error)?
        .read()
        .map_err(da_error)?;
    let tail: Vec<_> = route.tail.iter().map(String::as_str).collect();
    match (request.method.as_str(), tail.as_slice()) {
        ("GET", []) => Ok(
            json!({"projectId":project,"directorSessionId":director,"authorityEpoch":epoch,"changes":extension.transitions.values().filter(|c|c.project_id.as_str()==project).collect::<Vec<_>>()}),
        ),
        ("GET", ["handover"]) => Ok(
            json!({"items":ownership.reviews.values().filter(|r|r.project_id==*project).collect::<Vec<_>>()}),
        ),
        ("GET", ["start-reviews"]) => Ok(
            json!({"items":extension.start_reviews.values().filter(|r|r.project_id.as_str()==project).collect::<Vec<_>>()}),
        ),
        ("GET", ["start-reviews", id, "content"]) => da::api::start_content(project, id),
        ("GET", ["scopes", "candidates"]) => {
            let asks = crate::asks::store::AskStore::open_default()
                .map_err(da_error)?
                .history_snapshot()
                .map_err(da_error)?;
            let old: std::collections::BTreeSet<_> = extension
                .transitions
                .values()
                .filter(|c| c.project_id.as_str() == project)
                .filter_map(|c| c.old_director.as_ref().map(|id| id.as_str()))
                .collect();
            Ok(
                json!({"items":asks.asks.values().filter(|a|a.remote.is_none() && a.peer_inbound.is_none() && match ownership.asks.get(&a.ask_id){Some(o)=>o.project_id==*project && (o.owner_hold || director.as_deref()!=Some(o.controller.as_str())),None=>a.project_id.as_deref()==Some(project) || a.requester.session().is_some_and(|id|old.contains(id))})
                .map(|a|json!({"askId":a.ask_id,"title":a.title,"requester":a.requester,"targetSession":a.target_cutex_session_id,"projectHint":a.project_id,"provenanceKnown":ownership.asks.contains_key(&a.ask_id)})).collect::<Vec<_>>() }),
            )
        }
        ("POST", ["scopes"]) => {
            let scope: Scope = body(request)?;
            let roster =
                crate::agent_management::AgentManagementStore::open_default().map_err(da_error)?;
            let _fence = roster.lock_mutations().map_err(da_error)?;
            let (_, current, current_epoch) = da::authority_for_project(project)?;
            if current_epoch != scope.expected_epoch {
                return Err(AskError::new(
                    "authority_conflict",
                    "Director epoch changed",
                ));
            }
            let asks = crate::asks::store::AskStore::open_default()
                .map_err(da_error)?
                .lookup_snapshot(&scope.ask_ids.iter().map(String::as_str).collect::<Vec<_>>())
                .map_err(da_error)?;
            da::Store::open_default()?.approve_scope(
                da::ApprovedScope {
                    id: scope.id.clone(),
                    project_id: project.clone(),
                    director: current,
                    ask_ids: scope.ask_ids,
                    expected_epoch: current_epoch,
                },
                &asks,
            )?;
            Ok(json!({"ok":true,"scopeId":scope.id}))
        }
        ("POST", [kind, id, verb])
            if matches!(*kind, "handover" | "start-reviews")
                && matches!(*verb, "accept" | "discard") =>
        {
            let decision: Decision = body(request)?;
            let args = json!({"review_id":id,"expected_revision":decision.expected_revision,"expected_epoch":decision.expected_epoch,"action_id":decision.action_id,"reason":decision.reason,"decision":verb});
            if *kind == "handover" {
                da::api::review_handover_as(None, Some(project), &args)
            } else {
                da::api::review_start_as(None, Some(project), &args)
            }
        }
        _ => Err(AskError::new(
            "not_found",
            "Unknown Director route or method",
        )),
    }
}
fn da_error(error: impl std::fmt::Display) -> AskError {
    AskError::new("director_state_unavailable", error.to_string())
}
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match execute(request) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => {
            let status = match error.code {
                "not_found"
                | "project_not_found"
                | "review_not_found"
                | "start_review_not_found"
                | "director_not_found" => 404,
                "invalid_request" | "invalid_scope" => 400,
                "director_state_unavailable" => 503,
                _ => 409,
            };
            super::server::write_v2_error(
                stream,
                status,
                "Director request failed",
                error.code,
                &error.detail,
                false,
                error.extra,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exported_review_and_director_shapes_accept_actual_serialized_records() {
        let review = da::HandoverReview {
            id: "h1".into(),
            project_id: "p1".into(),
            ask_id: "a1".into(),
            from: "old".into(),
            to: "new".into(),
            change_id: "c1".into(),
            revision: 0,
            state: "pending".into(),
            reason: "ownerApprovedScope".into(),
            provenance_known: false,
            actor: None,
            owner_only: false,
            resolved_to: None,
        };
        super::super::schema_export::assert_valid(
            "director",
            "handoverReview",
            &serde_json::to_value(review).unwrap(),
        );
        super::super::schema_export::assert_valid(
            "director",
            "director",
            &json!({"projectId":"p1","directorSessionId":"new","authorityEpoch":2,"changes":[]}),
        );
        super::super::schema_export::assert_valid(
            "director",
            "director",
            &json!({"projectId":"p1","directorSessionId":null,"authorityEpoch":2,"changes":[]}),
        );
        super::super::schema_export::assert_valid(
            "director",
            "scopeCandidate",
            &json!({"askId":"a1","title":"held project work","requester":{"kind":"agent"},"targetSession":"target","projectHint":"p1","provenanceKnown":true}),
        );
    }
    #[test]
    fn vacant_project_exposes_its_last_epoch_instead_of_hiding_review_resources() {
        let mut roster = crate::agent_management::AgentManagementSnapshot::empty();
        let project = crate::agent_management::ProjectId::new("vacant").unwrap();
        roster.project_states.insert(project,serde_json::from_value(json!({"project_id":"vacant","lifecycle":"active","revision":3,"created_at":"2026-10-24T00:00:00Z","updated_at":"2026-10-24T00:00:00Z","last_director_epoch":7})).unwrap());
        assert_eq!(
            da::project_director_state(&roster, "vacant").unwrap(),
            (None, 7)
        );
    }
    #[test]
    fn routes_are_exact_and_decode_each_segment() {
        assert_eq!(
            route("/v2/projects/p/director/handover/a%3Ab/accept?x=1").unwrap(),
            Route {
                project: "p".into(),
                tail: vec!["handover".into(), "a:b".into(), "accept".into()]
            }
        );
        assert!(!is_path("/v2/projects/p/directors"));
        assert!(!is_path("/v2/projects//director"));
    }
    #[test]
    fn decision_schema_rejects_missing_cas_unknown_and_duplicate_fields() {
        assert!(
            serde_json::from_str::<Decision>(r#"{"expectedEpoch":1,"expectedRevision":0}"#).is_ok()
        );
        for text in [
            r#"{"expectedEpoch":1}"#,
            r#"{"expectedEpoch":1,"expectedRevision":0,"force":true}"#,
            r#"{"expectedEpoch":1,"expectedEpoch":2,"expectedRevision":0}"#,
        ] {
            assert!(serde_json::from_str::<Decision>(text).is_err())
        }
    }
}
