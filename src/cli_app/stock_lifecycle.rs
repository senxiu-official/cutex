//! Opt-in stock child ownership inside the existing Management process.
use anyhow::{Context, ensure};
use cutex::agent_management::{StockRuntimeExecutor, StockRuntimeReceipt};
use cutex::app_server::runtime::AppServerRuntimeLayout;
use cutex::launch::command::LaunchCommand;
use cutex::launch::stock::{ExternalInputBinding, StockBundle};
use cutex::session::model::{
    CutexAppServerRuntimeBinding, CutexSessionRecord, LaunchProfileSource,
};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReviewedStockRuntimeAction {
    pub action_id: cutex::agent_management::AgentActionId,
    pub review: cutex::agent_management::StockRuntimeReview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReviewedRuntimePackageUpgrade {
    pub action_id: cutex::agent_management::AgentActionId,
    pub review: cutex::agent_management::RuntimePackageUpgradeReview,
}

impl ReviewedRuntimePackageUpgrade {
    pub(super) fn review(id: &str) -> anyhow::Result<Self> {
        let client = super::management_control_plane::ManagementControlClient::connect()?;
        let cutex_session_id = cutex::role_revision::CutexSessionId::new(id.to_string())
            .map_err(|_| anyhow::anyhow!("exact durable stock Agent ID required"))?;
        Ok(Self {
            action_id: cutex::agent_management::AgentActionId::new(format!(
                "tui-runtime-package-{}",
                uuid::Uuid::new_v4()
            ))?,
            review: client.review_runtime_package_upgrade(cutex_session_id)?,
        })
    }

    pub(super) fn execute(
        &self,
    ) -> anyhow::Result<cutex::agent_management::RuntimePackageUpgradeReceipt> {
        super::management_control_plane::ManagementControlClient::connect()?
            .apply_runtime_package_upgrade(self.action_id.clone(), self.review.clone())
    }
}

impl ReviewedStockRuntimeAction {
    pub(super) fn review(id: &str, restart: bool) -> anyhow::Result<Self> {
        let client = super::management_control_plane::ManagementControlClient::connect()?;
        let cutex_session_id = cutex::role_revision::CutexSessionId::new(id.to_string())
            .map_err(|_| anyhow::anyhow!("exact durable stock Agent ID required"))?;
        Ok(Self {
            action_id: cutex::agent_management::AgentActionId::new(format!(
                "tui-stock-runtime-{}",
                uuid::Uuid::new_v4()
            ))?,
            review: client.review_stock_runtime(cutex_session_id, restart)?,
        })
    }

    pub(super) fn execute(&self) -> anyhow::Result<cutex::agent_management::StockRuntimeReceipt> {
        super::management_control_plane::ManagementControlClient::connect()?
            .run_stock_runtime(self.action_id.clone(), self.review.clone())
    }
}

/// Ordinary online/foreground entry points share the same owner as Human/TUI.
/// A running owner is reused; an unresolved prior start is never replaced.
/// The owner's explicit-launch start from the CLI (`cutex human start` /
/// `restart`, `cutex session online` / restart on this host, resume from
/// the launcher); every caller is the owner.
pub(super) fn online(id: &str, restart: bool) -> anyhow::Result<StockRuntimeReceipt> {
    let receipt = online_inner(id, restart)?;
    // After the start: clearing first let an asks relaunch race this start.
    cutex::asks::wake::note_owner_start(id);
    Ok(receipt)
}

fn online_inner(id: &str, restart: bool) -> anyhow::Result<StockRuntimeReceipt> {
    let sessions = cutex::session::store::load_cutex_session_store()?;
    let record = sessions.sessions.get(id).context("agent disappeared")?;
    ensure!(
        !cutex::agent_management::agent_execution_blocked(&record),
        "restore the archived Agent before starting it"
    );
    ensure!(
        cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &cutex::platform::host::current_host_name()
        ),
        "this runtime must be managed on its owning host"
    );
    // An unresolved claim from a crashed launch is cleared by the service
    // during review when its owner is proven gone; a live launch still wins.
    if !restart {
        if let Some(binding) = &record.app_server_runtime {
            if stock_runtime_has_exited(record, binding)? {
                return request_dead_runtime_online(record);
            }
            verify_stock_process(record, binding)?;
            super::app_server_runtime::verify_exact_live_runtime_claim(record, binding)?;
            let receipt = matching_ready_receipt(record, &sessions)
                .context("runtime has no matching Ready receipt; use human recovery")?;
            return request_existing_owner_reconnect(record, receipt);
        }
    }
    let action = ReviewedStockRuntimeAction::review(id, restart)?;
    eprintln!("Action: {}", action.action_id.as_str());
    let receipt = action.execute()?;
    ensure!(
        receipt.stage == cutex::agent_management::StockRuntimeStage::Ready
            && receipt.error.is_none(),
        "{}; inspect cutex human action {}",
        receipt.error.as_deref().unwrap_or("runtime readiness incomplete"),
        receipt.action_id.as_str()
    );
    Ok(receipt)
}

fn request_existing_owner_reconnect(
    record: &CutexSessionRecord,
    receipt: &StockRuntimeReceipt,
) -> anyhow::Result<StockRuntimeReceipt> {
    let client = super::management_control_plane::ManagementControlClient::connect()?;
    reconnect_existing_owner(record, receipt, |path, body| {
        client.request_with_timeout(
            "POST",
            path,
            Some(body),
            std::time::Duration::from_secs(120),
        )
    })
}

/// A historical Ready receipt proves process ownership, not present bridge
/// health. Ask Management to reconnect that exact occurrence before returning it.
fn reconnect_existing_owner(
    record: &CutexSessionRecord,
    receipt: &StockRuntimeReceipt,
    send: impl FnOnce(&str, &[u8]) -> anyhow::Result<serde_json::Value>,
) -> anyhow::Result<StockRuntimeReceipt> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let encoded = url::form_urlencoded::byte_serialize(record.cutex_session_id.as_bytes())
        .collect::<String>();
    let path = format!("/v2/sessions/{encoded}/cutex/requests");
    let body = serde_json::to_vec(&serde_json::json!({"requestId":request_id,
        "method":"cutex/runtime/online", "params":{"expectedRuntimeGeneration":record.runtime_generation,
        "reason":"cutex_cli_reconnect", "openVisibleTerminal":false}}))?;
    let response = send(&path, &body)?;
    ensure!(
        response["contractVersion"] == 2
            && response["requestId"] == request_id
            && response["cutexSessionId"] == record.cutex_session_id
            && response
                .pointer("/cutex/method")
                .and_then(serde_json::Value::as_str)
                == Some("cutex/runtime/online"),
        "Management reconnect response identity mismatch"
    );
    let result = response
        .pointer("/cutex/result")
        .context("Management reconnect result missing")?;
    ensure!(
        result["status"] == "online"
            && result["runtimeGeneration"] == receipt.expected_generation
            && result["runtimeAgentId"] == receipt.runtime_agent_id
            && result["actionId"] == receipt.action_id.as_str(),
        "Management did not reconnect the expected runtime occurrence"
    );
    Ok(receipt.clone())
}

/// The service reconciles a proven exited occurrence under its mutation lock.
/// This does not request a restart of a live process or replay an old Ready action.
fn request_dead_runtime_online(record: &CutexSessionRecord) -> anyhow::Result<StockRuntimeReceipt> {
    let client = super::management_control_plane::ManagementControlClient::connect()?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let encoded = url::form_urlencoded::byte_serialize(record.cutex_session_id.as_bytes())
        .collect::<String>();
    let response: serde_json::Value = client.request_with_timeout(
        "POST",
        &format!("/v2/sessions/{encoded}/cutex/requests"),
        Some(&serde_json::to_vec(&serde_json::json!({
            "requestId":request_id, "method":"cutex/runtime/online",
            "params":{"expectedRuntimeGeneration":record.runtime_generation,
                "reason":"foreground_recover_exited_runtime", "openVisibleTerminal":false}
        }))?),
        std::time::Duration::from_secs(120),
    )?;
    ensure!(
        response["requestId"] == request_id
            && response["cutexSessionId"] == record.cutex_session_id
            && response
                .pointer("/cutex/result/status")
                .and_then(serde_json::Value::as_str)
                == Some("online"),
        "Runtime recovery failed: {response}"
    );
    let sessions = cutex::session::store::load_cutex_session_store()?;
    let current = sessions
        .sessions
        .get(&record.cutex_session_id)
        .context("agent disappeared after recovery")?;
    let receipt = matching_ready_receipt(current, &sessions)
        .context("recovered runtime Ready receipt missing")?;
    ensure!(
        response
            .pointer("/cutex/result/runtimeGeneration")
            .and_then(serde_json::Value::as_u64)
            == Some(receipt.expected_generation),
        "runtime changed during foreground recovery; retry"
    );
    Ok(receipt.clone())
}

