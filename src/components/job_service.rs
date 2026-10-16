//! Component `job-service` (components/job-service, contract v2): cutex runs
//! the Job Service as a systemd user unit and selects it for new agents.
//!
//! - **Versions** live side by side in
//!   `<install base>/components/job-service/<version>/cutex-job-service`; the
//!   unit runs that exact path (checked against `receipt.json` before every
//!   start). Agents' `job_mcp` descriptors pin adapter path + sha256, so old
//!   versions stay until `cutex component prune` finds them unpinned.
//! - **Layout** (fixed; Management's Jobs bridge reads the same paths):
//!   `~/.cutex/runtime/job-service/v1/` = state root, `job-service.sock`,
//!   `api.token`, `grant.key` (32 random bytes each, 0600 in a 0700 dir,
//!   created once and kept). Completion lane `--completion-v2
//!   http://127.0.0.1:<bus port>` with the Agent Bus's dedicated
//!   `runtime/task-service/job-service-completion.token` (created in the bus's
//!   format when the bus has not made it yet).
//! - **NCI**: always `--external-source nci --external-notify-file
//!   ~/.cutex/runtime/inbound-mail/notify-address-v1.json`; the Job Service
//!   reads the file on every `prepare_remote`, so the argv is valid before
//!   inbound mail is configured (agents get `mailNotConfigured` until then).
//! - **Launchers**: the selected runtime's `codex` (the descriptor's
//!   launcher), every installed native `codex`, and every launcher a session
//!   receipt or the current selection still pins.
//! - **Restart gate** (contract §0.2): the daemon restarts only when its
//!   version or launcher set changed and no local job is running / launch
//!   pending and no runtime launch is in flight. Otherwise the new version is
//!   selected in `state.json`, the descriptor keeps naming the running daemon,
//!   and the state is `restartPending` until Management's watcher
//!   ([`super::settle_pending`]) finds the gate open.
//! - **External**: a live service on the socket (or holding the state flock)
//!   that is not one of this install's versions is reported `external` and
//!   never replaced. Linux only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context as _};
use serde_json::{json, Value};

use super::{states, Context, Entry, Handler, Metadata};
use crate::launch::job_mcp::JobMcpDescriptor;
use crate::launch::stock::VerifiedFile;
use crate::upgrade::layout::Layout;
use crate::upgrade::service::{ServiceNames, UNIT_MARKER};
use crate::upgrade::stage::{upgrade_error, UpgradeError};

pub const NAME: &str = "job-service";
pub const JOB_EXE: &str = "cutex-job-service";
pub const JOB_UNIT: &str = "cutex-job-service.service";
pub const NCI_SOURCE: &str = "nci";
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// Local job states a daemon restart turns into interrupted / launch_unknown.
const ACTIVE_STATES: [&str; 2] = ["launch_pending", "running"];

type ReadyFn = fn(&Plan, Duration) -> anyhow::Result<JobMcpDescriptor>;

pub struct JobService {
    metadata: Metadata,
    /// `None` = [`default_unit_dir`], started with systemctl; a directory =
    /// only written there (tests, staging), no systemctl, no descriptor.
    pub unit_dir: Option<PathBuf>,
    /// Resolves the running daemon's descriptor after a (re)start (tests substitute it).
    pub ready: ReadyFn,
    pub ready_timeout: Duration,
}

impl Default for JobService {
    fn default() -> Self {
        Self {
            metadata: Metadata::parse(include_str!("../../components/job-service/component.json")),
            unit_dir: None,
            ready: wait_ready,
            ready_timeout: READY_TIMEOUT,
        }
    }
}

/// The fixed private paths (all under `~/.cutex/runtime`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub api_token: PathBuf,
    pub grant_key: PathBuf,
    pub state: PathBuf,
    pub completion_token: PathBuf,
    pub notify_file: PathBuf,
}

impl Paths {
    pub fn under(runtime: &Path) -> Self {
        let root = runtime.join("job-service/v1");
        Self {
            socket: root.join("job-service.sock"),
            api_token: root.join("api.token"),
            grant_key: root.join("grant.key"),
            state: root.join("state.json"),
            completion_token: runtime.join("task-service/job-service-completion.token"),
            notify_file: runtime.join("inbound-mail/notify-address-v1.json"),
            root,
        }
    }

    pub fn current() -> anyhow::Result<Self> {
        Ok(Self::under(&crate::config::paths::runtime_dir()?))
    }
}

/// `cutex-job-service.service`, or (debug builds with
/// `CUTEX_SERVICE_TEST_NAME`) `cutex-<label>-job-service.service`.
pub fn unit_name() -> anyhow::Result<String> {
    let names = ServiceNames::for_install()?;
    Ok(match names.bus_unit.strip_suffix("-agent-bus.service") {
        Some(prefix) if prefix != "cutex" => format!("{prefix}-job-service.service"),
        _ => JOB_UNIT.to_string(),
    })
}

