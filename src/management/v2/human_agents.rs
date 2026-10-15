//! Root-only Human agent creation: `POST /v2/human/agents`.
//!
//! Behaviour mirrors `cutex human new` (persist an empty native thread, adopt
//! it as a durable Agent) plus the Desktop follow-ups: make it visible, set
//! its profile, add it to a Project (cwd defaults to the Project
//! `workspace_root`) and optionally start it. The bin-side executor does the
//! work; this module owns parsing, idempotency and the response contract.
//!
//! Without `cwd` and `projectId` (capability `human-agent-scratch-cwd`) the
//! agent gets a fresh owner-private scratch workspace
//! `<cutex config dir>/agents-scratch/<yyyymmdd>-<8 hex>` (0700 on Unix,
//! protected owner DACL on Windows), allocated after every validation step and
//! right before the agent is created; it becomes the session cwd and is never
//! deleted by cutex once an agent uses it. If creation fails before an agent
//! exists the (still empty) directory is removed again. A retry with the same
//! `requestId` replays the stored answer, so it never allocates a second
//! directory. A Project without `workspace_root` still answers
//! `workspace_root_required`.

use std::net::TcpStream;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::management::server::ManagementRequestContext;

use super::native_requests::request_idempotency_repository;
use super::native_requests::BeginRequest;
use super::repository::EventRepository;
use super::user_input::UserInputExecutionError;

/// Idempotency namespace; requestIds are scoped to this route.
pub const HUMAN_AGENT_IDEMPOTENCY_SCOPE: &str = "cutex/human/agents";
pub const HUMAN_AGENT_NAME_MAX_CHARS: usize = 128;
/// Scratch workspaces of cwd-less, Project-less agents live under
/// `<cutex config dir>/agents-scratch` (quick chats: `chats`).
pub const AGENT_SCRATCH_DIR_NAME: &str = "agents-scratch";

/// Create a fresh owner-private scratch workspace for a cwd-less agent.
pub fn create_agent_scratch_dir() -> anyhow::Result<std::path::PathBuf> {
    create_agent_scratch_dir_in(&crate::config::paths::config_dir()?)
}

/// `<config_dir>/agents-scratch/<yyyymmdd>-<8 hex>`, owner-only.
pub fn create_agent_scratch_dir_in(
    config_dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    super::human_sessions::create_private_workspace_in(config_dir, AGENT_SCRATCH_DIR_NAME)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HumanCreateAgentRequest {
    pub request_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::session::metadata::deserialize_optional_session_cwd"
    )]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default)]
    pub start: bool,
    /// Optional per-thread launch overrides (sandbox, approval, network,
    /// writable roots, model, effort); absent fields use the agent defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<crate::config::new_thread::LaunchOverrides>,
}

impl HumanCreateAgentRequest {
    /// Shape checks that need no filesystem or store access.
    pub fn validate(&self) -> Result<(), UserInputExecutionError> {
        if self.request_id.is_empty() || self.request_id.chars().count() > 256 {
            return Err(validation_error(
                "requestId must be a non-empty string of at most 256 characters",
            ));
        }
        if self.name.trim().is_empty()
            || self.name.trim() != self.name
            || self.name.chars().count() > HUMAN_AGENT_NAME_MAX_CHARS
            || self.name.chars().any(char::is_control)
        {
            return Err(validation_error(
                "name must be a trimmed, non-empty agent name of at most 128 characters",
            ));
        }
        if let Some(profile) = &self.profile {
            if profile.trim().is_empty() || profile.chars().any(char::is_control) {
                return Err(validation_error("profile must be a non-empty string"));
            }
        }
        if let Some(project_id) = &self.project_id {
            if crate::agent_management::ProjectId::new(project_id.clone()).is_err() {
                return Err(validation_error("projectId is not a valid project ID"));
            }
        }
        if let Some(cwd) = &self.cwd {
            if cwd.is_empty()
                || cwd.chars().any(char::is_control)
                || !std::path::Path::new(cwd).is_absolute()
            {
                return Err(validation_error("cwd must be an absolute path"));
            }
            // A filesystem root cannot be an agent's cwd (the roster import refuses it after
            // the agent already exists, which left a half-created agent).
            if std::path::Path::new(cwd).parent().is_none() {
                return Err(validation_error("cwd must not be a filesystem root"));
            }
        }
        if let Some(launch) = &self.launch {
            launch.validate().map_err(|message| validation_error(&message))?;
        }
        Ok(())
    }
}

