//! Product deployment and creation; no credentials are stored here.
use std::path::{Path, PathBuf};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use super::{DshSession, CutexSessionRecord, CutexSessionRuntimeBackend};

pub const PINNED_DSH_VERSION: &str = "0.2.1-rc.3";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DshDeployment {
    pub node: PathBuf,
    pub adapter: PathBuf,
    pub dsh_cli: PathBuf,
    pub install_root: PathBuf,
    pub version: String,
}

pub fn deployment_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("dsh/deployment.json"))
}

impl DshDeployment {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.version == PINNED_DSH_VERSION, "DSH deployment must pin {PINNED_DSH_VERSION}");
        for path in [&self.node, &self.adapter, &self.dsh_cli] {
            ensure!(path.is_absolute() && path.is_file(), "DSH deployment runtime file missing: {}", path.display());
        }
        ensure!(self.install_root.is_absolute() && self.install_root.is_dir(), "DSH install root missing");
        let metadata: Value = serde_json::from_slice(&std::fs::read(self.install_root.join("node_modules/@deepseek-ai/dsh/package.json"))?)?;
        ensure!(metadata["name"] == "@deepseek-ai/dsh" && metadata["version"] == PINNED_DSH_VERSION, "DSH installed package version mismatch");
        ensure!(self.dsh_cli.canonicalize()? == self.install_root.join("node_modules/@deepseek-ai/dsh/lib/bin.js").canonicalize()?, "DSH CLI must belong to the pinned installation");
        let node = crate::components::dsh_runtime::node_version(&self.node)?;
        ensure!(crate::components::dsh_runtime::node_version_supported(&node), "DSH requires Node >=24.9.0; detected {node}");
        Ok(())
    }
    pub fn install(&self) -> anyhow::Result<()> {
        self.validate()?;
        crate::config::atomic::write_private_pretty_json_atomic(&deployment_path()?, self, "DSH deployment")
    }
    pub fn selected() -> anyhow::Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(deployment_path()?).context("DSH runtime is not installed; install the dsh-runtime component")?)?;
        value.validate()?;
        Ok(value)
    }
}

/// Stop/recovery must remain possible when a pinned launch dependency breaks.
pub fn require_control(record: &CutexSessionRecord) -> anyhow::Result<()> {
    if record.dsh_session.as_ref().is_some_and(|session| session.flavor == super::AcpFlavor::Kimi) {
        return crate::runtime::kimi::require_control(record);
    }
    ensure!(record.runtime_backend == CutexSessionRuntimeBackend::Dsh && !crate::agent_management::agent_execution_blocked(&record), "not an active DSH session");
    let session = record.dsh_session.as_ref().context("DSH session binding missing")?;
    if session.spike { return super::require_spike(); }
    ensure!(cfg!(any(windows, target_os = "linux")), "DSH managed runtime is supported on Windows and Linux");
    Ok(())
}

pub fn require_session(record: &CutexSessionRecord) -> anyhow::Result<()> {
    if record.dsh_session.as_ref().is_some_and(|session| session.flavor == super::AcpFlavor::Kimi) {
        return crate::runtime::kimi::require_session(record);
    }
    require_control(record)?;
    let session = record.dsh_session.as_ref().context("DSH session binding missing")?;
    if session.spike { return Ok(()); }
    DshDeployment { node:session.node.clone(), adapter:session.adapter.clone(), dsh_cli:session.dsh_cli.clone(), install_root:session.install_root.clone(), version:session.deployment_version.clone() }.validate()?;
    ensure!(session.state_dir.is_absolute() && session.patch == session.state_dir.join("profile.patch.yml"), "DSH product patch must be session-owned");
    crate::profiles::dsh::DshOptions { model: session.model.clone(), reasoning_effort: session.reasoning_effort.clone(), api_key_env: session.api_key_env.clone() }.validate()?;
    Ok(())
}

/// Preallocate the app-server conversation id, not an ACP native id. This lets
/// an unstarted agent be imported into a Project without starting a model/runtime.
pub fn create_profile(name: &str, cwd: &Path, profile: Option<String>, managed: bool, created_by: Option<crate::session::model::SessionCreatedBy>) -> anyhow::Result<CutexSessionRecord> {
    create_profile_variant(name, cwd, profile, None, managed, created_by)
}

