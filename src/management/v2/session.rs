use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::OnceLock;

use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::agent_bus::model::AgentRegistrationClass;
use crate::app_server::manager::AppServerManagedRuntimeStatus;
use crate::config::paths::runtime_dir;
use crate::im::registry::ImRegistry;
use crate::management::v2::repository::EventRepository;
use crate::platform::process::process_is_running;
use crate::session::model::CutexSessionArchiveState;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::model::LaunchProfileSource;
use crate::session::store::load_cutex_session_store_shared;

use super::activity::load_session_activity_states;
use super::activity::SessionActivityState;

pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
pub const CUTEX_METHOD_REGISTRY_VERSION: u8 = 3;

pub const CUTEX_METHOD_REGISTRY_INDEX: &str = include_str!("schema/cutex-method-registry-v3.json");

/// Digest of the embedded method registry index, computed from the compiled bytes.
pub fn cutex_method_registry_index_sha256() -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(CUTEX_METHOD_REGISTRY_INDEX.as_bytes()))
}

pub const CUTEX_METHOD_REGISTRY_METHODS: &[&str] = &[
    "cutex/session/get",
    "cutex/session/retire",
    "cutex/session/restore",
    "cutex/session/defaults/update",
    "cutex/session/profile/set",
    "cutex/session/profile/clear",
    "cutex/session/metadata/update",
    "cutex/session/profile/switch",
    "cutex/session/history/revert",
    "cutex/session/files/revert",
    "cutex/session/delete",
    "cutex/session/fork",
    "cutex/session/groups/get",
    "cutex/session/groups/set",
    "cutex/session/groups/add",
    "cutex/session/groups/remove",
    "cutex/session/visibility/show",
    "cutex/session/visibility/hide",
    "cutex/runtime/online",
    "cutex/runtime/offline",
    "cutex/runtime/close",
    "cutex/focus/get",
    "cutex/focus/set",
    "cutex/focus/clear",
    "cutex/userInput/submit",
    "cutex/userInput/queue/list",
    "cutex/userInput/queue/move",
    "cutex/userInput/queue/steer",
    "cutex/userInput/queue/yield",
    "cutex/userInput/queue/update",
    "cutex/userInput/queue/remove",
    "cutex/userInput/queue/flush",
    "cutex/userInput/queue/pause",
    "cutex/userInput/queue/resume",
    "cutex/turn/stop",
    "cutex/ask/create",
    "cutex/ask/stop",
    "cutex/ask/close",
    "cutex/ask/retry",
    "cutex/ask/respond",
    "cutex/ask/resume",
    "cutex/ask/reopen",
    "cutex/agentBus/wake",
    "cutex/asks/pause",
    "cutex/asks/resume",
    "cutex/ownerNotify/ack",
    "cutex/ownerNotify/reply",
    "cutex/ownerNotify/dismiss",
    "cutex/ownerNotify/delete",
];

#[cfg(test)]
#[test]
fn method_registry_indexes_and_request_enum_match() {
    let index: Value = serde_json::from_str(CUTEX_METHOD_REGISTRY_INDEX).unwrap();
    let schema: Value = serde_json::from_str(include_str!("schema/cutex-methods-v2.json")).unwrap();
    let registry: Vec<_> = index["methods"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    let requests: Vec<_> = schema.pointer("/$defs/request/properties/method/enum").unwrap().as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(registry, CUTEX_METHOD_REGISTRY_METHODS);
    assert_eq!(registry, requests);
}

pub fn cutex_method_is_registered(method: &str) -> bool {
    CUTEX_METHOD_REGISTRY_METHODS.contains(&method)
}

const SESSION_PROJECTION_FILE: &str = "session-projection-state.json";
const SESSION_PROJECTION_LOCK_FILE: &str = "session-projection-state.lock";

/// Process lock of the projection store; it also holds the last parsed
/// version, keyed by file identity.
static SESSION_PROJECTION_LOCK: OnceLock<Mutex<Option<CachedProjectionStore>>> = OnceLock::new();

struct CachedProjectionStore {
    path: std::path::PathBuf,
    stamp: crate::platform::file_stamp::FileStamp,
    bytes: (usize, u64),
    store: SessionProjectionStore,
}

fn projection_digest(bytes: &[u8]) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    (bytes.len(), hasher.finish())
}
static ACTIVITY_LOAD_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

pub type RuntimeStatusLoader = fn(&str) -> anyhow::Result<Option<AppServerManagedRuntimeStatus>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FocusState {
    revision: u64,
    owner: String,
    mobile_muted: bool,
    source: Option<String>,
    updated_at: Option<String>,
}