/// Debug drills only (with `CUTEX_SERVICE_TEST_NAME`): the systemd user unit
/// directory for component units (e.g. `$XDG_RUNTIME_DIR/systemd/user`), so a
/// private test home's drill-named unit is one the user manager reads.
pub const TEST_UNIT_DIR_ENV: &str = "CUTEX_COMPONENT_TEST_UNIT_DIR";

/// The systemd user unit directory. Unit tests always stay in the test home.
pub fn default_unit_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return crate::config::paths::home_dir().map(|home| home.join(".config/systemd/user"));
    }
    if cfg!(debug_assertions)
        && std::env::var(crate::upgrade::service::TEST_SERVICE_NAME_ENV).is_ok_and(|v| !v.is_empty())
    {
        if let Some(dir) = std::env::var_os(TEST_UNIT_DIR_ENV).map(PathBuf::from).filter(|p| p.is_absolute()) {
            return Some(dir);
        }
    }
    crate::upgrade::service::default_unit_dir()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Everything an install would write.
#[derive(Clone, Debug)]
pub struct Plan {
    pub binary: PathBuf,
    pub launcher: VerifiedFile,
    pub allowed_launchers: Vec<PathBuf>,
    pub bus_url: String,
    pub paths: Paths,
    pub unit_name: String,
    pub unit_dir: PathBuf,
    pub start: bool,
    pub argv: Vec<String>,
    pub unit_text: String,
    native_home: PathBuf,
    bundle_manifest: PathBuf,
}

impl Plan {
    pub fn unit_path(&self) -> PathBuf {
        self.unit_dir.join(&self.unit_name)
    }
}

fn verified(path: &Path) -> anyhow::Result<VerifiedFile> {
    let path = path.canonicalize().with_context(|| format!("resolve {}", path.display()))?;
    Ok(VerifiedFile { sha256: crate::agent_management::file_sha256(&path)?, path })
}

fn allowed_launchers(layout: &Layout, launcher: &Path, pinned: &[PathBuf]) -> Vec<PathBuf> {
    let mut list = vec![launcher.to_path_buf()];
    let mut natives: Vec<PathBuf> = std::fs::read_dir(layout.natives_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join(crate::upgrade::stage::NATIVE_CLI))
        .filter(|path| path.is_file())
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    natives.sort();
    list.extend(natives);
    let mut pinned: Vec<PathBuf> = pinned.iter().filter(|p| p.is_file()).filter_map(|p| p.canonicalize().ok()).collect();
    pinned.sort();
    list.extend(pinned);
    let mut seen = std::collections::BTreeSet::new();
    list.retain(|path| seen.insert(path.clone()));
    list
}

fn argv(binary: &Path, launcher: &Path, allowed: &[PathBuf], paths: &Paths, bus_url: &str) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut argv = vec![
        s(binary),
        "serve".into(),
        s(&paths.root),
        s(&paths.socket),
        s(&paths.api_token),
        s(&paths.grant_key),
        s(launcher),
    ];
    for extra in allowed.iter().filter(|p| p.as_path() != launcher) {
        argv.push("--allow-launcher".into());
        argv.push(s(extra));
    }
    argv.extend(["--completion-v2".into(), bus_url.to_string(), s(&paths.completion_token)]);
    argv.extend(["--external-source".into(), NCI_SOURCE.into(), "--external-notify-file".into(), s(&paths.notify_file)]);
    argv
}

/// `WorkingDirectory=` takes the path as is (no quotes; spaces are fine);
/// `%` is a specifier in both fields.
pub fn unit_text(argv: &[String], working_dir: &Path, after: &str) -> String {
    let exec = argv.iter().map(|a| crate::upgrade::service::systemd_quote(a).replace('%', "%%")).collect::<Vec<_>>().join(" ");
    format!(
        "{UNIT_MARKER}\n[Unit]\nDescription=cutex Job Service\nAfter={after}\n\n[Service]\nType=simple\n\
         WorkingDirectory={}\nExecStart={exec}\nUMask=0077\nRestart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=default.target\n",
        working_dir.display().to_string().replace('%', "%%"),
    )
}

fn unit_is_ours(path: &Path) -> Option<bool> {
    std::fs::read_to_string(path).ok().map(|text| text.starts_with(UNIT_MARKER))
}

fn ensure_linux() -> anyhow::Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(upgrade_error(501, "unsupported_platform", "the Job Service runs on Linux hosts only"));
    }
    Ok(())
}


impl JobService {
    fn unit_dir(&self) -> anyhow::Result<(PathBuf, bool)> {
        Ok(match &self.unit_dir {
            Some(dir) => (dir.clone(), false),
            None => (default_unit_dir().context("no home directory for systemd user units")?, true),
        })
    }