/// r265 (C3): `variant` = a runtime variant of the profile (None = its default).
pub fn create_profile_variant(name: &str, cwd: &Path, profile: Option<String>, variant: Option<&str>, managed: bool, created_by: Option<crate::session::model::SessionCreatedBy>) -> anyhow::Result<CutexSessionRecord> {
    create_profile_variant_with(name, cwd, profile, variant, managed, created_by, DshCreateOverrides::default())
}

/// What an agent-created DSH agent (`agent_create` / `member_replace`) takes
/// from its resolved managed spec instead of the variant defaults.
#[derive(Debug, Clone, Default)]
pub struct DshCreateOverrides {
    pub model: Option<String>,
    /// `Some("")`: no explicit effort (DSH's default).
    pub reasoning_effort: Option<String>,
    pub permissions: Option<String>,
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
}

pub fn create_profile_variant_with(name: &str, cwd: &Path, profile: Option<String>, variant: Option<&str>, managed: bool, created_by: Option<crate::session::model::SessionCreatedBy>, overrides: DshCreateOverrides) -> anyhow::Result<CutexSessionRecord> {
    ensure!(!name.trim().is_empty() && name.trim() == name && name.chars().count() <= 128 && !name.chars().any(char::is_control), "valid DSH agent name required");
    ensure!(cwd.is_absolute() && cwd.is_dir(), "absolute DSH working directory required");
    let deployment = DshDeployment::selected()?;
    let (profile, options, plan) = match profile {
        Some(profile) => {
            let store = crate::profiles::settings_api::load_store()?;
            let account = crate::profiles::lookup::find_account(&store, &profile)?.context("DSH profile missing")?;
            let plan = crate::profiles::runtime_variants::plan(account, variant).map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
            ensure!(plan.runtime() == &crate::profiles::model::CliKind::Dsh, "profile variant is not a DSH variant");
            (Some(account.name.clone()), plan.dsh_options(), Some(plan))
        }
        None => (None, crate::profiles::dsh::DshOptions::default(), None),
    };
    let mut options = options;
    if let Some(model) = overrides.model.as_deref().map(str::trim).filter(|model| !model.is_empty()) {
        crate::profiles::dsh::validate_catalog_model(model)?;
        options.model = model.to_string();
    }
    if let Some(effort) = &overrides.reasoning_effort {
        options.reasoning_effort = Some(effort.trim().to_string()).filter(|effort| !effort.is_empty());
    }
    options.validate()?;
    let cwd = crate::agent_management::strip_verbatim_prefix(cwd.canonicalize()?.to_str().context("DSH cwd must be Unicode")?).to_string();
    let id = format!("cutex.{}", uuid::Uuid::new_v4());
    let state_dir = crate::config::paths::runtime_dir()?.join("dsh").join(&id);
    std::fs::create_dir_all(&state_dir)?;
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(&state_dir)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut record = CutexSessionRecord::new(id.clone(), None, crate::platform::host::current_host_name(), cwd, None)?;
    record.stamp_created_by_at_creation(created_by.as_ref())?;
    record.runtime_backend = CutexSessionRuntimeBackend::Dsh;
    record.formal_agent_name = managed.then(|| name.into());
    record.thread_name = Some(name.into());
    record.registration_class = if managed { crate::agent_bus::model::AgentRegistrationClass::Persistent } else { crate::agent_bus::model::AgentRegistrationClass::LocalOnly };
    record.agent_enabled = managed;
    record.exposed_to_backend = true;
    record.profile = profile;
    record.provider_family = Some("deepseek".into());
    record.agent_groups = crate::agent_bus::groups::normalize_registered_agent_groups(Vec::new(), None, &record.cwd);
    record.model_defaults = Some(options.model.clone());
    record.reasoning_defaults = options.reasoning_effort.clone();
    record.permission_defaults = overrides.permissions.clone();
    record.sandbox_mode = overrides.sandbox_mode.clone();
    record.approval_policy = overrides.approval_policy.clone();
    record.profile_binding = plan.map(|plan| plan.binding(Some(options.model.clone()), options.reasoning_effort.clone(), Some(deployment.version.clone())));
    record.dsh_session = Some(DshSession {
            flavor: Default::default(), kimi: None,
        thread_id: uuid::Uuid::new_v4().to_string(), native_session_id: None,
        patch: state_dir.join("profile.patch.yml"), state_dir,
        node: deployment.node, adapter: deployment.adapter, dsh_cli: deployment.dsh_cli,
        install_root: deployment.install_root, deployment_version: deployment.version,
        model: options.model, reasoning_effort: options.reasoning_effort, api_key_env: options.api_key_env, spike: false,
    });
    crate::session::store::with_locked_session_rows(&crate::session::store::cutex_sessions_path()?, |rows| {
        ensure!(!managed || !rows.sessions().any(|(_, other)| !other.is_retired() && other.formal_agent_name == record.formal_agent_name), "agent name already exists");
        rows.insert_session(id, record.clone()); rows.commit().map(drop)
    })?;
    Ok(record)
}

