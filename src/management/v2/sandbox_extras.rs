//! Owner-only `GET|PATCH /v2/sessions/{id}/sandbox-extras` (capability
//! `session-sandbox-extras`): network access and extra writable roots of one
//! workspace-write Codex session, changeable while it runs.
//!
//! The session record is the source of truth (`sandbox_network_access`,
//! `sandbox_writable_roots`): every start / resume / stock launch rebuilds the
//! workspace-write extras from it, so a change survives runtime restart,
//! relaunch and daemon restart. A PATCH stores the new values first, then (when
//! the runtime is connected) sends native `thread/settings/update` with the
//! whole workspace-write `sandboxPolicy`, which applies from the next turn; the
//! running turn keeps the policy it started with. The mirror of
//! `thread/settings/updated` (cli_app/app_server_state_sync.rs) then writes the
//! same values again.
//!
//! Validation: Codex sessions only (Claude agents have their own write / exec
//! rules), effective sandbox `workspace-write` (the extras mean nothing under
//! read-only or full access, and a mode switch rebuilds them), roots absolute,
//! existing directories on this host (canonicalised, Windows verbatim prefix
//! stripped), at most `MAX_WRITABLE_ROOTS`, never the cutex home or anything
//! inside it. The thread cwd is always writable and is dropped from the list.

use std::net::TcpStream;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::{ManagementNativeForwardError, ManagementRequestContext};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

pub const CAPABILITY: &str = "session-sandbox-extras";

/// `Some(cutexSessionId)` for `/v2/sessions/{id}/sandbox-extras`.
pub fn session_id_from_path(path: &str) -> Option<String> {
    let encoded = path.split('?').next()?.strip_prefix("/v2/sessions/")?.strip_suffix("/sandbox-extras")?;
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::attachments::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control)).then_some(decoded)
}

pub fn is_path(path: &str) -> bool {
    session_id_from_path(path).is_some()
}

/// PATCH body: absent fields keep the stored value; at least one is required.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxExtrasPatch {
    #[serde(default)]
    pub network_access: Option<bool>,
    #[serde(default)]
    pub writable_roots: Option<Vec<String>>,
}

/// A refused change: HTTP status, error code, message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }
}

/// Why the extras cannot be changed for this record, else None.
pub fn not_editable_reason(record: &CutexSessionRecord) -> Option<(&'static str, &'static str)> {
    if record.is_retired() {
        return Some(("session_retired", "the session is archived"));
    }
    if record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription {
        return Some(("unsupported_runtime", "Claude agents use their own write / command rules, not a Codex sandbox"));
    }
    let (sandbox, _) = crate::runtime::args::effective_runtime_permission_defaults(record);
    if sandbox.as_deref() != Some("workspace-write") {
        return Some((
            "sandbox_not_workspace_write",
            "network access and extra writable directories apply only to workspace-write (自动); switch the permission mode first",
        ));
    }
    None
}

/// The resource body for a record.
pub fn view(record: &CutexSessionRecord) -> Value {
    let (sandbox, _) = crate::runtime::args::effective_runtime_permission_defaults(record);
    let reason = not_editable_reason(record);
    json!({
        "cutexSessionId": record.cutex_session_id,
        "sandbox": sandbox,
        "networkAccess": record.sandbox_network_access.unwrap_or(false),
        "writableRoots": record.sandbox_writable_roots,
        "editable": reason.is_none(),
        "reason": reason.map(|(code, _)| code),
    })
}

fn is_inside(path: &Path, base: &Path) -> bool {
    if cfg!(windows) {
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        lower(path).starts_with(lower(base))
    } else {
        path.starts_with(base)
    }
}

