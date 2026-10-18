use std::sync::Mutex;
use std::sync::OnceLock;

use cutex::im::registry::ImRegistry;
use cutex::management::server::ManagementNativeForwardError;
use cutex::management::server::ManagementRequestContext;
use cutex::platform::process::process_is_running;
use cutex::session::service::coding_registration_from_cutex_session_record;
use cutex::session::store::load_cutex_session_store;

static MANAGEMENT_V2_SESSION_MUTATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// The lock every Management v2 session mutation holds (delete/fork too).
pub(super) fn lock_session_mutations() -> Result<
    std::sync::MutexGuard<'static, ()>,
    cutex::management::v2::user_input::UserInputExecutionError,
> {
    MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| session_mutation_invalid("management v2 session mutation lock poisoned"))
}

pub(crate) fn load_management_v2_registry() -> anyhow::Result<ImRegistry> {
    Ok(ImRegistry::default())
}

pub(crate) fn management_request_context() -> ManagementRequestContext {
    ManagementRequestContext {
        explicit_launch_action,
        adopt_saved_native,
        review_agent_archive,
        execute_agent_archive,
        durable_agent_candidates,
        import_durable_agent,
        load_registry: load_management_v2_registry,
        load_runtime_status: load_app_server_runtime_status,
        forward_native_request: forward_management_native_request,
        respond_native_server_request: respond_management_native_server_request,
        handle_user_input: super::app_server_user_input::submit_v2,
        flush_user_input_queue: flush_management_user_input_queue,
        steer_user_input_queue: super::app_server_user_input::steer_queued_v2,
        interrupt_active_turn: super::app_server_user_input::interrupt_active_turn_v2,
        load_bootstrap_state: load_management_bootstrap_state,
        load_latest_plan: super::session_history::latest_plan_for_management,
        load_latest_token_usage: super::session_history::latest_token_usage_for_management,
        revert_session_history: super::session_history::revert_for_management,
        delete_session: super::session_delete::delete_for_management,
        fork_session: super::session_fork::fork_for_management,
        side_chat: super::side_chats::dispatch,
        open_offline_history: super::session_history::open_offline_history_endpoint,
        mutate_session: mutate_management_v2_session,
        online_session: online_management_v2_session,
        bind_project_authority: super::agent_management::bind_project_authority,
        import_legacy_director_ownership: super::agent_management::import_legacy_director_ownership,
        reconcile_agent_reservation: super::agent_management::reconcile_agent_reservation,
        list_management_projects,
        read_management_project,
        update_management_project_presentation,
        execute_management_operator_action,
        execute_management_project_mutation,
        create_human_agent,
        create_human_session,
    }
}

/// Clear a launch claim whose creator and child are proven gone (a crashed
/// launch), or whose launch already failed readiness (its live child is
/// stopped first). A live launch keeps its claim and surfaces
/// `launch_in_progress`.
pub(super) fn auto_clear_dead_launch_claim(key: &str) -> anyhow::Result<()> {
    let id = cutex::role_revision::CutexSessionId::new(key.to_string())
        .map_err(|_| anyhow::anyhow!("invalid durable Agent ID"))?;
    let cleared = management_agent_provider()?.clear_dead_launch_claim(
        &cutex::session::store::cutex_sessions_path()?,
        &id,
        &mut super::stock_lifecycle::launch_claim_owner_alive,
        // A child whose launch already failed readiness is abandoned: stop
        // that exact occurrence (PID + start time verified) before clearing.
        &mut |record| {
            let outcome = super::native_stop::stop_and_commit(record, true)?;
            anyhow::ensure!(outcome.stopped, "runtime stop incomplete: {}", outcome.detail);
            Ok(())
        },
    )?;
    if cleared {
        eprintln!("cleared abandoned launch claim for {key}: owner process absent");
    }
    Ok(())
}

fn has_runtime_claim(record: &cutex::session::model::CutexSessionRecord) -> bool {
    cutex::session::archive::record_has_runtime_claim(record) || record.pending_launch_id.is_some()
}

fn runtime_claim_fields(record: &cutex::session::model::CutexSessionRecord) -> String {
    let mut fields = Vec::new();
    if let Some(claim) = &record.app_server_launch_claim_id {
        fields.push(format!("launch claim {claim}"));
    }
    if let Some(binding) = &record.app_server_runtime {
        fields.push(format!("app-server binding (pid {})", binding.pid));
    }
    if let Some(pid) = record.runtime_pid {
        fields.push(format!("runtime pid {pid}"));
    }
    if let Some(pid) = record.alden_pid {
        fields.push(format!("alden pid {pid}"));
    }
    if let Some(agent) = &record.current_runtime_agent_id {
        fields.push(format!("runtime agent {agent}"));
    }
    if let Some(launch) = &record.pending_launch_id {
        fields.push(format!("pending launch {launch}"));
    }
    fields.join(", ")
}

/// Clear the runtime claim an offline explicit-launch agent still carries
/// from a runtime that exited (or a launch that crashed): the reconciliation
/// `cutex/runtime/online` does before a start, without starting anything.
/// Only proven absence clears: a live launch owner, a running (possibly
/// reused) PID or an unverifiable binding keeps the claim and is reported.
/// Nothing is stopped or killed here. Returns what was cleared.
fn reconcile_exited_explicit_runtime(key: &str) -> anyhow::Result<String> {
    let _guard = MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("management v2 session mutation lock poisoned"))?;
    let load = || -> anyhow::Result<cutex::session::model::CutexSessionRecord> {
        cutex::session::store::with_cutex_session_store(|store| store.sessions.get(key).cloned())?
            .ok_or_else(|| anyhow::anyhow!("agent disappeared"))
    };
    let record = load()?;
    anyhow::ensure!(record.explicit_launch.is_some(), "not an explicit-launch agent");
    anyhow::ensure!(!record.is_retired(), "restore the archived Agent first");
    anyhow::ensure!(
        cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &cutex::platform::host::current_host_name()
        ),
        "manage this runtime on its owning host"
    );
    if !has_runtime_claim(&record) {
        return Ok("no runtime claim".into());
    }
    let mut cleared = Vec::new();
    if record.app_server_launch_claim_id.is_some() {
        // Refuses (launch_in_progress) while the launch owner is alive.
        auto_clear_dead_launch_claim(key)?;
        if load()?.app_server_launch_claim_id.is_none() {
            cleared.push("abandoned launch claim".to_string());
        }
    }
    let record = load()?;
    if let Some(binding) = &record.app_server_runtime {
        anyhow::ensure!(
            super::stock_lifecycle::stock_runtime_has_exited(&record, binding)?,
            "runtime process {} is still running; take the agent offline or stop it first",
            binding.pid
        );
        let stopped = super::native_stop::stop_and_commit(&record, false)?;
        anyhow::ensure!(stopped.stopped, "exited runtime cleanup incomplete: {}", stopped.detail);
        cleared.push(format!("exited runtime (pid {})", binding.pid));
    }
    let record = load()?;
    if record.app_server_runtime.is_none()
        && record.app_server_launch_claim_id.is_none()
        && record.pending_launch_id.is_none()
        && cutex::session::archive::record_has_runtime_claim(&record)
    {
        // Leftover ids without a binding (older runtimes): a PID that is
        // running cannot be told apart from a reused one, so it stays.
        if let Some(pid) = record.runtime_pid.into_iter().chain(record.alden_pid).find(|pid| process_is_running(*pid)) {
            anyhow::bail!(
                "recorded runtime pid {pid} is running (possibly a reused PID); the claim was left in place"
            );
        }
        let fields = runtime_claim_fields(&record);
        cutex::agent_management::commit_stock_runtime_stop(
            &cutex::session::store::cutex_sessions_path()?,
            &record,
        )?;
        cleared.push(format!("leftover runtime ids ({fields})"));
    }
    let record = load()?;
    anyhow::ensure!(
        !has_runtime_claim(&record),
        "runtime claim remains: {}",
        runtime_claim_fields(&record)
    );
    Ok(format!("cleared {}", cleared.join("; ")))
}

fn explicit_launch_action(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::ExplicitLaunchRequest,
) -> Result<serde_json::Value, cutex::agent_management::AgentManagementError> {
    let operation = || -> anyhow::Result<_> {
        if let cutex::agent_management::ExplicitLaunchRequest::RuntimeStatus { action_id } = request
        {
            return Ok(serde_json::to_value(
                management_agent_provider()?.runtime_action_status(
                    &cutex::session::store::cutex_sessions_path()?,
                    action_id,
                )?,
            )?);
        }
        if let cutex::agent_management::ExplicitLaunchRequest::RecoverRuntime {
            cutex_session_id,
            action_id,
        } = request
        {
            let mut runtime = super::stock_lifecycle::StockExecutor::default();
            return Ok(serde_json::to_value(
                management_agent_provider()?.recover_failed_runtime(
                    principal,
                    &cutex::session::store::cutex_sessions_path()?,
                    cutex_session_id,
                    action_id,
                    &mut runtime,
                )?,
            )?);
        }
        if let cutex::agent_management::ExplicitLaunchRequest::ReconcileExitedRuntime {
            cutex_session_id,
        } = request
        {
            return Ok(serde_json::json!({
                "detail": reconcile_exited_explicit_runtime(cutex_session_id.as_str())?,
            }));
        }
        if let cutex::agent_management::ExplicitLaunchRequest::ReviewRuntime {
            cutex_session_id,
            ..
        } = request
        {
            auto_clear_dead_launch_claim(cutex_session_id.as_str())?;
        }
        #[cfg(target_os = "linux")]
        if let cutex::agent_management::ExplicitLaunchRequest::MaintenanceStart { action_id } =
            request
        {
            let mut runtime = super::stock_lifecycle::StockExecutor::default();
            return Ok(serde_json::to_value(
                management_agent_provider()?.start_maintenance(
                    principal,
                    &cutex::session::store::cutex_sessions_path()?,
                    action_id,
                    &mut runtime,
                )?,
            )?);
        }
        #[cfg(target_os = "linux")]
        if let cutex::agent_management::ExplicitLaunchRequest::MaintenanceRecoveryStart {
            request,
        } = request
        {
            let mut runtime = super::stock_lifecycle::StockExecutor::default();
            return Ok(serde_json::to_value(
                management_agent_provider()?.recover_start_maintenance(
                    principal,
                    &cutex::session::store::cutex_sessions_path()?,
                    request,
                    &mut runtime,
                )?,
            )?);
        }
        if let cutex::agent_management::ExplicitLaunchRequest::Run { action_id, review } = request {
            let mut runtime = super::stock_lifecycle::StockExecutor::default();
            return Ok(serde_json::to_value(
                management_agent_provider()?.execute_stock_runtime(
                    principal,
                    &cutex::session::store::cutex_sessions_path()?,
                    action_id,
                    review,
                    &mut runtime,
                )?,
            )?);
        }
        management_agent_provider()?.explicit_launch_action(
            principal,
            &cutex::session::store::cutex_sessions_path()?,
            request,
        )
    };
    operation().map_err(|e| {
        cutex::agent_management::AgentManagementError::OwnerActionRequired(format!("{e:#}"))
    })
}

fn adopt_saved_native(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::HumanAdoptRequest,
) -> Result<cutex::agent_management::HumanAdoptResult, cutex::agent_management::AgentManagementError>
{
    adopt_native(principal, request, false, None, None)
}

/// Adoption of a thread this process just created with
/// `light_new::create_native`, which already read the persisted empty thread
/// (identity, cwd, paginated history) back from the same native home. The
/// source re-read would spawn a second native only to repeat that check.
pub(super) fn adopt_created_native(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::HumanAdoptRequest,
) -> Result<cutex::agent_management::HumanAdoptResult, cutex::agent_management::AgentManagementError>
{
    adopt_native(principal, request, true, None, None)
}

/// Adoption of a thread this process just forked (and read back) on the
/// source session's own native home `history_home`, which is not the
/// selected deployment's home when the source has its own runtime home.
/// `source_profile` (the source session's profile) keeps the fork on that
/// profile when the request carries no creation defaults.
pub(super) fn adopt_forked_native(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::HumanAdoptRequest,
    history_home: &std::path::Path,
    source_profile: Option<&str>,
) -> Result<cutex::agent_management::HumanAdoptResult, cutex::agent_management::AgentManagementError>
{
    adopt_native(principal, request, true, Some(history_home), source_profile)
}

fn adopt_native(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::HumanAdoptRequest,
    source_verified: bool,
    history_home: Option<&std::path::Path>,
    source_profile: Option<&str>,
) -> Result<cutex::agent_management::HumanAdoptResult, cutex::agent_management::AgentManagementError>
{
    let operation = || -> anyhow::Result<_> {
        let mut timing = super::light_new::CreateTiming::new("adopt_saved_native");
        let path = cutex::session::store::cutex_sessions_path()?;
        let store = cutex::session::store::load_cutex_session_store()?;
        if !source_verified
            && !store
                .human_adoption_receipts
                .contains_key(request.action_id.as_str())
        {
            use cutex::catalog::CatalogEndpoint;
            let launch = super::session_native_workflow::NativeLaunch {
                cwd: request.cwd.clone().into(),
                native_home: cutex::launch::local_deployment::LocalDeployment::source_home()?,
                profile: None,
                model: None,
            };
            let mut endpoint = launch.endpoint()?;
            let native = endpoint.request(
                "thread/read",
                serde_json::json!({"threadId":request.native_id,"includeTurns":false}),
            )?;
            anyhow::ensure!(
                native
                    .pointer("/thread/id")
                    .and_then(serde_json::Value::as_str)
                    == Some(request.native_id.as_str())
                    && native
                        .pointer("/thread/cwd")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|cwd| std::path::Path::new(cwd).canonicalize().ok())
                        == Some(std::path::Path::new(&request.cwd).canonicalize()?),
                "native source/cwd changed; review again"
            );
        }
        timing.phase("verify thread/read");
        let result = management_agent_provider()?.adopt_saved_native_from(
            principal,
            &path,
            request,
            &cutex::platform::host::current_host_name(),
            history_home,
            source_profile,
        );
        timing.phase("provider adopt");
        result
    };
    operation().map_err(|e| {
        cutex::agent_management::AgentManagementError::OwnerActionRequired(format!("{e:#}"))
    })
}

fn review_agent_archive(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::AgentArchiveReviewRequest,
) -> Result<
    cutex::agent_management::AgentArchiveReview,
    cutex::agent_management::AgentManagementError,
> {
    let path = cutex::session::store::cutex_sessions_path()
        .map_err(|_| cutex::agent_management::AgentManagementError::PersistenceUnavailable)?;
    management_agent_provider()?.review_agent_archive(
        principal,
        &path,
        &request.cutex_session_id,
        request.operation,
    )
}

fn execute_agent_archive(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::AgentArchiveRequest,
) -> Result<
    cutex::agent_management::AgentArchiveReceipt,
    cutex::agent_management::AgentManagementError,
> {
    let path = cutex::session::store::cutex_sessions_path()
        .map_err(|_| cutex::agent_management::AgentManagementError::PersistenceUnavailable)?;
    let _guard = MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| cutex::agent_management::AgentManagementError::PersistenceUnavailable)?;
    let provider = management_agent_provider()?;
    if request.review.operation == cutex::agent_management::AgentArchiveOperation::Archive {
        let blockers = provider.agent_archive_blockers(&request.review.cutex_session_id)?;
        if !blockers.director_projects.is_empty() {
            return Err(cutex::agent_management::AgentManagementError::Conflict(
                "archive_requires_explicit_director_rotation",
            ));
        }
        stop_runtime_before_archive(request.review.cutex_session_id.as_str())
            .map_err(|error| cutex::agent_management::AgentManagementError::OwnerActionRequired(error.message))?;
    }
    provider.execute_agent_archive(
        principal,
        &path,
        request,
        &mut super::management_archive::OfflineArchiveRuntime,
    )
}

fn durable_agent_candidates(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
) -> Result<
    Vec<cutex::agent_management::DurableAgentCandidate>,
    cutex::agent_management::AgentManagementError,
> {
    let path = cutex::session::store::cutex_sessions_path()
        .map_err(|_| cutex::agent_management::AgentManagementError::PersistenceUnavailable)?;
    let mut candidates = management_agent_provider()?.durable_agent_candidates(principal, &path)?;
    let observer = ManagementProjectRuntimeObserver::load()?;
    for candidate in &mut candidates {
        let Some(id) = candidate.cutex_session_id.as_ref() else {
            continue;
        };
        candidate.online = observer
            .sessions
            .sessions
            .get(id.as_str())
            .is_some_and(|record| {
                record.cutex_session_id == id.as_str()
                    && observer.live_agents.iter().any(|agent| {
                        agent.cutex_session_id.as_deref() == Some(id.as_str())
                            && record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
                            && process_is_running(agent.pid)
                    })
            });
    }
    Ok(candidates)
}

fn import_durable_agent(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::agent_management::DurableImportRequest,
) -> Result<
    cutex::agent_management::DurableImportReceipt,
    cutex::agent_management::AgentManagementError,
> {
    let path = cutex::session::store::cutex_sessions_path()
        .map_err(|_| cutex::agent_management::AgentManagementError::PersistenceUnavailable)?;
    management_agent_provider()?.import_durable_agent(principal, &path, request)
}

fn human_agent_error(
    code: &str,
    message: impl Into<String>,
    details: serde_json::Value,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "human_agent".to_string(),
        code: code.to_string(),
        message: message.into(),
        retryable: false,
        details,
        outcome_unknown: false,
    }
}

fn human_agent_validation(
    code: &str,
    message: impl Into<String>,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    let mut error = human_agent_error(code, message, serde_json::json!({}));
    error.stage = "validation".to_string();
    error
}

