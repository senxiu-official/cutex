//! Management v2 routes and daemon effects of the owner-initiated Director
//! rotation (`owner_rotation.rs`; capability `owner-rotation`). Root
//! credential only (incl. client / device tokens).
//!
//! - `POST /v2/director-rotations` `{requestId, projectId,
//!   expectedDirectorCutexSessionId, expectedAuthorityEpoch, predecessor:
//!   "archive"|"member"|"operator", handoffTimeoutSecs?, successor?: {name?,
//!   profile?, model?, reasoningEffort?, cwd?}}` → 202 `{rotation}` (200 for an
//!   identical replay of `requestId`); 400 `invalid_request`; 404
//!   `project_not_found`; 409 `project_has_no_director` /
//!   `stale_project_authority` / `rotation_in_progress` /
//!   `idempotency_conflict` / `project_not_active`.
//! - `GET /v2/director-rotations?projectId=&limit=` → `{rotations}` newest first.
//! - `GET /v2/director-rotations/{rotationId}` → `{rotation}`; 404
//!   `rotation_not_found`.
//! - `POST /v2/director-rotations/{rotationId}/cancel` → `{rotation}`; 409
//!   `rotation_not_cancellable` once the successor creation began.
//! - `POST /v2/director-rotations/{rotationId}/continue` (continue without the
//!   handoff; `handoff_requested` or `timeout`) → `{rotation}`; 409
//!   `rotation_not_waiting`.
//!
//! Every change is published on the host event stream as
//! `cutex/ownerRotation/updated {rotation}`.

use std::net::TcpStream;
use std::path::Path;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::owner_rotation::{
    rotation_body, AgentFacts, ApiError, DelegationCounts, Driver, EffectError, ProjectFacts,
    Rotation, RotationEffects, RotationStore, StartRequest, UPDATED_EVENT,
};
use crate::agent_management::{AgentActionId, AgentManagementError, ProjectId, ProjectLifecycle};
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::control_plane::{
    HumanManagementOperatorActionRequest, HumanManagementOperatorKind,
    HumanManagementOperatorSchema, HumanManagementPrincipal, HumanManagementProjectMutationKind,
    HumanManagementProjectMutationRequest, HumanManagementProjectMutationSchema,
};
use crate::management::server::ManagementRequestContext;
use crate::role_revision::CutexSessionId;

pub const PATH: &str = "/v2/director-rotations";
const POLL: Duration = Duration::from_secs(2);

/// `schema/export/owner-rotation.json` (schema export index entry).
pub const EXPORTED_SCHEMA: super::schema_export::ExportedSchema = super::schema_export::ExportedSchema {
    name: "owner-rotation",
    title: "Owner-initiated Director rotation",
    routes: &[
        "POST /v2/director-rotations",
        "GET /v2/director-rotations",
        "GET /v2/director-rotations/{rotationId}",
        "POST /v2/director-rotations/{rotationId}/cancel",
        "POST /v2/director-rotations/{rotationId}/continue",
        "host event cutex/ownerRotation/updated",
    ],
    text: include_str!("schema/export/owner-rotation.json"),
};
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

pub fn is_path(path: &str) -> bool {
    path == PATH || path.starts_with("/v2/director-rotations/")
}

// ---------------------------------------------------------------------------
// Driver thread

static DRIVER: OnceLock<Driver<DaemonEffects>> = OnceLock::new();
static WAKE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
static THREAD: OnceLock<()> = OnceLock::new();

fn driver(context: ManagementRequestContext) -> anyhow::Result<&'static Driver<DaemonEffects>> {
    if let Some(driver) = DRIVER.get() {
        return Ok(driver);
    }
    let store = RotationStore::at(RotationStore::default_path()?, publish);
    Ok(DRIVER.get_or_init(|| Driver::new(store, DaemonEffects { context })))
}

fn wake_pair() -> &'static (Mutex<bool>, Condvar) {
    WAKE.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

fn wake() {
    let (flag, condvar) = wake_pair();
    if let Ok(mut flag) = flag.lock() {
        *flag = true;
        condvar.notify_all();
    }
}

