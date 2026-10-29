//! Kimi Code (`kimi acp`, MoonshotAI/kimi-code, pinned) as a cutex runtime (r266).
//!
//! A Kimi agent is an ACP-adapter session like DSH: its record has
//! `runtime_backend = kimi` and a `dsh_session` of flavor `kimi` (the shared
//! adapter binding: state directory, pinned Node / adapter / Kimi CLI, facade
//! thread id, native Kimi session id). The `kimi-runtime` component's adapter
//! speaks the same app-server subset and control methods as the DSH adapter, so
//! delivery, asks, history, fork / revert / delete and offline history go
//! through the same daemon code. What is Kimi's own lives here: the pinned
//! deployment, the per-agent private `KIMI_CODE_HOME` (config.toml with the
//! provider, models, permission rules and switches; AGENTS.md with cutex's
//! instructions), the permission mapping and the launch credential.
pub mod account_tokens;
pub mod credential;
pub mod redaction;

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::runtime::dsh::{AcpFlavor, DshSession};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend};

pub const PINNED_KIMI_VERSION: &str = "2.3.3";
pub const KIMI_PACKAGE: &str = "@moonshot-ai/kimi-code";
/// Kimi Code's own `engines.node`.
pub const NODE_MINIMUM: (u64, u64, u64) = (22, 19, 0);
/// The variable Kimi's `api_key_env` names for an API-key launch (K2 contract §5).
pub const KIMI_KEY_ENV: &str = "KIMI_PROFILE_API_KEY";
/// The provider id of an API-key launch in the agent's config.toml (K2 contract §5).
pub const API_KEY_PROVIDER: &str = "cutex-kimi";
/// Tools cutex turns off: Kimi's cron (cutex alarms are the one scheduler) and
/// mid-turn owner questions (cutex agents do not block on a question card).
pub const DISABLED_TOOLS: &[&str] = &["CronCreate", "CronList", "CronDelete", "AskUserQuestion"];
/// Most bytes of cutex's instructions in the agent's `KIMI_CODE_HOME/AGENTS.md`.
pub const KIMI_INSTRUCTIONS_MAX_BYTES: usize = 64 * 1024;

/// Kimi launch options of one agent (flavor `kimi` only; stored in its record).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KimiOptions {
    pub credential: KimiCredentialSource,
    /// Model aliases cutex declares for an API-key launch (empty: Kimi's catalog
    /// below for the selected model; account launches use the account's catalog).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<KimiModel>,
}

/// Where a Kimi agent's credential comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum KimiCredentialSource {
    /// A daemon environment variable holds a Kimi / Moonshot API key (unmanaged
    /// hosts and tests); `base_url` is the Kimi-compatible endpoint.
    Env { env_name: String, base_url: String },
    /// The agent's Profile binding, resolved at every start by the Profile
    /// resolver (K2, `profiles::kimi_launch`).
    Profile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KimiModel {
    /// Kimi's model alias (what `/model` and ACP show), e.g. `kimi-code/kimi-for-coding`.
    pub alias: String,
    /// The provider's model id.
    pub model: String,
    pub max_context_size: u64,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub support_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl KimiModel {
    pub fn image_input(&self) -> bool {
        self.capabilities.iter().any(|capability| capability == "image_in")
    }
}

/// Kimi Code membership models (`api.kimi.com/coding`, `api.kimi.ai/coding`).
pub const KIMI_CODE_MODELS: &[&str] = &["kimi-for-coding", "k3-256k", "k3", "kimi-for-coding-highspeed"];
/// Moonshot Open Platform models (`api.moonshot.ai`, `api.moonshot.cn`).
pub const MOONSHOT_MODELS: &[&str] = &["kimi-k3", "kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6"];

/// The model entry an API-key launch declares, from cutex's documented model
/// facts (`profiles::known_models`; the account route uses the account's own
/// catalog instead). Unknown ids get a generic entry (256k context, tools).
pub fn catalog_model(model: &str) -> KimiModel {
    let known = crate::profiles::known_models::known_model(model);
    let mut capabilities = vec!["tool_use".to_string()];
    let mut support_efforts = Vec::new();
    let mut default_effort = None;
    let mut context = 262_144u64;
    if let Some(known) = known {
        if known.image {
            capabilities.push("image_in".into());
        }
        if !known.efforts.is_empty() {
            capabilities.push("thinking".into());
            support_efforts = known.efforts.iter().map(|effort| effort.to_string()).collect();
            default_effort = known.default_effort.map(str::to_string);
        }
        context = known.context_window.and_then(|value| u64::try_from(value).ok()).unwrap_or(context);
    }
    KimiModel {
        alias: model.to_string(),
        model: model.to_string(),
        max_context_size: context,
        capabilities,
        support_efforts,
        default_effort,
        display_name: known.map(|known| known.display_name.to_string()),
    }
}

/// The models an API-key launch declares: the provider's documented models
/// and the agent's own (which may be any id the provider serves). Profile
/// launches name the family by the profile's provider id (`family`; a relay
/// URL says nothing about it); only env-credential launches (no profile) and
/// provider `kimi` fall back to the endpoint text.
pub fn api_key_models(
    family: Option<crate::profiles::kimi::ModelFamily>,
    base_url: Option<&str>,
    selected: &str,
    extra: &[KimiModel],
) -> Vec<KimiModel> {
    use crate::profiles::kimi::ModelFamily;
    let family = match (family, base_url) {
        (Some(ModelFamily::KimiCode), _) => KIMI_CODE_MODELS,
        (Some(ModelFamily::Moonshot), _) => MOONSHOT_MODELS,
        (None, Some(url)) if url.contains("/coding") => KIMI_CODE_MODELS,
        (None, Some(url)) if url.contains("moonshot") => MOONSHOT_MODELS,
        _ => &[],
    };
    let mut models: Vec<KimiModel> = extra.to_vec();
    for id in family.iter().copied().chain(std::iter::once(selected)) {
        if !models.iter().any(|entry| entry.alias == id) {
            models.push(catalog_model(id));
        }
    }
    models
}

// ---------------------------------------------------------------- deployment

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KimiDeployment {
    pub node: PathBuf,
    pub adapter: PathBuf,
    /// `<install_root>/node_modules/@moonshot-ai/kimi-code/dist/main.mjs`.
    pub kimi_cli: PathBuf,
    pub install_root: PathBuf,
    pub version: String,
}

pub fn deployment_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("kimi/deployment.json"))
}