/// Launch settings of an owner-created Claude agent / quick chat: the
/// profile's Claude defaults (model, effort, write / exec rules), the
/// per-profile new-session model, then the request's model / effort. The
/// Codex-only permission overrides are refused (Claude uses the profile's
/// write / exec rules).
fn claude_launch_defaults(
    account: &cutex::profiles::model::StoredAccount,
    config: &cutex::profiles::model::CodezConfig,
    launch: Option<&cutex::config::new_thread::LaunchOverrides>,
) -> Result<cutex::profiles::appearance::ClaudeDefaults, cutex::management::v2::user_input::UserInputExecutionError>
{
    if launch.is_some_and(|launch| launch.has_permission_overrides()) {
        return Err(human_agent_validation(
            "unsupported_launch_option",
            CLAUDE_CODEX_ONLY_LAUNCH_OPTIONS,
        ));
    }
    if let Some(mode) = launch.and_then(|launch| launch.claude_permission_mode.as_deref()) {
        if !cutex::runtime::claude::launch_permission_modes().iter().any(|offered| offered == mode) {
            return Err(human_agent_validation(
                "unsupported_launch_option",
                format!("the installed Claude runtime does not offer the permission mode `{mode}`; update the claude-runtime component"),
            ));
        }
    }
    let mut defaults = account.options.claude.clone();
    if let Some(model) = config.new_session_defaults.get(&account.name).and_then(|d| d.model.clone()) {
        defaults.model = model;
    }
    if let Some(launch) = launch {
        if let Some(model) = &launch.model {
            defaults.model = model.clone();
        }
        if let Some(effort) = &launch.reasoning_effort {
            defaults.effort = cutex::profiles::appearance::ClaudeEffort::parse(effort)
                .map_err(|error| human_agent_validation("unsupported_launch_option", format!("{error:#}")))?;
        }
    }
    Ok(defaults)
}

const CLAUDE_CODEX_ONLY_LAUNCH_OPTIONS: &str = "Claude profiles use their own write/exec permissions; sandbox, approvalPolicy, networkAccess and writableRoots are Codex-only";

/// `launch.claudePermissionMode` on a Codex profile.
fn refuse_claude_mode_for_codex(
    launch: Option<&cutex::config::new_thread::LaunchOverrides>,
) -> Result<(), cutex::management::v2::user_input::UserInputExecutionError> {
    if launch.is_some_and(|launch| launch.claude_permission_mode.is_some()) {
        return Err(human_agent_validation(
            "unsupported_launch_option",
            "claudePermissionMode applies to Claude profiles only",
        ));
    }
    Ok(())
}

/// Root-only `POST /v2/human/agents`: `cutex human new` (Codex profiles) or
/// the Claude creator + roster import (Claude profiles, `human-agent-claude`)
/// + visible + profile
/// + optional Project membership (cwd defaults to the Project workspace
/// root) + optional start. The cwd is used exactly; no per-agent
/// subdirectory is created. Without cwd and Project the agent gets a private
/// scratch workspace `<config dir>/agents-scratch/<id>`
/// (`human-agent-scratch-cwd`).
fn create_human_agent(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::management::v2::human_agents::HumanCreateAgentRequest,
) -> Result<String, cutex::management::v2::user_input::UserInputExecutionError> {
    use cutex::management::control_plane::{
        HumanManagementProjectMutationKind, HumanManagementProjectMutationRequest,
        HumanManagementProjectMutationSchema,
    };
    request.validate()?;
    // Clients may select the profile by id (the desktop picker does); every
    // later step (defaults, record, profile check) uses the canonical name.
    let requested_profile = request
        .profile
        .as_deref()
        .map(cutex::config::new_session::resolve_profile_name)
        .transpose()
        .map_err(|error| human_agent_validation("unsupported_profile", format!("{error:#}")))?;
    // 1. Resolve the Project (if any) and the cwd. No side effects yet.
    let project = match &request.project_id {
        Some(project_id) => {
            let projects = list_management_projects(principal).map_err(|error| {
                human_agent_error(
                    "event_persistence_unavailable",
                    error.to_string(),
                    serde_json::json!({}),
                )
            })?;
            let summary = projects
                .projects
                .into_iter()
                .find(|project| project.project_id.as_str() == project_id)
                .ok_or_else(|| {
                    let mut error = human_agent_validation(
                        "project_not_found",
                        "the active Project does not exist on this host",
                    );
                    error.details = serde_json::json!({ "projectId": project_id });
                    error
                })?;
            Some(summary)
        }
        None => None,
    };
    // `None` = neither cwd nor Project: a private scratch workspace
    // (`human-agent-scratch-cwd`), allocated only after every validation.
    let given_cwd = match (&request.cwd, project.as_ref()) {
        (Some(cwd), _) => Some(cwd.clone()),
        (None, Some(project)) => Some(project.workspace_root.clone().ok_or_else(|| {
            human_agent_validation(
                "workspace_root_required",
                "the Project has no workspace_root; pass cwd or set the Project workspace root",
            )
        })?),
        (None, None) => None,
    };
    if let Some(cwd) = &given_cwd {
        if !std::path::Path::new(cwd).is_dir() {
            return Err(human_agent_validation(
                "invalid_request",
                "cwd must be an existing directory",
            ));
        }
    }
    let config = cutex::config::store::load_codez_config_checked()
        .map_err(|error| human_agent_validation("invalid_request", format!("{error:#}")))?;
    let notification = config.new_session_notification;
    // Claude subscription profiles have their own creator. Without an
    // explicit profile the global default profile decides (as for Codex).
    let effective_profile = match &requested_profile {
        Some(profile) => Some(profile.clone()),
        None => config
            .default_profile
            .as_deref()
            .map(cutex::config::new_session::resolve_profile_name)
            .transpose()
            .map_err(|error| human_agent_validation("unsupported_profile", format!("{error:#}")))?,
    };
    let claude = match &effective_profile {
        Some(profile) => super::claude_profile::account(profile)
            .map_err(|error| human_agent_validation("unsupported_profile", format!("{error:#}")))?,
        None => None,
    };

    // Launch settings are validated before any side effect (the scratch
    // directory below), so a validation answer never leaves a directory.
    let claude_defaults = claude
        .as_ref()
        .map(|account| claude_launch_defaults(account, &config, request.launch.as_ref()))
        .transpose()?;
    if claude.is_none() {
        refuse_claude_mode_for_codex(request.launch.as_ref())?;
    }
    let configuration = match &claude {
        Some(_) => None,
        None => Some(
            super::light_new::human_configuration(
                requested_profile.as_deref(),
                cutex::config::new_thread::ThreadKind::Agent,
                request.launch.as_ref(),
            )
            .map_err(|error| human_agent_validation("unsupported_profile", format!("{error:#}")))?,
        ),
    };
    let scratch = match &given_cwd {
        Some(_) => None,
        None => Some(cutex::management::v2::human_agents::create_agent_scratch_dir().map_err(|error| {
            human_agent_error("scratch_dir_failed", format!("{error:#}"), serde_json::json!({}))
        })?),
    };
    let cwd = match (&given_cwd, &scratch) {
        (Some(cwd), _) => cwd.clone(),
        (None, Some(dir)) => dir.to_string_lossy().into_owned(),
        (None, None) => unreachable!("scratch allocated above"),
    };
    // Creation failed before an agent exists: drop an untouched scratch dir.
    let creation_failed = |error: anyhow::Error| {
        if let Some(dir) = &scratch {
            let _ = std::fs::remove_dir(dir);
        }
        human_agent_error("agent_creation_failed", format!("{error:#}"), serde_json::json!({}))
    };

    // 2. Create the agent: a Claude profile through the Claude creator
    //    (same as agent_create / quick chat) + roster import; a Codex profile
    //    as `cutex human new` (native thread + adopt).
    let id = match (&claude, configuration) {
        (Some(account), _) => {
            let defaults = claude_defaults.expect("Claude defaults resolved above");
            let record = super::light_new::canonical_cwd(std::path::Path::new(&cwd))
                .and_then(|cwd| super::claude_profile::create(account, &request.name, &cwd, true, defaults, (None, None)))
                .map_err(creation_failed)?;
            let id = record.cutex_session_id.clone();
            if let Some(mode) = request.launch.as_ref().and_then(|launch| launch.claude_permission_mode.as_deref()) {
                cutex::runtime::claude::set_initial_permission_mode(&id, mode).map_err(|error| {
                    human_agent_error(
                        "agent_setup_incomplete",
                        format!("agent {id} was created but its permission mode failed: {error:#}"),
                        serde_json::json!({ "cutexSessionId": id, "step": "permission mode" }),
                    )
                })?;
            }
            let incomplete = |step: &str, message: String| {
                human_agent_error(
                    "agent_setup_incomplete",
                    format!("agent {id} was created but {step} failed: {message}"),
                    serde_json::json!({ "cutexSessionId": id, "step": step }),
                )
            };
            let thread = record
                .app_server_thread_id()
                .ok_or_else(|| incomplete("notification", "Claude thread missing".to_string()))?;
            cutex::notify::session::session(thread, cutex::notify::session::Change::Set(notification))
                .map_err(|error| incomplete("notification", format!("{error:#}")))?;
            super::claude_structure::import_claude_agent(principal, &id, &request.name, "human-agent-import")
                .map_err(|error| incomplete("roster import", format!("{error:#}")))?;
            id
        }
        (None, Some(configuration)) => {
            let adopted = super::light_new::create_configured_with(
                &request.name,
                &cwd,
                configuration,
                notification,
                |adopt| adopt_created_native(principal, adopt).map_err(|error| anyhow::anyhow!("{error}")),
            )
            .map_err(creation_failed)?;
            let id = adopted.adopted.record.cutex_session_id.clone();
            if let Some(error) = &adopted.error {
                return Err(human_agent_error(
                    "agent_setup_incomplete",
                    format!("agent {id} was created but roster import failed: {error}"),
                    serde_json::json!({ "cutexSessionId": id, "step": "roster import" }),
                ));
            }
            id
        }
        (None, None) => unreachable!("Codex configuration resolved above"),
    };
    let incomplete = |step: &str, message: String| {
        human_agent_error(
            "agent_setup_incomplete",
            format!("agent {id} was created but {step} failed: {message}"),
            serde_json::json!({ "cutexSessionId": id, "step": step }),
        )
    };
    let current = |id: &str| -> Result<CutexSessionRecordSnapshot, String> {
        cutex::session::store::with_cutex_session_store(|store| {
            store
                .sessions
                .get(id)
                .map(|record| CutexSessionRecordSnapshot {
                    revision: record.durable_revision(),
                    exposed: record.exposed_to_backend,
                    profile: record.profile.clone(),
                    runtime_generation: record.runtime_generation,
                })
        })
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(|| "agent disappeared".to_string())
    };

    // 3. Visible to every client.
    let snapshot = current(&id).map_err(|error| incomplete("visibility", error))?;
    if !snapshot.exposed {
        mutate_management_v2_session(
            &id,
            "cutex/session/visibility/show",
            serde_json::json!({ "expectedRevision": snapshot.revision }),
        )
        .map_err(|error| incomplete("visibility", error.message))?;
    }

    // 4. Profile (creation defaults normally already persisted it).
    if let Some(profile) = &requested_profile {
        let snapshot = current(&id).map_err(|error| incomplete("profile", error))?;
        if snapshot.profile.as_deref() != Some(profile.as_str()) {
            mutate_management_v2_session(
                &id,
                "cutex/session/profile/set",
                serde_json::json!({ "expectedRevision": snapshot.revision, "profile": profile }),
            )
            .map_err(|error| incomplete("profile", error.message))?;
        }
    }

    // 5. Project membership through the normal Human mutation path.
    if let Some(project) = &project {
        let member = cutex::role_revision::CutexSessionId::new(id.clone())
            .map_err(|_| incomplete("project membership", "invalid agent ID".to_string()))?;
        let fresh = list_management_projects(principal)
            .ok()
            .and_then(|projects| {
                projects
                    .projects
                    .into_iter()
                    .find(|candidate| candidate.project_id == project.project_id)
            })
            .unwrap_or_else(|| project.clone());
        let action_id =
            cutex::agent_management::AgentActionId::new(format!("human-agent-member:{id}"))
                .map_err(|error| incomplete("project membership", error.to_string()))?;
        execute_management_project_mutation(
            principal,
            &HumanManagementProjectMutationRequest {
                schema: HumanManagementProjectMutationSchema::V1,
                action_id,
                project_id: fresh.project_id.clone(),
                expected_authority_epoch: Some(fresh.authority_epoch),
                expected_project_revision: Some(fresh.project_revision),
                operation: HumanManagementProjectMutationKind::AddMember {
                    cutex_session_id: member,
                },
            },
        )
        .map_err(|error| incomplete("project membership", error.to_string()))?;
    }

    // 6. Optional start (same path as `cutex human start` via Management).
    if request.start {
        let snapshot = current(&id).map_err(|error| incomplete("start", error))?;
        online_management_v2_session(
            Some(principal),
            &id,
            serde_json::json!({ "expectedRuntimeGeneration": snapshot.runtime_generation, "openVisibleTerminal": false }),
        )
        .map_err(|error| incomplete("start", error.message))?;
    }
    Ok(id)
}

/// Root-only `POST /v2/human/sessions`: a quick chat = plain owned session
/// (what `cutex new` creates) with the quick-chat launch defaults, made
/// visible, optionally named and started. Without cwd a private scratch
/// directory `<config dir>/chats/<id>` is used.
fn create_human_session(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::management::v2::human_sessions::HumanCreateSessionRequest,
) -> Result<String, cutex::management::v2::user_input::UserInputExecutionError> {
    let mut timing = super::light_new::CreateTiming::new("human_session");
    request.validate()?;
    let config = cutex::config::store::load_codez_config_checked()
        .map_err(|error| human_agent_validation("invalid_request", format!("{error:#}")))?;
    // Request profile, else the quick-chat default, else the global default.
    let profile = cutex::management::v2::settings_api::quick_chat_profile(
        &config,
        request.profile.as_deref(),
    )
    .ok_or_else(|| {
        human_agent_validation(
            "unsupported_profile",
            "no profile given and no quick-chat or default profile is selected in Settings",
        )
    })?;
    // Selected by name or id; defaults and the record use the name.
    let profile = cutex::config::new_session::resolve_profile_name(&profile)
        .map_err(|error| human_agent_validation("unsupported_profile", format!("{error:#}")))?;
    if let Some(cwd) = &request.cwd {
        if !std::path::Path::new(cwd).is_dir() {
            return Err(human_agent_validation(
                "invalid_request",
                "cwd must be an existing directory",
            ));
        }
    }
    let claude = super::claude_profile::account(&profile)
        .map_err(|error| human_agent_validation("unsupported_profile", format!("{error:#}")))?;
    // 1. Validate everything that needs no side effect.
    let claude_defaults = claude
        .as_ref()
        .map(|account| claude_launch_defaults(account, &config, request.launch.as_ref()))
        .transpose()?;
    if claude.is_none() {
        refuse_claude_mode_for_codex(request.launch.as_ref())?;
    }
    let configuration = match &claude {
        Some(_) => None,
        None => Some(
            super::light_new::human_configuration(
                Some(&profile),
                cutex::config::new_thread::ThreadKind::QuickChat,
                request.launch.as_ref(),
            )
            .map_err(|error| human_agent_validation("unsupported_profile", format!("{error:#}")))?,
        ),
    };
    timing.phase("validate+configuration");
    // 2. cwd: as given, else a fresh private scratch directory.
    let scratch = match &request.cwd {
        Some(_) => None,
        None => Some(cutex::management::v2::human_sessions::create_scratch_dir().map_err(|error| {
            human_agent_error("scratch_dir_failed", format!("{error:#}"), serde_json::json!({}))
        })?),
    };
    let cwd = scratch
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from(request.cwd.as_deref().unwrap_or_default()));
    let created = match (configuration, &claude) {
        (Some(configuration), _) => super::light_new::create_owned_session_with(
            &profile,
            &cwd,
            configuration,
            |_| {},
            |adopt| adopt_created_native(principal, adopt).map_err(|error| anyhow::anyhow!("{error}")),
        ),
        (None, Some(account)) => (|| {
            let defaults = claude_defaults.clone().expect("Claude defaults resolved above");
            let cwd = super::light_new::canonical_cwd(&cwd)?;
            let name = format!("Session {}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            let id = super::claude_profile::create(account, &name, &cwd, false, defaults, (None, None))?.cutex_session_id;
            if let Some(mode) = request.launch.as_ref().and_then(|launch| launch.claude_permission_mode.as_deref()) {
                cutex::runtime::claude::set_initial_permission_mode(&id, mode)
                    .map_err(|error| anyhow::anyhow!("session {id} was created but its permission mode failed: {error:#}"))?;
            }
            Ok(id)
        })(),
        (None, None) => unreachable!("Codex configuration resolved above"),
    };
    let id = match created {
        Ok(id) => id,
        Err(error) => {
            if let Some(dir) = &scratch {
                // Only an untouched scratch directory is removed.
                let _ = std::fs::remove_dir(dir);
            }
            return Err(human_agent_error(
                "session_creation_failed",
                format!("{error:#}"),
                serde_json::json!({}),
            ));
        }
    };
    let incomplete = |step: &str, message: String| {
        human_agent_error(
            "session_setup_incomplete",
            format!("session {id} was created but {step} failed: {message}"),
            serde_json::json!({ "cutexSessionId": id, "step": step }),
        )
    };
    let current = |id: &str| -> Result<CutexSessionRecordSnapshot, String> {
        cutex::session::store::with_cutex_session_store(|store| {
            store
                .sessions
                .get(id)
                .map(|record| CutexSessionRecordSnapshot {
                    revision: record.durable_revision(),
                    exposed: record.exposed_to_backend,
                    profile: record.profile.clone(),
                    runtime_generation: record.runtime_generation,
                })
        })
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(|| "session disappeared".to_string())
    };
    // Background writers (runtime reconciliation, activity) may race the
    // whole-store CAS right after creation; these follow-ups are plain
    // desired-state writes, so re-read and retry a few times.
    let follow_up = |step: &str, method: &str, extra: serde_json::Value| {
        let mut attempt = 0;
        loop {
            let snapshot = current(&id).map_err(|error| incomplete(step, error))?;
            if method == "cutex/session/visibility/show" && snapshot.exposed {
                return Ok(());
            }
            let mut params = serde_json::json!({ "expectedRevision": snapshot.revision });
            if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
                params.extend(extra.clone());
            }
            match mutate_management_v2_session(&id, method, params) {
                Ok(_) => return Ok(()),
                Err(error)
                    if attempt < 4
                        && (error.code == "revision_conflict"
                            || error.message.contains("store revision conflict")) =>
                {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                }
                Err(error) => return Err(incomplete(step, error.message)),
            }
        }
    };
    timing.phase("create");
    // 3. Visible to every client (workflow step sessions stay hidden; a
    // Claude session starts exposed, so hide it).
    if !request.hidden {
        follow_up("visibility", "cutex/session/visibility/show", serde_json::json!({}))?;
    } else if current(&id).map_err(|error| incomplete("visibility", error))?.exposed {
        follow_up("visibility", "cutex/session/visibility/hide", serde_json::json!({}))?;
    }
    // 4. Explicit name (otherwise the name follows the native thread title).
    if let Some(name) = &request.display_name {
        follow_up(
            "display name",
            "cutex/session/metadata/update",
            serde_json::json!({ "displayName": name }),
        )?;
    }
    timing.phase("visibility+name");
    // 5. Optional start.
    if request.start {
        let snapshot = current(&id).map_err(|error| incomplete("start", error))?;
        online_management_v2_session(
            Some(principal),
            &id,
            serde_json::json!({ "expectedRuntimeGeneration": snapshot.runtime_generation, "openVisibleTerminal": false }),
        )
        .map_err(|error| incomplete("start", error.message))?;
        timing.phase("start");
    }
    Ok(id)
}