fn wait() {
    let (flag, condvar) = wake_pair();
    if let Ok(guard) = flag.lock() {
        if let Ok((mut guard, _)) = condvar.wait_timeout_while(guard, POLL, |woken| !*woken) {
            *guard = false;
        }
    }
}

/// Start the rotation thread (`management serve`); it resumes rotations an
/// earlier process left unfinished. Later calls do nothing.
pub fn start_driver(context: ManagementRequestContext) {
    let driver = match driver(context) {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("owner rotation: state directory unavailable: {error:#}");
            return;
        }
    };
    if THREAD.set(()).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("cutex-owner-rotation".into())
        .spawn(move || loop {
            driver.tick();
            wait();
        });
    if let Err(error) = spawned {
        eprintln!("owner rotation: could not start its thread: {error}");
    }
}

fn publish(rotation: &Rotation) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        UPDATED_EVENT,
        rotation_body(rotation),
    ) {
        eprintln!("owner rotation: could not publish event: {error:#}");
    }
}

// ---------------------------------------------------------------------------
// Routes

pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    root_token: Option<&str>,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    if !super::server::authenticated_root_request(request, root_token) {
        return super::server::write_v2_error(
            stream,
            401,
            "Unauthorized",
            "unauthorized",
            "the Management root credential is required for Director rotations",
            false,
            json!({}),
        );
    }
    let driver = match driver(context) {
        Ok(driver) => driver,
        Err(error) => return write_error(stream, &ApiError::new(503, "event_persistence_unavailable", format!("{error:#}"))),
    };
    start_driver(context);
    let (path, query) = request.path.split_once('?').unwrap_or((request.path.as_str(), ""));
    let rest = path.strip_prefix(PATH).unwrap_or_default();
    let segments: Vec<&str> = rest.split('/').filter(|segment| !segment.is_empty()).collect();
    let result = match (request.method.as_str(), segments.as_slice()) {
        ("POST", []) => {
            let payload: StartRequest = match serde_json::from_slice(&request.body) {
                Ok(payload) => payload,
                Err(error) => {
                    return write_error(
                        stream,
                        &ApiError::new(400, "invalid_request", format!("strict rotation request parsing failed: {error}")),
                    )
                }
            };
            match driver.start(&payload) {
                Ok((rotation, created)) => {
                    wake();
                    let (status, reason) = if created { (202, "Accepted") } else { (200, "OK") };
                    return write_json_response(stream, status, reason, &rotation_body(&rotation));
                }
                Err(error) => Err(error),
            }
        }
        ("GET", []) => list(driver, query),
        ("GET", [id]) => driver
            .store
            .get(id)
            .map_err(|error| ApiError::new(503, "event_persistence_unavailable", format!("{error:#}")))
            .and_then(|rotation| {
                rotation
                    .map(|rotation| rotation_body(&rotation))
                    .ok_or_else(|| ApiError::new(404, "rotation_not_found", "no such rotation"))
            }),
        ("POST", [id, "cancel"]) => driver.cancel(id).map(|rotation| {
            wake();
            rotation_body(&rotation)
        }),
        ("POST", [id, "continue"]) => driver.continue_without_handoff(id).map(|rotation| {
            wake();
            rotation_body(&rotation)
        }),
        ("GET" | "POST", _) => Err(ApiError::new(404, "not_found", "unknown Director rotation route")),
        _ => Err(ApiError::new(405, "method_not_allowed", "method not allowed")),
    };
    match result {
        Ok(body) => write_json_response(stream, 200, "OK", &body),
        Err(error) => write_error(stream, &error),
    }
}

fn list(driver: &Driver<DaemonEffects>, query: &str) -> Result<Value, ApiError> {
    let mut project_id = None;
    let mut limit = DEFAULT_LIMIT;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "projectId" => project_id = Some(value.into_owned()),
            "limit" => {
                limit = value
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| ApiError::new(400, "invalid_request", "limit must be 1-100"))?
            }
            _ => {}
        }
    }
    let rotations = driver
        .store
        .list(project_id.as_deref(), limit)
        .map_err(|error| ApiError::new(503, "event_persistence_unavailable", format!("{error:#}")))?;
    Ok(json!({ "rotations": rotations }))
}