pub fn kimi_cli_in(install_root: &Path) -> PathBuf {
    install_root.join("node_modules/@moonshot-ai/kimi-code/dist/main.mjs")
}

/// Stable release `vX.Y.Z` at or above Kimi Code's `engines.node`.
pub fn node_version_ready(text: &str) -> bool {
    let text = text.trim().strip_prefix('v').unwrap_or(text.trim());
    let parts: Vec<_> = text.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit())) {
        return false;
    }
    let number = |index: usize| parts[index].parse::<u64>().unwrap_or(0);
    (number(0), number(1), number(2)) >= NODE_MINIMUM
}

impl KimiDeployment {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.version == PINNED_KIMI_VERSION, "Kimi Code deployment must pin {PINNED_KIMI_VERSION}");
        for path in [&self.node, &self.adapter, &self.kimi_cli] {
            ensure!(path.is_absolute() && path.is_file(), "Kimi deployment runtime file missing: {}", path.display());
        }
        ensure!(self.install_root.is_absolute() && self.install_root.is_dir(), "Kimi Code install root missing");
        let metadata: Value = serde_json::from_slice(&std::fs::read(self.install_root.join("node_modules/@moonshot-ai/kimi-code/package.json"))?)?;
        ensure!(metadata["name"] == KIMI_PACKAGE && metadata["version"] == PINNED_KIMI_VERSION, "Kimi Code installed package version mismatch");
        ensure!(self.kimi_cli.canonicalize()? == kimi_cli_in(&self.install_root).canonicalize()?, "Kimi CLI must belong to the pinned installation");
        let node = crate::components::dsh_runtime::node_version(&self.node)?;
        ensure!(node_version_ready(&node), "Kimi Code requires Node >=22.19.0; detected {node}");
        Ok(())
    }
    pub fn install(&self) -> anyhow::Result<()> {
        self.validate()?;
        crate::config::atomic::write_private_pretty_json_atomic(&deployment_path()?, self, "Kimi deployment")
    }
    pub fn selected() -> anyhow::Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(deployment_path()?).context("Kimi Code runtime is not installed; install the kimi-runtime component")?)?;
        value.validate()?;
        Ok(value)
    }
}

// ---------------------------------------------------------------- sessions

/// The agent's Kimi binding (flavor `kimi`).
pub fn session(record: &CutexSessionRecord) -> anyhow::Result<&DshSession> {
    let session = record.dsh_session.as_ref().filter(|session| session.flavor == AcpFlavor::Kimi).context("not a Kimi Code agent")?;
    ensure!(record.runtime_backend == CutexSessionRuntimeBackend::Kimi, "Kimi binding on a non-Kimi record");
    Ok(session)
}

