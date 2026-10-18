//! Owner-only host diagnostics. A finding is an observation, never permission
//! to kill a process. Fixes recheck the observed occurrence under writer locks.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::server::ManagementRequestContext;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::TcpStream,
    path::Path,
};

pub const PATH: &str = "/v2/health";
pub const FIX_PATH: &str = "/v2/health/fix";
pub fn is_path(path: &str) -> bool {
    matches!(path.split('?').next(), Some(PATH | FIX_PATH))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    pub action: String,
    pub safe: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    pub code: String,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cutex_session_id: Option<String>,
    pub detail: Value,
    pub fix: Option<Fix>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FixRequest {
    #[serde(default)]
    pub finding_ids: Option<Vec<String>>,
    #[serde(default = "yes")]
    pub dry_run: bool,
}
fn yes() -> bool {
    true
}

fn finding(
    code: &str,
    record: Option<&CutexSessionRecord>,
    detail: Value,
    action: Option<&str>,
) -> Finding {
    // The identity includes the observed runtime occurrence. A reused session
    // or changed PID cannot accidentally authorize a different cleanup.
    let evidence = json!([
        code,
        record.map(|r| (
            &r.cutex_session_id,
            r.revision,
            r.runtime_generation,
            r.runtime_pid,
            r.alden_pid,
            &r.app_server_runtime,
            &r.current_runtime_agent_id,
            &r.app_server_launch_claim_id,
            &r.pending_launch_id
        )),
        detail
    ]);
    Finding {
        id: format!("health-{:x}", Sha256::digest(evidence.to_string())),
        code: code.into(),
        severity: "warn".into(),
        cutex_session_id: record.map(|r| r.cutex_session_id.clone()),
        detail,
        fix: action.map(|action| Fix {
            action: action.into(),
            safe: true,
        }),
    }
}

fn pids(record: &CutexSessionRecord) -> Vec<u32> {
    record
        .runtime_pid
        .into_iter()
        .chain(record.alden_pid)
        .chain(record.app_server_runtime.as_ref().map(|b| b.pid))
        .collect()
}

/// Fail closed on process-query errors. Windows' general-purpose helper falls
/// back to tasklist and treats a failed fallback as absence, which is unsuitable
/// for an owner cleanup decision (and expensive for many stale records).
/// Also the liveness check of the Claude dead-adapter cleanup.
pub(crate) fn process_may_be_running(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, STILL_ACTIVE,
        };
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut exit_code = 0;
        let known = GetExitCodeProcess(handle, &mut exit_code) != 0;
        CloseHandle(handle);
        !known || exit_code == STILL_ACTIVE as u32
    }
    #[cfg(not(windows))]
    {
        crate::platform::process::process_is_running(pid)
    }
}
fn local(record: &CutexSessionRecord) -> bool {
    crate::runtime::lifecycle::cutex_session_host_is_local(
        &record.host_id,
        &crate::platform::host::current_host_name(),
    ) && matches!(
        record.runtime_backend,
        CutexSessionRuntimeBackend::Host
            | CutexSessionRuntimeBackend::HostForeground
            | CutexSessionRuntimeBackend::ClaudeSubscription
    )
}
fn provably_dead(record: &CutexSessionRecord, running: &dyn Fn(u32) -> bool) -> bool {
    let pids = pids(record);
    local(record)
        && !pids.is_empty()
        && pids.iter().all(|pid| *pid > 0 && !running(*pid))
        && record.app_server_launch_claim_id.is_none()
        && record.pending_launch_id.is_none()
}

