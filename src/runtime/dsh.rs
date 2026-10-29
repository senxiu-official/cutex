//! Managed DSH ACP runtime. The debug-only spike entry remains isolated.
mod credential;
mod product;
mod windows_shell;
pub use product::{create_profile, create_profile_variant, create_profile_variant_with, DshCreateOverrides, deployment_path, native_schema, require_control, require_session, permission_knobs, DshDeployment};
/// The native schema of an ACP-adapter session (DSH or Kimi Code).
pub fn acp_native_schema(session: &DshSession) -> serde_json::Value {
    match session.flavor {
        AcpFlavor::Dsh => native_schema(),
        AcpFlavor::Kimi => crate::runtime::kimi::native_schema(),
    }
}
/// The revert swap intent a crash may leave in a DSH state directory
/// (`cli_app::dsh_structure`). A start never reads half-swapped files.
pub const REVERT_INTENT_FILE: &str = "revert-intent.json";

/// The provider key this agent's next start would use (C3's resolver: stored
/// profile key or the bound env variable), for redaction only; `None` when
/// unresolvable. Never logged.
pub fn credential_for_redaction(record: &CutexSessionRecord) -> Option<String> {
    let session = record.dsh_session.as_ref()?;
    if session.flavor == AcpFlavor::Kimi {
        return crate::runtime::kimi::credential::for_redaction(record);
    }
    if session.spike {
        return None;
    }
    credential::secret_env(record, session).ok().map(|(_, value)| value).filter(|value| !value.trim().is_empty())
}

/// Earlier keys of the agent's bound profile (replaced or removed; kept in
/// its key file for redaction only): native logs written while one of them
/// was current must stay redacted when projected again after a rotation.
pub fn retired_credentials_for_redaction(record: &CutexSessionRecord) -> Vec<String> {
    match (&record.dsh_session, &record.profile_binding) {
        (Some(session), Some(binding)) if !session.spike => crate::profiles::stored_keys::retired_values(&binding.profile_id),
        _ => Vec::new(),
    }
}

/// Everything redaction of this agent's output must cover: the current key
/// and every retired one.
pub fn credentials_for_redaction(record: &CutexSessionRecord) -> Vec<String> {
    let mut values = retired_credentials_for_redaction(record);
    values.extend(credential_for_redaction(record));
    values
}
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::session::model::{
    CutexAppServerRuntimeBinding, CutexAppServerTransport, CutexSessionRecord,
    CutexSessionRuntimeBackend,
};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct LaunchScopeGuard {
    id: String,
    armed: bool,
}
impl Drop for LaunchScopeGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = crate::runtime::process_scope::terminate_managed_agent_scope(&self.id, true);
        }
    }
}

/// Which ACP runtime a cutex ACP-adapter session drives. The adapter protocol
/// (launch file, `dsh/*` control methods, history reader, session operations)
/// is shared; DSH was its first user, Kimi Code (r266) the second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpFlavor {
    #[default]
    Dsh,
    Kimi,
}
impl AcpFlavor {
    pub fn is_dsh(&self) -> bool { *self == AcpFlavor::Dsh }
    /// The runtime name cutex shows (`agentParty.runtime`, summaries, Bus).
    pub fn runtime_name(self) -> &'static str { match self { AcpFlavor::Dsh => "dsh", AcpFlavor::Kimi => "kimi" } }
    /// `runtime/<root>/<cutex id>` holds a session's private state.
    pub fn state_root(self) -> &'static str { self.runtime_name() }
    pub fn backend(self) -> CutexSessionRuntimeBackend { match self { AcpFlavor::Dsh => CutexSessionRuntimeBackend::Dsh, AcpFlavor::Kimi => CutexSessionRuntimeBackend::Kimi } }
    pub fn display(self) -> &'static str { match self { AcpFlavor::Dsh => "DSH", AcpFlavor::Kimi => "Kimi Code" } }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DshSession {
    /// Absent (DSH) in every record written before r266.
    #[serde(default, skip_serializing_if = "AcpFlavor::is_dsh")]
    pub flavor: AcpFlavor,
    /// Kimi Code launch options (flavor `kimi` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kimi: Option<crate::runtime::kimi::KimiOptions>,
    pub thread_id: String,
    pub state_dir: PathBuf,
    pub node: PathBuf,
    pub adapter: PathBuf,
    pub dsh_cli: PathBuf,
    pub patch: PathBuf,
    pub install_root: PathBuf,
    /// The app-server conversation id above differs from the native ACP id.
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub deployment_version: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub api_key_env: String,
    /// Old records were explicitly private debug spikes, never product launches.
    #[serde(default = "is_spike_default")]
    pub spike: bool,
}
fn is_spike_default() -> bool { true }

