//! Human history editing. Native APIs own the mutation; Cutex retains identity.
use super::session_tui_layout as theme;
use anyhow::{ensure, Context};
use cutex::app_server::client::{AppServerClient, AppServerClientError, AppServerClientOptions, AppServerHandle};
#[cfg(all(test, target_os = "linux"))]
use cutex::app_server::client::AppServerEndpoint;
use cutex::catalog::{CatalogEndpoint, OwnedStdioEndpoint};
use cutex::management::v2::user_input::UserInputExecutionError;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Native `thread/revert` interrupts, shuts down, rewrites and reloads the
/// thread; allow it more than the runtime client's default request timeout.
const NATIVE_REVERT_TIMEOUT: Duration = Duration::from_secs(120);

/// A typed refusal or failure the Management API maps to a stable error code.
#[derive(Debug)]
pub(crate) struct RevertFailure {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub outcome_unknown: bool,
}
impl std::fmt::Display for RevertFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RevertFailure {}
fn failure(code: &'static str, message: impl Into<String>, retryable: bool) -> anyhow::Error {
    RevertFailure { code, message: message.into(), retryable, outcome_unknown: false }.into()
}

/// Facts about one completed revert (shared by the TUI and Management API).
#[derive(Debug)]
pub(crate) struct RevertOutcome {
    pub action_id: String,
    pub thread_id: String,
    pub before_turn_id: String,
    /// Removed turn IDs, newest first.
    pub removed_turn_ids: Vec<String>,
    pub backup: PathBuf,
    pub receipt: PathBuf,
    pub native_method: &'static str,
    pub native_result: Value,
}

#[derive(Clone, Debug)]
pub(super) struct Turn {
    pub id: String,
    pub text: String,
    pub time: String,
    pub active: bool,
    pub legacy_input_turn: bool,
}
#[derive(Clone, Debug)]
pub(super) struct History {
    pub thread: String,
    pub turns: Vec<Turn>, // newest first, ordered by provider (not timestamps)
    pub paginated: bool,
    pub path: PathBuf,
}
enum Connection {
    /// A separate client connection to a runtime's loopback/unix listener.
    Live(AppServerClient),
    /// The daemon's own connection to the session runtime it manages.
    Managed(AppServerHandle),
    Isolated(OwnedStdioEndpoint),
}
impl Connection {
    fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        match self {
            Self::Live(client) => Ok(client.handle().request(method, params)?),
            Self::Managed(handle) => {
                Ok(handle.request_with_timeout(method, params, NATIVE_REVERT_TIMEOUT)?)
            }
            Self::Isolated(endpoint) => Ok(endpoint.request(method, params)?),
        }
    }
    fn is_isolated(&self) -> bool {
        matches!(self, Self::Isolated(_))
    }
}

/// Resolve a Cutex session (or exact native thread) ID to its record, thread and native home.
fn resolve(
    id: &str,
) -> anyhow::Result<(Option<cutex::session::model::CutexSessionRecord>, String, PathBuf)> {
    let store = cutex::session::store::load_cutex_session_store()?;
    let record = store
        .sessions
        .get(id)
        .or_else(|| {
            store
                .sessions
                .values()
                .find(|r| r.codex_session_id.as_deref() == Some(id))
        })
        .cloned();
    let thread = record
        .as_ref()
        .and_then(|r| r.codex_session_id.clone())
        .unwrap_or_else(|| id.into());
    ensure!(
        uuid::Uuid::parse_str(&thread)?.to_string() == thread,
        "exact native thread ID required"
    );
    let home = native_home_for(record.as_ref())?;
    if let Some(record) = &record {
        if record.is_retired() {
            return Err(failure("history_revert_unsupported", "retired history is read-only", false));
        }
    }
    Ok((record, thread, home))
}
/// The session's native home: its explicit-launch home, else the selected
/// deployment's (or host) Codex home.
pub(crate) fn native_home_for(
    record: Option<&cutex::session::model::CutexSessionRecord>,
) -> anyhow::Result<PathBuf> {
    match record.and_then(|r| r.explicit_launch.as_ref()) {
        Some(contract) => Ok(contract.native_home.clone()),
        None => cutex::launch::local_deployment::LocalDeployment::source_home(),
    }
}

/// Reply bound of offline history readers: a native `thread/turns/list
/// itemsView=full` page of an image-generating agent was 393 MB (30 turns, up
/// to 88 MB each), far past the stdio default of 8 MiB, which failed the read.
const HISTORY_MAX_MESSAGE_BYTES: usize = 1024 * 1024 * 1024;