/// Liveness of an unresolved launch claim's owner (creator or child), used to
/// clear claims left behind by a crash. `true` keeps the claim: a recorded
/// child with a matching start time is still running, or the publication
/// lease is still held by its creator/child. A missing lease file means no
/// process can hold it.
pub(super) fn launch_claim_owner_alive(
    record: &CutexSessionRecord,
    receipt: Option<&StockRuntimeReceipt>,
) -> anyhow::Result<bool> {
    use cutex::platform::process::{process_is_running, process_started_at};
    let bindings = receipt
        .and_then(|receipt| receipt.binding.as_ref())
        .into_iter()
        .chain(record.app_server_runtime.as_ref());
    for binding in bindings {
        if process_is_running(binding.pid) {
            let expected = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
            match process_started_at(binding.pid) {
                Ok(actual) if actual.timestamp() != expected.timestamp() => {} // PID reused
                _ => return Ok(true),
            }
        }
    }
    if let Some(pid) = record.runtime_pid {
        if record.app_server_runtime.is_none() && process_is_running(pid) {
            return Ok(true);
        }
    }
    let Some(receipt) = receipt else {
        return Ok(false);
    };
    let Some(publication) = receipt.publication.as_ref() else {
        return Ok(false);
    };
    if !publication.path.exists() {
        return Ok(false);
    }
    match super::stock_publication::lease(&receipt.claim_id, Some(publication)) {
        Ok(_lease) => Ok(false),
        Err(error) if format!("{error:#}").contains("publication_busy") => Ok(true),
        Err(error) => Err(error.context("launch claim lease evidence is ambiguous")),
    }
}

/// Only proven process absence permits automatic offline reconciliation.
/// Access errors, PID reuse and identity mismatches remain errors.
pub(super) fn stock_runtime_has_exited(
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
) -> anyhow::Result<bool> {
    // A dead Windows owner is already conclusive process-absence evidence.  Do
    // not require its historical executable package first: a retired release
    // root must not prevent reconciliation of an already exited runtime.  A
    // live or reused PID still takes the strict bundle/start-time/image path.
    // Likewise an occurrence recorded before this boot is gone, whatever now
    // runs under its PID.
    if cutex::session::boot_fence::with_host_probe(|host| {
        cutex::session::boot_fence::binding_process(binding, host)
            == cutex::session::boot_fence::BindingProcess::PreviousBoot
    }) {
        return Ok(true);
    }
    #[cfg(windows)]
    if !cutex::platform::process::process_is_running(binding.pid) {
        return Ok(true);
    }
    let sessions = cutex::session::store::load_cutex_session_store()?;
    let contract = running_stock_contract(record, binding, &sessions)?;
    let bundle = StockBundle::load_running(&contract)?;
    #[cfg(windows)]
    {
        return Ok(super::stock_publication::verified_process(
            binding.pid,
            &binding.started_at,
            &bundle.executable.path,
        )?
        .is_none());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        match cutex::platform::process::process_executable(binding.pid) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    #[cfg(not(windows))]
    {
        verify_stock_process_with_bundle(binding, &bundle)?;
        Ok(false)
    }
}

pub(super) fn matching_ready_receipt<'a>(
    record: &CutexSessionRecord,
    sessions: &'a cutex::session::model::CutexSessionStore,
) -> Option<&'a StockRuntimeReceipt> {
    let binding = record.app_server_runtime.as_ref()?;
    sessions
        .explicit_launch_receipts
        .values()
        .find_map(|receipt| match receipt {
            cutex::agent_management::ExplicitLaunchActionReceipt::Runtime(receipt)
                if receipt.stage == cutex::agent_management::StockRuntimeStage::Ready
                    && receipt.error.is_none()
                    && receipt.review.subject.cutex_session_id.as_str()
                        == record.cutex_session_id
                    && receipt.binding.as_ref() == Some(binding)
                    && receipt.expected_generation == record.runtime_generation
                    && record.current_runtime_agent_id.as_deref()
                        == Some(&receipt.runtime_agent_id) =>
            {
                Some(receipt)
            }
            _ => None,
        })
}

/// Reattach the Management bridge to a proven existing owner without launching
/// a process, advancing its generation, or replaying a completed start action.
pub(super) fn reconnect_ready_runtime(
    record: &CutexSessionRecord,
    sessions: &cutex::session::model::CutexSessionStore,
) -> anyhow::Result<StockRuntimeReceipt> {
    ensure!(!cutex::agent_management::agent_execution_blocked(&record), "cannot reconnect an archived agent");
    ensure!(
        record.app_server_launch_claim_id.is_none(),
        "unresolved runtime claim requires recovery"
    );
    ensure!(
        cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &cutex::platform::host::current_host_name()
        ),
        "runtime belongs to another host"
    );
    let receipt = matching_ready_receipt(record, sessions)
        .context("runtime has no matching Ready receipt; use human recovery")?;
    let binding = record
        .app_server_runtime
        .as_ref()
        .context("runtime binding missing")?;
    verify_stock_process(record, binding)?;
    super::app_server_runtime::verify_exact_live_runtime_claim(record, binding)?;
    StockExecutor::default().connect(record, receipt)?;
    let status = super::app_server_runtime::runtime_manager()
        .refresh_agent_bus_registration(&record.cutex_session_id)?
        .context("reconnected runtime has no Agent Bus bridge")?;
    ensure!(
        status.running
            && status.registered
            && status.runtime_agent_id == receipt.runtime_agent_id
            && status.thread_id == receipt.review.contract.native_id,
        "reconnected runtime bridge is not registered for the expected occurrence"
    );
    Ok(receipt.clone())
}

#[derive(Default)]
pub(super) struct StockExecutor {
    child: Option<super::stock_publication::GatedChild>,
    publication: Option<(cutex::agent_management::StockPublication, std::fs::File)>,
}
impl Drop for StockExecutor {
    fn drop(&mut self) {
        let _ = self.cleanup_owned();
    }
}

/// `TMPDIR` for a spawned native: an explicit one stays supported unless it
/// is another cutex home's stock-tmp (inherited from an agent shell), else
/// this home's private `runtime/stock-tmp`.
pub(super) fn launch_tmpdir(configured: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    cutex::config::paths::launch_tmp_dir(configured)
}

pub(super) fn clean_launch(
    program: &std::path::Path,
    home: &std::path::Path,
) -> anyhow::Result<LaunchCommand> {
    // An independent instance never launches a native over a home outside it.
    cutex::config::instance::require_inside_home(home, "native home")?;
    let mut launch = LaunchCommand::new(
        program
            .to_str()
            .context("stock executable path must be UTF-8")?,
    );
    // Enumerate names only to remove inherited values, including fork aliases.
    for (key, _) in std::env::vars_os() {
        launch =
            launch.env_remove(key.into_string().map_err(|_| {
                anyhow::anyhow!("non-UTF8 environment key cannot be safely scrubbed")
            })?);
    }
    #[cfg(not(windows))]
    for key in ["HOME"] {
        launch = launch.env(
            key,
            std::env::var(key).with_context(|| format!("private {key} required"))?,
        );
    }
    #[cfg(windows)]
    for key in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SystemRoot",
        "SystemDrive",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "PATH",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
    ] {
        if let Ok(value) = std::env::var(key) {
            launch = launch.env(key, value);
        }
    }
    // Keep nested `cutex` calls (notify callbacks, agent shells) in this instance.
    if let Some((key, value)) = cutex::config::instance::child_env() {
        launch = launch.env(key, value.to_str().context("CUTEX_HOME must be UTF-8")?);
    }
    let tmp = launch_tmpdir(std::env::var("TMPDIR").ok().as_deref())?;
    launch = launch.env("TMPDIR", tmp.to_str().context("TMPDIR must be UTF-8")?);
    #[cfg(windows)]
    {
        let value = tmp.to_str().context("temporary path must be UTF-8")?;
        launch = launch.env("TEMP", value).env("TMP", value);
    }

    #[cfg(feature = "stock-launch-test-hook")]
    if std::env::var("CUTEX_STOCK_TEST_GUARDED_NATIVE").as_deref() == Ok("1") {
        // Explicit S7/S6f same-host-UID fixture only. Default builds have no
        // inherited preload/endpoint authority. This is not OS isolation.
        let private = std::path::PathBuf::from(std::env::var("CUTEX_TEST_PRIVATE_HOME")?);
        ensure!(
            private.is_absolute()
                && private == std::path::PathBuf::from(std::env::var("HOME")?)
                && private.join(".cutex-test-private-home").is_file(),
            "guarded native requires exact private fixture HOME"
        );
        let guard = std::path::Path::new(
            "/srv/projects/cutex-light-core-r1/artifacts/s7a/connect-guard-error.so",
        );
        ensure!(
            guard.canonicalize()? == guard
                && cutex::agent_management::file_sha256(guard)?.as_str()
                    == "8fdf96e00b65c625c6c132f157eea03e081ae6ead7bcfc8dc82ad1865e2d9faa",
            "private approved guard changed"
        );
        let ports = std::env::var("S4_TEST_ALLOWED_PORTS")?;
        ensure!(
            !ports.is_empty()
                && ports
                    .split(',')
                    .all(|p| p.parse::<std::num::NonZeroU16>().is_ok()),
            "private owned ports required"
        );
        launch = launch
            .env("LD_PRELOAD", guard.to_str().unwrap())
            .env("S4_TEST_ALLOWED_PORTS", ports);
    }
    #[cfg(not(windows))]
    {
        launch = launch.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    }
    Ok(launch
        .env(
            "CODEX_HOME",
            home.to_str().context("native home must be UTF-8")?,
        )
        .env("LANG", "C.UTF-8")
        .env("TERM", "xterm-256color"))
}
/// Foreground rendering uses the user's terminal capabilities. The background
/// runtime's deliberately minimal environment is not a terminal description.
fn foreground_terminal(
    mut launch: LaunchCommand,
    environment: impl IntoIterator<Item = (String, String)>,
    truecolor: bool,
) -> LaunchCommand {
    for (key, value) in environment {
        if matches!(
            key.as_str(),
            "TERM"
                | "COLORTERM"
                | "TERM_PROGRAM"
                | "TERM_PROGRAM_VERSION"
                | "WT_SESSION"
                | "NO_COLOR"
                | "FORCE_COLOR"
                | "CLICOLOR"
                | "CLICOLOR_FORCE"
        ) {
            launch = launch.env_unset(&key).env(key, value);
        }
    }
    if truecolor {
        launch.envs.retain(|(key, _)| key != "COLORTERM");
        launch.envs.push(("COLORTERM".into(), "truecolor".into()));
    }
    launch
}