fn add_debug_home_boundary(env: &mut BTreeMap<String, String>, mcp_env: &mut Vec<Value>, home: &std::path::Path, debug_build: bool) {
    // Release cutex-mcp deliberately refuses CUTEX_TEST_PRIVATE_HOME. HOME /
    // USERPROFILE and the explicit scoped Bus credentials are ordinary runtime
    // configuration; only a debug child needs the marked test-home override.
    if debug_build {
        env.insert("CUTEX_TEST_PRIVATE_HOME".into(), home.to_string_lossy().into_owned());
    }
    // cutex-mcp is cutex's own process, not DSH's: like the cute-codex and
    // Claude runtimes it runs in the cutex user's home (and instance), so the
    // tools that read local cutex state (alarm_set's creator mapping) see the
    // agent. Only DSH itself keeps the isolated home.
    let daemon = |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
    for name in ["HOME", "USERPROFILE", "CUTEX_HOME"] {
        if let Some(value) = daemon(name) {
            mcp_env.retain(|entry| entry["name"] != name);
            mcp_env.push(json!({"name": name, "value": value}));
        }
    }
    if debug_build {
        for name in ["CUTEX_TEST_PRIVATE_HOME", crate::config::paths::FORBID_REAL_HOME_ENV] {
            if let Some(value) = daemon(name) {
                mcp_env.push(json!({"name": name, "value": value}));
            }
        }
    }
}

fn startup_wait() -> anyhow::Result<Duration> {
    if cfg!(debug_assertions) {
        if let Ok(value) = std::env::var("CUTEX_DSH_TEST_STARTUP_DEADLINE_SECONDS") {
            require_private_test()?;
            let seconds: u64 = value.parse().context("invalid private DSH startup deadline")?;
            ensure!((1..=180).contains(&seconds), "private DSH startup deadline must be 1..180 seconds");
            return Ok(Duration::from_secs(seconds));
        }
    }
    Ok(Duration::from_secs(150))
}