pub fn options(session: &DshSession) -> anyhow::Result<&KimiOptions> {
    session.kimi.as_ref().context("Kimi launch options missing")
}

/// Stop / recovery stay possible when a pinned launch dependency breaks.
pub fn require_control(record: &CutexSessionRecord) -> anyhow::Result<()> {
    ensure!(!crate::agent_management::agent_execution_blocked(record), "not an active Kimi Code session");
    session(record)?;
    ensure!(cfg!(any(windows, target_os = "linux")), "Kimi Code managed runtime is supported on Windows and Linux");
    Ok(())
}

pub fn require_session(record: &CutexSessionRecord) -> anyhow::Result<()> {
    require_control(record)?;
    let session = session(record)?;
    KimiDeployment { node: session.node.clone(), adapter: session.adapter.clone(), kimi_cli: session.dsh_cli.clone(),
        install_root: session.install_root.clone(), version: session.deployment_version.clone() }.validate()?;
    ensure!(session.state_dir.is_absolute() && session.patch == kimi_home(session).join("config.toml"), "Kimi config must be session-owned");
    options(session)?;
    Ok(())
}

/// The agent's private `KIMI_CODE_HOME`.
pub fn kimi_home(session: &DshSession) -> PathBuf {
    session.state_dir.join("home").join("kimi")
}

/// Kimi's session root inside the agent's home (fork / revert / delete).
pub fn sessions_root(session: &DshSession) -> PathBuf {
    kimi_home(session).join("sessions")
}

/// What an agent creation takes (Management, agent tools, tests).
#[derive(Debug, Clone)]
pub struct KimiCreate {
    pub name: String,
    pub cwd: PathBuf,
    pub profile: Option<String>,
    pub model: String,
    /// `None`: the model's default.
    pub reasoning_effort: Option<String>,
    pub options: KimiOptions,
    pub managed: bool,
    pub created_by: Option<crate::session::model::SessionCreatedBy>,
    pub permissions: Option<String>,
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
    /// The frozen r265 Profile binding (Profile launches).
    pub profile_binding: Option<crate::profiles::runtime_variants::ProfileLaunchBinding>,
}

pub fn valid_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && model.chars().count() <= 128 && !model.chars().any(|c| c.is_control() || c == '"')
}

pub const EFFORTS: &[&str] = &["off", "on", "low", "medium", "high", "max"];