    fn unit_path(&self) -> anyhow::Result<(PathBuf, String, bool)> {
        let (dir, start) = self.unit_dir()?;
        let name = unit_name()?;
        Ok((dir.join(&name), name, start))
    }

    /// The plan for the version in `dir` (binary checked against its receipt).
    pub fn plan(&self, layout: &Layout, dir: &Path) -> anyhow::Result<Plan> {
        self.plan_for(layout, dir, false)
    }

    /// `dry_run`: a version not downloaded yet is planned at its future path.
    fn plan_for(&self, layout: &Layout, dir: &Path, dry_run: bool) -> anyhow::Result<Plan> {
        ensure_linux()?;
        let record = layout
            .read_install()
            .map_err(|_| upgrade_error(409, "unmanaged_install", "not a managed cutex install (no install.json)"))?;
        let binary = if dry_run && !dir.join(super::RECEIPT_FILE).exists() {
            dir.join(JOB_EXE)
        } else {
            super::verify_file(dir, JOB_EXE)?.canonicalize()?
        };
        let selected = crate::launch::local_deployment::LocalDeployment::selected()?.ok_or_else(|| {
            upgrade_error(409, "no_runtime_selected", "no runtime is selected for new agents (`cutex human install-runtime`)")
        })?;
        let bundle: crate::launch::stock::StockBundle = serde_json::from_slice(
            &std::fs::read(&selected.bundle_manifest).with_context(|| format!("read {}", selected.bundle_manifest.display()))?,
        )
        .context("selected runtime bundle unreadable")?;
        let cli = bundle.cli.ok_or_else(|| {
            upgrade_error(409, "no_launcher", "the selected runtime has no `codex` CLI, which the Job Service needs as its sandbox launcher")
        })?;
        let launcher = verified(&cli.path)?;
        let mut pinned = pinned_descriptors().into_iter().map(|(_, d)| d.launcher.path).collect::<Vec<_>>();
        pinned.extend(selected.job_mcp.as_ref().map(|d| d.launcher.path.clone()));
        let allowed = allowed_launchers(layout, &launcher.path, &pinned);
        let config = crate::config::store::load_codez_config();
        let port = crate::agent_bus::service::agent_bus_port(&config);
        ensure!(port != 0, "no Agent Bus port configured");
        let bus_url = crate::agent_bus::service::agent_bus_base_url(port);
        let paths = Paths::current()?;
        ensure!(
            paths.socket.as_os_str().len() < 104,
            "the Job Service socket path {} is too long for a Unix socket",
            paths.socket.display()
        );
        let (unit_dir, start) = self.unit_dir()?;
        let argv = argv(&binary, &launcher.path, &allowed, &paths, &bus_url);
        let unit_text = unit_text(&argv, &paths.root, &ServiceNames::of(&record).bus_unit);
        Ok(Plan {
            binary,
            launcher,
            allowed_launchers: allowed,
            bus_url,
            paths,
            unit_name: unit_name()?,
            unit_dir,
            start,
            argv,
            unit_text,
            native_home: selected.native_home,
            bundle_manifest: selected.bundle_manifest,
        })
    }
}

/// Latest runtime receipt per session that carries a `job_mcp` descriptor.
fn pinned_descriptors() -> Vec<(String, JobMcpDescriptor)> {
    use crate::agent_management::ExplicitLaunchActionReceipt;
    let Ok(store) = crate::session::store::load_cutex_session_store() else { return Vec::new() };
    let mut latest: std::collections::BTreeMap<String, (u64, JobMcpDescriptor)> = Default::default();
    for receipt in store.explicit_launch_receipts.values() {
        if let ExplicitLaunchActionReceipt::Runtime(r) = receipt {
            if let Some(job) = &r.review.job_mcp {
                let id = r.review.subject.cutex_session_id.as_str().to_string();
                if latest.get(&id).is_none_or(|(g, _)| *g <= r.expected_generation) {
                    latest.insert(id, (r.expected_generation, job.descriptor.clone()));
                }
            }
        }
    }
    latest.into_iter().map(|(id, (_, d))| (id, d)).collect()
}

/// A runtime launch that is between review and ready (its descriptor would
/// go stale under a restart). Older than 10 minutes counts as abandoned.
fn launch_in_flight() -> usize {
    use crate::agent_management::{ExplicitLaunchActionReceipt, StockRuntimeStage};
    let Ok(store) = crate::session::store::load_cutex_session_store() else { return 0 };
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(10);
    store
        .explicit_launch_receipts
        .values()
        .filter(|receipt| match receipt {
            ExplicitLaunchActionReceipt::Runtime(r) => {
                r.review.job_mcp.is_some()
                    && matches!(r.stage, StockRuntimeStage::Prepared | StockRuntimeStage::Claimed | StockRuntimeStage::Spawned)
                    && chrono::DateTime::parse_from_rfc3339(&r.updated_at).is_ok_and(|t| t.with_timezone(&chrono::Utc) > cutoff)
            }
            _ => false,
        })
        .count()
}