impl Default for FocusState {
    fn default() -> Self {
        Self {
            revision: 1,
            owner: "none".to_string(),
            mobile_muted: false,
            source: None,
            updated_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionProjectionEntry {
    revision: u64,
    fingerprint: String,
    focus: FocusState,
}

impl Default for SessionProjectionEntry {
    fn default() -> Self {
        Self {
            revision: 1,
            fingerprint: String::new(),
            focus: FocusState::default(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionProjectionStore {
    #[serde(default = "projection_store_version")]
    version: u8,
    /// Ordered, so an unchanged store serializes to the same bytes.
    #[serde(default)]
    sessions: std::collections::BTreeMap<String, SessionProjectionEntry>,
}

fn projection_store_version() -> u8 {
    1
}

pub fn session_list_resource(
    registry: &ImRegistry,
    load_runtime_status: RuntimeStatusLoader,
    repository: &EventRepository,
) -> anyhow::Result<Value> {
    session_list_resource_with_hidden(registry, load_runtime_status, repository, false)
}

/// Owner listing. `include_hidden` is only honoured for the Management root
/// credential (enforced by the route); hidden rows carry `"visible": false`.
/// Since `im-exposure-retired` only local-only sessions without the IM flag
/// can still be hidden; persistent agents are always listed.
pub fn session_list_resource_with_hidden(
    registry: &ImRegistry,
    load_runtime_status: RuntimeStatusLoader,
    repository: &EventRepository,
    include_hidden: bool,
) -> anyhow::Result<Value> {
    let store = load_cutex_session_store_shared()?;
    let host_id = crate::platform::host::current_host_name();
    let management = management_identity(repository)?;
    let activity_states = load_activity_states_best_effort();
    let mut seen = HashSet::new();
    let mut records = store
        .sessions
        .values()
        .filter(|record| {
            crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, &host_id)
        })
        .filter(|record| record.is_active())
        .filter(|record| is_durable_management_session(record))
        .filter(|record| seen.insert(record.cutex_session_id.clone()))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.cutex_session_id.cmp(&right.cutex_session_id));
    let projection_inputs = records
        .into_iter()
        .filter_map(|record| {
            let visible = session_is_visible(registry, record);
            (visible || include_hidden).then_some((record, visible))
        })
        .map(|(record, visible)| {
            Ok((
                record,
                visible,
                load_runtime_status(&record.cutex_session_id)?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    // Build every row (and its fingerprint) outside the projection lock; under
    // the lock only the small per-row projection entries are compared and
    // updated. An unchanged listing then neither serializes nor writes the
    // projection file.
    let context = ProjectionContext::default();
    let prepared = projection_inputs
        .into_iter()
        .map(|(record, visible, runtime_status)| {
            Ok((
                record.cutex_session_id.clone(),
                prepare_session_projection(
                    record,
                    visible,
                    runtime_status,
                    activity_states.get(&record.cutex_session_id),
                    &management,
                    &context,
                )?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let sessions = with_projection_store_tracking(|store| {
        let mut changed = false;
        let sessions = prepared
            .into_iter()
            .map(|(cutex_session_id, prepared)| {
                let (value, entry_changed) =
                    update_projection_entry(store, &cutex_session_id, |entry| {
                        apply_session_projection(prepared, entry)
                    })?;
                changed |= entry_changed;
                Ok(value)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok((sessions, changed))
    })?;
    Ok(json!({
        "contractVersion": 2,
        "hostId": host_id,
        "management": management,
        "sessions": sessions,
    }))
}

pub fn session_resource(
    cutex_session_id: &str,
    registry: &ImRegistry,
    load_runtime_status: RuntimeStatusLoader,
    repository: &EventRepository,
) -> anyhow::Result<Option<Value>> {
    session_resource_with_visibility(
        cutex_session_id,
        registry,
        load_runtime_status,
        repository,
        true,
    )
}

pub fn session_resource_including_hidden(
    cutex_session_id: &str,
    registry: &ImRegistry,
    load_runtime_status: RuntimeStatusLoader,
    repository: &EventRepository,
) -> anyhow::Result<Option<Value>> {
    session_resource_with_visibility(
        cutex_session_id,
        registry,
        load_runtime_status,
        repository,
        false,
    )
}

pub fn session_resource_including_archive(
    cutex_session_id: &str,
    registry: &ImRegistry,
    load_runtime_status: RuntimeStatusLoader,
    repository: &EventRepository,
) -> anyhow::Result<Option<Value>> {
    let store = load_cutex_session_store_shared()?;
    let Some(record) = store
        .sessions
        .values()
        .find(|record| record.cutex_session_id == cutex_session_id)
    else {
        return Ok(None);
    };
    if !crate::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &crate::platform::host::current_host_name(),
    ) || !is_durable_management_session(record)
    {
        return Ok(None);
    }
    if record.is_retired() {
        return retired_session_resource(cutex_session_id);
    }
    let visible = session_is_visible(registry, record);
    let activity_states = load_activity_states_best_effort();
    Ok(Some(project_session(
        record,
        visible,
        load_runtime_status(&record.cutex_session_id)?,
        activity_states.get(&record.cutex_session_id),
        &management_identity(repository)?,
    )?))
}

fn session_resource_with_visibility(
    cutex_session_id: &str,
    registry: &ImRegistry,
    load_runtime_status: RuntimeStatusLoader,
    repository: &EventRepository,
    require_visible: bool,
) -> anyhow::Result<Option<Value>> {
    let store = load_cutex_session_store_shared()?;
    let Some(record) = store
        .sessions
        .values()
        .find(|record| record.cutex_session_id == cutex_session_id)
    else {
        return Ok(None);
    };
    let visible = session_is_visible(registry, record);
    if !crate::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &crate::platform::host::current_host_name(),
    ) || record.is_retired()
        || !is_durable_management_session(record)
        || (require_visible && !visible)
    {
        return Ok(None);
    }
    let activity_states = load_activity_states_best_effort();
    Ok(Some(project_session(
        record,
        visible,
        load_runtime_status(&record.cutex_session_id)?,
        activity_states.get(&record.cutex_session_id),
        &management_identity(repository)?,
    )?))
}

pub fn management_identity(repository: &EventRepository) -> anyhow::Result<Value> {
    let metadata = repository.stream_metadata()?;
    Ok(json!({
        "eventStream": metadata,
        "nativeRequestPolicy": {
            "version": 2,
            "sha256": super::native_requests::native_request_policy_sha256(),
            "allowRulesSha256": super::native_requests::native_request_allow_rules_sha256(),
        },
        "cutexMethodRegistry": {
            "version": CUTEX_METHOD_REGISTRY_VERSION,
            "indexSha256": cutex_method_registry_index_sha256(),
            "schemaSha256": super::contract_validation::cutex_method_schema_sha256(),
            "capabilities": {
                "sessionArchive": {
                    "version": 1,
                    "states": ["active", "retired"],
                    "retireMethod": "cutex/session/retire",
                    "restoreMethod": "cutex/session/restore",
                    "archiveQuery": "GET /v2/sessions?lifecycle=retired",
                    "archiveView": "GET /v2/sessions/{cutexSessionId}?lifecycle=retired",
                    "retireRequiresRuntimeFence": false,
                    "retireStopsRuntime": true,
                    "restoreStartsRuntime": false,
                },
                "sessionActivity": {
                    "version": 2,
                    "source": "native_app_server_events",
                    "fields": [
                        "runtimeGeneration",
                        "lastOutputAt",
                        "lastOutputCompletedAt",
                        "lastTurnCompletedAt",
                        "lastFileChangeAt",
                        "lastOutput",
                        "lastToolCall"
                    ],
                    "changesSessionRevision": false,
                }
            },
        },
        "maxRequestBytes": MAX_REQUEST_BYTES,
    }))
}

pub fn retired_session_list_resource() -> anyhow::Result<Value> {
    // The shared snapshot is current as of this read (no whole-store copy).
    let store = load_cutex_session_store_shared()?;
    let host_id = crate::platform::host::current_host_name();
    let activity_states = load_activity_states_best_effort();
    let projects = load_agent_management_snapshot_best_effort();
    let context = ProjectionContext::default();
    let mut sessions = store
        .sessions
        .values()
        .filter(|record| {
            crate::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, &host_id)
        })
        .filter(|record| record.is_retired())
        .filter(|record| is_durable_management_session(record))
        .map(|record| {
            let mut value = retired_session_resource_value_with(
                record,
                activity_states.get(&record.cutex_session_id),
                &context,
            );
            annotate_retired_session_project(&mut value, record, projects.as_ref());
            value
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        left.get("cutexSessionId")
            .and_then(Value::as_str)
            .cmp(&right.get("cutexSessionId").and_then(Value::as_str))
    });
    Ok(json!({
        "contractVersion": 2,
        "hostId": host_id,
        "lifecycle": CutexSessionArchiveState::Retired.label(),
        "sessions": sessions,
    }))
}

pub fn retired_session_resource(cutex_session_id: &str) -> anyhow::Result<Option<Value>> {
    let store = load_cutex_session_store_shared()?;
    let activity_states = load_activity_states_best_effort();
    let record = store.sessions.values().find(|record| {
        record.cutex_session_id == cutex_session_id
            && crate::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &crate::platform::host::current_host_name(),
            )
            && record.is_retired()
            && is_durable_management_session(record)
    });
    Ok(record.map(|record| {
        let mut value =
            retired_session_resource_value(record, activity_states.get(&record.cutex_session_id));
        let projects = load_agent_management_snapshot_best_effort();
        annotate_retired_session_project(&mut value, record, projects.as_ref());
        value
    }))
}

fn load_agent_management_snapshot_best_effort(
) -> Option<crate::agent_management::AgentManagementSnapshot> {
    crate::agent_management::AgentManagementStore::open_default()
        .and_then(|store| store.snapshot().map_err(anyhow::Error::new))
        .ok()
}

/// Archived sessions carry their project like host rows do (`projectId`,
/// `projectName`; null = no project). When the project store is unreadable the
/// keys are left out, so clients treat the list as unscoped instead of
/// misfiling every archived agent as project-less.
fn annotate_retired_session_project(
    value: &mut Value,
    record: &CutexSessionRecord,
    snapshot: Option<&crate::agent_management::AgentManagementSnapshot>,
) {
    let Some(snapshot) = snapshot else {
        return;
    };
    let project = snapshot
        .agents
        .iter()
        .find(|(id, _)| id.as_str() == record.cutex_session_id)
        .and_then(|(_, agent)| crate::agent_management::current_project_id(snapshot, agent));
    match project {
        Some(id) => {
            let presentation = crate::agent_management::effective_presentation(
                &id,
                snapshot.project_presentations.get(&id),
            );
            value["projectId"] = json!(id.as_str());
            value["projectName"] = json!(presentation.display_name);
        }
        None => {
            value["projectId"] = Value::Null;
            value["projectName"] = Value::Null;
        }
    }
}

/// Claude sessions: `claudeAccount {id, label, kind, isDefault}`, the Claude
/// account the session was created on (resolved from its frozen login home;
/// it never changes). Absent on other sessions.
fn insert_claude_account(object: &mut serde_json::Map<String, Value>, record: &CutexSessionRecord) {
    if record.claude_session.is_none() {
        return;
    }
    let registry = crate::runtime::claude_accounts::load_cached();
    if let Some(account) = crate::runtime::claude_accounts::session_account_json(&registry, record) {
        object.insert("claudeAccount".to_string(), account);
    }
}

fn retired_session_resource_value(
    record: &CutexSessionRecord,
    activity: Option<&SessionActivityState>,
) -> Value {
    retired_session_resource_value_with(record, activity, &ProjectionContext::default())
}

fn retired_session_resource_value_with(
    record: &CutexSessionRecord,
    activity: Option<&SessionActivityState>,
    context: &ProjectionContext,
) -> Value {
    let mut value = retired_session_resource_fields(record, activity, context);
    insert_claude_account(value.as_object_mut().expect("session resource object"), record);
    value
}

fn retired_session_resource_fields(
    record: &CutexSessionRecord,
    activity: Option<&SessionActivityState>,
    context: &ProjectionContext,
) -> Value {
    json!({
        "contractVersion": 2,
        "cutexSessionId": record.cutex_session_id,
        "revision": record.durable_revision(),
        "lifecycle": record.archive_state.label(),
        "retiredAt": record.retired_at,
        "hostId": record.host_id,
        "displayName": crate::session::metadata::cutex_session_presented_name(record),
        "threadName": record.thread_name,
        "cwd": record.cwd,
        "managedCwd": record.managed_cwd,
        "profile": record.profile,
        "configuredProfile": record.profile,
        "effectiveNextLaunch": effective_next_launch_with(record, context),
        // Presentation metadata, as on active sessions (an archived agent
        // keeps its name, note and avatar in lists).
        "formalAgentName": record.formal_agent_name,
        "displayNameOverride": record.display_name_override,
        "note": record.note,
        "avatar": record.avatar,
        "groups": normalized_groups(&record.agent_groups),
        "registrationClass": if record.registration_class == AgentRegistrationClass::Persistent {
            "persistent"
        } else {
            "local_only"
        },
        "native": { "threadId": record.app_server_thread_id() },
        "runtime": {
            "backend": runtime_backend_name(record.runtime_backend),
            "status": "offline",
            "runtimeGeneration": record.runtime_generation,
            "runtimeAgentId": Value::Null,
        },
        "runtimeDefaults": runtime_defaults_resource(record),
        "activity": activity_resource(activity),
        "createdAt": record.created_at,
        "updatedAt": record.updated_at,
    })
}

pub fn focus_resource(cutex_session_id: &str) -> anyhow::Result<Value> {
    with_projection_entry(cutex_session_id, |entry| {
        ensure_focus_timestamp(&mut entry.focus);
        serde_json::to_value(&entry.focus).context("Failed to serialize management v2 focus")
    })
}

pub fn runtime_defaults_resource(record: &CutexSessionRecord) -> Value {
    let mut defaults = json!({
        "backend": runtime_backend_name(record.runtime_backend),
        "managedCwd": record.managed_cwd,
        "permissions": record.permission_defaults,
        "approvalPolicy": record.approval_policy,
        "sandboxMode": record.sandbox_mode,
        "model": record.model_defaults,
        "reasoningEffort": record.reasoning_defaults,
        "cliArgs": record.default_cli_args,
        "groups": normalized_groups(&record.agent_groups),
    });
    // session-fast-mode: only when the session has its own tier (absent =
    // the profile config decides), so the frozen v2 field set stays as is.
    if let Some(tier) = &record.service_tier {
        defaults["serviceTier"] = json!(tier);
    }
    // Claude sessions only (claude-tools-settings); applied at the next start.
    // Rules not set for the session follow the profile's current rule.
    if let Some(claude) = &record.claude_session {
        let (write, exec) = crate::runtime::claude::effective_tools(record, claude)
            .unwrap_or((claude.allow_write, claude.allow_exec));
        defaults["claudeAllowWrite"] = json!(write);
        defaults["claudeAllowExec"] = json!(exec);
    }
    defaults
}

fn activity_resource(activity: Option<&SessionActivityState>) -> Value {
    let activity = activity.cloned().unwrap_or_default();
    let mut resource = json!({
        "revision": activity.revision,
        "runtimeGeneration": activity.runtime_generation,
        "lastOutputAt": activity.last_output_at,
        "lastOutputCompletedAt": activity.last_output_completed_at,
        "lastTurnCompletedAt": activity.last_turn_completed_at,
        "lastFileChangeAt": activity.last_file_change_at,
    });
    let object = resource
        .as_object_mut()
        .expect("session activity resource is an object");
    if let Some(output) = activity.last_output.as_ref() {
        object.insert("lastOutput".to_string(), output_resource(output));
    }
    if let Some(tool) = activity.last_tool_call.as_ref() {
        object.insert("lastToolCall".to_string(), tool_resource(tool));
    }
    resource
}

fn output_resource(output: &crate::observability::SafeOutputProjection) -> Value {
    let mut value = json!({
        "cutexSessionId": output.association.cutex_session_id,
        "class": output.class,
        "displayText": output.display_text,
        "updatedAt": output.updated_at,
        "runtimeGeneration": output.runtime_generation,
    });
    insert_task_association(&mut value, &output.association);
    value
}

fn tool_resource(tool: &crate::observability::SafeToolCallProjection) -> Value {
    let mut value = json!({
        "cutexSessionId": tool.association.cutex_session_id,
        "class": tool.class,
        "status": tool.status,
        "displayText": tool.display_text,
        "updatedAt": tool.updated_at,
        "runtimeGeneration": tool.runtime_generation,
    });
    insert_task_association(&mut value, &tool.association);
    value
}

fn insert_task_association(
    value: &mut Value,
    association: &crate::observability::ObservationAssociation,
) {
    let object = value
        .as_object_mut()
        .expect("observability resource is an object");
    if let Some(project_id) = association.project_id.as_ref() {
        object.insert("projectId".to_string(), json!(project_id));
    }
    if let Some(assignment_id) = association.assignment_id.as_ref() {
        object.insert("assignmentId".to_string(), json!(assignment_id));
    }
    if let Some(attempt_number) = association.attempt_number {
        object.insert("attemptNumber".to_string(), json!(attempt_number));
    }
}

fn load_activity_states_best_effort() -> HashMap<String, SessionActivityState> {
    match load_session_activity_states() {
        Ok(states) => {
            ACTIVITY_LOAD_WARNING_EMITTED.store(false, Ordering::Relaxed);
            states
        }
        Err(error) => {
            if !ACTIVITY_LOAD_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "warning: session activity projection is unavailable; returning null activity: {error:#}"
                );
            }
            HashMap::new()
        }
    }
}

fn tui_is_known_attached(record: &CutexSessionRecord, app_server_connected: bool) -> bool {
    app_server_connected
        && record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden
        && record.alden_pid.is_some_and(process_is_running)
}

pub fn set_focus(
    cutex_session_id: &str,
    expected_revision: Option<u64>,
    owner: &str,
    mobile_muted: bool,
    source: Option<String>,
) -> anyhow::Result<Value> {
    if !matches!(owner, "pc" | "mobile" | "backend" | "none") {
        anyhow::bail!("focus owner is outside the v2 contract");
    }
    with_projection_entry(cutex_session_id, |entry| {
        if let Some(expected_revision) = expected_revision.filter(|expected| *expected != entry.focus.revision) {
            anyhow::bail!(
                "focus revision conflict: expected {expected_revision}, current {}",
                entry.focus.revision
            );
        }
        if entry.focus.revision >= crate::management::v2::model::MAX_SAFE_SEQUENCE {
            anyhow::bail!("focus revision exhausted the JSON-safe integer range");
        }
        entry.focus.revision += 1;
        entry.focus.owner = owner.to_string();
        entry.focus.mobile_muted = mobile_muted;
        entry.focus.source = source;
        entry.focus.updated_at = Some(Utc::now().to_rfc3339());
        serde_json::to_value(&entry.focus).context("Failed to serialize management v2 focus")
    })
}

pub fn clear_focus(cutex_session_id: &str, expected_revision: Option<u64>) -> anyhow::Result<Value> {
    set_focus(cutex_session_id, expected_revision, "none", false, None)
}

fn project_session(
    record: &CutexSessionRecord,
    visible: bool,
    runtime_status: Option<AppServerManagedRuntimeStatus>,
    activity: Option<&SessionActivityState>,
    management: &Value,
) -> anyhow::Result<Value> {
    let prepared = prepare_session_projection(
        record,
        visible,
        runtime_status,
        activity,
        management,
        &ProjectionContext::default(),
    )?;
    with_projection_entry(&record.cutex_session_id, |entry| {
        apply_session_projection(prepared, entry)
    })
}

#[cfg(test)]
fn project_session_entry(
    record: &CutexSessionRecord,
    visible: bool,
    runtime_status: Option<AppServerManagedRuntimeStatus>,
    activity: Option<&SessionActivityState>,
    management: &Value,
    entry: &mut SessionProjectionEntry,
) -> anyhow::Result<Value> {
    let prepared = prepare_session_projection(
        record,
        visible,
        runtime_status,
        activity,
        management,
        &ProjectionContext::default(),
    )?;
    apply_session_projection(prepared, entry)
}

/// Per-request inputs shared by every projected row. The global default
/// Profile (for records that inherit it) is read at most once per request,
/// so a config change is visible on the next request.
#[derive(Default)]
struct ProjectionContext {
    global_default_profile: std::cell::OnceCell<Option<String>>,
}

impl ProjectionContext {
    fn global_default_profile(&self) -> Option<&str> {
        self.global_default_profile
            .get_or_init(|| {
                crate::config::store::load_codez_config()
                    .default_profile
                    .map(|profile| profile.trim().to_string())
                    .filter(|profile| !profile.is_empty())
            })
            .as_deref()
    }
}

/// A session row built outside the projection lock: the resource without
/// `focus`, and the hash of every field that counts as a change (all but
/// `revision`, `activity`, `focus` and `management.eventStream`).
struct PreparedSessionProjection {
    value: Value,
    fingerprint_base: [u8; 32],
}

/// Feeds serialized JSON straight into a hasher (no intermediate buffer).
struct HashWriter<'a>(&'a mut sha2::Sha256);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        sha2::Digest::update(self.0, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hash_json_field(hasher: &mut sha2::Sha256, key: &str, value: &Value) -> anyhow::Result<()> {
    use sha2::Digest;
    hasher.update(key.as_bytes());
    hasher.update([0]);
    serde_json::to_writer(HashWriter(hasher), value)?;
    hasher.update([0]);
    Ok(())
}

fn session_fingerprint_base(value: &Value) -> anyhow::Result<[u8; 32]> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    for (key, field) in value.as_object().expect("session resource object") {
        match key.as_str() {
            // `revision` is the record's own, `activity` changes without a
            // session revision and focus is hashed under the lock.
            "revision" | "activity" | "focus" => {}
            // The host-wide journal cursor advances for other sessions too.
            // Keep its current value in the response, but never count it as
            // this session's change (otherwise every unrelated event rewrites
            // all rows).
            "management" if field.is_object() => {
                hasher.update(key.as_bytes());
                hasher.update([1]);
                for (inner_key, inner) in field.as_object().expect("checked object") {
                    if inner_key != "eventStream" {
                        hash_json_field(&mut hasher, inner_key, inner)?;
                    }
                }
                hasher.update([1]);
            }
            _ => hash_json_field(&mut hasher, key, field)?,
        }
    }
    Ok(hasher.finalize().into())
}

/// Adds the persisted per-session projection state to a prepared row: the
/// focus, and the projection revision that moves when the fingerprint does.
/// Runs under the projection lock, so it only hashes the small focus state.
fn apply_session_projection(
    prepared: PreparedSessionProjection,
    entry: &mut SessionProjectionEntry,
) -> anyhow::Result<Value> {
    use sha2::Digest;
    let PreparedSessionProjection {
        mut value,
        fingerprint_base,
    } = prepared;
    ensure_focus_timestamp(&mut entry.focus);
    let mut hasher = sha2::Sha256::new();
    hasher.update(fingerprint_base);
    serde_json::to_writer(HashWriter(&mut hasher), &entry.focus)?;
    let fingerprint = format!("{:x}", hasher.finalize());
    if !entry.fingerprint.is_empty() && entry.fingerprint != fingerprint {
        entry.revision = entry
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= super::model::MAX_SAFE_SEQUENCE)
            .context("management v2 session revision exhausted")?;
    }
    if entry.fingerprint != fingerprint {
        entry.fingerprint = fingerprint;
    }
    value
        .as_object_mut()
        .expect("session resource object")
        .insert("focus".to_string(), serde_json::to_value(&entry.focus)?);
    Ok(value)
}

fn prepare_session_projection(
    record: &CutexSessionRecord,
    visible: bool,
    runtime_status: Option<AppServerManagedRuntimeStatus>,
    activity: Option<&SessionActivityState>,
    management: &Value,
    context: &ProjectionContext,
) -> anyhow::Result<PreparedSessionProjection> {
    let backend = runtime_backend_name(record.runtime_backend);
    let app_server_connected = runtime_status
        .as_ref()
        .is_some_and(|status| status.connected);
    let runtime_state = if app_server_connected {
        "online"
    } else if runtime_status
        .as_ref()
        .and_then(|status| status.last_error.as_ref())
        .is_some()
    {
        "error"
    } else {
        "offline"
    };
    let schema = record
        .app_server_runtime
        .as_ref()
        .map(|binding| {
            json!({
                "protocol": "codex-app-server",
                "majorVersion": 2,
                "version": binding.schema_version,
                "sha256": binding.schema_sha256,
                "channel": "experimental",
                // The Codex app-server implements `turn/steer` itself (r47+);
                // its per-turn refusals are mapped by the queue steer route.
                "capabilities": { "experimentalApi": true, "features": { "turnSteer": true } },
                "extensions": ["cutex-inter-agent-v2"],
            })
        })
        .or_else(|| {
            record.claude_session.as_ref().map(|claude| {
                let mut schema = crate::runtime::claude::native_schema(claude);
                // Advertise steer only when the running adapter generation
                // really steers (`claude/capabilities` sameTurnSteering).
                let live = runtime_status.as_ref().filter(|status| status.connected).and_then(|status| {
                    crate::runtime::claude::live_same_turn_steering(&record.cutex_session_id, status.runtime_generation)
                });
                if let Some(features) = schema.pointer_mut("/capabilities/features").filter(|features| features.is_object()) {
                    let steer = crate::runtime::claude::projected_turn_steer(&features["turnSteer"], live);
                    if features.get("turnSteer").is_some() || live.is_some() {
                        features["turnSteer"] = json!(steer);
                    }
                }
                schema
            })
        });
    let effective_next_launch = effective_next_launch_with(record, context);
    let launched_profile = record
        .app_server_runtime
        .as_ref()
        .and_then(|binding| binding.current_launched_profile());
    let launch_profile_source = record.app_server_runtime.as_ref().and_then(|binding| {
        binding.launched_profile.as_ref().map(|_| {
            binding
                .launch_profile_source
                .as_ref()
                .map(|source| source.as_str())
                .unwrap_or(LaunchProfileSource::Unknown.as_str())
        })
    });
    let mut value = json!({
        "contractVersion": 2,
        "cutexSessionId": record.cutex_session_id,
        "revision": record.durable_revision(),
        "lifecycle": record.archive_state.label(),
        "retiredAt": record.retired_at,
        "hostId": record.host_id,
        "displayName": crate::session::metadata::cutex_session_presented_name(record),
        "threadName": record.thread_name,
        "cwd": record.cwd,
        "profile": record.profile,
        "configuredProfile": record.profile,
        "effectiveNextLaunch": effective_next_launch,
        "launchedProfile": launched_profile,
        "launchProfileSource": launch_profile_source,
        "groups": normalized_groups(&record.agent_groups),
        "registrationClass": registration_class_value(record)?,
        "visible": visible,
        "native": {
            "threadId": record.app_server_thread_id(),
            "schema": schema,
        },
        "runtime": {
            "backend": backend,
            "status": runtime_state,
            "appServerConnected": app_server_connected,
            "runtimeGeneration": record.runtime_generation,
            "runtimeAgentId": record.current_runtime_agent_id,
            "activeTurnId": runtime_status.as_ref().and_then(|status| status.active_turn_id.clone()),
            "activeTurnObservedAt": runtime_status.as_ref().and_then(|status| status.active_turn_observed_at.clone()),
            "threadStatus": runtime_status.as_ref().and_then(|status| status.thread_status.clone()),
            "threadStatusObservedAt": runtime_status.as_ref().and_then(|status| status.thread_status_observed_at.clone()),
            "threadSettings": runtime_status.as_ref().and_then(|status| status.thread_settings.clone()),
            "threadSettingsSource": runtime_status.as_ref().and_then(|status| status.thread_settings_source.clone()),
            "threadSettingsComplete": runtime_status.as_ref().is_some_and(|status| status.thread_settings_complete),
            "threadSettingsObservedAt": runtime_status.as_ref().and_then(|status| status.thread_settings_observed_at.clone()),
            "runtimeWorkspaceRoots": runtime_status.as_ref().and_then(|status| status.runtime_workspace_roots.clone()),
            "instructionSources": runtime_status.as_ref().and_then(|status| status.instruction_sources.clone()),
            "resumeSnapshotObservedAt": runtime_status.as_ref().and_then(|status| status.resume_snapshot_observed_at.clone()),
            "requiresVisibleTerminal": record.runtime_backend == CutexSessionRuntimeBackend::HostForeground,
            "tuiAttached": tui_is_known_attached(record, app_server_connected),
            "lastEventAt": runtime_status.as_ref().and_then(|status| status.last_event_at.clone()),
            "lastError": runtime_status.as_ref().and_then(|status| status.last_error.clone()),
        },
        "runtimeDefaults": runtime_defaults_resource(record),
        "activity": activity_resource(activity),
        "management": management,
        "createdAt": record.created_at,
        "updatedAt": record.updated_at,
    });
    // Presentation metadata (Management `cutex/session/metadata/update`).
    // `displayName` above already prefers `displayNameOverride`.
    let presentation = value.as_object_mut().expect("session resource object");
    presentation.insert(
        "formalAgentName".to_string(),
        json!(record.formal_agent_name),
    );
    presentation.insert(
        "displayNameOverride".to_string(),
        json!(record.display_name_override),
    );
    presentation.insert("note".to_string(), json!(record.note));
    presentation.insert("avatar".to_string(), json!(record.avatar));
    insert_claude_account(presentation, record);
    let fingerprint_base = session_fingerprint_base(&value)?;
    Ok(PreparedSessionProjection {
        value,
        fingerprint_base,
    })
}

fn effective_next_launch_with(record: &CutexSessionRecord, context: &ProjectionContext) -> Value {
    if let Some(session) = &record.claude_session {
        return json!({"profile":Value::Null,"source":"claude_subscription","model":session.model});
    }
    if let Some(profile) = record
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
    {
        return json!({ "profile": profile, "source": "session_configured" });
    }
    if let Some(profile) = context.global_default_profile() {
        return json!({ "profile": profile, "source": "global_default" });
    }
    json!({ "profile": Value::Null, "source": "unavailable" })
}

fn ensure_focus_timestamp(focus: &mut FocusState) {
    if focus.updated_at.is_none() {
        focus.updated_at = Some(Utc::now().to_rfc3339());
    }
}

/// Owner-listing visibility. Since `im-exposure-retired` every persistent
/// (managed agent) session is visible: the legacy IM exposure flag
/// (`exposed_to_backend`, `agent_create` `expose_to_im`) no longer hides an
/// agent from the owner. Local-only sessions (old CLI-tracked / unmanaged
/// sessions, many of them) keep the old rule: visible only when exposed or
/// visible in the IM registry.
fn session_is_visible(registry: &ImRegistry, record: &CutexSessionRecord) -> bool {
    record.registration_class == AgentRegistrationClass::Persistent
        || record.exposed_to_backend
        || registry.sessions.values().any(|entry| {
            entry.visible
                && (entry.session_id == record.cutex_session_id
                    || record.codex_session_id.as_deref() == Some(entry.session_id.as_str()))
        })
}

fn is_durable_management_session(record: &CutexSessionRecord) -> bool {
    matches!(
        record.registration_class,
        AgentRegistrationClass::Persistent | AgentRegistrationClass::LocalOnly
    )
}

fn normalized_groups(groups: &[String]) -> Vec<String> {
    let mut groups = groups
        .iter()
        .map(|group| group.trim())
        .filter(|group| !group.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    groups.sort();
    groups.dedup();
    groups
}

fn runtime_backend_name(backend: CutexSessionRuntimeBackend) -> &'static str {
    match backend {
        CutexSessionRuntimeBackend::Host => "host",
        CutexSessionRuntimeBackend::HostForeground => "host_foreground",
        CutexSessionRuntimeBackend::Docker => "docker",
        CutexSessionRuntimeBackend::CuteAlden => "cute_alden",
        CutexSessionRuntimeBackend::ClaudeSubscription => "claude_subscription",
        CutexSessionRuntimeBackend::Future => "future",
    }
}

fn registration_class_value(record: &CutexSessionRecord) -> anyhow::Result<Value> {
    match record.registration_class {
        AgentRegistrationClass::Persistent => Ok(json!("persistent")),
        AgentRegistrationClass::LocalOnly => Ok(json!("local_only")),
        AgentRegistrationClass::Ephemeral => {
            anyhow::bail!("ephemeral agents are not durable management v2 sessions")
        }
    }
}

fn with_projection_entry<T>(
    cutex_session_id: &str,
    action: impl FnOnce(&mut SessionProjectionEntry) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_projection_store_tracking(|store| update_projection_entry(store, cutex_session_id, action))
}

fn update_projection_entry<T>(
    store: &mut SessionProjectionStore,
    cutex_session_id: &str,
    action: impl FnOnce(&mut SessionProjectionEntry) -> anyhow::Result<T>,
) -> anyhow::Result<(T, bool)> {
    // Only this entry can change. Compare every persisted field, including
    // focus/revision, and treat inserting even a default entry as a change.
    let before = store.sessions.get(cutex_session_id).cloned();
    let entry = store
        .sessions
        .entry(cutex_session_id.to_string())
        .or_default();
    let result = action(entry)?;
    Ok((result, before.as_ref() != Some(entry)))
}

fn with_projection_store_tracking<T>(
    action: impl FnOnce(&mut SessionProjectionStore) -> anyhow::Result<(T, bool)>,
) -> anyhow::Result<T> {
    with_projection_store_at(
        &runtime_dir()?.join("management-v2"),
        SESSION_PROJECTION_LOCK.get_or_init(|| Mutex::new(None)),
        action,
    )
}

fn with_projection_store_at<T>(
    root: &Path,
    cache: &Mutex<Option<CachedProjectionStore>>,
    action: impl FnOnce(&mut SessionProjectionStore) -> anyhow::Result<(T, bool)>,
) -> anyhow::Result<T> {
    use crate::platform::file_stamp::FileStamp;
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("management v2 session projection lock was poisoned"))?;
    fs::create_dir_all(root)?;
    secure_directory(root)?;
    let lock_path = root.join(SESSION_PROJECTION_LOCK_FILE);
    let lock_file = open_private_lock(&lock_path)?;
    lock_file.lock()?;
    let result = (|| {
        let path = root.join(SESSION_PROJECTION_FILE);
        // Every session read goes through here (2-3 per second from clients
        // on the Linux host, 2026-10-09) and used to parse and rewrite the file each
        // time. Reuse the parsed version while the file is unchanged and
        // write only a changed serialization.
        let stamp = FileStamp::of(&path);
        let cached = cache
            .take()
            .filter(|entry| entry.path == path && stamp.as_ref() == Some(&entry.stamp));
        let cache_hit = cached.is_some();
        let (mut store, before) = match cached {
            Some(entry) => (entry.store, Some(entry.bytes)),
            None => match fs::read(&path) {
                Ok(bytes) => (
                    parse_projection_store(&path, &bytes)?,
                    (FileStamp::of(&path) == stamp).then(|| projection_digest(&bytes)),
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    (empty_projection_store(), None)
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "Failed to read management v2 session projections: {}",
                            path.display()
                        )
                    })
                }
            },
        };
        let (result, changed) = action(&mut store)?;
        if cache_hit && !changed {
            // A cold/external reload still takes the original serialization
            // path (normalization/version checks). Only a validated cached
            // no-op may preserve its canonical bytes without serializing.
            *cache = Some(CachedProjectionStore {
                path: path.clone(),
                stamp: stamp.expect("cache hit has a stamp"),
                bytes: before.expect("cache hit has a digest"),
                store,
            });
            return Ok(result);
        }
        #[cfg(test)]
        PROJECTION_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
        let data = serde_json::to_vec_pretty(&store)
            .context("Failed to serialize management v2 session projections")?;
        let after = projection_digest(&data);
        if before == Some(after) {
            if let Some(stamp) = stamp {
                *cache = Some(CachedProjectionStore {
                    path: path.clone(),
                    stamp,
                    bytes: after,
                    store,
                });
            }
            return Ok(result);
        }
        crate::config::atomic::write_private_bytes_atomic(&path, &data)
            .context("Failed to write management v2 session projections")?;
        if let Some(stamp) = FileStamp::of(&path) {
            *cache = Some(CachedProjectionStore {
                path: path.clone(),
                stamp,
                bytes: after,
                store,
            });
        }
        Ok(result)
    })();
    let unlock = lock_file.unlock();
    if result.is_ok() {
        unlock?;
    }
    result
}

fn parse_projection_store(path: &Path, bytes: &[u8]) -> anyhow::Result<SessionProjectionStore> {
    let store: SessionProjectionStore = serde_json::from_slice(bytes).with_context(|| {
        format!(
            "Failed to parse management v2 session projections: {}",
            path.display()
        )
    })?;
    if store.version != 1 {
        anyhow::bail!("unsupported management v2 session projection version");
    }
    Ok(store)
}

fn empty_projection_store() -> SessionProjectionStore {
    SessionProjectionStore {
        version: 1,
        sessions: Default::default(),
    }
}

fn open_private_lock(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
thread_local! { static PROJECTION_SERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// Projection-file serializations on the calling thread (benchmarks).
#[cfg(test)]
pub(crate) fn projection_serializations_on_this_thread() -> usize {
    PROJECTION_SERIALIZATIONS.with(|count| count.get())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::MutexGuard;

    use uuid::Uuid;

    use super::*;


    struct ProjectionFixture {
        root: PathBuf,
        cache: Mutex<Option<CachedProjectionStore>>,
    }

    impl ProjectionFixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("cutex-projection-private-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            secure_directory(&root).unwrap();
            Self {
                root,
                cache: Mutex::new(None),
            }
        }

        fn entry<T>(
            &self,
            id: &str,
            action: impl FnOnce(&mut SessionProjectionEntry) -> anyhow::Result<T>,
        ) -> anyhow::Result<T> {
            with_projection_store_at(&self.root, &self.cache, |store| {
                update_projection_entry(store, id, action)
            })
        }

        fn read(&self) -> SessionProjectionStore {
            serde_json::from_slice(&fs::read(self.root.join(SESSION_PROJECTION_FILE)).unwrap())
                .unwrap()
        }
    }

    impl Drop for ProjectionFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn projection_cached_noop_skips_store_serialization_and_write() {
        let fixture = ProjectionFixture::new();
        fixture
            .entry("target", |entry| {
                entry.fingerprint = "original".into();
                Ok(())
            })
            .unwrap();
        let path = fixture.root.join(SESSION_PROJECTION_FILE);
        let stamp = crate::platform::file_stamp::FileStamp::of(&path);
        let before = fs::read(&path).unwrap();
        PROJECTION_SERIALIZATIONS.with(|count| count.set(0));
        for _ in 0..3 {
            fixture.entry("target", |entry| Ok(entry.revision)).unwrap();
        }
        assert_eq!(PROJECTION_SERIALIZATIONS.with(|count| count.get()), 0);
        assert_eq!(crate::platform::file_stamp::FileStamp::of(&path), stamp);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn projection_entry_changes_and_default_insert_persist() {
        let fixture = ProjectionFixture::new();
        fixture.entry("target", |_| Ok(())).unwrap();
        assert!(
            fixture.read().sessions.contains_key("target"),
            "default insertion is still a change"
        );
        fixture
            .entry("other", |entry| {
                entry.fingerprint = "keep".into();
                Ok(())
            })
            .unwrap();
        PROJECTION_SERIALIZATIONS.with(|count| count.set(0));
        fixture
            .entry("target", |entry| {
                entry.revision = 9;
                entry.fingerprint = "next".into();
                entry.focus.mobile_muted = true;
                entry.focus.updated_at = Some("fixture-time".into());
                Ok(())
            })
            .unwrap();
        let persisted = fixture.read();
        assert_eq!(persisted.sessions["target"].revision, 9);
        assert!(persisted.sessions["target"].focus.mobile_muted);
        assert_eq!(
            persisted.sessions["target"].focus.updated_at.as_deref(),
            Some("fixture-time")
        );
        assert_eq!(persisted.sessions["other"].fingerprint, "keep");
        assert_eq!(PROJECTION_SERIALIZATIONS.with(|count| count.get()), 1);
        // A focus-only edit must not be mistaken for an unchanged fingerprint.
        fixture
            .entry("target", |entry| {
                entry.focus.revision += 1;
                entry.focus.owner = "desktop".into();
                Ok(())
            })
            .unwrap();
        assert_eq!(fixture.read().sessions["target"].focus.owner, "desktop");
        assert_eq!(PROJECTION_SERIALIZATIONS.with(|count| count.get()), 2);
    }

    #[test]
    fn projection_external_reload_and_failed_action_do_not_reuse_stale_cache() {
        let fixture = ProjectionFixture::new();
        fixture.entry("target", |_| Ok(())).unwrap();
        let path = fixture.root.join(SESSION_PROJECTION_FILE);
        let mut external = fixture.read();
        external.sessions.get_mut("target").unwrap().fingerprint = "external".into();
        // Simulate another process under the same file protocol; atomic replace
        // invalidates the file-identity stamp even at identical mtime/length.
        crate::config::atomic::write_private_pretty_json_atomic(&path, &external, "fixture")
            .unwrap();
        PROJECTION_SERIALIZATIONS.with(|count| count.set(0));
        assert_eq!(
            fixture
                .entry("target", |entry| Ok(entry.fingerprint.clone()))
                .unwrap(),
            "external"
        );
        assert_eq!(
            PROJECTION_SERIALIZATIONS.with(|count| count.get()),
            1,
            "cold reload preserves canonicalization"
        );
        let result: anyhow::Result<()> = fixture.entry("target", |entry| {
            entry.fingerprint = "uncommitted".into();
            anyhow::bail!("fixture failure")
        });
        assert!(result.is_err());
        assert!(fixture.cache.lock().unwrap().is_none());
        assert_eq!(
            fixture
                .entry("target", |entry| Ok(entry.fingerprint.clone()))
                .unwrap(),
            "external"
        );
        fs::write(&path, br#"{"version":99,"sessions":{}}"#).unwrap();
        assert!(
            fixture.entry("target", |_| Ok(())).is_err(),
            "unknown schema is still refused"
        );
    }

    #[test]
    fn projection_other_session_events_preserve_bytes_stamp_and_revision() {
        use super::super::model::{CutexMessage, EventCorrelation, EventSource, PendingEvent};
        use crate::platform::file_stamp::FileStamp;
        let fixture = ProjectionFixture::new();
        let repository =
            EventRepository::open(fixture.root.join("events"), "fixture-host").unwrap();
        let record = |name: &str| {
            CutexSessionRecord::new_at(
                format!("cutex.{name}"),
                Some(format!("thread-{name}")),
                "fixture-host".into(),
                fixture.root.to_string_lossy().into_owned(),
                Some("fixture-profile".into()),
                "2026-10-14T00:00:00Z".into(),
            )
            .unwrap()
        };
        let mut a = record("a");
        let b = record("b");
        let project = |record: &CutexSessionRecord, metadata: &Value| {
            fixture
                .entry(&record.cutex_session_id, |entry| {
                    project_session_entry(record, true, None, None, metadata, entry)
                })
                .unwrap()
        };
        let initial_metadata = management_identity(&repository).unwrap();
        let initial_a = project(&a, &initial_metadata);
        project(&b, &initial_metadata);
        let path = fixture.root.join(SESSION_PROJECTION_FILE);
        let initial_bytes = fs::read(&path).unwrap();
        let initial_stamp = FileStamp::of(&path);
        let initial_revision = fixture.read().sessions[&a.cutex_session_id].revision;
        let event = repository
            .append(PendingEvent {
                cutex_session_id: b.cutex_session_id.clone(),
                host_id: "fixture-host".into(),
                source: EventSource::Cutex,
                schema: None,
                correlation: EventCorrelation::default(),
                native: None,
                cutex: Some(CutexMessage {
                    method: "cutex/test/event".into(),
                    params: json!({}),
                }),
            })
            .unwrap();
        let latest_metadata = management_identity(&repository).unwrap();
        assert_ne!(
            latest_metadata["eventStream"],
            initial_metadata["eventStream"]
        );
        let latest_a = project(&a, &latest_metadata);
        project(&b, &latest_metadata);
        assert_eq!(
            latest_a.pointer("/management/eventStream/latest/sequence"),
            Some(&json!(event.sequence))
        );
        assert_eq!(latest_a["revision"], initial_a["revision"]);
        assert_eq!(
            fixture.read().sessions[&a.cutex_session_id].revision,
            initial_revision
        );
        assert_eq!(fs::read(&path).unwrap(), initial_bytes);
        assert_eq!(FileStamp::of(&path), initial_stamp);
        // The summary's bulk path also produces identical persisted bytes.
        with_projection_store_at(&fixture.root, &fixture.cache, |store| {
            for record in [&a, &b] {
                project_session_entry(
                    record,
                    true,
                    None,
                    None,
                    &latest_metadata,
                    store.sessions.get_mut(&record.cutex_session_id).unwrap(),
                )?;
            }
            Ok(((), true))
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), initial_bytes);
        assert_eq!(FileStamp::of(&path), initial_stamp);

        a.display_name_override = Some("changed fixture".into());
        a.revision += 1;
        let changed_a = project(&a, &latest_metadata);
        assert_eq!(changed_a["revision"], json!(a.durable_revision()));
        assert_eq!(
            fixture.read().sessions[&a.cutex_session_id].revision,
            initial_revision + 1
        );
        assert_ne!(fs::read(&path).unwrap(), initial_bytes);
        assert_ne!(FileStamp::of(&path), initial_stamp);
        let changed_stamp = FileStamp::of(&path);
        fixture
            .entry(&a.cutex_session_id, |entry| {
                entry.focus.mobile_muted = true;
                entry.focus.revision += 1;
                project_session_entry(&a, true, None, None, &latest_metadata, entry)
            })
            .unwrap();
        assert!(
            fixture.read().sessions[&a.cutex_session_id]
                .focus
                .mobile_muted
        );
        assert_eq!(
            fixture.read().sessions[&a.cutex_session_id].revision,
            initial_revision + 2
        );
        assert_ne!(FileStamp::of(&path), changed_stamp);
    }

    #[test]
    #[ignore = "private synthetic projection performance comparison"]
    fn projection_noop_470_entries_benchmark() {
        use std::time::Instant;
        let fixture = ProjectionFixture::new();
        with_projection_store_at(&fixture.root, &fixture.cache, |store| {
            for index in 0..470 {
                let mut entry = SessionProjectionEntry::default();
                entry.fingerprint = format!("synthetic-{index:03}-{}", "x".repeat(3300));
                entry.focus.updated_at = Some("2026-10-14T00:00:00Z".into());
                store
                    .sessions
                    .insert(format!("cutex.fixture-{index:03}"), entry);
            }
            Ok(((), true))
        })
        .unwrap();
        let bytes = fs::metadata(fixture.root.join(SESSION_PROJECTION_FILE))
            .unwrap()
            .len();
        let started = Instant::now();
        PROJECTION_SERIALIZATIONS.with(|count| count.set(0));
        for _ in 0..50 {
            with_projection_store_at(&fixture.root, &fixture.cache, |store| {
                let (result, _) =
                    update_projection_entry(
                        store,
                        "cutex.fixture-000",
                        |entry| Ok(entry.revision),
                    )?;
                Ok((result, true)) // original unconditional serialize/digest path
            })
            .unwrap();
        }
        let baseline = started.elapsed();
        assert_eq!(PROJECTION_SERIALIZATIONS.with(|count| count.get()), 50);
        let started = Instant::now();
        PROJECTION_SERIALIZATIONS.with(|count| count.set(0));
        for _ in 0..50 {
            fixture
                .entry("cutex.fixture-000", |entry| Ok(entry.revision))
                .unwrap();
        }
        let optimized = started.elapsed();
        assert_eq!(PROJECTION_SERIALIZATIONS.with(|count| count.get()), 0);
        eprintln!("projection-noop fixture_entries=470 fixture_bytes={bytes} calls=50 baseline_us={} optimized_us={}", baseline.as_micros(), optimized.as_micros());
    }

    struct IsolatedTestHome {
        root: PathBuf,
        previous_home: Option<OsString>,
        _environment_guard: MutexGuard<'static, ()>,
    }

    impl IsolatedTestHome {
        fn new(prefix: &str) -> std::io::Result<Self> {
            let environment_guard = crate::config::paths::test_home_env_lock();
            let root = std::env::temp_dir().join(format!(
                "{prefix}-{}",
                &Uuid::new_v4().simple().to_string()[..8]
            ));
            create_owner_only_test_dir(&root)?;
            let previous_home = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", &root);
            }
            Ok(Self {
                root,
                previous_home,
                _environment_guard: environment_guard,
            })
        }

        fn root(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for IsolatedTestHome {
        fn drop(&mut self) {
            unsafe {
                match self.previous_home.take() {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    fn create_owner_only_test_dir(path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }

    #[cfg(not(unix))]
    fn create_owner_only_test_dir(path: &Path) -> std::io::Result<()> {
        fs::create_dir(path)
    }

    #[test]
    fn unchanged_projection_reads_do_not_rewrite_and_see_other_writers() {
        use crate::platform::file_stamp::FileStamp;
        let home = IsolatedTestHome::new("cutex-projection-cache").unwrap();
        let id = "cutex.projection-cache";
        let first = focus_resource(id).unwrap();
        let path = runtime_dir().unwrap().join("management-v2").join(SESSION_PROJECTION_FILE);
        let stamp = FileStamp::of(&path).expect("first read creates the entry");
        // Reads that change nothing neither rewrite nor alter the answer.
        for _ in 0..3 {
            assert_eq!(focus_resource(id).unwrap(), first);
        }
        assert_eq!(FileStamp::of(&path).unwrap(), stamp, "no-op reads must not rewrite");
        // A real change is written and seen.
        let focused = set_focus(id, None, "pc", false, None).unwrap();
        assert_ne!(FileStamp::of(&path).unwrap(), stamp);
        assert_eq!(focus_resource(id).unwrap(), focused);
        // Another writer (atomic replace) is seen through the cache.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut store = parse_projection_store(&path, &fs::read(&path).unwrap()).unwrap();
        store.sessions.get_mut(id).unwrap().focus.owner = "mobile".into();
        crate::config::atomic::write_private_bytes_atomic(&path, &serde_json::to_vec_pretty(&store).unwrap())
            .unwrap();
        assert_eq!(focus_resource(id).unwrap()["owner"], "mobile");
        drop(home);
    }

    #[test]
    fn backend_names_match_frozen_contract() {
        assert_eq!(
            runtime_backend_name(CutexSessionRuntimeBackend::Host),
            "host"
        );
        assert_eq!(
            runtime_backend_name(CutexSessionRuntimeBackend::HostForeground),
            "host_foreground"
        );
        assert_eq!(
            runtime_backend_name(CutexSessionRuntimeBackend::CuteAlden),
            "cute_alden"
        );
    }

    #[test]
    fn management_identity_digests_are_computed_from_embedded_bytes() {
        use sha2::Digest;

        assert_eq!(MAX_REQUEST_BYTES, 16_777_216);
        for digest in [
            cutex_method_registry_index_sha256(),
            super::super::contract_validation::cutex_method_schema_sha256(),
            super::super::native_requests::native_request_policy_sha256(),
            super::super::native_requests::native_request_allow_rules_sha256(),
        ] {
            assert_eq!(digest.len(), 64);
        }
        assert_eq!(
            cutex_method_registry_index_sha256(),
            format!("{:x}", sha2::Sha256::digest(CUTEX_METHOD_REGISTRY_INDEX.as_bytes()))
        );
    }

    #[test]
    fn runtime_defaults_use_the_frozen_v2_field_names() {
        let mut record = CutexSessionRecord::from_codex_session_id("thread-defaults")
            .expect("durable test session");
        record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        record.permission_defaults = Some("full-access".to_string());
        record.reasoning_defaults = Some("xhigh".to_string());
        record.agent_groups = vec!["waveline".to_string()];

        assert_eq!(
            runtime_defaults_resource(&record),
            json!({
                "backend": "host_foreground",
                "managedCwd": null,
                "permissions": "full-access",
                "approvalPolicy": null,
                "sandboxMode": null,
                "model": null,
                "reasoningEffort": "xhigh",
                "cliArgs": [],
                "groups": ["waveline"],
            })
        );
        record.service_tier = Some("priority".to_string());
        assert_eq!(runtime_defaults_resource(&record)["serviceTier"], "priority");
    }

    #[test]
    fn session_resource_exposes_manager_observed_native_runtime_state() {
        let mut record = CutexSessionRecord::from_codex_session_id("thread-native-state")
            .expect("durable test session");
        record.cutex_session_id = "cutex-native-state".to_string();
        record.runtime_generation = 7;
        let runtime_status = AppServerManagedRuntimeStatus {
            cutex_session_id: record.cutex_session_id.clone(),
            thread_id: "thread-native-state".to_string(),
            runtime_generation: 7,
            connected: true,
            active_turn_id: Some("turn-active".to_string()),
            active_turn_observed_at: Some("2026-07-24T00:00:03Z".to_string()),
            thread_status: Some(json!({
                "type": "active",
                "activeFlags": ["waitingOnApproval"]
            })),
            thread_status_observed_at: Some("2026-07-24T00:00:02Z".to_string()),
            thread_settings: Some(json!({
                "model": "gpt-test",
                "activePermissionProfile": { "id": ":workspace" },
                "collaborationMode": {
                    "mode": "plan",
                    "settings": {
                        "model": "",
                        "reasoning_effort": "medium",
                        "developer_instructions": null
                    }
                }
            })),
            thread_settings_source: Some("thread/settings/updated".to_string()),
            thread_settings_complete: true,
            thread_settings_observed_at: Some("2026-07-24T00:00:01Z".to_string()),
            runtime_workspace_roots: Some(json!(["/workspace", "/shared"])),
            instruction_sources: Some(json!(["/workspace/AGENTS.md"])),
            resume_snapshot_observed_at: Some("2026-07-24T00:00:00Z".to_string()),
            event_method_counts: HashMap::new(),
            initialized_user_agent: Some("test-client".to_string()),
            last_event_at: Some("2026-07-24T00:00:00Z".to_string()),
            last_error: None,
        };
        let mut entry = SessionProjectionEntry::default();
        let activity = SessionActivityState {
            revision: 4,
            runtime_generation: Some(7),
            last_output_at: Some("2026-07-24T00:00:04Z".to_string()),
            last_output_completed_at: Some("2026-07-24T00:00:05Z".to_string()),
            last_turn_completed_at: Some("2026-07-24T00:00:06Z".to_string()),
            last_file_change_at: Some("2026-07-24T00:00:03Z".to_string()),
            last_output: Some(crate::observability::SafeOutputProjection {
                association: crate::observability::ObservationAssociation::session(
                    "cutex-test-session",
                )
                .with_task("assignment-1".to_string(), Some(2)),
                class: crate::observability::SafeOutputClass::FinalVisible,
                display_text: "visible result".to_string(),
                updated_at: "2026-07-24T00:00:05Z".to_string(),
                runtime_generation: 7,
            }),
            last_tool_call: Some(crate::observability::SafeToolCallProjection {
                association: crate::observability::ObservationAssociation::session(
                    "cutex-test-session",
                )
                .with_task("assignment-1".to_string(), Some(2)),
                class: crate::observability::SafeToolCallClass::Command,
                status: crate::observability::SafeToolCallStatus::Finished,
                display_text: "Command".to_string(),
                updated_at: "2026-07-24T00:00:04Z".to_string(),
                runtime_generation: 7,
            }),
            ..SessionActivityState::default()
        };

        let resource = project_session_entry(
            &record,
            true,
            Some(runtime_status.clone()),
            Some(&activity),
            &json!({}),
            &mut entry,
        )
        .expect("session resource");

        assert_eq!(
            resource.pointer("/runtime/activeTurnId"),
            Some(&json!("turn-active"))
        );
        assert_eq!(
            resource.pointer("/runtime/activeTurnObservedAt"),
            Some(&json!("2026-07-24T00:00:03Z"))
        );
        assert_eq!(
            resource.pointer("/runtime/threadStatus"),
            Some(&json!({
                "type": "active",
                "activeFlags": ["waitingOnApproval"]
            }))
        );
        assert_eq!(
            resource.pointer("/runtime/threadStatusObservedAt"),
            Some(&json!("2026-07-24T00:00:02Z"))
        );
        assert_eq!(
            resource.pointer("/runtime/threadSettings"),
            Some(&json!({
                "model": "gpt-test",
                "activePermissionProfile": { "id": ":workspace" },
                "collaborationMode": {
                    "mode": "plan",
                    "settings": {
                        "model": "",
                        "reasoning_effort": "medium",
                        "developer_instructions": null
                    }
                }
            }))
        );
        assert_eq!(
            resource.pointer("/runtime/threadSettingsSource"),
            Some(&json!("thread/settings/updated"))
        );
        assert_eq!(
            resource.pointer("/runtime/threadSettingsComplete"),
            Some(&json!(true))
        );
        assert_eq!(
            resource.pointer("/runtime/threadSettingsObservedAt"),
            Some(&json!("2026-07-24T00:00:01Z"))
        );
        assert_eq!(
            resource.pointer("/runtime/runtimeWorkspaceRoots"),
            Some(&json!(["/workspace", "/shared"]))
        );
        assert_eq!(
            resource.pointer("/runtime/instructionSources"),
            Some(&json!(["/workspace/AGENTS.md"]))
        );
        assert_eq!(
            resource.pointer("/runtime/resumeSnapshotObservedAt"),
            Some(&json!("2026-07-24T00:00:00Z"))
        );
        assert_eq!(
            resource.get("activity"),
            Some(&json!({
                "revision": 4,
                "runtimeGeneration": 7,
                "lastOutputAt": "2026-07-24T00:00:04Z",
                "lastOutputCompletedAt": "2026-07-24T00:00:05Z",
                "lastTurnCompletedAt": "2026-07-24T00:00:06Z",
                "lastFileChangeAt": "2026-07-24T00:00:03Z",
                "lastOutput": {
                    "cutexSessionId": "cutex-test-session",
                    "assignmentId": "assignment-1",
                    "attemptNumber": 2,
                    "class": "final_visible",
                    "displayText": "visible result",
                    "updatedAt": "2026-07-24T00:00:05Z",
                    "runtimeGeneration": 7
                },
                "lastToolCall": {
                    "cutexSessionId": "cutex-test-session",
                    "assignmentId": "assignment-1",
                    "attemptNumber": 2,
                    "class": "command",
                    "status": "finished",
                    "displayText": "Command",
                    "updatedAt": "2026-07-24T00:00:04Z",
                    "runtimeGeneration": 7
                }
            }))
        );

        let mut later_activity = activity;
        later_activity.revision = 5;
        later_activity.last_output_at = Some("2026-07-24T00:00:07Z".to_string());
        let later_resource = project_session_entry(
            &record,
            true,
            Some(runtime_status),
            Some(&later_activity),
            &json!({}),
            &mut entry,
        )
        .expect("session resource with later activity");
        assert_eq!(entry.revision, 1);
        assert_eq!(later_resource["revision"], record.durable_revision());
        assert_eq!(
            later_resource.pointer("/activity/lastOutputAt"),
            Some(&json!("2026-07-24T00:00:07Z"))
        );
    }

    #[test]
    fn tui_attachment_is_not_inferred_from_the_app_server_process() {
        let mut record =
            CutexSessionRecord::from_codex_session_id("thread-tui").expect("durable test session");
        record.runtime_pid = Some(std::process::id());
        assert!(!tui_is_known_attached(&record, true));

        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.alden_pid = Some(std::process::id());
        assert!(tui_is_known_attached(&record, true));
        assert!(!tui_is_known_attached(&record, false));
    }

    #[test]
    fn legacy_known_launch_projects_unknown_source_without_relabeling() {
        let mut record = CutexSessionRecord::from_codex_session_id("thread-legacy-profile")
            .expect("durable test session");
        record.cutex_session_id = "cutex-legacy-profile".to_string();
        record.profile = Some("configured-profile".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.app_server_runtime = Some(crate::session::model::CutexAppServerRuntimeBinding {
            transport: crate::session::model::CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/legacy-profile.sock".to_string(),
            pid: 4242,
            runtime_dir: "/tmp/legacy-profile".to_string(),
            launched_profile: Some("legacy-profile".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/legacy-profile.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-15T00:00:00Z".to_string(),
        });
        let mut entry = SessionProjectionEntry::default();
        let resource = project_session_entry(&record, true, None, None, &json!({}), &mut entry)
            .expect("legacy projection");
        assert_eq!(resource["launchedProfile"], "legacy-profile");
        assert_eq!(resource["launchProfileSource"], "unknown");
        assert_eq!(resource["configuredProfile"], "configured-profile");
    }

    #[test]
    fn profile_projection_distinguishes_durable_intent_global_fallback_and_unavailable_launch() {
        let previous_home = std::env::var_os("HOME");
        let isolated_home = IsolatedTestHome::new("cutex-profile-projection-test")
            .expect("create isolated test HOME");
        let mut record = CutexSessionRecord::from_codex_session_id("thread-profile-projection")
            .expect("durable test session");
        record.cutex_session_id = "cutex-profile-projection".to_string();
        let mut entry = SessionProjectionEntry::default();

        crate::config::store::save_codez_config(&crate::profiles::model::CodezConfig::default())
            .expect("clear global profile default");
        record.profile = Some("configured".to_string());
        let configured = project_session_entry(&record, true, None, None, &json!({}), &mut entry)
            .expect("configured projection");
        assert_eq!(configured["profile"], "configured");
        assert_eq!(configured["configuredProfile"], "configured");
        assert_eq!(
            configured["effectiveNextLaunch"],
            json!({ "profile": "configured", "source": "session_configured" })
        );

        record.profile = None;
        let global = crate::profiles::model::CodezConfig {
            default_profile: Some("global".to_string()),
            ..Default::default()
        };
        crate::config::store::save_codez_config(&global).expect("save global profile default");
        let fallback = project_session_entry(&record, true, None, None, &json!({}), &mut entry)
            .expect("global fallback projection");
        assert_eq!(fallback["profile"], Value::Null);
        assert_eq!(
            fallback["effectiveNextLaunch"],
            json!({ "profile": "global", "source": "global_default" })
        );

        crate::config::store::save_codez_config(&crate::profiles::model::CodezConfig::default())
            .expect("clear global profile default again");
        let unavailable = project_session_entry(&record, true, None, None, &json!({}), &mut entry)
            .expect("unavailable projection");
        assert_eq!(
            unavailable["effectiveNextLaunch"],
            json!({ "profile": Value::Null, "source": "unavailable" })
        );
        assert!(
            isolated_home.root().join(".cutex/config.json").is_file(),
            "the test config must stay inside the isolated HOME"
        );
        drop(isolated_home);
        assert_eq!(
            std::env::var_os("HOME"),
            previous_home,
            "the isolated HOME guard must restore the process environment"
        );
    }

    #[test]
    fn im_exposure_flag_no_longer_hides_durable_sessions() {
        let registry = ImRegistry::default();
        let mut durable = CutexSessionRecord::from_codex_session_id("thread-unexposed")
            .expect("durable test session");
        durable.registration_class = AgentRegistrationClass::Persistent;
        durable.exposed_to_backend = false;

        assert!(session_is_visible(&registry, &durable));
        assert!(is_durable_management_session(&durable));
        // Local-only keeps the old flag rule.
        durable.registration_class = AgentRegistrationClass::LocalOnly;
        assert!(!session_is_visible(&registry, &durable));
        durable.exposed_to_backend = true;
        assert!(session_is_visible(&registry, &durable));
        durable.exposed_to_backend = false;

        durable.registration_class = AgentRegistrationClass::Ephemeral;
        assert!(!is_durable_management_session(&durable));
        assert!(!session_is_visible(&registry, &durable));
    }

    fn no_runtime_status(_id: &str) -> anyhow::Result<Option<AppServerManagedRuntimeStatus>> {
        Ok(None)
    }

    #[test]
    fn unexposed_persistent_agents_are_listed_but_unexposed_local_only_stay_hidden() {
        let isolated_home =
            IsolatedTestHome::new("cutex-hidden-root-test").expect("create isolated test HOME");
        let mut hidden = CutexSessionRecord::from_codex_session_id(
            "018a0000-0000-7000-8000-00000000a001",
        )
        .expect("durable test session");
        hidden.registration_class = AgentRegistrationClass::Persistent;
        hidden.exposed_to_backend = false;
        let mut shown = CutexSessionRecord::from_codex_session_id(
            "018a0000-0000-7000-8000-00000000a002",
        )
        .expect("durable test session");
        shown.registration_class = AgentRegistrationClass::Persistent;
        shown.exposed_to_backend = true;
        let mut local = CutexSessionRecord::from_codex_session_id(
            "018a0000-0000-7000-8000-00000000a003",
        )
        .expect("local-only test session");
        local.registration_class = AgentRegistrationClass::LocalOnly;
        local.exposed_to_backend = false;
        let local_id = local.cutex_session_id.clone();
        let hidden_id = hidden.cutex_session_id.clone();
        let mut store = crate::session::store::load_cutex_session_store().expect("empty store");
        store.sessions.insert(hidden_id.clone(), hidden);
        store.sessions.insert(local_id.clone(), local);
        store
            .sessions
            .insert(shown.cutex_session_id.clone(), shown.clone());
        crate::session::store::save_cutex_session_store(&store).expect("save store");
        let repository = EventRepository::open(
            isolated_home.root().join("events"),
            crate::platform::host::current_host_name(),
        )
        .expect("open event repository");
        let registry = ImRegistry::default();
        let ids = |value: &Value| {
            value["sessions"]
                .as_array()
                .expect("sessions")
                .iter()
                .map(|session| session["cutexSessionId"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        // An `expose_to_im: false` persistent agent is listed for the owner
        // like any other agent; an unexposed local-only session is not.
        let ordinary = session_list_resource(&registry, no_runtime_status, &repository)
            .expect("ordinary list");
        let mut expected = vec![hidden_id.clone(), shown.cutex_session_id.clone()];
        expected.sort();
        assert_eq!(ids(&ordinary), expected);
        // Root includeHidden still adds the hidden local-only row.
        let root = session_list_resource_with_hidden(&registry, no_runtime_status, &repository, true)
            .expect("root list");
        let mut root_expected = expected.clone();
        root_expected.push(local_id.clone());
        root_expected.sort();
        assert_eq!(ids(&root), root_expected);
        let local_row = root["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["cutexSessionId"] == local_id.as_str())
            .unwrap();
        assert_eq!(local_row["visible"], false);
        assert!(session_resource(&local_id, &registry, no_runtime_status, &repository)
            .expect("ordinary exact")
            .is_none());
        let hidden_row = ordinary["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["cutexSessionId"] == hidden_id.as_str())
            .unwrap();
        assert_eq!(hidden_row["visible"], true);

        let exact = session_resource(&hidden_id, &registry, no_runtime_status, &repository)
            .expect("ordinary exact")
            .expect("unexposed durable session resolves");
        assert_eq!(exact["visible"], true);
        let exact =
            session_resource_including_hidden(&hidden_id, &registry, no_runtime_status, &repository)
                .expect("root exact")
                .expect("session resolves for root");
        assert_eq!(exact["visible"], true);
    }

    /// PERF-05/06: an unchanged listing neither serializes nor rewrites the
    /// projection file; a record or focus change still moves the projection
    /// revision; the global default Profile is read per request, so a config
    /// change shows on the next listing.
    #[test]
    fn unchanged_listings_skip_projection_writes_and_see_config_changes() {
        use crate::platform::file_stamp::FileStamp;
        let isolated_home =
            IsolatedTestHome::new("cutex-list-projection-test").expect("create isolated test HOME");
        let mut store = crate::session::store::load_cutex_session_store().expect("empty store");
        let mut ids = Vec::new();
        for index in 0..3 {
            let mut record = CutexSessionRecord::from_codex_session_id(&format!(
                "018a0000-0000-7000-8000-00000000b00{index}"
            ))
            .expect("durable test session");
            record.registration_class = AgentRegistrationClass::Persistent;
            ids.push(record.cutex_session_id.clone());
            store.sessions.insert(record.cutex_session_id.clone(), record);
        }
        crate::session::store::save_cutex_session_store(&store).expect("save store");
        let set_default = |profile: Option<&str>| {
            crate::config::store::save_codez_config(&crate::profiles::model::CodezConfig {
                default_profile: profile.map(str::to_string),
                ..Default::default()
            })
            .expect("save global profile default")
        };
        set_default(Some("first"));
        let repository = EventRepository::open(
            isolated_home.root().join("events"),
            crate::platform::host::current_host_name(),
        )
        .expect("open event repository");
        let registry = ImRegistry::default();
        let list = || session_list_resource(&registry, no_runtime_status, &repository).expect("list");
        let launches = |value: &Value| {
            value["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|session| session["effectiveNextLaunch"]["profile"].clone())
                .collect::<Vec<_>>()
        };
        let first = list();
        assert_eq!(launches(&first), vec![json!("first"); 3]);
        let path = runtime_dir().unwrap().join("management-v2").join(SESSION_PROJECTION_FILE);
        let stamp = FileStamp::of(&path).expect("first listing writes the entries");
        let projection_revision = |id: &str| {
            parse_projection_store(&path, &fs::read(&path).unwrap()).unwrap().sessions[id].revision
        };
        let revision = projection_revision(&ids[0]);
        let serialized = projection_serializations_on_this_thread();
        for _ in 0..3 {
            assert_eq!(list()["sessions"], first["sessions"]);
        }
        assert_eq!(projection_serializations_on_this_thread(), serialized);
        assert_eq!(FileStamp::of(&path).unwrap(), stamp, "unchanged listings must not rewrite");

        // A config change is visible on the next request and counts as a change.
        set_default(Some("second"));
        assert_eq!(launches(&list()), vec![json!("second"); 3]);
        assert_eq!(projection_revision(&ids[0]), revision + 1);
        // Focus changes are still part of the fingerprint.
        set_focus(&ids[1], None, "pc", false, None).expect("focus");
        let focused = list();
        assert_eq!(focused["sessions"][1]["focus"]["owner"], "pc");
        assert_eq!(projection_revision(&ids[1]), revision + 2);
        assert_eq!(projection_revision(&ids[2]), revision + 1);
        drop(isolated_home);
    }

    #[test]
    fn retired_projection_carries_project_membership() {
        let root = std::env::temp_dir().join(format!("cutex-retired-project-{}", Uuid::new_v4()));
        let store = crate::agent_management::AgentManagementStore::open(&root).unwrap();
        let project = crate::agent_management::ProjectId::new("project-alpha").unwrap();
        let agent = |id: &str| -> crate::agent_management::ManagedAgentRecord {
            serde_json::from_value(json!({
                "project_id": "project-alpha",
                "created_by_director_session": null,
                "cutex_session_id": id,
                "native_session_id": format!("native-{id}"),
                "spec": {"name": id, "cwd": "/tmp/x", "profile": null, "runtime_backend": "app_server",
                    "model": "m", "reasoning": "medium", "permissions": "default",
                    "approval_policy": "never", "sandbox_mode": "workspace-write", "groups": []},
                "created_at": "2026-10-08T00:00:00Z",
                "retired_at": null
            }))
            .unwrap()
        };
        store
            .with_state(true, |mut state| {
                let record = agent("cutex.archived-in-project");
                state.agents.insert(record.cutex_session_id.clone(), record);
                Ok((state, (), true))
            })
            .unwrap();
        let snapshot = store.snapshot().unwrap();

        let mut record = CutexSessionRecord::from_codex_session_id("thread-retired-project")
            .expect("durable test session");
        record.cutex_session_id = "cutex.archived-in-project".to_string();
        record.archive_state = CutexSessionArchiveState::Retired;
        let mut value = retired_session_resource_value(&record, None);
        annotate_retired_session_project(&mut value, &record, Some(&snapshot));
        assert_eq!(value["projectId"], project.as_str());
        assert!(value["projectName"].is_string());

        record.cutex_session_id = "cutex.archived-independent".to_string();
        let mut value = retired_session_resource_value(&record, None);
        annotate_retired_session_project(&mut value, &record, Some(&snapshot));
        assert!(value.get("projectId").is_some_and(Value::is_null));

        // Unreadable project store: keys absent, so clients do not filter.
        let mut value = retired_session_resource_value(&record, None);
        annotate_retired_session_project(&mut value, &record, None);
        assert!(value.get("projectId").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn retired_projection_is_archive_only_and_forces_offline_runtime_identity() {
        let mut record = CutexSessionRecord::from_codex_session_id("thread-retired")
            .expect("durable test session");
        record.cutex_session_id = "cutex-retired".to_string();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.archive_state = CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        record.revision = 8;
        record.runtime_generation = 4;
        record.current_runtime_agent_id = Some("stale-runtime".to_string());
        record.note = Some("reviewer".to_string());

        let resource = retired_session_resource_value(&record, None);

        // Presentation fields stay (the desktop lists archived agents with them).
        assert_eq!(resource["note"], "reviewer");
        for key in ["avatar", "displayNameOverride", "formalAgentName", "effectiveNextLaunch"] {
            assert!(resource.get(key).is_some(), "{key}");
        }
        assert_eq!(resource["lifecycle"], "retired");
        assert_eq!(resource["retiredAt"], "2026-08-10T00:01:00Z");
        assert_eq!(resource["revision"], 8);
        assert_eq!(resource["runtime"]["status"], "offline");
        assert!(resource["runtime"]["runtimeAgentId"].is_null());
        assert!(resource.get("visible").is_none());
        assert!(resource.get("management").is_none());
        assert_eq!(
            resource.get("activity"),
            Some(&json!({
                "revision": 0,
                "runtimeGeneration": null,
                "lastOutputAt": null,
                "lastOutputCompletedAt": null,
                "lastTurnCompletedAt": null,
                "lastFileChangeAt": null
            }))
        );
    }
}