/// Preallocates the app-server thread id (not Kimi's session id, which Kimi
/// mints at the first start), so an unstarted agent can join a Project.
pub fn create_agent(request: KimiCreate) -> anyhow::Result<CutexSessionRecord> {
    let KimiCreate { name, cwd, profile, model, reasoning_effort, options, managed, created_by, permissions, sandbox_mode, approval_policy, profile_binding } = request;
    ensure!(!name.trim().is_empty() && name.trim() == name && name.chars().count() <= 128 && !name.chars().any(char::is_control), "valid Kimi agent name required");
    ensure!(cwd.is_absolute() && cwd.is_dir(), "absolute Kimi working directory required");
    ensure!(valid_model(&model), "valid Kimi model required");
    if let Some(effort) = &reasoning_effort {
        ensure!(EFFORTS.contains(&effort.as_str()), "unsupported Kimi reasoning effort {effort}");
    }
    validate_options(&options)?;
    let deployment = KimiDeployment::selected()?;
    let cwd = crate::agent_management::strip_verbatim_prefix(cwd.canonicalize()?.to_str().context("Kimi cwd must be Unicode")?).to_string();
    let id = format!("cutex.{}", uuid::Uuid::new_v4());
    let state_dir = crate::config::paths::runtime_dir()?.join(AcpFlavor::Kimi.state_root()).join(&id);
    std::fs::create_dir_all(&state_dir)?;
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(&state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut record = CutexSessionRecord::new(id.clone(), None, crate::platform::host::current_host_name(), cwd, None)?;
    record.stamp_created_by_at_creation(created_by.as_ref())?;
    record.runtime_backend = CutexSessionRuntimeBackend::Kimi;
    record.formal_agent_name = managed.then(|| name.clone());
    record.thread_name = Some(name.clone());
    record.registration_class = if managed { crate::agent_bus::model::AgentRegistrationClass::Persistent } else { crate::agent_bus::model::AgentRegistrationClass::LocalOnly };
    record.agent_enabled = managed;
    record.exposed_to_backend = true;
    record.profile = profile;
    record.provider_family = Some("kimi".into());
    record.agent_groups = crate::agent_bus::groups::normalize_registered_agent_groups(Vec::new(), None, &record.cwd);
    record.model_defaults = Some(model.clone());
    record.reasoning_defaults = reasoning_effort.clone();
    record.permission_defaults = permissions;
    record.sandbox_mode = sandbox_mode;
    record.approval_policy = approval_policy;
    record.profile_binding = profile_binding;
    let home = state_dir.join("home").join("kimi");
    record.dsh_session = Some(DshSession {
        flavor: AcpFlavor::Kimi,
        kimi: Some(options),
        thread_id: uuid::Uuid::new_v4().to_string(),
        native_session_id: None,
        patch: home.join("config.toml"),
        state_dir,
        node: deployment.node,
        adapter: deployment.adapter,
        dsh_cli: deployment.kimi_cli,
        install_root: deployment.install_root,
        deployment_version: deployment.version,
        model,
        reasoning_effort,
        api_key_env: String::new(),
        spike: false,
    });
    crate::session::store::with_locked_session_rows(&crate::session::store::cutex_sessions_path()?, |rows| {
        ensure!(!managed || !rows.sessions().any(|(_, other)| !other.is_retired() && other.formal_agent_name == record.formal_agent_name), "agent name already exists");
        rows.insert_session(id, record.clone());
        rows.commit().map(drop)
    })?;
    Ok(record)
}

/// A Kimi agent on a Profile's Kimi variant (a Kimi-kind profile with its
/// Kimi account, or `kimi` on a Kimi / Moonshot API-key profile). The launch
/// binding is frozen here; the credential is resolved at every start.
pub fn create_profile_variant(
    name: &str,
    cwd: &Path,
    profile: &str,
    variant: Option<&str>,
    managed: bool,
    created_by: Option<crate::session::model::SessionCreatedBy>,
    overrides: crate::runtime::dsh::DshCreateOverrides,
) -> anyhow::Result<CutexSessionRecord> {
    let store = crate::profiles::settings_api::load_store()?;
    let account = crate::profiles::lookup::find_account(&store, profile)?.context("Kimi profile missing")?;
    let plan = crate::profiles::runtime_variants::plan(account, variant).map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    ensure!(plan.runtime() == &crate::profiles::model::CliKind::Kimi, "profile variant is not a Kimi variant");
    let (status, reason) = crate::profiles::runtime_variants::compatibility(account, &crate::profiles::model::CliKind::Kimi);
    ensure!(status == crate::profiles::runtime_variants::Status::Supported,
        "variant_runtime_unavailable: Kimi Code is not available on this host ({})", reason.unwrap_or(status.as_str()));
    let (default_model, default_effort) = plan.kimi_options();
    let model = overrides.model.as_deref().map(str::trim).filter(|model| !model.is_empty()).map(str::to_string).or(default_model)
        .context("model_required: the Kimi profile has no default model; pass a model")?;
    crate::profiles::kimi::validate_model(&model)?;
    let effort = match &overrides.reasoning_effort {
        Some(effort) => Some(effort.trim().to_string()).filter(|effort| !effort.is_empty() && !crate::profiles::reasoning::is_default(effort)),
        None => default_effort,
    };
    if let Some(effort) = &effort {
        crate::profiles::kimi::validate_effort(effort)?;
    }
    let binding = plan.binding(Some(model.clone()), effort.clone(), Some(PINNED_KIMI_VERSION.into()));
    create_agent(KimiCreate {
        name: name.into(),
        cwd: cwd.into(),
        profile: Some(account.name.clone()),
        model,
        reasoning_effort: effort,
        options: KimiOptions { credential: KimiCredentialSource::Profile, models: Vec::new() },
        managed,
        created_by,
        permissions: overrides.permissions,
        sandbox_mode: overrides.sandbox_mode,
        approval_policy: overrides.approval_policy,
        profile_binding: Some(binding),
    })
}

pub fn validate_options(options: &KimiOptions) -> anyhow::Result<()> {
    match &options.credential {
        KimiCredentialSource::Env { env_name, base_url } => {
            ensure!(!env_name.is_empty() && env_name.len() <= 128 && env_name.bytes().enumerate().all(|(i, b)| b == b'_' || b.is_ascii_uppercase() || (i > 0 && b.is_ascii_digit())),
                "Kimi API key variable must be an upper-case environment variable name");
            ensure!(env_name != KIMI_KEY_ENV, "{KIMI_KEY_ENV} is reserved for the Kimi child");
            ensure!(base_url.starts_with("https://") || (cfg!(debug_assertions) && base_url.starts_with("http://127.0.0.1:")), "Kimi base URL must be https");
        }
        KimiCredentialSource::Profile => {}
    }
    for model in &options.models {
        ensure!(valid_model(&model.alias) && valid_model(&model.model) && model.max_context_size > 0, "invalid Kimi model entry");
    }
    Ok(())
}

// ---------------------------------------------------------------- permissions

/// Kimi's ACP mode, the approval policy the adapter applies and extra
/// permission rules, for the agent's cutex settings. Kimi has no OS sandbox:
///
/// | cutex sandbox / approval | Kimi |
/// |---|---|
/// | danger-full-access + never | mode `auto` (Never Ask): nothing asks |
/// | danger-full-access + other | mode `yolo` (Ask When Needed): only sensitive files and dangerous commands ask |
/// | workspace-write (default) | mode `default` (Always Ask): reads and edits in the workspace run, commands and everything else ask |
/// | read-only | mode `default` + a `PreToolUse` hook refusing every tool outside [`READ_ONLY_TOOLS`]; the adapter declines any ask |
/// | approval never (other sandboxes) | asks are refused at once by the adapter (no owner prompt) |
///
/// Kimi Code 2.3.3 parses `[[permission.rules]]` but never applies them (its
/// rule state is only filled at runtime), so the rules are kept for later
/// versions while read-only is enforced by the hook (see [`read_only_hook`]).
pub fn permission_mapping(record: &CutexSessionRecord) -> (&'static str, &'static str, Vec<(&'static str, &'static str)>) {
    use crate::agent_management::envelope::{ApprovalStrictness, SandboxLevel};
    let (sandbox, approval) = crate::runtime::args::effective_runtime_permission_defaults(record);
    let never = approval.as_deref().and_then(ApprovalStrictness::parse) == Some(ApprovalStrictness::Never);
    let mut rules = vec![("allow", "mcp__cutex__*")];
    let mode = match sandbox.as_deref().and_then(SandboxLevel::parse) {
        Some(SandboxLevel::FullAccess) if never => "auto",
        Some(SandboxLevel::FullAccess) => "yolo",
        Some(SandboxLevel::ReadOnly) => {
            rules.extend([("deny", "Write"), ("deny", "Edit")]);
            "default"
        }
        _ => "default",
    };
    (mode, if never { "never" } else { "ask" }, rules)
}

/// Kimi tools a read-only agent may use: reading, searching, the turn plan,
/// web search / fetch and cutex's own tools. Everything else is refused before
/// it runs: `Write`, `Edit`, `Bash` (any shell command, read-only or not),
/// subagents (`Agent`, `AgentSwarm`), `Skill`, other MCP servers and any tool a
/// later Kimi version adds.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "Read", "Grep", "Glob", "ReadMediaFile", "SetTodoList", "TodoList", "TaskList", "TaskOutput", "WaitFor",
    "WebSearch", "FetchURL", "ExitPlanMode", "GetGoal", "select_tools", "NotifyUser", "mcp__cutex__.*",
];