/// The version a binary path belongs to (`…/components/job-service/<v>/cutex-job-service`).
fn version_of(layout: &Layout, path: &Path) -> Option<String> {
    let root = super::components_dir(layout).join(NAME).canonicalize().ok()?;
    let rest = path.strip_prefix(&root).ok()?;
    let mut parts = rest.components();
    let version = parts.next()?.as_os_str().to_str()?.to_string();
    (parts.next()?.as_os_str() == JOB_EXE && parts.next().is_none()).then_some(version)
}

/// Processes of this user executing a version's binary (daemon, adapters).
fn mapped_versions(layout: &Layout) -> std::collections::BTreeMap<String, usize> {
    let mut map = std::collections::BTreeMap::new();
    for (_, version, _) in component_processes(layout) {
        *map.entry(version).or_insert(0) += 1;
    }
    map
}

/// `(pid, version, is an agent's MCP adapter)` of this user's processes
/// running one of the installed versions (the daemon, job runners, the
/// `mcp-stdio` adapters agents spawn).
fn component_processes(layout: &Layout) -> Vec<(u32, String, bool)> {
    let mut list = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else { continue };
        let Some(version) = version_of(layout, &exe) else { continue };
        let adapter = std::fs::read(entry.path().join("cmdline"))
            .ok()
            .is_some_and(|cmd| cmd.split(|b| *b == 0).nth(1) == Some(b"mcp-stdio".as_slice()));
        list.push((pid, version, adapter));
    }
    list
}

/// Whether the state flock (`<root>/service.lock`) is held by someone.
fn state_lock_held(paths: &Paths) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let Ok(file) = std::fs::File::open(paths.root.join("service.lock")) else { return false };
        let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
        if !held {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
        return held;
    }
    #[allow(unreachable_code)]
    false
}

#[cfg(target_os = "linux")]
fn socket_peer_exe(paths: &Paths) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    let socket = std::os::unix::net::UnixStream::connect(&paths.socket).ok()?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut peer as *mut libc::ucred).cast(), &mut len)
    } == 0;
    (ok && peer.pid > 0).then(|| std::fs::read_link(format!("/proc/{}/exe", peer.pid)).ok()).flatten()
}

#[cfg(not(target_os = "linux"))]
fn socket_peer_exe(_: &Paths) -> Option<PathBuf> {
    None
}

/// Local jobs a restart would interrupt (external jobs survive restarts).
pub fn active_jobs(paths: &Paths) -> anyhow::Result<Vec<String>> {
    let bytes = match std::fs::read(&paths.state) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read the Job Service state"),
    };
    let state: Value = serde_json::from_slice(&bytes).context("Job Service state unreadable")?;
    let mut ids: Vec<String> = state["jobs"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, job)| job.get("external").is_none_or(Value::is_null))
        .filter(|(_, job)| job["state"].as_str().is_some_and(|s| ACTIVE_STATES.contains(&s)))
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    Ok(ids)
}

fn busy_error(count: usize, what: &str) -> anyhow::Error {
    UpgradeError::boxed(
        409,
        "jobs_running",
        format!("{count} local job(s) are running; {what} would interrupt them. Try again when they have finished"),
        json!({ "name": NAME, "activeJobs": count }),
    )
}

#[cfg(unix)]
fn private_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(meta.is_dir() && meta.uid() == unsafe { libc::geteuid() }, "{} is not a directory owned by this user", path.display());
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Create `path` with `value` once (0600, never replaced); an existing file
/// must be a private regular file.
#[cfg(unix)]
fn private_file_once(path: &Path, value: &[u8]) -> anyhow::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0 && meta.len() >= 32,
                "{} exists but is not a private credential file",
                path.display()
            );
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
            file.write_all(value)?;
            file.sync_all()?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

fn random_secret() -> anyhow::Result<Vec<u8>> {
    use ring::rand::SecureRandom;
    let mut bytes = vec![0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("no system randomness"))?;
    Ok(bytes)
}

#[cfg(unix)]
fn prepare_files(paths: &Paths) -> anyhow::Result<Vec<String>> {
    let mut made = Vec::new();
    private_dir(&paths.root)?;
    for file in [&paths.api_token, &paths.grant_key] {
        if private_file_once(file, &random_secret()?)? {
            made.push(format!("created {}", file.display()));
        }
    }
    private_dir(paths.completion_token.parent().context("completion token parent")?)?;
    let token = format!("cutex-job-service-{}", uuid::Uuid::new_v4());
    if private_file_once(&paths.completion_token, token.as_bytes())? {
        made.push(format!("created {}", paths.completion_token.display()));
    }
    Ok(made)
}