fn session_findings(store: &CutexSessionStore, running: &dyn Fn(u32) -> bool) -> Vec<Finding> {
    let mut ids: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for record in store.sessions.values() {
        if let Some(id) = &record.current_runtime_agent_id {
            ids.entry(id).or_default().push(&record.cutex_session_id);
        }
    }
    for sessions in ids.values_mut() {
        sessions.sort_unstable();
    }
    let mut result = Vec::new();
    for record in store.sessions.values().filter(|record| local(record)) {
        let dead = provably_dead(record, running);
        let observed_dead =
            !pids(record).is_empty() && pids(record).iter().all(|pid| *pid > 0 && !running(*pid));
        let clear = dead.then_some("clear_runtime_fields");
        if observed_dead {
            result.push(finding("stale_runtime_claim", Some(record), json!({"pids":pids(record),"criterion":"all recorded PIDs absent; pending launch evidence prevents cleanup"}), clear));
        }
        if let Some(id) = &record.current_runtime_agent_id {
            if ids[id.as_str()].len() > 1 {
                result.push(finding("duplicate_runtime_agent_id", Some(record), json!({"runtimeAgentId":id,"sessions":ids[id.as_str()],"criterion":"same runtime identity claimed by multiple records"}), clear));
            }
            // Claude binds its connection in claude_session, not app_server_runtime.
            let has_binding = match record.runtime_backend {
                CutexSessionRuntimeBackend::ClaudeSubscription => record
                    .claude_session
                    .as_ref()
                    .and_then(|session| session.connection_id.as_deref())
                    .is_some_and(|connection| {
                        !connection.is_empty() && *id == format!("claude.{connection}")
                    }),
                _ => record.app_server_runtime.is_some(),
            };
            if !has_binding {
                result.push(finding("orphan_runtime_id", Some(record), json!({"runtimeAgentId":id,"criterion":"runtime identity without binding; missing or live PID is report-only"}), clear));
            }
        }
        if record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription && observed_dead
        {
            result.push(finding(
                "claude_dead_adapter",
                Some(record),
                json!({"pids":pids(record),"criterion":"adapter PID absent; no pending launch"}),
                clear,
            ));
        }
        if let Some(claude) = &record.claude_session {
            let missing: Vec<_> = [
                &claude.deployment.python,
                &claude.deployment.adapter_source,
                &claude.deployment.claude_cli,
                &claude.deployment.tui_binary,
            ]
            .into_iter()
            .filter(|path| matches!(path.try_exists(), Ok(false)))
            .map(|path| path.display().to_string())
            .collect();
            if !missing.is_empty() {
                result.push(finding("missing_runtime_package", Some(record), json!({"missing":missing,
                    "criterion":"pinned Claude deployment files absent","nextAction":"owner must repair the pinned Claude deployment"}), None));
            }
        }
        if let Some(contract) = &record.explicit_launch {
            let mut package = super::runtime_packages::PackageSummary::from_manifest(
                &contract.bundle_manifest,
                None,
            );
            if matches!(contract.bundle_manifest.try_exists(), Ok(false)) {
                package
                    .missing
                    .push(contract.bundle_manifest.display().to_string());
            }
            if !package.missing.is_empty() {
                let repairable = !crate::session::archive::record_has_runtime_claim(record)
                    && record.agent_enabled
                    && !record.is_retired()
                    && record.runtime_backend == CutexSessionRuntimeBackend::Host
                    && record.registration_class
                        == crate::agent_bus::model::AgentRegistrationClass::Persistent
                    && crate::agent_management::missing_package_files(contract)
                        .is_ok_and(|files| !files.is_empty());
                result.push(finding("missing_runtime_package", Some(record), json!({"missing":package.missing,
                    "criterion":"pinned package files absent","nextAction":"runtime-package review/repair; native thread and selected package must validate"}), repairable.then_some("repair_runtime_package")));
            }
        }
    }
    result
}

#[cfg(unix)]
fn profile_directory_findings(root: &Path) -> anyhow::Result<Vec<Finding>> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("inspect profiles directory permissions"),
    };
    let mut directories = vec![(root.to_path_buf(), metadata)];
    for entry in std::fs::read_dir(root).context("list profile directories for health scan")? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            directories.push((entry.path(), metadata));
        }
    }
    directories.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(directories
        .into_iter()
        .filter_map(|(path, metadata)| {
            let mode = metadata.permissions().mode() & 0o7777;
            (mode & 0o022 != 0).then(|| {
                finding(
                    "profile_directory_writable",
                    None,
                    json!({
                        "path":path,
                        "mode":format!("{mode:04o}"),
                        "criterion":"profile auth ancestor is group/other writable; profile launches may be refused",
                        "nextAction":"Owner: review this directory, then run chmod 0700 on this exact path. Existing permissions are never changed automatically.",
                        "nextActionZhCN":"请由所有者检查此目录，再对这个确切路径执行 chmod 0700。现有目录权限不会自动修改。",
                        "repairCommand":["chmod","0700","--",path]
                    }),
                    None,
                )
            })
        })
        .collect())
}

/// Codex profiles whose official Z.ai / BigModel base URL is the Chat
/// Completions path (`/api/coding/paas/v4`): Codex speaks the Responses API,
/// which those vendors serve at `/api/v1`. Report-only: the owner repairs
/// the profile (`PATCH /v2/profiles/{id} {"providerBaseUrl": …}`).
fn profile_endpoint_findings(accounts: &[crate::profiles::model::StoredAccount]) -> Vec<Finding> {
    accounts
        .iter()
        .filter(|account| account.cli_kind == crate::profiles::model::CliKind::Codex)
        .filter_map(|account| {
            let base_url = crate::profiles::inspect::account_model_api_base(account)?;
            let suggested = crate::profiles::providers::glm_responses_base_url_for(&base_url)?;
            Some(finding(
                "profile_chat_completions_endpoint",
                None,
                json!({
                    "profileId": account.id,
                    "profileName": account.name,
                    "baseUrl": base_url,
                    "suggestedBaseUrl": suggested,
                    "criterion": "official GLM Coding Plan Chat Completions path; Codex profiles need the Responses endpoint",
                    "nextAction": format!("Owner: change this profile's base URL to {suggested} (PATCH /v2/profiles/{{id}} providerBaseUrl). Profiles are never rewritten automatically."),
                    "nextActionZhCN": format!("请由所有者把此 Profile 的 Base URL 改为 {suggested}（PATCH /v2/profiles/{{id}} providerBaseUrl）。不会自动修改 Profile。"),
                }),
                None,
            ))
        })
        .collect()
}

