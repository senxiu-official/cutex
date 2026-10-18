//! Handler of the optional component `claude-runtime` (components/claude-runtime/README.md,
//! components contract v2).
//!
//! The generic code unpacks the verified archive (adapter source, that platform's vendored
//! wheels, `requirements.lock` with hashes) to `<install base>/components/claude-runtime/<rN>/`
//! (immutable, `receipt.json`) and calls:
//!
//! - `prepare`: a venv in `<dir>/prepared/venv` with host Python >= 3.11 and
//!   `pip install --no-index --find-links wheels --require-hashes -r requirements.lock`, then an
//!   import probe. Idempotent; an unfinished venv is rebuilt. Nothing is downloaded.
//! - `select`: find the user-installed official `claude` CLI (cutex never ships or downloads
//!   it), the TUI binary (`codex` of the selected Codex runtime) and the default login home (kept
//!   from the previously selected deployment, else `<cutex home>/claude-home`), write
//!   `<dir>/prepared/deployment.json` and select it through the same code path as
//!   `cutex human claude install <file>` (`ClaudeDeployment::install`). That changes the runtime
//!   of NEW Claude sessions only: every session keeps the deployment it was created with
//!   (`runtime/claude_pin.rs`); nothing here repins. A missing CLI is `needsUser` (not selected);
//!   a default account that is not logged in is `needsUser` (selected).
//! - `deselect`: unselect for new sessions; files stay (`cutex component prune`).
//! - `probe`: file checks plus the cached account status, never a process.
//! - `pins`: sessions per version. `restart_gate`: always open (no service).
//!
//! Never reads credentials and never sends a model request.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context as _};
use serde_json::{json, Value};

use super::{states, Context, Entry, Handler, Metadata};
use crate::runtime::claude::ClaudeDeployment;
use crate::upgrade::layout::Layout;

pub const NAME: &str = "claude-runtime";
pub const LOCK_FILE: &str = "requirements.lock";
pub const WHEELS_DIR: &str = "wheels";
/// What `prepare` / `select` add to the immutable version dir.
pub const PREPARED_DIR: &str = "prepared";
pub const DEPLOYMENT_FILE: &str = "deployment.json";
pub const INSTALL_LOG: &str = "install.log";
pub const PREPARE_RECEIPT: &str = "prepare.json";
const VENV_DIR: &str = "venv";
/// Written last into the venv: pip and the import probe succeeded.
const VENV_READY: &str = ".cutex-ready";
const MIN_PYTHON: (u64, u64) = (3, 11);
const PIP_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

pub const PYTHON_ENV: &str = "CUTEX_CLAUDE_PYTHON";
pub const CLAUDE_CLI_ENV: &str = "CUTEX_CLAUDE_CLI";
pub const TUI_ENV: &str = "CUTEX_CLAUDE_TUI_BINARY";

/// The user's steps when no usable Python is found (short, for this platform).
pub fn python_steps(venv_module_missing: bool) -> Vec<String> {
    let mut steps = Vec::new();
    if cfg!(windows) {
        steps.push("Install Python 3.11 or newer: `winget install Python.Python.3.13` (or the python.org installer).".into());
    } else if cfg!(target_os = "macos") {
        // /usr/bin/python3 of the Command Line Tools is 3.9: too old.
        steps.push("Install Python 3.11 or newer: the python.org macOS installer, `brew install python@3.13` or `uv python install 3.13`.".into());
    } else if venv_module_missing {
        steps.push("Install Python's venv module: `sudo apt install python3-venv` (Debian / Ubuntu; other distributions: their python3 venv package).".into());
    } else {
        steps.push("Install Python 3.11 or newer with venv: `sudo apt install python3 python3-venv` (Debian 12+ / Ubuntu 24.04+; Ubuntu 22.04: `sudo apt install python3.11 python3.11-venv`) or your distribution's packages.".into());
    }
    steps.push(format!("If that Python is not on the cutex services' PATH, set {PYTHON_ENV}=<path to it> for them."));
    steps.push("Install the Claude runtime again.".into());
    steps
}