#[cfg(not(unix))]
fn prepare_files(_: &Paths) -> anyhow::Result<Vec<String>> {
    anyhow::bail!("the Job Service runs on Linux hosts only")
}

// ------------------------------------------------------------ the socket

/// `capabilities` over the private socket, plus the answering process.
#[derive(Clone, Debug)]
pub struct Running {
    pub pid: u32,
    pub executable: Option<PathBuf>,
    pub capabilities: Value,
}

#[cfg(target_os = "linux")]
pub fn probe_socket(paths: &Paths) -> Option<Running> {
    use std::io::{BufRead, Read, Write};
    use std::os::fd::AsRawFd;
    let mut socket = std::os::unix::net::UnixStream::connect(&paths.socket).ok()?;
    let timeout = Some(Duration::from_secs(3));
    socket.set_read_timeout(timeout).ok()?;
    socket.set_write_timeout(timeout).ok()?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut peer as *mut libc::ucred).cast(), &mut len)
    } == 0;
    if !ok || peer.pid <= 0 || peer.uid != unsafe { libc::geteuid() } {
        return None;
    }
    let token: String = std::fs::read(&paths.api_token).ok()?.iter().map(|b| format!("{b:02x}")).collect();
    let request = json!({"token": token, "method": "capabilities", "params": {}});
    serde_json::to_writer(&mut socket, &request).ok()?;
    socket.write_all(b"\n").ok()?;
    let mut line = String::new();
    std::io::BufReader::new(socket.take(1 << 20)).read_line(&mut line).ok()?;
    let reply: Value = serde_json::from_str(&line).ok()?;
    (reply["ok"] == true).then(|| Running {
        pid: peer.pid as u32,
        executable: std::fs::read_link(format!("/proc/{}/exe", peer.pid)).ok(),
        capabilities: reply["result"].clone(),
    })
}

#[cfg(not(target_os = "linux"))]
pub fn probe_socket(_: &Paths) -> Option<Running> {
    None
}

/// The descriptor for the running daemon, after the review a launch does.
pub fn review_running(plan: &Plan) -> anyhow::Result<JobMcpDescriptor> {
    let digest = crate::agent_management::file_sha256(&plan.bundle_manifest)?;
    let bundle = crate::launch::stock::StockBundle::load_references(&plan.native_home, &plan.bundle_manifest, &digest)?;
    let paths = &plan.paths;
    let canonical = |p: &Path| p.canonicalize().with_context(|| format!("resolve {}", p.display()));
    let draft = JobMcpDescriptor {
        version: 1,
        adapter: verified(&plan.binary)?,
        launcher: plan.launcher.clone(),
        endpoint: canonical(&paths.socket)?,
        api_token_file: canonical(&paths.api_token)?,
        grant_key_file: canonical(&paths.grant_key)?,
        daemon_pid: 0,
        daemon_start_ticks: 0,
    };
    Ok(draft.review_current(&bundle)?.descriptor)
}

fn wait_ready(plan: &Plan, timeout: Duration) -> anyhow::Result<JobMcpDescriptor> {
    let deadline = Instant::now() + timeout;
    loop {
        match review_running(plan) {
            Ok(descriptor) => return Ok(descriptor),
            Err(error) if Instant::now() >= deadline => {
                return Err(upgrade_error(
                    503,
                    "component_not_ready",
                    format!(
                        "the Job Service did not become ready in {}s: {error:#} (see `journalctl --user -u {}`)",
                        timeout.as_secs(),
                        plan.unit_name
                    ),
                ))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(300)),
        }
    }
}

fn select_descriptor(plan: &Plan, descriptor: Option<JobMcpDescriptor>) -> anyhow::Result<()> {
    crate::launch::local_deployment::LocalDeployment {
        native_home: plan.native_home.clone(),
        bundle_manifest: plan.bundle_manifest.clone(),
        job_mcp: descriptor,
    }
    .install()
}

fn descriptor_matches(descriptor: Option<&JobMcpDescriptor>, plan: &Plan) -> bool {
    descriptor.is_some_and(|d| {
        d.adapter.path == plan.binary && d.launcher == plan.launcher && Some(&d.endpoint) == plan.paths.socket.canonicalize().ok().as_ref()
    })
}

/// `<base>/components/job-service/restart-pending.json`: a selected change
/// (version or launcher set) the running daemon does not have yet.
fn pending_path(layout: &Layout) -> PathBuf {
    super::components_dir(layout).join(NAME).join("restart-pending.json")
}