struct CutexSessionRecordSnapshot {
    revision: u64,
    exposed: bool,
    profile: Option<String>,
    runtime_generation: u64,
}

pub(super) fn management_agent_provider() -> Result<
    cutex::agent_management::AgentManagementProvider,
    cutex::agent_management::AgentManagementError,
> {
    cutex::agent_management::AgentManagementProvider::open_default()
        .map_err(|_| cutex::agent_management::AgentManagementError::PersistenceUnavailable)
}

pub(super) fn list_management_projects(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
) -> Result<
    cutex::management::control_plane::HumanManagementProjectCollection,
    cutex::agent_management::AgentManagementError,
> {
    management_agent_provider()?.list_cutex_projects_for_management(principal)
}

fn read_management_project(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    project_id: &cutex::agent_management::ProjectId,
) -> Result<
    cutex::management::control_plane::HumanManagementProjectWorkspace,
    cutex::agent_management::AgentManagementError,
> {
    let observer = ManagementProjectRuntimeObserver::load()?;
    management_agent_provider()?.read_cutex_project_for_management(principal, project_id, &observer)
}

fn update_management_project_presentation(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::management::control_plane::HumanManagementPresentationUpdateRequest,
) -> Result<
    cutex::agent_management::ProjectPresentationSettings,
    cutex::agent_management::AgentManagementError,
> {
    management_agent_provider()?.update_project_presentation_for_management(principal, request)
}

fn execute_management_operator_action(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::management::control_plane::HumanManagementOperatorActionRequest,
) -> Result<
    cutex::management::control_plane::HumanManagementOperatorReceipt,
    cutex::agent_management::AgentManagementError,
> {
    management_agent_provider()?.execute_operator_action_for_management(principal, request)
}

pub(super) fn execute_management_project_mutation(
    principal: &cutex::management::control_plane::HumanManagementPrincipal,
    request: &cutex::management::control_plane::HumanManagementProjectMutationRequest,
) -> Result<
    cutex::management::control_plane::HumanManagementProjectMutationReceipt,
    cutex::agent_management::AgentManagementError,
> {
    management_agent_provider()?.execute_project_mutation_for_management(principal, request)
}

struct ManagementProjectRuntimeObserver {
    sessions: std::sync::Arc<cutex::session::model::CutexSessionStore>,
    live_agents: Vec<cutex::agent_bus::model::AgentBusAgent>,
    native_versions: Option<cutex::launch::version_registry::NativeVersionRegistry>,
}

impl ManagementProjectRuntimeObserver {
    fn load() -> Result<Self, cutex::agent_management::AgentManagementError> {
        Ok(Self {
            sessions: cutex::session::store::load_cutex_session_store_shared().map_err(|_| {
                cutex::agent_management::AgentManagementError::PersistenceUnavailable
            })?,
            live_agents: cutex::agent_bus::client::agent_bus_fetch_agents_if_healthy(
                &cutex::config::store::load_codez_config(),
            ),
            native_versions: cutex::launch::version_registry::load_relaxed(),
        })
    }
}

impl cutex::agent_management::ProjectRuntimeObserver for ManagementProjectRuntimeObserver {
    fn observe(
        &self,
        cutex_session_id: &cutex::role_revision::CutexSessionId,
    ) -> Result<
        cutex::agent_management::AgentRuntimeObservation,
        cutex::agent_management::AgentManagementError,
    > {
        use cutex::agent_management::{AgentManagementError, AgentRuntimeObservation};
        let record = self
            .sessions
            .sessions
            .get(cutex_session_id.as_str())
            .ok_or(AgentManagementError::NotFound("durable_session_not_found"))?;
        let native_session_id = record
            .codex_session_id
            .clone()
            .ok_or(AgentManagementError::NotFound("native_session_not_found"))?;
        let mut runtime_agent_ids = self
            .live_agents
            .iter()
            .filter(|agent| agent.cutex_session_id.as_deref() == Some(cutex_session_id.as_str()))
            .map(|agent| agent.id.clone())
            .collect::<Vec<_>>();
        runtime_agent_ids.sort();
        Ok(AgentRuntimeObservation {
            cutex_session_id: cutex_session_id.clone(),
            native_session_id,
            active: record.is_active(),
            cwd: cutex::session::service::cutex_session_launch_cwd(record).to_string(),
            profile: record.profile.clone().unwrap_or_default(),
            runtime_backend: serde_json::to_value(record.runtime_backend)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            model: record.model_defaults.clone().unwrap_or_default(),
            reasoning: record.reasoning_defaults.clone().unwrap_or_default(),
            permissions: record.permission_defaults.clone().unwrap_or_default(),
            approval_policy: record.approval_policy.clone().unwrap_or_default(),
            sandbox_mode: record.sandbox_mode.clone().unwrap_or_default(),
            groups: record.agent_groups.clone(),
            runtime_generation: record.runtime_generation,
            runtime_version: record
                .app_server_runtime
                .as_ref()
                .and_then(|binding| {
                    self.native_versions
                        .as_ref()
                        .and_then(|registry| registry.runtime_version_for_pid(binding.pid))
                })
                .or_else(|| {
                    record
                        .runtime_pid
                        .and_then(|pid| {
                            self.native_versions
                                .as_ref()
                                .and_then(|registry| registry.runtime_version_for_pid(pid))
                        })
                })
                .or_else(|| {
                    record
                        .explicit_launch
                        .as_ref()
                        .and_then(|contract| {
                            self.native_versions.as_ref().and_then(|registry| {
                                registry.runtime_version_for_launch_contract(contract)
                            })
                        })
                }),
            runtime_agent_ids,
            app_server_runtime: record.app_server_runtime.is_some(),
            agent_bus_endpoint_ids: Vec::new(),
        })
    }
}

fn load_management_bootstrap_state(
    cutex_session_id: &str,
    runtime_generation: u64,
) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "userInputQueue": cutex::management::v2::user_input::user_input_repository()?
            .snapshot(cutex_session_id)?,
        "userInputQueuePause": cutex::management::v2::user_input::user_input_repository()?
            .pause_state(cutex_session_id)?,
        "admission": cutex::management::v2::user_input::user_input_repository()?
            .admission(cutex_session_id)?,
        "asksPaused": cutex::asks::control::pause_state_default(cutex_session_id),
        "pendingServerRequests": cutex::management::v2::server_requests::snapshot(
            cutex_session_id,
            runtime_generation,
        )?,
        "agentBusMessages": cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
            .snapshot(cutex_session_id)?,
        // Agent messaging v2: the latest 200 messages this session sent.
        "agentBusSentMessages": cutex::management::v2::agent_bus_state::agent_bus_message_repository()?
            .sent_snapshot(cutex_session_id, 200)?,
    }))
}

pub(super) fn mutate_management_v2_session(
    cutex_session_id: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    use cutex::management::v2::user_input::UserInputExecutionError;
    use cutex::session::model::parse_cutex_session_runtime_backend;
    use cutex::session::service::cutex_session_key_for_user_id;
    use cutex::session::service::persist_cutex_session_store_and_im_record;
    use cutex::session::service::set_cutex_session_profile_by_key_with_expected_revision;

    let _guard = MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| session_mutation_invalid("management v2 session mutation lock poisoned"))?;
    if matches!(
        method,
        "cutex/runtime/online" | "cutex/runtime/offline" | "cutex/runtime/close"
    ) {
        let result = mutate_management_v2_runtime(cutex_session_id, method, &params);
        note_runtime_intent(cutex_session_id, method, &result);
        return result;
    }
    if matches!(method, "cutex/session/retire" | "cutex/session/restore") {
        return archive_management_v2_session(cutex_session_id, method, &params);
    }
    let mut store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let key = cutex_session_key_for_user_id(&store, cutex_session_id).ok_or_else(|| {
        UserInputExecutionError {
            stage: "route".to_string(),
            code: "session_not_found".to_string(),
            message: "the durable cutex session does not exist".to_string(),
            retryable: false,
            details: serde_json::json!({ "cutexSessionId": cutex_session_id }),
            outcome_unknown: false,
        }
    })?;
    if matches!(
        method,
        "cutex/session/profile/set" | "cutex/session/profile/clear"
            | "cutex/session/profile/switch"
    ) {
        let target = store.sessions.get(&key).ok_or_else(|| {
            session_mutation_invalid("cutex session disappeared during profile mutation")
        })?;
        // Optional fence: omitted applies to the current revision.
        let expected_revision = optional_expected_revision(&params)?
            .unwrap_or_else(|| target.durable_revision());
        if !cutex::runtime::lifecycle::cutex_session_host_is_local(
            &target.host_id,
            &cutex::platform::host::current_host_name(),
        ) {
            return Err(session_mutation_invalid(
                "profile mutation must be resolved on the target host",
            ));
        }
        // Profile switch resets the per-agent model/effort to the target defaults.
        let switch_defaults = (method == "cutex/session/profile/switch")
            .then(|| -> Result<_, cutex::management::v2::user_input::UserInputExecutionError> {
                let field = |key: &str| match params.get(key) {
                    None | Some(serde_json::Value::Null) => Ok(None),
                    Some(serde_json::Value::String(value)) if !value.trim().is_empty() => {
                        Ok(Some(value.trim().to_string()))
                    }
                    Some(_) => Err(session_mutation_invalid(&format!(
                        "{key} must be a non-empty string or null"
                    ))),
                };
                Ok((field("model")?, field("reasoningEffort")?))
            })
            .transpose()?;
        let mut claude_model = None;
        let profile = match method {
            "cutex/session/profile/set" | "cutex/session/profile/switch" => {
                let requested = params
                    .get("profile")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        session_mutation_invalid("profile must be a non-empty string")
                    })?;
                // Before the launch checks: an account change is the reason
                // a Claude session cannot take this profile.
                if let Err(mismatch) =
                    cutex::runtime::claude_accounts::check_profile_change(target, Some(requested))
                {
                    return Err(cutex::management::v2::profile_switch::claude_account_mismatch_error(&mismatch));
                }
                if target.claude_session.is_some() {
                    let (name, model) = super::launch::resolve_claude_profile_for_session(target, requested)
                        .map_err(session_mutation_invalid_error)?;
                    claude_model = Some(model);
                    Some(name)
                } else if target.explicit_launch.is_some() {
                    let mut candidate = target.clone();
                    candidate.profile = Some(requested.to_string());
                    if let Some((model, reasoning)) = &switch_defaults {
                        candidate.model_defaults = model.clone();
                        candidate.reasoning_defaults = reasoning.clone();
                    }
                    cutex::launch::stock::current_configuration(&candidate)
                        .map_err(session_mutation_invalid_error)?;
                    Some(requested.to_string())
                } else {
                    Some(
                        super::launch::resolve_launch_profile_override(requested)
                            .map_err(session_mutation_invalid_error)?
                            .account
                            .name,
                    )
                }
            }
            "cutex/session/profile/clear" => None,
            _ => unreachable!(),
        };
        // A Claude session stays on the Claude account it was created on.
        if let Err(mismatch) =
            cutex::runtime::claude_accounts::check_profile_change(target, profile.as_deref())
        {
            return Err(cutex::management::v2::profile_switch::claude_account_mismatch_error(&mismatch));
        }
        let outcome = set_cutex_session_profile_by_key_with_expected_revision(
            &mut store,
            &key,
            profile.clone(),
            expected_revision,
        )
        .map_err(|error| {
            let current = store
                .sessions
                .get(&key)
                .map(|record| record.durable_revision());
            if current.is_some_and(|current| current != expected_revision) {
                session_revision_conflict(expected_revision, current.expect("current revision"))
            } else {
                session_mutation_invalid_error(error)
            }
        })?;
        if let Some((model, reasoning)) = &switch_defaults {
            let record = store.sessions.get_mut(&key).expect("profile-updated session");
            record.model_defaults = model.clone();
            record.reasoning_defaults = reasoning.clone();
        }
        if let Some(model) = claude_model {
            let record = store.sessions.get_mut(&key).expect("profile-updated session");
            record.claude_session.as_mut().expect("Claude session").model = model.clone();
            record.model_defaults = Some(model);
        }
        persist_cutex_session_store_and_im_record(&store, &key).map_err(|error| {
            if error
                .downcast_ref::<cutex::session::store::CutexSessionStoreRevisionConflict>()
                .is_some()
            {
                session_mutation_persistence_error(error)
            } else {
                session_mutation_persistence_uncertain(format!("{error:#}"))
            }
        })?;
        return Ok(serde_json::json!({
            "cutexSessionId": outcome.key,
            "revision": store.sessions.get(&key).expect("persisted session").durable_revision(),
            "configuredProfile": profile,
            "model": store.sessions[&key].claude_session.as_ref().map(|s| s.model.clone())
                .or_else(|| switch_defaults.as_ref().and_then(|(model, _)| model.clone())),
            "reasoningEffort": switch_defaults.as_ref().and_then(|(_, reasoning)| reasoning.clone()),
        }));
    }
    let record = store
        .sessions
        .get_mut(&key)
        .ok_or_else(|| UserInputExecutionError {
            stage: "route".to_string(),
            code: "session_not_found".to_string(),
            message: "the durable cutex session disappeared during mutation".to_string(),
            retryable: false,
            details: serde_json::json!({ "cutexSessionId": cutex_session_id }),
            outcome_unknown: false,
        })?;
    let result = match method {
        "cutex/session/defaults/update" => {
            let patch = params
                .get("patch")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| session_mutation_invalid("defaults patch must be an object"))?;
            if let Some(backend) = patch.get("backend").and_then(serde_json::Value::as_str) {
                record.runtime_backend = parse_cutex_session_runtime_backend(backend)
                    .map_err(session_mutation_invalid_error)?;
            }
            apply_optional_string(patch, "managedCwd", &mut record.managed_cwd)?;
            apply_optional_string(patch, "permissions", &mut record.permission_defaults)?;
            apply_optional_string(patch, "approvalPolicy", &mut record.approval_policy)?;
            apply_optional_string(patch, "sandboxMode", &mut record.sandbox_mode)?;
            cutex::runtime::args::reconcile_patched_permissions(
                record,
                patch.contains_key("permissions"),
                patch.contains_key("sandboxMode"),
            )
            .map_err(session_mutation_invalid_error)?;
            apply_optional_string(patch, "model", &mut record.model_defaults)?;
            apply_optional_string(patch, "reasoningEffort", &mut record.reasoning_defaults)?;
            // session-fast-mode: stored for the next start / resume; clients
            // change a running thread with native thread/settings/update.
            apply_optional_string(patch, "serviceTier", &mut record.service_tier)?;
            if record.service_tier.as_deref() == Some("fast") {
                record.service_tier = Some("priority".to_string());
            }
            if let Some(args) = patch.get("cliArgs") {
                record.default_cli_args = json_string_array(args, "cliArgs")?;
            }
            if let Some(groups) = patch.get("groups") {
                record.agent_groups = normalized_string_array(groups, "groups")?;
            }
            // Claude tools take effect at the next start (the adapter gets
            // them as launch flags), so an online session keeps its tools.
            for (key, write) in [("claudeAllowWrite", true), ("claudeAllowExec", false)] {
                let Some(value) = patch.get(key) else { continue };
                let value = value
                    .as_bool()
                    .ok_or_else(|| session_mutation_invalid(&format!("{key} must be a boolean")))?;
                let claude = record
                    .claude_session
                    .as_mut()
                    .ok_or_else(|| session_mutation_invalid(&format!("{key} applies to Claude sessions only")))?;
                // Set for this session: no longer follows the profile.
                if write {
                    claude.allow_write = value;
                    claude.write_explicit = true;
                } else {
                    claude.allow_exec = value;
                    claude.exec_explicit = true;
                }
            }
            serde_json::json!({})
        }
        "cutex/session/groups/set" | "cutex/session/groups/add" | "cutex/session/groups/remove" => {
            let groups = normalized_string_array(
                params
                    .get("groups")
                    .ok_or_else(|| session_mutation_invalid("groups are required"))?,
                "groups",
            )?;
            match method {
                "cutex/session/groups/set" => record.agent_groups = groups,
                "cutex/session/groups/add" => {
                    record.agent_groups.extend(groups);
                    record.agent_groups.sort();
                    record.agent_groups.dedup();
                }
                "cutex/session/groups/remove" => {
                    record.agent_groups.retain(|group| !groups.contains(group));
                }
                _ => unreachable!(),
            }
            serde_json::json!({ "groups": record.agent_groups })
        }
        "cutex/session/metadata/update" => {
            let expected_revision =
                optional_expected_revision(&params)?.unwrap_or_else(|| record.durable_revision());
            if record.durable_revision() != expected_revision {
                return Err(session_revision_conflict(
                    expected_revision,
                    record.durable_revision(),
                ));
            }
            cutex::session::metadata::apply_session_metadata_patch(record, &params)
                .map_err(session_mutation_invalid_error)?;
            serde_json::json!({
                "cutexSessionId": record.cutex_session_id,
                "displayName": cutex::session::metadata::cutex_session_presented_name(record),
                "displayNameOverride": record.display_name_override,
                "note": record.note,
                "avatar": record.avatar,
            })
        }
        "cutex/session/visibility/show" => {
            record.exposed_to_backend = true;
            serde_json::json!({})
        }
        "cutex/session/visibility/hide" => {
            record.exposed_to_backend = false;
            serde_json::json!({})
        }
        _ => {
            return Err(session_mutation_invalid(
                "unsupported session mutation method",
            ))
        }
    };
    record
        .bump_durable_revision()
        .map_err(session_mutation_persistence_error)?;
    record.updated_at = chrono::Utc::now().to_rfc3339();
    persist_cutex_session_store_and_im_record(&store, &key).map_err(|error| {
        if error
            .downcast_ref::<cutex::session::store::CutexSessionStoreRevisionConflict>()
            .is_some()
        {
            session_mutation_persistence_error(error)
        } else {
            session_mutation_persistence_uncertain(format!("{error:#}"))
        }
    })?;
    let revision = store
        .sessions
        .get(&key)
        .map(|record| record.durable_revision())
        .ok_or_else(|| session_mutation_invalid("cutex session disappeared after mutation"))?;
    let mut result = result;
    result
        .as_object_mut()
        .expect("session mutation result object")
        .insert("revision".to_string(), serde_json::json!(revision));
    Ok(result)
}