/// A private, initialized stdio app-server on `home` for an offline session.
/// No thread is resumed: callers that only read stay read-only.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) fn isolated_endpoint(
    record: Option<&cutex::session::model::CutexSessionRecord>,
    home: &Path,
) -> anyhow::Result<OwnedStdioEndpoint> {
    let cwd = record
        .map(|r| PathBuf::from(&r.cwd))
        .filter(|cwd| cwd.is_dir())
        .map(Ok)
        .unwrap_or_else(|| {
            if record.is_some() {
                Ok(home.to_path_buf())
            } else {
                std::env::current_dir()
            }
        })?;
    if let Some((record, contract)) =
        record.and_then(|r| r.explicit_launch.as_ref().map(|c| (r, c)))
    {
        let bundle = cutex::launch::stock::StockBundle::load_running(contract)?;
        let profile = cutex::launch::stock::current_configuration(record)?;
        let launch = super::stock_lifecycle::configured(
            super::stock_lifecycle::clean_launch(&bundle.executable.path, home)?,
            &profile,
            true,
            matches!(contract.version, 3 | 4).then_some(home),
            &bundle,
            home,
        )?
        .args(["--listen", "stdio://"]);
        let mut command = launch.to_command();
        command.current_dir(&cwd);
        let mut options =
            cutex::catalog::StdioAppServerOptions::new(&bundle.executable.path, home.to_path_buf());
        options.max_message_bytes = HISTORY_MAX_MESSAGE_BYTES;
        let mut endpoint = OwnedStdioEndpoint::spawn_command(options, command)?;
        let initialized = endpoint.request("initialize", json!({"clientInfo":{"name":"cutex_history","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))?;
        // Compare canonically: Windows homes are stored verbatim (`\\?\C:\...`)
        // while the native runtime reports the plain path.
        ensure!(
            initialized["codexHome"]
                .as_str()
                .and_then(|reported| Path::new(reported).canonicalize().ok())
                == Some(home.canonicalize()?),
            "native history home mismatch"
        );
        endpoint.notify("initialized", None)?;
        Ok(endpoint)
    } else {
        let launch = super::session_native_workflow::NativeLaunch {
            cwd,
            native_home: home.to_path_buf(),
            profile: record.and_then(|r| r.profile.clone()),
            model: None,
        };
        launch.endpoint_bounded(Some(HISTORY_MAX_MESSAGE_BYTES))
    }
}

/// `ManagementRequestContext::open_offline_history`: a read-only native
/// reader on an offline session's own home (active or retired sessions).
pub(crate) fn open_offline_history_endpoint(
    cutex_session_id: &str,
) -> anyhow::Result<(Box<dyn cutex::catalog::CatalogEndpoint>, PathBuf)> {
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = cutex_session_id;
        anyhow::bail!("offline history requires a local Linux or Windows runtime")
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        let store = cutex::session::store::load_cutex_session_store()?;
        let key = cutex::session::service::cutex_session_key_for_user_id_including_retired(
            &store,
            cutex_session_id,
        )
        .context("cutex session not found")?;
        let record = store.sessions.get(&key).context("cutex session not found")?;
        ensure!(
            cutex::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &cutex::platform::host::current_host_name(),
            ),
            "offline history is only available on the session's own host"
        );
        if let Some(session) = &record.dsh_session {
            return Ok((Box::new(dsh_history_reader(session, &record.cwd)?), session.state_dir.clone()));
        }
        if let Some(session) = &record.claude_session {
            return Ok((
                Box::new(claude_history_reader(session)?),
                session.state_dir.clone(),
            ));
        }
        let home = native_home_for(Some(record))?;
        let endpoint = isolated_endpoint(Some(record), &home)?;
        Ok((Box::new(endpoint), home))
    }
}

/// `ManagementRequestContext::load_latest_plan`: `cutexState.latestPlan` from
/// the session's current native rollout (`cutex::launch::native_plan`).
/// `null` for sessions of another host, unbound sessions and whenever the
/// rollout cannot be read (it is presentation state only). A local Claude or
/// DSH session (no rollout) serves the newest checklist its adapter sent for
/// its current thread (`management::v2::claude_plan`).
pub(crate) fn latest_plan_for_management(cutex_session_id: &str) -> Value {
    if let Some(plan) = claude_latest(cutex_session_id, cutex::management::v2::claude_plan::latest) {
        return plan;
    }
    derive_from_native_rollout(cutex_session_id, cutex::launch::native_plan::latest_plan)
}

/// `ManagementRequestContext::load_latest_token_usage`:
/// `cutexState.latestTokenUsage` from the session's current native rollout
/// (`cutex::launch::native_token_usage`); `null` in the same cases as
/// [`latest_plan_for_management`], except that a local Claude or DSH session
/// serves the newest usage report its adapter sent for its current thread
/// (`management::v2::claude_token_usage`; neither has a rollout).
pub(crate) fn latest_token_usage_for_management(cutex_session_id: &str) -> Value {
    if let Some(usage) = claude_latest(cutex_session_id, cutex::management::v2::claude_token_usage::latest) {
        return usage;
    }
    derive_from_native_rollout(
        cutex_session_id,
        cutex::launch::native_token_usage::latest_token_usage,
    )
}

/// `Some` (a value or `null`) for a local Claude session, `None` otherwise:
/// what `latest(cutex session id, bound thread)` keeps for it
/// (`claude_token_usage` / `claude_plan`).
fn claude_latest(
    cutex_session_id: &str,
    latest: fn(&str, &str) -> anyhow::Result<Option<Value>>,
) -> Option<Value> {
    let store = cutex::session::store::load_cutex_session_store().ok()?;
    claude_latest_in(
        &store,
        cutex_session_id,
        &cutex::platform::host::current_host_name(),
        latest,
    )
}

fn claude_latest_in(
    store: &cutex::session::model::CutexSessionStore,
    cutex_session_id: &str,
    current_host: &str,
    latest: impl Fn(&str, &str) -> anyhow::Result<Option<Value>>,
) -> Option<Value> {
    let key = cutex::session::service::cutex_session_key_for_user_id_including_retired(
        store,
        cutex_session_id,
    )?;
    let record = store.sessions.get(&key)?;
    // Claude and DSH sessions have no rollout; the daemon keeps their values.
    let thread_id = record.claude_session.as_ref().map(|claude| claude.thread_id.as_str())
        .or_else(|| record.dsh_session.as_ref().map(|dsh| dsh.thread_id.as_str()))?;
    if !cutex::runtime::lifecycle::cutex_session_host_is_local(&record.host_id, current_host) {
        return Some(Value::Null);
    }
    Some(
        latest(&record.cutex_session_id, thread_id)
            .ok()
            .flatten()
            .unwrap_or(Value::Null),
    )
}

/// Runs `derive(native home, thread id)` for a local, bound Codex session;
/// `null` otherwise or on any error.
fn derive_from_native_rollout(
    cutex_session_id: &str,
    derive_value: fn(&Path, &str) -> anyhow::Result<Option<Value>>,
) -> Value {
    let derive = || -> anyhow::Result<Option<Value>> {
        let store = cutex::session::store::load_cutex_session_store()?;
        let Some(key) = cutex::session::service::cutex_session_key_for_user_id_including_retired(
            &store,
            cutex_session_id,
        ) else {
            return Ok(None);
        };
        let Some(record) = store.sessions.get(&key) else {
            return Ok(None);
        };
        if record.claude_session.is_some()
            || !cutex::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &cutex::platform::host::current_host_name(),
            )
        {
            return Ok(None);
        }
        let Some(thread) = record.codex_session_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(None);
        };
        let home = native_home_for(Some(record))?;
        derive_value(&home, thread)
    };
    derive().ok().flatten().unwrap_or(Value::Null)
}