pub fn native_schema() -> Value {
    use sha2::Digest;
    json!({
        "protocol":"dsh-acp-subset", "majorVersion":1, "version":"dsh-acp-v1", "channel":"experimental",
        "sha256":format!("{:x}", sha2::Sha256::digest(include_bytes!("../dsh-contract.json"))),
        "capabilities":{"experimentalApi":true,"features":{"turnSteer":false,"threadFork":true,"threadRevert":true,"threadDelete":true,"contextOccupancy":true,"billingUsage":true,"textInputOnly":false,"goals":false,"review":false,"skillInput":false,"memoryMode":false,"backgroundTerminals":false,"imageInput":true,"planMode":false,"compact":false,"threadSearch":false,"sideChat":false,"threadSettingsUpdate":true}},
        "extensions":["dsh-runtime-v1"]
    })
}

/// DSH's two permission knobs for an agent's cutex settings: the sandbox
/// mode (same three values as cute-codex) and the approval policy (`never`
/// rejects every ask without prompting; every other cutex policy asks the
/// owner). An agent without explicit settings keeps the product default,
/// workspace-write + ask.
pub fn permission_knobs(record: &CutexSessionRecord) -> (&'static str, &'static str) {
    let (sandbox, approval) = crate::runtime::args::effective_runtime_permission_defaults(record);
    let sandbox = match sandbox.as_deref() {
        Some("read-only") => "read-only",
        Some("danger-full-access") => "danger-full-access",
        _ => "workspace-write",
    };
    let approval = if approval.as_deref() == Some("never") { "never" } else { "ask" };
    (sandbox, approval)
}

/// The cutex DSH plugin beside the adapter (component >= r265); an older
/// component has none and keeps DSH's own behaviour.
fn cutex_plugin(session: &DshSession) -> Option<PathBuf> {
    let plugin = session.adapter.with_file_name("dsh-plugin.mjs");
    plugin.is_file().then_some(plugin)
}

/// Most bytes of the cutex instructions given to a DSH agent (a file read by
/// the cutex plugin, so no command-line cap: Claude's Linux budget).
pub const DSH_INSTRUCTIONS_MAX_BYTES: usize = 64 * 1024;

/// Instruction files DSH's own `agent-instructions` loads for `cwd`: from the
/// project root (`.git`, else `cwd` alone) down to `cwd`, `AGENTS.md`,
/// `CLAUDE.md` and their `.local` variants. cutex never injects these again.
pub fn dsh_project_instruction_files(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<&Path> = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir);
        if dir.join(".git").exists() {
            break;
        }
    }
    if !dirs.last().is_some_and(|dir| dir.join(".git").exists()) {
        dirs.truncate(1);
    }
    dirs.iter().rev().flat_map(|dir| ["AGENTS.md", "CLAUDE.md", "AGENTS.local.md", "CLAUDE.local.md"].map(|name| dir.join(name)))
        .filter(|path| path.is_file())
        .collect()
}