/// `cutex/session/retire` / `cutex/session/restore` in one call. Fences are
/// optional (checked when supplied). A durable Agent is refused only while it
/// is the Director of an active project, has open task assignments, or has a
/// live launch in progress; a running runtime is stopped as part of Archive.
/// Plain (non-Agent) sessions keep the original lifecycle transaction.
/// Caller holds `MANAGEMENT_V2_SESSION_MUTATION_LOCK`.
fn archive_management_v2_session(
    cutex_session_id: &str,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    use cutex::agent_management::{AgentArchiveOperation, AgentArchiveStage};
    use cutex::management::v2::user_input::UserInputExecutionError;
    let restoring = method == "cutex/session/restore";
    let object = params
        .as_object()
        .ok_or_else(|| session_mutation_invalid("params must be an object"))?;
    let reason = match object.get("reason") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(reason)) => Some(reason.clone()),
        Some(_) => return Err(session_mutation_invalid("reason must be a string")),
    };
    let supplied_generation = match object.get("expectedRuntimeGeneration") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|g| *g <= cutex::management::v2::model::MAX_SAFE_SEQUENCE)
                .ok_or_else(|| {
                    session_mutation_invalid("expectedRuntimeGeneration must be a JSON-safe integer")
                })?,
        ),
    };
    let load = || -> Result<(String, cutex::session::model::CutexSessionRecord), UserInputExecutionError> {
        let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
        let key = cutex::session::service::cutex_session_key_for_user_id_including_retired(
            &store,
            cutex_session_id,
        )
        .ok_or_else(|| UserInputExecutionError {
            stage: "route".to_string(),
            code: "session_not_found".to_string(),
            message: "the durable cutex session does not exist".to_string(),
            retryable: false,
            details: serde_json::json!({ "cutexSessionId": cutex_session_id }),
            outcome_unknown: false,
        })?;
        let record = store.sessions[&key].clone();
        Ok((key, record))
    };
    let (key, record) = load()?;
    if let Some(expected) = optional_expected_revision(params)? {
        if expected != record.durable_revision() {
            return Err(session_revision_conflict(expected, record.durable_revision()));
        }
    }
    if let Some(expected) = supplied_generation {
        if expected != record.runtime_generation {
            return Err(UserInputExecutionError {
                stage: "runtime".to_string(),
                code: "revision_conflict".to_string(),
                message: format!(
                    "runtime generation conflict: expected {expected}, current {}",
                    record.runtime_generation
                ),
                retryable: true,
                details: serde_json::json!({
                    "expectedRuntimeGeneration": expected,
                    "currentRuntimeGeneration": record.runtime_generation,
                    "resyncRequired": true,
                }),
                outcome_unknown: false,
            });
        }
    }
    if record.is_retired() != restoring {
        return Err(archive_state_conflict(restoring));
    }
    let durable_agent = record.agent_enabled
        && record.registration_class
            == cutex::agent_bus::model::AgentRegistrationClass::Persistent;
    if !durable_agent {
        let mut legacy = serde_json::json!({ "expectedRevision": record.durable_revision() });
        if !restoring {
            legacy["expectedRuntimeGeneration"] = serde_json::json!(record.runtime_generation);
            if let Some(reason) = &reason {
                legacy["reason"] = serde_json::json!(reason);
            }
        }
        return super::management_archive::mutate_management_v2_archive(&key, method, &legacy);
    }
    let id = cutex::role_revision::CutexSessionId::new(key.clone())
        .map_err(|_| session_mutation_invalid("invalid durable Agent ID"))?;
    let provider = management_agent_provider().map_err(archive_provider_error)?;
    let path = cutex::session::store::cutex_sessions_path()
        .map_err(session_mutation_persistence_error)?;
    if !restoring {
        // Refuse before touching the runtime.
        let blockers = provider
            .agent_archive_blockers(&id)
            .map_err(archive_provider_error)?;
        if !blockers.is_empty() {
            return Err(archive_blocked_error(&blockers));
        }
        stop_runtime_before_archive(&key)?;
    }
    let operation = if restoring {
        AgentArchiveOperation::Restore
    } else {
        AgentArchiveOperation::Archive
    };
    let receipt = provider
        .archive_agent(
            &path,
            &id,
            operation,
            reason,
            &mut super::management_archive::OfflineArchiveRuntime,
        )
        .map_err(|error| match error {
            cutex::agent_management::AgentManagementError::Conflict(
                "archive_requires_explicit_director_rotation",
            ) => provider
                .agent_archive_blockers(&id)
                .map(|blockers| archive_blocked_error(&blockers))
                .unwrap_or_else(archive_provider_error),
            error => archive_provider_error(error),
        })?;
    let record = match (receipt.stage, receipt.result) {
        (AgentArchiveStage::Committed, Some(record)) => record,
        (stage, _) => {
            return Err(UserInputExecutionError {
                stage: "runtime".to_string(),
                code: "runtime_stop_failed".to_string(),
                message: receipt
                    .error
                    .unwrap_or_else(|| format!("archive did not complete ({stage:?})")),
                retryable: true,
                details: serde_json::json!({ "lifecycleCommitted": false }),
                outcome_unknown: false,
            })
        }
    };
    Ok(serde_json::json!({
        "cutexSessionId": record.cutex_session_id,
        "revision": record.durable_revision(),
        "lifecycle": record.archive_state.label(),
        "runtimeGeneration": record.runtime_generation,
        "status": "offline",
        "retiredAt": record.retired_at,
    }))
}

/// Clears a dead launch claim, then stops a running runtime through the
/// ordinary offline path. A live launch in progress is refused.
pub(super) fn stop_runtime_before_archive(
    key: &str,
) -> Result<(), cutex::management::v2::user_input::UserInputExecutionError> {
    if let Err(error) = auto_clear_dead_launch_claim(key) {
        eprintln!("archive: launch claim check failed for {key}: {error:#}");
    }
    let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let Some(record) = store.sessions.get(key) else {
        return Ok(());
    };
    if record.app_server_launch_claim_id.is_some() {
        return Err(archive_provider_error(
            cutex::agent_management::AgentManagementError::Conflict(
                "archive_runtime_launch_in_progress",
            ),
        ));
    }
    let online = super::management_archive::archive_runtime_looks_online(record)
        .map_err(session_mutation_persistence_error)?;
    if online {
        let stopped = mutate_management_v2_runtime(
            key,
            "cutex/runtime/offline",
            &serde_json::json!({ "force": true }),
        )?;
        if stopped.get("status").and_then(serde_json::Value::as_str) != Some("offline") {
            return Err(cutex::management::v2::user_input::UserInputExecutionError {
                stage: "runtime".to_string(),
                code: "runtime_stop_failed".to_string(),
                message: "the runtime did not stop; the agent was not archived".to_string(),
                retryable: true,
                details: serde_json::json!({ "lifecycleCommitted": false, "stop": stopped }),
                outcome_unknown: false,
            });
        }
    }
    Ok(())
}

fn archive_state_conflict(
    restoring: bool,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    let (code, message) = if restoring {
        ("already_active", "the cutex session is already active")
    } else {
        ("already_retired", "the cutex session is already retired")
    };
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "route".to_string(),
        code: code.to_string(),
        message: message.to_string(),
        retryable: false,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

fn archive_blocked_error(
    blockers: &cutex::agent_management::AgentArchiveBlockers,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    // r219: the only blocker is the Agent Management Director authority.
    let message = format!(
        "this agent is the Director of active project(s) {}; set another Director or archive the project first",
        blockers
            .director_projects
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "authorization".to_string(),
        code: "archive_blocked_by_director".to_string(),
        message,
        retryable: false,
        details: serde_json::to_value(blockers).unwrap_or_default(),
        outcome_unknown: false,
    }
}

fn archive_provider_error(
    error: cutex::agent_management::AgentManagementError,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    use cutex::agent_management::AgentManagementError;
    let (stage, code, retryable) = match &error {
        AgentManagementError::Conflict("archive_runtime_launch_in_progress") => {
            ("runtime", "launch_in_progress", true)
        }
        AgentManagementError::Conflict("permanent_retirement_is_not_restorable") => {
            ("route", "permanently_closed", false)
        }
        AgentManagementError::Conflict("archive_state_conflict") => {
            ("route", "archive_state_conflict", false)
        }
        AgentManagementError::Conflict("requires_exact_persistent_durable_agent") => {
            ("route", "not_a_durable_agent", false)
        }
        AgentManagementError::PersistenceUnavailable => {
            ("route", "event_persistence_unavailable", true)
        }
        _ => ("route", "archive_failed", false),
    };
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: stage.to_string(),
        code: code.to_string(),
        message: error.to_string(),
        retryable,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

/// Durable online intent for restore after an unclean stop
/// (`session::runtime_intent`): a runtime that came online is intended
/// online; a requested offline / close drops the intent. Process death never
/// reaches here.
fn note_runtime_intent(
    cutex_session_id: &str,
    method: &str,
    result: &Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError>,
) {
    let Ok(result) = result else {
        return;
    };
    let online = method == "cutex/runtime/online";
    if online && result["status"] != "online" {
        return;
    }
    if !online && !matches!(method, "cutex/runtime/offline" | "cutex/runtime/close") {
        return;
    }
    // Requests may name a session by a short id: key by the durable id.
    let key = load_cutex_session_store()
        .ok()
        .and_then(|store| cutex::session::service::cutex_session_key_for_user_id(&store, cutex_session_id))
        .unwrap_or_else(|| cutex_session_id.to_string());
    if online {
        cutex::session::runtime_intent::note_online(&key)
    } else {
        cutex::session::runtime_intent::note_offline(&key)
    }
}

pub(crate) fn online_management_v2_session(
    owner: Option<&cutex::management::control_plane::HumanManagementPrincipal>,
    id: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    let result = online_management_v2_session_inner(owner, id, params);
    note_runtime_intent(id, "cutex/runtime/online", &result);
    result
}

fn online_management_v2_session_inner(
    owner: Option<&cutex::management::control_plane::HumanManagementPrincipal>,
    id: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    use cutex::agent_management::{ExplicitLaunchRequest, StockRuntimeReview};
    let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let key = cutex::session::service::cutex_session_key_for_user_id(&store, id)
        .ok_or_else(|| session_mutation_invalid("cutex session not found"))?;
    // Older daemons could store a preset alias that disagrees with the
    // explicit sandbox; launch follows the explicit sandbox, so persist that
    // instead of refusing the session (logged by the store helper).
    cutex::session::store::repair_cutex_session_permission_alias(&key)
        .map_err(session_mutation_persistence_error)?;
    if store.sessions[&key].explicit_launch.is_none() {
        return mutate_management_v2_session(id, "cutex/runtime/online", params);
    }
    let Some(owner) = owner else {
        // No owner principal (asks auto-wake / reconnect): never launch an
        // explicit-launch agent, but reconnect to its live exact binding.
        return reconnect_live_explicit_runtime(&key);
    };
    start_explicit_runtime(&key, &params, |key| {
        let review: StockRuntimeReview = serde_json::from_value(explicit_launch_action(
            owner,
            &ExplicitLaunchRequest::ReviewRuntime {
                cutex_session_id: cutex::role_revision::CutexSessionId::new(key.to_string())
                    .map_err(|_| anyhow::anyhow!("invalid durable Agent ID"))?,
                restart: false,
                receiver_canonical_byte_limit: Default::default(),
                job_mcp: None,
            },
        )?)?;
        let action_id = cutex::agent_management::AgentActionId::new(format!(
            "api-runtime-{}",
            uuid::Uuid::new_v4()
        ))?;
        serde_json::from_value(explicit_launch_action(
            owner,
            &ExplicitLaunchRequest::Run { action_id, review },
        )?)
        .map_err(Into::into)
    })
}

/// Asks reviewed relaunch (owner approval 2026-10-14; policy in
/// `cutex::asks::wake::decide`): start an offline explicit-launch agent for
/// `requester`'s delegation on the owner's latest reviewed launch. The same
/// start the owner's `cutex/runtime/online` runs (claim cleanup, exited
/// binding stop, reconnect of a live one, readiness handling), except that
/// the launch reuses the owner's earlier review instead of a new one
/// (`AgentManagementProvider::execute_reviewed_relaunch`). The action id
/// names the requester.
pub(crate) fn relaunch_reviewed_management_v2_session(
    id: &str,
    requester: &str,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    let result = (|| {
        let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
        let key = cutex::session::service::cutex_session_key_for_user_id(&store, id)
            .ok_or_else(|| session_mutation_invalid("cutex session not found"))?;
        if store.sessions[&key].explicit_launch.is_none() {
            return Err(session_mutation_invalid("reviewed relaunch applies only to explicit-launch agents"));
        }
        // As the owner's start does (launch follows the explicit sandbox).
        cutex::session::store::repair_cutex_session_permission_alias(&key)
            .map_err(session_mutation_persistence_error)?;
        let mut reviewed_action = None;
        let mut result = start_explicit_runtime(&key, &serde_json::json!({"openVisibleTerminal": false}), |key| {
            use cutex::agent_management::REVIEWED_RELAUNCH_ACTION_PREFIX as PREFIX;
            let action_id = cutex::agent_management::AgentActionId::new(format!(
                "{PREFIX}{requester}:{}",
                uuid::Uuid::new_v4()
            ))
            .or_else(|_| {
                cutex::agent_management::AgentActionId::new(format!("{PREFIX}{}", uuid::Uuid::new_v4()))
            })?;
            let mut runtime = super::stock_lifecycle::StockExecutor::default();
            let (receipt, reviewed) = management_agent_provider()?.execute_reviewed_relaunch(
                &cutex::session::store::cutex_sessions_path()?,
                &cutex::role_revision::CutexSessionId::new(key.to_string())
                    .map_err(|_| anyhow::anyhow!("invalid durable Agent ID"))?,
                requester,
                &action_id,
                &mut runtime,
            )?;
            reviewed_action = Some(reviewed);
            Ok(receipt)
        })?;
        result["startedBy"] = serde_json::json!({
            "requester": requester,
            "reviewedActionId": reviewed_action,
            "basis": "ownerReviewedLaunch",
        });
        Ok(result)
    })();
    note_runtime_intent(id, "cutex/runtime/online", &result);
    result
}

/// The owner's explicit-launch start sequence around `launch` (which returns
/// the launch receipt): mutation lock, generation check, archived / host /
/// profile checks, dead-claim cleanup, exited-binding stop or live-binding
/// reconnect, then readiness handling of the receipt.
fn start_explicit_runtime(
    key: &str,
    params: &serde_json::Value,
    launch: impl FnOnce(&str) -> anyhow::Result<cutex::agent_management::StockRuntimeReceipt>,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    use cutex::agent_management::{StockRuntimeReceipt, StockRuntimeStage};
    let key = key.to_string();
    let _guard = MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| session_mutation_invalid("management v2 session mutation lock poisoned"))?;
    let current = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let generation = current
        .sessions
        .get(&key)
        .ok_or_else(|| session_mutation_invalid("agent disappeared"))?
        .runtime_generation;
    let supplied_generation = params
        .get("expectedRuntimeGeneration")
        .and_then(serde_json::Value::as_u64);
    if supplied_generation.is_some_and(|supplied| supplied != generation) {
        return Err(cutex::management::v2::user_input::UserInputExecutionError {
            stage: "route".into(),
            code: "revision_conflict".into(),
            message: "runtime generation changed; reload the session before retrying".into(),
            retryable: true,
            outcome_unknown: false,
            details: serde_json::json!({"currentRuntimeGeneration": generation, "expectedRuntimeGeneration": params.get("expectedRuntimeGeneration"), "resyncRequired": true}),
        });
    }
    let operation = || -> anyhow::Result<StockRuntimeReceipt> {
        let store = load_cutex_session_store()?;
        let record = store
            .sessions
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("agent disappeared"))?;
        anyhow::ensure!(
            supplied_generation.is_none_or(|supplied| supplied == record.runtime_generation),
            "runtime generation changed; reload the session before retrying"
        );
        anyhow::ensure!(
            !record.is_retired(),
            "restore the archived Agent before starting it"
        );
        anyhow::ensure!(
            cutex::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &cutex::platform::host::current_host_name()
            ),
            "manage this runtime on its owning host"
        );
        anyhow::ensure!(params.get("launchProfile").is_none(), "native runtime uses its saved profile; use cutex session profile set {key} PROFILE before starting");
        if record.app_server_launch_claim_id.is_some() {
            // A crashed launch leaves its claim behind: clear it when the
            // owner is gone, then continue with the refreshed record.
            auto_clear_dead_launch_claim(&key)?;
        }
        let store = load_cutex_session_store()?;
        let record = store
            .sessions
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("agent disappeared"))?;
        if let Some(binding) = &record.app_server_runtime {
            if super::stock_lifecycle::stock_runtime_has_exited(record, binding)? {
                let stopped = super::native_stop::stop_and_commit(record, false)?;
                anyhow::ensure!(stopped.stopped, "exited runtime cleanup incomplete: {}", stopped.detail);
            } else {
                super::stock_lifecycle::verify_stock_process(record, binding)?;
                super::app_server_runtime::verify_exact_live_runtime_claim(record, binding)?;
                return super::stock_lifecycle::reconnect_ready_runtime(record, &store);
            }
        }
        launch(&key)
    };
    let receipt = operation().map_err(runtime_mutation_error)?;
    if receipt.stage != StockRuntimeStage::Ready || receipt.error.is_some() {
        if receipt.stage == StockRuntimeStage::Abandoned {
            // Readiness failed; the spawned runtime was stopped and its claim
            // released, so the next start begins clean.
            return Err(cutex::management::v2::user_input::UserInputExecutionError {
                stage: "runtime".into(),
                code: "runtime_start_failed".into(),
                message: receipt.error.clone().unwrap_or_else(|| "runtime start failed".into()),
                retryable: true,
                outcome_unknown: false,
                details: serde_json::json!({"actionId": receipt.action_id, "receipt": receipt}),
            });
        }
        return Err(cutex::management::v2::user_input::UserInputExecutionError {
            stage: "runtime".into(),
            code: "runtime_readiness_incomplete".into(),
            message: format!(
                "{}; inspect cutex human action {} (the next start stops its runtime and retries)",
                receipt.error.as_deref().unwrap_or("runtime readiness incomplete"),
                receipt.action_id.as_str()
            ),
            retryable: false,
            outcome_unknown: false,
            details: serde_json::json!({"actionId": receipt.action_id, "receipt": receipt}),
        });
    }
    let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let record = &store.sessions[&key];
    let mut result = serde_json::json!({"runtimeGeneration": record.runtime_generation, "runtimeAgentId": record.current_runtime_agent_id, "status": "online", "actionId": receipt.action_id, "attachCommand": format!("cutex session takeover {key}")});
    if params
        .get("openVisibleTerminal")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
    {
        result["foregroundRequiredReason"] = serde_json::json!(
            "native runtime is online; attach from a terminal using attachCommand"
        );
    }
    Ok(result)
}