/// Error raised before any side effect. The idempotency claim is released so
/// the same requestId may be retried after the caller fixes the input.
pub fn validation_error(message: &str) -> UserInputExecutionError {
    UserInputExecutionError {
        stage: "validation".to_string(),
        code: "invalid_request".to_string(),
        message: message.to_string(),
        retryable: false,
        details: json!({}),
        outcome_unknown: false,
    }
}

pub(super) fn handle_create(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
) -> anyhow::Result<()> {
    let request_id_hint = serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|value| value.get("requestId")?.as_str().map(str::to_string));
    let payload: HumanCreateAgentRequest = match serde_json::from_slice(&request.body) {
        Ok(payload) => payload,
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &format!("strict Human create-agent request parsing failed: {error}"),
                false,
                json!({ "requestId": request_id_hint }),
            )
        }
    };
    if let Err(error) = payload.validate() {
        return super::server::write_v2_error(
            stream,
            400,
            "Bad Request",
            &error.code,
            &error.message,
            false,
            json!({ "requestId": payload.request_id }),
        );
    }
    let canonical = serde_json::to_value(&payload)?;
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    run_idempotent_create(
        stream,
        context,
        repository,
        IdempotentCreate {
            scope: HUMAN_AGENT_IDEMPOTENCY_SCOPE,
            noun: "create-agent",
            request_id: &payload.request_id,
            canonical: &canonical,
        },
        || (context.create_human_agent)(&principal, &payload),
    )
}

/// Route-specific parameters of a Human create request.
pub(super) struct IdempotentCreate<'a> {
    pub scope: &'static str,
    pub noun: &'static str,
    pub request_id: &'a str,
    pub canonical: &'a Value,
}