/// What cutex adds to a DSH agent's system prompt, as for a Claude agent:
/// `# Cutex` (when enabled), the user AGENTS.md (DSH's home is isolated, so
/// it never sees the user's), `# File references` and, for project members,
/// `# Memory`. Project AGENTS.md / CLAUDE.md are DSH's own (not repeated).
/// Returns the text (if any) and every instruction source the agent gets.
pub fn instructions(record: &CutexSessionRecord) -> (Option<String>, Vec<PathBuf>) {
    let cwd = Path::new(&record.cwd);
    let memory = crate::memory::launch_memory_context_or_warn(&record.cutex_session_id);
    let root = memory.as_ref().map(|memory| memory.file_reference_root(cwd)).unwrap_or(cwd);
    let user = crate::memory::harness::user_agents_md();
    let mut sources = user.as_ref().map(|user| user.files.clone()).unwrap_or_default();
    sources.extend(dsh_project_instruction_files(cwd));
    let sections = crate::memory::harness::compose_claude_sections(
        crate::memory::harness::launch_harness(record),
        user,
        None,
        crate::memory::render_file_citation_instructions(root),
        memory.map(|memory| memory.instructions),
        DSH_INSTRUCTIONS_MAX_BYTES,
    );
    let text = crate::memory::harness::join_sections(&sections);
    ((!text.trim().is_empty()).then_some(text), sources)
}