/// The user's steps when the official Claude Code CLI is missing.
pub fn claude_cli_steps() -> Vec<String> {
    vec![
        if cfg!(windows) {
            "Install the Claude Code CLI (PowerShell): `irm https://claude.ai/install.ps1 | iex`".into()
        } else {
            "Install the Claude Code CLI: `curl -fsSL https://claude.ai/install.sh | bash`".into()
        },
        format!("Check it with `claude --version`. If it is not on PATH or in {}, set {CLAUDE_CLI_ENV}=<path to claude> for the cutex services.",
            if cfg!(windows) { "%USERPROFILE%\\.local\\bin" } else { "~/.local/bin" }),
        "Install the Claude runtime again (it reuses the prepared venv).".into(),
    ]
}

/// The user's steps when no Claude account is logged in.
pub fn login_steps() -> Vec<String> {
    vec![
        "Log in: `cutex human claude account login default` (runs the official `claude auth login` for cutex's Claude home).".into(),
        "Or use an API key instead: `cutex human claude account add <label> --api --api-key-stdin`.".into(),
    ]
}

/// Overrides of what the handler discovers by itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub python: Option<PathBuf>,
    pub claude_cli: Option<PathBuf>,
    pub tui_binary: Option<PathBuf>,
}

impl Options {
    /// `CUTEX_CLAUDE_PYTHON`, `CUTEX_CLAUDE_CLI`, `CUTEX_CLAUDE_TUI_BINARY`.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        Self { python: var(PYTHON_ENV), claude_cli: var(CLAUDE_CLI_ENV), tui_binary: var(TUI_ENV) }
    }
}

pub struct ClaudeRuntime {
    metadata: Metadata,
    /// Read when a step runs (`None` = [`Options::from_env`]); tests set it.
    pub options: Option<Options>,
}

impl Default for ClaudeRuntime {
    fn default() -> Self {
        Self { metadata: Metadata::parse(include_str!("../../components/claude-runtime/component.json")), options: None }
    }
}

impl ClaudeRuntime {
    fn options(&self) -> Options {
        self.options.clone().unwrap_or_else(Options::from_env)
    }
}

/// Something only the user can do; `select` reports it as `needsUser`.
#[derive(Debug)]
pub struct NeedsUser {
    pub code: &'static str,
    pub message: String,
    pub steps: Vec<String>,
}

impl std::fmt::Display for NeedsUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for NeedsUser {}

impl NeedsUser {
    fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message, "steps": self.steps})
    }
}

fn needs_user(code: &'static str, message: impl Into<String>, steps: Vec<String>) -> anyhow::Error {
    anyhow::Error::new(NeedsUser { code, message: message.into(), steps })
}

/// The last user step `prepare` / `select` stopped at (`prepared/needs-user.json`), for `probe`.
const NEEDS_USER_FILE: &str = "needs-user.json";