pub(super) fn option(
    launch: LaunchCommand,
    key: &str,
    value: impl serde::Serialize,
) -> anyhow::Result<LaunchCommand> {
    // TOML inline values; no credentials are accepted in this plan.
    let value = toml::Value::try_from(value)?;
    Ok(launch.arg("-c").arg(format!("{key}={value}")))
}

pub(super) fn configured(
    mut launch: LaunchCommand,
    profile: &cutex::launch::stock::StockConfiguration,
    owner: bool,
    migration_home: Option<&std::path::Path>,
    bundle: &cutex::launch::stock::StockBundle,
    native_home: &std::path::Path,
) -> anyhow::Result<LaunchCommand> {
    launch = option(launch, "model", &profile.model)?;
    launch = option(launch, "model_provider", &profile.model_provider)?;
    ensure!(
        profile
            .model_provider
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "unsupported stock provider key"
    );
    if let Some(projection) = &profile.selected_projection {
        cutex::profiles::model_variants::validate_runtime(projection, bundle, native_home)?;
        launch = launch.args(if let Some(home) = migration_home {
            projection.migrated_native_args(
                owner,
                &cutex::config::paths::host_codex_home_dir()?,
                home,
            )?
        } else {
            projection.native_args(owner)?
        });
        // Not in the reviewed projection (launch receipts persist it; older
        // releases refuse unknown keys there): read at spawn.
        launch = launch.args(
            cutex::launch::selected_profile::ImageHistoryTrimArgs::of_profile(&profile.profile_id)?.args(),
        );
        // A catalog-less API-key profile on its own provider: the automatic
        // reviewer reviews with the agent's model (`reviewer_catalog`).
        launch = launch.args(cutex::profiles::reviewer_catalog::launch_args(projection, &profile.model, bundle, native_home));
    } else if profile.private_chatgpt_auth.is_some() {
        launch = option(launch, "cli_auth_credentials_store", "file")?;
    } else {
        launch = option(
            launch,
            &format!("model_providers.{}", profile.model_provider),
            &profile.provider,
        )?;
    }
    // Remote attach inherits permissions from the existing owner. Codex 0.154
    // rejects explicit permission overrides when resuming a remote task.
    if owner {
        launch = option(launch, "approval_policy", &profile.approval)?;
        launch = option(launch, "sandbox_mode", &profile.sandbox)?;
        // Explicit reviewer only: None keeps the profile's own setting.
        if let Some(reviewer) = &profile.approvals_reviewer {
            launch = option(launch, "approvals_reviewer", reviewer)?;
        }
        // Honoured only when no named permission profile is selected (older
        // bundles); `permission_profile` below covers profile-mode launches.
        if let Some(table) = profile.legacy_workspace_write_table() {
            launch = option(launch, "sandbox_workspace_write", table)?;
        }
    }
    if let Some(reasoning) = &profile.reasoning {
        launch = option(launch, "model_reasoning_effort", reasoning)?;
    }
    drop_inherited_model_catalog(&launch);
    // D2: native instant interrupts stay off for every stock launch (owner,
    // review, light creation, history). Selected-profile settings carry no
    // `features` table, so this is a pin, not a default.
    let launch = launch.args(cutex::launch::native_features::instant_interrupt_off_args());
    option(launch, "analytics.enabled", false)
}

/// An agent home's `config.toml` is a copy of the shared home's and can carry
/// another profile's `model_catalog_json`; native then lists that profile's
/// models (stock.rs `drop_inherited_model_catalog`). Only per-agent homes are
/// edited, never the shared home (the user's own native config). Never blocks
/// a launch.
fn drop_inherited_model_catalog(launch: &LaunchCommand) {
    let Some(home) = launch
        .envs
        .iter()
        .rev()
        .find(|(key, _)| key == "CODEX_HOME")
        .map(|(_, value)| std::path::PathBuf::from(value))
    else {
        return;
    };
    let agents = match cutex::config::paths::runtime_dir() {
        Ok(runtime) => runtime.join("light/agents"),
        Err(_) => return,
    };
    let under_agents = match (home.canonicalize(), agents.canonicalize()) {
        (Ok(home), Ok(agents)) => home.starts_with(&agents) && home != agents,
        _ => false,
    };
    if !under_agents {
        return;
    }
    match cutex::launch::stock::drop_inherited_model_catalog(&home, &launch.args) {
        Ok(true) => eprintln!(
            "codex config: removed another profile's model_catalog_json from {}",
            home.display()
        ),
        Ok(false) => {}
        Err(error) => eprintln!("codex config: model catalog not checked: {error:#}"),
    }
}

/// Select the native permission profile (`default_permissions`), defining
/// `[permissions.cutex-workspace]` first when the thread needs network
/// access or extra writable roots.
pub(super) fn permission_profile(
    mut launch: LaunchCommand,
    profile: &cutex::launch::stock::StockConfiguration,
) -> anyhow::Result<LaunchCommand> {
    if let Some(definition) = profile.permission_profile_definition() {
        launch = option(
            launch,
            &format!(
                "permissions.{}",
                cutex::launch::stock::CUTEX_WORKSPACE_PERMISSION_PROFILE
            ),
            definition,
        )?;
    }
    option(launch, "default_permissions", profile.permission_profile_id()?)
}

#[cfg(test)]
pub(super) fn receiver_profile(sandbox: &str) -> anyhow::Result<&'static str> {
    match sandbox {
        "read-only" => Ok(":read-only"),
        "workspace-write" => Ok(":workspace"),
        "danger-full-access" => Ok(":danger-full-access"),
        _ => anyhow::bail!("unknown receiver permission profile"),
    }
}