fn write_error(stream: &mut TcpStream, error: &ApiError) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        error.status,
        super::server::http_reason_for(error.status),
        &error.code,
        &error.message,
        error.retryable,
        json!({}),
    )
}

// ---------------------------------------------------------------------------
// Daemon effects

pub struct DaemonEffects {
    context: ManagementRequestContext,
}

fn principal() -> HumanManagementPrincipal {
    HumanManagementPrincipal::authenticated()
}

fn am_error(error: AgentManagementError) -> EffectError {
    let retryable = matches!(error, AgentManagementError::PersistenceUnavailable);
    let code = match &error {
        AgentManagementError::Conflict(reason)
        | AgentManagementError::InvalidRequest(reason)
        | AgentManagementError::NotFound(reason) => reason.to_string(),
        AgentManagementError::ConflictDetail(detail) => detail
            .split(':')
            .next()
            .unwrap_or("conflict")
            .trim()
            .to_string(),
        other => other.code().to_string(),
    };
    EffectError::new(&code, error.to_string(), retryable)
}

fn input_error(error: super::user_input::UserInputExecutionError) -> EffectError {
    EffectError::new(&error.code, error.message, error.retryable)
}

fn unavailable(error: impl std::fmt::Display) -> EffectError {
    EffectError::new("persistence_unavailable", error.to_string(), true)
}

fn invalid(error: impl std::fmt::Display) -> EffectError {
    EffectError::new("invalid_request", error.to_string(), false)
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value.as_deref().map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
}

impl DaemonEffects {
    fn session(&self, cutex_session_id: &str, archived: bool) -> Result<Value, EffectError> {
        let repository = super::repository::management_v2_repository().map_err(unavailable)?;
        let registry = (self.context.load_registry)().map_err(unavailable)?;
        let session = if archived {
            super::session::session_resource_including_archive(
                cutex_session_id,
                &registry,
                self.context.load_runtime_status,
                repository,
            )
        } else {
            super::session::session_resource_including_hidden(
                cutex_session_id,
                &registry,
                self.context.load_runtime_status,
                repository,
            )
        };
        session
            .map_err(unavailable)?
            .ok_or_else(|| EffectError::new("session_not_found", format!("{cutex_session_id} does not exist on this host"), false))
    }
}

impl RotationEffects for DaemonEffects {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn project(&self, project_id: &str) -> Result<Option<ProjectFacts>, EffectError> {
        let collection = (self.context.list_management_projects)(&principal()).map_err(am_error)?;
        let found = collection
            .projects
            .iter()
            .chain(collection.archived_projects.iter())
            .find(|project| project.project_id.as_str() == project_id);
        Ok(found.map(|project| ProjectFacts {
            project_id: project.project_id.as_str().to_string(),
            display_name: project.presentation.display_name.clone(),
            active: project.lifecycle == ProjectLifecycle::Active,
            director: project
                .director_cutex_session_id
                .as_ref()
                .map(|id| id.as_str().to_string()),
            authority_epoch: project.authority_epoch,
            workspace_root: project.workspace_root.clone(),
        }))
    }