/// Canonical, de-duplicated roots for `record` (see the module docs).
/// `cutex_home`: the directory no root may be or lie in.
pub fn validate_roots(record: &CutexSessionRecord, roots: &[String], cutex_home: &Path) -> Result<Vec<String>, Refusal> {
    crate::config::new_thread::validate_writable_roots(roots)
        .map_err(|error| Refusal::new(400, "invalid_writable_root", format!("{error:#}")))?;
    let home = cutex_home.canonicalize().unwrap_or_else(|_| cutex_home.to_path_buf());
    let cwd = crate::session::metadata::normalize_session_cwd(crate::session::service::cutex_session_launch_cwd(record));
    let mut kept: Vec<String> = Vec::new();
    for root in roots {
        let canonical = Path::new(root)
            .canonicalize()
            .map_err(|_| Refusal::new(400, "invalid_writable_root", format!("writable root does not exist on this host: {root}")))?;
        if !canonical.is_dir() {
            return Err(Refusal::new(400, "invalid_writable_root", format!("writable root must be a directory: {root}")));
        }
        if is_inside(&canonical, &home) {
            return Err(Refusal::new(400, "writable_root_in_cutex_home", format!("writable root is inside the cutex home: {root}")));
        }
        let text = canonical
            .to_str()
            .ok_or_else(|| Refusal::new(400, "invalid_writable_root", format!("writable root must be UTF-8: {root}")))?;
        let text = crate::agent_management::strip_verbatim_prefix(text);
        if crate::session::metadata::normalize_session_cwd(&text) == cwd || kept.contains(&text) {
            continue;
        }
        kept.push(text);
    }
    Ok(kept)
}

/// Store a validated patch on the session `key_or_id` in the session store at
/// `store_path`. Returns the updated record and whether anything changed.
pub fn apply_patch_at(
    store_path: &Path,
    key_or_id: &str,
    patch: &SandboxExtrasPatch,
    cutex_home: &Path,
) -> Result<(CutexSessionRecord, bool), Refusal> {
    if patch.network_access.is_none() && patch.writable_roots.is_none() {
        return Err(Refusal::new(400, "invalid_request", "set networkAccess and / or writableRoots"));
    }
    let unavailable = |error: anyhow::Error| Refusal::new(503, "session_store_unavailable", format!("session store: {error:#}"));
    let Some((key, _)) = crate::session::store::load_session_row(store_path, key_or_id).map_err(unavailable)? else {
        return Err(Refusal::new(404, "session_not_found", "no such session"));
    };
    let outcome = crate::session::store::with_locked_session_rows(store_path, |rows| {
        let Some(record) = rows.session(&key).cloned() else {
            return Ok(Err(Refusal::new(404, "session_not_found", "no such session")));
        };
        if let Some((code, message)) = not_editable_reason(&record) {
            return Ok(Err(Refusal::new(409, code, message)));
        }
        let roots = match &patch.writable_roots {
            Some(roots) => match validate_roots(&record, roots, cutex_home) {
                Ok(roots) => Some(roots),
                Err(refusal) => return Ok(Err(refusal)),
            },
            None => None,
        };
        let target = rows.session_mut(&key).expect("record checked above");
        let mut changed = false;
        if let Some(network) = patch.network_access {
            if target.sandbox_network_access != Some(network) {
                target.sandbox_network_access = Some(network);
                changed = true;
            }
        }
        if let Some(roots) = roots {
            if target.sandbox_writable_roots != roots {
                target.sandbox_writable_roots = roots;
                changed = true;
            }
        }
        if changed {
            target.bump_durable_revision()?;
            target.updated_at = chrono::Utc::now().to_rfc3339();
            let updated = target.clone();
            rows.commit()?;
            return Ok(Ok((updated, true)));
        }
        Ok(Ok((target.clone(), false)))
    });
    outcome.map_err(unavailable)?
}

/// Native `thread/settings/update` params that put the record's extras on
/// the live thread (whole workspace-write policy, as the native replaces it).
pub fn live_settings_params(record: &CutexSessionRecord, thread_id: &str) -> Value {
    json!({
        "threadId": thread_id,
        "sandboxPolicy": {
            "type": "workspaceWrite",
            "writableRoots": record.sandbox_writable_roots,
            "networkAccess": record.sandbox_network_access.unwrap_or(false),
            "excludeTmpdirEnvVar": false,
            "excludeSlashTmp": false,
        },
    })
}