/// A neutral, model-free owner. Never retries thread/start after an uncertain
/// response; the provider journals the known ID before adoption and online.
#[cfg(any(unix, windows))]
pub(super) fn bootstrap_native(
    permit: &cutex::agent_management::BootstrapExecutionPermit<'_>,
    existing: Option<&str>,
) -> Result<String, cutex::agent_management::LifecycleFailure> {
    use cutex::agent_management::LifecycleFailure;
    use cutex::app_server::client::{AppServerClient, AppServerClientOptions, AppServerEndpoint};
    let review = permit.review();
    let mut known = existing.map(str::to_owned);
    let mut spawned = false;
    let mut operation = || -> anyhow::Result<String> {
        ensure!(
            cfg!(any(target_os = "linux", target_os = "macos", windows)),
            "private bootstrap unsupported on this platform"
        );
        ensure!(
            cutex::config::paths::host_codex_home_dir()?.canonicalize()? == review.native_home,
            "bootstrap home changed before spawn"
        );
        review
            .configuration
            .validate_auth_home(&review.native_home)?;
        let bundle = StockBundle::load_references(
            &review.native_home,
            &review.bundle_manifest,
            &review.bundle_sha256,
        )?;
        ensure!(bundle.soon_ingress(), "bootstrap bundle mismatch");
        let spec = review
            .request
            .operation
            .bootstrap_spec()
            .context("bootstrap operation required")?;
        ensure!(
            cutex::launch::stock::bootstrap_configuration(spec)? == review.configuration,
            "bootstrap config changed before spawn"
        );
        let directory = launch_tmpdir(std::env::var("TMPDIR").ok().as_deref())?.join(format!(
            "cb-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        }
        #[cfg(windows)]
        {
            std::fs::create_dir(&directory)?;
            cutex::platform::private_fs::secure_directory(&directory)?;
        }
        #[cfg(windows)]
        let layout =
            AppServerRuntimeLayout::prepare_stock(&format!("bootstrap-{}", uuid::Uuid::new_v4()))?;
        #[cfg(unix)]
        let socket = directory.join("native.sock");
        #[cfg(unix)]
        ensure!(
            socket.as_os_str().len() < 104,
            "private bootstrap socket path too long"
        );
        let launch = configured(
            clean_launch(&bundle.executable.path, &review.native_home)?,
            &review.configuration,
            true,
            None,
            &bundle,
            &review.native_home,
        )?;
        let launch = permission_profile(launch, &review.configuration)?;
        #[cfg(unix)]
        let launch = launch
            .arg("--listen")
            .arg(format!("unix://{}", socket.display()));
        #[cfg(windows)]
        let launch = layout
            .app_server_args()
            .into_iter()
            .skip(1)
            .fold(launch, |launch, arg| launch.arg(arg));
        let mut command = launch.to_command();
        if let Some(projection) = &review.configuration.selected_projection {
            if let Some(secret) = projection.secret()? {
                secret.apply(&mut command);
            }
        }
        let mut log_options = std::fs::OpenOptions::new();
        log_options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            log_options.mode(0o600);
        }
        let log = log_options.open(directory.join("native.stderr.log"))?;
        command
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            let parent = std::process::id();
            // Owned child only. Creator death before an ID is captured stays
            // uncertain in the journal, and cannot leave a reusable writer.
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() as u32 != parent {
                        return Err(std::io::Error::other("bootstrap creator exited"));
                    }
                    Ok(())
                });
            }
        }
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        #[cfg(unix)]
        let _owned = Owned(command.spawn()?);
        #[cfg(windows)]
        let _owned = {
            let lease = std::fs::File::create(directory.join("bootstrap.lock"))?;
            let secret = review
                .configuration
                .selected_projection
                .as_ref()
                .map(|projection| projection.secret())
                .transpose()?
                .flatten();
            let mut child = super::stock_publication::spawn_with_secret(
                &launch,
                &spec.cwd,
                &directory.join("native.stderr.log"),
                &lease,
                secret.as_ref(),
            )?;
            child.start_ephemeral()?;
            child
        };
        spawned = true;
        #[cfg(unix)]
        let endpoint = AppServerEndpoint::UnixSocket {
            socket_path: socket,
        };
        #[cfg(windows)]
        let endpoint = layout.endpoint();
        let client = AppServerClient::connect(AppServerClientOptions::new(endpoint))?;
        let handle = client.handle();
        let response = if let Some(native) = existing {
            handle.request("thread/resume", serde_json::json!({"threadId":native,"cwd":spec.cwd,"approvalPolicy":review.configuration.approval}))?
        } else {
            handle.request("thread/start", serde_json::json!({"cwd":spec.cwd,"approvalPolicy":review.configuration.approval,"ephemeral":false,"historyMode":"paginated"}))?
        };
        let native = response["thread"]["id"]
            .as_str()
            .context("bootstrap response missing native ID")?
            .to_owned();
        ensure!(
            uuid::Uuid::parse_str(&native)?.to_string() == native,
            "bootstrap malformed native ID"
        );
        ensure!(
            existing.is_none_or(|expected| expected == native),
            "bootstrap resumed wrong native ID"
        );
        #[cfg(feature = "stock-launch-test-hook")]
        if existing.is_none()
            && cutex::agent_management::bootstrap_test_fault(
                "CUTEX_BOOTSTRAP_TEST_PRE_ID_ACTION",
                &review.request.action_id,
            )
        {
            // The request may have created history, but its ID is not in the
            // Cutex journal. Abort the owned creator; no retry is authorized.
            std::process::exit(86);
        }
        known = Some(native.clone());
        // Accepted aa382cdb5 foundation: paginated read(true) awaits persist()
        // and propagates write failures. Metadata-only read is not this ACK.
        let ack = handle.request(
            "thread/read",
            serde_json::json!({"threadId":native,"includeTurns":true}),
        )?;
        ensure!(
            ack["thread"]["id"].as_str() == Some(&native)
                && ack["thread"]["historyMode"].as_str() == Some("paginated")
                && ack["thread"]["turns"].as_array().is_some_and(Vec::is_empty),
            "neutral bootstrap persistence ACK mismatch"
        );
        Ok(native)
    };
    operation().map_err(|error| LifecycleFailure {
        code: "reviewed_native_bootstrap_failed".into(),
        detail: error.to_string(),
        outcome_unknown: spawned || known.is_some(),
        known_native_session_id: known,
    })
}

#[cfg(not(any(unix, windows)))]
pub(super) fn bootstrap_native(
    _permit: &cutex::agent_management::BootstrapExecutionPermit<'_>,
    _existing: Option<&str>,
) -> Result<String, cutex::agent_management::LifecycleFailure> {
    Err(cutex::agent_management::LifecycleFailure::definite(
        "unsupported_bootstrap_platform",
        "private bootstrap requires Linux",
    ))
}