fn load_accounts() -> anyhow::Result<Vec<crate::profiles::model::StoredAccount>> {
    let path = crate::config::paths::accounts_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice::<crate::profiles::model::AccountsStore>(&bytes)
            .context("parse profile store for health scan")?
            .accounts),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("read profile store for health scan"),
    }
}

pub fn scan() -> anyhow::Result<Value> {
    let store = crate::session::store::load_cutex_session_store_shared()?;
    let mut findings = session_findings(&store, &process_may_be_running);
    // Report-only and best-effort: a profile store problem must not hide the
    // other findings.
    if let Ok(accounts) = load_accounts() {
        findings.extend(profile_endpoint_findings(&accounts));
    }
    #[cfg(unix)]
    findings.extend(profile_directory_findings(
        &crate::profiles::materialize::materialized_profiles_dir()?,
    )?);
    let repository = super::agent_bus_state::agent_bus_message_repository()?;
    let config = crate::config::store::load_codez_config();
    let bus = crate::agent_bus::client::AgentBusHttpClient::from_config(&config);
    let stuck = super::agent_bus_stuck::list_view(
        repository,
        &bus,
        super::agent_bus_stuck::DEFAULT_MIN_AGE_SECS,
        None,
        1,
        crate::platform::now_epoch_secs(),
        &|_| None,
    )?;
    let alarms = crate::agent_bus::alarms::list(None)?;
    findings.extend(bus_findings(&stuck, &alarms, &|id| {
        Ok(repository.message_state(id)?.is_some())
    })?);
    Ok(
        json!({"contractVersion":super::model::CONTRACT_VERSION,"scannedAt":chrono::Utc::now().to_rfc3339(),"findings":findings}),
    )
}