fn error(stream: &mut TcpStream, refusal: Refusal) -> anyhow::Result<()> {
    let reason = super::server::http_reason_for(refusal.status);
    super::server::write_v2_error(stream, refusal.status, reason, refusal.code, &refusal.message, refusal.status == 503, json!({}))
}

/// Push the stored extras to the connected runtime: `{status, diagnostic?}`
/// with status `applied` (from the next turn), `offline` (next start) or
/// `failed` (stored; next start).
fn apply_live(context: ManagementRequestContext, record: &CutexSessionRecord) -> Value {
    let session = (|| -> anyhow::Result<Option<Value>> {
        let registry = (context.load_registry)()?;
        let repository = super::repository::management_v2_repository()?;
        super::session::session_resource_including_hidden(&record.cutex_session_id, &registry, context.load_runtime_status, repository)
    })();
    let session = match session {
        Ok(Some(session)) => session,
        Ok(None) => return json!({ "status": "offline" }),
        Err(error) => return json!({ "status": "failed", "diagnostic": format!("{error:#}") }),
    };
    let thread_id = session.pointer("/native/threadId").and_then(Value::as_str).filter(|id| !id.is_empty());
    let connected = session.pointer("/runtime/appServerConnected").and_then(Value::as_bool).unwrap_or(false);
    let (Some(thread_id), true) = (thread_id, connected) else {
        return json!({ "status": "offline" });
    };
    let message = json!({
        "id": format!("cutex-sandbox-extras-{}", uuid::Uuid::new_v4()),
        "method": crate::app_server::commands::THREAD_SETTINGS_UPDATE_METHOD,
        "params": live_settings_params(record, thread_id),
    });
    match (context.forward_native_request)(&record.cutex_session_id, message) {
        Ok(response) => match response.get("error") {
            Some(error) => json!({ "status": "failed", "diagnostic": error.get("message").cloned().unwrap_or_else(|| error.clone()) }),
            None => json!({ "status": "applied" }),
        },
        Err(error) => json!({ "status": "failed", "diagnostic": match error {
            ManagementNativeForwardError::BeforeForward(message)
            | ManagementNativeForwardError::NativeRequestIdInUse(message)
            | ManagementNativeForwardError::OutcomeUnknown(message) => message,
            ManagementNativeForwardError::StaleRuntimeGeneration { .. } => "stale runtime generation".to_string(),
        } }),
    }
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest, context: ManagementRequestContext) -> anyhow::Result<()> {
    let Some(id) = session_id_from_path(&request.path) else {
        return error(stream, Refusal::new(400, "invalid_request", "an exact session id is required"));
    };
    let store_path = crate::session::store::cutex_sessions_path()?;
    match request.method.as_str() {
        "GET" => match crate::session::store::load_session_row(&store_path, &id) {
            Ok(Some((_, record))) => write_json_response(stream, 200, "OK", &view(&record)),
            Ok(None) => error(stream, Refusal::new(404, "session_not_found", "no such session")),
            Err(err) => error(stream, Refusal::new(503, "session_store_unavailable", format!("session store: {err:#}"))),
        },
        "PATCH" => {
            let patch: SandboxExtrasPatch = match serde_json::from_slice(&request.body) {
                Ok(patch) => patch,
                Err(err) => return error(stream, Refusal::new(400, "invalid_request", format!("invalid body: {err}"))),
            };
            let home = crate::config::paths::config_dir()?;
            let (record, changed) = match apply_patch_at(&store_path, &id, &patch, &home) {
                Ok(outcome) => outcome,
                Err(refusal) => return error(stream, refusal),
            };
            // Unchanged values are pushed too: the live thread may differ
            // (a refused earlier push), and the update is idempotent.
            let live = apply_live(context, &record);
            let mut body = view(&record);
            body["changed"] = json!(changed);
            body["live"] = live;
            write_json_response(stream, 200, "OK", &body)
        }
        _ => error(stream, Refusal::new(405, "method_not_allowed", "use GET or PATCH for /v2/sessions/{id}/sandbox-extras")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-sandbox-extras-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn workspace_record(id: &str, cwd: &Path) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new(id.into(), Some(format!("thread-{id}")), "testhost".into(), cwd.display().to_string(), None).unwrap();
        record.sandbox_mode = Some("workspace-write".into());
        record.permission_defaults = Some("workspace-write".into());
        record.approval_policy = Some("on-request".into());
        record.managed_cwd = Some(cwd.display().to_string());
        record
    }

    fn store_with(dir: &Path, record: CutexSessionRecord) -> PathBuf {
        let path = dir.join("cutex-sessions.json");
        crate::session::store::with_locked_session_store(&path, |store| {
            store.sessions.insert(record.cutex_session_id.clone(), record);
            crate::session::store::save_locked_session_store(&path, store)
        })
        .unwrap();
        path
    }

    fn canonical(path: &Path) -> String {
        crate::agent_management::strip_verbatim_prefix(path.canonicalize().unwrap().to_str().unwrap())
    }

    #[test]
    fn paths() {
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/sandbox-extras").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/cutex.s1/sandbox-extras?x=1").as_deref(), Some("cutex.s1"));
        assert_eq!(session_id_from_path("/v2/sessions/a%2Fb/sandbox-extras"), None);
        assert_eq!(session_id_from_path("/v2/sessions//sandbox-extras"), None);
        assert!(!is_path("/v2/sessions/cutex.s1/harness"));
        assert!(matches!(
            crate::management::v2::access::classify("PATCH", "/v2/sessions/cutex.s1/sandbox-extras"),
            Some(crate::management::v2::access::Class::OwnerOnly)
        ));
    }

    #[test]
    fn patch_body_rejects_unknown_fields() {
        assert!(serde_json::from_str::<SandboxExtrasPatch>(r#"{"sandbox":"danger-full-access"}"#).is_err());
        let patch: SandboxExtrasPatch = serde_json::from_str(r#"{"networkAccess":false}"#).unwrap();
        assert_eq!(patch.network_access, Some(false));
        assert!(patch.writable_roots.is_none());
    }

    #[test]
    fn set_then_relaunch_keeps_network_and_roots() {
        let dir = temp_dir("roundtrip");
        let cwd = dir.join("work");
        let extra = dir.join("extra");
        let home = dir.join(".cutex");
        for d in [&cwd, &extra, &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path = store_with(&dir, workspace_record("cutex.rt", &cwd));
        let patch = SandboxExtrasPatch {
            network_access: Some(false),
            // The cwd and a duplicate are dropped; the path is canonicalised.
            writable_roots: Some(vec![extra.display().to_string(), cwd.display().to_string(), format!("{}/", extra.display())]),
        };
        let (record, changed) = apply_patch_at(&path, "cutex.rt", &patch, &home).unwrap();
        assert!(changed);
        assert_eq!(record.sandbox_network_access, Some(false));
        assert_eq!(record.sandbox_writable_roots, vec![canonical(&extra)]);
        let (_, changed) = apply_patch_at(&path, "cutex.rt", &patch, &home).unwrap();
        assert!(!changed, "same values: no new revision");

        // Turn network on, keep the roots (absent field).
        let (record, _) = apply_patch_at(&path, "cutex.rt", &SandboxExtrasPatch { network_access: Some(true), writable_roots: None }, &home).unwrap();
        assert_eq!(record.sandbox_writable_roots, vec![canonical(&extra)]);

        // "Relaunch": read the record back from the store, as every launch
        // does, and build the native start / resume params from it.
        let (_, stored) = crate::session::store::load_session_row(&path, "cutex.rt").unwrap().unwrap();
        assert_eq!(stored.sandbox_network_access, Some(true));
        assert_eq!(stored.sandbox_writable_roots, vec![canonical(&extra)]);
        let mut fresh = stored.clone();
        fresh.codex_session_id = None;
        let start = crate::app_server::runtime::thread_start_params_for_session(&fresh, None).unwrap();
        let start = serde_json::to_value(start).unwrap();
        assert_eq!(start["sandbox"], "workspace-write");
        assert_eq!(start["config"]["sandbox_workspace_write"]["network_access"], true);
        assert_eq!(start["config"]["sandbox_workspace_write"]["writable_roots"], json!([canonical(&extra)]));
        let resume = serde_json::to_value(crate::app_server::runtime::thread_resume_params_for_session(&stored).unwrap()).unwrap();
        assert_eq!(resume["config"]["sandbox_workspace_write"]["network_access"], true);
        assert_eq!(resume["config"]["sandbox_workspace_write"]["writable_roots"], json!([canonical(&extra)]));

        // Clearing works the same way.
        let (record, _) = apply_patch_at(&path, "cutex.rt", &SandboxExtrasPatch { network_access: Some(false), writable_roots: Some(vec![]) }, &home).unwrap();
        assert!(record.sandbox_writable_roots.is_empty());
        let live = live_settings_params(&record, "thread-1");
        assert_eq!(live["sandboxPolicy"], json!({"type": "workspaceWrite", "writableRoots": [], "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false}));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_unsafe_or_meaningless_changes() {
        let dir = temp_dir("refuse");
        let cwd = dir.join("work");
        let home = dir.join(".cutex");
        let inside = home.join("runtime");
        for d in [&cwd, &inside] {
            std::fs::create_dir_all(d).unwrap();
        }
        let file = dir.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let path = store_with(&dir, workspace_record("cutex.rf", &cwd));
        let roots = |roots: Vec<String>| SandboxExtrasPatch { network_access: None, writable_roots: Some(roots) };
        let code = |patch: SandboxExtrasPatch| apply_patch_at(&path, "cutex.rf", &patch, &home).unwrap_err().code;
        assert_eq!(code(SandboxExtrasPatch::default()), "invalid_request");
        assert_eq!(code(roots(vec!["relative/dir".into()])), "invalid_writable_root");
        assert_eq!(code(roots(vec![dir.join("missing").display().to_string()])), "invalid_writable_root");
        assert_eq!(code(roots(vec![file.display().to_string()])), "invalid_writable_root");
        assert_eq!(code(roots(vec![home.display().to_string()])), "writable_root_in_cutex_home");
        assert_eq!(code(roots(vec![inside.display().to_string()])), "writable_root_in_cutex_home");
        assert_eq!(apply_patch_at(&path, "cutex.none", &roots(vec![]), &home).unwrap_err().code, "session_not_found");

        // Full access: nothing to change; Claude: not a Codex sandbox.
        let mut full = workspace_record("cutex.full", &cwd);
        full.sandbox_mode = Some("danger-full-access".into());
        full.permission_defaults = Some("danger-full-access".into());
        let path = store_with(&dir, full);
        let refusal = apply_patch_at(&path, "cutex.full", &SandboxExtrasPatch { network_access: Some(true), writable_roots: None }, &home).unwrap_err();
        assert_eq!((refusal.status, refusal.code), (409, "sandbox_not_workspace_write"));
        let mut claude = workspace_record("cutex.claude", &cwd);
        claude.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        assert_eq!(not_editable_reason(&claude).map(|r| r.0), Some("unsupported_runtime"));
        assert_eq!(view(&claude)["editable"], false);
        assert_eq!(view(&workspace_record("cutex.v", &cwd))["editable"], true);
        std::fs::remove_dir_all(&dir).ok();
    }
}