#[cfg(windows)]
fn create_reclaimed_job(id: &str) -> anyhow::Result<std::os::windows::io::OwnedHandle> {
    // An emptied Job can retain its name briefly while Windows finishes
    // closing the terminated root's handles. Never join a populated Job.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match crate::platform::windows_job::create_agent_job(
            &crate::runtime::process_scope::managed_agent_job_name(id),
        ) {
            Ok(job) => return Ok(job),
            Err(error) => {
                if Instant::now() >= deadline || !crate::runtime::process_scope::managed_agent_scope_is_absent(id)? {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Create {
    pub name: String,
    pub cwd: PathBuf,
    pub node: PathBuf,
    pub adapter: PathBuf,
    pub dsh_cli: PathBuf,
    pub patch: PathBuf,
    pub install_root: PathBuf,
}

/// This surface cannot run in release builds or against an ordinary home.
pub fn require_spike() -> anyhow::Result<()> {
    ensure!(
        cfg!(windows),
        "DSH spike controller currently requires Windows process containment"
    );
    require_private_test()
}

fn require_private_test() -> anyhow::Result<()> {
    ensure!(cfg!(debug_assertions), "DSH test overrides are debug-only");
    ensure!(
        std::env::var("CUTEX_DSH_SPIKE").as_deref() == Ok("1"),
        "DSH spike disabled"
    );
    let home = PathBuf::from(
        std::env::var("CUTEX_TEST_PRIVATE_HOME").context("private test home required")?,
    );
    ensure!(
        home.is_absolute() && home.join(".cutex-test-private-home").is_file(),
        "marked private home required"
    );
    ensure!(
        crate::config::paths::config_dir()?.starts_with(&home),
        "DSH spike refuses non-private config root"
    );
    Ok(())
}

pub fn create(request: Create) -> anyhow::Result<CutexSessionRecord> {
    require_spike()?;
    ensure!(
        !request.name.trim().is_empty() && request.name.len() <= 128,
        "name required"
    );
    ensure!(
        request.cwd.is_absolute() && request.cwd.is_dir(),
        "absolute working directory required"
    );
    for path in [
        &request.node,
        &request.adapter,
        &request.dsh_cli,
        &request.patch,
    ] {
        ensure!(
            path.is_absolute() && path.is_file(),
            "absolute runtime file missing"
        );
    }
    ensure!(
        request.install_root.is_absolute() && request.install_root.is_dir(),
        "absolute install root required"
    );
    let id = format!("cutex.{}", uuid::Uuid::new_v4());
    let state_dir = crate::config::paths::runtime_dir()?.join("dsh").join(&id);
    std::fs::create_dir_all(&state_dir)?;
    let mut record = CutexSessionRecord::new(
        id.clone(),
        None,
        crate::platform::host::current_host_name(),
        request.cwd.to_string_lossy().into_owned(),
        None,
    )?;
    record.runtime_backend = CutexSessionRuntimeBackend::Dsh;
    record.formal_agent_name = Some(request.name.clone());
    record.thread_name = Some(request.name);
    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    record.agent_enabled = true;
    record.exposed_to_backend = true;
    record.dsh_session = Some(DshSession {
        flavor: Default::default(), kimi: None,
        thread_id: String::new(),
        state_dir,
        node: request.node,
        adapter: request.adapter,
        dsh_cli: request.dsh_cli,
        patch: request.patch,
        install_root: request.install_root,
        native_session_id: None, deployment_version: String::new(), model: String::new(),
        reasoning_effort: None, api_key_env: String::new(), spike: true,
    });
    crate::session::store::with_locked_session_rows(
        &crate::session::store::cutex_sessions_path()?,
        |rows| {
            ensure!(
                !rows.sessions().any(|(_, other)| !other.is_retired()
                    && other.formal_agent_name == record.formal_agent_name),
                "DSH agent name already exists"
            );
            rows.insert_session(id, record.clone());
            rows.commit().map(drop)
        },
    )?;
    start(&record.cutex_session_id)
}

pub fn start(id: &str) -> anyhow::Result<CutexSessionRecord> {
    let store = crate::session::store::load_cutex_session_store()?;
    let record = store
        .sessions
        .get(id)
        .context("DSH session missing")?
        .clone();
    let session = record.dsh_session.as_ref().context("not a DSH session")?;
    let flavor = session.flavor;
    match flavor {
        AcpFlavor::Dsh => require_session(&record)?,
        AcpFlavor::Kimi => crate::runtime::kimi::require_session(&record)?,
    }
    let startup_wait = startup_wait()?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(session.state_dir.join("controller.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock_file).context("DSH lifecycle busy")?;
    ensure!(
        !session.state_dir.join(REVERT_INTENT_FILE).exists(),
        "a DSH history revert of this agent is unfinished; start it through cutex/runtime/online, which finishes the swap first"
    );
    let latest = crate::session::store::load_cutex_session_store()?;
    ensure!(
        latest.sessions.get(id) == Some(&record),
        "DSH record changed before lifecycle lock; retry after resync"
    );
    let kimi = match flavor {
        AcpFlavor::Dsh => None,
        AcpFlavor::Kimi => Some(crate::runtime::kimi::write_home(&record)?),
    };
    let instruction_sources = match &kimi {
        None => product::write_patch(&record)?,
        Some(kimi) => kimi.instruction_sources.clone(),
    };
    ensure!(
        record
            .app_server_runtime
            .as_ref()
            .is_none_or(|b| !crate::platform::process::process_is_running(b.pid)),
        "DSH runtime already running"
    );
    // A daemon may have died after spawn but before its binding CAS. The
    // session-owned scope still identifies that exact orphan occurrence.
    let stopped = crate::runtime::process_scope::terminate_managed_agent_scope(id, true)?;
    ensure!(stopped.stopped, "DSH previous process scope did not stop");
    ensure!(crate::runtime::process_scope::managed_agent_scope_is_absent(id)?, "DSH previous process scope is still populated");
    let generation = record
        .runtime_generation
        .checked_add(1)
        .context("generation overflow")?;
    let runtime_id = format!("{}.{}", flavor.runtime_name(), uuid::Uuid::new_v4());
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let home = session.state_dir.join("home");
    std::fs::create_dir_all(&home)?;
    if cfg!(debug_assertions) {
        std::fs::write(home.join(".cutex-test-private-home"), format!("{} isolated child\n", flavor.display()))?;
    }
    let mut env = BTreeMap::<String, String>::new();
    for key in ["PATH", "Path", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    for (key, value) in [
        ("HOME", home.clone()),
        ("USERPROFILE", home.clone()),
        ("TEMP", home.clone()),
        ("TMP", home.clone()),
        ("APPDATA", home.join("appdata")),
        ("LOCALAPPDATA", home.join("local")),
    ] {
        env.insert(key.into(), value.to_string_lossy().into_owned());
    }
    let sessions_root = match &kimi {
        None => {
            for (key, value) in [
                ("DSH_HOME", home.join("dsh")),
                ("DSH_SPIKE_INSTALL_ROOT", session.install_root.clone()),
                ("DSH_SPIKE_SESSIONS_ROOT", home.join("sessions")),
                ("DSH_INSTALL_ROOT", session.install_root.clone()),
                ("DSH_SESSION_LOG_ROOT", home.join("sessions")),
            ] {
                env.insert(key.into(), value.to_string_lossy().into_owned());
            }
            env.insert("DSH_TELEMETRY_DISABLED".into(), "1".into());
            home.join("sessions")
        }
        Some(kimi) => {
            // Kimi Code: a private KIMI_CODE_HOME, XDG dirs and temp dir per
            // agent, and its background features off (Phase-0 facts).
            let tmp = home.join("tmp");
            std::fs::create_dir_all(&tmp)?;
            for (key, value) in [
                ("KIMI_CODE_HOME", kimi.kimi_home.clone()),
                ("XDG_CONFIG_HOME", home.join(".config")),
                ("XDG_DATA_HOME", home.join(".local/share")),
                ("XDG_CACHE_HOME", home.join(".cache")),
                ("XDG_STATE_HOME", home.join(".local/state")),
                ("TMPDIR", tmp.clone()),
                ("TEMP", tmp.clone()),
                ("TMP", tmp),
                ("KIMI_INSTALL_ROOT", session.install_root.clone()),
            ] {
                env.insert(key.into(), value.to_string_lossy().into_owned());
            }
            for (key, value) in crate::runtime::kimi::KIMI_SWITCHES {
                env.insert((*key).into(), (*value).into());
            }
            crate::runtime::kimi::sessions_root(session)
        }
    };
    // r265 (C3): stored profile key or explicit env variable, per the frozen binding.
    let secret = match &kimi {
        Some(kimi) => kimi.secret_env.clone(),
        None if session.spike => None,
        None => Some(credential::secret_env(&record, session)?),
    };
    // profile-relay-runtimes (B4): the profile's DSH address, relays included.
    if kimi.is_none() && !session.spike {
        env.extend(credential::endpoint_env(&record)?);
    }
    let config = crate::config::store::load_codez_config_checked()?;
    let bus_token = crate::agent_bus::agent_auth::launch_credential(&config, &runtime_id, id)
        .context("private Bus token required")?;
    let bus_port = crate::agent_bus::service::agent_bus_port(&config);
    ensure!(
        !session.spike || ![24270, 24260, 25370, 25360].contains(&bus_port),
        "DSH spike refuses live Bus port"
    );
    let mcp = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "cutex-mcp.exe"
    } else {
        "cutex-mcp"
    });
    ensure!(mcp.is_file(), "cutex-mcp must be installed beside cutex");
    let mut mcp_env = vec![
        json!({"name":"CUTEX_AGENT_ID","value":runtime_id}),
        json!({"name":"CUTEX_RUNTIME_GENERATION","value":generation.to_string()}),
        json!({"name":"CUTEX_AGENT_BUS_URL","value":format!("http://127.0.0.1:{bus_port}/")}),
        json!({"name":"CUTEX_AGENT_BUS_TOKEN","value":bus_token}),
    ];
    add_debug_home_boundary(&mut env, &mut mcp_env, &home, cfg!(debug_assertions));
    let ready_file = session.state_dir.join("ready.json");
    if ready_file.exists() {
        std::fs::remove_file(&ready_file)?;
    }
    let launch_file = session.state_dir.join("launch.json");
    let endpoint_dir = crate::config::paths::runtime_dir()?.join("app-server").join(format!("dsh-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
    std::fs::create_dir_all(&endpoint_dir)?;
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(&endpoint_dir).context("secure DSH endpoint directory")?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&endpoint_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut arguments = match flavor {
        AcpFlavor::Dsh => vec![session.dsh_cli.to_string_lossy().into_owned(), "--profile".into(), "acp".into(), "--patch".into(), session.patch.to_string_lossy().into_owned()],
        AcpFlavor::Kimi => vec![session.dsh_cli.to_string_lossy().into_owned(), "acp".into()],
    };
    // Explicit offline fixtures only: release builds never honor this overlay.
    if flavor == AcpFlavor::Dsh && !session.spike && cfg!(debug_assertions) {
        if let Ok(overlay) = std::env::var("CUTEX_DSH_TEST_PATCH") {
            require_private_test()?;
            let private = PathBuf::from(std::env::var("CUTEX_TEST_PRIVATE_HOME")?);
            let path = PathBuf::from(overlay);
            ensure!(path.is_file() && path.canonicalize()?.starts_with(private.canonicalize()?), "test patch must stay inside private home");
            arguments.extend(["--patch".into(), path.to_string_lossy().into_owned()]);
        }
    }
    let mut launch = json!({
        "command":session.node,"args":arguments,"cwd":record.cwd,"env":env,"database":session.state_dir.join("timeline.sqlite"),
        "mcpServers":[{"name":"cutex","command":mcp,"args":[],"env":mcp_env}],
        "readyFile":ready_file,"token":token,"runtimeId":runtime_id,"sessionsRoot":sessions_root
    });
    if let Some(kimi) = &kimi {
        launch["threadId"] = json!(session.thread_id);
        launch["nativeSessionId"] = json!(session.native_session_id);
        launch["model"] = json!(kimi.model);
        launch["reasoningEffort"] = json!(kimi.reasoning_effort);
        launch["kimiHome"] = json!(kimi.kimi_home);
        launch["mode"] = json!(kimi.mode);
        launch["approvalPolicy"] = json!(kimi.approval_policy);
        launch["imageModels"] = json!(kimi.image_models);
        if kimi.read_only {
            launch["sandboxReadOnly"] = json!(true);
        }
        // Kimi accounts: where the adapter re-reads the OAuth tokens and the
        // rotated values' fingerprints for redaction (paths, never values).
        if let Some(paths) = kimi.redaction.as_ref().filter(|paths| !paths.is_empty()) {
            launch["redaction"] = serde_json::to_value(paths)?;
        }
        if let Some((name, _)) = &secret {
            launch["secretEnvMappings"] = json!([{"source":name,"target":crate::runtime::kimi::KIMI_KEY_ENV}]);
        }
        launch["instructionSources"] = json!(instruction_sources);
    } else if !session.spike {
        launch["threadId"] = json!(session.thread_id);
        launch["nativeSessionId"] = json!(session.native_session_id);
        launch["model"] = json!(session.model);
        launch["reasoningEffort"] = json!(session.reasoning_effort);
        launch["secretEnvMappings"] = json!([{"source":session.api_key_env,"target":"DEEPSEEK_API_KEY"}]);
        launch["instructionSources"] = json!(instruction_sources);
    }
    #[cfg(target_os = "linux")] {
        let socket = endpoint_dir.join("s");
        use std::os::unix::ffi::OsStrExt;
        ensure!(socket.as_os_str().as_bytes().len() < 104, "DSH Unix socket path too long; use a shorter private home");
        launch["socketPath"] = json!(socket);
    }
    write_private_pretty_json_atomic(
        &launch_file,
        &launch,
        "DSH spike launch",
    )?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(session.state_dir.join("adapter.log"))?;
    let mut command = Command::new(&session.node);
    // The Windows detached-spawn helper intentionally ignores env_clear.
    // Explicit removals are represented in Command::get_envs on every host.
    for (key, _) in std::env::vars_os() {
        command.env_remove(key);
    }
    command
        .arg(&session.adapter)
        .envs(&env)
        .env(if flavor == AcpFlavor::Kimi { "KIMI_CUTEX_CONFIG" } else { "DSH_CUTEX_CONFIG" }, &launch_file)
        .current_dir(&record.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    if let Some((name, value)) = &secret { command.env(name, value); }
    // Earlier keys of the profile, for the adapter's redaction only (its DSH
    // child gets an explicit environment without them; never in launch.json).
    let retired = retired_credentials_for_redaction(&record);
    if !retired.is_empty() { command.env("DSH_CUTEX_REDACT", serde_json::to_string(&retired)?); }
    #[cfg(windows)]
    let mut child = {
        let job = create_reclaimed_job(id)?;
        crate::platform::windows_spawn::spawn_detached_in_job(&command, &log, &job)?
    };
    #[cfg(target_os = "linux")]
    let mut child = {
        use crate::runtime::process_scope::{managed_agent_launch_plan, ManagedAgentProcessIsolation};
        let mut launch = crate::launch::command::LaunchCommand::new(session.node.to_string_lossy().into_owned()).arg(session.adapter.to_string_lossy().into_owned());
        for (key, value) in command.get_envs() {
            launch = match value { Some(value) => launch.env(key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()), None => launch.env_remove(key.to_string_lossy().into_owned()) };
        }
        ensure!(matches!(managed_agent_launch_plan(id, launch.clone()).isolation, ManagedAgentProcessIsolation::SystemdScope { .. }), "DSH requires a usable systemd user scope on Linux");
        crate::runtime::lifecycle::spawn_managed_agent_runtime_launch(id, &launch, &record.cwd, &session.state_dir.join("adapter.log"))?
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let mut child = command.spawn()?;
    let mut scope_guard = LaunchScopeGuard {
        id: id.into(),
        armed: true,
    };
    let deadline = Instant::now() + startup_wait;
    let ready: Value = loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("DSH adapter exited: {status}; see isolated adapter.log");
        }
        if let Ok(bytes) = std::fs::read(&ready_file) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                break value;
            }
        }
        if Instant::now() >= deadline {
            let _ = crate::runtime::process_scope::terminate_managed_agent_scope(id, true);
            #[cfg(not(windows))]
            let _ = child.kill();
            anyhow::bail!("DSH ready timeout");
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let facade = ready["threadId"]
        .as_str()
        .context("DSH app-server thread id missing")?;
    let native = ready["nativeSessionId"].as_str().unwrap_or(facade);
    ensure!(
        ready["pid"].as_u64() == Some(child.id() as u64) && ready["runtimeId"] == runtime_id,
        "DSH ready occurrence mismatch"
    );
    ensure!(
        (session.thread_id.is_empty() || session.thread_id == facade) && session.native_session_id.as_deref().is_none_or(|saved| saved == native),
        "DSH app-server/native identity changed"
    );
    let endpoint = ready["endpoint"].as_str().context("DSH endpoint missing")?;
    ensure!(
        endpoint.starts_with("ws://127.0.0.1:") || (cfg!(target_os = "linux") && endpoint.starts_with("unix://")),
        "DSH endpoint must be private loopback"
    );
    // Ephemeral connection capability follows the existing managed endpoint
    // layout. Durable native/projection logs stay outside its cleanup scope.
    crate::app_server::runtime::write_private_file(
        &endpoint_dir.join("rpc-token"),
        token.as_bytes(),
    )?;
    let started_at = crate::platform::process::process_started_at(child.id())?.to_rfc3339();
    let binding = CutexAppServerRuntimeBinding {
        transport: if endpoint.starts_with("unix://") { CutexAppServerTransport::UnixSocket } else { CutexAppServerTransport::LoopbackWebSocket },
        endpoint: endpoint.into(),
        pid: child.id(),
        runtime_dir: endpoint_dir.to_string_lossy().into_owned(),
        launched_profile: None,
        launched_profile_id: None,
        launch_profile_source: None,
        auth_token_path: Some(
            endpoint_dir
                .join("rpc-token")
                .to_string_lossy()
                .into_owned(),
        ),
        diagnostic_journal_path: session
            .state_dir
            .join("diagnostics.jsonl")
            .to_string_lossy()
            .into_owned(),
        schema_version: match flavor { AcpFlavor::Dsh => "dsh-acp-v1".into(), AcpFlavor::Kimi => "kimi-acp-v1".into() },
        schema_sha256: match flavor {
            AcpFlavor::Dsh => format!("{:x}", sha2::Sha256::digest(include_bytes!("dsh-contract.json"))),
            AcpFlavor::Kimi => crate::runtime::kimi::contract_sha256(),
        },
        started_at,
    };
    crate::session::store::with_locked_session_rows(
        &crate::session::store::cutex_sessions_path()?,
        |rows| {
            let current = rows.session_mut(id).context("DSH session disappeared")?;
            ensure!(
                current.runtime_generation == record.runtime_generation
                    && current.dsh_session == record.dsh_session,
                "DSH lifecycle conflict"
            );
            current.dsh_session.as_mut().unwrap().thread_id = facade.into();
            current.dsh_session.as_mut().unwrap().native_session_id = Some(native.into());
            current.runtime_generation = generation;
            current.current_runtime_agent_id = Some(runtime_id);
            current.runtime_pid = Some(binding.pid);
            current.app_server_runtime = Some(binding);
            rows.commit().map(drop)
        },
    )?;
    scope_guard.armed = false;
    // The adapter is this process's child (a systemd scope execs in place):
    // reap it when it exits, so a stopped adapter never lingers as a zombie
    // that still answers `kill(pid, 0)`.
    #[cfg(unix)]
    std::thread::spawn(move || { let _ = child.wait(); });
    crate::session::store::load_cutex_session_store()?
        .sessions
        .get(id)
        .cloned()
        .context("DSH record disappeared")
}

pub fn close(id: &str, expected_generation: u64) -> anyhow::Result<()> {
    let store = crate::session::store::load_cutex_session_store()?;
    let record = store.sessions.get(id).context("DSH session missing")?;
    match record.dsh_session.as_ref().map(|session| session.flavor) {
        Some(AcpFlavor::Kimi) => crate::runtime::kimi::require_control(record)?,
        _ => require_control(record)?,
    }
    ensure!(
        record.dsh_session.is_some() && record.runtime_generation == expected_generation,
        "DSH generation conflict"
    );
    let session = record.dsh_session.as_ref().unwrap();
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(session.state_dir.join("controller.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock_file).context("DSH lifecycle busy")?;
    ensure!(
        crate::session::store::load_cutex_session_store()?
            .sessions
            .get(id)
            == Some(record),
        "DSH close record changed"
    );
    crate::session::runtime_stop::RequestedStop::run(id, expected_generation, || {
    if let Some(binding) = &record.app_server_runtime {
        if crate::platform::process::process_is_running(binding.pid) {
            let graceful = (|| -> anyhow::Result<()> {
                let mut options = crate::app_server::client::AppServerClientOptions::new(
                    crate::app_server::runtime::endpoint_from_runtime_binding(binding)?,
                );
                options.request_timeout = Duration::from_secs(2);
                options.connect_timeout = Duration::from_secs(2);
                let client = crate::app_server::client::AppServerClient::connect(options)?;
                client.handle().request("dsh/server/shutdown", json!({}))?;
                Ok(())
            })();
            if graceful.is_ok() {
                let deadline = Instant::now() + Duration::from_secs(2);
                while crate::platform::process::process_is_running(binding.pid) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
        }
    }
    // Broken RPC and an absent pre-CAS binding must not bypass scope cleanup.
    let stopped = crate::runtime::process_scope::terminate_managed_agent_scope(id, true)?;
    ensure!(stopped.stopped, "DSH process scope not stopped");
    ensure!(crate::runtime::process_scope::managed_agent_scope_is_absent(id)?, "DSH process scope is still populated");
    crate::session::store::with_locked_session_rows(
        &crate::session::store::cutex_sessions_path()?,
        |rows| {
            let current = rows.session_mut(id).context("DSH session missing")?;
            ensure!(
                current.runtime_generation == expected_generation
                    && current.app_server_runtime == record.app_server_runtime,
                "DSH close claim changed"
            );
            current.app_server_runtime = None;
            current.runtime_pid = None;
            current.current_runtime_agent_id = None;
            rows.commit().map(drop)
        },
    )
    }, |_| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_children_keep_scoped_bus_env_without_the_rejected_test_home_flag() {
        let home = std::env::temp_dir().join("dsh-child-env-fixture");
        let mut env = BTreeMap::from([("HOME".into(), home.to_string_lossy().into_owned())]);
        let mut mcp = vec![json!({"name":"CUTEX_AGENT_BUS_TOKEN","value":"scoped-fixture"})];
        add_debug_home_boundary(&mut env, &mut mcp, &home, false);
        assert!(!env.contains_key("CUTEX_TEST_PRIVATE_HOME"));
        assert!(mcp.iter().all(|entry| entry["name"] != "CUTEX_TEST_PRIVATE_HOME"));
        assert_eq!(mcp[0]["value"], "scoped-fixture");
        add_debug_home_boundary(&mut env, &mut mcp, &home, true);
        assert_eq!(env["CUTEX_TEST_PRIVATE_HOME"], home.to_string_lossy());
        assert_eq!(env["HOME"], home.to_string_lossy(), "DSH keeps its isolated home");
        // cutex-mcp runs in the daemon's (cutex user's) home, never DSH's.
        let mcp_home = mcp.iter().find(|entry| entry["name"] == "HOME").map(|entry| entry["value"].clone());
        assert_eq!(mcp_home, std::env::var("HOME").ok().map(Value::from));
        assert_ne!(mcp_home, Some(Value::from(home.to_string_lossy().into_owned())));
    }

    #[test]
    fn native_identity_is_not_a_codex_rollout_and_round_trips() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.dsh-test".into(),
            None,
            "host".into(),
            "D:/fixture".into(),
            None,
            "2026-10-25T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::Dsh;
        record.dsh_session = Some(DshSession {
        flavor: Default::default(), kimi: None,
            thread_id: "native-dsh-id".into(),
            state_dir: "D:/fixture/state".into(),
            node: "D:/fixture/node.exe".into(),
            adapter: "D:/fixture/server.mjs".into(),
            dsh_cli: "D:/fixture/dsh.js".into(),
            patch: "D:/fixture/offline.yml".into(),
            install_root: "D:/fixture/install".into(),
            native_session_id: None, deployment_version: String::new(), model: String::new(),
            reasoning_effort: None, api_key_env: String::new(), spike: true,
        });
        assert!(record.codex_session_id.is_none());
        assert_eq!(record.app_server_thread_id(), Some("native-dsh-id"));
        let wire = serde_json::to_value(&record).unwrap();
        assert_eq!(wire["runtime_backend"], "dsh");
        assert_eq!(
            serde_json::from_value::<CutexSessionRecord>(wire).unwrap(),
            record
        );
        let resume = crate::app_server::runtime::thread_resume_params_for_session(&record).unwrap();
        assert_eq!(resume.thread_id, "native-dsh-id");
        assert!(
            resume.permissions.is_none()
                && resume.sandbox.is_none()
                && resume.approval_policy.is_none()
        );
        record.runtime_generation = 3;
        record.runtime_pid = Some(1234);
        record.current_runtime_agent_id = Some("dsh.runtime".into());
        record.app_server_runtime = Some(serde_json::from_value(json!({
            "transport":"loopback_web_socket","endpoint":"ws://127.0.0.1:24391","pid":1234,
            "runtime_dir":"D:/fixture/endpoint","diagnostic_journal_path":"D:/fixture/diag",
            "schema_version":"dsh-v1","schema_sha256":"dsh-v1","started_at":"2026-10-25T00:00:00Z"
        })).unwrap());
        let id = record.cutex_session_id.clone();
        let mut store = crate::session::model::CutexSessionStore::default();
        store.sessions.insert(id.clone(), record);
        let mut agent: crate::agent_bus::model::AgentBusAgent = serde_json::from_value(json!({
            "id":"dsh.runtime","name":"dsh","session_id":"native-dsh-id","profile":"-",
            "cwd":"D:/fixture","pid":1234,"last_seen_epoch_secs":0
        }))
        .unwrap();
        let reconcile =
            crate::session::runtime_reconciliation::reconcile_cutex_session_store_for_registration(
                &mut store,
                &agent,
                "host",
                "2026-10-25T00:00:01Z",
            )
            .unwrap();
        assert!(reconcile.outcome.is_none() && reconcile.store_fence_required);
        crate::session::runtime_reconciliation::reconcile_cutex_session_store_from_agent(
            &mut store,
            &agent,
            "host",
            "2026-10-25T00:00:02Z",
        )
        .unwrap();
        assert_eq!(
            store.sessions.len(),
            1,
            "DSH registration must not import a Codex session"
        );
        assert!(store.sessions[&id].codex_session_id.is_none());
        assert_eq!(store.sessions[&id].runtime_generation, 3);
        assert_eq!(
            store.sessions[&id].current_runtime_agent_id.as_deref(),
            Some("dsh.runtime")
        );
        agent.id = "dsh.stale".into();
        assert!(
            crate::session::runtime_reconciliation::reconcile_cutex_session_store_for_registration(
                &mut store,
                &agent,
                "host",
                "2026-10-25T00:00:03Z"
            )
            .is_err()
        );
    }
}