/// Whether the agent's cutex sandbox is read-only.
pub fn read_only(record: &CutexSessionRecord) -> bool {
    use crate::agent_management::envelope::SandboxLevel;
    let (sandbox, _) = crate::runtime::args::effective_runtime_permission_defaults(record);
    sandbox.as_deref().and_then(SandboxLevel::parse) == Some(SandboxLevel::ReadOnly)
}

/// The `[[hooks]]` entry of a read-only agent: Kimi runs it before every tool
/// call whose name is not in [`READ_ONLY_TOOLS`] (its matcher is a JavaScript
/// regular expression on the tool name) and refuses the call on exit code 2,
/// before any owner ask and before the tool runs. The command works in `sh` and
/// `cmd.exe` (Kimi spawns hooks with `shell: true`); its `exit 2` does not
/// depend on the message (`&`: in the background in `sh`, sequential in
/// `cmd`), and it has no `;` (a separator in `sh`). A hook that cannot start
/// or times out lets the call through (Kimi's rule): the adapter still
/// declines every ask, so only tools Kimi runs without asking (reads, and
/// edits inside a git workspace) depend on the hook alone.
pub fn read_only_hook() -> toml::Table {
    let matcher = format!("^(?!({})$)", READ_ONLY_TOOLS.join("|"));
    [
        ("event".to_string(), toml::Value::String("PreToolUse".into())),
        ("matcher".to_string(), toml::Value::String(matcher)),
        ("command".to_string(), toml::Value::String(READ_ONLY_HOOK_COMMAND.into())),
        ("timeout".to_string(), toml::Value::Integer(10)),
    ]
    .into_iter()
    .collect()
}