/// What an explicit-launch agent's online request without an owner may do
/// (r208): reconnect to a settled binding whose process is alive; anything
/// else (no binding, a launch claim, an exited process) needs the owner's
/// reviewed launch.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnerlessOnline {
    Reconnect,
    OwnerRequired,
}

pub(crate) fn ownerless_online_decision(
    record: &cutex::session::model::CutexSessionRecord,
    process_alive: impl Fn(&cutex::session::model::CutexSessionRecord) -> bool,
) -> OwnerlessOnline {
    if record.is_retired()
        || record.app_server_runtime.is_none()
        || record.app_server_launch_claim_id.is_some()
        || !process_alive(record)
    {
        return OwnerlessOnline::OwnerRequired;
    }
    OwnerlessOnline::Reconnect
}

fn reconnect_live_explicit_runtime(
    key: &str,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    let _guard = MANAGEMENT_V2_SESSION_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| session_mutation_invalid("management v2 session mutation lock poisoned"))?;
    let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let record = store
        .sessions
        .get(key)
        .ok_or_else(|| session_mutation_invalid("agent disappeared"))?;
    if ownerless_online_decision(record, cutex::asks::wake::runtime_binding_live) != OwnerlessOnline::Reconnect {
        return Err(native_online_route_error(key));
    }
    let binding = record.app_server_runtime.as_ref().expect("checked above");
    let operation = || -> anyhow::Result<()> {
        anyhow::ensure!(
            !super::stock_lifecycle::stock_runtime_has_exited(record, binding)?,
            "the runtime exited; the owner has to start it"
        );
        super::stock_lifecycle::verify_stock_process(record, binding)?;
        super::app_server_runtime::verify_exact_live_runtime_claim(record, binding)?;
        super::stock_lifecycle::reconnect_ready_runtime(record, &store)?;
        Ok(())
    };
    operation().map_err(runtime_mutation_error)?;
    let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let record = &store.sessions[key];
    Ok(serde_json::json!({
        "runtimeGeneration": record.runtime_generation,
        "runtimeAgentId": record.current_runtime_agent_id,
        "status": "online",
        "reconnected": true,
    }))
}

fn native_online_route_error(
    id: &str,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "authorization".into(), code: "human_runtime_route_required".into(),
        message: "repeat cutex/runtime/online using the existing Human Management bearer, or use the Human explicit-launch endpoint".into(),
        retryable: false, outcome_unknown: false,
        details: serde_json::json!({"endpoint": "/v2/agent-management/explicit-launch", "operations": ["review_runtime", "run"], "command": format!("cutex human start {id}"), "attachCommand": format!("cutex session takeover {id}")}),
    }
}

fn mutate_management_v2_runtime(
    cutex_session_id: &str,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, cutex::management::v2::user_input::UserInputExecutionError> {
    use cutex::management::v2::model::MAX_SAFE_SEQUENCE;
    use cutex::management::v2::user_input::UserInputExecutionError;
    use cutex::session::service::cutex_session_key_for_user_id;

    // Optional fence: when supplied it must match; when omitted the mutation
    // applies to whatever runtime generation is current (last-write-wins).
    let supplied_generation = match params.get("expectedRuntimeGeneration") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|generation| *generation <= MAX_SAFE_SEQUENCE)
                .ok_or_else(|| {
                    session_mutation_invalid("expectedRuntimeGeneration must be a JSON-safe integer")
                })?,
        ),
    };
    let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let key = cutex_session_key_for_user_id(&store, cutex_session_id).ok_or_else(|| {
        UserInputExecutionError {
            stage: "route".to_string(),
            code: "session_not_found".to_string(),
            message: "the durable cutex session does not exist".to_string(),
            retryable: false,
            details: serde_json::json!({ "cutexSessionId": cutex_session_id }),
            outcome_unknown: false,
        }
    })?;
    let record = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| session_mutation_invalid("cutex session disappeared during mutation"))?;
    if record.claude_session.is_none()
        && record.explicit_launch.is_some()
        && method == "cutex/runtime/online"
    {
        // Native-launch agents start through their own launch contract; that
        // route is chosen by `online_management_v2_session` for the Human
        // bearer. Reaching here means the caller had no Human authority.
        return Err(native_online_route_error(&key));
    }
    let expected_generation = supplied_generation.unwrap_or(record.runtime_generation);
    if record.runtime_generation != expected_generation {

        return Err(UserInputExecutionError {
            stage: "route".to_string(),
            code: "revision_conflict".to_string(),
            message: format!(
                "runtime generation conflict: expected {expected_generation}, current {}",
                record.runtime_generation
            ),
            retryable: true,
            details: serde_json::json!({
                "expectedRuntimeGeneration": expected_generation,
                "currentRuntimeGeneration": record.runtime_generation,
                "resyncRequired": true,
            }),
            outcome_unknown: false,
        });
    }
    if record.claude_session.is_some() {
        if params.get("launchProfile").is_some() || params.get("openVisibleTerminal") == Some(&serde_json::Value::Bool(true)) {
            return Err(session_mutation_invalid("Claude uses its saved subscription settings; open its TUI with Attach"));
        }
        let outcome = match method {
            "cutex/runtime/online" => {
                cutex::runtime::claude::start_expected(&key, Some(expected_generation)).map_err(runtime_mutation_error)?;
                super::claude_runtime::observe(&key).map_err(runtime_mutation_error)?;
                "online"
            }
            "cutex/runtime/offline" | "cutex/runtime/close" => {
                cutex::runtime::claude::close_expected(&key, Some(expected_generation)).map_err(runtime_mutation_error)?;
                super::claude_runtime::disconnect(&key).map_err(runtime_mutation_error)?;
                if method == "cutex/runtime/close" { "closed" } else { "offline" }
            }
            _ => return Err(session_mutation_invalid("unsupported Claude runtime method")),
        };
        let current = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
        let current = current.sessions.get(&key).ok_or_else(|| session_mutation_invalid("Claude session disappeared"))?;
        return Ok(serde_json::json!({"runtimeGeneration":current.runtime_generation,"runtimeAgentId":current.current_runtime_agent_id,"status":outcome}));
    }
    let entry = coding_registration_from_cutex_session_record(&record).ok_or_else(|| {
        session_mutation_invalid("cutex session has no bound Codex thread identity")
    })?;
    let config = cutex::config::store::load_codez_config();
    let launch_profile = if method == "cutex/runtime/online" {
        match params.get("launchProfile") {
            Some(serde_json::Value::String(requested)) => {
                let current_host = cutex::platform::host::current_host_name();
                if !cutex::runtime::lifecycle::cutex_session_host_is_local(
                    &record.host_id,
                    &current_host,
                ) {
                    return Err(launch_profile_unavailable(
                        requested,
                        anyhow::anyhow!(
                            "one-launch profile must be resolved on target host {} (current host {})",
                            record.host_id,
                            current_host
                        ),
                    ));
                }
                Some(
                    super::launch::resolve_launch_profile_override(requested)
                        .map_err(|error| launch_profile_unavailable(requested, error))?,
                )
            }
            Some(_) => {
                return Err(session_mutation_invalid(
                    "launchProfile must be a non-empty string",
                ))
            }
            None => None,
        }
    } else {
        None
    };
    let mut profile_application = LaunchProfileApplication::default();

    let status = match method {
        "cutex/runtime/online" => {
            let open_visible_terminal = params
                .get("openVisibleTerminal")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let cutover: Option<(&'static str, Option<u32>)> = 'decide: {
            let manager_status = super::app_server_runtime::runtime_manager()
                .status(cutex_session_id)
                .map_err(session_mutation_persistence_error)?;
            if manager_status.as_ref().is_some_and(|status| {
                status.connected && status.runtime_generation != record.runtime_generation
            }) {
                break 'decide Some(("manager_generation_does_not_match_persisted_runtime", None));
            }
            if manager_status
                .as_ref()
                .is_some_and(|status| status.connected && record.app_server_runtime.is_none())
            {
                break 'decide Some(("manager_runtime_binding_missing", None));
            }
            let mut manager_connected = manager_status
                .as_ref()
                .is_some_and(|status| status.connected);
            if manager_connected
                && record
                    .app_server_runtime
                    .as_ref()
                    .is_some_and(|binding| !process_is_running(binding.pid))
            {
                // A manager worker can lag the child exit by one poll tick.
                // Do not let that stale in-memory status suppress persisted
                // ownership recovery or a typed cutover decision.
                let _ = super::app_server_runtime::disconnect_runtime(cutex_session_id);
                manager_connected = false;
            }
            if !manager_connected {
                let recovery_action =
                    super::app_server_runtime::classify_local_persisted_runtime_recovery(
                        &record,
                        process_is_running,
                    );
                let launch_new_runtime = match recovery_action {
                    super::app_server_runtime::PersistedRuntimeRecoveryAction::Reconnect {
                        runtime_agent_id,
                    } => {
                        let binding = record
                            .app_server_runtime
                            .as_ref()
                            .expect("reconnect action requires a persisted binding");
                        if let Err(error) = super::app_server_runtime::connect_runtime(
                            &config,
                            &record,
                            binding,
                            &runtime_agent_id,
                        ) {
                            let rollback =
                                super::app_server_runtime::restore_recovery_snapshot_if_owned(
                                    &record,
                                );
                            eprintln!(
                                "same-binding runtime recovery failed for {}: {error:#}",
                                record.cutex_session_id
                            );
                            if let Err(rollback_error) = rollback {
                                eprintln!(
                                    "same-binding runtime recovery rollback failed for {}: {rollback_error:#}",
                                    record.cutex_session_id
                                );
                                break 'decide Some(("same_binding_reconnect_rollback_failed", Some(binding.pid)));
                            }
                            break 'decide Some(("same_binding_reconnect_failed", Some(binding.pid)));
                        }
                        match super::app_server_runtime::runtime_manager()
                            .status(&record.cutex_session_id)
                        {
                            Ok(Some(status))
                                if status.connected
                                    && status.runtime_generation == record.runtime_generation =>
                            {
                            }
                            Ok(_) | Err(_) => {
                                let _ = super::app_server_runtime::disconnect_runtime(
                                    &record.cutex_session_id,
                                );
                                break 'decide Some(("manager_owner_not_connected_after_recovery", Some(binding.pid)));
                            }
                        }
                        match super::app_server_runtime::persisted_runtime_ownership_matches(
                            &record,
                        ) {
                            Ok(true) => {}
                            Ok(false) => {
                                let _ = super::app_server_runtime::disconnect_runtime(
                                    &record.cutex_session_id,
                                );
                                break 'decide Some(("runtime_changed_during_same_binding_reconnect", Some(binding.pid)));
                            }
                            Err(error) => {
                                let _ = super::app_server_runtime::disconnect_runtime(
                                    &record.cutex_session_id,
                                );
                                eprintln!(
                                    "failed to verify same-binding runtime ownership for {}: {error:#}",
                                    record.cutex_session_id
                                );
                                break 'decide Some(("runtime_ownership_verification_failed", Some(binding.pid)));
                            }
                        }
                        false
                    }
                    super::app_server_runtime::PersistedRuntimeRecoveryAction::ClearStaleAndLaunch => {
                        if let Err(error) =
                            super::app_server_runtime::clear_stale_persisted_runtime(&record)
                        {
                            eprintln!(
                                "stale runtime cleanup failed for {}: {error:#}",
                                record.cutex_session_id
                            );
                            break 'decide Some(("stale_runtime_cleanup_failed", record
                                    .app_server_runtime
                                    .as_ref()
                                    .map(|binding| binding.pid)));
                        }
                        true
                    }
                    super::app_server_runtime::PersistedRuntimeRecoveryAction::Launch => true,
                    super::app_server_runtime::PersistedRuntimeRecoveryAction::CutoverRequired {
                        reason,
                        pid,
                    } => break 'decide Some((reason, pid)),
                };
                if launch_new_runtime {
                    let outcome =
                        super::management_lifecycle::start_cutex_session_online_with_profile(
                            &config,
                            &entry,
                            launch_profile.as_ref(),
                        )
                        .map_err(runtime_mutation_error)?;
                    profile_application.runtime |= outcome.runtime_launched;
                    profile_application.tui |= outcome.tui_launched;
                }
            }
                None
            };
            if let Some((reason, pid)) = cutover {
                // Offline→online cutover is done automatically: stop whatever
                // the stale claim points at, then launch a fresh generation.
                auto_cutover_runtime(&record, &entry, &config, reason, pid)?;
                let outcome =
                    super::management_lifecycle::start_cutex_session_online_with_profile(
                        &config,
                        &entry,
                        launch_profile.as_ref(),
                    )
                    .map_err(runtime_mutation_error)?;
                profile_application.runtime |= outcome.runtime_launched;
                profile_application.tui |= outcome.tui_launched;
            }
            profile_application.tui |=
                super::management_lifecycle::ensure_managed_tui_peer_with_profile(
                    &entry,
                    launch_profile.as_ref(),
                )
                .map_err(runtime_mutation_error)?;
            let foreground_required_reason = if record.runtime_backend
                == cutex::session::model::CutexSessionRuntimeBackend::HostForeground
                && open_visible_terminal
            {
                match cutex::management::host_foreground_actions::try_start_host_foreground_desktop_terminal_with_profile(
                    &record,
                    launch_profile.as_ref().map(|profile| profile.effective_name()),
                ) {
                    Ok(true) => {
                        profile_application.tui = true;
                        None
                    }
                    Ok(false) => {
                        eprintln!(
                            "visible terminal is required for host_foreground session {}",
                            record.cutex_session_id
                        );
                        Some("desktop_launcher_unavailable")
                    }
                    Err(error) => {
                        eprintln!(
                            "failed to launch visible terminal for {}: {error:#}",
                            record.cutex_session_id
                        );
                        Some("desktop_launcher_failed")
                    }
                }
            } else {
                None
            };
            ("online", foreground_required_reason)
        }
        "cutex/runtime/offline" | "cutex/runtime/close" => {
            let force = runtime_stop_force_policy(
                method,
                params
                    .get("force")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                cfg!(windows),
            );
            let live_agents =
                super::management_lifecycle::live_agents_for_management_entry(&config, &entry);
            let stop = super::management_lifecycle::stop_cutex_session_runtime_for_entry_fenced(
                &entry,
                &live_agents,
                force,
                Some(expected_generation),
            )
            .map_err(runtime_mutation_error)?;
            if !stop.stopped {
                ("closing", None)
            } else if method == "cutex/runtime/close" {
                ("closed", None)
            } else {
                ("offline", None)
            }
        }
        _ => {
            return Err(session_mutation_invalid(
                "unsupported runtime mutation method",
            ))
        }
    };
    let store = load_cutex_session_store().map_err(session_mutation_persistence_error)?;
    let record = store
        .sessions
        .get(&key)
        .ok_or_else(|| session_mutation_invalid("cutex session disappeared after mutation"))?;
    let mut result = serde_json::json!({
        "runtimeGeneration": record.runtime_generation,
        "runtimeAgentId": record.current_runtime_agent_id,
        "status": status.0,
    });
    if let Some(reason) = status.1 {
        result
            .as_object_mut()
            .expect("runtime mutation result object")
            .insert(
                "foregroundRequiredReason".to_string(),
                serde_json::Value::String(reason.to_string()),
            );
    }
    let launched_provenance = record.app_server_runtime.as_ref().and_then(|binding| {
        binding.launched_profile.as_deref().map(|selected| {
            let source = binding
                .launch_profile_source
                .as_ref()
                .map(|source| source.as_str())
                .unwrap_or("unknown");
            (selected, source)
        })
    });
    if let Some(profile) = launch_profile.as_ref() {
        let receipt = profile_application
            .receipt(
                &profile.requested,
                profile.effective_name(),
                "one_launch_override",
            )
            .ok_or_else(|| launch_profile_not_applied(profile))?;
        result
            .as_object_mut()
            .expect("runtime mutation result object")
            .insert("launchProfile".to_string(), receipt);
    } else if let Some((selected, source @ ("session_configured" | "global_default" | "unknown"))) =
        launched_provenance
    {
        if let Some(receipt) = profile_application.receipt(selected, selected, source) {
            result
                .as_object_mut()
                .expect("runtime mutation result object")
                .insert("launchProfile".to_string(), receipt);
        }
    }
    Ok(result)
}