impl StockRuntimeExecutor for StockExecutor {
    fn published_owner_absent(&mut self, receipt: &StockRuntimeReceipt) -> anyhow::Result<bool> {
        let binding = receipt
            .binding
            .as_ref()
            .context("published binding missing")?;
        #[cfg(target_os = "macos")]
        {
            if let Some(stat) = cutex::platform::macos_proc::stat(binding.pid)? {
                if !stat.zombie {
                    let actual = cutex::platform::process::process_started_at(binding.pid)?;
                    let original = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
                    if actual.timestamp() == original.timestamp() {
                        return Ok(false);
                    }
                }
            }
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", binding.pid));
            #[cfg(target_os = "linux")]
            match stat {
                Ok(stat) => {
                    let state = stat
                        .rsplit(')')
                        .next()
                        .context("invalid owned process stat")?
                        .split_whitespace()
                        .next();
                    if state != Some("Z") {
                        let actual = cutex::platform::process::process_started_at(binding.pid)?;
                        let original = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
                        if actual.timestamp() == original.timestamp() {
                            return Ok(false);
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            // Never signal a reused PID. Also refuse a live endpoint holder.
            match std::os::unix::net::UnixStream::connect(
                binding
                    .endpoint
                    .strip_prefix("unix://")
                    .context("invalid stock endpoint")?,
            ) {
                Ok(_) => anyhow::bail!("published endpoint remains live; ownership ambiguous"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => return Err(error.into()),
            }
            ensure!(
                receipt.publication.is_some(),
                "publication_missing: cannot recover legacy binding"
            );
            self.publication(receipt)?;
            Ok(true)
        }
        #[cfg(windows)]
        {
            if cutex::platform::process::process_is_running(binding.pid) {
                let actual = cutex::platform::process::process_started_at(binding.pid)?;
                let expected = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
                if actual == expected {
                    return Ok(false);
                }
            }
            ensure!(
                !AppServerRuntimeLayout::from_binding(binding)?.endpoint_ready(),
                "published endpoint remains live; ownership ambiguous"
            );
            ensure!(
                receipt.publication.is_some(),
                "runtime publication evidence missing"
            );
            self.publication(receipt)?;
            Ok(true)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        anyhow::bail!("stock publication unsupported on this platform")
    }
    fn publication(
        &mut self,
        receipt: &StockRuntimeReceipt,
    ) -> anyhow::Result<cutex::agent_management::StockPublication> {
        if let Some((publication, _)) = &self.publication {
            ensure!(
                receipt
                    .publication
                    .as_ref()
                    .is_none_or(|p| p == publication),
                "publication changed"
            );
            return Ok(publication.clone());
        }
        let publication =
            super::stock_publication::lease(&receipt.claim_id, receipt.publication.as_ref())?;
        let result = publication.0.clone();
        self.publication = Some(publication);
        Ok(result)
    }
    fn stop(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()> {
        let outcome = super::native_stop::stop_and_commit(record, true)?;
        ensure!(
            outcome.stopped,
            "native runtime stop incomplete: {}",
            outcome.detail
        );
        Ok(())
    }
    fn spawn(
        &mut self,
        record: &CutexSessionRecord,
        bundle: &StockBundle,
        receipt: &StockRuntimeReceipt,
    ) -> anyhow::Result<CutexAppServerRuntimeBinding> {
        let config = cutex::config::store::load_codez_config_checked()?;
        ensure!(
            config.agent_bus_enabled,
            "existing private Agent Bus required; no automatic service start"
        );
        // The runtime gets a per-agent credential bound to this runtime id and
        // durable session, never the shared root token (see
        // docs/agent-tool-security.md).
        let token = cutex::agent_bus::agent_auth::launch_credential(
            &config,
            &receipt.runtime_agent_id,
            &record.cutex_session_id,
        )
        .context("private authenticated Agent Bus required")?;
        let layout = AppServerRuntimeLayout::prepare_stock(&record.cutex_session_id)?;
        let profile = &receipt.review.configuration;
        let mut launch = configured(
            clean_launch(
                &bundle.executable.path,
                &receipt.review.contract.native_home,
            )?,
            profile,
            true,
            (matches!(receipt.review.contract.version, 3 | 4))
                .then_some(receipt.review.contract.native_home.as_path()),
            bundle,
            &receipt.review.contract.native_home,
        )?;
        let accounts = super::account_store::load_store_read_only()?;
        let account = accounts
            .accounts
            .iter()
            .find(|account| account.id == profile.profile_id)
            .context("launch profile unavailable for proxy configuration")?;
        for (key, value) in cutex::config::proxy::native_proxy_envs(account, &config) {
            launch = launch.env(key, value);
        }
        for (key, enabled) in cutex::profiles::settings_api::native_subagent_overrides(account) {
            launch = option(launch, key, enabled)?;
        }
        // request_user_input outside Plan mode: an agent handling a
        // delegation asks its requester through it (asks §5.6), and the
        // owner answers questions in the desktop. The native default hides
        // the tool in Default mode (cute-codex features
        // `default_mode_request_user_input`), which made questions unreachable.
        launch = option(launch, "features.default_mode_request_user_input", true)?;
        // Shell tools must not inherit the bus credential: `set` is applied
        // after the profile's include/exclude rules, so this blanks it even
        // under `inherit = "all"`. The cutex MCP server still receives it
        // through `env_vars` below.
        launch = option(
            launch,
            cutex::agent_bus::agent_auth::SHELL_ENV_BLANK_OVERRIDE,
            "",
        )?;
        if bundle.soon_ingress() {
            launch = permission_profile(launch, profile)?;
        }
        let scoped =
            cutex::launch::scoped_mcp::launch_injection_for_session(&record.cutex_session_id)
                .context("scoped MCP configuration")?;
        // Shared home plugins / MCP servers / skill toggles / hook trust
        // edited since this agent's own home was materialized
        // (launch/shared_config.rs). Before Cutex's own mcp_servers.cutex /
        // cutex_job and scoped MCP so those replace same-named entries; after
        // the profile projection, whose plugins / skills it carries over. A
        // broken shared config must not block the agent: warn and skip.
        let taken_envs: std::collections::BTreeSet<String> = launch
            .envs
            .iter()
            .chain(&scoped.envs)
            .map(|(key, _)| key.clone())
            .collect();
        match cutex::launch::shared_config::launch_injection(
            &receipt.review.contract.native_home,
            config.codex_config_follow_shared,
            &launch.args,
            &taken_envs,
        ) {
            Ok(injection) => launch = injection.apply(launch),
            Err(error) => eprintln!("codex shared config: not applied: {error:#}"),
        }
        #[allow(unused_mut)]
        let mut mcp_env = vec![
            "CUTEX_AGENT_ID",
            "CUTEX_RUNTIME_GENERATION",
            "CUTEX_AGENT_BUS_URL",
            "CUTEX_AGENT_BUS_TOKEN",
        ];
        #[cfg(feature = "stock-launch-test-hook")]
        if std::env::var("CUTEX_STOCK_TEST_GUARDED_NATIVE").as_deref() == Ok("1") {
            mcp_env.extend(["LD_PRELOAD", "S4_TEST_ALLOWED_PORTS"]);
        }
        launch = option(
            launch,
            "mcp_servers.cutex",
            serde_json::json!({"command":bundle.facade.path,"env_vars":mcp_env,
                "env":facade_config_env(cutex::management::service::management_port(&config)),
                "default_tools_approval_mode":"approve","enabled":cutex::agent_bus::tool_policy::cutex_mcp_enabled_for_launch(record,&profile.profile_id)}),
        )?;
        launch = option(
            launch,
            // Every supported native bundle (stock, S6, S6E, and 0.156.1)
            // reads Code Mode settings from [features.code_mode]. The legacy
            // root `code_mode` path is rejected as an unknown session flag.
            "features.code_mode.direct_only_tool_namespaces",
            vec!["mcp__cutex"],
        )?;
        if let Some(job) = &receipt.review.job_mcp {
            job.validate(bundle)?;
            // Business MCP tools retain ordinary native approval. Never add
            // this namespace to the privileged control direct-only set.
            launch = option(launch, "mcp_servers.cutex_job", job.config()?)?;
        }
        // Scoped MCP (DESIGN 4.11): project then agent servers, agent disables.
        // Built-in names are rejected by the store, so cutex/cutex_job stay as above.
        launch = scoped.apply(launch);
        // A daemon-owned native keeps the whole agent tree across transport
        // loss. Only opt in after probing this exact (possibly pinned) server;
        // older binaries may silently ignore unknown configuration keys.
        launch = cutex::launch::thread_retention::apply(
            launch, bundle, &receipt.review.contract.native_home,
        )?;
        // Design 034: opt-in stable Responses prompt cache key per host +
        // workspace, only after the exact native advertises support; never
        // refuses the launch (no support / failed probe = no key).
        if cutex::config::prompt_cache_key::current().enabled {
            launch = cutex::launch::prompt_cache_key::apply(
                launch,
                bundle,
                &receipt.review.contract.native_home,
                &cutex::platform::host::current_host_name(),
                &record.cwd,
            );
        }
        let mut args = layout.app_server_args();
        if bundle.common_ingress() {
            // The accepted artifact is the direct app-server, not the stock CLI.
            args.remove(0);
            let directory = std::path::PathBuf::from(layout.binding(0, String::new()).runtime_dir);
            let path = ExternalInputBinding::from_review(receipt).stage(&directory)?;
            args.extend([
                "--external-input-binding-file".into(),
                path.to_string_lossy().into_owned(),
            ]);
        }
        launch = launch
            .args(args)
            .env("CUTEX_AGENT_ID", &receipt.runtime_agent_id)
            .env(
                "CUTEX_RUNTIME_GENERATION",
                receipt.expected_generation.to_string(),
            )
            .env(
                "CUTEX_AGENT_BUS_URL",
                cutex::agent_bus::service::agent_bus_base_url(
                    cutex::agent_bus::service::agent_bus_port(&config),
                ),
            )
            .env("CUTEX_AGENT_BUS_TOKEN", token);
        let log = std::path::PathBuf::from(layout.binding(0, String::new()).runtime_dir)
            .join("stock.stderr.log");
        // One gated setsid child, without contacting systemd. It cannot exec
        // native stock until its binding has committed. This is not a cgroup.
        let secret = profile
            .selected_projection
            .as_ref()
            .map(|p| p.secret())
            .transpose()?
            .flatten();
        self.child = Some(super::stock_publication::spawn_with_secret(
            &launch,
            &cutex::session::reviewed_registration::occurrence_launch_cwd(receipt)?,
            &log,
            &self
                .publication
                .as_ref()
                .context("publication lease missing")?
                .1,
            secret.as_ref(),
        )?);
        let child = self.child.as_mut().expect("spawned owned child");
        let mut binding = layout.binding(
            child.id(),
            cutex::platform::process::process_started_at(child.id())?.to_rfc3339(),
        );
        binding.launched_profile = Some(profile.profile_name.clone());
        binding.launched_profile_id = Some(profile.profile_id.clone());
        binding.launch_profile_source = Some(if profile.inherited {
            LaunchProfileSource::GlobalDefault
        } else {
            LaunchProfileSource::SessionConfigured
        });
        binding.schema_version = if bundle.soon_ingress() {
            "U-0.153.4+provider-item-id-ca580a78-soon-v1"
        } else if bundle.common_ingress() {
            "U-0.153.4+S6-c2aaceb4-external-input-v1"
        } else {
            "stock-0.153.4-app-server-v2"
        }
        .into();
        binding.schema_sha256 = bundle.schema.sha256.as_str().to_string();
        Ok(binding)
    }
    fn connect(
        &mut self,
        record: &CutexSessionRecord,
        receipt: &StockRuntimeReceipt,
    ) -> anyhow::Result<()> {
        let binding = receipt.binding.as_ref().context("stock binding missing")?;
        let bundle = StockBundle::load_running(&receipt.review.contract)?;
        if bundle.common_ingress() {
            ExternalInputBinding::from_review(receipt)
                .verify(std::path::Path::new(&binding.runtime_dir))?;
        }
        if let Some(child) = &mut self.child {
            child.release()?;
            self.publication.take();
        }
        // Native exec is permitted only after the binding/receipt commit. The
        // existing manager connect below validates the actual native endpoint.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            // The endpoint first (a loopback connect); the process verification
            // (session store + bundle reload) only once it answers, not every round.
            if AppServerRuntimeLayout::from_binding(binding)?.endpoint_ready()
                && verify_stock_process(record, binding).is_ok()
            {
                break;
            }
            if let Some(child) = &mut self.child {
                ensure!(
                    !child.try_wait()?,
                    "published stock child exited before readiness"
                );
            }
            ensure!(
                std::time::Instant::now() < until,
                "published stock owner not ready; retry exact action, no fallback"
            );
            std::thread::park_timeout(std::time::Duration::from_millis(10));
        }
        let manager = super::app_server_runtime::runtime_manager();
        if let Some(status) = manager
            .status(&record.cutex_session_id)?
            .filter(|s| s.connected)
        {
            ensure!(
                status.thread_id == receipt.review.contract.native_id
                    && status.runtime_generation == receipt.expected_generation,
                "stock manager occurrence mismatch"
            );
        } else {
            let cfg = &receipt.review.configuration;
            manager.connect_binding(
                &record.cutex_session_id,
                binding,
                {
                    let mut resume = cutex::app_server::commands::ThreadResumeParams {
                        thread_id: receipt.review.contract.native_id.clone(),
                        // Management needs live turn identity, not conversation history.
                        // Descending limit one includes the native server's active overlay.
                        exclude_turns: Some(true),
                        initial_turns_page: Some(serde_json::json!({
                            "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"
                        })),
                        model: Some(cfg.model.clone()),
                        model_provider: Some(cfg.model_provider.clone()),
                        service_tier: cutex::app_server::runtime::session_service_tier(record),
                        cwd: Some(
                            cutex::session::reviewed_registration::occurrence_launch_cwd(receipt)?
                                .into(),
                        ),
                        approval_policy: Some(serde_json::json!(cfg.approval)),
                        // The coherent CLI requires the receiver's named profile.
                        // A legacy per-resume sandbox override erases that identity
                        // even when its read-only projection appears equivalent.
                        sandbox: (!bundle.soon_ingress()).then(|| cfg.sandbox.clone()),
                        // A thread persisted with a built-in profile would
                        // otherwise keep it over the launch default; select the
                        // extras profile explicitly (defined by the launch args).
                        permissions: (bundle.soon_ingress() && cfg.has_workspace_extras())
                            .then(|| cutex::launch::stock::CUTEX_WORKSPACE_PERMISSION_PROFILE.to_string()),
                        ..Default::default()
                    };
                    // `# Cutex`, project memory + file-citation instructions
                    // (and memory dir as a runtime workspace root for project
                    // members).
                    cutex::memory::attach_launch_augmentation_to_thread_resume(
                        &mut resume,
                        record,
                    );
                    resume
                },
                receipt.expected_generation,
                "host",
            )?;
        }
        if bundle.soon_ingress() {
            let status = manager
                .status(&record.cutex_session_id)?
                .context("receiver settings unavailable")?;
            let settings = status.thread_settings.context("receiver settings absent")?;
            ensure!(
                settings
                    .pointer("/activePermissionProfile/id")
                    .and_then(serde_json::Value::as_str)
                    == Some(receipt.review.configuration.permission_profile_id()?),
                "existing thread active permission profile mismatches reviewed receiver; explicit controller correction required"
            );
        }
        if manager
            .agent_bus_bridge_status(&record.cutex_session_id)?
            .is_none_or(|status| !status.running)
        {
            manager.stop_agent_bus_bridge(&record.cutex_session_id)?;
            let mut registration = super::app_server_runtime::runtime_agent_registration(
                record,
                binding,
                &receipt.runtime_agent_id,
            )?;
            // Stock registration carries the reviewed configuration unchanged;
            // it must not derive new collaboration groups from a cwd hash.
            registration.cwd =
                cutex::session::reviewed_registration::occurrence_launch_cwd(receipt)?;
            registration.groups = record.agent_groups.clone();
            registration.path_key = None;
            registration.name = receipt.review.subject.formal_name.clone();
            registration.base_name = Some(receipt.review.subject.formal_name.clone());
            let mut options = cutex::app_server::bus_bridge::AppServerAgentBusBridgeOptions::new(
                registration,
                &receipt.review.contract.native_id,
            )
            .with_cutex_session_id(&record.cutex_session_id);
            options.registration_only = !bundle.common_ingress();
            if !options.registration_only {
                options.external_input_generation = Some(receipt.expected_generation);
            }
            #[allow(unused_mut)]
            let mut config = cutex::config::store::load_codez_config_checked()?;
            #[cfg(feature = "stock-launch-test-hook")]
            {
                static DENIED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if std::env::var("CUTEX_STOCK_TEST_REGISTER_DENY_ACTION")
                    .ok()
                    .as_deref()
                    == Some(receipt.action_id.as_str())
                    && !DENIED.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    // Exercise the real private provider's authentication denial,
                    // not a successful fake registration or authority override.
                    config.agent_bus_token = Some("s4-deliberately-invalid-fixture-token".into());
                }
            }
            manager.start_agent_bus_bridge(
                &record.cutex_session_id,
                Arc::new(cutex::agent_bus::client::AgentBusHttpClient::from_config(
                    &config,
                )),
                options,
            )?;
        }
        #[cfg(feature = "stock-launch-test-hook")]
        {
            static INJECTED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if std::env::var("CUTEX_STOCK_TEST_LOST_READY_ACTION")
                .ok()
                .as_deref()
                == Some(receipt.action_id.as_str())
                && !INJECTED.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                anyhow::bail!("private test injected lost readiness after real registration");
            }
        }
        Ok(())
    }
    fn cleanup_owned(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            child.cleanup()?;
        }
        self.publication.take();
        Ok(())
    }
    fn retain_owner(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = std::thread::Builder::new()
                .name("stock-child-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

pub(super) fn verify_stock_process(
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
) -> anyhow::Result<()> {
    let sessions = cutex::session::store::load_cutex_session_store()?;
    let contract = running_stock_contract(record, binding, &sessions)?;
    let bundle = StockBundle::load_running(&contract)?;
    verify_stock_process_with_bundle(binding, &bundle)
}

// Desired configuration may change while an older runtime is still alive.
// Attach/stop must verify that occurrence's launch receipt, not the next package.
fn running_stock_contract(
    record: &CutexSessionRecord,
    binding: &CutexAppServerRuntimeBinding,
    sessions: &cutex::session::model::CutexSessionStore,
) -> anyhow::Result<cutex::agent_management::ExplicitLaunchContract> {
    for receipt in sessions.explicit_launch_receipts.values() {
        if let cutex::agent_management::ExplicitLaunchActionReceipt::Runtime(receipt) = receipt {
            if receipt.review.subject.cutex_session_id.as_str() == record.cutex_session_id
                && receipt.binding.as_ref() == Some(binding)
                && receipt.expected_generation == record.runtime_generation
                && record
                    .current_runtime_agent_id
                    .as_deref()
                    .is_none_or(|id| id == receipt.runtime_agent_id)
            {
                return Ok(receipt.review.contract.clone());
            }
        }
    }
    record
        .explicit_launch
        .clone()
        .context("runtime package binding missing")
}

pub(super) fn verify_stock_process_with_bundle(
    binding: &CutexAppServerRuntimeBinding,
    bundle: &StockBundle,
) -> anyhow::Result<()> {
    ensure!(
        binding.schema_sha256 == bundle.schema.sha256.as_str(),
        "stock binding schema mismatch"
    );
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        ensure!(
            cutex::platform::process::process_executable(binding.pid)? == bundle.executable.path,
            "stock process executable mismatch"
        );
        let actual = cutex::platform::process::process_started_at(binding.pid)?;
        let expected = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
        ensure!(
            actual.timestamp() == expected.timestamp(),
            "stock PID generation changed"
        );
        ensure!(
            unsafe { libc::getpgid(binding.pid as i32) } == binding.pid as i32,
            "stock process group mismatch"
        );
        Ok(())
    }
    #[cfg(windows)]
    {
        ensure!(
            super::stock_publication::verified_process(
                binding.pid,
                &binding.started_at,
                &bundle.executable.path
            )?
            .is_some(),
            "runtime process exited"
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = bundle;
        anyhow::bail!("stock subset requires Linux")
    }
}
pub(super) fn request(path: &std::path::Path, management_url: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(management_url)?;
    ensure!(
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.path() == "/",
        "explicit local Management URL required"
    );
    let request: cutex::agent_management::ExplicitLaunchRequest =
        serde_json::from_slice(&std::fs::read(path)?)?;
    let config = cutex::config::store::load_codez_config_checked()?;
    let token = cutex::management::service::management_root_credential(&config, None)?;
    let result = cutex::management::remote::management_http_json_with_timeout(
        management_url,
        "POST",
        "/v2/agent-management/explicit-launch",
        Some(token),
        Some(&serde_json::to_vec(&request)?),
        std::time::Duration::from_secs(60),
    )?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub(super) fn attach(id: &str) -> anyhow::Result<()> {
    attach_status(id).map(|_| ())
}
pub(super) fn attach_status(id: &str) -> anyhow::Result<std::process::ExitStatus> {
    let mut timing = StartupTiming::new();
    // All foreground entry points reconcile exactly once, including Recent Sessions.
    online(id, false)?;
    timing.mark("owner-ready-and-reconnected");
    let store = cutex::session::store::load_cutex_session_store()?;
    let record = store
        .sessions
        .get(id)
        .context("exact stock durable ID required")?;
    ensure!(!cutex::agent_management::agent_execution_blocked(&record), "archived stock Agent cannot attach");
    let binding = record
        .app_server_runtime
        .as_ref()
        .context("stock runtime is offline")?;
    let contract = running_stock_contract(record, binding, &store)?;
    let bundle = StockBundle::load_running(&contract)?;
    ensure!(
        !bundle.common_ingress() || bundle.soon_ingress(),
        "U+S6 bundle contains only app-server; this slice has no pinned compatible CLI attach artifact"
    );
    // Reuse the bundle already validated above instead of hashing it again.
    verify_stock_process_with_bundle(binding, &bundle)
        .context("Inspect running native process for foreground attach")?;
    super::app_server_runtime::verify_exact_live_runtime_claim(record, binding)
        .context("Verify foreground runtime endpoint ownership")?;
    ensure!(
        record.app_server_launch_claim_id.is_none(),
        "stock readiness unresolved; replay launch action"
    );
    let ready = matching_ready_receipt(record, &store).context("runtime Ready receipt missing")?;
    // online() already reconnected this owner. The checks above revalidate its
    // identity; a second management reconnect repeats the same expensive work.
    timing.mark("running-bundle-and-owner-verified");
    // Remote CLI config must describe the running occurrence, not silently
    // substitute local OpenAI defaults or a newly selected durable profile.
    // A frontend-only update does not require restarting its app-server owner.
    // Keep validating the running occurrence above; select the desired CLI only
    // when its server, host and wire schema are byte-identical to that owner.
    let desired = record
        .explicit_launch
        .as_ref()
        .filter(|desired| desired.bundle_sha256 != contract.bundle_sha256)
        .and_then(|desired| match StockBundle::load(desired) {
            Ok(bundle) => Some(bundle),
            Err(error) => {
                eprintln!("Optional frontend update unavailable; using running bundle: {error}");
                None
            }
        });
    let installed = (|| -> anyhow::Result<Option<StockBundle>> {
        let Some(deployment) = cutex::launch::local_deployment::LocalDeployment::selected()? else {
            return Ok(None);
        };
        let manifest: StockBundle =
            serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)?;
        StockBundle::load_references(
            manifest
                .shared_config
                .path
                .parent()
                .context("bundle parent missing")?,
            &deployment.bundle_manifest,
            &cutex::agent_management::file_sha256(&deployment.bundle_manifest)?,
        )
        .map(Some)
    })()
    .unwrap_or_else(|error| {
        eprintln!("Optional installed frontend unavailable: {error}");
        None
    });
    let frontend = installed
        .as_ref()
        .into_iter()
        .chain(desired.as_ref())
        .find(|desired| {
            desired.version == 4
                && bundle.version == 4
                && desired.executable.sha256 == bundle.executable.sha256
                && desired.code_mode_host.sha256 == bundle.code_mode_host.sha256
                && desired.schema.sha256 == bundle.schema.sha256
        })
        .unwrap_or(&bundle);
    frontend.validate_frontend()?;
    timing.mark("frontend-selected");
    let cli = match &frontend.cli {
        Some(cli) => cli,
        None if frontend.version == 4 => anyhow::bail!(
            "this runtime deployment has no TUI frontend (codex) installed; install one to attach"
        ),
        None => &frontend.executable,
    };
    let actual_cwd = cutex::session::reviewed_registration::occurrence_launch_cwd(ready)?;
    let launch = foreground_terminal(
        clean_launch(&cli.path, &contract.native_home)?,
        std::env::vars(),
        cutex::config::store::load_codez_config_checked()?.terminal_truecolor,
    )
    .env(
        "CUTEX_NOTIFICATION_CONTROL",
        std::env::current_exe()?.to_string_lossy(),
    )
    .env(
        "CUTEX_AGENT_STATUS",
        cutex::launch::agent_status::for_record(record),
    )
    .args([
        "resume",
        &contract.native_id,
        "--no-alt-screen",
        "--cd",
        &actual_cwd,
        "-c",
        "tui.resume_cwd=\"current\"",
    ]);
    let layout = AppServerRuntimeLayout::from_binding(binding)?;
    let launch = launch.args(layout.remote_tui_args());
    let mut launch = configured(
        launch,
        &ready.review.configuration,
        false,
        matches!(contract.version, 3 | 4).then_some(contract.native_home.as_path()),
        &bundle,
        &contract.native_home,
    )?;
    if let Some(status) = ready
        .review
        .configuration
        .selected_projection
        .as_ref()
        .and_then(|p| p.status.as_ref())
    {
        launch = launch.arg("--status-items-file").arg(
            status
                .materialize_current_display_named(
                    super::account_store::load_store_read_only()?
                        .accounts
                        .iter()
                        .find(|a| a.id == ready.review.configuration.profile_id)
                        .map(|a| a.name.as_str()),
                )
                .context("Materialize foreground status display")?
                .to_str()
                .context("status path must be UTF-8")?,
        );
    }
    let status_items = ready
        .review
        .configuration
        .selected_projection
        .as_ref()
        .and_then(|p| p.settings.tui.as_ref())
        .and_then(|t| t.status_line.as_ref());
    launch = super::status_preferences::apply(
        launch,
        status_items,
        Some(&ready.review.configuration.profile_id),
    )?;
    timing.mark("display-and-profile-prepared");
    let (status, diagnostic) =
        foreground_status(&mut layout.apply_remote_tui_auth(launch).to_command())?;
    ensure!(
        status.success(),
        "stock CLI failed ({status}); owner was not restarted{}",
        if diagnostic.is_empty() {
            String::new()
        } else {
            format!(": {diagnostic}")
        }
    );
    Ok(status)
}

/// Keep terminal stdout/stdin intact while retaining a bounded stderr tail for
/// the selector's F2 diagnostics. Drain concurrently so a verbose child cannot
/// block on a full pipe. A descendant retaining stderr must not delay TUI return.
pub(super) fn foreground_status(
    command: &mut std::process::Command,
) -> anyhow::Result<(std::process::ExitStatus, String)> {
    use std::io::{Read, Write};
    const LIMIT: usize = 16 * 1024;
    let mut child = command.stderr(std::process::Stdio::piped()).spawn()?;
    let mut stderr = child
        .stderr
        .take()
        .context("foreground stderr pipe missing")?;
    let tail = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let captured = Arc::clone(&tail);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            let count = match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if let Ok(mut tail) = captured.lock() {
                tail.extend_from_slice(&chunk[..count]);
                let excess = tail.len().saturating_sub(LIMIT);
                tail.drain(..excess);
            }
            let _ = std::io::stderr().write_all(&chunk[..count]);
        }
        let _ = done_tx.send(());
    });
    let status = child.wait()?;
    let _ = done_rx.recv_timeout(std::time::Duration::from_secs(1));
    let diagnostic = tail
        .lock()
        .map(|tail| {
            String::from_utf8_lossy(&tail)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Ok((status, diagnostic))
}

/// Literal (non-secret) environment of the `cutex` MCP façade, written into
/// `mcp_servers.cutex.env` so only the façade gets it: the native process and
/// its shells do not (an agent shell's `cutex` CLI still reads its own home and
/// token, so it must not be pointed at another daemon's port).
///
/// The façade's browser tools call this daemon's Management API. Without
/// `CUTEX_MANAGEMENT_URL` the façade resolved the port from the OS user's
/// default `~/.cutex` (it sees neither `CUTEX_HOME` nor a test private home),
/// so an independent instance or a private dev home sent
/// `/v2/browser/commands` to another daemon, which refused every call with
/// `permission_denied`.
///
/// `CUTEX_MCP_APPROVAL_WAIT_SECS` opts the façade in to approval-wait: a
/// gated service / agent launch holds up to that long for the owner's
/// decision (Codex's MCP tool timeout is 300 s, so the hold fits). A pinned
/// older façade ignores it and answers `approval_required` at once.
fn facade_config_env(management_port: u16) -> serde_json::Value {
    serde_json::json!({
        "CUTEX_MANAGEMENT_URL": cutex::management::service::management_base_url(management_port),
        (cutex::approvals::APPROVAL_WAIT_ENV): cutex::approvals::APPROVAL_WAIT_SECS.to_string(),
    })
}

#[cfg(test)]
mod facade_env_tests {
    #[test]
    fn facade_gets_this_daemons_management_endpoint_only_via_mcp_config() {
        let env = super::facade_config_env(25070);
        let object = env.as_object().unwrap();
        assert_eq!(object.len(), 2, "the endpoint and the approval hold; secrets stay in env_vars");
        assert_eq!(object["CUTEX_MCP_APPROVAL_WAIT_SECS"], "120");
        let raw = object["CUTEX_MANAGEMENT_URL"].as_str().unwrap();
        // Exactly the shape the façade's browser transport accepts.
        let url = url::Url::parse(raw).unwrap();
        assert!(url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.path() == "/");
        assert!(url.query().is_none() && url.username().is_empty() && url.password().is_none());
        assert_eq!(url.port(), Some(25070));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod ingress_guard_tests {
    #[test]
    fn foreground_failure_retains_stderr_and_exit_code() {
        let (status, diagnostic) =
            super::foreground_status(std::process::Command::new("sh").args([
                "-c",
                "printf 'Error: status items file exceeds 8192 bytes\\n' >&2; exit 7",
            ]))
            .unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(diagnostic, "Error: status items file exceeds 8192 bytes");
    }

    #[test]
    fn existing_ready_owner_requires_successful_management_reconnect() {
        let native = "018e0000-0000-7000-8000-0001b000b82e";
        let mut record = cutex::session::model::CutexSessionRecord::new(
            "cutex.test".into(),
            Some(native.into()),
            "private".into(),
            "/private".into(),
            None,
        )
        .unwrap();
        record.runtime_generation = 1;
        let receipt: cutex::agent_management::StockRuntimeReceipt = serde_json::from_value(serde_json::json!({
            "launch_cwd":"/private", "action_id":"reviewed-register", "stage":"spawned", "claim_id":"claim", "runtime_agent_id":"stock.test", "expected_generation":1,
            "publication":null,"error":null,"updated_at":"2026-01-01T00:00:00Z",
            "binding":{"transport":"unix_socket","endpoint":"unix:///private/sock","pid":1234,"runtime_dir":"/private","launched_profile":"alpha","diagnostic_journal_path":"/private/journal","schema_version":"test","schema_sha256":"e".repeat(64),"started_at":"2026-01-01T00:00:00Z"},
            "review":{
                "subject":{"cutex_session_id":record.cutex_session_id,"formal_name":"formal","durable_sha256":"a".repeat(64),"authority_sha256":"a".repeat(64),"current_project_id":null,"revision":record.revision,"runtime_generation":0},
                "contract":{"version":2,"native_id":native,"native_home":"/private","bundle_manifest":"/private/manifest","bundle_sha256":"b".repeat(64)},
                "configuration":{"profile_name":"alpha","profile_id":"private-profile","inherited":false,"profile_sha256":"c".repeat(64),"account_sha256":"d".repeat(64),"model":"private-model","reasoning":null,"model_provider":"private","provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses","requires_openai_auth":false,"supports_websockets":false},"sandbox":"read-only","approval":"on-request"},
                "restart":false
            }
        })).unwrap();
        let mut sent = false;
        let result = super::reconnect_existing_owner(&record, &receipt, |path, body| {
            sent = true;
            assert_eq!(path, "/v2/sessions/cutex.test/cutex/requests");
            let request: serde_json::Value = serde_json::from_slice(body)?;
            assert_eq!(request["method"], "cutex/runtime/online");
            assert_eq!(request["params"]["expectedRuntimeGeneration"], 1);
            Ok(serde_json::json!({"contractVersion":2,"requestId":request["requestId"],
                "cutexSessionId":record.cutex_session_id,"cutex":{"method":"cutex/runtime/online",
                "result":{"status":"online","runtimeGeneration":1,"runtimeAgentId":receipt.runtime_agent_id,
                "actionId":receipt.action_id}}}))
        }).unwrap();
        assert!(sent);
        assert_eq!(result, receipt);
        assert!(
            super::reconnect_existing_owner(&record, &receipt, |_, _| anyhow::bail!(
                "bridge unavailable"
            ))
            .is_err()
        );
        assert!(super::reconnect_existing_owner(&record, &receipt, |_, body| {
            let request: serde_json::Value = serde_json::from_slice(body)?;
            Ok(serde_json::json!({"contractVersion":2,"requestId":request["requestId"],
                "cutexSessionId":record.cutex_session_id,"cutex":{"method":"cutex/runtime/online",
                "result":{"status":"online","runtimeGeneration":2,"runtimeAgentId":receipt.runtime_agent_id,
                "actionId":receipt.action_id}}}))
        }).is_err());
    }

    #[test]
    fn foreground_tmpdir_defaults_without_shell_export_and_honors_override() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("stock-tmp").unwrap();
        let tmp = super::launch_tmpdir(None).unwrap();
        assert_eq!(tmp, home.root().join(".cutex/runtime/stock-tmp"));
        assert!(tmp.is_dir());
        assert_eq!(super::launch_tmpdir(Some("")).unwrap(), tmp);
        assert_eq!(
            super::launch_tmpdir(Some("/custom/tmp")).unwrap(),
            std::path::PathBuf::from("/custom/tmp")
        );
        // An agent shell of another cutex home exports that home's stock-tmp.
        assert_eq!(
            super::launch_tmpdir(Some("/other/home/.cutex/runtime/stock-tmp")).unwrap(),
            tmp
        );
        assert_eq!(
            super::launch_tmpdir(Some("/other/home/.cutex/runtime/stock-tmp/cb-1234")).unwrap(),
            tmp
        );
        // This home's own stock-tmp (e.g. a nested `cutex` call) is kept.
        let own = tmp.join("nested");
        assert_eq!(super::launch_tmpdir(own.to_str()).unwrap(), own);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(tmp).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn coherent_receiver_profiles_are_explicit_and_unknown_is_not_full_access() {
        for (sandbox, expected) in [
            ("read-only", ":read-only"),
            ("workspace-write", ":workspace"),
            ("danger-full-access", ":danger-full-access"),
        ] {
            assert_eq!(super::receiver_profile(sandbox).unwrap(), expected);
        }
        for unknown in ["", "full-access", "managed", "inherit", "unknown"] {
            assert!(super::receiver_profile(unknown).is_err());
        }
    }
}

#[cfg(all(test, windows))]
mod windows_environment_tests {
    use super::*;
    use crate::cli_app::native_stop;
    use cutex::session::model::{CutexAppServerTransport, CutexSessionRecord};

    #[test]
    fn windows_launch_preserves_os_environment_and_can_run_a_child() {
        let _home = crate::cli_app::test_home::IsolatedTestHome::new("windows-launch-env").unwrap();
        let program = std::path::PathBuf::from(std::env::var("COMSPEC").unwrap());
        let home = std::path::PathBuf::from(std::env::var("CUTEX_TEST_PRIVATE_HOME").unwrap());
        let launch = clean_launch(&program, &home)
            .unwrap()
            .args(["/d", "/c", "exit", "0"]);
        assert!(
            launch
                .envs
                .iter()
                .any(|(key, value)| key == "PATH" && value == &std::env::var("PATH").unwrap())
        );
        assert!(launch.envs.iter().any(|(key, _)| key == "SystemRoot"));
        assert!(
            !launch
                .envs
                .iter()
                .any(|(key, _)| matches!(key.as_str(), "OPENAI_API_KEY" | "CUTEX_AGENT_BUS_TOKEN"))
        );
        assert!(launch.to_command().status().unwrap().success());
    }

    #[test]
    fn windows_dead_owner_reconciles_without_historical_package() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("windows-dead-owner")
            .expect("isolated test home");
        let native = uuid::Uuid::new_v4().to_string();
        let mut record = CutexSessionRecord::new(
            format!("cutex.{native}"),
            Some(native.clone()),
            "private".into(),
            home.root().display().to_string(),
            None,
        )
        .unwrap();
        record.host_id = cutex::platform::host::current_host_name();
        record.explicit_launch = Some(cutex::agent_management::ExplicitLaunchContract {
            version: 1,
            migration_action_id: None,
            native_id: native,
            native_home: home.root().into(),
            bundle_manifest: home.root().join("removed-historical-bundle.json"),
            bundle_sha256: cutex::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        let binding = cutex::session::model::CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::LoopbackWebSocket,
            endpoint: "ws://127.0.0.1:9".into(),
            pid: 0,
            runtime_dir: home.root().join("runtime").display().to_string(),
            launched_profile: None,
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: home.root().join("journal.jsonl").display().to_string(),
            schema_version: "test".into(),
            schema_sha256: "a".repeat(64),
            // After this boot: an older start time reads as a previous-boot occurrence
            // (`native_owner_previous_boot`) on a host booted since then.
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        record.app_server_runtime = Some(binding.clone());
        assert!(stock_runtime_has_exited(&record, &binding).unwrap());
        let outcome = native_stop::stop_processes(&record, false).unwrap();
        assert!(outcome.stopped);
        assert_eq!(outcome.detail, "owned_runtime_process_absent");
    }
}

#[cfg(test)]
mod foreground_terminal_tests {
    use super::*;
    #[test]
    fn frontend_truecolor_opt_in_supplies_ssh_marker_only_to_child() {
        let launch = foreground_terminal(
            LaunchCommand::new("native"),
            [("COLORTERM".into(), "old".into())],
            true,
        );
        assert_eq!(
            launch
                .envs
                .iter()
                .filter(|(key, _)| key == "COLORTERM")
                .count(),
            1
        );
        assert!(
            launch
                .envs
                .contains(&("COLORTERM".into(), "truecolor".into()))
        );
        let auto = foreground_terminal(LaunchCommand::new("native"), [], false);
        assert!(!auto.envs.iter().any(|(key, _)| key == "COLORTERM"));
    }
    #[test]
    fn frontend_preserves_terminal_color_capabilities_and_user_overrides() {
        let launch = foreground_terminal(
            LaunchCommand::new("native").env("TERM", "xterm-256color"),
            [
                ("TERM", "xterm-direct"),
                ("COLORTERM", "truecolor"),
                ("NO_COLOR", "1"),
                ("WT_SESSION", "terminal-id"),
                ("OPENAI_API_KEY", "must-not-forward"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
            false,
        );
        assert!(
            launch
                .envs
                .contains(&("COLORTERM".into(), "truecolor".into()))
        );
        assert!(launch.envs.contains(&("NO_COLOR".into(), "1".into())));
        assert!(
            launch
                .envs
                .contains(&("WT_SESSION".into(), "terminal-id".into()))
        );
        assert_eq!(launch.envs.iter().filter(|(k, _)| k == "TERM").count(), 1);
        assert!(
            launch
                .envs
                .contains(&("TERM".into(), "xterm-direct".into()))
        );
        assert!(!launch.envs.iter().any(|(k, _)| k == "OPENAI_API_KEY"));
    }
}

/// Opt-in stage timing contains no credentials, paths, arguments, or conversation text.
struct StartupTiming {
    started: std::time::Instant,
    previous: std::time::Instant,
    enabled: bool,
}
impl StartupTiming {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            previous: now,
            enabled: std::env::var_os("CUTEX_STARTUP_TIMING").is_some_and(|v| v == "1"),
        }
    }
    fn mark(&mut self, stage: &str) {
        let now = std::time::Instant::now();
        if self.enabled {
            eprintln!(
                "Cutex startup: {stage}: {} ms (total {} ms)",
                now.duration_since(self.previous).as_millis(),
                now.duration_since(self.started).as_millis()
            );
        }
        self.previous = now;
    }
}