pub const READ_ONLY_HOOK_COMMAND: &str = "echo cutex: this agent is read-only and may not change files or run commands 1>&2 & exit 2";

// ---------------------------------------------------------------- instructions

/// Project instruction files Kimi loads itself for `cwd`: from the project
/// root (`.git`, else `cwd` alone) down to `cwd`, `.kimi-code/AGENTS.md` and the
/// first of `AGENTS.md` / `agents.md`. cutex never injects these again.
pub fn project_instruction_files(cwd: &Path) -> Vec<PathBuf> {
    let root = project_root(cwd);
    let mut dirs: Vec<&Path> = cwd.ancestors().take_while(|dir| dir.starts_with(&root)).collect();
    dirs.reverse();
    let mut files = Vec::new();
    for dir in dirs {
        let dot = dir.join(".kimi-code").join("AGENTS.md");
        if dot.is_file() {
            files.push(dot);
        }
        if let Some(plain) = ["AGENTS.md", "agents.md"].iter().map(|name| dir.join(name)).find(|path| path.is_file()) {
            files.push(plain);
        }
    }
    files
}

/// Kimi's project root for `cwd`: the nearest ancestor with a `.git` entry, else `cwd`.
pub fn project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(cwd).to_path_buf()
}

/// Kimi reads instructions, skills and MCP files on every level from its
/// project root down to the cwd. A root above the cwd whose `.git` is no git
/// repository (an empty `.git` directory, e.g. in a home folder) would make
/// Kimi read the owner's files outside the agent's folder: refused.
pub fn check_project_root(cwd: &Path) -> anyhow::Result<()> {
    let root = project_root(cwd);
    if root == cwd {
        return Ok(());
    }
    let marker = root.join(".git");
    let repository = marker.is_file() || marker.join("HEAD").is_file();
    ensure!(repository,
        "Kimi Code would treat {} as this agent's project (it has an empty .git) and read instruction files above the agent's folder; make the folder a git repository (git init) or remove the stray .git there",
        root.display());
    Ok(())
}

/// A workspace cutex allocated for the agent (`<config>/agents-scratch/<id>`
/// for an agent without a folder, `<config>/chats/<id>` for a quick chat) is
/// the agent's own folder: Kimi's project root is pinned there with an empty
/// `.git` directory (Kimi takes the first ancestor with `.git`, without a HEAD
/// check). Otherwise a stray `.git` above the cutex home (an empty `~/.git`)
/// would be Kimi's project and the start refused. Other folders are the
/// owner's: never touched (`check_project_root` decides).
pub fn pin_scratch_project_root(cwd: &Path) -> anyhow::Result<()> {
    match crate::config::paths::config_dir() {
        Ok(config) => pin_scratch_project_root_in(&config, cwd),
        Err(_) => Ok(()),
    }
}