/// Owned configuration only; no key value is ever interpolated into this file.
/// Returns the instruction sources for the adapter's thread responses.
pub(super) fn write_patch(record: &CutexSessionRecord) -> anyhow::Result<Vec<PathBuf>> {
    let session = record.dsh_session.as_ref().context("DSH session missing")?;
    if session.spike { return Ok(Vec::new()); }
    // Configure the existing public shell provider before writing any patch
    // or instructions. Store activation can escape the command's Windows Job.
    let shell_patch = super::windows_shell::patch()?;
    let cwd = serde_json::to_string(&record.cwd)?;
    // The live model (a change on the chat lands in `model_defaults`) also
    // decides DSH's image capability, which DSH fixes at start.
    let live_model = record.model_defaults.as_deref().map(str::trim).filter(|model| !model.is_empty()).unwrap_or(&session.model);
    let model = serde_json::to_string(live_model)?;
    let (sandbox, approval) = permission_knobs(record);
    let mut text = format!("- id: acp\n  config:\n    provider: deepseek-official\n    model: {model}\n- id: session-persistence-jsonl\n  config:\n    root: !!js process.env.DSH_SESSION_LOG_ROOT\n    compression: none\n- id: sandbox-policy\n  config:\n    mode: {sandbox}\n    workspaceRoot: {cwd}\n- id: approval\n  config:\n    policy: {approval}\n- id: session-log-deepseek\n  config:\n    enabled: false\n");
    text.push_str(&shell_patch);
    // Fresh sessions pin the agent's knobs through the presets service (a
    // patch replaces the row's whole config, so DSH's own presets are kept).
    text.push_str(&format!("- id: permission\n  config:\n    presets:\n      read-only:\n        sandbox: read-only\n        approval: ask\n      workspace-write:\n        sandbox: workspace-write\n        approval: ask\n      danger-full-access:\n        sandbox: danger-full-access\n        approval: never\n      cutex-agent:\n        name: Cutex agent settings\n        sandbox: {sandbox}\n        approval: {approval}\n    defaultPreset: cutex-agent\n"));
    // A resumed session keeps its pinned knobs in DSH; the cutex plugin
    // re-applies the agent's current settings to it at every start.
    // The plugin also adds cutex's instructions as one literal system-prompt
    // section (read from a private file: no template escaping, no size cap
    // of a command line).
    let (instructions, sources) = instructions(record);
    let instructions_file = session.state_dir.join("cutex-instructions.md");
    if let Some(plugin) = cutex_plugin(session) {
        let plugin = serde_json::to_string(&plugin.to_string_lossy().replace('\\', "/"))?;
        text.push_str(&format!("- insert:\n    - id: cutex-bridge\n      name: {plugin}\n      config:\n        permissionPreset: cutex-agent\n"));
        if let Some(instructions) = instructions {
            std::fs::create_dir_all(&session.state_dir)?;
            crate::config::atomic::write_private_bytes_atomic(&instructions_file, instructions.as_bytes())?;
            let file = serde_json::to_string(&instructions_file.to_string_lossy().replace('\\', "/"))?;
            text.push_str(&format!("        instructionsFile: {file}\n"));
        }
    }
    crate::config::atomic::write_private_bytes_atomic(&session.patch, text.as_bytes())?;
    Ok(sources)
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;

    /// C-5 (r265 E2E): the adapter reports each request's token usage
    /// (`thread/tokenUsage/updated`), which /v2/usage counts and prices.
    #[test]
    fn dsh_schema_and_contract_declare_billing_usage() {
        assert_eq!(native_schema()["capabilities"]["features"]["billingUsage"], true);
        let contract: Value = serde_json::from_slice(include_bytes!("../dsh-contract.json")).unwrap();
        assert_eq!(contract["capabilities"]["billingUsage"], true);
    }

    /// The contract declares every notification the adapter sends: token
    /// usage per request, ACP tool progress (`item/updated`, ACP path) and the
    /// live model / effort selection (additive, r265).
    #[test]
    fn dsh_contract_declares_every_adapter_notification() {
        let contract: Value = serde_json::from_slice(include_bytes!("../dsh-contract.json")).unwrap();
        let declared: Vec<&str> = contract["notifications"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        for method in ["thread/tokenUsage/updated", "item/updated", "dsh/launchSelection", "turn/plan/updated", "item/started", "item/completed"] {
            assert!(declared.contains(&method), "{method} not declared");
        }
        let unique: std::collections::BTreeSet<_> = declared.iter().collect();
        assert_eq!(unique.len(), declared.len(), "no duplicates");
    }

    #[test]
    fn dsh_loads_project_instructions_itself_so_cutex_only_lists_them() {
        let base = std::env::temp_dir().join(format!("dsh-instructions-{}", uuid::Uuid::new_v4()));
        let sub = base.join("repo").join("crate");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::write(base.join("AGENTS.md"), "outside").unwrap();
        for (dir, name) in [("repo", "AGENTS.md"), ("repo", "CLAUDE.local.md"), ("repo/crate", "CLAUDE.md"), ("repo/crate", "AGENTS.override.md")] {
            std::fs::write(base.join(dir).join(name), "x").unwrap();
        }
        assert_eq!(dsh_project_instruction_files(&sub), vec![base.join("repo/AGENTS.md"), base.join("repo/CLAUDE.local.md"), base.join("repo/crate/CLAUDE.md")]);
        // Without a project root DSH reads the cwd only.
        let lone = base.join("lone");
        std::fs::create_dir_all(&lone).unwrap();
        std::fs::write(lone.join("AGENTS.md"), "x").unwrap();
        assert_eq!(dsh_project_instruction_files(&lone), vec![lone.join("AGENTS.md")]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dsh_stop_control_survives_missing_launch_dependencies() {
        let missing = std::env::temp_dir().join(format!("missing-dsh-{}", uuid::Uuid::new_v4()));
        let mut record = CutexSessionRecord::new(
            format!("cutex.{}", uuid::Uuid::new_v4()), None,
            crate::platform::host::current_host_name(), std::env::temp_dir().to_string_lossy().into_owned(), None,
        ).unwrap();
        record.runtime_backend = CutexSessionRuntimeBackend::Dsh;
        record.dsh_session = Some(DshSession {
            flavor: Default::default(), kimi: None,
            thread_id: uuid::Uuid::new_v4().to_string(), native_session_id: None,
            state_dir: missing.clone(), node: missing.join("node"), adapter: missing.join("server.mjs"),
            dsh_cli: missing.join("bin.js"), patch: missing.join("profile.patch.yml"), install_root: missing,
            deployment_version: PINNED_DSH_VERSION.into(), model: "deepseek-v4-flash".into(),
            reasoning_effort: None, api_key_env: "DEEPSEEK_API_KEY".into(), spike: false,
        });
        require_control(&record).unwrap();
        assert!(require_session(&record).unwrap_err().to_string().contains("runtime file missing"));
    }
}