fn record_needs_user(dir: &Path, step: Option<&NeedsUser>) {
    let path = prepared_dir(dir).join(NEEDS_USER_FILE);
    match step {
        Some(step) => {
            let _ = std::fs::create_dir_all(prepared_dir(dir));
            let _ = std::fs::write(&path, serde_json::to_vec_pretty(&step.to_json()).unwrap_or_default());
        }
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `Ok(value)` with `needsUser` for a user step (recorded for `probe`), else the error.
fn user_step_or(dir: &Path, error: anyhow::Error, value: impl FnOnce(Value) -> Value) -> anyhow::Result<Value> {
    match error.downcast_ref::<NeedsUser>() {
        Some(step) => {
            record_needs_user(dir, Some(step));
            Ok(value(step.to_json()))
        }
        None => Err(error),
    }
}

// ---------------------------------------------------------------- layout

pub fn prepared_dir(dir: &Path) -> PathBuf {
    dir.join(PREPARED_DIR)
}

pub fn venv_python(dir: &Path) -> PathBuf {
    let venv = prepared_dir(dir).join(VENV_DIR);
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

fn venv_ready(dir: &Path) -> bool {
    venv_python(dir).is_file() && prepared_dir(dir).join(VENV_DIR).join(VENV_READY).is_file()
}

/// The files the archive must contain.
pub const REQUIRED_FILES: &[&str] =
    &["src/cute_claude/__main__.py", "src/cute_claude/features.json", "scripts/connect_tui.py", LOCK_FILE];

pub fn check_layout(dir: &Path) -> anyhow::Result<()> {
    ensure!(dir.is_absolute(), "claude-runtime directory must be absolute: {}", dir.display());
    for file in REQUIRED_FILES {
        ensure!(dir.join(file).is_file(), "claude-runtime at {} lacks {file}", dir.display());
    }
    ensure!(dir.join(WHEELS_DIR).is_dir(), "claude-runtime at {} lacks {WHEELS_DIR}/", dir.display());
    Ok(())
}

/// The selected deployment file, unvalidated (a broken one still names its runtime).
fn selected_raw() -> Option<Value> {
    let path = crate::config::paths::runtime_dir().ok()?.join("claude").join(DEPLOYMENT_FILE);
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

fn selected_is(dir: &Path) -> bool {
    selected_raw().and_then(|v| v["adapter_source"].as_str().map(PathBuf::from)).is_some_and(|source| same_path(dir, &source))
}

// ---------------------------------------------------------------- handler

impl Handler for ClaudeRuntime {
    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn required_files(&self) -> &'static [&'static str] {
        REQUIRED_FILES
    }

    /// `source` = the cutex checkout or `components/claude-runtime`: the shipped source files are
    /// copied and that platform's wheels vendored (`scripts/vendor_wheels.py`, needs PyPI) exactly
    /// as the release build does.
    fn from_source(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        let dir = [source.to_path_buf(), source.join("components").join(NAME)]
            .into_iter()
            .find(|dir| {
                std::fs::read_to_string(dir.join("component.json")).is_ok_and(|text| text.contains(&format!("\"{NAME}\"")))
            })
            .with_context(|| format!("{} is neither a cutex checkout nor components/{NAME}", source.display()))?;
        copy_source(&dir, dest)?;
        let python = find_python(&self.options())?;
        let mut command = Command::new(&python.path);
        command
            .arg(dir.join("scripts").join("vendor_wheels.py"))
            .args(["--platform", &crate::upgrade::manifest::platform_key(), "--requirements"])
            .arg(dir.join("requirements-runtime.txt"))
            .arg("--out")
            .arg(dest);
        let log = dest.join("vendor.log");
        let out = run_logged(command, &log, PIP_TIMEOUT)?;
        let _ = std::fs::remove_file(&log);
        ensure!(out.success, "vendoring the Claude runtime's wheels failed ({}; needs PyPI): {}", out.status, out.tail.trim());
        Ok(())
    }

    fn external(&self, _layout: &Layout) -> bool {
        false
    }

    /// A hand-deployed host selects its Claude runtime in
    /// `runtime/claude/deployment.json` (the deployment kits' stage script):
    /// usable when that deployment validates.
    fn configured_by_hand(&self) -> bool {
        ClaudeDeployment::selected().and_then(|deployment| deployment.validate()).is_ok()
    }

    fn prepare(&self, ctx: &Context) -> anyhow::Result<Value> {
        check_layout(&ctx.dir)?;
        if ctx.dry_run {
            return Ok(json!({"venv": prepared_dir(&ctx.dir).join(VENV_DIR), "ready": venv_ready(&ctx.dir)}));
        }
        // Before anything runs from it: every archive file still matches receipt.json
        // (the venv is built from these wheels and this lock).
        let receipt = super::read_receipt(&ctx.dir).with_context(|| format!("{} has no receipt", ctx.dir.display()))?;
        for file in receipt.files.keys() {
            super::verify_file(&ctx.dir, file)?;
        }
        match prepare_venv(&ctx.dir, &self.options()) {
            Ok(value) => Ok(value),
            Err(error) => user_step_or(&ctx.dir, error, |step| json!({"prepared": false, "needsUser": step})),
        }
    }

    fn select(&self, ctx: &Context) -> anyhow::Result<Value> {
        if ctx.dry_run {
            return Ok(json!({"selected": false, "dryRun": true}));
        }
        select_dir(&ctx.dir, &self.options())
    }

    fn deselect(&self, ctx: &Context, _force: bool) -> anyhow::Result<Vec<String>> {
        let path = crate::config::paths::runtime_dir()?.join("claude").join(DEPLOYMENT_FILE);
        if !selected_is(&ctx.dir) {
            return Ok(vec![]);
        }
        let action = format!("unselect {} for new Claude sessions (existing sessions keep their runtime)", ctx.dir.display());
        if !ctx.dry_run {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        Ok(vec![action])
    }

    fn probe(&self, layout: &Layout, entry: &Entry) -> Value {
        probe_dir(&super::version_dir(layout, NAME, &entry.selected))
    }

    fn pins(&self, layout: &Layout) -> BTreeMap<String, usize> {
        pins_under(&super::components_dir(layout).join(NAME))
    }

    fn restart_gate(&self, _layout: &Layout) -> Result<(), String> {
        Ok(())
    }
}

/// `prepare` without the trait (tests, `--from-source` builds).
pub fn prepare_venv(dir: &Path, options: &Options) -> anyhow::Result<Value> {
    check_layout(dir)?;
    let prepared = prepared_dir(dir);
    std::fs::create_dir_all(&prepared)?;
    let log_path = prepared.join(INSTALL_LOG);
    let venv = prepared.join(VENV_DIR);
    let python = venv_python(dir);
    if venv_ready(dir) {
        let receipt: Value = std::fs::read(prepared.join(PREPARE_RECEIPT))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or(Value::Null);
        return Ok(json!({"python": python, "venvCreated": false, "receipt": receipt}));
    }
    if venv.exists() {
        log(&log_path, &format!("removing the unfinished venv {}", venv.display()));
        std::fs::remove_dir_all(&venv).with_context(|| format!("remove unfinished venv {}", venv.display()))?;
    }
    let base = find_python(options)?;
    log(&log_path, &format!("base Python {} ({})", base.path.display(), base.version_text()));
    let mut command = Command::new(&base.path);
    command.args(["-m", "venv"]).arg(&venv);
    let out = run_logged(command, &log_path, PROBE_TIMEOUT * 5)?;
    if !out.success {
        if out.tail.contains("ensurepip") || out.tail.contains("No module named venv") {
            let _ = std::fs::remove_dir_all(&venv);
            return Err(needs_user(
                "python_venv_missing",
                format!("Python {} at {} has no working venv module", base.version_text(), base.path.display()),
                python_steps(true),
            ));
        }
        bail!("creating the venv with {} failed ({}). See {}", base.path.display(), out.status, log_path.display());
    }
    ensure!(python.is_file(), "the venv has no {}", python.display());
    let mut pip = Command::new(&python);
    pip.args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input", "--no-index", "--require-hashes", "--find-links"])
        .arg(dir.join(WHEELS_DIR))
        .arg("-r")
        .arg(dir.join(LOCK_FILE));
    let out = run_logged(pip, &log_path, PIP_TIMEOUT)?;
    if !out.success {
        let supported = std::fs::read_to_string(dir.join(LOCK_FILE))
            .ok()
            .and_then(|text| text.lines().find_map(|l| l.strip_prefix("# python-versions: ").map(str::to_string)))
            .unwrap_or_default();
        bail!(
            "installing the Claude runtime's dependencies from its vendored wheels failed with Python {} ({}). \
             This runtime ships wheels for Python {supported}; set CUTEX_CLAUDE_PYTHON to one of those. See {}",
            base.version_text(),
            out.status,
            log_path.display()
        );
    }
    let mut check = Command::new(&python);
    check
        .args([
            "-c",
            "import json, claude_agent_sdk, cute_claude, cute_claude.__main__; \
             print(json.dumps({'sdk': claude_agent_sdk.__version__, 'adapterVersion': cute_claude.__version__}))",
        ])
        .env("PYTHONPATH", dir.join("src"));
    let probe = capture(check, PROBE_TIMEOUT)?;
    ensure!(probe.success, "the new venv cannot import the adapter: {}", probe.tail.trim());
    let versions: Value = serde_json::from_str(probe.stdout.trim()).unwrap_or(Value::Null);
    let receipt = json!({
        "component": NAME,
        "preparedAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "basePython": base.path,
        "basePythonVersion": base.version_text(),
        "sdk": versions["sdk"],
        "adapterVersion": versions["adapterVersion"],
    });
    crate::config::atomic::write_private_pretty_json_atomic(&prepared.join(PREPARE_RECEIPT), &receipt, "Claude runtime receipt")?;
    std::fs::write(venv.join(VENV_READY), b"ok\n")?;
    record_needs_user(dir, None);
    Ok(json!({"python": python, "venvCreated": true, "receipt": receipt}))
}

/// `select` without the trait: `{selected, deployment?, needsUser?}`.
pub fn select_dir(dir: &Path, options: &Options) -> anyhow::Result<Value> {
    check_layout(dir)?;
    if !venv_ready(dir) {
        // prepare stopped at a user step (Python): nothing to select yet.
        if let Some(step) = recorded_needs_user(dir) {
            return Ok(json!({"selected": false, "needsUser": step}));
        }
        anyhow::bail!("the Claude runtime at {} is not prepared", dir.display());
    }
    let deployment = match deployment_for(dir, options) {
        Ok(deployment) => deployment,
        Err(error) => return user_step_or(dir, error, |step| json!({"selected": false, "needsUser": step})),
    };
    record_needs_user(dir, None);
    crate::config::atomic::write_private_pretty_json_atomic(&prepared_dir(dir).join(DEPLOYMENT_FILE), &deployment.0, "Claude deployment")?;
    // The same selection `cutex human claude install <file>` makes: new sessions only.
    deployment.0.install()?;
    let mut value = json!({"selected": true, "deployment": deployment.0, "claudeCliVersion": deployment.1});
    if let Some(step) = login_needed(true) {
        value["needsUser"] = step.to_json();
    }
    Ok(value)
}

fn recorded_needs_user(dir: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(prepared_dir(dir).join(NEEDS_USER_FILE)).ok()?).ok()
}

fn deployment_for(dir: &Path, options: &Options) -> anyhow::Result<(ClaudeDeployment, String)> {
    let (claude_cli, version) = find_claude_cli(options)?;
    let tui_binary = find_tui_binary(options)?;
    let claude_home = default_claude_home()?;
    let deployment = ClaudeDeployment { python: venv_python(dir), adapter_source: dir.to_path_buf(), claude_cli, claude_home, tui_binary };
    deployment.validate()?;
    Ok((deployment, version))
}

/// `needsUser` when no Claude account can run a turn: the default account
/// is not logged in and no other account exists. `check` runs the official
/// `claude auth status` once (after a select); otherwise the cached status is
/// used and an unknown status counts as fine.
fn login_needed(check: bool) -> Option<NeedsUser> {
    let registry = crate::runtime::claude_accounts::load().ok();
    if registry.as_ref().is_some_and(|r| !r.accounts.is_empty()) {
        return None;
    }
    let status = if check {
        crate::runtime::claude_accounts::check_status(crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID).ok()
    } else {
        registry.and_then(|r| r.default_status)
    };
    let status = status?;
    if status.logged_in {
        return None;
    }
    Some(NeedsUser {
        code: "claude_login_required",
        message: "the Claude runtime is installed, but no Claude account is logged in yet".into(),
        steps: login_steps(),
    })
}

/// `{state, steps?, needsUser?, …}`: `steps` (top level) are the short user steps of a
/// `needsUser` state. File checks and the cached account status only, no process.
pub fn probe_dir(dir: &Path) -> Value {
    let needs = |step: Value, extra: Value| {
        let mut value = json!({"state": states::NEEDS_USER, "steps": step["steps"].clone(), "needsUser": step, "dir": dir});
        if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            value.extend(extra.clone());
        }
        value
    };
    if let Some(step) = recorded_needs_user(dir) {
        return needs(step, json!({}));
    }
    if !venv_ready(dir) {
        return json!({"state": states::DEGRADED, "problem": "the venv is not prepared; install again", "dir": dir});
    }
    if !selected_is(dir) {
        let problem = if selected_raw().is_some() {
            "another Claude runtime is selected for new sessions"
        } else {
            "not selected for new sessions; install again"
        };
        return json!({"state": states::DEGRADED, "problem": problem, "dir": dir});
    }
    if let Err(error) = ClaudeDeployment::selected() {
        return json!({"state": states::DEGRADED, "problem": format!("{error:#}"), "dir": dir});
    }
    let selected = selected_raw().unwrap_or(Value::Null);
    let detail = json!({"claudeCli": selected["claude_cli"], "claudeHome": selected["claude_home"], "dir": dir});
    match login_needed(false) {
        Some(step) => needs(step.to_json(), json!({"selected": detail})),
        None => json!({"state": states::READY, "selected": detail}),
    }
}

/// Version -> sessions (not retired) whose Claude deployment is that version
/// dir under `root` (`<base>/components/claude-runtime`).
pub fn pins_under(root: &Path) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let Ok(store) = crate::session::store::load_cutex_session_store() else { return out };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for record in store.sessions.values().filter(|record| !record.is_retired()) {
        let Some(session) = &record.claude_session else { continue };
        let source = &session.deployment.adapter_source;
        let source = source.canonicalize().unwrap_or_else(|_| source.clone());
        if source.parent() == Some(root.as_path()) {
            if let Some(version) = source.file_name().and_then(|n| n.to_str()) {
                *out.entry(version.to_string()).or_insert(0) += 1;
            }
        }
    }
    out
}

fn copy_source(dir: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest.join("scripts"))?;
    for file in ["component.json", "README.md", "LICENSE", "pyproject.toml"] {
        std::fs::copy(dir.join(file), dest.join(file)).with_context(|| format!("copy {file}"))?;
    }
    std::fs::copy(dir.join("scripts/connect_tui.py"), dest.join("scripts/connect_tui.py"))?;
    fn walk(from: &Path, to: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_text = name.to_string_lossy();
            if name_text == "__pycache__" || name_text.ends_with(".pyc") || name_text.ends_with(".egg-info") {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(&entry.path(), &to.join(&name))?;
            } else if kind.is_file() {
                std::fs::copy(entry.path(), to.join(&name))?;
            }
        }
        Ok(())
    }
    walk(&dir.join("src"), &dest.join("src"))
}