/// A stopped Claude agent's history: the deployed adapter's read-only
/// `cute_claude.history_reader` on the agent's private state directory. It
/// never takes the adapter's writer lease and serves only history reads.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn dsh_history_reader(session: &cutex::runtime::dsh::DshSession, cwd: &str) -> anyhow::Result<OwnedStdioEndpoint> {
    let reader = session.adapter.parent().context("DSH adapter source missing")?.join("history-reader.mjs");
    anyhow::ensure!(reader.is_file(), "DSH history reader missing from pinned runtime");
    let config = session.state_dir.join("history-reader.json");
    cutex::config::atomic::write_private_pretty_json_atomic(&config, &json!({
        "database":session.state_dir.join("timeline.sqlite"),"threadId":session.thread_id,
        "nativeSessionId":session.native_session_id,"cwd":cwd
    }), "DSH offline history reader")?;
    let mut command = std::process::Command::new(&session.node);
    command.arg(reader).env_clear().env("DSH_CUTEX_HISTORY_CONFIG", &config).current_dir(&session.state_dir);
    // Its results are redacted like the runtime's (stored deltas of an older
    // timeline may split a secret that the projection joins again).
    command.env("DSH_CUTEX_REDACT", serde_json::to_string(&super::dsh_structure::redaction_secrets(session))?);
    if let Some(sources) = super::dsh_structure::redaction_sources(session) {
        command.env("KIMI_CUTEX_REDACT_SOURCES", sources);
    }
    for key in ["PATH","Path","SystemRoot","WINDIR","TEMP","TMP"] {
        if let Some(value) = std::env::var_os(key) { command.env(key, value); }
    }
    let mut options = cutex::catalog::StdioAppServerOptions::new(&session.node, session.state_dir.clone());
    options.max_message_bytes = 64 * 1024 * 1024;
    let mut endpoint = OwnedStdioEndpoint::spawn_command(options, command)?;
    endpoint.request("initialize", json!({"clientInfo":{"name":"cutex_history","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}))?;
    endpoint.notify("initialized", None)?;
    Ok(endpoint)
}

fn claude_history_reader(
    session: &cutex::runtime::claude::ClaudeSession,
) -> anyhow::Result<OwnedStdioEndpoint> {
    let deployment = &session.deployment;
    let mut command = std::process::Command::new(&deployment.python);
    command
        .args(["-u", "-m", "cute_claude.history_reader", "--state-dir"])
        .arg(&session.state_dir)
        .env("PYTHONPATH", deployment.adapter_source.join("src"))
        .current_dir(&deployment.adapter_source);
    let mut options = cutex::catalog::StdioAppServerOptions::new(
        &deployment.python,
        session.state_dir.clone(),
    );
    // A single turn may be large; pages are bounded by the reader.
    options.max_message_bytes = 64 * 1024 * 1024;
    let mut endpoint = OwnedStdioEndpoint::spawn_command(options, command)?;
    let initialized = endpoint.request(
        "initialize",
        json!({"clientInfo":{"name":"cutex_history","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}),
    )?;
    // An agent that never started has no state directory yet; compare
    // canonically when possible (Windows `\\?\` prefixes).
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    ensure!(
        initialized["codexHome"].as_str().map(|reported| canonical(Path::new(reported)))
            == Some(canonical(&session.state_dir)),
        "Claude history reader state mismatch"
    );
    endpoint.notify("initialized", None)?;
    Ok(endpoint)
}

pub(super) struct Editor {
    connection: Connection,
    thread: String,
    home: PathBuf,
}
impl Editor {
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    pub(super) fn open(_id: &str) -> anyhow::Result<Self> {
        anyhow::bail!("History revert currently requires a local Linux runtime")
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    pub(super) fn open(id: &str) -> anyhow::Result<Self> {
        let (record, thread, home) = resolve(id)?;
        let record = record.as_ref();
        let connection = if let Some(binding) = record.and_then(|r| r.app_server_runtime.as_ref()) {
            let endpoint = cutex::app_server::runtime::endpoint_from_runtime_binding(binding)?;
            let mut options = AppServerClientOptions::new(endpoint);
            options.request_timeout = Duration::from_secs(120);
            Connection::Live(AppServerClient::connect(options)?)
        } else {
            Connection::Isolated(isolated_endpoint(record, &home)?)
        };
        Ok(Self {
            connection,
            thread,
            home,
        })
    }
    /// Edit history through the daemon's own connection to a live runtime:
    /// native `thread/revert` reloads the loaded thread in place, so the
    /// runtime process, its subscriptions and the thread identity survive.
    pub(crate) fn managed(id: &str, handle: AppServerHandle) -> anyhow::Result<Self> {
        let (_, thread, home) = resolve(id)?;
        Ok(Self {
            connection: Connection::Managed(handle),
            thread,
            home,
        })
    }
    pub(super) fn read(&mut self) -> anyhow::Result<History> {
        let metadata = self.connection.request(
            "thread/read",
            json!({"threadId":self.thread,"includeTurns":false}),
        )?;
        let thread = metadata.get("thread").context("thread metadata missing")?;
        ensure!(
            thread["id"].as_str() == Some(&self.thread),
            "thread identity changed"
        );
        let paginated = thread["historyMode"].as_str() == Some("paginated");
        let path = thread["path"]
            .as_str()
            .context("native history path is unavailable")?
            .into();
        let mut turns = Vec::new();
        // 0.154 supports bounded turn summaries for legacy history as well.
        // Never hydrate command output for a history-selection screen.
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let page = self.connection.request("thread/turns/list", json!({"threadId":self.thread,"sortDirection":"desc","limit":100,"itemsView":"summary","cursor":cursor}))?;
            for turn in page["data"].as_array().context("turn page missing")? {
                turns.push(parse_turn(turn)?);
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            let Some(next) = &cursor else {
                break;
            };
            ensure!(
                seen.insert(next.clone()),
                "native history pagination did not advance"
            );
        }
        let mut unique = std::collections::BTreeSet::new();
        ensure!(
            turns.iter().all(|t| unique.insert(t.id.clone())),
            "duplicate native turn IDs"
        );
        Ok(History {
            thread: self.thread.clone(),
            turns,
            paginated,
            path,
        })
    }
    /// TUI entry: keep `keep` and every older turn, after confirming the reviewed list.
    pub(super) fn revert(&mut self, reviewed: &History, keep: &str) -> anyhow::Result<String> {
        let current = self.read()?;
        ensure!(
            current.thread == reviewed.thread && ids(&current) == ids(reviewed),
            "history changed; reload and choose the retained turn again"
        );
        let remove = current
            .turns
            .iter()
            .position(|t| t.id == keep)
            .context("selected turn disappeared")?;
        ensure!(
            remove > 0,
            "this is already the newest turn; nothing to remove"
        );
        ensure!(
            !current.turns[remove].active,
            "cannot retain an unfinished turn"
        );
        let outcome = self.revert_turns(current, remove, Some(keep))?;
        Ok(format!(
            "Removed {} newer turn(s); same session identity. Recovery copy: {}",
            outcome.removed_turn_ids.len(),
            outcome.backup.display()
        ))
    }
    /// Management entry: remove `before_turn_id` and every newer turn.
    pub(crate) fn revert_before(&mut self, before_turn_id: &str) -> anyhow::Result<RevertOutcome> {
        let current = self.read()?;
        let index = current
            .turns
            .iter()
            .position(|t| t.id == before_turn_id)
            .ok_or_else(|| {
                failure(
                    "turn_not_found",
                    format!("turn {before_turn_id} is not in this thread's history"),
                    false,
                )
            })?;
        self.revert_turns(current, index + 1, None)
    }
    /// Remove the newest `remove` turns of `current` (newest first).
    fn revert_turns(
        &mut self,
        mut current: History,
        remove: usize,
        keep: Option<&str>,
    ) -> anyhow::Result<RevertOutcome> {
        ensure!(remove > 0 && remove <= current.turns.len(), "invalid revert boundary");
        let before = current.turns[remove - 1].id.clone();
        if !current.paginated && !current.turns[..remove].iter().all(|t| t.legacy_input_turn) {
            return Err(failure("history_revert_unsupported",
                "this legacy range includes non-user turns; native rollback counts user turns and cannot safely express this boundary. No history was changed", false));
        }
        if let Connection::Isolated(endpoint) = &self.connection {
            ensure!(!has_writer(&current.path, endpoint.child_id())?, "this session is open in another native runtime; disconnect/close it before reverting");
            self.connection.request(
                "thread/resume",
                json!({"threadId":self.thread,"deferGoalContinuation":true}),
            )?;
        }
        if let Some(active) = current.turns.iter().find(|t| t.active) {
            self.connection.request(
                "turn/interrupt",
                json!({"threadId":self.thread,"turnId":active.id}),
            )?;
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let latest = self.read()?;
                if ids(&latest) != ids(&current) {
                    return Err(failure("history_changed", "history changed while stopping; reload before reverting", true));
                }
                if !latest.turns.iter().any(|t| t.active) {
                    current = latest;
                    break;
                }
                ensure!(
                    std::time::Instant::now() < deadline,
                    "current turn has not stopped; history was not reverted"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        // Re-read after stopping and validate the exact source before backing up.
        let metadata = self.connection.request(
            "thread/read",
            json!({"threadId":self.thread,"includeTurns":false}),
        )?;
        let source = PathBuf::from(
            metadata["thread"]["path"]
                .as_str()
                .context("native history path missing")?,
        )
        .canonicalize()?;
        ensure!(
            source.starts_with(self.home.canonicalize()?),
            "history path is outside this session's home"
        );
        let backup = backup_history(&self.home, &source)?;
        let latest = self.read()?;
        if ids(&latest) != ids(&current) || latest.turns.iter().any(|t| t.active) {
            return Err(failure("history_changed", "history changed while preparing the recovery copy; reload before reverting", true));
        }
        let receipt = backup.directory.join("receipt.json");
        let removed_turn_ids: Vec<String> = ids(&current)[..remove].to_vec();
        let mut facts = json!({"actionId":backup.action,"threadId":self.thread,"keepTurnId":keep,"beforeTurnId":before,"removedTurns":remove,"removedTurnIds":removed_turn_ids,"live":!self.connection.is_isolated(),"source":source,"backup":backup.path,"sources":backup.copies,"status":"prepared"});
        cutex::config::atomic::write_private_pretty_json_atomic(
            &receipt,
            &facts,
            "history revert",
        )?;
        let (native_method, params) = if current.paginated {
            ("thread/revert", json!({"threadId":self.thread,"beforeTurnId":before}))
        } else {
            ("thread/rollback", json!({"threadId":self.thread,"numTurns":remove}))
        };
        let native_result = match self.connection.request(native_method, params) {
            Ok(result) => result,
            Err(error) => {
                // A JSON-RPC "method not found" / "invalid request" is a refusal
                // before any mutation (e.g. legacy rollback on a newer native).
                let rejected = matches!(
                    error.downcast_ref::<AppServerClientError>(),
                    Some(AppServerClientError::Rpc(rpc)) if rpc.code == -32601 || rpc.code == -32600
                );
                facts["status"] = json!(if rejected { "rejected" } else { "outcome_unknown" });
                facts["error"] = json!(error.to_string());
                cutex::config::atomic::write_private_pretty_json_atomic(
                    &receipt,
                    &facts,
                    "history revert",
                )?;
                if rejected {
                    return Err(failure(
                        "history_revert_unsupported",
                        format!("native {native_method} was refused: {error}. No history was changed"),
                        false,
                    ));
                }
                return Err(RevertFailure {
                    code: "native_request_failed",
                    message: format!("Revert not confirmed: {error}; recovery record: {}. Reload history before retrying.", receipt.display()),
                    retryable: false,
                    outcome_unknown: true,
                }
                .into());
            }
        };
        let after = self.read()?;
        if ids(&after) != ids(&current)[remove..] {
            return Err(RevertFailure {
                code: "persistence_uncertain",
                message: format!(
                    "revert response received but retained history differs; inspect {}",
                    receipt.display()
                ),
                retryable: false,
                outcome_unknown: true,
            }
            .into());
        }
        facts["status"] = json!("complete");
        facts["resultPath"] = json!(after.path);
        cutex::config::atomic::write_private_pretty_json_atomic(
            &receipt,
            &facts,
            "history revert",
        )?;
        Ok(RevertOutcome {
            action_id: backup.action,
            thread_id: self.thread.clone(),
            before_turn_id: before,
            removed_turn_ids,
            backup: backup.path,
            receipt,
            native_method,
            native_result,
        })
    }
}

/// A private recovery copy of a native rollout (and its ancestors) under
/// `runtime/history-reverts/<action>/`.
pub(crate) struct HistoryBackup {
    pub action: String,
    pub directory: PathBuf,
    /// Copy of the thread's own rollout (`before.jsonl`).
    pub path: PathBuf,
    pub copies: Vec<Value>,
}

pub(crate) fn backup_history(home: &Path, source: &Path) -> anyhow::Result<HistoryBackup> {
    let action = format!("revert-{}", uuid::Uuid::new_v4());
    let directory = cutex::config::paths::runtime_dir()?
        .join("history-reverts")
        .join(&action);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(&directory)?;
        cutex::platform::private_fs::secure_directory(&directory)?;
    }
    #[cfg(not(any(unix, windows)))]
    std::fs::create_dir_all(&directory)?;
    let backup = directory.join("before.jsonl");
    let sources = cutex::launch::native_history::recovery_sources(home, source)?;
    let mut copies = Vec::new();
    for (index, path) in sources.iter().enumerate() {
        let target = if index == 0 {
            backup.clone()
        } else {
            directory.join(format!("ancestor-{index}.jsonl"))
        };
        let size = std::fs::metadata(path)?.len();
        let copied = std::fs::copy(path, &target)
            .context("could not save recovery copy; history unchanged")?;
        if copied != size || std::fs::metadata(path)?.len() != size {
            return Err(failure("history_changed", "history changed during backup; reload before reverting", true));
        }
        // FlushFileBuffers needs write access on Windows; a read-only handle fails with os error 5.
        std::fs::OpenOptions::new().write(true).open(&target)?.sync_all()?;
        copies.push(json!({"source":path,"backup":target}));
    }
    Ok(HistoryBackup {
        action,
        directory,
        path: backup,
        copies,
    })
}

/// `cutex/session/history/revert` backend. Prefers the daemon's own connection
/// to a live runtime (no restart); otherwise falls back to the TUI editor's
/// connection choice (a runtime listener binding, else an isolated runtime).
pub(crate) fn revert_for_management(
    cutex_session_id: &str,
    before_turn_id: &str,
    rewind_files: bool,
) -> Result<Value, UserInputExecutionError> {
    let store = cutex::session::store::load_cutex_session_store().map_err(revert_error)?;
    let record = store.sessions.get(cutex_session_id).ok_or_else(|| {
        revert_error(failure("session_not_found", "the agent does not exist on this host", false))
    })?;
    if record.dsh_session.is_some() {
        return super::dsh_structure::revert(cutex_session_id, before_turn_id, rewind_files);
    }
    if record.claude_session.is_some() {
        return super::claude_structure::revert(record, before_turn_id, rewind_files);
    }
    if rewind_files {
        // Codex agents revert files through cutex/session/files/revert.
        return Err(revert_error(failure(
            "rewind_files_unsupported",
            "rewindFiles applies to Claude agents only; use cutex/session/files/revert for Codex agents",
            false,
        )));
    }
    let (runtime_pid, runtime_generation) = (record.runtime_pid, record.runtime_generation);
    let handle = super::app_server_runtime::runtime_manager()
        .handle(cutex_session_id)
        .ok();
    let live_runtime = handle.is_some();
    // Hold the queued-input lock so an interrupted turn's completion cannot
    // flush queued input into the thread while it is being rewritten.
    let input_lock = super::app_server_user_input::session_input_lock(cutex_session_id)
        .map_err(revert_error)?;
    let _input_guard = input_lock
        .lock()
        .map_err(|_| revert_error(anyhow::anyhow!("session input lock was poisoned")))?;
    let mut editor = match handle {
        Some(handle) => Editor::managed(cutex_session_id, handle),
        None => Editor::open(cutex_session_id),
    }
    .map_err(revert_error)?;
    let live = !editor.connection.is_isolated();
    let outcome = editor.revert_before(before_turn_id).map_err(revert_error)?;
    drop(editor);
    drop(_input_guard);
    if live_runtime {
        // The runtime event worker was parked on the input lock; let it drain
        // the interrupt/revert notifications so the session resource no longer
        // reports the removed turn as active when the client re-bootstraps.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline
            && super::app_server_runtime::runtime_manager()
                .status(cutex_session_id)
                .ok()
                .flatten()
                .is_some_and(|status| status.active_turn_id.is_some())
        {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let removed: Vec<&str> = outcome.removed_turn_ids.iter().map(String::as_str).collect();
    forget_reverted_activity(cutex_session_id, &removed);
    Ok(json!({
        "cutexSessionId": cutex_session_id,
        "threadId": outcome.thread_id,
        "revertedBeforeTurnId": outcome.before_turn_id,
        "removedTurns": outcome.removed_turn_ids.len(),
        "removedTurnIds": outcome.removed_turn_ids,
        "live": live,
        "runtimeRestarted": false,
        "runtimePid": if live_runtime { runtime_pid.map(Value::from).unwrap_or(Value::Null) } else { Value::Null },
        "runtimeGeneration": runtime_generation,
        "actionId": outcome.action_id,
        "backupPath": outcome.backup,
        "receiptPath": outcome.receipt,
        "native": { "method": outcome.native_method, "result": outcome.native_result },
    }))
}

/// After a history revert (Codex, Claude, DSH): the agent list's
/// `lastOutput` / `lastToolCall` must not preview a removed turn
/// (`activity::forget_reverted_turns`). Presentation state: a failure is
/// logged, never fails the revert.
pub(crate) fn forget_reverted_activity(cutex_session_id: &str, removed_turn_ids: &[&str]) {
    if let Err(error) = cutex::management::v2::activity::forget_reverted_turns(cutex_session_id, removed_turn_ids) {
        eprintln!("session activity of {cutex_session_id} not reset after revert: {error:#}");
    }
}

fn revert_error(error: anyhow::Error) -> UserInputExecutionError {
    match error.downcast::<RevertFailure>() {
        Ok(failure) => UserInputExecutionError {
            stage: "route".into(),
            code: failure.code.into(),
            message: failure.message,
            retryable: failure.retryable,
            details: json!({}),
            outcome_unknown: failure.outcome_unknown,
        },
        Err(error) => UserInputExecutionError {
            stage: "route".into(),
            code: "history_revert_failed".into(),
            message: format!("{error:#}"),
            retryable: false,
            details: json!({}),
            outcome_unknown: false,
        },
    }
}
fn filtered_turns(history: &History, filter: &str) -> Vec<usize> {
    let filter = filter.to_lowercase();
    history
        .turns
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            filter.is_empty()
                || format!("{} {} {}", t.time, t.text, t.id)
                    .to_lowercase()
                    .contains(&filter)
        })
        .map(|(index, _)| index)
        .collect()
}

fn ids(history: &History) -> Vec<String> {
    history.turns.iter().map(|t| t.id.clone()).collect()
}
fn parse_turn(value: &Value) -> anyhow::Result<Turn> {
    let mut text = Vec::new();
    for item in value["items"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("userMessage") => {
                let parts: Vec<_> = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                text.push(format!("You: {}", parts.join(" ")));
            }
            Some("agentMessage") => {
                if let Some(s) = item["text"].as_str() {
                    text.push(format!("Agent: {s}"));
                }
            }
            _ => {}
        }
    }
    let time = value["startedAt"]
        .as_i64()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "Time unavailable".into());
    Ok(Turn {
        id: value["id"].as_str().context("turn ID missing")?.into(),
        text: if text.is_empty() {
            "(No text summary)".into()
        } else {
            text.join("\n").chars().take(4000).collect()
        },
        time,
        active: value["status"].as_str() == Some("inProgress"),
        legacy_input_turn: value["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i["type"] == "userMessage")),
    })
}
#[cfg(target_os = "linux")]
fn has_writer(path: &Path, own_pid: u32) -> anyhow::Result<bool> {
    let expected = path.canonicalize()?;
    for p in std::fs::read_dir("/proc")? {
        let p = p?;
        if p.file_name().to_string_lossy() == own_pid.to_string() {
            continue;
        }
        if !p
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).ok().as_ref() != Some(&expected) {
                continue;
            }
            let info = std::fs::read_to_string(p.path().join("fdinfo").join(fd.file_name()))?;
            if let Some(flags) = info.lines().find_map(|l| l.strip_prefix("flags:\t")) {
                if u32::from_str_radix(flags.trim(), 8)? & 3 != 0 {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
#[cfg(target_os = "macos")]
fn has_writer(path: &Path, own_pid: u32) -> anyhow::Result<bool> {
    let expected = path.canonicalize()?;
    Ok(cutex::platform::macos_proc::open_writers(&expected)?
        .into_iter()
        .any(|pid| pid != own_pid))
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn has_writer(_: &Path, _: u32) -> anyhow::Result<bool> {
    anyhow::bail!("Bring this session online before reverting history on this platform; isolated writer detection is unavailable")
}

pub(super) fn run(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    events: &mut super::session_tui::ShellEvents,
    id: String,
) -> anyhow::Result<Option<String>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    use ratatui::{
        layout::{Constraint, Layout},
        style::Style,
        text::{Line, Span},
        widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    };
    enum Command {
        Reload,
        Revert(History, String),
    }
    enum Reply {
        Loaded(History),
        Done(String),
        Failed(String),
    }
    let (send, receive) = std::sync::mpsc::channel::<Command>();
    let (reply, results) = std::sync::mpsc::channel::<Reply>();
    let title = id.clone();
    std::thread::spawn(move || {
        let mut editor = match Editor::open(&id) {
            Ok(e) => e,
            Err(e) => {
                let _ = reply.send(Reply::Failed(format!("{e:#}")));
                return;
            }
        };
        let load = |editor: &mut Editor| match editor.read() {
            Ok(h) => Reply::Loaded(h),
            Err(e) => Reply::Failed(format!("{e:#}")),
        };
        if reply.send(load(&mut editor)).is_err() {
            return;
        }
        while let Ok(command) = receive.recv() {
            let value = match command {
                Command::Reload => load(&mut editor),
                Command::Revert(h, keep) => match editor.revert(&h, &keep) {
                    Ok(s) => {
                        // Release the temporary owner before the user can resume.
                        drop(editor);
                        let _ = reply.send(Reply::Done(s));
                        return;
                    }
                    Err(e) => Reply::Failed(format!("{e:#}")),
                },
            };
            if reply.send(value).is_err() {
                return;
            }
        }
    });
    let mut history: Option<History> = None;
    let mut filter = String::new();
    let mut editing = false;
    let mut selected = 0usize;
    let mut confirmation: Option<usize> = None;
    let mut busy = true;
    let mut applying = false;
    let mut message = String::from("Loading history…");
    let mut list_state = ListState::default();
    loop {
        while let Ok(result) = results.try_recv() {
            busy = false;
            applying = false;
            match result {
                Reply::Loaded(h) => {
                    history = Some(h);
                    selected = 0;
                    confirmation = None;
                    message = "Choose the last turn to keep. Newest first.".into();
                }
                Reply::Done(s) => return Ok(Some(s)),
                Reply::Failed(s) => {
                    message = s;
                    confirmation = None;
                }
            }
        }
        let visible = history
            .as_ref()
            .map(|h| filtered_turns(h, &filter))
            .unwrap_or_default();
        selected = selected.min(visible.len().saturating_sub(1));
        list_state.select((!visible.is_empty()).then_some(selected));
        terminal.draw(|frame|{
            let area=frame.area();
            let regions=Layout::vertical([Constraint::Length(2),Constraint::Min(4),Constraint::Length(3),Constraint::Length(1)]).split(area);
            let columns=Layout::horizontal([Constraint::Percentage(52),Constraint::Percentage(48)]).split(regions[1]);
            frame.render_widget(Paragraph::new(format!("Revert history · {title}\nFilter{}: {filter}",if editing{" [typing]"}else{" [/ ]"})).style(Style::default().fg(theme::focus())),regions[0]);
            let items:Vec<ListItem>=visible.iter().map(|i|{
                let h=history.as_ref().unwrap();let t=&h.turns[*i];
                let summary=t.text.lines().next().unwrap_or("");
                ListItem::new(vec![Line::from(vec![Span::styled(format!("{}  ",t.time),Style::default().fg(theme::focus())),Span::raw(format!("Turn {}{}",h.turns.len()-i,if t.active{" · running"}else{""}))]),Line::raw(summary.chars().take(180).collect::<String>())])
            }).collect();
            frame.render_stateful_widget(List::new(items).block(Block::default().borders(Borders::ALL).title(" History · newest → oldest ")).highlight_style(Style::default().bg(theme::selection())).highlight_symbol("› "),columns[0],&mut list_state);
            let details=if let Some(i)=confirmation {
                let h=history.as_ref().unwrap();let t=&h.turns[i];
                format!("Keep through {}\n{}\n\nRemove {i} newer turns.\n{}\n\nThe session ID and Cutex Agent authority stay the same.\nFiles, sent messages, Tasks and Jobs will not be undone.\nA recovery copy is saved before editing.\n\nEnter: Confirm revert\nEsc: Back",t.time,t.text.lines().take(3).collect::<Vec<_>>().join("\n").chars().take(180).collect::<String>(),if h.turns.iter().any(|t|t.active){"The current turn will be stopped."}else{""})
            }else if let Some(i)=visible.get(selected){history.as_ref().unwrap().turns[*i].text.chars().take(4000).collect()}else{"No matching turns".into()};
            frame.render_widget(Paragraph::new(details).wrap(Wrap{trim:false}).block(Block::default().borders(Borders::ALL).title(if confirmation.is_some(){" Confirm revert "}else{" Details "})),columns[1]);
            frame.render_widget(Paragraph::new(&*message).wrap(Wrap{trim:true}).style(Style::default().fg(if busy{theme::accent()}else{theme::text()})),regions[2]);
            frame.render_widget(Paragraph::new(if busy{"Working…"}else if confirmation.is_some(){"Enter Confirm revert · Esc Cancel"}else{"↑/↓ Select · / Filter · Enter Keep through this turn · r Reload · Esc Back"}),regions[3]);
        })?;
        let Some(event) = events.next()? else {
            continue;
        };
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Release
            || (key.code == KeyCode::Enter && key.kind != KeyEventKind::Press)
        {
            continue;
        }
        if busy {
            if key.code == KeyCode::Esc && !applying {
                return Ok(None);
            }
            continue;
        }
        if editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => editing = false,
                KeyCode::Backspace => {
                    filter.pop();
                    selected = 0;
                }
                KeyCode::Char(c) => {
                    filter.push(c);
                    selected = 0;
                }
                _ => {}
            }
            continue;
        }
        if let Some(i) = confirmation {
            match key.code {
                KeyCode::Esc => confirmation = None,
                KeyCode::Enter => {
                    let h = history.as_ref().unwrap();
                    send.send(Command::Revert(h.clone(), h.turns[i].id.clone()))?;
                    busy = true;
                    applying = true;
                    message="Stopping the current turn if needed, saving recovery copy, then reverting…".into();
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('/') => editing = true,
            KeyCode::Char('r') => {
                send.send(Command::Reload)?;
                busy = true;
                message = "Reloading…".into();
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(visible.len().saturating_sub(1)),
            KeyCode::PageDown => selected = (selected + 10).min(visible.len().saturating_sub(1)),
            KeyCode::PageUp => selected = selected.saturating_sub(10),
            KeyCode::Enter => {
                if let Some(i) = visible.get(selected) {
                    if *i == 0 {
                        message = "This is the newest turn; nothing to remove.".into();
                    } else if history.as_ref().unwrap().turns[*i].active {
                        message = "Choose a completed turn to retain.".into();
                    } else {
                        confirmation = Some(*i);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rust <-> adapter handshake of the offline Claude history reader. Runs
    /// when CUTEX_TEST_CLAUDE_PYTHON names a Python with the adapter's
    /// dependencies (e.g. `components/claude-runtime/.venv`); no model or login is used.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn claude_history_reader_serves_the_adapter_state_read_only() {
        let Some(python) = std::env::var_os("CUTEX_TEST_CLAUDE_PYTHON").map(PathBuf::from) else {
            eprintln!("skipped: CUTEX_TEST_CLAUDE_PYTHON is not set");
            return;
        };
        let adapter = Path::new(env!("CARGO_MANIFEST_DIR")).join("components/claude-runtime");
        let root = std::env::temp_dir().join(format!("cutex-claude-reader-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        let session: cutex::runtime::claude::ClaudeSession = serde_json::from_value(json!({
            "deployment": {"python": python, "adapter_source": adapter, "claude_cli": "/claude",
                           "claude_home": "/home", "tui_binary": "/tui"},
            "thread_id": "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11", "state_dir": state, "model": "sonnet"
        }))
        .unwrap();
        // Never started: no state yet, empty pages, typed thread/read error.
        let mut endpoint = claude_history_reader(&session).expect("reader starts");
        let page = endpoint
            .request("thread/turns/list", json!({"threadId": "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11", "limit": 5}))
            .expect("empty page");
        assert_eq!(page["data"], json!([]));
        let missing = endpoint
            .request("thread/read", json!({"threadId": "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11"}))
            .expect_err("no thread yet");
        assert!(missing.to_string().contains("-32004"), "{missing}");
        drop(endpoint);
        // A thread written by the adapter's own store.
        let seeded = std::process::Command::new(&session.deployment.python)
            .args([
                "-c",
                concat!(
                    "import sys; from pathlib import Path; from cute_claude.store import Store; ",
                    "s = Store(Path(sys.argv[1])); s.ensure_thread('8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11', 'sonnet', sys.argv[1], ''); s.close()",
                ),
            ])
            .arg(&state)
            .env("PYTHONPATH", adapter.join("src"))
            .status()
            .expect("seed state");
        assert!(seeded.success());
        let mut endpoint = claude_history_reader(&session).expect("reader starts");
        let thread = endpoint
            .request("thread/read", json!({"threadId": "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11", "includeTurns": true}))
            .expect("thread/read");
        assert_eq!(thread["thread"]["id"], "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11");
        assert_eq!(thread["thread"]["turns"], json!([]));
        let refused = endpoint
            .request("turn/start", json!({"threadId": "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11"}))
            .expect_err("read-only reader");
        assert!(refused.to_string().contains("-32601"), "{refused}");
        drop(endpoint);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn summaries_keep_turn_ids_and_local_time_without_counting_messages_as_turns() {
        let turn=parse_turn(&json!({"id":"turn-one","status":"completed","startedAt":null,"items":[{"type":"userMessage","content":[{"type":"text","text":"问题 中文"}]},{"type":"commandExecution","command":"echo no"},{"type":"agentMessage","text":"Answer"}]})).unwrap();
        assert_eq!(turn.id, "turn-one");
        assert_eq!(turn.time, "Time unavailable");
        assert!(turn.text.contains("问题 中文") && turn.text.contains("Answer"));
        assert!(!turn.text.contains("echo no"));
    }
    #[test]
    fn revert_failures_map_to_stable_management_codes() {
        let mapped = revert_error(failure("turn_not_found", "missing", false));
        assert_eq!((mapped.code.as_str(), mapped.retryable), ("turn_not_found", false));
        let mapped = revert_error(failure("history_changed", "changed", true));
        assert!(mapped.retryable && !mapped.outcome_unknown);
        let unknown = revert_error(
            RevertFailure {
                code: "native_request_failed",
                message: "x".into(),
                retryable: false,
                outcome_unknown: true,
            }
            .into(),
        );
        assert!(unknown.outcome_unknown);
        let generic = revert_error(anyhow::anyhow!("boom"));
        assert_eq!(generic.code, "history_revert_failed");
        assert!(generic.message.contains("boom"));
    }
    /// A Claude `thread/tokenUsage/updated` (reported at turn end) becomes the
    /// session's bootstrap / history `cutexState.latestTokenUsage`; Codex
    /// sessions keep the rollout derivation.
    #[test]
    fn claude_token_usage_event_feeds_latest_token_usage() {
        use cutex::management::v2::claude_token_usage;
        use cutex::management::v2::model::{
            EventCorrelation, EventEnvelope, EventSource, NativeMessage, NativeMessageKind,
        };
        let root = std::env::temp_dir().join(format!("cutex-claude-usage-{}", uuid::Uuid::new_v4()));
        let thread = "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a11";
        let mut claude = cutex::session::model::CutexSessionRecord::new(
            "cutex-claude-1".into(), None, "host-a".into(), "/w".into(), None).unwrap();
        claude.claude_session = Some(serde_json::from_value(json!({
            "deployment": {"python": "/py", "adapter_source": "/a", "claude_cli": "/claude",
                           "claude_home": "/home", "tui_binary": "/tui"},
            "thread_id": thread, "state_dir": "/state", "model": "opus"
        })).unwrap());
        let codex = cutex::session::model::CutexSessionRecord::new(
            "cutex-codex-1".into(), Some("00000000-0000-7000-8000-000000000001".into()),
            "host-a".into(), "/w".into(), None).unwrap();
        let mut store = cutex::session::model::CutexSessionStore::default();
        store.sessions.insert(claude.cutex_session_id.clone(), claude.clone());
        store.sessions.insert(codex.cutex_session_id.clone(), codex.clone());
        let latest = |session: &str, thread: &str| claude_token_usage::latest_at(&root, session, thread);
        let read = |id: &str| claude_latest_in(&store, id, "host-a", latest);

        // No report yet: null (not the Codex rollout derivation).
        assert_eq!(read(&claude.cutex_session_id), Some(Value::Null));
        let usage = json!({
            "total": {"totalTokens": 52000, "inputTokens": 50000, "cachedInputTokens": 40000,
                      "cacheWriteInputTokens": 9000, "outputTokens": 2000, "reasoningOutputTokens": 0},
            "last": {"totalTokens": 48000, "inputTokens": 48000, "cachedInputTokens": 0,
                     "cacheWriteInputTokens": 0, "outputTokens": 0, "reasoningOutputTokens": 0},
            "modelContextWindow": 1000000
        });
        let envelope = EventEnvelope {
            contract_version: cutex::management::v2::model::CONTRACT_VERSION,
            event_id: "e1".into(), cursor: "c1".into(), stream_id: "s".into(), sequence: 9,
            received_at: "2026-10-20T03:04:05Z".into(),
            cutex_session_id: claude.cutex_session_id.clone(), host_id: "host-a".into(),
            source: EventSource::AppServer, sensitivity: "private".into(), schema: None,
            correlation: EventCorrelation::default(),
            native: Some(NativeMessage { kind: NativeMessageKind::Notification, message: json!({
                "method": "thread/tokenUsage/updated",
                "params": {"threadId": thread, "turnId": "turn-7", "tokenUsage": usage}
            })}),
            cutex: None,
        };
        assert!(claude_token_usage::record_event_at(&root, &envelope, thread).unwrap());
        let expected = json!({"threadId": thread, "turnId": "turn-7", "tokenUsage": usage,
                              "updatedAt": "2026-10-20T03:04:05Z"});
        assert_eq!(read(&claude.cutex_session_id), Some(expected.clone()));
        let mut body = json!({"cutexSessionId": claude.cutex_session_id});
        cutex::management::v2::history::attach_cutex_state(
            &mut body, Value::Null, read(&claude.cutex_session_id).unwrap());
        assert_eq!(body["cutexState"]["latestTokenUsage"], expected);
        // Codex: not answered here (falls through to the rollout).
        assert_eq!(read(&codex.cutex_session_id), None);
        // A Claude session of another host: null.
        assert_eq!(claude_latest_in(&store, &claude.cutex_session_id, "host-b", latest), Some(Value::Null));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Claude `turn/plan/updated` (TodoWrite / Task* checklist) becomes the
    /// session's bootstrap / history `cutexState.latestPlan` (it was always
    /// null for Claude sessions); Codex sessions keep the rollout derivation.
    #[test]
    fn claude_plan_event_feeds_latest_plan() {
        use cutex::management::v2::claude_plan;
        use cutex::management::v2::model::{
            EventCorrelation, EventEnvelope, EventSource, NativeMessage, NativeMessageKind,
        };
        let root = std::env::temp_dir().join(format!("cutex-claude-plan-{}", uuid::Uuid::new_v4()));
        let thread = "8c0e4b8e-6f3a-4c1e-9a52-0d9f6e2b7a12";
        let mut claude = cutex::session::model::CutexSessionRecord::new(
            "cutex-claude-2".into(), None, "host-a".into(), "/w".into(), None).unwrap();
        claude.claude_session = Some(serde_json::from_value(json!({
            "deployment": {"python": "/py", "adapter_source": "/a", "claude_cli": "/claude",
                           "claude_home": "/home", "tui_binary": "/tui"},
            "thread_id": thread, "state_dir": "/state", "model": "opus"
        })).unwrap());
        let codex = cutex::session::model::CutexSessionRecord::new(
            "cutex-codex-2".into(), Some("00000000-0000-7000-8000-000000000002".into()),
            "host-a".into(), "/w".into(), None).unwrap();
        let mut store = cutex::session::model::CutexSessionStore::default();
        store.sessions.insert(claude.cutex_session_id.clone(), claude.clone());
        store.sessions.insert(codex.cutex_session_id.clone(), codex.clone());
        let latest = |session: &str, thread: &str| claude_plan::latest_at(&root, session, thread);
        let read = |id: &str| claude_latest_in(&store, id, "host-a", latest);

        assert_eq!(read(&claude.cutex_session_id), Some(Value::Null));
        let plan = json!([{"step": "Read the code", "status": "completed"},
                          {"step": "Fix the bug", "status": "inProgress"}]);
        let envelope = EventEnvelope {
            contract_version: cutex::management::v2::model::CONTRACT_VERSION,
            event_id: "e1".into(), cursor: "c1".into(), stream_id: "s".into(), sequence: 9,
            received_at: "2026-10-21T03:04:05Z".into(),
            cutex_session_id: claude.cutex_session_id.clone(), host_id: "host-a".into(),
            source: EventSource::AppServer, sensitivity: "private".into(), schema: None,
            correlation: EventCorrelation::default(),
            native: Some(NativeMessage { kind: NativeMessageKind::Notification, message: json!({
                "method": "turn/plan/updated",
                "params": {"threadId": thread, "turnId": "turn-3", "explanation": null, "plan": plan}
            })}),
            cutex: None,
        };
        assert!(claude_plan::record_event_at(&root, &envelope, thread).unwrap());
        let expected = json!({"threadId": thread, "turnId": "turn-3", "explanation": null, "plan": plan,
                              "updatedAt": "2026-10-21T03:04:05Z"});
        assert_eq!(read(&claude.cutex_session_id), Some(expected.clone()));
        let mut body = json!({"cutexSessionId": claude.cutex_session_id});
        cutex::management::v2::history::attach_cutex_state(
            &mut body, read(&claude.cutex_session_id).unwrap(), Value::Null);
        assert_eq!(body["cutexState"]["latestPlan"], expected);
        assert_eq!(read(&codex.cutex_session_id), None);
        assert_eq!(claude_latest_in(&store, &claude.cutex_session_id, "host-b", latest), Some(Value::Null));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// r265: a DSH session (no rollout either) serves the checklist its
    /// adapter sent (`todo_write` as `turn/plan/updated`) for its bound
    /// thread, so it survives a desktop reconnect; a revert of that turn drops it.
    #[test]
    fn dsh_plan_event_feeds_latest_plan() {
        use cutex::management::v2::claude_plan;
        use cutex::management::v2::model::{
            EventCorrelation, EventEnvelope, EventSource, NativeMessage, NativeMessageKind,
        };
        let root = std::env::temp_dir().join(format!("cutex-dsh-plan-{}", uuid::Uuid::new_v4()));
        let thread = "dsh-thread-1";
        let mut dsh = cutex::session::model::CutexSessionRecord::new(
            "cutex-dsh-2".into(), None, "host-a".into(), "/w".into(), None).unwrap();
        dsh.dsh_session = Some(serde_json::from_value(json!({
            "thread_id": thread, "state_dir": "/s", "node": "/n", "adapter": "/a", "dsh_cli": "/d",
            "patch": "/s/profile.patch.yml", "install_root": "/i", "native_session_id": "n", "spike": false,
        })).unwrap());
        let mut store = cutex::session::model::CutexSessionStore::default();
        store.sessions.insert(dsh.cutex_session_id.clone(), dsh.clone());
        let latest = |session: &str, thread: &str| claude_plan::latest_at(&root, session, thread);
        let read = |id: &str| claude_latest_in(&store, id, "host-a", latest);
        assert_eq!(read(&dsh.cutex_session_id), Some(Value::Null));
        let plan = json!([{"step": "Inspect", "status": "completed"}, {"step": "Write", "status": "pending"}]);
        let envelope = |thread_id: &str| EventEnvelope {
            contract_version: cutex::management::v2::model::CONTRACT_VERSION,
            event_id: "e1".into(), cursor: "c1".into(), stream_id: "s".into(), sequence: 3,
            received_at: "2026-10-27T03:04:05Z".into(),
            cutex_session_id: dsh.cutex_session_id.clone(), host_id: "host-a".into(),
            source: EventSource::AppServer, sensitivity: "private".into(), schema: None,
            correlation: EventCorrelation::default(),
            native: Some(NativeMessage { kind: NativeMessageKind::Notification, message: json!({
                "method": "turn/plan/updated",
                "params": {"threadId": thread_id, "turnId": "n:turn:2", "explanation": null, "plan": plan}
            })}),
            cutex: None,
        };
        assert!(!claude_plan::record_event_at(&root, &envelope("other"), thread).unwrap());
        assert!(claude_plan::record_event_at(&root, &envelope(thread), thread).unwrap());
        assert_eq!(read(&dsh.cutex_session_id), Some(json!({"threadId": thread, "turnId": "n:turn:2",
            "explanation": null, "plan": plan, "updatedAt": "2026-10-27T03:04:05Z"})));
        claude_plan::forget_removed_turns_at(&root, &dsh.cutex_session_id, &["n:turn:2"]).unwrap();
        assert_eq!(read(&dsh.cutex_session_id), Some(Value::Null));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn filtering_preserves_newest_first_and_real_removal_boundary() {
        let h = History {
            thread: "t".into(),
            path: "/tmp/history".into(),
            paginated: false,
            turns: (0..5)
                .map(|i| Turn {
                    id: i.to_string(),
                    text: if i % 2 == 0 {
                        "中文 MATCH".into()
                    } else {
                        "hidden".into()
                    },
                    time: "Time unavailable".into(),
                    active: false,
                    legacy_input_turn: true,
                })
                .collect(),
        };
        assert_eq!(filtered_turns(&h, "match"), vec![0, 2, 4]);
        assert_eq!(filtered_turns(&h, "中文"), vec![0, 2, 4]);
        assert_eq!(filtered_turns(&h, "absent"), Vec::<usize>::new());
        assert_eq!(filtered_turns(&h, "match")[1], 2); // remove two turns, including the hidden one
    }
    #[test]
    #[ignore = "isolated native mock-model fixture required"]
    #[cfg(target_os = "linux")]
    fn actual_native_revert_preserves_identity_and_recovery_copy() {
        let fixture = PathBuf::from(std::env::var("CUTEX_HISTORY_FIXTURE").unwrap());
        assert!(fixture.join(".cutex-test-private-home").is_file());
        let data: Value =
            serde_json::from_slice(&std::fs::read(fixture.join("fixture.json")).unwrap()).unwrap();
        let mut options = AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
            socket_path: data["socket"].as_str().unwrap().into(),
        });
        options.request_timeout = Duration::from_secs(120);
        let mut editor = if std::env::var_os("CUTEX_HISTORY_OFFLINE").is_some() {
            Editor::open(data["thread"].as_str().unwrap()).unwrap()
        } else {
            Editor {
                connection: Connection::Live(AppServerClient::connect(options).unwrap()),
                thread: data["thread"].as_str().unwrap().into(),
                home: fixture.join(".cutex/codex-home"),
            }
        };
        let history = editor.read().unwrap();
        assert_eq!(history.turns.len(), 3);
        assert!(history.turns[0].text.contains("Third"));
        assert!(history.turns[2].text.contains("First"));
        let middle = history.turns[1].id.clone();
        editor.revert(&history, &middle).unwrap();
        assert!(editor.revert(&history, &middle).is_err());
        let history = editor.read().unwrap();
        let keep = history.turns[1].id.clone();
        let outcome = editor.revert(&history, &keep).unwrap();
        let after = editor.read().unwrap();
        assert_eq!(
            cutex::launch::native_history::current(&editor.home, &editor.thread).unwrap(),
            after.path.canonicalize().unwrap()
        );
        assert_eq!(after.thread, history.thread);
        assert_eq!(after.turns.len(), 1);
        assert_eq!(after.turns[0].id, keep);
        assert!(outcome.contains("Removed 1 newer turn(s)"));
        let receipts: Vec<_> = std::fs::read_dir(fixture.join(".cutex/runtime/history-reverts"))
            .unwrap()
            .collect();
        assert_eq!(receipts.len(), 2);
        let root = receipts[0].as_ref().unwrap().path();
        assert!(std::fs::metadata(root.join("before.jsonl")).unwrap().len() > 0);
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(root.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["status"], "complete");
        assert!(editor.revert(&history, &keep).is_err());
        let mut source_counts = receipts
            .iter()
            .map(|entry| {
                let receipt: Value = serde_json::from_slice(
                    &std::fs::read(entry.as_ref().unwrap().path().join("receipt.json")).unwrap(),
                )
                .unwrap();
                receipt["sources"].as_array().unwrap().len()
            })
            .collect::<Vec<_>>();
        source_counts.sort();
        assert_eq!(
            source_counts,
            if history.paginated {
                vec![1, 2]
            } else {
                vec![1, 1]
            }
        );
        println!("{}", outcome);
    }
}