    fn agent(&self, cutex_session_id: &str) -> Result<Option<AgentFacts>, EffectError> {
        let store = crate::session::store::load_cutex_session_store().map_err(unavailable)?;
        let Some(record) = store
            .sessions
            .values()
            .find(|record| record.cutex_session_id == cutex_session_id)
        else {
            return Ok(None);
        };
        let project_id = crate::memory::load_agent_management_snapshot()
            .ok()
            .and_then(|snapshot| {
                let id = CutexSessionId::new(cutex_session_id.to_string()).ok()?;
                let agent = snapshot.agents.get(&id)?;
                crate::agent_management::current_project_id(&snapshot, agent)
            })
            .map(|project| project.as_str().to_string());
        let name = record
            .formal_agent_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| crate::session::metadata::cutex_session_presented_name(record));
        Ok(Some(AgentFacts {
            cutex_session_id: record.cutex_session_id.clone(),
            name,
            profile: non_empty(&record.profile),
            model: non_empty(&record.model_defaults),
            reasoning_effort: non_empty(&record.reasoning_defaults),
            cwd: crate::session::metadata::cutex_session_launch_cwd(record).to_string(),
            sandbox: non_empty(&record.sandbox_mode)
                .filter(|mode| crate::config::new_thread::SANDBOX_MODES.contains(&mode.as_str())),
            approval_policy: non_empty(&record.approval_policy).filter(|policy| {
                crate::config::new_thread::APPROVAL_POLICIES.contains(&policy.as_str())
            }),
            retired: record.is_retired(),
            project_id,
        }))
    }

    fn delegations(&self, cutex_session_id: &str) -> DelegationCounts {
        let Ok(state) = crate::asks::store::AskStore::open_default().and_then(|store| store.snapshot()) else {
            return DelegationCounts::default();
        };
        let open = state.asks.values().filter(|ask| ask.is_open());
        let mut counts = DelegationCounts::default();
        for ask in open {
            if ask.requester_session() == Some(cutex_session_id) {
                counts.requested += 1;
            }
            if ask.target_cutex_session_id == cutex_session_id {
                counts.handling += 1;
            }
        }
        counts
    }

    fn send_message(
        &self,
        cutex_session_id: &str,
        client_user_message_id: &str,
        text: &str,
    ) -> Result<(), EffectError> {
        let repository = super::repository::management_v2_repository().map_err(unavailable)?;
        let session = self.session(cutex_session_id, false)?;
        let request = super::server::ValidatedCutexRequest {
            request_id: client_user_message_id.to_string(),
            method: "cutex/userInput/submit".into(),
            params: json!({
                "clientUserMessageId": client_user_message_id,
                "origin": {"kind": "other", "clientId": super::owner_rotation::CLIENT_ID},
                "strategy": "auto",
                "input": [{"type": "text", "text": text, "text_elements": []}],
                "startIfOffline": true,
            }),
        };
        let result = super::offline_submit::dispatch(
            repository,
            self.context,
            Some(principal()),
            cutex_session_id,
            &session,
            &request,
        )
        .map_err(input_error)?;
        // An owner message ends a delegation pause of the session (as for
        // every other owner message).
        if let Some(disposition) = result.get("disposition").and_then(Value::as_str).filter(|_| super::server::owner_message_landed(&result)) {
            crate::asks::control::owner_message_default(cutex_session_id, disposition);
        }
        Ok(())
    }

    fn turn_active(&self, cutex_session_id: &str) -> Result<bool, EffectError> {
        let session = self.session(cutex_session_id, false)?;
        Ok(session
            .pointer("/runtime/activeTurnId")
            .is_some_and(|turn| !turn.is_null()))
    }

    fn handoff_ready(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    }

    fn find_agent_by_name(&self, name: &str, since: &str) -> Result<Option<String>, EffectError> {
        let since = DateTime::parse_from_rfc3339(since).map_err(invalid)?;
        let store = crate::session::store::load_cutex_session_store().map_err(unavailable)?;
        Ok(store
            .sessions
            .values()
            .filter(|record| record.is_active())
            .filter(|record| {
                record.formal_agent_name.as_deref() == Some(name)
                    || crate::session::metadata::cutex_session_presented_name(record) == name
            })
            .filter(|record| {
                DateTime::parse_from_rfc3339(&record.created_at).is_ok_and(|created| created >= since)
            })
            .map(|record| record.cutex_session_id.clone())
            .next())
    }

    fn create_successor(&self, rotation: &Rotation) -> Result<String, EffectError> {
        let spec = &rotation.successor_spec;
        let launch = crate::config::new_thread::LaunchOverrides {
            sandbox: spec.sandbox.clone(),
            approval_policy: spec.approval_policy.clone(),
            model: spec.model.clone(),
            reasoning_effort: spec.reasoning_effort.clone(),
            ..Default::default()
        };
        let request = super::human_agents::HumanCreateAgentRequest {
            request_id: format!("owner-rotation-{}-successor", rotation.rotation_id),
            name: spec.name.clone(),
            project_id: Some(rotation.project_id.clone()),
            cwd: Some(spec.cwd.clone()),
            profile: spec.profile.clone(),
            start: false,
            launch: (!launch.is_empty()).then_some(launch),
        };
        request.validate().map_err(input_error)?;
        (self.context.create_human_agent)(&principal(), &request).map_err(input_error)
    }

    fn rebind(
        &self,
        project_id: &str,
        action_id: &str,
        expected_authority_epoch: u64,
        successor: &str,
    ) -> Result<u64, EffectError> {
        let request = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new(action_id).map_err(am_error)?,
            project_id: ProjectId::new(project_id).map_err(am_error)?,
            expected_authority_epoch: Some(expected_authority_epoch),
            expected_project_revision: None,
            operation: HumanManagementProjectMutationKind::RotateDirector {
                successor_cutex_session_id: CutexSessionId::new(successor.to_string()).map_err(invalid)?,
            },
        };
        (self.context.execute_management_project_mutation)(&principal(), &request).map_err(am_error)?;
        let project = self
            .project(project_id)?
            .ok_or_else(|| EffectError::new("project_not_found", "the Project disappeared", false))?;
        if project.director.as_deref() != Some(successor) {
            return Err(EffectError::new(
                "stale_project_authority",
                "the Project's Director changed again after the rebind",
                false,
            ));
        }
        Ok(project.authority_epoch)
    }

    fn archive(&self, cutex_session_id: &str, request_id: &str, reason: &str) -> Result<(), EffectError> {
        let repository = super::repository::management_v2_repository().map_err(unavailable)?;
        let session = self.session(cutex_session_id, true)?;
        if session.get("lifecycle").and_then(Value::as_str) == Some("retired") {
            return Ok(());
        }
        let request = super::server::ValidatedCutexRequest {
            request_id: request_id.to_string(),
            method: "cutex/session/retire".into(),
            params: json!({ "reason": reason }),
        };
        super::archive::dispatch_session_retire(
            repository,
            self.context.mutate_session,
            cutex_session_id,
            &session,
            &request,
        )
        .map(|_| ())
        .map_err(input_error)
    }

    fn ensure_member(&self, project_id: &str, cutex_session_id: &str, action_id: &str) -> Result<(), EffectError> {
        if self
            .agent(cutex_session_id)?
            .is_some_and(|agent| agent.project_id.as_deref() == Some(project_id))
        {
            return Ok(());
        }
        let request = HumanManagementProjectMutationRequest {
            schema: HumanManagementProjectMutationSchema::V1,
            action_id: AgentActionId::new(action_id).map_err(am_error)?,
            project_id: ProjectId::new(project_id).map_err(am_error)?,
            expected_authority_epoch: None,
            expected_project_revision: None,
            operation: HumanManagementProjectMutationKind::AddMember {
                cutex_session_id: CutexSessionId::new(cutex_session_id.to_string()).map_err(invalid)?,
            },
        };
        match (self.context.execute_management_project_mutation)(&principal(), &request) {
            Ok(_) | Err(AgentManagementError::Conflict("agent_already_project_member")) => Ok(()),
            Err(error) => Err(am_error(error)),
        }
    }

    fn grant_operator(&self, project_id: &str, cutex_session_id: &str, action_id: &str) -> Result<(), EffectError> {
        let request = HumanManagementOperatorActionRequest {
            schema: HumanManagementOperatorSchema::V1,
            action_id: AgentActionId::new(action_id).map_err(am_error)?,
            project_id: ProjectId::new(project_id).map_err(am_error)?,
            expected_authority_epoch: None,
            expected_grant_revision: None,
            operation: HumanManagementOperatorKind::Grant,
            operator_cutex_session_id: CutexSessionId::new(cutex_session_id.to_string()).map_err(invalid)?,
        };
        match (self.context.execute_management_operator_action)(&principal(), &request) {
            Ok(_) | Err(AgentManagementError::Conflict("operator_already_granted")) => Ok(()),
            Err(error) => Err(am_error(error)),
        }
    }
}