fn in_use_error(agents: usize, adapters: usize) -> anyhow::Error {
    UpgradeError::boxed(
        409,
        "component_pinned",
        format!(
            "{agents} agent(s) were launched with the Job Service tools and {adapters} agent tool process(es) are running; \
             removing it breaks their `cutex_job` calls (use --force to remove anyway)"
        ),
        json!({ "name": NAME, "agents": agents, "adapters": adapters }),
    )
}

/// Agents (existing sessions) whose latest launch pinned a descriptor of this
/// install's socket.
fn pinning_agents(paths: &Paths) -> usize {
    let socket = paths.socket.canonicalize().unwrap_or_else(|_| paths.socket.clone());
    let sessions = crate::session::store::load_cutex_session_store().map(|s| s.sessions).unwrap_or_default();
    pinned_descriptors()
        .into_iter()
        .filter(|(id, d)| sessions.contains_key(id) && (d.endpoint == socket || d.endpoint == paths.socket))
        .count()
}

impl JobService {
    /// Keep the selected descriptor on the daemon that is actually running
    /// (deferred restart): review the running version with the descriptor's
    /// launcher (or the plan's). A failed review leaves the selection as is.
    fn keep_running_selected(&self, layout: &Layout, plan: &Plan, running: &Running) -> Option<String> {
        let exe = running.executable.as_ref()?;
        let version = version_of(layout, exe)?;
        let current = crate::launch::local_deployment::LocalDeployment::selected().ok().flatten().and_then(|d| d.job_mcp);
        if current.as_ref().is_some_and(|d| &d.adapter.path == exe) {
            return None;
        }
        let mut old = self.plan(layout, &super::version_dir(layout, NAME, &version)).ok()?;
        if let Some(d) = &current {
            old.launcher = d.launcher.clone();
        }
        let reviewed = review_running(&old).ok()?;
        select_descriptor(plan, Some(reviewed)).ok()?;
        Some(format!("job_mcp descriptor kept on the running {version}"))
    }

    fn write_unit(&self, plan: &Plan, exec: &dyn crate::upgrade::service::Exec) -> anyhow::Result<()> {
        std::fs::create_dir_all(&plan.unit_dir)?;
        crate::config::atomic::write_bytes_atomic(&plan.unit_path(), plan.unit_text.as_bytes())?;
        if plan.start {
            exec.run("systemctl", &args(&["--user", "daemon-reload"]))?;
        }
        Ok(())
    }
}

/// `cutex service uninstall`: stop and remove the Job Service unit cutex
/// wrote (a foreign unit is left alone). Returns the actions taken.
pub fn uninstall_unit(exec: &dyn crate::upgrade::service::Exec, dry_run: bool) -> anyhow::Result<Vec<String>> {
    let Some(dir) = default_unit_dir() else { return Ok(Vec::new()) };
    let unit = unit_name()?;
    let path = dir.join(&unit);
    if unit_is_ours(&path) != Some(true) {
        return Ok(Vec::new());
    }
    let actions = vec![format!("systemctl --user disable --now {unit}"), format!("remove {}", path.display())];
    if !dry_run {
        let _ = exec.run("systemctl", &args(&["--user", "disable", "--now", &unit]));
        std::fs::remove_file(&path)?;
    }
    Ok(actions)
}

