//! Claude's adapter and native session are separate identities from Codex rollouts.
//! This controller never reads login credentials or submits a model prompt.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_server::client::{AppServerClient, AppServerClientOptions, AppServerEndpoint};
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::profiles::appearance::AgentTurnQuestions;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};
use crate::session::store::{
    cutex_sessions_path, with_locked_session_rows,
};

pub mod fork_history;
pub mod permission_rules;
pub mod code_sources;
pub mod skill_catalog;
pub mod side;
pub mod structure;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDeployment {
    pub python: PathBuf,
    pub adapter_source: PathBuf,
    pub claude_cli: PathBuf,
    pub claude_home: PathBuf,
    pub tui_binary: PathBuf,
}

impl ClaudeDeployment {
    pub fn validate(&self) -> anyhow::Result<()> {
        for path in [&self.python, &self.claude_cli, &self.tui_binary] {
            ensure!(
                path.is_absolute() && path.is_file(),
                "executable missing: {}",
                path.display()
            );
        }
        ensure!(
            self.adapter_source.is_absolute()
                && self
                    .adapter_source
                    .join("src/cute_claude/__main__.py")
                    .is_file()
                && self.adapter_source.join("scripts/connect_tui.py").is_file(),
            "Claude adapter source missing"
        );
        ensure!(
            self.claude_home.is_absolute() && self.claude_home.is_dir(),
            "Claude login home missing"
        );
        // The login (credentials, sessions) belongs to the instance; binaries
        // and the adapter source may be shared read-only.
        crate::config::instance::require_inside_home(&self.claude_home, "Claude claude_home")?;
        Ok(())
    }

    pub fn install(&self) -> anyhow::Result<()> {
        self.validate()?;
        write_private_pretty_json_atomic(&deployment_path()?, self, "Claude deployment")
    }

    pub fn selected() -> anyhow::Result<Self> {
        let value: Self = serde_json::from_slice(
            &std::fs::read(deployment_path()?)
                .context("Claude runtime not installed; use cutex human claude install")?,
        )?;
        value.validate()?;
        Ok(value)
    }
}