/// Shared requestId idempotency + response contract for Human create routes:
/// the executor returns the new cutex session ID; the response carries it and
/// the session resource (including hidden).
pub(super) fn run_idempotent_create(
    stream: &mut TcpStream,
    context: ManagementRequestContext,
    repository: &EventRepository,
    create: IdempotentCreate<'_>,
    execute: impl FnOnce() -> Result<String, UserInputExecutionError>,
) -> anyhow::Result<()> {
    let IdempotentCreate { scope, noun, request_id, canonical } = create;
    let idempotency = request_idempotency_repository()?;
    let claim = match idempotency.begin(scope, request_id, canonical) {
        Ok(BeginRequest::Forward(claim)) => claim,
        Ok(BeginRequest::InProgress) => {
            return super::server::write_v2_error(
                stream,
                202,
                "Accepted",
                "request_in_progress",
                &format!("an identical {noun} request is still in progress"),
                true,
                json!({ "requestId": request_id, "retryAfterMs": 1000 }),
            )
        }
        Ok(BeginRequest::Completed(response)) => {
            return write_json_response(stream, response.status, &response.reason, &response.body)
        }
        Ok(BeginRequest::Conflict) => {
            return super::server::write_v2_error(
                stream,
                409,
                "Conflict",
                "idempotency_conflict",
                "requestId was already used with a different request body",
                false,
                json!({ "requestId": request_id }),
            )
        }
        Ok(BeginRequest::OutcomeUnknown) => {
            return super::server::write_v2_error(
                stream,
                409,
                "Conflict",
                "request_outcome_unknown",
                "a previous attempt with this requestId did not finish; inspect the session list before creating again",
                false,
                json!({ "requestId": request_id, "resyncRequired": true }),
            )
        }
        Ok(BeginRequest::CapacityExceeded(full)) => {
            return super::server::write_idempotency_capacity_exceeded(stream, request_id, &full)
        }
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "event_persistence_unavailable",
                "durable management request state is unavailable",
                true,
                json!({ "diagnostic": format!("{error:#}") }),
            )
        }
    };
    match execute() {
        Ok(cutex_session_id) => {
            let registry = (context.load_registry)()?;
            let session = super::session::session_resource_including_hidden(
                &cutex_session_id,
                &registry,
                context.load_runtime_status,
                repository,
            )
            .ok()
            .flatten()
            .unwrap_or(Value::Null);
            let response = json!({
                "contractVersion": 2,
                "requestId": request_id,
                "cutexSessionId": cutex_session_id,
                "session": session,
            });
            if let Err(error) = idempotency.complete(&claim, 200, "OK", response.clone()) {
                let _ = idempotency.mark_outcome_unknown(&claim);
                eprintln!("human {noun} idempotency completion failed: {error:#}");
            }
            write_json_response(stream, 200, "OK", &response)
        }
        Err(error) if error.stage == "validation" => {
            let _ = idempotency.release_before_forward(&claim);
            let status = super::server::human_agent_error_status(&error.code);
            super::server::write_v2_error(
                stream,
                status,
                super::server::http_reason_for(status),
                &error.code,
                &error.message,
                false,
                error.details,
            )
        }
        Err(error) => super::server::write_cutex_operation_error_for(
            stream,
            idempotency,
            &claim,
            request_id,
            error,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> Result<HumanCreateAgentRequest, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn create_agent_request_is_strict_and_validated() {
        let ok = request(json!({
            "requestId": "r1",
            "name": "Worker",
            "projectId": "desktop-test",
            "profile": "ds-api",
            "start": true
        }))
        .expect("valid request");
        ok.validate().expect("valid");
        assert!(ok.cwd.is_none() && ok.start);
        let defaults = request(json!({ "requestId": "r2", "name": "W" })).unwrap();
        assert!(!defaults.start && defaults.project_id.is_none() && defaults.launch.is_none());
        // Backwards compatible: no launch key is serialized when absent, so
        // existing idempotency fingerprints are unchanged.
        assert!(serde_json::to_value(&defaults).unwrap().get("launch").is_none());
        let launch = request(json!({ "requestId": "r3", "name": "W",
            "launch": { "sandbox": "workspace-write", "networkAccess": true, "approvalPolicy": "on-request" } }))
        .unwrap();
        launch.validate().expect("launch overrides");
        assert!(request(json!({ "requestId": "r", "name": "W", "launch": { "network": true } })).is_err());

        assert!(request(json!({ "requestId": "r", "name": "W", "extra": 1 })).is_err());
        assert!(request(json!({ "name": "W" })).is_err());
        for invalid in [
            json!({ "requestId": "", "name": "W" }),
            json!({ "requestId": "r", "name": " W" }),
            json!({ "requestId": "r", "name": "" }),
            json!({ "requestId": "r", "name": "x".repeat(129) }),
            json!({ "requestId": "r", "name": "W", "cwd": "relative/dir" }),
            json!({ "requestId": "r", "name": "W", "cwd": if cfg!(windows) { "C:\\" } else { "/" } }),
            json!({ "requestId": "r", "name": "W", "profile": " " }),
            json!({ "requestId": "r", "name": "W", "projectId": "bad id/!" }),
            json!({ "requestId": "r", "name": "W", "launch": { "sandbox": "everything" } }),
            json!({ "requestId": "r", "name": "W", "launch": { "approvalPolicy": "untrusted" } }),
        ] {
            let parsed = request(invalid.clone()).expect("shape parses");
            let error = parsed.validate().expect_err(&invalid.to_string());
            assert_eq!(error.stage, "validation");
            assert_eq!(error.code, "invalid_request");
        }
    }

    #[test]
    fn agent_scratch_dirs_are_unique_private_directories_under_agents_scratch() {
        let base = std::env::temp_dir().join(format!(
            "cutex-agent-scratch-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir(&base).unwrap();
        let first = create_agent_scratch_dir_in(&base).expect("scratch");
        let second = create_agent_scratch_dir_in(&base).expect("scratch");
        assert_ne!(first, second);
        for dir in [&first, &second] {
            assert!(dir.is_dir());
            assert_eq!(dir.parent().unwrap(), base.join(AGENT_SCRATCH_DIR_NAME));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for path in [dir.as_path(), dir.parent().unwrap()] {
                    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
                }
            }
            #[cfg(windows)]
            crate::platform::private_fs::open_validated_directory(dir).expect("owner-only DACL");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