fn runtime_stop_force_policy(method: &str, requested_force: bool, windows: bool) -> bool {
    requested_force
        || (windows && matches!(method, "cutex/runtime/offline" | "cutex/runtime/close"))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LaunchProfileApplication {
    runtime: bool,
    tui: bool,
}

impl LaunchProfileApplication {
    fn scope(self) -> Option<&'static str> {
        match (self.runtime, self.tui) {
            (true, true) => Some("runtime_and_tui"),
            (true, false) => Some("runtime"),
            (false, true) => Some("tui"),
            (false, false) => None,
        }
    }

    fn receipt(self, requested: &str, effective: &str, source: &str) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "requested": requested,
            "selected": effective,
            "effective": effective,
            "source": source,
            "applicationScope": self.scope()?,
            "persisted": false,
        }))
    }
}

fn launch_profile_unavailable(
    requested: &str,
    error: anyhow::Error,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "runtime".to_string(),
        code: "launch_profile_unavailable".to_string(),
        message: format!("one-launch profile '{requested}' is unavailable: {error:#}"),
        retryable: false,
        details: serde_json::json!({
            "requested": requested,
            "persisted": false,
        }),
        outcome_unknown: false,
    }
}

fn launch_profile_not_applied(
    profile: &super::launch::ResolvedLaunchProfile,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "runtime".to_string(),
        code: "launch_profile_not_applied".to_string(),
        message:
            "the requested profile was not applied because the action reused existing processes"
                .to_string(),
        retryable: false,
        details: serde_json::json!({
            "requested": profile.requested,
            "effective": profile.effective_name(),
            "applicationScope": "none",
            "persisted": false,
        }),
        outcome_unknown: false,
    }
}

fn runtime_mutation_error(
    error: anyhow::Error,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "runtime".to_string(),
        code: "runtime_mutation_failed".to_string(),
        message: format!("{error:#}"),
        retryable: true,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

/// Perform the offline step of an offline→online cutover on the owner's
/// behalf. The stop is fenced on the inspected generation and forced; only a
/// stop that cannot complete still surfaces `cutover_required`.
fn auto_cutover_runtime(
    record: &cutex::session::model::CutexSessionRecord,
    entry: &cutex::im::registry::CodingSessionRegistration,
    config: &cutex::profiles::model::CodezConfig,
    reason: &'static str,
    pid: Option<u32>,
) -> Result<(), cutex::management::v2::user_input::UserInputExecutionError> {
    eprintln!(
        "automatic runtime cutover for {} ({reason}); stopping the previous runtime",
        record.cutex_session_id
    );
    let _ = super::app_server_runtime::disconnect_runtime(&record.cutex_session_id);
    let live_agents = super::management_lifecycle::live_agents_for_management_entry(config, entry);
    match super::management_lifecycle::stop_cutex_session_runtime_for_entry_fenced(
        entry,
        &live_agents,
        true,
        Some(record.runtime_generation),
    ) {
        Ok(stop) if stop.stopped => Ok(()),
        Ok(stop) => {
            let mut error = runtime_cutover_required(record, reason, pid);
            error.message = format!(
                "automatic cutover could not stop the previous runtime: {}",
                stop.detail
            );
            Err(error)
        }
        Err(stop_error) => {
            let mut error = runtime_cutover_required(record, reason, pid);
            error.message = format!(
                "automatic cutover could not stop the previous runtime: {stop_error:#}"
            );
            Err(error)
        }
    }
}

fn runtime_cutover_required(
    record: &cutex::session::model::CutexSessionRecord,
    reason: &'static str,
    pid: Option<u32>,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "runtime".to_string(),
        code: "cutover_required".to_string(),
        message:
            "the existing managed runtime could not be safely recovered; explicit offline/cutover is required before launching a new generation"
                .to_string(),
        retryable: false,
        details: serde_json::json!({
            "runtimeGeneration": record.runtime_generation,
            "runtimePid": pid,
            "reason": reason,
            "recoveryAttempts": 1,
            "requiredAction": "cutex/runtime/offline",
        }),
        outcome_unknown: false,
    }
}

fn apply_optional_string(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    target: &mut Option<String>,
) -> Result<(), cutex::management::v2::user_input::UserInputExecutionError> {
    let Some(value) = object.get(key) else {
        return Ok(());
    };
    *target = match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(value) => Some(value.clone()),
        _ => {
            return Err(session_mutation_invalid(&format!(
                "{key} must be string or null"
            )))
        }
    };
    Ok(())
}

fn json_string_array(
    value: &serde_json::Value,
    label: &str,
) -> Result<Vec<String>, cutex::management::v2::user_input::UserInputExecutionError> {
    value
        .as_array()
        .ok_or_else(|| session_mutation_invalid(&format!("{label} must be an array")))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| session_mutation_invalid(&format!("{label} must contain strings")))
        })
        .collect()
}

fn normalized_string_array(
    value: &serde_json::Value,
    label: &str,
) -> Result<Vec<String>, cutex::management::v2::user_input::UserInputExecutionError> {
    let mut values = json_string_array(value, label)?;
    if values.iter().any(|value| value.is_empty()) {
        return Err(session_mutation_invalid(&format!(
            "{label} must not contain empty strings"
        )));
    }
    values.sort();
    values.dedup();
    Ok(values)
}

/// `expectedRevision` is an optional fence: `None` when omitted or null.
fn optional_expected_revision(
    params: &serde_json::Value,
) -> Result<Option<u64>, cutex::management::v2::user_input::UserInputExecutionError> {
    match params.get("expectedRevision") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|revision| *revision <= cutex::management::v2::model::MAX_SAFE_SEQUENCE)
            .map(Some)
            .ok_or_else(|| session_mutation_invalid("expectedRevision must be a JSON-safe integer")),
    }
}

fn session_mutation_invalid(
    message: &str,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "route".to_string(),
        code: "invalid_request".to_string(),
        message: message.to_string(),
        retryable: false,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

fn session_mutation_invalid_error(
    error: anyhow::Error,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    session_mutation_invalid(&format!("{error:#}"))
}

pub(super) fn session_revision_conflict(
    expected_revision: u64,
    current_revision: u64,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "route".to_string(),
        code: "revision_conflict".to_string(),
        message: format!(
            "session revision conflict: expected {expected_revision}, current {current_revision}"
        ),
        retryable: true,
        details: serde_json::json!({
            "expectedRevision": expected_revision,
            "currentRevision": current_revision,
            "resyncRequired": true,
        }),
        outcome_unknown: false,
    }
}

fn session_mutation_persistence_error(
    error: anyhow::Error,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "route".to_string(),
        code: "event_persistence_unavailable".to_string(),
        message: format!("{error:#}"),
        retryable: true,
        details: serde_json::json!({}),
        outcome_unknown: false,
    }
}

fn session_mutation_persistence_uncertain(
    detail: impl Into<String>,
) -> cutex::management::v2::user_input::UserInputExecutionError {
    cutex::management::v2::user_input::UserInputExecutionError {
        stage: "persistence".to_string(),
        code: "persistence_uncertain".to_string(),
        message: "the durable session outcome is uncertain; resync before retrying".to_string(),
        retryable: false,
        details: serde_json::json!({
            "diagnostic": detail.into(),
            "resyncRequired": true,
        }),
        outcome_unknown: true,
    }
}

fn flush_management_user_input_queue(
    cutex_session_id: &str,
    max_items: usize,
    ignore_pause: bool,
) -> anyhow::Result<usize> {
    let mut flushed = 0;
    while flushed < max_items
        && super::app_server_user_input::flush_queued(cutex_session_id, ignore_pause)?
    {
        flushed += 1;
    }
    Ok(flushed)
}

fn respond_management_native_server_request(
    cutex_session_id: &str,
    expected_runtime_generation: u64,
    message: serde_json::Value,
) -> Result<(), ManagementNativeForwardError> {
    use cutex::app_server::client::AppServerClientError;
    use cutex::app_server::manager::AppServerExactRuntimeHandleError;

    let handle = selected_native_manager(cutex_session_id)
        .map_err(|error| ManagementNativeForwardError::BeforeForward(format!("{error:#}")))?
        .handle_for_generation(cutex_session_id, expected_runtime_generation)
        .map_err(|error| match error {
            AppServerExactRuntimeHandleError::StaleGeneration { expected, actual } => {
                ManagementNativeForwardError::StaleRuntimeGeneration { expected, actual }
            }
            AppServerExactRuntimeHandleError::Unavailable(message) => {
                ManagementNativeForwardError::BeforeForward(message)
            }
        })?;
    handle.respond_raw(message).map_err(|error| match error {
        AppServerClientError::InvalidEndpoint(message)
        | AppServerClientError::InvalidOptions(message)
        | AppServerClientError::Connect(message)
        | AppServerClientError::Protocol(message)
        | AppServerClientError::RequestIdInUse(message) => {
            ManagementNativeForwardError::BeforeForward(message)
        }
        AppServerClientError::Backpressure(queue) => {
            ManagementNativeForwardError::BeforeForward(format!("app-server {queue} queue is full"))
        }
        AppServerClientError::Shutdown => ManagementNativeForwardError::BeforeForward(
            "app-server client is shut down".to_string(),
        ),
        AppServerClientError::Transport(message) | AppServerClientError::Disconnected(message) => {
            ManagementNativeForwardError::OutcomeUnknown(message)
        }
        AppServerClientError::Timeout { method } => ManagementNativeForwardError::OutcomeUnknown(
            format!("timed out waiting for app-server {method}"),
        ),
        AppServerClientError::Rpc(error) => ManagementNativeForwardError::OutcomeUnknown(format!(
            "unexpected native RPC error {}: {}",
            error.code, error.message
        )),
    })
}

fn forward_management_native_request(
    cutex_session_id: &str,
    message: serde_json::Value,
) -> Result<serde_json::Value, ManagementNativeForwardError> {
    use cutex::app_server::client::AppServerClientError;

    let handle = selected_native_manager(cutex_session_id)
        .map_err(|error| ManagementNativeForwardError::BeforeForward(format!("{error:#}")))?
        .handle(cutex_session_id)
        .map_err(|error| ManagementNativeForwardError::BeforeForward(format!("{error:#}")))?;
    handle
        .request_raw_message(message)
        .map_err(|error| match error {
            AppServerClientError::RequestIdInUse(message) => {
                ManagementNativeForwardError::NativeRequestIdInUse(message)
            }
            AppServerClientError::InvalidEndpoint(message)
            | AppServerClientError::InvalidOptions(message)
            | AppServerClientError::Connect(message)
            | AppServerClientError::Protocol(message) => {
                ManagementNativeForwardError::BeforeForward(message)
            }
            AppServerClientError::Backpressure(queue) => {
                ManagementNativeForwardError::BeforeForward(format!(
                    "app-server {queue} queue is full"
                ))
            }
            AppServerClientError::Shutdown => ManagementNativeForwardError::BeforeForward(
                "app-server client is shut down".to_string(),
            ),
            AppServerClientError::Transport(message)
            | AppServerClientError::Disconnected(message) => {
                ManagementNativeForwardError::OutcomeUnknown(message)
            }
            AppServerClientError::Timeout { method } => {
                ManagementNativeForwardError::OutcomeUnknown(format!(
                    "timed out waiting for app-server {method}"
                ))
            }
            AppServerClientError::Rpc(error) => {
                ManagementNativeForwardError::OutcomeUnknown(format!(
                    "unexpected collapsed native RPC error {}: {}",
                    error.code, error.message
                ))
            }
        })
}

/// Picks the runtime manager for a native forward. Only the record's kind is
/// needed, so this reads the shared snapshot (no whole-store copy).
fn selected_native_manager(id: &str) -> anyhow::Result<&'static cutex::app_server::manager::AppServerRuntimeManager> {
    let is_claude = cutex::session::store::with_cutex_session_store(|store| {
        store
            .sessions
            .get(id)
            .is_some_and(|record| record.claude_session.is_some())
    })?;
    if is_claude {
        Ok(super::claude_runtime::manager())
    } else {
        Ok(super::app_server_runtime::runtime_manager())
    }
}