pub(crate) fn deployment_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("claude/deployment.json"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSession {
    pub deployment: ClaudeDeployment,
    pub thread_id: String,
    pub state_dir: PathBuf,
    pub model: String,
    #[serde(default)]
    pub effort: Option<crate::profiles::appearance::ClaudeEffort>,
    #[serde(default)]
    pub instructions: String,
    /// File-editing tools. Unless [`Self::write_explicit`], this is only the
    /// value at creation / last start: launches follow the profile's current
    /// rule ([`effective_tools`]).
    #[serde(default)]
    pub allow_write: bool,
    /// Bash; follows the profile unless [`Self::exec_explicit`].
    #[serde(default)]
    pub allow_exec: bool,
    /// The write rule was set for this session (CLI flag, wizard, session
    /// settings) and no longer follows the profile. Records written before
    /// r225 have neither flag: they all follow the profile.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub write_explicit: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exec_explicit: bool,
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub connection_id: Option<String>,
    /// The Claude Code permission mode the owner created the agent in
    /// (`launch.claudePermissionMode`, capability
    /// `claude-launch-permission-mode`). Sent once as
    /// `thread/settings/update` at the first successful start, then cleared:
    /// afterwards the adapter keeps the thread's mode itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_permission_mode: Option<String>,
}

/// Exact-occurrence proof for Archive. The controller lock remains held from
/// preparation through offline verification, so a concurrent Start/Close cannot
/// replace the occurrence between those phases.
pub struct ClaudeArchiveRuntime {
    controller_lock: File,
    adapter_pid: Option<u32>,
    adapter_started_at: Option<DateTime<Utc>>,
}

impl ClaudeArchiveRuntime {
    pub fn prepare(record: &CutexSessionRecord) -> anyhow::Result<Self> {
        ensure!(
            record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription,
            "session is not a Claude subscription runtime"
        );
        let session = record
            .claude_session
            .as_ref()
            .context("Claude session missing")?;
        ensure!(
            session.native_session_id.is_some(),
            "Claude native session identity missing"
        );
        let controller_lock = lock(session)?;
        let Some(pid) = record.runtime_pid else {
            // A cleared durable claim with a reachable server is not an offline
            // proof. Archive must fail closed rather than shutdown an occurrence
            // that was not part of the reviewed record.
            ensure!(
                connect(session).is_err(),
                "Claude runtime claim is cleared but its adapter is still reachable"
            );
            return Ok(Self {
                controller_lock,
                adapter_pid: None,
                adapter_started_at: None,
            });
        };
        let adapter_started_at = if crate::platform::process::process_is_running(pid) {
            Some(crate::platform::process::process_started_at(pid).context(
                "Claude adapter process identity unavailable; Archive made no change",
            )?)
        } else {
            None
        };
        let (client, ready) = match connect(session) {
            Ok(connected) => connected,
            Err(error) => {
                let occurrence_exited = match adapter_started_at {
                    None => true,
                    Some(started_at) => {
                        Self::adapter_occurrence_exited(pid, started_at)?
                    }
                };
                if occurrence_exited {
                    return Ok(Self {
                        controller_lock,
                        adapter_pid: Some(pid),
                        adapter_started_at,
                    });
                }
                return Err(error).context("Claude adapter is alive but unreachable");
            }
        };
        let status = Self::exact_status(
            client.handle().request(
                "claude/runtime/status",
                json!({"threadId":session.thread_id}),
            )?,
            record,
            session,
            ready.pid,
        )?;
        ensure!(
            matches!(
                status["state"].as_str(),
                Some("waiting" | "working" | "interrupting" | "offline" | "closed")
            ),
            "Claude runtime is not in an archivable state: {}",
            status["state"]
        );
        Ok(Self {
            controller_lock,
            adapter_pid: Some(pid),
            adapter_started_at,
        })
    }

    pub fn stop_and_verify(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()> {
        let session = Self::reviewed_session(record)?;
        let Some(pid) = self.adapter_pid else {
            return self.verify_offline(record);
        };
        let Some(started_at) = self
            .adapter_started_at
        else {
            return self.verify_offline(record);
        };
        ensure!(
            crate::platform::process::process_started_at(pid)? == started_at,
            "Claude adapter process occurrence changed before stop"
        );
        let (client, ready) = match connect(session) {
            Ok(connected) => connected,
            Err(error) => {
                if Self::adapter_occurrence_exited(pid, started_at)? {
                    return self.verify_offline(record);
                }
                return Err(error).context("Claude adapter is alive but unreachable");
            }
        };
        let _status = Self::exact_status(
            client.handle().request(
                "claude/runtime/status",
                json!({"threadId":session.thread_id}),
            )?,
            record,
            session,
            ready.pid,
        )?;
        let closed = client.handle().request(
            "claude/runtime/close",
            json!({"threadId":session.thread_id}),
        )?;
        validate_status(session, &closed)?;
        ensure!(
            closed["processExitConfirmed"] == true
                || matches!(closed["state"].as_str(), Some("offline" | "closed")),
            "Claude native child exit is unconfirmed"
        );
        client.handle().request(
            "claude/server/shutdown",
            json!({"runtimeId":ready.runtime_id}),
        )?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::platform::process::process_is_running(pid) {
            if Instant::now() >= deadline {
                anyhow::bail!("Claude adapter PID {pid} did not exit after shutdown");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        ensure!(connect(session).is_err(), "Claude adapter endpoint remained reachable");
        let _ = &self.controller_lock;
        Ok(())
    }

    pub fn verify_offline(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()> {
        let session = Self::reviewed_session(record)?;
        ensure!(
            connect(session).is_err(),
            "Claude adapter endpoint remained reachable"
        );
        if let Some(pid) = self.adapter_pid {
            if let Some(started_at) = self.adapter_started_at {
                if crate::platform::process::process_is_running(pid) {
                    let observed = crate::platform::process::process_started_at(pid)?;
                    ensure!(
                        observed != started_at,
                        "Claude adapter PID is still running"
                    );
                }
            }
        }
        let _ = &self.controller_lock;
        Ok(())
    }

    fn reviewed_session(
        record: &CutexSessionRecord,
    ) -> anyhow::Result<&crate::runtime::claude::ClaudeSession> {
        ensure!(
            record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription
                && record.claude_session.is_some(),
            "reviewed Claude session changed"
        );
        Ok(record.claude_session.as_ref().unwrap())
    }

    fn adapter_occurrence_exited(
        pid: u32,
        started_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        if !crate::platform::process::process_is_running(pid) {
            return Ok(true);
        }
        Ok(crate::platform::process::process_started_at(pid)? != started_at)
    }

    fn exact_status(
        status: Value,
        record: &CutexSessionRecord,
        session: &ClaudeSession,
        ready_pid: u32,
    ) -> anyhow::Result<Value> {
        validate_status(session, &status)?;
        let native = session
            .native_session_id
            .as_deref()
            .context("Claude native session identity missing")?;
        ensure!(
            status["sessionId"].as_str() == Some(native),
            "Claude status native identity mismatch"
        );
        ensure!(
            status["connectionId"].as_str() == session.connection_id.as_deref(),
            "Claude status connection occurrence mismatch"
        );
        ensure!(
            record.runtime_pid == Some(ready_pid),
            "Claude durable adapter PID mismatch"
        );
        Ok(status)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Ready {
    endpoint: String,
    runtime_id: String,
    pid: u32,
}

/// The adapter's private SQLite writer lease arbitrates server ownership.
/// This shorter lock serializes local controller operations, not model turns.
pub(crate) fn lock(session: &ClaudeSession) -> anyhow::Result<File> {
    std::fs::create_dir_all(&session.state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&session.state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(session.state_dir.join("controller.lock"))?;
    file.try_lock_exclusive()
        .context("Claude lifecycle operation is already running; retry when it finishes")?;
    Ok(file)
}

/// The current write / exec rules of a Claude profile (unset = on); a
/// session without profile gets the default.
pub fn profile_tools(profile: Option<&str>) -> anyhow::Result<(bool, bool)> {
    let default = crate::profiles::appearance::CLAUDE_TOOLS_DEFAULT;
    let Some(name) = profile else { return Ok((default, default)) };
    let path = crate::config::paths::accounts_path()?;
    if !path.exists() {
        return Ok((default, default));
    }
    let accounts: crate::profiles::model::AccountsStore = serde_json::from_slice(&std::fs::read(path)?)?;
    Ok(match crate::profiles::lookup::find_account(&accounts, name)? {
        Some(account) => (account.options.claude.write(), account.options.claude.exec()),
        None => (default, default),
    })
}

/// Tools a start of this session launches with: a rule set for the session
/// wins, otherwise the profile's current rule (a session without profile
/// keeps its stored values: nothing to follow).
pub fn effective_tools(record: &CutexSessionRecord, session: &ClaudeSession) -> anyhow::Result<(bool, bool)> {
    let profile = match record.profile.as_deref() {
        Some(name) if !(session.write_explicit && session.exec_explicit) => profile_tools(Some(name))?,
        _ => (session.allow_write, session.allow_exec),
    };
    Ok((
        if session.write_explicit { session.allow_write } else { profile.0 },
        if session.exec_explicit { session.allow_exec } else { profile.1 },
    ))
}

/// Adapter tool flags of a start ([`effective_tools`]).
pub fn tool_args(record: &CutexSessionRecord, session: &ClaudeSession) -> anyhow::Result<Vec<&'static str>> {
    let (write, exec) = effective_tools(record, session)?;
    Ok([(write, "--allow-write"), (exec, "--allow-exec")]
        .into_iter()
        .filter_map(|(on, flag)| on.then_some(flag))
        .collect())
}

pub fn create(
    name: &str,
    cwd: &Path,
    model: &str,
    allow_write: bool,
    allow_exec: bool,
) -> anyhow::Result<CutexSessionRecord> {
    create_profile(name,cwd,model,Some(allow_write),Some(allow_exec),None,true,None)
}

/// `allow_write` / `allow_exec`: Some = set for this session, None = follow the
/// profile's rule (resolved again at every start, see [`effective_tools`]).
pub fn create_profile(name:&str,cwd:&Path,model:&str,allow_write:Option<bool>,allow_exec:Option<bool>,profile:Option<String>,managed:bool,effort:Option<crate::profiles::appearance::ClaudeEffort>)->anyhow::Result<CutexSessionRecord> {
    create_profile_with_creator(name,cwd,model,allow_write,allow_exec,profile,managed,effort,None)
}

/// The actor is supplied only by an authorized new-session path; imports and
/// legacy callers keep None. Stamped before the first session row is persisted.
#[allow(clippy::too_many_arguments)]
pub fn create_profile_with_creator(name:&str,cwd:&Path,model:&str,allow_write:Option<bool>,allow_exec:Option<bool>,profile:Option<String>,managed:bool,effort:Option<crate::profiles::appearance::ClaudeEffort>,created_by:Option<crate::session::model::SessionCreatedBy>)->anyhow::Result<CutexSessionRecord> {
    ensure!(
        !name.trim().is_empty()
            && name.trim() == name
            && name.chars().count() <= 128
            && !name.chars().any(char::is_control),
        "valid agent name required"
    );
    ensure!(
        !model.trim().is_empty() && model.len() <= 256 && !model.chars().any(char::is_control),
        "model required"
    );
    let mut deployment = ClaudeDeployment::selected()?;
    // The profile's Claude account decides the login home the session
    // freezes (default = the deployment's shared home, as before accounts).
    let (account, home) = super::claude_accounts::home_for_profile(profile.as_deref(), &deployment.claude_home)?;
    if account != super::claude_accounts::DEFAULT_ACCOUNT_ID {
        deployment.claude_home = home;
        if super::claude_accounts::api_launch_config(&deployment.claude_home)?.is_some() {
            ensure!(
                adapter_features(&deployment.adapter_source)["apiAccounts"] == true,
                "Claude API account {account} needs an adapter that declares features.apiAccounts (claude-r7 or newer); install it with cutex human claude install"
            );
        }
    }
    let cwd = cwd.canonicalize()?;
    ensure!(cwd.is_dir(), "working directory required");
    // The record keeps the ordinary spelling, not the Win32 verbatim form
    // canonicalize() returns on Windows (`\\?\F:\x`): Projects, specs and
    // the owner name the directory without it (r244 `mismatch: cwd`).
    let cwd = std::path::PathBuf::from(crate::agent_management::strip_verbatim_prefix(
        cwd.to_str().context("working directory must be valid Unicode")?,
    ));
    let thread_id = uuid::Uuid::new_v4().to_string();
    let id = format!("cutex.{}", uuid::Uuid::new_v4());
    let mut record = CutexSessionRecord::new(
        id.clone(),
        None,
        crate::platform::host::current_host_name(),
        cwd.to_string_lossy().into_owned(),
        None,
    )?;
    record.stamp_created_by_at_creation(created_by.as_ref())?;
    record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
    record.formal_agent_name = managed.then(||name.into());
    record.thread_name = Some(name.into());
    record.profile = profile;
    record.agent_enabled = managed;
    record.agent_groups = crate::agent_bus::groups::normalize_registered_agent_groups(Vec::new(), None, &record.cwd);
    record.exposed_to_backend = true;
    record.registration_class = if managed {crate::agent_bus::model::AgentRegistrationClass::Persistent} else {crate::agent_bus::model::AgentRegistrationClass::LocalOnly};
    record.model_defaults = Some(model.into());
    record.reasoning_defaults = effort.map(|v|v.as_str().into());
    let profile_tools = profile_tools(record.profile.as_deref())?;
    record.claude_session = Some(ClaudeSession {
        deployment,
        thread_id,
        state_dir: crate::config::paths::runtime_dir()?
            .join("claude/sessions")
            .join(&id),
        model: model.into(),
        effort,
        instructions: String::new(),
        allow_write: allow_write.unwrap_or(profile_tools.0),
        allow_exec: allow_exec.unwrap_or(profile_tools.1),
        write_explicit: allow_write.is_some(),
        exec_explicit: allow_exec.is_some(),
        native_session_id: None,
        connection_id: None,
        initial_permission_mode: None,
    });
    let path = cutex_sessions_path()?;
    with_locked_session_rows(&path, |rows| {
        ensure!(
            !rows
                .sessions()
                .map(|(_, record)| record)
                .any(|r| !r.is_retired() && r.formal_agent_name.as_deref() == Some(name)),
            "agent name already exists"
        );
        rows.insert_session(id, record.clone());
        rows.commit().map(drop)
    })?;
    crate::notify::session::session(
        &record.claude_session.as_ref().unwrap().thread_id,
        crate::notify::session::Change::Set(
            crate::config::store::load_codez_config_checked()?.new_session_notification,
        ),
    )
    .with_context(|| {
        format!(
            "Claude agent {} was saved; notification preference initialization failed",
            record.cutex_session_id
        )
    })?;
    Ok(record)
}

pub(super) fn record(id: &str) -> anyhow::Result<CutexSessionRecord> {
    // One record out of the cached store, not a copy of the whole store.
    let record = crate::session::store::with_cutex_session_store(|store| {
        store.sessions.get(id).cloned()
    })?
    .context("session not found")?;
    ensure!(
        record.runtime_backend == CutexSessionRuntimeBackend::ClaudeSubscription
            && record.claude_session.is_some(),
        "session is not a Claude subscription runtime"
    );
    ensure!(
        crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name()
        ),
        "use SSH to manage this remote Claude runtime"
    );
    Ok(record)
}

pub(super) fn connect(session: &ClaudeSession) -> anyhow::Result<(AppServerClient, Ready)> {
    connect_with(session, false)
}

/// `interactive`: the connection relays the owner's decisions
/// (`claudeInteractiveApprovals`); it subscribes to no thread, so it never
/// receives approval requests itself.
fn connect_with(session: &ClaudeSession, interactive: bool) -> anyhow::Result<(AppServerClient, Ready)> {
    let ready: Ready =
        serde_json::from_slice(&std::fs::read(session.state_dir.join("server.json"))?)?;
    let token = std::fs::read_to_string(session.state_dir.join("rpc-token"))?;
    let mut options = AppServerClientOptions::new(AppServerEndpoint::LoopbackWebSocket {
        url: ready.endpoint.clone(),
        bearer_token: Some(token.trim().into()),
    });
    if interactive {
        options.extra_capabilities.insert("claudeInteractiveApprovals".to_string(), Value::Bool(true));
    }
    options.connect_timeout = Duration::from_millis(300);
    options.request_timeout = Duration::from_secs(60);
    let client = AppServerClient::connect(options)?;
    let capabilities = client.handle().request("claude/capabilities", json!({}))?;
    ensure!(
        capabilities["implementation"] == "cute-claude-app-server"
            && capabilities["runtimeId"] == ready.runtime_id
            && capabilities["backend"] == "claude-subscription-sdk"
            && capabilities["persistentRuntime"] == true
            && capabilities["ensureThread"] == "claude/thread/ensure",
        "Claude adapter identity/capabilities mismatch"
    );
    Ok((client, ready))
}

/// Why a safety-check decision could not be handed to the Claude runtime.
#[derive(Debug)]
pub enum SafetyDecisionError {
    /// No adapter runs for this agent.
    Offline,
    /// The agent's adapter predates safety grants (feature `safetyGrants`).
    Unsupported,
    /// The adapter refused (`data.code`, e.g. `safety_block_closed`).
    Refused { code: String, message: String, data: Value },
    Failed(anyhow::Error),
}

/// Forward the owner's decision on a safety block
/// (`thread/safetyCheck/grant|dismiss`, docs/safety-grants.md) to the
/// agent's running adapter. Management calls this only from its owner-only
/// route; the adapter accepts it only from an interactive connection.
pub fn safety_check_decision(id: &str, method: &str, params: Value) -> Result<Value, SafetyDecisionError> {
    let record = record(id).map_err(SafetyDecisionError::Failed)?;
    let session = record.claude_session.as_ref().ok_or(SafetyDecisionError::Unsupported)?;
    if adapter_features(&session.deployment.adapter_source)["safetyGrants"] != true {
        return Err(SafetyDecisionError::Unsupported);
    }
    if record.runtime_pid.is_none() {
        return Err(SafetyDecisionError::Offline);
    }
    let (client, _) = connect_with(session, true).map_err(|_| SafetyDecisionError::Offline)?;
    match client.handle().request(method, params) {
        Ok(result) => Ok(result),
        Err(crate::app_server::client::AppServerClientError::Rpc(error)) => {
            let data = error.data.unwrap_or(Value::Null);
            let code = data.get("code").and_then(Value::as_str).unwrap_or("runtime_refused").to_string();
            Err(SafetyDecisionError::Refused { code, message: error.message, data })
        }
        Err(error) => Err(SafetyDecisionError::Failed(anyhow::Error::new(error))),
    }
}

/// The adapter's recent safety blocks of this agent's thread
/// (`thread/safetyCheck/list`), newest first; empty when the adapter does
/// not run or predates `safetyGrants`. Observers use it to catch blocks
/// reported while nobody was connected.
pub fn safety_blocks(id: &str) -> anyhow::Result<Vec<Value>> {
    let record = record(id)?;
    let Some(session) = record.claude_session.as_ref() else {
        return Ok(Vec::new());
    };
    if record.runtime_pid.is_none() || adapter_features(&session.deployment.adapter_source)["safetyGrants"] != true {
        return Ok(Vec::new());
    }
    let (client, _) = connect(session)?;
    let listed = client.handle().request("thread/safetyCheck/list", json!({"threadId": session.thread_id}))?;
    Ok(listed["data"].as_array().cloned().unwrap_or_default())
}

/// Applies the profile's native runtime overrides and returns its
/// AskUserQuestion mode and file checkpointing policy (default on).
fn apply_profile_runtime(record: &CutexSessionRecord, command: &mut Command) -> anyhow::Result<(AgentTurnQuestions, bool)> {
    let default_checkpointing = crate::profiles::settings_api::DEFAULT_CLAUDE_FILE_CHECKPOINTING;
    let Some(name) = record.profile.as_deref() else { return Ok((AgentTurnQuestions::default(), default_checkpointing)); };
    let accounts: crate::profiles::model::AccountsStore = serde_json::from_slice(
        &std::fs::read(crate::config::paths::accounts_path()?)?)?;
    let account = crate::profiles::lookup::find_account(&accounts, name)?
        .context("Claude launch profile missing")?;
    let questions = account.options.agent_turn_questions.unwrap_or_default();
    let checkpointing = account.options.claude_file_checkpointing.unwrap_or(default_checkpointing);
    // claude-compaction: `claude-runtime.json` (profiles/claude_compaction.rs).
    if let Some(settings) = crate::profiles::claude_compaction::read_id(&account.id)? {
        command.envs(settings.envs());
    }
    // claude-compaction-models: the per-model table for the adapter (claude-r24+; older ones ignore it).
    if let Some(table) = crate::profiles::claude_compaction::read_models_id(&account.id)? {
        command.env(crate::profiles::claude_compaction::MODELS_ENV,
            crate::profiles::claude_compaction::models_json(&table).to_string());
    }
    Ok((questions, checkpointing))
}

/// The launch text cutex appends to the Claude Code preset system prompt
/// (`# Cutex`, AGENTS.md, `# File references`, `# Memory`), written to the
/// session state dir for adapters whose `features.json` declares
/// `systemAppend`. Older adapters keep their own system prompt (and get no
/// file: they would refuse the flag).
fn write_system_append(record: &CutexSessionRecord, session: &ClaudeSession, features: &Value) -> anyhow::Result<Option<PathBuf>> {
    if features["systemAppend"] != true {
        return Ok(None);
    }
    let text = crate::memory::harness::join_sections(&crate::memory::harness::claude_system_append_sections(record));
    let path = session.state_dir.join(SYSTEM_APPEND_FILE);
    crate::config::atomic::write_private_bytes_atomic(&path, text.as_bytes())
        .with_context(|| format!("write Claude system prompt append {}", path.display()))?;
    Ok(Some(path))
}

/// File name of the system prompt append in a Claude session state dir.
pub const SYSTEM_APPEND_FILE: &str = "system-append.md";

/// `--agent-turn-questions` for adapters whose `features.json` declares
/// `agentTurnQuestions`; an older adapter would refuse the flag (it keeps
/// waiting for an answer in every turn).
fn agent_turn_questions_args(features: &Value, mode: AgentTurnQuestions) -> Vec<&'static str> {
    if features["agentTurnQuestions"] == true {
        vec!["--agent-turn-questions", mode.as_str()]
    } else {
        Vec::new()
    }
}

fn file_checkpointing_args(features: &Value, enabled: bool) -> Vec<&'static str> {
    if features["fileCheckpointing"] == true {
        vec!["--file-checkpointing", if enabled { "enabled" } else { "disabled" }]
    } else {
        Vec::new()
    }
}

/// `cutex-mcp-tools.json`: the façade and the agent's tools. An adapter
/// declaring `features.approvalWait` (claude-r16+) also gets
/// `approvalWaitSecs`: it gives the façade that hold (approval-wait) and
/// lets the cutex tools that may hold run past its usual tool timeout.
fn mcp_tools_descriptor(executable: &std::path::Path, tools: Value, features: &Value) -> Value {
    let mut descriptor = json!({"executable": executable, "tools": tools});
    if features["approvalWait"] == true {
        descriptor["approvalWaitSecs"] = json!(crate::approvals::APPROVAL_WAIT_SECS);
    }
    descriptor
}

fn ensure_server(record: &CutexSessionRecord) -> anyhow::Result<(AppServerClient, Ready)> {
    let session = record
        .claude_session
        .as_ref()
        .context("Claude session missing")?;
    if let Ok(connected) = connect(session) {
        return Ok(connected);
    }
    ensure!(!record.runtime_pid.is_some_and(crate::platform::process::process_is_running),
        "existing Claude adapter is alive but unreachable; no duplicate server was started");
    session.deployment.validate()?;
    let mcp = std::env::current_exe()?.with_file_name(if cfg!(windows) { "cutex-mcp.exe" } else { "cutex-mcp" });
    ensure!(mcp.is_file(), "Cutex MCP executable missing: {}", mcp.display());
    write_private_pretty_json_atomic(&session.state_dir.join("cutex-mcp-tools.json"),
        // Role-filtered at launch, like a stock runtime's tools/list.
        &mcp_tools_descriptor(&mcp, if record.agent_enabled || record.is_owned_session() {crate::agent_bus::tool_policy::tools_list_for_session(&record.cutex_session_id)["tools"].clone()}else{json!([])},
            &adapter_features(&session.deployment.adapter_source)), "Claude MCP tools")?;
    let log_path = session.state_dir.join("adapter.stderr.log");
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
        let previous = session.state_dir.join("adapter.stderr.log.1");
        if previous.exists() { std::fs::remove_file(&previous)?; }
        std::fs::rename(&log_path, previous)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(session.state_dir.join("adapter.stderr.log"))?;
    let mut command = Command::new(&session.deployment.python);
    // A different profile also resets an interactive model override when the
    // two profiles happen to have the same default model. A normal restart
    // preserves that override. This launch hint never changes a live owner.
    let profile_launch_path = session.state_dir.join("profile-launch.json");
    let profile_id = record.profile.as_deref().map(|name| -> anyhow::Result<String> {
        let store = crate::profiles::settings_api::load_store()?;
        Ok(crate::profiles::settings_api::find(&store, name).context("Claude launch profile missing")?.id.clone())
    }).transpose()?;
    let profile_launch = json!({"profileId": profile_id});
    let previous_profile = std::fs::read(&profile_launch_path).ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
    // No sidecar yet (the first start after this release, or a fresh
    // session) is not a profile change: never reset a hand-picked model then.
    command.env("CUTEX_CLAUDE_RESET_LAUNCH_MODEL", if record.profile.is_some()
        && previous_profile.as_ref().is_some_and(|previous| previous != &profile_launch) { "1" } else { "0" });
    let (agent_turn_questions, checkpointing) = apply_profile_runtime(record, &mut command)?;
    // proxy-modes: the Profile's effective proxy (loopback always in NO_PROXY).
    command.envs(crate::config::proxy_outbound::claude_runtime_proxy_envs(record.profile.as_deref())?);
    command
        .args([
            "-u",
            "-m",
            "cute_claude",
            "--backend",
            "subscription",
            "--listen",
            "ws://127.0.0.1:0",
        ])
        .arg("--model")
        .arg(&session.model)
        .arg("--workspace")
        .arg(&record.cwd)
        .arg("--state-dir")
        .arg(&session.state_dir)
        .arg("--cutex-mcp-config")
        .arg(session.state_dir.join("cutex-mcp-tools.json"))
        .arg("--ready-file")
        .arg(session.state_dir.join("server.json"))
        .arg("--claude-cli")
        .arg(&session.deployment.claude_cli)
        .arg("--claude-config-dir")
        .arg(&session.deployment.claude_home)
        .env("PYTHONPATH", session.deployment.adapter_source.join("src"))
        .current_dir(&record.cwd);
    #[cfg(not(windows))]
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    if let Some(effort) = session.effort {
        command.arg("--effort").arg(effort.as_str());
    }
    command.args(tool_args(record, session)?);
    let features = native_schema(session)["capabilities"]["features"].clone();
    if features["externalInput"] == true {
        command.arg("--external-owner-id").arg(&record.cutex_session_id)
            .arg("--external-runtime-generation")
            .arg(record.runtime_generation.checked_add(1).context("generation overflow")?.to_string());
    }
    if features["managedSkills"] == true {
        for path in structure::managed_skill_roots(record)? {
            command.arg("--managed-skills-root").arg(path);
        }
    }
    // API account: the adapter reads the key file named here and gives the
    // key only to this session's Claude Code process.
    let api_config = session.state_dir.join(super::claude_accounts::API_ACCOUNT_CONFIG_FILE);
    match super::claude_accounts::api_launch_config(&session.deployment.claude_home)? {
        Some(mut config) => {
            ensure!(
                features["apiAccounts"] == true,
                "this session's Claude account is an API account; its adapter does not declare features.apiAccounts (needs claude-r7 or newer: cutex human claude repin)"
            );
            // r265 key rotation: this runtime keeps the key it started with.
            // The adapter's key helper re-reads `keyFile`, so point it at a
            // per-launch private copy instead of the account's mutable file.
            super::claude_accounts::bind_launch_key(&mut config, &session.state_dir)?;
            write_private_pretty_json_atomic(&api_config, &config, "Claude API account")?;
            command.arg("--api-account-config").arg(&api_config);
        }
        None => {
            if api_config.exists() {
                std::fs::remove_file(&api_config)?;
            }
            let bound = session.state_dir.join(super::claude_accounts::BOUND_API_KEY_FILE);
            if bound.exists() {
                std::fs::remove_file(&bound)?;
            }
        }
    }
    // Persistent "always allow" rules of this workspace (claude-permission-rules).
    permission_rules::prepare_launch(record, session)?;
    command.args(agent_turn_questions_args(&features, agent_turn_questions));
    command.args(file_checkpointing_args(&features, checkpointing));
    // claude-code-sources: Claude Code settings sources, claude.ai connectors
    // and the host's MCP layers (secret values only in this env). A broken MCP
    // store must not block the agent: warn and launch without these flags.
    let code_plan = match code_sources::prepare_launch(record, &session.state_dir, &features) {
        Ok((plan, args)) => {
            command.args(args);
            for (key, value) in &plan.envs {
                command.env(key, value);
            }
            Some(plan)
        }
        Err(error) => {
            eprintln!("claude: Claude Code sources / MCP layers skipped for {}: {error:#}", record.cutex_session_id);
            None
        }
    };
    if let Some(path) = write_system_append(record, session, &features)? {
        command.arg("--system-append-file").arg(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    // The adapter and its descendants (Claude Code, shells, dev servers) share
    // the Agent's kill-on-close Job object, whose last handle lives in the
    // adapter: when the adapter exits or is killed nothing is left behind,
    // and a management restart does not touch it.
    #[cfg(windows)]
    let mut child = {
        let job = crate::platform::windows_job::create_or_join_agent_job(
            &crate::runtime::process_scope::managed_agent_job_name(&record.cutex_session_id),
        )?;
        crate::platform::windows_spawn::spawn_detached_in_job(&command, &log, &job)?
    };
    #[cfg(not(windows))]
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let result = loop {
        if let Some(status) = child.try_wait()? {
            break Err(anyhow::anyhow!(
                "Claude adapter exited {status}; see {}",
                session.state_dir.join("adapter.stderr.log").display()
            ));
        }
        if let Ok(connected) = connect(session) {
            break Ok(connected);
        }
        if Instant::now() >= deadline {
            break Err(anyhow::anyhow!(
                "Claude adapter startup still unconfirmed; retry Start for {id}; see {log}",
                id = record.cutex_session_id,
                log = session.state_dir.join("adapter.stderr.log").display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // A CLI may exit while the adapter stays alive. In the long-lived management
    // process this waiter also reaps the child; it never kills it on TUI detach.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    if result.is_ok() {
        // A hint only: the adapter is already serving, so a failed write must
        // not turn a started runtime into a failed start.
        if let Err(error) = write_private_pretty_json_atomic(&profile_launch_path, &profile_launch, "Claude launch profile") {
            eprintln!("claude: launch profile hint not written for {}: {error:#}", record.cutex_session_id);
        }
        if let Some(plan) = &code_plan {
            code_sources::record_launch(&session.state_dir, plan);
        }
    }
    result
}

/// The Claude permission modes a new agent may start in with the selected
/// Claude runtime (`features.json` `claudePermissionModes`; empty = the
/// runtime does not offer them, or none is installed).
pub fn launch_permission_modes() -> Vec<String> {
    let Ok(deployment) = ClaudeDeployment::selected() else {
        return Vec::new();
    };
    permission_modes_of(&adapter_features(&deployment.adapter_source))
}

fn permission_modes_of(features: &Value) -> Vec<String> {
    let listed = features["claudePermissionModes"].as_array().cloned().unwrap_or_default();
    crate::config::new_thread::CLAUDE_PERMISSION_MODES
        .iter()
        .filter(|mode| listed.iter().any(|value| value.as_str() == Some(**mode)))
        .map(|mode| mode.to_string())
        .collect()
}

/// Records the permission mode a just-created Claude agent starts in
/// (`launch.claudePermissionMode`). Refused unless the selected Claude
/// runtime offers it.
pub fn set_initial_permission_mode(id: &str, mode: &str) -> anyhow::Result<()> {
    ensure!(
        launch_permission_modes().iter().any(|offered| offered == mode),
        "the installed Claude runtime does not offer the permission mode `{mode}`; update the claude-runtime component"
    );
    let path = cutex_sessions_path()?;
    crate::session::store::update_session_row(&path, id, |current| {
        let binding = current.claude_session.as_mut().context("Claude session missing")?;
        binding.initial_permission_mode = Some(mode.to_string());
        Ok(())
    })?
    .context("session removed after creation")?;
    Ok(())
}

/// `thread/settings/update` for a starting mode, in the Codex v2 shape the
/// adapter maps (components/claude-runtime protocol.py `requested_mode`):
/// plan turns plan mode on over the adapter's default mode.
fn initial_mode_patch(thread_id: &str, mode: &str) -> anyhow::Result<Value> {
    let workspace = json!({"type": "workspaceWrite", "writableRoots": [], "networkAccess": true,
        "excludeTmpdirEnvVar": false, "excludeSlashTmp": false});
    let mut patch = match mode {
        "bypassPermissions" => json!({"approvalPolicy": "never", "sandboxPolicy": {"type": "dangerFullAccess"}}),
        "auto" => json!({"approvalPolicy": "on-request", "approvalsReviewer": "auto_review", "sandboxPolicy": workspace}),
        "acceptEdits" => json!({"approvalPolicy": "on-request", "approvalsReviewer": "user", "sandboxPolicy": workspace}),
        "default" => json!({"approvalPolicy": "on-request", "approvalsReviewer": "user",
            "sandboxPolicy": {"type": "readOnly", "networkAccess": false}}),
        "plan" => json!({"collaborationMode": {"mode": "plan",
            "settings": {"model": "", "reasoning_effort": null, "developer_instructions": null}}}),
        other => anyhow::bail!("unknown Claude permission mode `{other}`"),
    };
    patch["threadId"] = json!(thread_id);
    Ok(patch)
}

fn validate_status(session: &ClaudeSession, status: &Value) -> anyhow::Result<()> {
    ensure!(
        status["threadId"] == session.thread_id,
        "Claude response belongs to another thread"
    );
    if let Some(expected) = &session.native_session_id {
        if let Some(actual) = status["sessionId"].as_str() {
            ensure!(actual == expected, "Claude native session identity changed");
        }
    }
    Ok(())
}

pub fn start(id: &str) -> anyhow::Result<Value> {
    start_expected(id, None)
}

pub fn start_expected(id: &str, expected_generation: Option<u64>) -> anyhow::Result<Value> {
    let lane = crate::management::v2::user_input::session_input_lock(id)?;
    let _operation = lane.reserve()?;
    let initial = record(id)?;
    let _lock = lock(initial.claude_session.as_ref().unwrap())?;
    let record = record(id)?;
    check_generation(&record, expected_generation)?;
    ensure!(
        !crate::agent_management::agent_execution_blocked(&record),
        "restore the archived agent before starting it"
    );
    // A re-pin requested while the session was online takes effect here,
    // when no adapter is running and this Start spawns a fresh one.
    let record = super::claude_pin::apply_pending_at_start(record)?;
    let session = record
        .claude_session
        .as_ref()
        .context("Claude session missing")?;
    let (client, ready) = ensure_server(&record)?;
    client.handle().request("claude/thread/ensure", json!({"threadId":session.thread_id,"cwd":record.cwd,"model":session.model,"instructions":session.instructions}))?;
    if let Some(mode) = &session.initial_permission_mode {
        // Before claude/runtime/start: the adapter stores the mode and the
        // native session connects in it.
        let patch = initial_mode_patch(&session.thread_id, mode)?;
        client.handle().request("thread/settings/update", patch).with_context(|| {
            format!("the Claude agent could not be put into its permission mode `{mode}`")
        })?;
    }
    if let Some(expected) = &session.native_session_id {
        let saved = client.handle().request(
            "claude/thread/inspect",
            json!({"threadId":session.thread_id}),
        )?;
        ensure!(saved["nativeSession"]["session_id"].as_str() == Some(expected), "Claude native history mapping is missing or changed; restore its saved adapter state before starting");
    }
    let status = client.handle().request(
        "claude/runtime/start",
        json!({"threadId":session.thread_id}),
    )?;
    validate_status(session, &status)?;
    ensure!(
        matches!(status["state"].as_str(), Some("waiting" | "working")),
        "Claude runtime is not ready: {}",
        status["state"]
    );
    let native = status["sessionId"]
        .as_str()
        .context("Claude native identity missing")?;
    let connection = status["connectionId"]
        .as_str()
        .context("Claude connection identity missing")?;
    let path = cutex_sessions_path()?;
    crate::session::store::update_session_row(&path, id, |current| {
        ensure!(
            current.claude_session.as_ref() == Some(session),
            "Claude configuration changed during start; reload this session"
        );
        let binding = current.claude_session.as_mut().unwrap();
        if binding.connection_id.as_deref() != Some(connection) {
            current.runtime_generation = current
                .runtime_generation
                .checked_add(1)
                .context("generation overflow")?;
        }
        binding.connection_id = Some(connection.into());
        binding.native_session_id = Some(native.into());
        // Applied above; from now on the adapter keeps the thread's mode.
        binding.initial_permission_mode = None;
        current.current_runtime_agent_id = Some(format!("claude.{connection}"));
        current.runtime_pid = Some(ready.pid);
        current.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(())
    })?
    .context("session removed during start")?;
    if adapter_features(&session.deployment.adapter_source)["externalInput"] == true {
        let current = self::record(id)?;
        client.handle().request("claude/externalInput/bind", json!({
            "ownerId": id, "runtimeGeneration": current.runtime_generation
        }))?;
    }
    refresh_mcp_binding(id)?;
    Ok(status)
}

pub fn refresh_mcp_binding(id: &str) -> anyhow::Result<()> {
    let record = record(id)?;
    let session = record.claude_session.as_ref().unwrap();
    let config = crate::config::store::load_codez_config_checked()?;
    // The binding is readable by the agent itself, so it carries only this
    // agent's credential (bound to its runtime id and durable session).
    let token = crate::agent_bus::agent_auth::launch_credential(
        &config,
        record.current_runtime_agent_id.as_deref().unwrap_or_default(),
        &record.cutex_session_id,
    )
    .context("Agent Bus token missing")?;
    let port = crate::agent_bus::service::agent_bus_port(&config);
    write_private_pretty_json_atomic(&session.state_dir.join("cutex-mcp-binding.json"),
        &json!({"threadId":session.thread_id,"runtimeAgentId":record.current_runtime_agent_id,
            "generation":record.runtime_generation,"url":format!("http://127.0.0.1:{port}/"),"token":token}), "Claude MCP binding")
}

pub fn status(id: &str) -> anyhow::Result<Value> {
    let record = record(id)?;
    let session = record.claude_session.as_ref().unwrap();
    if record.runtime_pid.is_none() {
        return Ok(json!({"threadId":session.thread_id,"state":"offline"}));
    }
    let (client, _) = connect(session)?;
    let status = client.handle().request(
        "claude/runtime/status",
        json!({"threadId":session.thread_id}),
    )?;
    validate_status(session, &status)?;
    Ok(status)
}

pub fn close(id: &str) -> anyhow::Result<Value> {
    close_expected(id, None)
}

pub fn close_expected(id: &str, expected_generation: Option<u64>) -> anyhow::Result<Value> {
    let lane = crate::management::v2::user_input::session_input_lock(id)?;
    let _operation = lane.reserve()?;
    let initial = record(id)?;
    let _lock = lock(initial.claude_session.as_ref().unwrap())?;
    let record = record(id)?;
    check_generation(&record, expected_generation.or(Some(initial.runtime_generation)))?;
    ensure!(record.claude_session == initial.claude_session,
        "Claude configuration changed before close; owner unchanged");
    crate::session::runtime_stop::RequestedStop::run(id, record.runtime_generation, || {
    let session = record.claude_session.as_ref().unwrap();
    if record.runtime_pid.is_none() {
        return Ok(json!({"threadId":session.thread_id,"state":"offline"}));
    }
    let (client, ready) = match connect(session) {
        Ok(connected) => connected,
        Err(error) => {
            // The recorded adapter died without a close (crash, kill, host
            // restart): nothing is left to stop, so the close settles its
            // stale claim like a confirmed close instead of failing (offline,
            // profile switch, archive and delete all stop through here).
            if clear_dead_adapter_claim(
                &cutex_sessions_path()?,
                id,
                &record,
                crate::management::v2::health::process_may_be_running,
            )? {
                return Ok(json!({
                    "threadId": session.thread_id,
                    "state": "offline",
                    "staleRuntimeCleared": true,
                }));
            }
            return Err(error);
        }
    };
    let status = client.handle().request(
        "claude/runtime/close",
        json!({"threadId":session.thread_id}),
    )?;
    validate_status(session, &status)?;
    ensure!(
        status["processExitConfirmed"] == true || status["state"] == "offline",
        "Claude child exit is unconfirmed; retained runtime binding"
    );
    let path = cutex_sessions_path()?;
    crate::session::store::update_session_row(&path, id, |current| {
        ensure!(
            current.claude_session.as_ref() == Some(session)
                && current.runtime_generation == record.runtime_generation
                && current.runtime_pid == record.runtime_pid,
            "Claude configuration changed during close; reload this session"
        );
        current.runtime_pid = None;
        current.current_runtime_agent_id = None;
        current.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(())
    })?
    .context("session removed during close")?;
    client.handle().request("claude/server/shutdown", json!({"runtimeId":ready.runtime_id}))?;
    Ok(status)
    }, |_| true)
}

/// Clear the runtime claim of a Claude session whose recorded adapter is
/// gone: its PID is not running (`running` fails closed) and the caller saw
/// its endpoint refuse. Exact occurrence only: the row must still carry the
/// observed PID, generation and Claude binding, so a Start that rebound the
/// session meanwhile keeps its claim. Returns whether the row was cleared.
/// The same fields the `claude_dead_adapter` health fix clears.
pub(crate) fn clear_dead_adapter_claim(
    path: &Path,
    id: &str,
    observed: &CutexSessionRecord,
    running: impl Fn(u32) -> bool,
) -> anyhow::Result<bool> {
    let Some(pid) = observed.runtime_pid else {
        return Ok(false);
    };
    if pid == 0 || running(pid) {
        return Ok(false);
    }
    with_locked_session_rows(path, |rows| {
        let Some(current) = rows.session(id) else {
            return Ok(false);
        };
        if current.runtime_pid != Some(pid)
            || current.runtime_generation != observed.runtime_generation
            || current.claude_session != observed.claude_session
            || current.claude_session.is_none()
        {
            return Ok(false);
        }
        crate::session::runtime_reconciliation::clear_runtime_fields(
            rows.session_mut(id).context("session disappeared")?,
            true,
        );
        rows.commit()?;
        Ok(true)
    })
}

/// Observer reconciliation (management's Claude observer, every 5 s): a
/// session whose recorded adapter PID is gone and whose endpoint refuses is
/// offline, so its stale claim is cleared and it stops being observed.
/// Skipped while a Start / Close holds the controller lock.
pub fn reconcile_dead_adapter(id: &str) -> anyhow::Result<bool> {
    let lane = crate::management::v2::user_input::session_input_lock(id)?;
    let _operation = match lane.try_lock() {
        Ok(reservation) => reservation,
        // Observer callbacks must not block a lifecycle operation that joins
        // them. The next observer poll retries reconciliation.
        Err(crate::management::v2::session_coordination::ReservationError::WouldBlock) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let initial = record(id)?;
    let Some(pid) = initial.runtime_pid else {
        return Ok(false);
    };
    if initial.is_retired() || crate::management::v2::health::process_may_be_running(pid) {
        return Ok(false);
    }
    let Ok(_lock) = lock(initial.claude_session.as_ref().unwrap()) else {
        return Ok(false);
    };
    let record = record(id)?;
    if connect(record.claude_session.as_ref().unwrap()).is_ok() {
        return Ok(false);
    }
    clear_dead_adapter_claim(
        &cutex_sessions_path()?,
        id,
        &record,
        crate::management::v2::health::process_may_be_running,
    )
}

fn check_generation(record: &CutexSessionRecord, expected: Option<u64>) -> anyhow::Result<()> {
    if let Some(expected) = expected {
        ensure!(
            expected == record.runtime_generation,
            "runtime generation changed: expected {expected}, current {}",
            record.runtime_generation
        );
    }
    Ok(())
}

/// Schema identity of the adapter's Codex app-server subset.
pub const ADAPTER_SCHEMA_VERSION: &str = "0.3.0+cutex";

/// `native.schema` of a Claude session resource: the adapter protocol plus
/// `capabilities.features`, the Codex v2 surfaces the deployed adapter serves
/// (`src/cute_claude/features.json`), so clients can hide the others.
/// `features` is null for an adapter that predates the file. Files are read
/// from the session's deployment and cached by modification time.
pub fn native_schema(session: &ClaudeSession) -> Value {
    let source = session.deployment.adapter_source.join("src/cute_claude");
    let mut features = adapter_features(&session.deployment.adapter_source);
    // Desktops before `claude-session-skills` list the Codex catalog in their
    // skill picker. Advertising it here would offer Claude paths outside this
    // session's managed catalog. Newer desktops read GET /v2/sessions/{id}/skills
    // and key on `managedSkills` instead. The adapter's own skills/list + skill
    // input remain available to its TUI.
    if features["skillInput"] == true {
        features["skillInput"] = json!(false);
    }
    let sha256 = cached_file_value(&source.join("protocol.py"), |raw| {
        use sha2::Digest;
        Some(Value::String(format!("{:x}", sha2::Sha256::digest(raw))))
    });
    json!({
        "protocol": "claude-app-server-subset",
        "majorVersion": 1,
        "version": ADAPTER_SCHEMA_VERSION,
        "sha256": sha256,
        "channel": "experimental",
        "capabilities": { "experimentalApi": true, "features": features },
        "extensions": ["claude-runtime-v1"],
    })
}

type SteeringCache = std::collections::HashMap<String, (u64, bool)>;

fn live_steering() -> &'static std::sync::Mutex<SteeringCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<SteeringCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Record what a session's live adapter generation reported in
/// `claude/capabilities` (`input.sameTurnSteering`), so the session
/// projection advertises `turnSteer` only when the running adapter really
/// steers. Called wherever cutex reads the live capabilities.
pub fn note_live_capabilities(session: &str, generation: u64, capabilities: &Value) {
    let steers = capabilities.pointer("/input/sameTurnSteering").and_then(Value::as_bool) == Some(true);
    if let Ok(mut cache) = live_steering().lock() {
        cache.insert(session.to_string(), (generation, steers));
    }
}

/// `input.sameTurnSteering` of `session`'s live adapter `generation`, when
/// cutex has read it (`None`: not read for this generation).
pub fn live_same_turn_steering(session: &str, generation: u64) -> Option<bool> {
    let cache = live_steering().lock().ok()?;
    cache.get(session).filter(|(known, _)| *known == generation).map(|(_, steers)| *steers)
}

/// The projected `turnSteer` of a Claude session: the deployed adapter's
/// static claim (`features.json`) and, once read, the live generation's
/// `sameTurnSteering`; either one saying no wins.
pub fn projected_turn_steer(static_claim: &Value, live: Option<bool>) -> bool {
    static_claim.as_bool() == Some(true) && live != Some(false)
}

/// `features` of an adapter source's `features.json` (null when absent).
pub fn adapter_features(adapter_source: &Path) -> Value {
    cached_file_value(&adapter_source.join("src/cute_claude/features.json"), |raw| {
        serde_json::from_slice::<Value>(raw)
            .ok()?
            .get("features")
            .filter(|features| features.is_object())
            .cloned()
    })
}

fn cached_file_value(path: &Path, parse: impl Fn(&[u8]) -> Option<Value>) -> Value {
    type Cache = std::collections::HashMap<PathBuf, (std::time::SystemTime, Value)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let Ok(modified) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return Value::Null;
    };
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, value)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(path) {
        if *at == modified {
            return value.clone();
        }
    }
    let value = std::fs::read(path)
        .ok()
        .and_then(|raw| parse(&raw))
        .unwrap_or(Value::Null);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), (modified, value.clone()));
    value
}

/// Observe only an already running native owner. This never calls Start.
/// Tokens stay inside the transport endpoint and are never included in API data.
pub struct ClaudeObservation {
    pub record: CutexSessionRecord,
    pub endpoint: AppServerEndpoint,
    pub resume: crate::app_server::commands::ThreadResumeParams,
    pub schema: crate::app_server::journal::AppServerSchemaIdentity,
    pub diagnostic_path: PathBuf,
}

/// A confirmed inactive occurrence must wait for Start or a new runtime identity,
/// rather than repeatedly probing a stale cached session claim.
#[derive(Debug)]
pub enum ClaudeObservationOffline {
    RuntimeOffline,
    OccurrenceOffline,
}
impl std::fmt::Display for ClaudeObservationOffline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RuntimeOffline => "Claude runtime is offline",
            Self::OccurrenceOffline => "Claude native occurrence is not online",
        })
    }
}
impl std::error::Error for ClaudeObservationOffline {}

pub fn observation(id: &str) -> anyhow::Result<ClaudeObservation> {
    let record = record(id)?;
    if crate::agent_management::agent_execution_blocked(&record) || record.runtime_pid.is_none() {
        return Err(ClaudeObservationOffline::RuntimeOffline.into());
    }
    let session = record.claude_session.as_ref().unwrap();
    let (client, ready) = connect(session)?;
    let status = client.handle().request(
        "claude/runtime/status",
        json!({"threadId":session.thread_id}),
    )?;
    validate_status(session, &status)?;
    validate_observation_occurrence(session.connection_id.as_deref(), record.runtime_pid, ready.pid, &status)?;
    let endpoint = AppServerEndpoint::LoopbackWebSocket {
        url: ready.endpoint,
        bearer_token: Some(
            std::fs::read_to_string(session.state_dir.join("rpc-token"))?
                .trim()
                .into(),
        ),
    };
    let schema = crate::app_server::journal::AppServerSchemaIdentity {
        version: ADAPTER_SCHEMA_VERSION.into(),
        sha256: crate::agent_management::file_sha256(
            &session
                .deployment
                .adapter_source
                .join("src/cute_claude/protocol.py"),
        )?
        .as_str()
        .to_string(),
    };
    let resume = crate::app_server::commands::ThreadResumeParams {
        thread_id: session.thread_id.clone(),
        model: Some(session.model.clone()),
        model_provider: Some("cute-claude".into()),
        ..Default::default()
    };
    Ok(ClaudeObservation {
        endpoint,
        resume,
        schema,
        diagnostic_path: session.state_dir.join("management-events.jsonl"),
        record,
    })
}

fn validate_observation_occurrence(
    connection_id: Option<&str>, recorded_pid: Option<u32>, adapter_pid: u32, status: &Value,
) -> anyhow::Result<()> {
    // Reconnect only the recorded occurrence, never adopt a replacement.
    if status["connectionId"].as_str() != connection_id || recorded_pid != Some(adapter_pid) {
        return Err(ClaudeObservationOffline::OccurrenceOffline.into());
    }
    match status["state"].as_str() {
        Some("waiting" | "working" | "interrupting") => Ok(()),
        // The SDK can replace a failed child under this same adapter lease.
        // Suspending here permanently strands a recovered, still-live runtime:
        // the observer only reactivates a suspended entry on identity change.
        Some("starting" | "reconnecting") => anyhow::bail!(
            "Claude native occurrence is temporarily {}: {}",
            status["state"].as_str().unwrap(), status["lastError"].as_str().unwrap_or("recovery in progress")
        ),
        _ => Err(ClaudeObservationOffline::OccurrenceOffline.into()),
    }
}

pub fn attach(id: &str) -> anyhow::Result<()> {attach_status(id).map(|_|())}

pub fn attach_status(id: &str) -> anyhow::Result<std::process::ExitStatus> {
    let live = status(id)?;
    ensure!(
        matches!(
            live["state"].as_str(),
            Some("waiting" | "working" | "interrupting")
        ),
        "Claude runtime is offline; start it first"
    );
    let record = record(id)?;
    let session = record.claude_session.as_ref().unwrap();
    let (_, ready) = connect(session)?;
    let accounts: crate::profiles::model::AccountsStore = serde_json::from_slice(&std::fs::read(crate::config::paths::accounts_path()?)?)?;
    let profile_id = record.profile.as_deref().map(|name| crate::profiles::lookup::find_account(&accounts, name))
        .transpose()?.flatten().map(|account| account.id.as_str());
    let (preferences_path, preferences) = crate::config::status_line::load(profile_id)?;
    let order: Vec<String> = preferences.get("tui").and_then(|t| t.get("status_line"))
        .map(|v| v.clone().try_into()).transpose()?.unwrap_or_else(||
            ["model-with-reasoning","current-dir","cutex_welcome","cutex_profile","cutex_notification"].into_iter().map(String::from).collect());
    let config = crate::config::store::load_codez_config_checked()?;
    let catalog = crate::profiles::materialize::custom_status_items_catalog_json(&config)?.context("status catalog missing")?;
    let status_file = crate::launch::selected_status::materialize_session_display(catalog.as_bytes(), &order, record.profile.as_deref().unwrap_or("Claude"))?;
    let ui = session.state_dir.join("tui-launch.json");
    write_private_pretty_json_atomic(&ui, &json!({"statusLine":order,"statusFile":status_file,
        "useColors":preferences.get("tui").and_then(|t|t.get("status_line_use_colors")).and_then(|v|v.as_bool()).unwrap_or(true),
        "preferencesPath":preferences_path,"cutexBinary":std::env::current_exe()?}), "Claude frontend settings")?;
    let status = Command::new(&session.deployment.python)
        .env("CUTEX_AGENT_STATUS", crate::launch::agent_status::for_record(&record))
        .arg("-u")
        .arg(
            session
                .deployment
                .adapter_source
                .join("scripts/connect_tui.py"),
        )
        .arg("--binary")
        .arg(&session.deployment.tui_binary)
        .arg("--endpoint")
        .arg(ready.endpoint)
        .arg("--model")
        .arg(live["model"].as_str().unwrap_or(&session.model))
        .arg("--workspace")
        .arg(&record.cwd)
        .arg("--token-file")
        .arg(session.state_dir.join("rpc-token"))
        .arg("--ui-config")
        .arg(ui)
        .arg("--resume")
        .arg(&session.thread_id)
        .status()?;
    ensure!(
        status.success(),
        "Claude foreground TUI exited {status}; backend was not restarted"
    );
    Ok(status)
}

#[cfg(test)]
mod tests {
    #[test]
    fn observer_retries_transient_recovery_without_losing_the_occurrence_fence() {
        use super::*;
        use crate::management::v2::claude_observer::{ObserverState, RuntimeIdentity};
        let identity = RuntimeIdentity { generation: 16, pid: 42, connection_id: Some("same".into()) };
        let now = std::time::Instant::now();
        let mut schedule = ObserverState::default();
        schedule.activate("agent", identity.clone(), now);
        for state in ["starting", "reconnecting"] {
            let status = json!({"state":state,"connectionId":"same","lastError":"socket reset"});
            let error = validate_observation_occurrence(Some("same"), Some(42), 42, &status).unwrap_err();
            assert!(error.downcast_ref::<ClaudeObservationOffline>().is_none(), "transient recovery must not suspend observation");
            assert!(error.to_string().contains("socket reset"));
            schedule.complete("agent", &identity, Some(("observe", &error.to_string())), now);
        }
        let later = now + Duration::from_secs(10);
        assert!(schedule.due("agent", later));
        let recovered = json!({"state":"waiting","connectionId":"same"});
        validate_observation_occurrence(Some("same"), Some(42), 42, &recovered).unwrap();
        schedule.complete("agent", &identity, None, later);
        assert_eq!(schedule.snapshot(later)["sessions"][0]["state"], "observing");
        for status in [
            json!({"state":"closed","connectionId":"same"}),
            json!({"state":"reconnecting","connectionId":"different"}),
        ] {
            assert!(validate_observation_occurrence(Some("same"), Some(42), 42, &status)
                .unwrap_err().downcast_ref::<ClaudeObservationOffline>().is_some());
        }
        assert!(validate_observation_occurrence(Some("same"), Some(41), 42, &recovered)
            .unwrap_err().downcast_ref::<ClaudeObservationOffline>().is_some());
    }

    #[test]
    fn approval_wait_only_for_adapters_that_declare_it() {
        let path = std::path::Path::new("/x/cutex-mcp");
        let old = super::mcp_tools_descriptor(path, json!([]), &json!({"externalInput": true}));
        assert!(old.get("approvalWaitSecs").is_none(), "{old}");
        let new = super::mcp_tools_descriptor(path, json!([{"name": "service_register"}]), &json!({"approvalWait": true}));
        assert_eq!(new["approvalWaitSecs"], 120);
        assert_eq!(new["tools"][0]["name"], "service_register");
        assert_eq!(new["executable"], "/x/cutex-mcp");
    }

    #[test]
    fn launch_permission_modes_follow_the_adapter_in_display_order() {
        let features = json!({"claudePermissionModes": ["bypassPermissions", "plan", "default", "bogus"]});
        assert_eq!(super::permission_modes_of(&features), ["default", "plan", "bypassPermissions"]);
        assert!(super::permission_modes_of(&json!({})).is_empty());
        assert!(super::permission_modes_of(&Value::Null).is_empty());
    }

    #[test]
    fn initial_mode_patches_are_the_adapter_mapping() {
        let patch = |mode| super::initial_mode_patch("t1", mode).unwrap();
        assert_eq!(patch("bypassPermissions")["approvalPolicy"], "never");
        assert_eq!(patch("auto")["approvalsReviewer"], "auto_review");
        assert_eq!(patch("acceptEdits")["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(patch("acceptEdits")["approvalsReviewer"], "user");
        assert_eq!(patch("default")["sandboxPolicy"]["type"], "readOnly");
        let plan = patch("plan");
        assert_eq!(plan["collaborationMode"]["mode"], "plan");
        assert!(plan.get("approvalPolicy").is_none(), "plan keeps the mode underneath");
        assert!(["default", "acceptEdits", "plan", "auto", "bypassPermissions"].iter().all(|m| patch(m)["threadId"] == "t1"));
        assert!(super::initial_mode_patch("t1", "dontAsk").is_err());
    }

    #[test]
    fn initial_permission_mode_is_omitted_when_unset() {
        let session: super::ClaudeSession = serde_json::from_value(json!({
            "deployment": {"python": "p", "adapter_source": "a", "claude_cli": "c", "claude_home": "h", "tui_binary": "t"},
            "thread_id": "t", "state_dir": "s", "model": "m"
        })).unwrap();
        assert!(session.initial_permission_mode.is_none());
        let saved = serde_json::to_value(&session).unwrap();
        assert!(saved.get("initial_permission_mode").is_none());
    }

    use super::*;

    #[cfg(unix)]
    #[test]
    fn archive_replay_accepts_an_already_missing_adapter_pid() {
        let root = std::env::temp_dir().join(format!(
            "cutex-claude-archive-replay-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut child = Command::new("sleep").arg("0").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        std::fs::write(
            root.join("server.json"),
            json!({"endpoint":"ws://127.0.0.1:1","runtimeId":"runtime","pid":pid}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("rpc-token"), "test-token").unwrap();
        let session = ClaudeSession {
            deployment: ClaudeDeployment {
                python: "/python".into(),
                adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(),
                claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: "adapter-thread".into(),
            state_dir: root.clone(),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false, write_explicit: false, exec_explicit: false,
            native_session_id: Some("native-session".into()),
            connection_id: Some("connection".into()),
            initial_permission_mode: None,
        };
        let mut record = CutexSessionRecord::new_at(
            "cutex.archive-replay".into(),
            None,
            crate::platform::host::current_host_name(),
            "/tmp".into(),
            None,
            "2026-09-28T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        record.runtime_pid = Some(pid);
        record.claude_session = Some(session);

        let mut runtime = ClaudeArchiveRuntime::prepare(&record).unwrap();
        assert_eq!(runtime.adapter_pid, Some(pid));
        assert_eq!(runtime.adapter_started_at, None);
        runtime.stop_and_verify(&record).unwrap();
        runtime.verify_offline(&record).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// A Claude record whose adapter died days ago: PID gone, `server.json`
    /// still names a loopback endpoint that refuses (live 2026-10-15 report).
    #[cfg(unix)]
    fn stale_claude_record(id: &str, state_dir: &Path, pid: u32) -> CutexSessionRecord {
        std::fs::create_dir_all(state_dir).unwrap();
        std::fs::write(
            state_dir.join("server.json"),
            json!({"endpoint":"ws://127.0.0.1:1","runtimeId":"runtime","pid":pid}).to_string(),
        )
        .unwrap();
        std::fs::write(state_dir.join("rpc-token"), "test-token").unwrap();
        let mut record = CutexSessionRecord::new_at(
            id.into(),
            None,
            crate::platform::host::current_host_name(),
            "/tmp".into(),
            Some("rowan-browser".into()),
            "2026-10-11T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        record.runtime_pid = Some(pid);
        record.runtime_generation = 1;
        record.current_runtime_agent_id = Some("claude.connection".into());
        record.claude_session = Some(ClaudeSession {
            deployment: ClaudeDeployment {
                python: "/python".into(),
                adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(),
                claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: "adapter-thread".into(),
            state_dir: state_dir.to_path_buf(),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false, write_explicit: false, exec_explicit: false,
            native_session_id: Some("native-session".into()),
            connection_id: Some("connection".into()),
            initial_permission_mode: None,
        });
        record
    }

    #[cfg(unix)]
    #[test]
    fn dead_adapter_claim_is_cleared_only_for_the_exact_dead_occurrence() {
        use crate::session::store::{load_cutex_session_store_from_path, save_cutex_session_store_to_path};
        let root = std::env::temp_dir().join(format!("cutex-claude-dead-{}", uuid::Uuid::new_v4()));
        let path = root.join("sessions.json");
        let pid = dead_pid();
        let record = stale_claude_record("cutex.dead", &root.join("state"), pid);
        let mut store = crate::session::model::CutexSessionStore::default();
        store.sessions.insert("cutex.dead".into(), record.clone());
        save_cutex_session_store_to_path(&path, &store).unwrap();
        let load = || load_cutex_session_store_from_path(&path).unwrap().sessions["cutex.dead"].clone();

        // A live (or unknown) PID is never cleared.
        assert!(!clear_dead_adapter_claim(&path, "cutex.dead", &record, |_| true).unwrap());
        // A Start rebound the session meanwhile: its new claim stays.
        let mut rebound = record.clone();
        rebound.runtime_generation = 2;
        assert!(!clear_dead_adapter_claim(&path, "cutex.dead", &rebound, |_| false).unwrap());
        assert_eq!(load().runtime_pid, Some(pid));

        assert!(clear_dead_adapter_claim(&path, "cutex.dead", &record, |_| false).unwrap());
        let cleared = load();
        assert_eq!(cleared.runtime_pid, None);
        assert_eq!(cleared.current_runtime_agent_id, None);
        // Identity and configuration stay: the next Start resumes the same
        // native session on the configured profile.
        assert_eq!(cleared.runtime_generation, 1);
        assert_eq!(cleared.profile.as_deref(), Some("rowan-browser"));
        assert_eq!(cleared.claude_session, record.claude_session);
        // Idempotent: the observed occurrence is gone now.
        assert!(!clear_dead_adapter_claim(&path, "cutex.dead", &record, |_| false).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Owner report 2026-10-15: stopping / switching / deleting a Claude agent
    /// whose adapter died failed with `failed to connect app-server loopback
    /// websocket … Connection refused`. The close now settles the stale claim
    /// and reports offline; the dead-adapter observer reconciliation too.
    #[cfg(unix)]
    #[test]
    fn close_and_observer_settle_a_dead_adapter_as_offline() {
        let _guard = crate::config::paths::test_home_env_lock();
        let home = std::env::temp_dir().join(format!("cutex-claude-close-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join(".cutex")).unwrap();
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let run = || {
            let path = cutex_sessions_path().unwrap();
            let mut store = crate::session::model::CutexSessionStore::default();
            let close = stale_claude_record("cutex.close", &home.join("close"), dead_pid());
            let observed = stale_claude_record("cutex.observed", &home.join("observed"), dead_pid());
            let mut live = stale_claude_record("cutex.live", &home.join("live"), std::process::id());
            live.runtime_generation = 3;
            for record in [close, observed, live] {
                store.sessions.insert(record.cutex_session_id.clone(), record);
            }
            crate::session::store::save_cutex_session_store_to_path(&path, &store).unwrap();

            let status = close_expected("cutex.close", Some(1)).unwrap();
            assert_eq!(status["state"], "offline");
            assert_eq!(status["staleRuntimeCleared"], true);
            let after = record("cutex.close").unwrap();
            assert_eq!((after.runtime_pid, after.current_runtime_agent_id), (None, None));
            // A second close is the ordinary offline no-op.
            assert_eq!(close_expected("cutex.close", Some(1)).unwrap()["state"], "offline");

            assert!(reconcile_dead_adapter("cutex.observed").unwrap());
            assert_eq!(record("cutex.observed").unwrap().runtime_pid, None);
            assert!(!reconcile_dead_adapter("cutex.observed").unwrap());

            // Alive but unreachable: fail closed, the claim stays.
            assert!(!reconcile_dead_adapter("cutex.live").unwrap());
            assert!(close_expected("cutex.live", Some(3)).is_err());
            assert_eq!(record("cutex.live").unwrap().runtime_pid, Some(std::process::id()));
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn profile_compaction_validates_and_passes_native_environment() {
        use crate::profiles::claude_compaction::ClaudeCompaction;
        let settings: ClaudeCompaction = serde_json::from_str(
            r#"{"auto_compact_window":300000,"auto_compact_percent":80}"#).unwrap();
        settings.validate().unwrap();
        let mut command = Command::new("unused-test-command");
        command.envs(settings.envs());
        let env: std::collections::BTreeMap<_, _> = command.get_envs()
            .map(|(k,v)| (k.to_str().unwrap(),v.unwrap().to_str().unwrap())).collect();
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "300000");
        assert_eq!(env["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "80");
        for (window,percent) in [(99999,80),(1000001,80),(300000,0),(300000,101)] {
            assert!(ClaudeCompaction { auto_compact_window:window, auto_compact_percent:percent }.validate().is_err());
        }
    }

    #[test]
    fn checkpointing_flag_defaults_on_and_is_compatible_with_old_adapters() {
        assert!(crate::profiles::settings_api::DEFAULT_CLAUDE_FILE_CHECKPOINTING);
        assert_eq!(file_checkpointing_args(&json!({"fileCheckpointing": true}), true),
            ["--file-checkpointing", "enabled"]);
        assert_eq!(file_checkpointing_args(&json!({"fileCheckpointing": true}), false),
            ["--file-checkpointing", "disabled"]);
        for older in [json!(null), json!({}), json!({"fileCheckpointing": false})] {
            assert!(file_checkpointing_args(&older, true).is_empty());
        }
        let shipped: Value = serde_json::from_str(include_str!(
            "../../components/claude-runtime/src/cute_claude/features.json"
        )).unwrap();
        assert_eq!(shipped["features"]["fileCheckpointing"], true);
    }

    #[test]
    fn agent_turn_questions_flag_only_for_adapters_that_declare_it() {
        let declared = json!({"agentTurnQuestions": true});
        assert_eq!(
            agent_turn_questions_args(&declared, AgentTurnQuestions::Decide),
            ["--agent-turn-questions", "decide"]
        );
        assert_eq!(
            agent_turn_questions_args(&declared, AgentTurnQuestions::Wait),
            ["--agent-turn-questions", "wait"]
        );
        for older in [json!(null), json!({}), json!({"agentTurnQuestions": false})] {
            assert!(agent_turn_questions_args(&older, AgentTurnQuestions::Wait).is_empty());
        }
        let shipped: Value = serde_json::from_str(include_str!(
            "../../components/claude-runtime/src/cute_claude/features.json"
        ))
        .unwrap();
        assert_eq!(shipped["features"]["agentTurnQuestions"], true);
    }

    #[test]
    fn system_append_file_only_for_adapters_that_declare_it() {
        let root = std::env::temp_dir().join(format!("cutex-claude-append-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let session = ClaudeSession {
            deployment: ClaudeDeployment {
                python: "/python".into(),
                adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(),
                claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: "adapter-thread".into(),
            state_dir: root.clone(),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false, write_explicit: false, exec_explicit: false,
            native_session_id: None,
            connection_id: None,
            initial_permission_mode: None,
        };
        let mut record = CutexSessionRecord::new(
            "cutex.append-test".into(),
            None,
            crate::platform::host::current_host_name(),
            root.to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        record.formal_agent_name = Some("Appender".into());
        record.agent_enabled = true;
        for older in [json!(null), json!({}), json!({"systemAppend": false})] {
            assert!(write_system_append(&record, &session, &older).unwrap().is_none());
        }
        assert!(!root.join(SYSTEM_APPEND_FILE).exists());
        let path = write_system_append(&record, &session, &json!({"systemAppend": true}))
            .unwrap()
            .expect("declared");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Cutex\n") && text.contains("- Name: Appender (id cutex.append-test)"));
        assert!(text.contains("# File references"));
        let shipped: Value = serde_json::from_str(include_str!(
            "../../components/claude-runtime/src/cute_claude/features.json"
        ))
        .unwrap();
        assert_eq!(shipped["features"]["systemAppend"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn turn_steer_is_projected_only_when_the_live_adapter_steers() {
        let session = format!("cutex.steer-{}", uuid::Uuid::new_v4().simple());
        assert_eq!(live_same_turn_steering(&session, 3), None);
        note_live_capabilities(&session, 3, &json!({"input": {"sameTurnSteering": false}}));
        assert_eq!(live_same_turn_steering(&session, 3), Some(false));
        assert_eq!(live_same_turn_steering(&session, 4), None, "another generation is unknown");
        note_live_capabilities(&session, 4, &json!({"input": {"sameTurnSteering": true}}));
        assert_eq!(live_same_turn_steering(&session, 4), Some(true));
        // Static claim and live report: either one saying no wins.
        assert!(projected_turn_steer(&json!(true), None));
        assert!(projected_turn_steer(&json!(true), Some(true)));
        assert!(!projected_turn_steer(&json!(true), Some(false)));
        assert!(!projected_turn_steer(&json!(false), Some(true)));
        assert!(!projected_turn_steer(&Value::Null, None));
    }

    #[test]
    fn native_schema_reports_the_deployed_adapter_features() {
        use sha2::Digest;
        let root = std::env::temp_dir().join(format!("cutex-claude-schema-{}", uuid::Uuid::new_v4()));
        let source = root.join("src/cute_claude");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("protocol.py"), "PROTOCOL = 1
").unwrap();
        let session: ClaudeSession = serde_json::from_value(json!({
            "deployment":{"python":"/python","adapter_source":root,"claude_cli":"/claude","claude_home":"/home","tui_binary":"/tui"},
            "thread_id":"adapter-thread", "state_dir":"/state", "model":"sonnet"
        })).unwrap();
        // An adapter that predates features.json: unknown, never guessed.
        let schema = native_schema(&session);
        assert_eq!(schema["protocol"], "claude-app-server-subset");
        assert_eq!(schema["version"], ADAPTER_SCHEMA_VERSION);
        assert_eq!(schema["capabilities"]["features"], Value::Null);
        assert_eq!(
            schema["sha256"],
            format!("{:x}", sha2::Sha256::digest(b"PROTOCOL = 1
"))
        );
        let features = source.join("features.json");
        std::fs::write(&features, r#"{"version":1,"features":{"turnSteer":false,"approvals":true}}"#).unwrap();
        assert_eq!(
            native_schema(&session)["capabilities"]["features"],
            json!({"turnSteer": false, "approvals": true})
        );
        // A redeployed file is re-read (cache keyed by modification time).
        std::fs::write(&features, r#"{"version":1,"features":{"turnSteer":true,"skillInput":true}}"#).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&features)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            native_schema(&session)["capabilities"]["features"],
            json!({"turnSteer": true, "skillInput": false})
        );
        assert_eq!(adapter_features(&root)["skillInput"], true, "native TUI catalog remains enabled");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_cannot_rebind_a_different_thread_or_native_session() {
        let session: ClaudeSession = serde_json::from_value(json!({
            "deployment":{"python":"/python","adapter_source":"/adapter","claude_cli":"/claude","claude_home":"/home","tui_binary":"/tui"},
            "thread_id":"adapter-thread", "state_dir":"/state", "model":"sonnet", "native_session_id":"native-session"
        })).unwrap();
        assert!(validate_status(
            &session,
            &json!({"threadId":"other","sessionId":"native-session"})
        )
        .is_err());
        assert!(validate_status(
            &session,
            &json!({"threadId":"adapter-thread","sessionId":"other"})
        )
        .is_err());
        assert!(validate_status(
            &session,
            &json!({"threadId":"adapter-thread","sessionId":"native-session"})
        )
        .is_ok());
        assert!(validate_status(
            &session,
            &json!({"threadId":"adapter-thread","state":"offline"})
        )
        .is_ok());
    }
}