fn pin_scratch_project_root_in(config: &Path, cwd: &Path) -> anyhow::Result<()> {
    if project_root(cwd) == cwd {
        return Ok(());
    }
    let scratch = cwd.file_name().and_then(|name| name.to_str()).is_some_and(crate::asks::caps::is_allocated_scratch_name)
        && cwd.parent().is_some_and(|area| {
            area.parent() == Some(config)
                && area.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
                    name == crate::management::v2::human_agents::AGENT_SCRATCH_DIR_NAME
                        || name == crate::management::v2::human_sessions::SCRATCH_DIR_NAME
                })
        });
    if !scratch || !cwd.is_dir() {
        return Ok(());
    }
    let marker = cwd.join(".git");
    std::fs::create_dir(&marker).with_context(|| format!("pinning Kimi's project root at {}", cwd.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// What cutex adds to a Kimi agent's instructions, as for a Claude or DSH
/// agent: `# Cutex` (when enabled), the user AGENTS.md (Kimi's home is
/// private, so it never sees the user's), `# File references` and, for
/// project members, `# Memory`. Project files are Kimi's own (not repeated).
pub fn instructions(record: &CutexSessionRecord) -> (Option<String>, Vec<PathBuf>) {
    let cwd = Path::new(&record.cwd);
    let memory = crate::memory::launch_memory_context_or_warn(&record.cutex_session_id);
    let root = memory.as_ref().map(|memory| memory.file_reference_root(cwd)).unwrap_or(cwd);
    let user = crate::memory::harness::user_agents_md();
    let mut sources = user.as_ref().map(|user| user.files.clone()).unwrap_or_default();
    sources.extend(project_instruction_files(cwd));
    let sections = crate::memory::harness::compose_claude_sections(
        crate::memory::harness::launch_harness(record),
        user,
        None,
        crate::memory::render_file_citation_instructions(root),
        memory.map(|memory| memory.instructions),
        KIMI_INSTRUCTIONS_MAX_BYTES,
    );
    let text = crate::memory::harness::join_sections(&sections);
    ((!text.trim().is_empty()).then_some(text), sources)
}

// ---------------------------------------------------------------- launch

/// What the adapter launch needs beyond the shared ACP launch file.
pub struct KimiLaunchParts {
    pub instruction_sources: Vec<PathBuf>,
    /// `(adapter env name, value)`; mapped to `KIMI_PROFILE_API_KEY` for Kimi.
    pub secret_env: Option<(String, String)>,
    pub mode: &'static str,
    pub approval_policy: &'static str,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub image_models: Vec<String>,
    pub kimi_home: PathBuf,
    /// The cutex sandbox is read-only (the adapter declines every ask).
    pub read_only: bool,
    /// Where the adapter reads the account's current OAuth tokens and rotated
    /// fingerprints for redaction (paths only; `None` for API-key launches).
    pub redaction: Option<redaction::RedactionPaths>,
}

/// Writes the agent's private `KIMI_CODE_HOME` for this start (config.toml,
/// AGENTS.md, shared login links). Never writes a key or token.
pub fn write_home(record: &CutexSessionRecord) -> anyhow::Result<KimiLaunchParts> {
    let session = session(record)?;
    let options = options(session)?;
    pin_scratch_project_root(Path::new(&record.cwd))?;
    check_project_root(Path::new(&record.cwd))?;
    let home = kimi_home(session);
    std::fs::create_dir_all(&home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    }
    // The live model (a change on the chat lands in `model_defaults`).
    let model = record.model_defaults.as_deref().map(str::trim).filter(|model| !model.is_empty()).unwrap_or(&session.model).to_string();
    let effort = record.reasoning_defaults.clone().or_else(|| session.reasoning_effort.clone()).filter(|effort| !effort.is_empty());
    let resolved = credential::resolve(record, session, options)?;
    resolved.install_into(&home)?;
    let declared = if resolved.declare_models {
        api_key_models(resolved.model_family, resolved.base_url.as_deref(), &model, &options.models)
    } else {
        Vec::new()
    };
    let (mode, approval_policy, rules) = permission_mapping(record);
    let read_only = read_only(record);
    let config = config_table(&resolved, &declared, &model, &rules, read_only);
    let image_models = image_models(&config);
    let text = format!("# Written by cutex at every start of this agent; edits are replaced.\n{}", toml::to_string(&config)?);
    crate::config::atomic::write_private_bytes_atomic(&session.patch, text.as_bytes())?;
    let (instructions, instruction_sources) = instructions(record);
    let agents = home.join("AGENTS.md");
    match instructions {
        Some(text) => crate::config::atomic::write_private_bytes_atomic(&agents, text.as_bytes())?,
        None => {
            let _ = std::fs::remove_file(&agents);
        }
    }
    // A Kimi account: cutex refreshes the shared token ahead of Kimi (no Kimi
    // process of the account races a refresh) and records its values'
    // fingerprints for redaction.
    let redaction = match resolved.account() {
        Some(account) => {
            if let Err(error) = account_tokens::ensure_fresh(&account) {
                eprintln!("kimi: the account {} token was not refreshed before the start: {error}", account.account_id);
            }
            account_tokens::remember_current(&account);
            Some(redaction::RedactionPaths { credentials_dir: Some(home.join("credentials")), ledger: redaction::ledger_path(&account.account_id).ok() })
        }
        None => None,
    };
    Ok(KimiLaunchParts { instruction_sources, secret_env: resolved.secret_env, mode, approval_policy, model, reasoning_effort: effort, image_models, kimi_home: home, read_only, redaction })
}

fn strings(values: &[impl AsRef<str>]) -> toml::Value {
    toml::Value::Array(values.iter().map(|value| toml::Value::String(value.as_ref().to_string())).collect())
}

/// The agent's `config.toml`: the credential's tables, the declared models,
/// cutex's switches and the permission rules.
pub fn config_table(resolved: &credential::ResolvedCredential, models: &[KimiModel], default_model: &str, rules: &[(&str, &str)], read_only: bool) -> toml::Table {
    let mut config = resolved.config.clone();
    config.insert("default_model".into(), toml::Value::String(default_model.into()));
    config.insert("default_permission_mode".into(), toml::Value::String("manual".into()));
    config.insert("telemetry".into(), toml::Value::Boolean(false));
    config.insert("auto_session_title".into(), toml::Value::Boolean(false));
    let mut watch = toml::Table::new();
    watch.insert("enabled".into(), toml::Value::Boolean(false));
    config.insert("watch".into(), toml::Value::Table(watch));
    let mut tools = toml::Table::new();
    tools.insert("disabled".into(), strings(DISABLED_TOOLS));
    config.insert("tools".into(), toml::Value::Table(tools));
    let table = config.entry("models").or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(table) = table.as_table_mut() {
        for model in models {
            let mut entry = toml::Table::new();
            entry.insert("provider".into(), toml::Value::String(resolved.provider_id.clone()));
            entry.insert("model".into(), toml::Value::String(model.model.clone()));
            entry.insert("max_context_size".into(), toml::Value::Integer(model.max_context_size.min(i64::MAX as u64) as i64));
            entry.insert("capabilities".into(), strings(&model.capabilities));
            if !model.support_efforts.is_empty() {
                entry.insert("support_efforts".into(), strings(&model.support_efforts));
            }
            if let Some(effort) = &model.default_effort {
                entry.insert("default_effort".into(), toml::Value::String(effort.clone()));
            }
            if let Some(name) = &model.display_name {
                entry.insert("display_name".into(), toml::Value::String(name.clone()));
            }
            table.insert(model.alias.clone(), toml::Value::Table(entry));
        }
    }
    let rules = rules
        .iter()
        .map(|(decision, pattern)| {
            let mut rule = toml::Table::new();
            rule.insert("decision".into(), toml::Value::String(decision.to_string()));
            rule.insert("pattern".into(), toml::Value::String(pattern.to_string()));
            toml::Value::Table(rule)
        })
        .collect();
    let mut permission = toml::Table::new();
    permission.insert("rules".into(), toml::Value::Array(rules));
    config.insert("permission".into(), toml::Value::Table(permission));
    if read_only {
        config.insert("hooks".into(), toml::Value::Array(vec![toml::Value::Table(read_only_hook())]));
    } else {
        config.remove("hooks");
    }
    config
}

/// Model aliases of a config that take image input (the adapter refuses
/// images for the others before they reach Kimi).
pub fn image_models(config: &toml::Table) -> Vec<String> {
    config
        .get("models")
        .and_then(toml::Value::as_table)
        .map(|models| {
            models
                .iter()
                .filter(|(_, entry)| {
                    entry.get("capabilities").and_then(toml::Value::as_array).is_some_and(|caps| caps.iter().any(|cap| cap.as_str() == Some("image_in")))
                })
                .map(|(alias, _)| alias.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The ACP switches every Kimi child gets (Phase-0 facts: no inotify watchers,
/// no telemetry, no Kimi cron, no self-update, no region marker).
pub const KIMI_SWITCHES: &[(&str, &str)] = &[
    ("KIMI_CODE_WATCH", "0"),
    ("KIMI_DISABLE_TELEMETRY", "1"),
    ("KIMI_DISABLE_CRON", "1"),
    ("KIMI_CODE_NO_AUTO_UPDATE", "1"),
    ("KIMI_CODE_REGION_MARKER", "off"),
];

pub fn native_schema() -> Value {
    use sha2::Digest;
    json!({
        "protocol":"kimi-acp-subset", "majorVersion":1, "version":"kimi-acp-v1", "channel":"experimental",
        "sha256":format!("{:x}", sha2::Sha256::digest(include_bytes!("kimi-contract.json"))),
        "capabilities":{"experimentalApi":true,"features":{"turnSteer":false,"threadFork":true,"threadRevert":true,"threadDelete":true,"contextOccupancy":true,"billingUsage":true,"textInputOnly":false,"goals":false,"review":false,"skillInput":false,"memoryMode":false,"backgroundTerminals":false,"imageInput":true,"planMode":false,"compact":false,"threadSearch":false,"sideChat":false,"threadSettingsUpdate":true,"osSandbox":false}},
        "extensions":["kimi-runtime-v1"]
    })
}

pub fn contract_sha256() -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(include_bytes!("kimi-contract.json")))
}

#[cfg(test)]
#[path = "kimi_tests.rs"]
mod tests;