fn bus_findings(
    stuck: &Value,
    alarms: &Value,
    exists: &dyn Fn(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<Vec<Finding>> {
    let mut findings = Vec::new();
    if stuck["total"].as_u64().unwrap_or(0) > 0 {
        findings.push(finding("stuck_bus_messages", None, json!({"count":stuck["total"],"nextAction":super::agent_bus_stuck::LIST_PATH,"liveQueue":stuck["liveQueue"]}), None));
    }
    for alarm in alarms.as_array().into_iter().flatten() {
        if alarm["state"] == "queued" {
            if let Some(message_id) = alarm["message_id"].as_str() {
                if !exists(message_id)? {
                    findings.push(finding("lost_alarms", None, json!({"alarmId":alarm["alarm_id"],"messageId":message_id,"to":alarm["to"],
                        "criterion":"queued alarm without durable Bus record; report only, no replay"}), None));
                }
            }
        }
    }
    Ok(findings)
}

/// Apply only a still-identical observation. Re-evaluated inside the shared
/// cross-process session writer lock, and Claude's controller lock if needed.
fn clear_observed(
    path: &Path,
    expected: &Finding,
    running: &dyn Fn(u32) -> bool,
) -> anyhow::Result<bool> {
    let id = expected
        .cutex_session_id
        .as_deref()
        .context("finding has no session")?;
    let before = crate::session::store::load_cutex_session_store_from_path(path)?;
    let Some(record) = before.sessions.get(id) else {
        return Ok(false);
    };
    let _controller = record
        .claude_session
        .as_ref()
        .map(crate::runtime::claude::lock)
        .transpose()?;
    crate::session::store::with_locked_session_rows(path, |rows| {
        let present = session_findings(rows.committed(), running)
            .into_iter()
            .any(|f| f == *expected && f.fix.is_some());
        if !present {
            return Ok(false);
        }
        let record = rows
            .session_mut(id)
            .context("finding session disappeared")?;
        crate::session::runtime_reconciliation::clear_runtime_fields(record, false);
        rows.commit()?;
        Ok(true)
    })
}

/// A fix that stopped part way: the error, and what was already done
/// (those fixes stay applied: each wrote its audit record and event).
#[derive(Debug)]
pub struct FixFailure {
    pub error: anyhow::Error,
    pub applied: Vec<String>,
    pub skipped: Vec<Value>,
    /// The finding whose fix failed (`None`: failed before or after the fixes).
    pub failed: Option<String>,
}

impl FixFailure {
    fn before_any(error: anyhow::Error) -> Self {
        Self { error, applied: Vec::new(), skipped: Vec::new(), failed: None }
    }

    /// `finding_changed` or `health_fix_failed`.
    pub fn code(&self) -> &'static str {
        if format!("{:#}", self.error).starts_with("finding_changed:") {
            "finding_changed"
        } else {
            "health_fix_failed"
        }
    }

    /// Error `details`: the fixes applied before the failure and the one
    /// that failed, so a caller knows what changed.
    pub fn details(&self) -> Value {
        json!({"applied": self.applied, "skipped": self.skipped, "failedFindingId": self.failed})
    }
}

impl std::fmt::Display for FixFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)?;
        if !self.applied.is_empty() {
            write!(f, " (already applied: {})", self.applied.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for FixFailure {}

struct FixOutcome {
    applied: Vec<String>,
    skipped: Vec<Value>,
    planned: Vec<Value>,
}

/// Walk the selected findings in order; `apply_one` fixes one finding and
/// returns the session it resolved. Stops at the first failure, reporting
/// what was applied before it.
fn apply_findings(
    findings: &[Finding],
    selected: &BTreeSet<String>,
    dry_run: bool,
    mut apply_one: impl FnMut(&Finding) -> anyhow::Result<String>,
) -> Result<FixOutcome, FixFailure> {
    let mut outcome = FixOutcome { applied: Vec::new(), skipped: Vec::new(), planned: Vec::new() };
    let mut cleared = BTreeSet::new();
    for finding in findings.iter().filter(|f| selected.contains(&f.id)) {
        if finding.fix.is_none() {
            outcome.skipped.push(json!({"id":finding.id,"reason":"report_only"}));
            continue;
        }
        if dry_run {
            outcome.planned.push(json!({"id":finding.id,"fix":finding.fix}));
            continue;
        }
        if finding
            .cutex_session_id
            .as_ref()
            .is_some_and(|id| cleared.contains(id))
        {
            outcome.skipped.push(json!({"id":finding.id,"reason":"resolved_by_previous_fix"}));
            continue;
        }
        match apply_one(finding) {
            Ok(session) => {
                cleared.insert(session);
                outcome.applied.push(finding.id.clone());
            }
            Err(error) => {
                return Err(FixFailure {
                    error,
                    applied: outcome.applied,
                    skipped: outcome.skipped,
                    failed: Some(finding.id.clone()),
                })
            }
        }
    }
    Ok(outcome)
}

pub fn fix(request: &FixRequest, context: ManagementRequestContext) -> Result<Value, FixFailure> {
    let initial = scan().map_err(FixFailure::before_any)?;
    let findings: Vec<Finding> = serde_json::from_value(initial["findings"].clone())
        .map_err(|error| FixFailure::before_any(error.into()))?;
    let selected: BTreeSet<String> = request
        .finding_ids
        .clone()
        .unwrap_or_else(|| findings.iter().map(|f| f.id.clone()).collect())
        .into_iter()
        .collect();
    let known: BTreeSet<_> = findings.iter().map(|f| f.id.as_str()).collect();
    if !selected.iter().all(|id| known.contains(id.as_str())) {
        return Err(FixFailure::before_any(anyhow::anyhow!(
            "finding_changed: selected finding no longer exists; scan again"
        )));
    }
    let outcome = apply_findings(&findings, &selected, request.dry_run, |finding| {
        apply_fix(finding, context)
    })?;
    let after = scan().map_err(|error| FixFailure {
        error,
        applied: outcome.applied.clone(),
        skipped: outcome.skipped.clone(),
        failed: None,
    })?;
    Ok(
        json!({"applied":outcome.applied,"skipped":outcome.skipped,"planned":outcome.planned,"dryRun":request.dry_run,"findings":after["findings"]}),
    )
}

/// Apply one finding's fix; returns the session it resolved.
fn apply_fix(finding: &Finding, context: ManagementRequestContext) -> anyhow::Result<String> {
    if finding
        .fix
        .as_ref()
        .is_some_and(|fix| fix.action == "repair_runtime_package")
    {
        repair_package(finding, context)?;
    } else {
        anyhow::ensure!(
            clear_observed(
                &crate::session::store::cutex_sessions_path()?,
                finding,
                &process_may_be_running
            )?,
            "finding_changed: runtime evidence changed; scan again"
        );
    }
    let id = finding.cutex_session_id.clone().context("finding without a session")?;
    crate::agent_bus::audit::append_agent_bus_audit_record(
        json!({"event":"health_fixed","timestamp":chrono::Utc::now().to_rfc3339(),"findingId":finding.id,"code":finding.code,"cutexSessionId":id,"action":finding.fix}),
    )?;
    if finding
        .fix
        .as_ref()
        .is_some_and(|fix| fix.action != "repair_runtime_package")
    {
        let snapshot = crate::session::store::load_cutex_session_store_shared()?;
        let row = snapshot
            .sessions
            .get(&id)
            .context("repaired session absent")?;
        super::server::append_cutex_event(
            super::repository::management_v2_repository()?,
            &id,
            Default::default(),
            "cutex/runtime/offline",
            json!({"runtimeGeneration":row.runtime_generation,"backend":row.runtime_backend,"status":"offline","reason":format!("health: {}", finding.code),"occurredAt":chrono::Utc::now().to_rfc3339()}),
        )?;
    }
    Ok(id)
}

fn repair_package(finding: &Finding, context: ManagementRequestContext) -> anyhow::Result<()> {
    use crate::agent_management::ExplicitLaunchRequest;
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    let id = crate::role_revision::CutexSessionId::new(
        finding
            .cutex_session_id
            .clone()
            .context("package session absent")?,
    )
    .map_err(|e| anyhow::anyhow!("invalid session id: {e:?}"))?;
    let current = crate::session::store::load_cutex_session_store_shared()?;
    anyhow::ensure!(
        session_findings(&current, &process_may_be_running).contains(finding),
        "finding_changed: package evidence changed"
    );
    // Both existing operations recheck the native thread, offline state,
    // generation, contract and selected package under their mutation locks.
    let value = (context.explicit_launch_action)(
        &principal,
        &ExplicitLaunchRequest::ReviewRuntimePackageUpgrade {
            cutex_session_id: id,
        },
    )
    .map_err(|e| anyhow::anyhow!("finding_changed: package repair review refused: {e}"))?;
    let review: crate::agent_management::RuntimePackageUpgradeReview =
        serde_json::from_value(value)?;
    anyhow::ensure!(
        review.repair.is_some(),
        "finding_changed: package no longer requires repair"
    );
    (context.explicit_launch_action)(
        &principal,
        &ExplicitLaunchRequest::ApplyRuntimePackageUpgrade {
            action_id: crate::agent_management::AgentActionId::new(finding.id.clone())?,
            review,
        },
    )
    .map_err(|e| anyhow::anyhow!("finding_changed: package repair refused: {e}"))?;
    Ok(())
}

pub fn start_scan() {
    if !crate::config::store::load_codez_config()
        .health
        .scan_on_start
    {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("cutex-health-scan".into())
        .spawn(|| match scan() {
            Ok(report) => {
                if let Some(findings) = report["findings"].as_array() {
                    for f in findings {
                        if f["severity"] == "warn" || f["severity"] == "error" {
                            eprintln!("health warning: {} ({})", f["code"], f["id"]);
                        }
                    }
                }
            }
            Err(error) => eprintln!("health scan unavailable: {error:#}"),
        });
}

fn error(stream: &mut TcpStream, status: u16, code: &str, message: &str) -> anyhow::Result<()> {
    super::server::write_v2_error(
        stream,
        status,
        match status {
            400 => "Bad Request",
            405 => "Method Not Allowed",
            503 => "Service Unavailable",
            _ => "Conflict",
        },
        code,
        message,
        false,
        json!({}),
    )
}
pub fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    match (
        request.method.as_str(),
        request.path.split('?').next().unwrap_or_default(),
    ) {
        ("GET", PATH) => match scan() {
            Ok(report) => write_json_response(stream, 200, "OK", &report),
            Err(e) => error(stream, 503, "health_unavailable", &format!("{e:#}")),
        },
        ("POST", FIX_PATH) => {
            let parsed: FixRequest = match serde_json::from_slice(&request.body) {
                Ok(parsed) => parsed,
                Err(e) => return error(stream, 400, "invalid_request", &e.to_string()),
            };
            if parsed.finding_ids.as_ref().is_some_and(|ids| {
                ids.len() > 1000 || ids.iter().any(|id| id.is_empty() || id.len() > 200)
            }) {
                return error(
                    stream,
                    400,
                    "invalid_request",
                    "findingIds accepts at most 1000 nonempty IDs of at most 200 bytes",
                );
            }
            let key = match request
                .headers
                .get("idempotency-key")
                .filter(|s| !s.trim().is_empty() && s.len() <= 200)
            {
                Some(key) => key,
                None => {
                    return error(
                        stream,
                        400,
                        "invalid_request",
                        "Idempotency-Key is required (1-200 bytes)",
                    )
                }
            };
            let repo = super::request_idempotency::request_idempotency_repository()?;
            use super::request_idempotency::BeginRequest;
            match repo.begin(FIX_PATH, key, &serde_json::to_value(&parsed)?)? {
                BeginRequest::Completed(response) => {
                    write_json_response(stream, response.status, &response.reason, &response.body)
                }
                BeginRequest::Forward(claim) => {
                    let (status, body) = match fix(&parsed, context) {
                        Ok(result) => (200, result),
                        // Lists the fixes applied before the failure.
                        Err(failure) => (
                            409,
                            json!({"contractVersion":2,"error":{"source":"cutex","code":failure.code(),"message":format!("{:#}", failure.error),"retryable":false,"details":failure.details()}}),
                        ),
                    };
                    let reason = if status == 200 { "OK" } else { "Conflict" };
                    repo.complete(&claim, status, reason, body.clone())?;
                    write_json_response(stream, status, reason, &body)
                }
                BeginRequest::Conflict => error(
                    stream,
                    409,
                    "idempotency_conflict",
                    "Idempotency-Key was used with different arguments",
                ),
                _ => error(
                    stream,
                    409,
                    "request_in_progress",
                    "Request is pending or its outcome is unknown; inspect health before retrying",
                ),
            }
        }
        _ => error(
            stream,
            405,
            "method_not_allowed",
            "use GET /v2/health or POST /v2/health/fix",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_profiles_on_the_chat_completions_path_get_a_report_only_warning() {
        use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let create = |name: &str, preset: &str, base_url: Option<&str>| {
            create_profile(&NewProfile {
                name: name.into(),
                kind: NewProfileKind::ApiKey,
                preset: crate::profiles::providers::find_preset(preset),
                base_url: base_url.map(str::to_string),
                model: None,
                api_key: None,
            })
            .unwrap()
        };
        let old = create("old-glm", "zai", Some("https://api.z.ai/api/coding/paas/v4"));
        let fixed = create("new-glm", "zai", None);
        let relay = create("relay", "custom", Some("https://relay.example.com/api/coding/paas/v4"));
        let findings = profile_endpoint_findings(&[old.clone(), fixed, relay]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "profile_chat_completions_endpoint");
        assert_eq!(findings[0].detail["profileId"], old.id);
        assert_eq!(findings[0].detail["suggestedBaseUrl"], "https://api.z.ai/api/v1");
        assert!(findings[0].fix.is_none());
    }
    fn record(id: &str, pid: Option<u32>) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            id.into(),
            None,
            crate::platform::host::current_host_name(),
            std::env::temp_dir().display().to_string(),
            None,
            "2026-10-11T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_pid = pid;
        record.current_runtime_agent_id = Some("same-runtime".into());
        record
    }
    fn store(records: Vec<CutexSessionRecord>) -> CutexSessionStore {
        CutexSessionStore {
            sessions: records
                .into_iter()
                .map(|r| (r.cutex_session_id.clone(), r))
                .collect(),
            ..Default::default()
        }
    }
    fn claude_record(root: &Path) -> CutexSessionRecord {
        let mut r = record("cutex.claude", Some(43210));
        r.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        r.current_runtime_agent_id = Some("claude.connection".into());
        r.claude_session = Some(crate::runtime::claude::ClaudeSession {
            deployment: crate::runtime::claude::ClaudeDeployment {
                python: root.join("python"),
                adapter_source: root.join("claude-r9"),
                claude_cli: root.join("claude"),
                claude_home: root.join("account"),
                tui_binary: root.join("tui"),
            },
            thread_id: "thread".into(),
            state_dir: root.join("state"),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: true,
            allow_exec: true,
            write_explicit: false,
            exec_explicit: false,
            native_session_id: Some("native-session".into()),
            connection_id: Some("connection".into()),
            initial_permission_mode: None,
        });
        r
    }
    #[test]
    fn claude_connection_is_bound_even_with_an_older_pinned_deployment() {
        let root = crate::config::paths::runtime_dir()
            .unwrap()
            .join(format!("health-claude-{}", uuid::Uuid::new_v4()));
        for name in ["claude-r9", "claude-r10", "account"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        for name in ["python", "claude", "tui"] {
            std::fs::write(root.join(name), b"fixture").unwrap();
        }
        let r = claude_record(&root);
        assert!(r.app_server_runtime.is_none());
        assert!(session_findings(&store(vec![r.clone()]), &|_| true).is_empty());
        let dead = session_findings(&store(vec![r.clone()]), &|_| false);
        assert_eq!(dead.len(), 2);
        for code in ["stale_runtime_claim", "claude_dead_adapter"] {
            assert!(dead.iter().any(|f| f.code == code && f.fix.is_some()));
        }
        std::fs::remove_dir(root.join("claude-r9")).unwrap();
        let missing = session_findings(&store(vec![r]), &|_| true);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].code, "missing_runtime_package");
        assert!(missing[0].fix.is_none());
        assert_eq!(missing[0].detail["missing"], json!([root.join("claude-r9")]));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn claude_missing_or_mismatched_connections_are_still_orphans() {
        for connection in [None, Some(""), Some("different-connection")] {
            let mut r = claude_record(Path::new("absent-health-fixture"));
            r.claude_session.as_mut().unwrap().connection_id = connection.map(str::to_owned);
            for running in [false, true] {
                let findings = session_findings(&store(vec![r.clone()]), &|_| running);
                let orphan = findings
                    .iter()
                    .find(|f| f.code == "orphan_runtime_id")
                    .unwrap();
                assert_eq!(orphan.fix.is_some(), !running);
            }
        }
        let mut r = claude_record(Path::new("absent-health-fixture"));
        r.claude_session = None;
        assert!(session_findings(&store(vec![r]), &|_| true)
            .iter()
            .any(|f| f.code == "orphan_runtime_id" && f.fix.is_none()));
    }
    #[test]
    fn dead_claim_duplicate_orphan_and_claude_have_safe_fixes() {
        let mut claude = record("cutex.claude", Some(43210));
        claude.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        let mut s = store(vec![record("cutex.native", Some(43211)), claude]);
        let findings = session_findings(&s, &|_| false);
        for code in [
            "stale_runtime_claim",
            "duplicate_runtime_agent_id",
            "orphan_runtime_id",
            "claude_dead_adapter",
        ] {
            assert!(
                findings.iter().any(|f| f.code == code && f.fix.is_some()),
                "{code}"
            );
        }
        for r in s.sessions.values_mut() {
            crate::session::runtime_reconciliation::clear_runtime_fields(r, false);
        }
        assert!(session_findings(&s, &|_| false).is_empty());
    }
    #[test]
    fn live_unknown_zero_and_launching_pids_never_have_a_cleanup() {
        for pid in [None, Some(0), Some(std::process::id())] {
            let s = store(vec![record("cutex.a", pid), record("cutex.b", pid)]);
            assert!(session_findings(&s, &|_| true)
                .iter()
                .all(|f| f.fix.is_none()));
        }
        let mut r = record("cutex.launching", Some(43210));
        r.app_server_launch_claim_id = Some("launch-owner-not-verified".into());
        assert!(session_findings(&store(vec![r]), &|_| false)
            .iter()
            .all(|f| f.fix.is_none()));
    }
    #[test]
    fn missing_manifest_is_report_only_even_without_runtime_claim() {
        let mut r = record("cutex.package", None);
        r.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: "native".into(),
            native_home: std::env::temp_dir(),
            bundle_manifest: std::env::temp_dir().join(format!("absent-{}", uuid::Uuid::new_v4())),
            bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        let findings = session_findings(&store(vec![r]), &|_| false);
        assert!(findings
            .iter()
            .any(|f| f.code == "missing_runtime_package" && f.fix.is_none()));
    }
    #[test]
    fn recheck_refuses_changed_pid_and_duplicate_fix_is_noop() {
        let dir = std::env::temp_dir().join(format!("cutex-health-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cutex-sessions.json");
        crate::session::store::with_locked_session_rows(&path, |rows| {
            rows.insert_session("cutex.health", record("cutex.health", Some(43210)));
            rows.commit()?;
            Ok(())
        })
        .unwrap();
        let s = crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
        let observed = session_findings(&s, &|_| false)
            .into_iter()
            .find(|f| f.code == "stale_runtime_claim")
            .unwrap();
        assert!(!clear_observed(&path, &observed, &|_| true).unwrap());
        assert_eq!(
            crate::session::store::load_cutex_session_store_from_path(&path)
                .unwrap()
                .sessions["cutex.health"]
                .runtime_pid,
            Some(43210)
        );
        assert!(clear_observed(&path, &observed, &|_| false).unwrap());
        assert!(!clear_observed(&path, &observed, &|_| false).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_failed_fix_reports_the_fixes_already_applied() {
        let s = store(vec![record("cutex.first", Some(43210)), record("cutex.second", Some(43211)), record("cutex.third", Some(43212))]);
        let findings: Vec<Finding> = session_findings(&s, &|_| false)
            .into_iter()
            .filter(|f| f.code == "stale_runtime_claim")
            .collect();
        assert_eq!(findings.len(), 3);
        let selected: BTreeSet<String> = findings.iter().map(|f| f.id.clone()).collect();
        let mut calls = 0;
        let failure = apply_findings(&findings, &selected, false, |finding| {
            calls += 1;
            if calls == 2 {
                anyhow::bail!("finding_changed: runtime evidence changed; scan again")
            }
            Ok(finding.cutex_session_id.clone().unwrap())
        })
        .err()
        .unwrap();
        assert_eq!(calls, 2, "stops at the first failure");
        assert_eq!(failure.applied, vec![findings[0].id.clone()]);
        assert_eq!(failure.failed.as_deref(), Some(findings[1].id.as_str()));
        assert_eq!(failure.code(), "finding_changed");
        let details = failure.details();
        assert_eq!(details["applied"], json!([findings[0].id]));
        assert_eq!(details["failedFindingId"], json!(findings[1].id));
        assert!(failure.to_string().contains("already applied"));
        // A failure before any fix lists none.
        let failure = apply_findings(&findings, &selected, false, |_| anyhow::bail!("disk full")).err().unwrap();
        assert!(failure.applied.is_empty());
        assert_eq!(failure.code(), "health_fix_failed");
        // A dry run applies nothing.
        let outcome = apply_findings(&findings, &selected, true, |_| unreachable!()).ok().unwrap();
        assert_eq!((outcome.applied.len(), outcome.planned.len()), (0, 3));
    }
    #[test]
    fn request_defaults_to_dry_run_and_rejects_unknown_fields() {
        assert!(
            serde_json::from_value::<FixRequest>(json!({}))
                .unwrap()
                .dry_run
        );
        assert!(serde_json::from_value::<FixRequest>(json!({"force":true})).is_err());
        assert!(serde_json::from_value::<FixRequest>(json!({"dryRun":"false"})).is_err());
    }
    #[test]
    fn stuck_messages_and_lost_alarms_are_report_only_and_do_not_replay() {
        let alarms = json!([
            {"alarm_id":"lost","message_id":"alarm-lost","state":"queued","to":"cutex.a"},
            {"alarm_id":"saved","message_id":"alarm-saved","state":"queued","to":"cutex.a"},
            {"alarm_id":"later","message_id":"alarm-later","state":"scheduled","to":"cutex.a"}
        ]);
        let findings = bus_findings(
            &json!({"total":3,"liveQueue":{"available":false}}),
            &alarms,
            &|id| Ok(id == "alarm-saved"),
        )
        .unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].code, "stuck_bus_messages");
        assert_eq!(findings[0].detail["count"], 3);
        assert_eq!(findings[1].code, "lost_alarms");
        assert_eq!(findings[1].detail["alarmId"], "lost");
        assert!(findings.iter().all(|f| f.fix.is_none()));
    }
    #[cfg(unix)]
    #[test]
    fn writable_profile_directories_are_report_only() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::config::paths::test_home_env_lock();
        let home = crate::config::paths::home_dir().unwrap();
        let root = home.join(format!("health-profiles-{}", uuid::Uuid::new_v4()));
        assert!(profile_directory_findings(&root).unwrap().is_empty());
        assert!(!root.exists(), "scan never creates directories");
        for (name, mode) in [
            ("", 0o775),
            ("group", 0o770),
            ("other", 0o702),
            ("safe", 0o700),
            ("readable", 0o755),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let nested = root.join("safe/nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::write(root.join("not-a-directory"), b"fixture").unwrap();
        std::os::unix::fs::symlink(&nested, root.join("linked-directory")).unwrap();
        let findings = profile_directory_findings(&root).unwrap();
        assert_eq!(findings.len(), 3);
        assert!(findings.iter().all(|f| f.code == "profile_directory_writable"
            && f.severity == "warn"
            && f.fix.is_none()));
        assert_eq!(findings[0].detail["path"], json!(root));
        assert_eq!(findings[0].detail["mode"], "0775");
        assert_eq!(
            findings[0].detail["repairCommand"],
            json!(["chmod", "0700", "--", root])
        );
        let selected = findings.iter().map(|f| f.id.clone()).collect();
        let outcome =
            apply_findings(&findings, &selected, false, |_| unreachable!()).unwrap();
        assert!(outcome.applied.is_empty());
        assert_eq!(outcome.skipped.len(), 3);
        for finding in &findings {
            let path = Path::new(finding.detail["path"].as_str().unwrap());
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(finding.detail["mode"], format!("{mode:04o}"));
        }
        assert_eq!(findings, profile_directory_findings(&root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn health_config_optout_roundtrips() {
        let cfg: crate::profiles::model::CodezConfig =
            serde_json::from_value(json!({"health":{"scan_on_start":false}})).unwrap();
        assert!(!cfg.health.scan_on_start);
        assert_eq!(
            serde_json::to_value(cfg).unwrap()["health"]["scan_on_start"],
            false
        );
        assert!(
            crate::profiles::model::CodezConfig::default()
                .health
                .scan_on_start
        );
    }
}