// ---------------------------------------------------------------- Python

#[derive(Clone, Debug)]
pub struct Python {
    pub path: PathBuf,
    pub version: (u64, u64, u64),
}

impl Python {
    fn version_text(&self) -> String {
        format!("{}.{}.{}", self.version.0, self.version.1, self.version.2)
    }
}

/// `CUTEX_CLAUDE_PYTHON`, else the first Python 3.11+ of: Windows `py -3`,
/// `python`, `python3`; elsewhere `python3.13`, `python3.12`, `python3.11`,
/// `python3.14`, `python3`. Store stubs (`WindowsApps`) are skipped.
pub fn find_python(options: &Options) -> anyhow::Result<Python> {
    let mut candidates: Vec<(PathBuf, Vec<&str>)> = Vec::new();
    if let Some(path) = &options.python {
        candidates.push((path.clone(), vec![]));
    } else if cfg!(windows) {
        candidates.push(("py".into(), vec!["-3"]));
        candidates.push(("python".into(), vec![]));
        candidates.push(("python3".into(), vec![]));
    } else {
        for name in ["python3.13", "python3.12", "python3.11", "python3.14", "python3"] {
            candidates.push((name.into(), vec![]));
        }
    }
    let mut seen = Vec::new();
    for (exe, prefix) in &candidates {
        let mut command = Command::new(exe);
        command.args(prefix).args(["-c", "import json, sys; print(json.dumps({'v': list(sys.version_info[:3]), 'exe': sys.executable}))"]);
        let Ok(out) = capture(command, Duration::from_secs(20)) else { continue };
        if !out.success {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(out.stdout.trim()) else { continue };
        let v: Vec<u64> = value["v"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        let Some(path) = value["exe"].as_str().filter(|p| !p.is_empty()).map(PathBuf::from) else { continue };
        if v.len() != 3 || path.to_string_lossy().contains("WindowsApps") {
            continue;
        }
        let python = Python { path, version: (v[0], v[1], v[2]) };
        if (python.version.0, python.version.1) >= MIN_PYTHON {
            return Ok(python);
        }
        seen.push(format!("{} {}", python.path.display(), python.version_text()));
    }
    let found = if seen.is_empty() { "none found".to_string() } else { format!("found only {}", seen.join(", ")) };
    Err(needs_user(
        if seen.is_empty() { "python_missing" } else { "python_too_old" },
        format!("the Claude runtime needs Python {}.{} or newer ({found})", MIN_PYTHON.0, MIN_PYTHON.1),
        python_steps(false),
    ))
}

// ---------------------------------------------------------------- claude CLI

/// Where `claude` may be: PATH first, then the official installers' and npm's default places.
pub fn claude_cli_candidates(home: Option<&Path>, path_var: Option<&std::ffi::OsStr>, appdata: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            for name in names {
                out.push(dir.join(name));
            }
        }
    }
    if let Some(home) = home {
        if cfg!(windows) {
            out.push(home.join(".local").join("bin").join("claude.exe"));
        } else {
            out.push(home.join(".local/bin/claude"));
            out.push(home.join(".claude/local/claude"));
            out.push(home.join(".npm-global/bin/claude"));
        }
    }
    if cfg!(windows) {
        if let Some(appdata) = appdata {
            out.push(appdata.join("npm").join("claude.cmd"));
        }
    } else {
        out.push("/usr/local/bin/claude".into());
        out.push("/usr/bin/claude".into());
    }
    let mut unique = Vec::new();
    for path in out {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

/// The user's Claude Code (`CUTEX_CLAUDE_CLI`, else the candidates); it must answer `--version`.
pub fn find_claude_cli(options: &Options) -> anyhow::Result<(PathBuf, String)> {
    let candidates = match &options.claude_cli {
        Some(path) => vec![path.clone()],
        None => claude_cli_candidates(
            crate::config::paths::home_dir().as_deref(),
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("APPDATA").map(PathBuf::from).as_deref(),
        ),
    };
    let mut broken = Vec::new();
    for path in candidates {
        if !path.is_absolute() || !path.is_file() {
            continue;
        }
        let mut command = Command::new(&path);
        command.arg("--version");
        match capture(command, Duration::from_secs(30)) {
            Ok(out) if out.success && !out.stdout.trim().is_empty() => {
                return Ok((path, out.stdout.trim().lines().next().unwrap_or_default().to_string()));
            }
            Ok(out) => broken.push(format!("{} ({}: {})", path.display(), out.status, out.tail.trim())),
            Err(error) => broken.push(format!("{} ({error:#})", path.display())),
        }
    }
    let detail = if broken.is_empty() { String::new() } else { format!(" Found but not working: {}.", broken.join("; ")) };
    Err(needs_user("claude_cli_missing", format!("the official Claude Code CLI (`claude`) was not found.{detail}"), claude_cli_steps()))
}

// ---------------------------------------------------------------- TUI, home

/// `CUTEX_CLAUDE_TUI_BINARY`, else the `cli` (`codex`) of the selected Codex runtime bundle.
/// Only `cutex human claude attach` runs it; the deployment format requires the file.
pub fn find_tui_binary(options: &Options) -> anyhow::Result<PathBuf> {
    if let Some(path) = &options.tui_binary {
        ensure!(path.is_absolute() && path.is_file(), "TUI binary missing: {}", path.display());
        return Ok(path.clone());
    }
    crate::launch::local_deployment::LocalDeployment::selected()
        .ok()
        .flatten()
        .and_then(|deployment| std::fs::read(&deployment.bundle_manifest).ok())
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|bundle| bundle["cli"]["path"].as_str().map(PathBuf::from))
        .filter(|path| path.is_absolute() && path.is_file())
        .ok_or_else(|| {
            needs_user(
                "codex_runtime_missing",
                "no Codex runtime with a `codex` binary is selected",
                vec!["Select the Codex runtime first (`cutex human install-runtime`, install guide), then install the Claude runtime again.".into()],
            )
        })
}

/// The default account's login home: that of the deployment selected before (an update keeps
/// the login), else `<cutex home>/claude-home`.
pub fn default_claude_home() -> anyhow::Result<PathBuf> {
    if let Some(home) = selected_raw()
        .and_then(|value| value["claude_home"].as_str().map(PathBuf::from))
        .filter(|home| home.is_absolute() && home.is_dir())
    {
        return Ok(home);
    }
    let home = crate::config::paths::config_dir()?.join("claude-home");
    crate::profiles::materialize::create_private_dir_all(&home)?;
    Ok(home)
}

// ---------------------------------------------------------------- processes

struct Output {
    success: bool,
    status: String,
    stdout: String,
    /// Last few KB of the output, for messages.
    tail: String,
}

#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}
#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

fn wait(child: &mut std::process::Child, timeout: Duration) -> anyhow::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {} s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn tail_of(text: &str) -> String {
    let start = text.len().saturating_sub(4096);
    let start = (start..text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
    text[start..].to_string()
}

fn capture(mut command: Command, timeout: Duration) -> anyhow::Result<Output> {
    hide_window(&mut command);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout_pipe = child.stdout.take().context("stdout")?;
    let mut stderr_pipe = child.stderr.take().context("stderr")?;
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout_pipe.read_to_string(&mut s);
        s
    });
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });
    let status = wait(&mut child, timeout)?;
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Output { success: status.success(), status: status.to_string(), tail: tail_of(&format!("{stdout}{stderr}")), stdout })
}

fn log(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let _ = writeln!(file, "[{now}] {line}");
    }
}

fn run_logged(mut command: Command, log_path: &Path, timeout: Duration) -> anyhow::Result<Output> {
    log(log_path, &format!("run {:?}", command));
    let file = std::fs::OpenOptions::new().create(true).append(true).open(log_path)?;
    let start = file.metadata()?.len();
    hide_window(&mut command);
    command.stdin(Stdio::null()).stdout(file.try_clone()?).stderr(file);
    let mut child = command.spawn().with_context(|| format!("start {:?}", command.get_program()))?;
    let status = wait(&mut child, timeout)?;
    let mut tail = String::new();
    if let Ok(mut file) = std::fs::File::open(log_path) {
        use std::io::{Seek, SeekFrom};
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(start.max(len.saturating_sub(8192))));
        let mut bytes = Vec::new();
        let _ = file.read_to_end(&mut bytes);
        tail = String::from_utf8_lossy(&bytes).into_owned();
    }
    log(log_path, &format!("exit {status}"));
    Ok(Output { success: status.success(), status: status.to_string(), stdout: String::new(), tail })
}

#[cfg(test)]
#[path = "claude_runtime_tests.rs"]
mod tests;