impl Handler for JobService {
    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn required_files(&self) -> &'static [&'static str] {
        &[JOB_EXE]
    }

    /// `source` = the cutex checkout or `components/job-service` itself. A
    /// built `target/release/cutex-job-service` is used as is, else this
    /// runs `cargo build --release --locked` there.
    fn from_source(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        ensure_linux()?;
        let crate_dir = [source.to_path_buf(), source.join("components/job-service")]
            .into_iter()
            .find(|dir| {
                std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|text| text.contains("name = \"cutex-job-service\""))
            })
            .with_context(|| format!("{} is neither a cutex checkout nor components/job-service", source.display()))?;
        let built = crate_dir.join("target/release").join(JOB_EXE);
        if !built.is_file() {
            let status = std::process::Command::new("cargo")
                .args(["build", "--release", "--locked", "--bin", JOB_EXE])
                .current_dir(&crate_dir)
                .stdin(std::process::Stdio::null())
                .status()
                .context("cannot run cargo")?;
            ensure!(status.success(), "cargo build of {} failed", crate_dir.display());
        }
        crate::upgrade::layout::copy_file_atomic(&built, &dest.join(JOB_EXE))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dest.join(JOB_EXE), std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    fn external(&self, layout: &Layout) -> bool {
        let Ok(paths) = Paths::current() else { return false };
        if let Ok((path, _, _)) = self.unit_path() {
            if unit_is_ours(&path) == Some(false) {
                return true;
            }
        }
        match socket_peer_exe(&paths) {
            Some(exe) => version_of(layout, &exe).is_none(),
            None => state_lock_held(&paths),
        }
    }

    fn prepare(&self, ctx: &Context) -> anyhow::Result<Value> {
        let plan = self.plan_for(ctx.layout, &ctx.dir, ctx.dry_run)?;
        if unit_is_ours(&plan.unit_path()) == Some(false) {
            return Err(UpgradeError::boxed(
                409,
                "component_external",
                format!("{} exists and was not written by cutex; refusing to replace it", plan.unit_path().display()),
                json!({ "name": NAME }),
            ));
        }
        let actions = if ctx.dry_run { Vec::new() } else { prepare_files(&plan.paths)? };
        Ok(json!({
            "binary": plan.binary,
            "launcher": plan.launcher.path,
            "allowedLaunchers": plan.allowed_launchers,
            "unit": plan.unit_name,
            "actions": actions,
        }))
    }

    fn select(&self, ctx: &Context) -> anyhow::Result<Value> {
        let layout = ctx.layout;
        let plan = self.plan_for(layout, &ctx.dir, ctx.dry_run)?;
        let unit_path = plan.unit_path();
        let unit_changed = std::fs::read_to_string(&unit_path).ok().as_deref() != Some(plan.unit_text.as_str());
        if !plan.start {
            // Staging / tests: only the unit file.
            if !ctx.dry_run && unit_changed {
                self.write_unit(&plan, ctx.exec)?;
            }
            return Ok(json!({ "restartPending": false, "restarted": false, "started": false, "unitPath": unit_path }));
        }
        let running = probe_socket(&plan.paths);
        let running_current = running.as_ref().is_some_and(|r| r.executable.as_deref() == Some(plan.binary.as_path()));
        let restart = unit_changed || !running_current;
        if restart && running.is_some() {
            if let Err(reason) = self.restart_gate(layout) {
                let mut notes = Vec::new();
                if !ctx.dry_run {
                    std::fs::create_dir_all(pending_path(layout).parent().context("pending parent")?)?;
                    crate::config::atomic::write_bytes_atomic(
                        &pending_path(layout),
                        serde_json::to_vec_pretty(&json!({ "version": ctx.version, "reason": reason, "since": super::now() }))?.as_slice(),
                    )?;
                    notes.extend(self.keep_running_selected(layout, &plan, running.as_ref().expect("checked")));
                }
                return Ok(json!({ "restartPending": true, "restarted": false, "reason": reason, "notes": notes }));
            }
        }
        let mut actions = Vec::new();
        if unit_changed {
            actions.push(format!("write {}", unit_path.display()));
        }
        if restart {
            actions.push(format!("systemctl --user restart {}", plan.unit_name));
        }
        actions.push("select the job_mcp descriptor for new agents".into());
        if ctx.dry_run {
            return Ok(json!({ "restartPending": false, "restarted": false, "actions": actions }));
        }
        if unit_changed {
            self.write_unit(&plan, ctx.exec)?;
        }
        if restart {
            ctx.exec.run("systemctl", &args(&["--user", "enable", &plan.unit_name]))?;
            ctx.exec.run("systemctl", &args(&["--user", "restart", &plan.unit_name]))?;
        }
        let reviewed = (self.ready)(&plan, self.ready_timeout)?;
        let current = crate::launch::local_deployment::LocalDeployment::selected()?.and_then(|d| d.job_mcp);
        if restart || !descriptor_matches(current.as_ref(), &plan) {
            select_descriptor(&plan, Some(reviewed))?;
        }
        let _ = std::fs::remove_file(pending_path(layout));
        Ok(json!({ "restartPending": false, "restarted": restart, "actions": actions }))
    }

    fn deselect(&self, ctx: &Context, force: bool) -> anyhow::Result<Vec<String>> {
        ensure_linux()?;
        let (path, unit, started) = self.unit_path()?;
        if unit_is_ours(&path) == Some(false) {
            return Err(UpgradeError::boxed(
                409,
                "component_external",
                format!("{} was not written by cutex; leaving it", path.display()),
                json!({ "name": NAME }),
            ));
        }
        let paths = Paths::current()?;
        if !force {
            let busy = if probe_socket(&paths).is_some() { active_jobs(&paths)?.len() } else { 0 };
            if busy > 0 {
                return Err(busy_error(busy, "removing the Job Service"));
            }
            let agents = pinning_agents(&paths);
            let adapters = component_processes(ctx.layout).iter().filter(|(_, _, adapter)| *adapter).count();
            if agents > 0 || adapters > 0 {
                return Err(in_use_error(agents, adapters));
            }
        }
        let mut actions = Vec::new();
        let selected = crate::launch::local_deployment::LocalDeployment::selected()?;
        let socket = paths.socket.canonicalize().ok();
        let unselect = selected
            .as_ref()
            .is_some_and(|d| d.job_mcp.as_ref().is_some_and(|j| Some(&j.endpoint) == socket.as_ref() || j.endpoint == paths.socket));
        if unselect {
            actions.push("unselect the job_mcp descriptor (new agents get no cutex_job tools)".into());
        }
        if started {
            actions.push(format!("systemctl --user disable --now {unit}"));
        }
        if path.exists() {
            actions.push(format!("remove {}", path.display()));
        }
        if ctx.dry_run {
            return Ok(actions);
        }
        if unselect {
            let mut deployment = selected.expect("checked above");
            deployment.job_mcp = None;
            deployment.install()?;
        }
        if started {
            let _ = ctx.exec.run("systemctl", &args(&["--user", "disable", "--now", &unit]));
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(pending_path(ctx.layout));
        if started {
            ctx.exec.run("systemctl", &args(&["--user", "daemon-reload"]))?;
        }
        Ok(actions)
    }

    fn probe(&self, layout: &Layout, entry: &Entry) -> Value {
        let Ok(paths) = Paths::current() else { return json!({ "state": states::DEGRADED }) };
        let running = probe_socket(&paths);
        let binary = super::version_dir(layout, NAME, &entry.selected).join(JOB_EXE).canonicalize().ok();
        let pending: Option<Value> = std::fs::read(pending_path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let selected = crate::launch::local_deployment::LocalDeployment::selected().ok().flatten().and_then(|d| d.job_mcp);
        let caps = running.as_ref().map(|r| &r.capabilities);
        let nci = caps.is_some_and(|c| {
            c["externalNotifyFile"] == true && c["sources"].as_array().is_some_and(|s| s.iter().any(|v| v == NCI_SOURCE))
        });
        let running_version = running.as_ref().and_then(|r| r.executable.as_ref()).and_then(|exe| version_of(layout, exe));
        let deployment = crate::launch::local_deployment::LocalDeployment::selected().ok().flatten();
        let cli = deployment.as_ref().and_then(|d| {
            serde_json::from_slice::<crate::launch::stock::StockBundle>(&std::fs::read(&d.bundle_manifest).ok()?).ok()?.cli
        });
        let steps: Vec<&str> = match (&deployment, &cli) {
            (None, _) => vec![
                "Set up a Codex runtime for new agents (`cutex human install-runtime`)",
                "Install the Job Service again",
            ],
            (Some(_), None) => vec![
                "Select a Codex runtime that ships the `codex` CLI (the Job Service's sandbox launcher)",
                "Install the Job Service again",
            ],
            _ => Vec::new(),
        };
        let state = if running.is_none() && !steps.is_empty() {
            states::NEEDS_USER
        } else if running.is_none() {
            states::DEGRADED
        } else if pending.is_some() || running.as_ref().and_then(|r| r.executable.clone()) != binary {
            states::RESTART_PENDING
        } else if entry.last_error.is_some() || selected.is_none() {
            states::DEGRADED
        } else {
            states::READY
        };
        json!({
            "state": state,
            "steps": steps,
            "running": running.as_ref().map(|r| json!({ "pid": r.pid, "version": running_version })),
            "restartPending": pending,
            "nci": nci,
            "nciMailConfigured": paths.notify_file.is_file(),
            "descriptorSelected": selected.is_some(),
            "activeJobs": active_jobs(&paths).map(|v| v.len()).unwrap_or(0),
        })
    }

    fn pins(&self, layout: &Layout) -> std::collections::BTreeMap<String, usize> {
        let mut pins = mapped_versions(layout);
        let sessions = crate::session::store::load_cutex_session_store().map(|s| s.sessions).unwrap_or_default();
        let selected = crate::launch::local_deployment::LocalDeployment::selected().ok().flatten().and_then(|d| d.job_mcp);
        let descriptors = pinned_descriptors()
            .into_iter()
            .filter(|(id, _)| sessions.contains_key(id))
            .map(|(_, d)| d)
            .chain(selected);
        for descriptor in descriptors {
            if let Some(version) = version_of(layout, &descriptor.adapter.path) {
                *pins.entry(version).or_insert(0) += 1;
            }
        }
        pins
    }

    fn restart_gate(&self, _layout: &Layout) -> Result<(), String> {
        let paths = Paths::current().map_err(|e| format!("{e:#}"))?;
        let jobs = active_jobs(&paths).map_err(|e| format!("{e:#}"))?;
        if !jobs.is_empty() {
            return Err(format!("{} local job(s) running or launching", jobs.len()));
        }
        let launches = launch_in_flight();
        if launches > 0 {
            return Err(format!("{launches} agent launch(es) in flight"));
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "job_service_tests.rs"]
mod tests;