pub(crate) fn load_app_server_runtime_status(
    cutex_session_id: &str,
) -> anyhow::Result<Option<cutex::app_server::manager::AppServerManagedRuntimeStatus>> {
    if let Some(status) = super::claude_runtime::manager().status(cutex_session_id)? {
        return Ok(Some(status));
    }
    super::app_server_runtime::runtime_manager().status(cutex_session_id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_permission_mode_needs_a_claude_profile_and_an_offering_runtime() {
        let _home = super::super::test_home::IsolatedTestHome::new("claude-launch-mode").unwrap();
        let launch: cutex::config::new_thread::LaunchOverrides =
            serde_json::from_value(serde_json::json!({"claudePermissionMode": "plan"})).unwrap();
        let codex = super::refuse_claude_mode_for_codex(Some(&launch)).unwrap_err();
        assert_eq!(codex.code, "unsupported_launch_option");
        assert!(super::refuse_claude_mode_for_codex(None).is_ok());
        // No Claude runtime installed in the test home: no mode is offered.
        let account = cutex::profiles::lifecycle::new_claude_account("Claude main");
        let config = cutex::profiles::model::CodezConfig::default();
        let refused = super::claude_launch_defaults(&account, &config, Some(&launch)).unwrap_err();
        assert_eq!(refused.code, "unsupported_launch_option");
        assert!(refused.message.contains("does not offer"));
        let model_only: cutex::config::new_thread::LaunchOverrides =
            serde_json::from_value(serde_json::json!({"model": "sonnet"})).unwrap();
        assert!(super::claude_launch_defaults(&account, &config, Some(&model_only)).is_ok());
    }

    /// r208 C: an explicit-launch agent's online request without an owner
    /// (asks auto-wake) reconnects to a live exact binding; with no binding,
    /// a launch claim or a dead process it needs the owner.
    #[test]
    fn ownerless_online_reconnects_only_a_live_explicit_binding() {
        let mut record = cutex::session::model::CutexSessionRecord::new_at(
            "cutex-1".into(),
            None,
            cutex::platform::host::current_host_name(),
            "/missing".into(),
            None,
            "2026-08-07T00:00:00Z".into(),
        )
        .unwrap();
        record.explicit_launch = Some(
            serde_json::from_value(serde_json::json!({
                "version":999,"native_id":"invalid","native_home":"/missing",
                "bundle_manifest":"/missing","bundle_sha256":"a".repeat(64)
            }))
            .unwrap(),
        );
        let alive = |_: &cutex::session::model::CutexSessionRecord| true;
        let dead = |_: &cutex::session::model::CutexSessionRecord| false;
        assert_eq!(super::ownerless_online_decision(&record, alive), super::OwnerlessOnline::OwnerRequired, "no binding");
        record.app_server_runtime = Some(serde_json::from_value(serde_json::json!({
            "transport": "loopback_web_socket", "endpoint": "ws://127.0.0.1:1", "pid": 4242,
            "runtime_dir": "/missing", "diagnostic_journal_path": "/missing/j", "schema_version": "v",
            "schema_sha256": "b".repeat(64), "started_at": "2026-08-07T00:00:00Z"
        })).unwrap());
        record.current_runtime_agent_id = Some("runtime-agent".into());
        assert_eq!(super::ownerless_online_decision(&record, alive), super::OwnerlessOnline::Reconnect);
        assert_eq!(super::ownerless_online_decision(&record, dead), super::OwnerlessOnline::OwnerRequired);
        record.app_server_launch_claim_id = Some("claim".into());
        assert_eq!(super::ownerless_online_decision(&record, alive), super::OwnerlessOnline::OwnerRequired, "launch claim");
    }

    #[test]
    fn native_online_without_human_bearer_returns_exact_supported_routes() {
        let error = super::native_online_route_error("cutex.example");
        assert_eq!(error.code, "human_runtime_route_required");
        assert!(!error.retryable);
        assert!(!error.outcome_unknown);
        assert_eq!(
            error.details["endpoint"],
            "/v2/agent-management/explicit-launch"
        );
        assert_eq!(error.details["command"], "cutex human start cutex.example");
        assert_eq!(
            error.details["attachCommand"],
            "cutex session takeover cutex.example"
        );
    }

    /// One request through `handle_v2_request` (bridge token `bridge-test`,
    /// root `root-test`) on a real socket; returns the raw response.
    fn serve_v2_once(method: &str, path: &str, bearer: Option<&str>) -> String {
        use cutex::http::server::SimpleHttpRequest;
        use std::io::Read;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut socket = std::net::TcpStream::connect(address).unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            response
        });
        let (mut socket, _) = listener.accept().unwrap();
        let request = SimpleHttpRequest {
            method: method.into(),
            path: path.into(),
            headers: bearer
                .map(|bearer| ("authorization".to_string(), format!("Bearer {bearer}")))
                .into_iter()
                .collect(),
            body: b"{}".to_vec(),
        };
        cutex::management::v2::server::handle_v2_request(
            &mut socket,
            &request,
            Some("bridge-test"),
            Some("root-test"),
            management_request_context(),
        )
        .unwrap();
        drop(socket);
        client.join().unwrap()
    }

    fn assert_gone(response: &str, label: &str) {
        assert!(response.starts_with("HTTP/1.1 410 Gone"), "{label}: {response}");
        let body: serde_json::Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "tool_removed", "{label}");
        assert_eq!(body["error"]["retryable"], false, "{label}");
    }

    /// Only the Management diagnostic error log may have been written.
    fn assert_no_state_written(home: &crate::cli_app::test_home::IsolatedTestHome) {
        let mut pending = vec![home.root().to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    assert!(
                        name == ".cutex-test-private-home"
                            || path.parent().is_some_and(|p| p.ends_with("service-logs")),
                        "unexpected state write: {}",
                        path.display()
                    );
                }
            }
        }
    }

    /// r219: Release rotation is gone; its Management routes answer 410
    /// `tool_removed` without credentials and before any state access.
    #[test]
    fn removed_release_rotation_routes_answer_gone_without_credentials() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("r219-gone").unwrap();
        for (method, path) in [
            ("GET", "/v2/release-rotation"),
            ("GET", "/v2/release-rotation/template"),
            ("POST", "/v2/release-rotation/template"),
            ("POST", "/v2/release-rotation/retry"),
            ("POST", "/v2/release-rotation/director-request"),
            ("POST", "/v2/release-rotation?x=1"),
        ] {
            assert_gone(&serve_v2_once(method, path, None), &format!("{method} {path}"));
        }
        assert_no_state_written(&home);
    }

    /// r219: the Task Service (incl. seat administration and the owner task
    /// read) answers 410 `tool_removed` before authentication: without a
    /// credential, with the ordinary bearer, with the root credential and with
    /// a former owner-task read token. Neighbouring session / project routes
    /// keep their own authentication; nothing touches state.
    #[test]
    fn removed_task_service_routes_answer_gone_with_or_without_credentials() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("r219-tasks-gone").unwrap();
        for (method, path) in [
            ("POST", "/v2/task-service/seats/bind"),
            ("GET", "/v2/task-service/seats"),
            ("POST", "/v2/task-service/management-query"),
            ("POST", "/v2/task-service/human-recovery"),
            ("GET", "/v2/task-service/anything"),
            ("GET", "/v2/tasks/task-1"),
            ("GET", "/v2/tasks/task-1?x=1"),
            ("GET", "/v2/sessions/cutex.worker/tasks"),
            ("GET", "/v2/sessions/cutex.worker/tasks?state=open&limit=5"),
            ("GET", "/v2/projects/project-alpha/tasks"),
            ("GET", "/v2/projects/project-alpha/tasks?limit=1"),
        ] {
            for bearer in [None, Some("bridge-test"), Some("root-test"), Some("owner-reader-token")] {
                assert_gone(
                    &serve_v2_once(method, path, bearer),
                    &format!("{method} {path} {bearer:?}"),
                );
            }
        }
        for (method, path) in [
            ("GET", "/v2/projects/project-alpha/memory"),
            ("GET", "/v2/projects/project-alpha/fs/dir"),
            ("GET", "/v2/projects/project-alpha/mcp"),
            ("GET", "/v2/projects/project-alpha/tasks/task-1"),
            ("GET", "/v2/sessions/cutex.worker/history"),
            ("GET", "/v2/sessions/cutex.worker/tasks/extra"),
        ] {
            let response = serve_v2_once(method, path, None);
            assert!(
                response.starts_with("HTTP/1.1 401 Unauthorized"),
                "{method} {path}: {response}"
            );
        }
        assert_no_state_written(&home);
    }

    /// The seat credential is gone; the root-only Agent Management and Job
    /// Service routes still refuse the ordinary bearer and a missing one.
    #[test]
    fn legacy_import_and_job_service_routes_still_require_the_root_credential() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("r219-root-routes").unwrap();
        for (method, path) in [
            ("POST", "/v2/agent-management/legacy-director-ownership-import"),
            ("POST", "/v2/agent-management/reservation-reconciliation"),
            ("POST", "/v2/job-service/management-query"),
            ("GET", "/v2/job-service/capabilities"),
            ("POST", "/v2/job-service/cancel"),
            ("POST", "/v2/job-service/output"),
        ] {
            for bearer in [None, Some("bridge-test"), Some("wrong")] {
                let response = serve_v2_once(method, path, bearer);
                assert!(
                    response.starts_with("HTTP/1.1 401 Unauthorized"),
                    "{method} {path} {bearer:?}: {response}"
                );
            }
        }
        assert_no_state_written(&home);
    }
    use super::*;
    use crate::cli_app::test_home::IsolatedTestHome;
    use cutex::agent_bus::model::{AgentBusAgent, AgentBusRegisterRequest};
    use cutex::config::paths::config_dir;
    use cutex::config::store::save_codez_config;
    use cutex::im::registry::load_im_registry;
    use cutex::profiles::materialize::materialized_account_files;
    use cutex::profiles::model::{AccountsStore, CliKind, RuntimeConfig, StoredAccount};
    use cutex::profiles::store::save_store;
    use cutex::session::model::{
        CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore,
    };
    use cutex::session::store::save_cutex_session_store;
    use serde_json::json;
    use std::fs;
    use std::io;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn profile(name: &str) -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            claude_code: Default::default(),
            id: format!("{name}-id"),
            name: name.to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: Some("test".to_string()),
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn owner_hidden_stop_http_preserves_visibility_and_rejects_other_callers() {
        use cutex::http::server::SimpleHttpRequest;
        use std::io::Read;
        let _fixture = ProductionHandlerFixture::new();
        let id = "cutex.hidden-owner-stop";
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id();
        let reaper = thread::spawn(move || child.wait().unwrap());
        let mut record = CutexSessionRecord::new(
            id.into(),
            Some("018e0000-0000-7000-8000-0000e0009b50".into()),
            cutex::platform::host::current_host_name(),
            _fixture.home.root().to_string_lossy().into(),
            None,
        )
        .unwrap();
        record.registration_class = cutex::agent_bus::model::AgentRegistrationClass::Persistent;
        record.runtime_generation = 1;
        record.runtime_pid = Some(pid);
        record.exposed_to_backend = false;
        let mut store = CutexSessionStore::default();
        store.sessions.insert(id.into(), record);
        save_cutex_session_store(&store).unwrap();
        let route = |method: &str, path: &str, token: &str, body: serde_json::Value| {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let client = thread::spawn(move || {
                let mut socket = std::net::TcpStream::connect(address).unwrap();
                let mut response = String::new();
                socket.read_to_string(&mut response).unwrap();
                response
            });
            let (mut socket, _) = listener.accept().unwrap();
            let request = SimpleHttpRequest {
                method: method.into(),
                path: path.into(),
                headers: [("authorization".into(), format!("Bearer {token}"))]
                    .into_iter()
                    .collect(),
                body: serde_json::to_vec(&body).unwrap(),
            };
            cutex::management::v2::server::handle_v2_request(
                &mut socket,
                &request,
                Some("bridge-test"),
                Some("root-test"),
                management_request_context(),
            )
            .unwrap();
            drop(socket);
            client.join().unwrap()
        };
        let path = format!("/v2/sessions/{id}/cutex/requests");
        let stop = |request_id: &str, method: &str| json!({"requestId":request_id,"method":method,"params":{"expectedRuntimeGeneration":1,"force":false}});
        let before = cutex::session::store::session_store_json_bytes(&cutex::session::store::cutex_sessions_path().unwrap()).unwrap();
        for (token, status) in [("wrong", "401")] {
            let response = route(
                "POST",
                &path,
                token,
                stop("denied", "cutex/runtime/offline"),
            );
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status}")),
                "{response}"
            );
            assert_eq!(
                cutex::session::store::session_store_json_bytes(&cutex::session::store::cutex_sessions_path().unwrap()).unwrap(),
                before
            );
        }
        // im-exposure-retired: the legacy IM flag no longer hides a durable
        // agent from the owner's list.
        let visible = route("GET", "/v2/sessions", "bridge-test", json!({}));
        assert!(visible.contains(id), "{visible}");
        let response = route(
            "POST",
            &path,
            "root-test",
            stop("owner-stop", "cutex/runtime/offline"),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"status\":\"offline\""), "{response}");
        reaper.join().unwrap();
        let current = load_cutex_session_store().unwrap();
        assert!(!current.sessions[id].exposed_to_backend);
        assert!(current.sessions[id].runtime_pid.is_none());
        assert!(route("GET", "/v2/sessions", "bridge-test", json!({})).contains(id));
        assert!(route(
            "GET",
            &format!("/v2/sessions/{id}"),
            "bridge-test",
            json!({})
        )
        .starts_with("HTTP/1.1 200"));
        assert!(route(
            "POST",
            &path,
            "root-test",
            stop("owner-close", "cutex/runtime/close")
        )
        .starts_with("HTTP/1.1 200"));
        for foreign in [true, false] {
            let mut current = load_cutex_session_store().unwrap();
            let record = current.sessions.get_mut(id).unwrap();
            if foreign {
                record.host_id = "foreign-host".into();
            } else {
                record.host_id = cutex::platform::host::current_host_name();
                record.retired_at = Some("2026-09-01T00:00:00Z".into());
                record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
            }
            save_cutex_session_store(&current).unwrap();
            let before = cutex::session::store::session_store_json_bytes(&cutex::session::store::cutex_sessions_path().unwrap()).unwrap();
            let response = route(
                "POST",
                &path,
                "root-test",
                stop("foreign-retired", "cutex/runtime/offline"),
            );
            assert!(response.starts_with("HTTP/1.1 404"), "{response}");
            assert_eq!(
                cutex::session::store::session_store_json_bytes(&cutex::session::store::cutex_sessions_path().unwrap()).unwrap(),
                before
            );
        }
    }

    fn save_resolvable_profile(account: &StoredAccount) {
        save_store(&AccountsStore {
            version: 3,
            accounts: vec![account.clone()],
            active_account_id: Some(account.id.clone()),
        })
        .expect("save profile store");
        let files = materialized_account_files(account).expect("profile files");
        fs::create_dir_all(files.auth_path.parent().expect("profile parent"))
            .expect("create profile directory");
        fs::write(&files.auth_path, "{}\n").expect("write profile auth");
        fs::write(&files.config_path, "model = \"test\"\n").expect("write profile config");
    }

    fn save_local_session(id: &str) {
        let record = CutexSessionRecord::new_at(
            id.to_string(),
            Some("thread-management-profile".to_string()),
            cutex::platform::host::current_host_name(),
            "/tmp/management-profile".to_string(),
            None,
            "2026-08-15T00:00:00Z".to_string(),
        )
        .expect("session record");
        let mut store = CutexSessionStore::default();
        store.sessions.insert(id.to_string(), record);
        save_cutex_session_store(&store).expect("save session store");
    }

    #[cfg(unix)]
    struct ProductionHandlerFixture {
        home: IsolatedTestHome,
        bus_stop: Arc<AtomicBool>,
        bus_thread: Option<thread::JoinHandle<()>>,
        old_cutex_codex_bin: Option<std::ffi::OsString>,
        old_cutex_alden_bin: Option<std::ffi::OsString>,
        old_alden_registry: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl ProductionHandlerFixture {
        fn new() -> Self {
            use cutex::http::server::{
                read_simple_http_request, write_http_response, write_json_response,
            };
            use std::os::unix::fs::PermissionsExt;

            let home = IsolatedTestHome::new("c20").expect("create isolated fixture HOME");
            let root = home.root().to_path_buf();
            let codex = root.join("fake-codex.py");
            fs::write(
                &codex,
                r##"#!/usr/bin/python3
import base64, hashlib, json, os, socket, struct, sys, time

def recv_exact(conn, count):
    data = b''
    while len(data) < count:
        part = conn.recv(count - len(data))
        if not part: return None
        data += part
    return data

def recv_frame(conn):
    head = recv_exact(conn, 2)
    if not head: return None
    opcode = head[0] & 15
    size = head[1] & 127
    if size == 126: size = struct.unpack('!H', recv_exact(conn, 2))[0]
    elif size == 127: size = struct.unpack('!Q', recv_exact(conn, 8))[0]
    mask = recv_exact(conn, 4) if (head[1] & 128) else None
    data = recv_exact(conn, size) or b''
    if mask: data = bytes(value ^ mask[index % 4] for index, value in enumerate(data))
    return opcode, data

def send_json(conn, value):
    data = json.dumps(value).encode()
    size = len(data)
    if size < 126: header = bytes([129, size])
    elif size < 65536: header = bytes([129, 126]) + struct.pack('!H', size)
    else: header = bytes([129, 127]) + struct.pack('!Q', size)
    conn.sendall(header + data)

if '--listen' not in sys.argv:
    while True: time.sleep(1)
path = sys.argv[sys.argv.index('--listen') + 1].removeprefix('unix://')
try: os.unlink(path)
except FileNotFoundError: pass
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(path)
server.listen(4)
while True:
    conn, _ = server.accept()
    headers = b''
    while b'\r\n\r\n' not in headers: headers += conn.recv(1024)
    key = [line.split(b': ', 1)[1] for line in headers.split(b'\r\n') if line.lower().startswith(b'sec-websocket-key:')][0]
    accept = base64.b64encode(hashlib.sha1(key + b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
    conn.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + b'\r\n\r\n')
    while True:
        frame = recv_frame(conn)
        if not frame: break
        opcode, payload = frame
        if opcode == 8: break
        if opcode != 1: continue
        request = json.loads(payload.decode())
        if 'id' not in request: continue
        if request.get('method') == 'initialize': result = {'userAgent': 'fixture'}
        elif request.get('method') == 'thread/resume': result = {'thread': {'id': request['params']['threadId'], 'status': {'type': 'idle'}}}
        else: result = {}
        send_json(conn, {'id': request['id'], 'result': result})
    conn.close()
"##,
            )
            .expect("write fake app-server program");
            let alden = root.join("fake-alden.sh");
            fs::write(
                &alden,
                "#!/bin/sh\nif [ \"$1\" = \"--list\" ]; then\n  if [ -f \"$CUTEX_TEST_ALDEN_REGISTRY\" ]; then\n    while read -r pid name; do\n      if kill -0 \"$pid\" 2>/dev/null; then printf '%s\\t%s\\n' \"$pid\" \"$name\"; fi\n    done < \"$CUTEX_TEST_ALDEN_REGISTRY\"\n  fi\n  exit 0\nfi\nname=''\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--name\" ]; then name=\"$2\"; shift 2; continue; fi\n  if [ \"$1\" = \"--\" ]; then shift; break; fi\n  shift\ndone\nprintf '%s\\t%s\\n' \"$$\" \"$name\" > \"$CUTEX_TEST_ALDEN_REGISTRY\"\nexec \"$@\"\n",
            )
            .expect("write fake cute-alden program");
            fs::set_permissions(&codex, fs::Permissions::from_mode(0o700))
                .expect("make fake app-server executable");
            fs::set_permissions(&alden, fs::Permissions::from_mode(0o700))
                .expect("make fake cute-alden executable");

            let listener = (24_000..25_000)
                .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
                .expect("bind fixture Agent Bus in the permitted port range");
            listener
                .set_nonblocking(true)
                .expect("make fixture Agent Bus nonblocking");
            let port = listener
                .local_addr()
                .expect("fixture Agent Bus address")
                .port();
            let registered = Arc::new(Mutex::new(None::<AgentBusAgent>));
            let registered_for_thread = Arc::clone(&registered);
            let bus_stop = Arc::new(AtomicBool::new(false));
            let bus_stop_for_thread = Arc::clone(&bus_stop);
            let bus_thread = thread::spawn(move || {
                while !bus_stop_for_thread.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_simple_http_request(&mut stream)
                                .expect("read fixture Agent Bus request");
                            let path = request.path.split('?').next().unwrap_or(&request.path);
                            match (request.method.as_str(), path) {
                                ("GET", "/") => {
                                    write_http_response(&mut stream, 200, "OK", "text/plain", b"ok")
                                        .expect("write fixture Agent Bus health")
                                }
                                ("POST", "/api/agents/register") => {
                                    let registration: AgentBusRegisterRequest =
                                        serde_json::from_slice(&request.body)
                                            .expect("parse fixture runtime registration");
                                    *registered_for_thread
                                        .lock()
                                        .expect("fixture registration lock") =
                                        Some(AgentBusAgent {
                                            id: registration.id,
                                            // Keep the response's durable fields equal to the
                                            // fixture record. The handler still consumes the actual
                                            // registration endpoint, while this isolates the launch
                                            // occurrence from unrelated Agent Bus reconciliation.
                                            name: "fixture".to_string(),
                                            base_name: None,
                                            thread_name: None,
                                            path_key: None,
                                            session_id: registration.session_id,
                                            cutex_session_id: None,
                                            profile: registration.profile,
                                            cwd: registration.cwd,
                                            pid: registration.pid,
                                            host_id: registration.host_id,
                                            groups: Vec::new(),
                                            registration_class: registration.registration_class,
                                            last_seen_epoch_secs: 1,
                                        });
                                    write_json_response(
                                        &mut stream,
                                        200,
                                        "OK",
                                        &json!({ "ok": true }),
                                    )
                                    .expect("write fixture runtime registration");
                                }
                                ("GET", "/api/agents") => {
                                    let agents = registered_for_thread
                                        .lock()
                                        .expect("fixture registration lock")
                                        .clone()
                                        .into_iter()
                                        .collect::<Vec<_>>();
                                    write_json_response(
                                        &mut stream,
                                        200,
                                        "OK",
                                        &serde_json::to_value(agents)
                                            .expect("serialize fixture agents"),
                                    )
                                    .expect("write fixture agents");
                                }
                                ("GET", "/api/messages/poll") => write_json_response(
                                    &mut stream,
                                    200,
                                    "OK",
                                    &json!({ "messages": [] }),
                                )
                                .expect("write fixture poll"),
                                ("POST", "/api/agents/unregister") => write_json_response(
                                    &mut stream,
                                    200,
                                    "OK",
                                    &json!({ "ok": true, "removed": true }),
                                )
                                .expect("write fixture unregister"),
                                _ => write_json_response(
                                    &mut stream,
                                    200,
                                    "OK",
                                    &json!({ "ok": true }),
                                )
                                .expect("write fixture Agent Bus response"),
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("fixture Agent Bus accept failed: {error}"),
                    }
                }
            });

            let old_cutex_codex_bin = std::env::var_os("CUTEX_CODEX_BIN");
            let old_cutex_alden_bin = std::env::var_os("CUTEX_ALDEN_BIN");
            let old_alden_registry = std::env::var_os("CUTEX_TEST_ALDEN_REGISTRY");
            unsafe {
                std::env::set_var("CUTEX_CODEX_BIN", &codex);
                std::env::set_var("CUTEX_ALDEN_BIN", &alden);
                std::env::set_var("CUTEX_TEST_ALDEN_REGISTRY", root.join("alden-registry"));
            }
            let config = cutex::profiles::model::CodezConfig {
                agent_bus_enabled: true,
                agent_bus_port: Some(port),
                ..Default::default()
            };
            save_codez_config(&config).expect("save fixture config");
            Self {
                home,
                bus_stop,
                bus_thread: Some(bus_thread),
                old_cutex_codex_bin,
                old_cutex_alden_bin,
                old_alden_registry,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for ProductionHandlerFixture {
        fn drop(&mut self) {
            self.bus_stop.store(true, Ordering::Release);
            let _ = std::net::TcpStream::connect(("127.0.0.1", 9));
            if let Some(thread) = self.bus_thread.take() {
                let _ = thread.join();
            }
            unsafe {
                match self.old_cutex_codex_bin.take() {
                    Some(value) => std::env::set_var("CUTEX_CODEX_BIN", value),
                    None => std::env::remove_var("CUTEX_CODEX_BIN"),
                }
                match self.old_cutex_alden_bin.take() {
                    Some(value) => std::env::set_var("CUTEX_ALDEN_BIN", value),
                    None => std::env::remove_var("CUTEX_ALDEN_BIN"),
                }
                match self.old_alden_registry.take() {
                    Some(value) => std::env::set_var("CUTEX_TEST_ALDEN_REGISTRY", value),
                    None => std::env::remove_var("CUTEX_TEST_ALDEN_REGISTRY"),
                }
            }
            // `user_input_repository` is process-global. Keep this disposable
            // fixture root alive for the rest of the test process after the
            // first production launch initializes that repository.
            self.home.retain_root();
        }
    }

    #[cfg(unix)]
    fn save_runtime_session(
        id: &str,
        profile: Option<&str>,
        backend: CutexSessionRuntimeBackend,
    ) -> CutexSessionRecord {
        let cwd = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .expect("fixture HOME is set")
            .join("work");
        fs::create_dir_all(&cwd).expect("create fixture runtime working directory");
        let mut record = CutexSessionRecord::new_at(
            id.to_string(),
            Some(format!("thread-{id}")),
            cutex::platform::host::current_host_name(),
            cwd.display().to_string(),
            profile.map(str::to_string),
            "2026-08-15T00:00:00Z".to_string(),
        )
        .expect("create runtime session");
        record.runtime_backend = backend;
        record.display_name_hint = Some("fixture".to_string());
        record.agent_enabled = true;
        let codex_home =
            cutex::config::paths::host_codex_home_dir().expect("resolve fixture Codex home");
        fs::create_dir_all(&codex_home).expect("create fixture Codex home");
        fs::write(
            codex_home.join("session_index.jsonl"),
            format!(
                "{{\"id\":\"{}\",\"timestamp\":\"2026-08-15T00:00:00Z\"}}\n",
                record
                    .codex_session_id
                    .as_deref()
                    .expect("fixture Codex session id")
            ),
        )
        .expect("write fixture Codex session index");
        let mut store = load_cutex_session_store().expect("load fixture session store");
        store.sessions.clear();
        store.sessions.insert(id.to_string(), record.clone());
        save_cutex_session_store(&store).expect("save runtime session");
        record
    }

    #[cfg(unix)]
    fn assert_production_receipt(
        result: &serde_json::Value,
        selected: &str,
        source: &str,
        scope: &str,
    ) {
        assert_eq!(result["launchProfile"]["selected"], selected);
        assert_eq!(result["launchProfile"]["effective"], selected);
        assert_eq!(result["launchProfile"]["source"], source);
        assert_eq!(result["launchProfile"]["applicationScope"], scope);
        assert_eq!(result["launchProfile"]["persisted"], false);
    }

    #[cfg(unix)]
    fn stop_production_runtime(id: &str, generation: u64) {
        mutate_management_v2_session(
            id,
            "cutex/runtime/offline",
            json!({ "expectedRuntimeGeneration": generation, "force": true }),
        )
        .expect("stop fixture runtime");
    }

    #[test]
    fn one_launch_profile_application_scope_tracks_only_started_processes() {
        assert_eq!(LaunchProfileApplication::default().scope(), None);
        assert_eq!(
            LaunchProfileApplication {
                runtime: true,
                tui: false,
            }
            .scope(),
            Some("runtime")
        );
        assert_eq!(
            LaunchProfileApplication {
                runtime: false,
                tui: true,
            }
            .scope(),
            Some("tui")
        );
        assert_eq!(
            LaunchProfileApplication {
                runtime: true,
                tui: true,
            }
            .scope(),
            Some("runtime_and_tui")
        );
    }

    #[test]
    fn one_launch_profile_receipt_is_explicitly_non_persistent() {
        assert_eq!(
            LaunchProfileApplication {
                runtime: true,
                tui: false,
            }
            .receipt("profile-id", "beta", "one_launch_override"),
            Some(json!({
                "requested": "profile-id",
                "selected": "beta",
                "effective": "beta",
                "source": "one_launch_override",
                "applicationScope": "runtime",
                "persisted": false,
            }))
        );
        assert!(LaunchProfileApplication::default()
            .receipt("beta", "beta", "one_launch_override")
            .is_none());
    }

    #[test]
    fn legacy_tui_only_receipt_keeps_unknown_provenance() {
        assert_eq!(
            LaunchProfileApplication {
                runtime: false,
                tui: true,
            }
            .receipt("legacy-profile", "legacy-profile", "unknown"),
            Some(json!({
                "requested": "legacy-profile",
                "selected": "legacy-profile",
                "effective": "legacy-profile",
                "source": "unknown",
                "applicationScope": "tui",
                "persisted": false,
            }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn production_runtime_handler_constructs_runtime_tui_and_combined_profile_receipts() {
        let _fixture = ProductionHandlerFixture::new();
        let alpha = profile("alpha");
        let beta = profile("beta");
        save_resolvable_profile(&alpha);
        save_resolvable_profile(&beta);
        save_store(&AccountsStore {
            version: 3,
            accounts: vec![alpha, beta],
            active_account_id: Some("beta-id".to_string()),
        })
        .expect("save fixture profile store");
        let mut config = cutex::config::store::load_codez_config();
        config.default_profile = Some("beta".to_string());
        save_codez_config(&config).expect("set fixture global default");

        save_runtime_session(
            "cutex.runtime.session-profile",
            Some("alpha"),
            CutexSessionRuntimeBackend::Host,
        );
        let session_profile = mutate_management_v2_runtime(
            "cutex.runtime.session-profile",
            "cutex/runtime/online",
            &json!({ "expectedRuntimeGeneration": 0, "openVisibleTerminal": false }),
        )
        .expect("start session-configured runtime through production handler");
        assert_production_receipt(&session_profile, "alpha", "session_configured", "runtime");
        stop_production_runtime("cutex.runtime.session-profile", 1);

        save_runtime_session(
            "cutex.runtime.global-profile",
            None,
            CutexSessionRuntimeBackend::Host,
        );
        let global_profile = mutate_management_v2_runtime(
            "cutex.runtime.global-profile",
            "cutex/runtime/online",
            &json!({ "expectedRuntimeGeneration": 0, "openVisibleTerminal": false }),
        )
        .expect("start global-default runtime through production handler");
        assert_production_receipt(&global_profile, "beta", "global_default", "runtime");
        stop_production_runtime("cutex.runtime.global-profile", 1);

        save_runtime_session(
            "cutex.runtime.combined",
            Some("alpha"),
            CutexSessionRuntimeBackend::CuteAlden,
        );
        let combined = mutate_management_v2_session(
            "cutex.runtime.combined",
            "cutex/runtime/online",
            json!({ "expectedRuntimeGeneration": 0, "openVisibleTerminal": false }),
        )
        .expect("start combined runtime and TUI through production session handler");
        assert_production_receipt(&combined, "alpha", "session_configured", "runtime_and_tui");
        stop_production_runtime("cutex.runtime.combined", 1);

        save_runtime_session(
            "cutex.runtime.legacy-tui",
            Some("alpha"),
            CutexSessionRuntimeBackend::Host,
        );
        let runtime = mutate_management_v2_runtime(
            "cutex.runtime.legacy-tui",
            "cutex/runtime/online",
            &json!({ "expectedRuntimeGeneration": 0, "openVisibleTerminal": false }),
        )
        .expect("start fixture runtime before TUI-only restoration");
        assert_production_receipt(&runtime, "alpha", "session_configured", "runtime");
        let mut store = load_cutex_session_store().expect("load fixture runtime binding");
        let record = store
            .sessions
            .get_mut("cutex.runtime.legacy-tui")
            .expect("fixture session exists");
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record
            .app_server_runtime
            .as_mut()
            .expect("fixture runtime binding exists")
            .launch_profile_source = None;
        save_cutex_session_store(&store).expect("save legacy TUI fixture");
        let legacy_tui = mutate_management_v2_session(
            "cutex.runtime.legacy-tui",
            "cutex/runtime/online",
            json!({ "expectedRuntimeGeneration": 1, "openVisibleTerminal": false }),
        )
        .expect("restore legacy TUI through production session handler");
        assert_production_receipt(&legacy_tui, "alpha", "unknown", "tui");
        stop_production_runtime("cutex.runtime.legacy-tui", 1);
    }

    #[cfg(unix)]
    #[test]
    fn production_one_launch_override_leaves_durable_configured_intent_unchanged() {
        let _fixture = ProductionHandlerFixture::new();
        let alpha = profile("alpha");
        let beta = profile("beta");
        save_resolvable_profile(&alpha);
        save_resolvable_profile(&beta);
        save_store(&AccountsStore {
            version: 3,
            accounts: vec![alpha, beta],
            active_account_id: Some("beta-id".to_string()),
        })
        .expect("save fixture profile store");
        save_runtime_session(
            "cutex.runtime.one-launch",
            Some("beta"),
            CutexSessionRuntimeBackend::Host,
        );
        let before = load_cutex_session_store()
            .expect("load configured intent")
            .sessions["cutex.runtime.one-launch"]
            .clone();
        let one_launch = mutate_management_v2_runtime(
            "cutex.runtime.one-launch",
            "cutex/runtime/online",
            &json!({
                "expectedRuntimeGeneration": 0,
                "openVisibleTerminal": false,
                "launchProfile": "alpha"
            }),
        )
        .expect("apply one-launch override through production handler");
        assert_production_receipt(&one_launch, "alpha", "one_launch_override", "runtime");
        let after = load_cutex_session_store()
            .expect("reload durable configured intent")
            .sessions["cutex.runtime.one-launch"]
            .clone();
        assert_eq!(after.profile, before.profile);
        assert_eq!(after.durable_revision(), before.durable_revision());
        stop_production_runtime("cutex.runtime.one-launch", 1);
    }

    #[test]
    fn management_profile_handler_uses_durable_store_and_reports_im_write_uncertainty() {
        let _home = IsolatedTestHome::new("cmp").expect("create isolated HOME");
        let id = "cutex.management-profile";
        let account = profile("alpha");
        save_resolvable_profile(&account);
        save_local_session(id);
        let initial_revision =
            load_cutex_session_store().expect("load session").sessions[id].durable_revision();

        let set = mutate_management_v2_session(
            id,
            "cutex/session/profile/set",
            json!({ "expectedRevision": initial_revision, "profile": "alpha" }),
        )
        .expect("set configured profile");
        assert_eq!(set["configuredProfile"], "alpha");
        let configured = load_cutex_session_store()
            .expect("load configured session")
            .sessions[id]
            .clone();
        assert_eq!(configured.profile.as_deref(), Some("alpha"));
        assert_eq!(
            load_im_registry()
                .expect("load mirrored IM record")
                .sessions["thread-management-profile"]
                .profile
                .as_deref(),
            Some("alpha")
        );

        let stale = mutate_management_v2_session(
            id,
            "cutex/session/profile/clear",
            json!({ "expectedRevision": initial_revision }),
        )
        .expect_err("stale revision must be rejected");
        assert_eq!(stale.code, "revision_conflict");
        assert_eq!(stale.details["resyncRequired"], true);
        assert_eq!(
            load_cutex_session_store()
                .expect("reload after stale request")
                .sessions[id]
                .profile,
            Some("alpha".to_string())
        );

        let im_registry_path = config_dir().expect("config dir").join("im-sessions.json");
        fs::remove_file(&im_registry_path).expect("remove IM registry for failure fixture");
        fs::create_dir(&im_registry_path).expect("replace IM registry with failure fixture");
        let uncertain = mutate_management_v2_session(
            id,
            "cutex/session/profile/clear",
            json!({ "expectedRevision": configured.durable_revision() }),
        )
        .expect_err("IM persistence failure must surface as outcome unknown");
        assert_eq!(uncertain.code, "persistence_uncertain");
        assert!(!uncertain.retryable);
        assert!(uncertain.outcome_unknown);
        assert_eq!(uncertain.details["resyncRequired"], true);
        assert_eq!(
            load_cutex_session_store()
                .expect("durable store persists before IM failure")
                .sessions[id]
                .profile,
            None
        );
        fs::remove_dir(im_registry_path).expect("remove IM failure fixture");
    }

    #[test]
    fn failed_same_binding_recovery_returns_typed_bounded_cutover() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            None,
            "2026-08-07T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.runtime_generation = 7;

        let error = runtime_cutover_required(&record, "same_binding_reconnect_failed", Some(4242));

        assert_eq!(error.stage, "runtime");
        assert_eq!(error.code, "cutover_required");
        assert!(!error.retryable);
        assert!(!error.outcome_unknown);
        assert_eq!(error.details["runtimeGeneration"], 7);
        assert_eq!(error.details["runtimePid"], 4242);
        assert_eq!(error.details["recoveryAttempts"], 1);
        assert_eq!(error.details["requiredAction"], "cutex/runtime/offline");
    }

    #[test]
    fn windows_terminal_runtime_actions_use_force_termination() {
        assert!(runtime_stop_force_policy(
            "cutex/runtime/offline",
            false,
            true
        ));
        assert!(runtime_stop_force_policy(
            "cutex/runtime/close",
            false,
            true
        ));
        assert!(!runtime_stop_force_policy(
            "cutex/runtime/close",
            false,
            false
        ));
        assert!(runtime_stop_force_policy(
            "cutex/runtime/close",
            true,
            false
        ));
    }
}
