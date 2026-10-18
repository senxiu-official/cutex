use std::process::Child;
use std::process::ExitStatus;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
use std::ffi::OsStr;
#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
use std::fs;
#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use chrono::Utc;
use uuid::Uuid;

use super::account_store::load_store;
use super::agent_bus_runtime;
use super::app_server_runtime;
use super::launch::ResolvedLaunchProfile;
use super::launch_command;
use super::session_reconcile;

use cutex::agent_bus::client::agent_bus_fetch_agents;
use cutex::agent_bus::client::agent_bus_fetch_agents_if_healthy;
use cutex::agent_bus::model::AgentBusAgent;
use cutex::agent_bus::store::agent_endpoint_is_usable_for_this_host;
use cutex::app_server::runtime::cleanup_runtime_binding_files;
use cutex::app_server::runtime::AppServerRuntimeLayout;
use cutex::config::env::CUTEX_AGENT_BUS_TOKEN_ENV_VAR;
use cutex::config::env::CUTEX_AGENT_BUS_URL_ENV_VAR;
use cutex::config::env::CUTEX_AGENT_GROUPS_ENV_VAR;
use cutex::config::env::CUTEX_AGENT_HINT_ENV_VAR;
use cutex::config::env::CUTEX_AGENT_HOST_ID_ENV_VAR;
use cutex::config::env::CUTEX_AGENT_ID_ENV_VAR;
use cutex::config::env::CUTEX_AGENT_NAME_ENV_VAR;
use cutex::config::env::CUTEX_RUNTIME_HEARTBEAT_TOKEN_ENV_VAR;
use cutex::config::env::CUTEX_RUNTIME_HEARTBEAT_URL_ENV_VAR;
use cutex::config::env::CUTEX_RUNTIME_LAUNCH_ID_ENV_VAR;
#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
use cutex::config::paths::config_dir;
use cutex::config::store::load_codez_config;
use cutex::im::registry::CodingSessionRegistration;
use cutex::platform::host::current_host_name;
use cutex::platform::process::{process_is_running, terminate_process_and_wait};
use cutex::profiles::inspect::account_model_provider;
use cutex::profiles::lookup::find_account;
use cutex::profiles::model::{CodezConfig, MaterializedAccountFiles, StoredAccount};
use cutex::runtime::alden::{
    cute_alden_program, find_cute_alden_session_by_name, wrap_launch_with_cute_alden_server_only,
};
use cutex::runtime::lifecycle::{
    cutex_session_host_is_local, live_remote_tui_attach_plan, session_online_log_path,
    session_online_log_tail, session_online_resume_plan, session_runtime_stop_target,
    spawn_detached_session_launch, spawn_managed_agent_runtime_launch,
};
use cutex::runtime::lifecycle::{
    default_cutex_alden_session_name, session_online_agent_id,
    session_online_agent_identity_env_with_id, session_online_base_codex_args,
    session_online_terminal_color_env,
};
#[cfg(test)]
use cutex::runtime::lifecycle::{finalize_session_online_launch, SessionOnlineLaunch};
use cutex::runtime::process_scope::terminate_managed_agent_scope;
use cutex::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, LaunchProfileSource};
use cutex::session::service::{
    apply_session_online_runtime_observation, clear_cutex_session_runtime_record,
    cutex_session_key_for_user_id, cutex_session_launch_cwd,
    persist_cutex_session_store_and_im_record,
};
use cutex::session::store::load_cutex_session_store;
use cutex::session::store::CutexSessionStoreRevisionConflict;

static SESSION_ONLINE_START_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const RUNTIME_STOP_CAS_ATTEMPTS: usize = 3;

pub(crate) fn ensure_managed_tui_peer_with_profile(
    entry: &CodingSessionRegistration,
    launch_profile: Option<&ResolvedLaunchProfile>,
) -> anyhow::Result<bool> {
    let store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, &entry.session_id)
        .ok_or_else(|| anyhow!("cutex session is not known: {}", entry.session_id))?;
    let record = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared while restoring TUI peer: {key}"))?;
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    if record.runtime_backend != CutexSessionRuntimeBackend::CuteAlden {
        return Ok(false);
    }
    let mut binding = record
        .app_server_runtime
        .clone()
        .context("managed app-server binding is missing")?;
    ensure_cutex_session_runtime_host_is_local(&record)?;
    if !process_is_running(binding.pid) {
        anyhow::bail!(
            "manager-owned app-server process is not running: {}",
            binding.pid
        );
    }
    let layout = AppServerRuntimeLayout::from_binding(&binding)?;
    if !layout.endpoint_ready() {
        anyhow::bail!(
            "manager-owned app-server endpoint is unavailable: {}",
            binding.endpoint
        );
    }
    let session_name = record
        .alden_session_name
        .clone()
        .unwrap_or_else(|| default_cutex_alden_session_name(&record));
    if let Some(existing) = find_cute_alden_session_by_name(&session_name) {
        persist_managed_tui_peer(&key, &binding, &session_name, existing.pid)?;
        return Ok(false);
    }

    let occurrence_profile = if launch_profile.is_none() {
        let live_agents = if binding.launched_profile.is_none() {
            let config = load_codez_config();
            live_agents_for_management_entry(&config, entry)
        } else {
            Vec::new()
        };
        runtime_occurrence_profile_name(&record, &binding, &live_agents)
    } else {
        None
    };
    if binding.launched_profile.is_none() {
        binding.launched_profile = occurrence_profile.clone();
    }
    let default_account = if launch_profile.is_none() {
        match occurrence_profile.as_deref() {
            Some(profile) => Some(account_for_runtime_occurrence(profile)?),
            None => {
                // A legacy binding has no trustworthy launch-profile evidence.
                // Do not relabel its live core from the current global default
                // merely to recreate a visible peer.
                return Ok(false);
            }
        }
    } else {
        None
    };
    let account = launch_profile
        .map(|profile| &profile.account)
        .or(default_account.as_ref())
        .expect("launch profile or durable session profile");
    let prevalidated_files = launch_profile.map(|profile| &profile.files);
    let log_path = session_online_log_path(&record)?;
    let launch_cwd = cutex_session_launch_cwd(&record).to_string();
    let mut started = start_remote_tui(
        &record,
        account,
        &layout,
        &launch_cwd,
        &log_path,
        prevalidated_files,
    )?;
    if let Err(error) = persist_managed_tui_peer(&key, &binding, &started.session_name, started.pid)
    {
        let cleanup = (|| -> anyhow::Result<()> {
            let outcome = terminate_process_and_wait(started.pid, true)?;
            if !outcome.stopped {
                anyhow::bail!(
                    "restored remote TUI process {} did not stop: {}",
                    started.pid,
                    outcome.detail
                );
            }
            stop_startup_child(&mut started.child)
        })();
        return Err(match cleanup {
            Ok(()) => error.context("failed to persist restored remote TUI peer"),
            Err(cleanup_error) => error.context(format!(
                "failed to persist restored remote TUI peer; cleanup also failed: {cleanup_error:#}"
            )),
        });
    }
    spawn_detached_child_reaper(
        started.child,
        format!("remote TUI {}", record.cutex_session_id),
    );
    Ok(true)
}

fn persist_managed_tui_peer(
    key: &str,
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    session_name: &str,
    alden_pid: u32,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let record = store
        .sessions
        .get_mut(key)
        .ok_or_else(|| anyhow!("cutex session disappeared while recording TUI peer: {key}"))?;
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    let current_binding = record
        .app_server_runtime
        .as_mut()
        .context("managed app-server binding disappeared while recording TUI peer")?;
    if current_binding.pid != binding.pid
        || current_binding.endpoint != binding.endpoint
        || current_binding.started_at != binding.started_at
    {
        anyhow::bail!("managed app-server binding changed while recording TUI peer");
    }
    if current_binding.launched_profile.is_none() {
        current_binding.launched_profile = binding.launched_profile.clone();
    }
    record.alden_session_name = Some(session_name.to_string());
    record.alden_pid = Some(alden_pid);
    record.runtime_pid = Some(binding.pid);
    record.pending_launch_id = None;
    record.app_server_launch_claim_id = None;
    record.updated_at = Utc::now().to_rfc3339();
    persist_cutex_session_store_and_im_record(&store, key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionOnlineStartOutcome {
    pub(crate) runtime_launched: bool,
    pub(crate) tui_launched: bool,
}

type RuntimeOccurrenceValidator<'a> = dyn Fn(&CutexSessionRecord) -> anyhow::Result<()> + 'a;

pub(crate) fn start_cutex_session_online_with_profile(
    config: &CodezConfig,
    entry: &CodingSessionRegistration,
    launch_profile: Option<&ResolvedLaunchProfile>,
) -> anyhow::Result<SessionOnlineStartOutcome> {
    start_cutex_session_online_with_profile_inner(config, entry, launch_profile, None)
}

pub(crate) fn start_cutex_session_online_with_profile_if(
    config: &CodezConfig,
    entry: &CodingSessionRegistration,
    launch_profile: Option<&ResolvedLaunchProfile>,
    validate_occurrence: &RuntimeOccurrenceValidator<'_>,
) -> anyhow::Result<SessionOnlineStartOutcome> {
    start_cutex_session_online_with_profile_inner(
        config,
        entry,
        launch_profile,
        Some(validate_occurrence),
    )
}

fn start_cutex_session_online_with_profile_inner(
    config: &CodezConfig,
    entry: &CodingSessionRegistration,
    launch_profile: Option<&ResolvedLaunchProfile>,
    validate_occurrence: Option<&RuntimeOccurrenceValidator<'_>>,
) -> anyhow::Result<SessionOnlineStartOutcome> {
    let _start_guard = SESSION_ONLINE_START_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow!("session online start lock was poisoned"))?;
    let mut store = load_cutex_session_store()?;
    let key = cutex_session_key_for_user_id(&store, &entry.session_id)
        .ok_or_else(|| anyhow!("cutex session is not known: {}", entry.session_id))?;
    let record = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared while starting: {key}"))?;
    cutex::agent_management::require_migrated_runtime(&record)?;
    if validate_occurrence.is_none() { ensure_agent_bus_for_launch_record()?; }
    if record.is_retired() {
        anyhow::bail!("cutex session is retired: {key}");
    }
    if let Some(validate) = validate_occurrence {
        validate(&record)?;
    }
    ensure_runtime_claim_is_clear(&record)?;
    let next_runtime_generation = record
        .runtime_generation
        .checked_add(1)
        .filter(|generation| *generation <= cutex::management::v2::model::MAX_SAFE_SEQUENCE)
        .context("runtime generation exhausted the JSON-safe integer range")?;
    let mut launch_record = record.clone();
    launch_record.runtime_generation = next_runtime_generation;
    ensure_cutex_session_runtime_host_is_local(&record)?;
    let default_account = if launch_profile.is_none() {
        Some(account_for_cutex_session(&record)?)
    } else {
        None
    };
    let account = launch_profile
        .map(|profile| &profile.account)
        .or(default_account.as_ref())
        .expect("launch profile or durable session profile");
    let launch_profile_source = if launch_profile.is_some() {
        LaunchProfileSource::OneLaunchOverride
    } else if record
        .profile
        .as_deref()
        .is_some_and(|profile| !profile.trim().is_empty())
    {
        LaunchProfileSource::SessionConfigured
    } else {
        LaunchProfileSource::GlobalDefault
    };
    let prevalidated_files = launch_profile.map(|profile| &profile.files);
    if !runtime_backend_uses_managed_app_server(record.runtime_backend) {
        anyhow::bail!(
            "session.online does not support runtime_backend={:?}",
            record.runtime_backend
        );
    }
    let resume_plan = session_online_resume_plan(&record, account)?;
    let layout = AppServerRuntimeLayout::prepare(&record.cutex_session_id)?;
    let runtime_agent_id = session_online_agent_id(account, &record);
    let app_server_launch = match app_server_launch_command_with_profile(
        &record,
        account,
        &resume_plan.groups,
        &layout,
        &runtime_agent_id,
        prevalidated_files,
    ) {
        Ok(launch) => launch,
        Err(error) => {
            return Err(match layout.cleanup_files() {
                Ok(()) => error,
                Err(cleanup_error) => error.context(format!(
                    "failed to remove app-server runtime files after launch preparation failed: {cleanup_error:#}"
                )),
            });
        }
    };
    let log_path = match session_online_log_path(&record) {
        Ok(path) => path,
        Err(error) => {
            return Err(match layout.cleanup_files() {
                Ok(()) => error,
                Err(cleanup_error) => error.context(format!(
                    "failed to remove app-server runtime files after log-path preparation failed: {cleanup_error:#}"
                )),
            });
        }
    };

    // Publish an ownership claim before creating the child. A crash or
    // process handoff in this window must remain "owner unknown" rather than
    // looking like a clean offline record to the next online request.
    let app_server_launch_claim_id = Uuid::new_v4().to_string();
    let claim_result = match validate_occurrence {
        Some(validate) => commit_fenced_online_claim_using(
            &mut store,
            &key,
            &record,
            &app_server_launch_claim_id,
            validate,
            load_cutex_session_store,
            persist_cutex_session_store_and_im_record,
        ),
        None => {
            let mut pending_record = record.clone();
            pending_record.app_server_launch_claim_id = Some(app_server_launch_claim_id.clone());
            pending_record.updated_at = Utc::now().to_rfc3339();
            store.sessions.insert(key.clone(), pending_record);
            persist_cutex_session_store_and_im_record(&store, &key)
        }
    };
    if let Err(error) = claim_result {
        let cleanup = layout.cleanup_files();
        let rollback = rollback_after_cleanup(&cleanup, || {
            restore_app_server_launch_claim(&key, &record, &app_server_launch_claim_id)
        });
        return Err(error_with_cleanup_and_rollback(
            error,
            cleanup,
            rollback,
            "pending ownership claim",
        ));
    }

    if validate_occurrence.is_some() {
        if let Err(error) = ensure_agent_bus_for_launch_record() {
            let cleanup = layout.cleanup_files();
            let rollback = rollback_after_cleanup(&cleanup, || {
                restore_app_server_launch_claim(&key, &record, &app_server_launch_claim_id)
            });
            return Err(error_with_cleanup_and_rollback(
                error,
                cleanup,
                rollback,
                "agent bus prelaunch",
            ));
        }
    }

    let mut app_server_child = match spawn_managed_agent_runtime_launch(
        &record.cutex_session_id,
        &app_server_launch,
        &resume_plan.launch_cwd,
        &log_path,
    ) {
        Ok(child) => child,
        Err(error) => {
            let cleanup = layout.cleanup_files();
            let rollback = rollback_after_cleanup(&cleanup, || {
                restore_app_server_launch_claim(&key, &record, &app_server_launch_claim_id)
            });
            return Err(error_with_cleanup_and_rollback(
                error,
                cleanup,
                rollback,
                "child spawn",
            ));
        }
    };
    let mut binding = layout.binding(app_server_child.id(), Utc::now().to_rfc3339());
    binding.launched_profile = Some(account.name.clone());
    binding.launched_profile_id = Some(account.id.clone());
    binding.launch_profile_source = Some(launch_profile_source);
    let mut remote_tui_child = None;
    let mut alden_session_name = None;
    let mut alden_pid = None;

    let start_result = (|| -> anyhow::Result<AgentBusAgent> {
        wait_for_app_server_endpoint(&layout, &mut app_server_child, &log_path)?;
        app_server_runtime::connect_runtime_with_model_provider(
            config,
            &launch_record,
            &binding,
            &runtime_agent_id,
            account_model_provider(account).as_deref(),
        )
        .context("failed to connect the managed app-server runtime")?;

        if record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden {
            let started = start_remote_tui(
                &record,
                account,
                &layout,
                &resume_plan.launch_cwd,
                &log_path,
                prevalidated_files,
            )?;
            alden_session_name = Some(started.session_name);
            alden_pid = Some(started.pid);
            remote_tui_child = Some(started.child);
        }

        wait_for_runtime_agent(config, &runtime_agent_id).ok_or_else(|| {
            anyhow!(
                "app-server runtime did not register on the agent bus; log tail: {}",
                session_online_log_tail(&log_path)
            )
        })
    })();
    let final_live_agent = match start_result {
        Ok(agent) => agent,
        Err(error) => {
            let cleanup = cleanup_failed_app_server_start(
                &record.cutex_session_id,
                &binding,
                &mut app_server_child,
                remote_tui_child.as_mut(),
                alden_session_name.as_deref(),
            );
            let rollback = rollback_after_cleanup(&cleanup, || {
                restore_prelaunch_record_after_failed_start(
                    &key,
                    &record,
                    &binding,
                    &runtime_agent_id,
                    alden_pid,
                    &app_server_launch_claim_id,
                )
            });
            return Err(error_with_cleanup_and_rollback(
                error, cleanup, rollback, "launch",
            ));
        }
    };

    let timestamp = Utc::now().to_rfc3339();
    let persistence_result = (|| {
        // Runtime registration is expected to update the durable store while
        // launch waits on the Agent Bus. Merge that fenced update instead of
        // writing the pre-registration snapshot back over it.
        let mut store = load_cutex_session_store()?;
        let current = store.sessions.get(&key).ok_or_else(|| {
            anyhow!("cutex session disappeared while finalizing app-server: {key}")
        })?;
        if !runtime_claim_belongs_to_started_attempt(
            current,
            &record,
            &binding,
            &runtime_agent_id,
            alden_pid,
            &app_server_launch_claim_id,
        ) {
            anyhow::bail!(
                "runtime ownership changed concurrently; app-server launch was not finalized"
            );
        }
        let observed_pid = alden_pid.unwrap_or(binding.pid);
        let reconcile_outcome = apply_session_online_runtime_observation(
            &mut store,
            &key,
            Some(&final_live_agent),
            alden_session_name.as_deref(),
            record.runtime_backend,
            observed_pid,
            &current_host_name(),
            &timestamp,
        )?;
        let stored_record = store.sessions.get_mut(&key).ok_or_else(|| {
            anyhow!("cutex session disappeared while recording app-server: {key}")
        })?;
        stored_record.app_server_runtime = Some(binding.clone());
        stored_record.pending_launch_id = None;
        stored_record.app_server_launch_claim_id = None;
        stored_record.runtime_pid = Some(binding.pid);
        stored_record.current_runtime_agent_id = Some(runtime_agent_id.clone());
        stored_record.alden_session_name = alden_session_name.clone();
        stored_record.alden_pid = alden_pid;
        stored_record.updated_at = timestamp.clone();
        persist_cutex_session_store_and_im_record(&store, &key)?;
        anyhow::Ok(reconcile_outcome)
    })();
    let reconcile_outcome = match persistence_result {
        Ok(outcome) => outcome,
        Err(error) => {
            let cleanup = cleanup_failed_app_server_start(
                &record.cutex_session_id,
                &binding,
                &mut app_server_child,
                remote_tui_child.as_mut(),
                alden_session_name.as_deref(),
            );
            let rollback = rollback_after_cleanup(&cleanup, || {
                restore_prelaunch_record_after_failed_start(
                    &key,
                    &record,
                    &binding,
                    &runtime_agent_id,
                    alden_pid,
                    &app_server_launch_claim_id,
                )
            });
            return Err(error_with_cleanup_and_rollback(
                error,
                cleanup,
                rollback,
                "persistence",
            ));
        }
    };
    if let Some(outcome) = reconcile_outcome {
        if let Err(error) = session_reconcile::append_cutex_session_reconcile_events(
            &outcome,
            &final_live_agent,
            &timestamp,
        ) {
            eprintln!("failed to append management v2 runtime endpoint event: {error:#}");
        }
    }
    spawn_detached_child_reaper(
        app_server_child,
        format!("app-server {}", record.cutex_session_id),
    );
    if let Some(child) = remote_tui_child {
        spawn_detached_child_reaper(child, format!("remote TUI {}", record.cutex_session_id));
    }

    Ok(SessionOnlineStartOutcome {
        runtime_launched: true,
        tui_launched: record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden,
    })
}

fn commit_fenced_online_claim_using(
    store: &mut cutex::session::model::CutexSessionStore,
    key: &str,
    record: &CutexSessionRecord,
    launch_claim_id: &str,
    validate_occurrence: &RuntimeOccurrenceValidator<'_>,
    reload: impl FnOnce() -> anyhow::Result<cutex::session::model::CutexSessionStore>,
    mut persist: impl FnMut(&cutex::session::model::CutexSessionStore, &str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // The external occurrence recheck and the ownership-claim write share the
    // durable store revision CAS. A newer registration either appears in this
    // recheck or advances the revision before persist; both outcomes fence
    // before a child, bridge, registration, or runtime claim can be created.
    let current_store = reload()?;
    if current_store.store_revision.get() != store.store_revision.get() {
        anyhow::bail!("runtime occurrence changed before the fenced launch claim");
    }
    let current = current_store
        .sessions
        .get(key)
        .ok_or_else(|| anyhow!("cutex session disappeared before fenced launch: {key}"))?;
    validate_occurrence(current)?;
    let mut pending_record = record.clone();
    pending_record.app_server_launch_claim_id = Some(launch_claim_id.to_string());
    pending_record.updated_at = Utc::now().to_rfc3339();
    store.sessions.insert(key.to_string(), pending_record);
    persist(store, key)
}

pub(crate) fn runtime_backend_uses_managed_app_server(backend: CutexSessionRuntimeBackend) -> bool {
    matches!(
        backend,
        CutexSessionRuntimeBackend::Host
            | CutexSessionRuntimeBackend::HostForeground
            | CutexSessionRuntimeBackend::CuteAlden
    )
}

pub(super) fn spawn_detached_child_reaper(mut child: Child, label: String) {
    let thread_name = format!("cutex-child-reaper-{}", child.id());
    let _ = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            if let Err(error) = child.wait() {
                eprintln!("failed to reap {label}: {error}");
            }
        });
}

#[cfg(test)]
pub(crate) fn app_server_launch_command(
    record: &CutexSessionRecord,
    account: &StoredAccount,
    groups: &[String],
    layout: &AppServerRuntimeLayout,
    runtime_agent_id: &str,
) -> anyhow::Result<cutex::launch::command::LaunchCommand> {
    app_server_launch_command_with_profile(record, account, groups, layout, runtime_agent_id, None)
}

fn app_server_launch_command_with_profile(
    record: &CutexSessionRecord,
    account: &StoredAccount,
    groups: &[String],
    layout: &AppServerRuntimeLayout,
    runtime_agent_id: &str,
    prevalidated_files: Option<&MaterializedAccountFiles>,
) -> anyhow::Result<cutex::launch::command::LaunchCommand> {
    let mut args = session_online_base_codex_args(record, account)?;
    args.extend(layout.app_server_args());
    let launch = launch_command_for_profile(account, &args, true, groups, prevalidated_files)?;
    let launch = session_online_agent_identity_env_with_id(
        session_online_terminal_color_env(launch),
        record,
        groups,
        runtime_agent_id,
    );
    Ok(strip_legacy_runtime_env(launch))
}

#[cfg(test)]
pub(crate) fn remote_tui_launch_command(
    record: &CutexSessionRecord,
    account: &StoredAccount,
    layout: &AppServerRuntimeLayout,
) -> anyhow::Result<cutex::launch::command::LaunchCommand> {
    remote_tui_launch_command_with_profile(record, account, layout, None)
}

pub(crate) fn remote_tui_launch_command_with_profile(
    record: &CutexSessionRecord,
    account: &StoredAccount,
    layout: &AppServerRuntimeLayout,
    prevalidated_files: Option<&MaterializedAccountFiles>,
) -> anyhow::Result<cutex::launch::command::LaunchCommand> {
    let mut args = live_remote_tui_attach_plan(record, account)?.effective_args;
    args.extend(layout.remote_tui_args());
    let launch = launch_command_for_profile(account, &args, false, &[], prevalidated_files)?;
    let mut launch = launch;
    launch.args = cutex::runtime::lifecycle::remote_attach_args(launch.args);
    let launch = layout.apply_remote_tui_auth(session_online_terminal_color_env(launch));
    Ok(strip_tui_runtime_env(strip_legacy_runtime_env(launch)))
}

fn launch_command_for_profile(
    account: &StoredAccount,
    args: &[String],
    agent_mode: bool,
    groups: &[String],
    prevalidated_files: Option<&MaterializedAccountFiles>,
) -> anyhow::Result<cutex::launch::command::LaunchCommand> {
    match prevalidated_files {
        Some(files) => launch_command::codex_launch_command_with_prevalidated_profile(
            account, args, agent_mode, groups, files,
        ),
        None => {
            launch_command::codex_launch_command_with_agent_mode(account, args, agent_mode, groups)
        }
    }
}

fn strip_legacy_runtime_env(
    mut launch: cutex::launch::command::LaunchCommand,
) -> cutex::launch::command::LaunchCommand {
    for key in [
        "CUTEX_OBSERVER_URL",
        "CUTEX_OBSERVER_TOKEN",
        CUTEX_RUNTIME_HEARTBEAT_URL_ENV_VAR,
        CUTEX_RUNTIME_HEARTBEAT_TOKEN_ENV_VAR,
        CUTEX_RUNTIME_LAUNCH_ID_ENV_VAR,
    ] {
        launch = launch.env_unset(key);
    }
    launch
}

fn strip_tui_runtime_env(
    mut launch: cutex::launch::command::LaunchCommand,
) -> cutex::launch::command::LaunchCommand {
    for key in [
        CUTEX_AGENT_BUS_URL_ENV_VAR,
        CUTEX_AGENT_BUS_TOKEN_ENV_VAR,
        CUTEX_AGENT_ID_ENV_VAR,
        CUTEX_AGENT_NAME_ENV_VAR,
        CUTEX_AGENT_GROUPS_ENV_VAR,
        CUTEX_AGENT_HOST_ID_ENV_VAR,
        CUTEX_AGENT_HINT_ENV_VAR,
        "CODEX_THREAD_ID",
    ] {
        launch = launch.env_unset(key);
    }
    launch
}

pub(super) fn wait_for_app_server_endpoint(
    layout: &AppServerRuntimeLayout,
    child: &mut Child,
    log_path: &std::path::Path,
) -> anyhow::Result<()> {
    for _ in 0..50 {
        if layout.endpoint_ready() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect app-server process")?
        {
            anyhow::bail!(
                "app-server exited early with status {status}; log tail: {}",
                session_online_log_tail(log_path)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!(
        "app-server endpoint did not become ready; log tail: {}",
        session_online_log_tail(log_path)
    )
}

struct StartedRemoteTui {
    session_name: String,
    pid: u32,
    child: Child,
}

fn remote_tui_launcher_failed(status: &ExitStatus) -> bool {
    !status.success()
}

fn start_remote_tui(
    record: &CutexSessionRecord,
    account: &StoredAccount,
    layout: &AppServerRuntimeLayout,
    cwd: &str,
    log_path: &std::path::Path,
    prevalidated_files: Option<&MaterializedAccountFiles>,
) -> anyhow::Result<StartedRemoteTui> {
    let session_name = record
        .alden_session_name
        .clone()
        .unwrap_or_else(|| default_cutex_alden_session_name(record));
    if find_cute_alden_session_by_name(&session_name).is_some() {
        anyhow::bail!("cute-alden session already exists: {session_name}");
    }
    let launch =
        remote_tui_launch_command_with_profile(record, account, layout, prevalidated_files)?;
    let launch =
        wrap_launch_with_cute_alden_server_only(launch, &cute_alden_program()?, &session_name, cwd);
    let mut child = spawn_detached_session_launch(&launch, cwd, log_path)?;
    for _ in 0..20 {
        if let Some(session) = find_cute_alden_session_by_name(&session_name) {
            return Ok(StartedRemoteTui {
                session_name,
                pid: session.pid,
                child,
            });
        }
        match child.try_wait() {
            Ok(Some(status)) if remote_tui_launcher_failed(&status) => {
                anyhow::bail!(
                    "cute-alden remote TUI exited before registration with status {status}; log tail: {}",
                    session_online_log_tail(log_path)
                );
            }
            // `cute-alden --server-only` may successfully hand the server off
            // and exit before the named session reaches the registry.
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => {
                if let Err(cleanup_error) = stop_startup_child(&mut child) {
                    return Err(anyhow!(error).context(format!(
                        "failed to inspect cute-alden launch; cleanup also failed: {cleanup_error:#}"
                    )));
                }
                return Err(error).context("failed to inspect cute-alden launch");
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let cleanup_error = stop_startup_child(&mut child).err();
    if let Some(cleanup_error) = cleanup_error {
        anyhow::bail!(
            "cute-alden remote TUI did not register and cleanup failed: {cleanup_error:#}; log tail: {}",
            session_online_log_tail(log_path)
        );
    }
    anyhow::bail!(
        "cute-alden remote TUI did not register; log tail: {}",
        session_online_log_tail(log_path)
    )
}

fn wait_for_runtime_agent(config: &CodezConfig, runtime_agent_id: &str) -> Option<AgentBusAgent> {
    for _ in 0..20 {
        if let Some(agent) = agent_bus_fetch_agents_if_healthy(config)
            .into_iter()
            .find(|agent| agent.id == runtime_agent_id)
        {
            return Some(agent);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn ensure_runtime_claim_is_clear(record: &CutexSessionRecord) -> anyhow::Result<()> {
    let runtime_agent_id_present = record
        .current_runtime_agent_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let app_server_launch_claim_present = record
        .app_server_launch_claim_id
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if record.app_server_runtime.is_some()
        || runtime_agent_id_present
        || record.runtime_pid.is_some()
        || record.alden_pid.is_some()
        || app_server_launch_claim_present
    {
        anyhow::bail!(
            "runtime ownership claim must be recovered, cleared, or fenced before a new generation is launched"
        );
    }
    Ok(())
}

fn restore_prelaunch_record_after_failed_start(
    key: &str,
    before: &CutexSessionRecord,
    failed_binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    failed_runtime_agent_id: &str,
    failed_alden_pid: Option<u32>,
    failed_app_server_launch_claim_id: &str,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let current = store
        .sessions
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared during launch rollback: {key}"))?;
    if !runtime_claim_belongs_to_failed_start(
        &current,
        before,
        failed_binding,
        failed_runtime_agent_id,
        failed_alden_pid,
        failed_app_server_launch_claim_id,
    ) {
        anyhow::bail!(
            "runtime ownership changed concurrently; failed launch rollback was not applied"
        );
    }
    if current == *before {
        return Ok(());
    }
    store.sessions.insert(key.to_string(), before.clone());
    persist_cutex_session_store_and_im_record(&store, key)
}

fn restore_app_server_launch_claim(
    key: &str,
    before: &CutexSessionRecord,
    app_server_launch_claim_id: &str,
) -> anyhow::Result<()> {
    let mut store = load_cutex_session_store()?;
    let current = store.sessions.get(key).cloned().ok_or_else(|| {
        anyhow!("cutex session disappeared during app-server launch rollback: {key}")
    })?;
    if current != *before
        && (current.app_server_launch_claim_id.as_deref() != Some(app_server_launch_claim_id)
            || current.host_id != before.host_id
            || current.runtime_backend != before.runtime_backend
            || current.profile != before.profile
            || current.pending_launch_id != before.pending_launch_id)
    {
        anyhow::bail!(
            "runtime ownership changed concurrently; app-server launch rollback was not applied"
        );
    }
    if current == *before {
        return Ok(());
    }
    store.sessions.insert(key.to_string(), before.clone());
    persist_cutex_session_store_and_im_record(&store, key)
}

fn runtime_claim_belongs_to_failed_start(
    current: &CutexSessionRecord,
    before: &CutexSessionRecord,
    failed_binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    failed_runtime_agent_id: &str,
    failed_alden_pid: Option<u32>,
    failed_app_server_launch_claim_id: &str,
) -> bool {
    let stable_runtime_spec_matches = current.host_id == before.host_id
        && current.explicit_launch == before.explicit_launch
        && current.runtime_backend == before.runtime_backend
        && current.profile == before.profile
        && current.codex_session_id == before.codex_session_id;
    let next_generation = before.runtime_generation.checked_add(1);
    let generation_matches = current.runtime_generation == before.runtime_generation
        || next_generation == Some(current.runtime_generation);
    let binding_matches = current.app_server_runtime == before.app_server_runtime
        || current.app_server_runtime.as_ref() == Some(failed_binding);
    let runtime_agent_matches = current.current_runtime_agent_id == before.current_runtime_agent_id
        || current.current_runtime_agent_id.as_deref() == Some(failed_runtime_agent_id);
    let runtime_pid_matches = current.runtime_pid == before.runtime_pid
        || current.runtime_pid == Some(failed_binding.pid);
    let alden_pid_matches =
        current.alden_pid == before.alden_pid || current.alden_pid == failed_alden_pid;
    let app_server_launch_claim_matches = current.app_server_launch_claim_id
        == before.app_server_launch_claim_id
        || current.app_server_launch_claim_id.as_deref() == Some(failed_app_server_launch_claim_id);
    let legacy_launch_correlation_matches = current.pending_launch_id == before.pending_launch_id;
    stable_runtime_spec_matches
        && generation_matches
        && binding_matches
        && runtime_agent_matches
        && runtime_pid_matches
        && alden_pid_matches
        && app_server_launch_claim_matches
        && legacy_launch_correlation_matches
}

fn runtime_claim_belongs_to_started_attempt(
    current: &CutexSessionRecord,
    before: &CutexSessionRecord,
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    runtime_agent_id: &str,
    alden_pid: Option<u32>,
    app_server_launch_claim_id: &str,
) -> bool {
    current.app_server_launch_claim_id.as_deref() == Some(app_server_launch_claim_id)
        && runtime_claim_belongs_to_failed_start(
            current,
            before,
            binding,
            runtime_agent_id,
            alden_pid,
            app_server_launch_claim_id,
        )
}

fn rollback_after_cleanup(
    cleanup: &anyhow::Result<()>,
    rollback: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if cleanup.is_err() {
        return Err(anyhow!(
            "child/runtime cleanup failed; durable ownership claim was retained"
        ));
    }
    rollback()
}

fn error_with_cleanup_and_rollback(
    error: anyhow::Error,
    cleanup: anyhow::Result<()>,
    rollback: anyhow::Result<()>,
    stage: &str,
) -> anyhow::Error {
    let error = match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "failed to clean up child/runtime files after {stage} failure: {cleanup_error:#}"
        )),
    };
    match rollback {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "failed to restore durable runtime state after {stage} rollback: {rollback_error:#}"
        )),
    }
}

fn clear_stale_failed_start_claim(
    store: &mut cutex::session::model::CutexSessionStore,
    key: &str,
) -> anyhow::Result<()> {
    let legacy_pending_launch_id = store
        .sessions
        .get(key)
        .and_then(|record| record.pending_launch_id.clone());
    clear_cutex_session_runtime_record(store, key, true)?;
    if let Some(record) = store.sessions.get_mut(key) {
        record.pending_launch_id = legacy_pending_launch_id;
    }
    Ok(())
}

fn cleanup_failed_app_server_start(
    cutex_session_id: &str,
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    app_server_child: &mut Child,
    remote_tui_child: Option<&mut Child>,
    alden_session_name: Option<&str>,
) -> anyhow::Result<()> {
    let mut errors = Vec::new();
    match terminate_managed_agent_scope(cutex_session_id, true) {
        Ok(outcome) if outcome.stopped => {}
        Ok(outcome) => errors.push(format!(
            "managed Agent process scope did not stop: {}",
            outcome.detail
        )),
        Err(error) => errors.push(format!(
            "failed to stop managed Agent process scope: {error:#}"
        )),
    }
    if let Some(session) = alden_session_name.and_then(find_cute_alden_session_by_name) {
        match terminate_process_and_wait(session.pid, true) {
            Ok(outcome) if outcome.stopped => {}
            Ok(outcome) => errors.push(format!(
                "cute-alden process {} did not stop: {}",
                session.pid, outcome.detail
            )),
            Err(error) => errors.push(format!(
                "failed to stop cute-alden process {}: {error:#}",
                session.pid
            )),
        }
    }
    if let Some(child) = remote_tui_child {
        if let Err(error) = stop_startup_child(child) {
            errors.push(format!("failed to stop remote TUI launch child: {error:#}"));
        }
    }
    if let Err(error) = app_server_runtime::disconnect_runtime(cutex_session_id) {
        errors.push(format!(
            "failed to disconnect managed app-server: {error:#}"
        ));
    }
    if let Err(error) = stop_startup_child(app_server_child) {
        errors.push(format!("failed to stop app-server launch child: {error:#}"));
    }
    if let Err(error) = cleanup_runtime_binding_files(binding) {
        errors.push(format!(
            "failed to remove app-server runtime files: {error:#}"
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}

fn stop_startup_child(child: &mut Child) -> anyhow::Result<()> {
    if child
        .try_wait()
        .context("failed to inspect launch child")?
        .is_none()
    {
        child.kill().context("failed to terminate launch child")?;
    }
    child
        .wait()
        .context("failed to reap launch child")
        .map(|_| ())
}

fn ensure_agent_bus_for_launch_record() -> anyhow::Result<()> {
    let config = load_codez_config();
    if !config.agent_bus_enabled {
        anyhow::bail!("agent bus is disabled");
    }
    agent_bus_runtime::ensure_agent_bus_running(&config, true)
}

fn ensure_cutex_session_runtime_host_is_local(record: &CutexSessionRecord) -> anyhow::Result<()> {
    let current_host = current_host_name();
    if cutex_session_host_is_local(&record.host_id, &current_host) {
        return Ok(());
    }
    anyhow::bail!(
        "remote_runtime_manager_required: session host_id={} current_host={} cutex_session_id={}",
        record.host_id,
        current_host,
        record.cutex_session_id
    )
}

fn effective_session_profile_name<'a>(
    record: &'a CutexSessionRecord,
    global_config: &'a CodezConfig,
) -> anyhow::Result<&'a str> {
    record
        .profile
        .as_deref()
        .or(global_config.default_profile.as_deref())
        .ok_or_else(|| {
            anyhow!(
                "cutex session follows the global default, but no global default profile is set"
            )
        })
}

fn account_for_cutex_session(record: &CutexSessionRecord) -> anyhow::Result<StoredAccount> {
    let global_config = load_codez_config();
    let profile = effective_session_profile_name(record, &global_config)?;
    let store = load_store()?;
    find_account(&store, profile)?
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "Account not found for effective session profile ({})",
                cutex::profiles::identity::unknown_profile_message(
                    profile,
                    &cutex::profiles::identity::available_profile_names(&store),
                )
            )
        })
}

fn account_for_runtime_occurrence(profile: &str) -> anyhow::Result<StoredAccount> {
    let profile = profile.trim();
    if profile.is_empty() {
        anyhow::bail!("managed runtime occurrence profile evidence is empty");
    }
    let store = load_store()?;
    find_account(&store, profile)?
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "Account not found for launched runtime profile ({})",
                cutex::profiles::identity::unknown_profile_message(
                    profile,
                    &cutex::profiles::identity::available_profile_names(&store),
                )
            )
        })
}

fn runtime_occurrence_profile_name(
    record: &CutexSessionRecord,
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    live_agents: &[AgentBusAgent],
) -> Option<String> {
    normalized_runtime_profile(binding.current_launched_profile().as_deref()).or_else(|| {
        live_agents
            .iter()
            .find(|agent| runtime_agent_matches_binding(record, binding, agent))
            .and_then(|agent| normalized_runtime_profile(Some(&agent.profile)))
    })
}

fn runtime_agent_matches_binding(
    record: &CutexSessionRecord,
    binding: &cutex::session::model::CutexAppServerRuntimeBinding,
    agent: &AgentBusAgent,
) -> bool {
    if agent.pid != binding.pid {
        return false;
    }
    match record.current_runtime_agent_id.as_deref() {
        Some(runtime_agent_id) => agent.id == runtime_agent_id,
        None => record
            .codex_session_id
            .as_deref()
            .is_some_and(|session_id| agent.session_id.as_deref() == Some(session_id)),
    }
}

fn normalized_runtime_profile(profile: Option<&str>) -> Option<String> {
    profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty() && *profile != "-")
        .map(str::to_string)
}

#[cfg(test)]
pub(crate) fn session_online_launch_command(
    record: &CutexSessionRecord,
    account: &StoredAccount,
) -> anyhow::Result<SessionOnlineLaunch> {
    let resume_plan = session_online_resume_plan(record, account)?;
    let base_launch = launch_command::codex_launch_command_with_agent_mode(
        account,
        &resume_plan.effective_args,
        true,
        &resume_plan.groups,
    )?;
    finalize_session_online_launch(record, account, base_launch, &resume_plan)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRuntimeStopResult {
    pub(crate) had_runtime: bool,
    pub(crate) stopped: bool,
    pub(crate) forced: bool,
    pub(crate) pid: Option<u32>,
    pub(crate) pids: Vec<u32>,
    pub(crate) alden_session_name: Option<String>,
    pub(crate) runtime_agent_id: Option<String>,
    pub(crate) detail: String,
}

pub(crate) fn stop_cutex_session_runtime_for_entry(
    entry: &CodingSessionRegistration,
    live_agents: &[AgentBusAgent],
    force: bool,
) -> anyhow::Result<SessionRuntimeStopResult> {
    stop_cutex_session_runtime_for_entry_fenced(entry, live_agents, force, None)
}

pub(crate) fn stop_cutex_session_runtime_for_entry_fenced(
    entry: &CodingSessionRegistration,
    live_agents: &[AgentBusAgent],
    force: bool,
    fence: Option<u64>,
) -> anyhow::Result<SessionRuntimeStopResult> {
    let mut store = load_cutex_session_store()?;
    let Some(key) = cutex_session_key_for_user_id(&store, &entry.session_id) else {
        if let Some(agent) = live_agents.first() {
            anyhow::bail!(
                "runtime_stop_unsupported: cutex session record missing for live agent {}",
                agent.id
            );
        }
        return Ok(SessionRuntimeStopResult {
            had_runtime: false,
            stopped: true,
            forced: false,
            pid: None,
            pids: Vec::new(),
            alden_session_name: None,
            runtime_agent_id: None,
            detail: "cutex_session_not_found".to_string(),
        });
    };
    let record = store
        .sessions
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow!("cutex session disappeared while stopping: {key}"))?;
    if let Some(generation) = fence {
        anyhow::ensure!(
            record.runtime_generation == generation,
            "runtime changed before stop; owner unchanged"
        );
    }
    ensure_cutex_session_runtime_host_is_local(&record)?;
    if record.explicit_launch.is_some() {
        let outcome = super::native_stop::stop_and_commit(&record, force)?;
        return Ok(SessionRuntimeStopResult {
            had_runtime: record.app_server_runtime.is_some(),
            stopped: outcome.stopped,
            forced: outcome.forced,
            pid: record.app_server_runtime.as_ref().map(|b| b.pid),
            pids: record
                .app_server_runtime
                .as_ref()
                .map(|b| vec![b.pid])
                .unwrap_or_default(),
            alden_session_name: None,
            runtime_agent_id: record.current_runtime_agent_id.clone(),
            detail: outcome.detail,
        });
    }
    let alden_session = record
        .alden_session_name
        .as_deref()
        .and_then(find_cute_alden_session_by_name);
    let local_host = current_host_name();
    let target =
        session_runtime_stop_target(&record, live_agents, alden_session.as_ref(), &local_host);
    let app_server_binding = record.app_server_runtime.clone();
    let turn_stop_detail = match app_server_runtime::runtime_manager()
        .interrupt_active_turn(&record.cutex_session_id)
    {
        Ok(Some(turn_id)) => Some(format!("turn_interrupted:{turn_id}")),
        Ok(None) => None,
        Err(error) if force => Some(format!("turn_interrupt_failed_forced:{error:#}")),
        Err(error) => {
            return Err(error).context(
                "runtime stop refused because the active turn could not be interrupted safely",
            );
        }
    };
    let disconnect_error = app_server_runtime::disconnect_runtime(&record.cutex_session_id)
        .err()
        .map(|error| format!("manager_disconnect_failed:{error:#}"));
    let scope_stop = terminate_managed_agent_scope(&record.cutex_session_id, force)?;

    if !target.had_runtime && !scope_stop.found {
        persist_runtime_stop_with_reconciliation(store, &key, &record)?;
        return Ok(SessionRuntimeStopResult {
            had_runtime: false,
            stopped: true,
            forced: false,
            pid: target.pid,
            pids: target.pids,
            alden_session_name: target.alden_session_name,
            runtime_agent_id: target.runtime_agent_id,
            detail: "already_offline".to_string(),
        });
    }

    if target.pids.is_empty()
        && force
        && app_server_binding.is_none()
        && alden_session.is_none()
        && live_agents.is_empty()
        && scope_stop.stopped
    {
        clear_stale_failed_start_claim(&mut store, &key)?;
        persist_cutex_session_store_and_im_record(&store, &key)?;
        return Ok(SessionRuntimeStopResult {
            had_runtime: true,
            stopped: true,
            forced: scope_stop.forced,
            pid: None,
            pids: Vec::new(),
            alden_session_name: target.alden_session_name,
            runtime_agent_id: target.runtime_agent_id,
            detail: if scope_stop.found {
                format!("stale_runtime_claim_cleared,{}", scope_stop.detail)
            } else {
                "stale_runtime_claim_cleared".to_string()
            },
        });
    }

    if target.pids.is_empty() && !scope_stop.found {
        anyhow::bail!("runtime_stop_unsupported: no local cute-alden/runtime pid recorded");
    }

    let mut pids_stopped = true;
    let mut forced = scope_stop.forced;
    let mut details = Vec::new();
    if let Some(detail) = turn_stop_detail {
        details.push(detail);
    }
    if scope_stop.found {
        details.push(scope_stop.detail);
    }
    if let Some(error) = disconnect_error {
        details.push(error);
    }
    for pid in target.pids.iter().copied() {
        let outcome = terminate_process_and_wait(pid, force)?;
        pids_stopped &= outcome.stopped;
        forced |= outcome.forced;
        details.push(format!("{pid}:{}", outcome.detail));
    }
    // The PID fallback can finish termination after the first scope wait timed
    // out. Reobserve the whole scope before deciding whether to retain claims.
    let stopped = pids_stopped
        && (scope_stop.stopped
            || cutex::runtime::process_scope::managed_agent_scope_is_absent(
                &record.cutex_session_id,
            )?);
    if stopped {
        if let Some(binding) = app_server_binding.as_ref() {
            match cleanup_runtime_binding_files(binding) {
                Ok(()) => details.push("app_server_files_removed".to_string()),
                Err(error) => details.push(format!("app_server_cleanup_failed:{error:#}")),
            }
        }
        persist_runtime_stop_with_reconciliation(store, &key, &record)?;
    }

    Ok(SessionRuntimeStopResult {
        had_runtime: true,
        stopped,
        forced,
        pid: target.pid,
        pids: target.pids,
        alden_session_name: target.alden_session_name,
        runtime_agent_id: target.runtime_agent_id,
        detail: details.join(","),
    })
}

fn persist_runtime_stop_with_reconciliation(
    store: cutex::session::model::CutexSessionStore,
    key: &str,
    expected: &CutexSessionRecord,
) -> anyhow::Result<()> {
    persist_runtime_stop_with_reconciliation_using(
        store,
        key,
        expected,
        persist_cutex_session_store_and_im_record,
        load_cutex_session_store,
    )
}

fn persist_runtime_stop_with_reconciliation_using(
    mut store: cutex::session::model::CutexSessionStore,
    key: &str,
    expected: &CutexSessionRecord,
    mut persist: impl FnMut(&cutex::session::model::CutexSessionStore, &str) -> anyhow::Result<()>,
    mut reload: impl FnMut() -> anyhow::Result<cutex::session::model::CutexSessionStore>,
) -> anyhow::Result<()> {
    for attempt in 0..RUNTIME_STOP_CAS_ATTEMPTS {
        clear_cutex_session_runtime_record(&mut store, key, true)?;
        match persist(&store, key) {
            Ok(()) => return Ok(()),
            Err(error)
                if error
                    .downcast_ref::<CutexSessionStoreRevisionConflict>()
                    .is_some() =>
            {
                let current_store = reload()?;
                let current = current_store.sessions.get(key).ok_or_else(|| {
                    anyhow!("runtime_stop_revision_conflict: expected durable session disappeared")
                })?;
                match reconcile_runtime_stop_revision_conflict(expected, current) {
                    RuntimeStopRevisionReconciliation::ProvenOffline => return Ok(()),
                    RuntimeStopRevisionReconciliation::SameClaim
                        if attempt + 1 < RUNTIME_STOP_CAS_ATTEMPTS =>
                    {
                        store = current_store;
                    }
                    RuntimeStopRevisionReconciliation::SameClaim => {
                        anyhow::bail!(
                            "runtime_stop_revision_conflict: bounded CAS retries exhausted"
                        )
                    }
                    RuntimeStopRevisionReconciliation::Fence(reason) => {
                        anyhow::bail!("runtime_stop_revision_conflict: {reason}")
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("runtime stop CAS loop returns on every final attempt")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeStopRevisionReconciliation {
    ProvenOffline,
    SameClaim,
    Fence(&'static str),
}

fn reconcile_runtime_stop_revision_conflict(
    expected: &CutexSessionRecord,
    current: &CutexSessionRecord,
) -> RuntimeStopRevisionReconciliation {
    if current.is_retired() {
        return RuntimeStopRevisionReconciliation::Fence("durable session was retired");
    }
    if !same_runtime_stop_identity(expected, current) {
        return RuntimeStopRevisionReconciliation::Fence(
            "durable target identity or managed specification changed",
        );
    }
    if runtime_claim_is_offline(current) {
        return RuntimeStopRevisionReconciliation::ProvenOffline;
    }
    if same_runtime_claim(expected, current) {
        return RuntimeStopRevisionReconciliation::SameClaim;
    }
    RuntimeStopRevisionReconciliation::Fence(
        "runtime occurrence changed or is only partially cleared",
    )
}

fn same_runtime_stop_identity(expected: &CutexSessionRecord, current: &CutexSessionRecord) -> bool {
    fn without_runtime_and_heartbeat(mut record: CutexSessionRecord) -> CutexSessionRecord {
        record.pending_launch_id = None;
        record.app_server_launch_claim_id = None;
        record.alden_session_name = None;
        record.alden_pid = None;
        record.runtime_pid = None;
        record.app_server_runtime = None;
        record.current_runtime_agent_id = None;
        record.last_seen_at = None;
        record.updated_at.clear();
        record
    }
    without_runtime_and_heartbeat(expected.clone())
        == without_runtime_and_heartbeat(current.clone())
}

fn runtime_claim_is_offline(record: &CutexSessionRecord) -> bool {
    record.pending_launch_id.is_none()
        && record.app_server_launch_claim_id.is_none()
        && record.alden_pid.is_none()
        && record.runtime_pid.is_none()
        && record.app_server_runtime.is_none()
        && record.current_runtime_agent_id.is_none()
}

fn same_runtime_claim(expected: &CutexSessionRecord, current: &CutexSessionRecord) -> bool {
    expected.pending_launch_id == current.pending_launch_id
        && expected.app_server_launch_claim_id == current.app_server_launch_claim_id
        && expected.alden_session_name == current.alden_session_name
        && expected.alden_pid == current.alden_pid
        && expected.runtime_pid == current.runtime_pid
        && expected.app_server_runtime == current.app_server_runtime
        && expected.current_runtime_agent_id == current.current_runtime_agent_id
}

pub(crate) fn live_agents_for_management_entry(
    config: &CodezConfig,
    entry: &CodingSessionRegistration,
) -> Vec<AgentBusAgent> {
    filter_live_agents_for_management_identity(
        agent_bus_fetch_agents_if_healthy(config),
        &entry.session_id,
        entry.last_runtime_agent_id.as_deref(),
    )
}

pub(crate) fn try_live_agents_for_management_entry(
    config: &CodezConfig,
    entry: &CodingSessionRegistration,
) -> anyhow::Result<Vec<AgentBusAgent>> {
    try_live_agents_for_management_identity(
        config,
        &entry.session_id,
        entry.last_runtime_agent_id.as_deref(),
    )
}

pub(crate) fn try_live_agents_for_management_identity(
    config: &CodezConfig,
    session_id: &str,
    last_runtime_agent_id: Option<&str>,
) -> anyhow::Result<Vec<AgentBusAgent>> {
    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    if let Some(agents) = archive_agent_bus_test_empty_roster()? {
        return Ok(agents);
    }

    Ok(filter_live_agents_for_management_identity(
        agent_bus_fetch_agents(config)?,
        session_id,
        last_runtime_agent_id,
    ))
}

#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
const ARCHIVE_AGENT_BUS_EMPTY_ROSTER_MARKER_ENV_VAR: &str =
    "CUTEX_ARCHIVE_TEST_EMPTY_ROSTER_MARKER";
#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
const ARCHIVE_AGENT_BUS_EMPTY_ROSTER_MARKER_CONTENT: &[u8] = b"cutex-archive-empty-roster-v1\n";

#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
fn archive_agent_bus_test_empty_roster() -> anyhow::Result<Option<Vec<AgentBusAgent>>> {
    let marker = std::env::var_os(ARCHIVE_AGENT_BUS_EMPTY_ROSTER_MARKER_ENV_VAR);
    archive_agent_bus_test_empty_roster_with_config_root(marker.as_deref(), config_dir)
}

#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
fn archive_agent_bus_test_empty_roster_with_config_root<F>(
    marker: Option<&OsStr>,
    resolve_config_root: F,
) -> anyhow::Result<Option<Vec<AgentBusAgent>>>
where
    F: FnOnce() -> anyhow::Result<PathBuf>,
{
    let Some(marker) = marker else {
        return Ok(None);
    };
    let config_root = resolve_config_root()?;
    archive_agent_bus_test_empty_roster_from_marker(Some(marker), &config_root)
}

#[cfg(feature = "archive-agent-bus-roster-test-fixture")]
fn archive_agent_bus_test_empty_roster_from_marker(
    marker: Option<&OsStr>,
    config_root: &Path,
) -> anyhow::Result<Option<Vec<AgentBusAgent>>> {
    let Some(marker) = marker else {
        return Ok(None);
    };
    let marker = Path::new(marker);
    if !marker.is_absolute() {
        anyhow::bail!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker must be an absolute path"
        );
    }

    let metadata = fs::symlink_metadata(marker).with_context(|| {
        format!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker is unavailable: {}",
            marker.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker must not be a symlink"
        );
    }
    if !metadata.is_file() {
        anyhow::bail!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker must be a regular file"
        );
    }

    let fixture_root = config_root.join("test-fixtures").canonicalize().with_context(|| {
        format!(
            "archive_agent_bus_roster_test_fixture_configuration_error: fixture root is unavailable: {}",
            config_root.join("test-fixtures").display()
        )
    })?;
    let marker = marker.canonicalize().with_context(|| {
        format!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker cannot be canonicalized: {}",
            marker.display()
        )
    })?;
    if !marker.starts_with(&fixture_root) {
        anyhow::bail!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker is outside the test-fixtures root"
        );
    }

    let content = fs::read(&marker).with_context(|| {
        format!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker cannot be read: {}",
            marker.display()
        )
    })?;
    if content != ARCHIVE_AGENT_BUS_EMPTY_ROSTER_MARKER_CONTENT {
        anyhow::bail!(
            "archive_agent_bus_roster_test_fixture_configuration_error: marker content is invalid"
        );
    }
    Ok(Some(Vec::new()))
}

pub(crate) fn filter_live_agents_for_management_identity(
    agents: impl IntoIterator<Item = AgentBusAgent>,
    session_id: &str,
    last_runtime_agent_id: Option<&str>,
) -> Vec<AgentBusAgent> {
    agents
        .into_iter()
        .filter(|agent| {
            agent.session_id.as_deref() == Some(session_id)
                || (agent.session_id.is_none() && last_runtime_agent_id == Some(agent.id.as_str()))
        })
        .filter(agent_endpoint_is_usable_for_this_host)
        .collect()
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use cutex::session::model::{CutexAppServerRuntimeBinding, CutexAppServerTransport};
    use std::fs;

    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    struct ArchiveRosterFixtureDir(std::path::PathBuf);

    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    impl ArchiveRosterFixtureDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("cutex-archive-roster-{}", Uuid::new_v4()));
            fs::create_dir_all(path.join("test-fixtures")).expect("fixture directory");
            Self(path)
        }

        fn marker(&self, name: &str) -> std::path::PathBuf {
            self.0.join("test-fixtures").join(name)
        }
    }

    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    impl Drop for ArchiveRosterFixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    fn write_archive_roster_marker(path: &std::path::Path, content: &[u8]) {
        fs::write(path, content).expect("write marker");
    }

    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    #[test]
    fn archive_roster_fixture_uses_a_valid_empty_roster_marker() {
        let fixture = ArchiveRosterFixtureDir::new();
        let marker = fixture.marker("empty-roster");
        write_archive_roster_marker(&marker, ARCHIVE_AGENT_BUS_EMPTY_ROSTER_MARKER_CONTENT);

        let roster =
            archive_agent_bus_test_empty_roster_from_marker(Some(marker.as_os_str()), &fixture.0)
                .expect("valid marker")
                .expect("valid marker must select the fixture roster");
        assert!(roster.is_empty());
    }

    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    #[test]
    fn archive_roster_fixture_wrapper_skips_config_resolution_without_a_marker() {
        let config_root_resolved = std::cell::Cell::new(false);

        let roster = archive_agent_bus_test_empty_roster_with_config_root(None, || {
            config_root_resolved.set(true);
            anyhow::bail!("config root must not be resolved without a marker");
        })
        .expect("unconfigured fixture must retain the real TCP roster path");

        assert!(roster.is_none());
        assert!(!config_root_resolved.get());
    }

    #[cfg(feature = "archive-agent-bus-roster-test-fixture")]
    #[test]
    fn archive_roster_fixture_rejects_invalid_markers_before_roster_access() {
        let fixture = ArchiveRosterFixtureDir::new();
        let outside = fixture.0.join("outside-marker");
        write_archive_roster_marker(&outside, ARCHIVE_AGENT_BUS_EMPTY_ROSTER_MARKER_CONTENT);
        let missing = fixture.marker("missing-marker");
        let directory = fixture.marker("directory-marker");
        fs::create_dir(&directory).expect("marker directory");
        let malformed = fixture.marker("malformed-marker");
        write_archive_roster_marker(&malformed, b"not the marker content\n");

        let relative = archive_agent_bus_test_empty_roster_from_marker(
            Some(OsStr::new("relative-marker")),
            &fixture.0,
        )
        .expect_err("relative marker must fail");
        assert!(relative
            .to_string()
            .contains("archive_agent_bus_roster_test_fixture_configuration_error"));
        for marker in [&outside, &missing, &directory, &malformed] {
            let error = archive_agent_bus_test_empty_roster_from_marker(
                Some(marker.as_os_str()),
                &fixture.0,
            )
            .expect_err("invalid marker must fail");
            assert!(error
                .to_string()
                .contains("archive_agent_bus_roster_test_fixture_configuration_error"));
        }
    }

    #[cfg(all(feature = "archive-agent-bus-roster-test-fixture", any(unix, windows)))]
    #[test]
    fn archive_roster_fixture_rejects_a_symlink_marker() {
        let fixture = ArchiveRosterFixtureDir::new();
        let target = fixture.marker("target-marker");
        write_archive_roster_marker(&target, ARCHIVE_AGENT_BUS_EMPTY_ROSTER_MARKER_CONTENT);
        let link = fixture.marker("linked-marker");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("symlink marker");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).expect("symlink marker");

        let error =
            archive_agent_bus_test_empty_roster_from_marker(Some(link.as_os_str()), &fixture.0)
                .expect_err("symlink marker must fail");
        assert!(error
            .to_string()
            .contains("archive_agent_bus_roster_test_fixture_configuration_error"));
    }

    fn record(profile: Option<&str>) -> CutexSessionRecord {
        CutexSessionRecord::new_at(
            "cutex.profile-resolution".to_string(),
            Some("thread-profile-resolution".to_string()),
            "devbox".to_string(),
            "/tmp/project".to_string(),
            profile.map(str::to_string),
            "2026-08-07T00:00:00Z".to_string(),
        )
        .expect("session record")
    }

    #[test]
    fn production_fenced_claim_rejects_newer_occurrence_after_initial_check_without_effect() {
        let key = "session";
        let expected = record(None);
        let mut initial = cutex::session::model::CutexSessionStore::default();
        initial.sessions.insert(key.to_string(), expected.clone());
        let mut current = cutex::session::model::CutexSessionStore::default();
        current.sessions.insert(key.to_string(), expected.clone());
        let newer = current.sessions.get_mut(key).unwrap();
        newer.runtime_generation += 1;
        newer.current_runtime_agent_id = Some("runtime-newer".to_string());
        let persisted = std::cell::Cell::new(0usize);

        let error = commit_fenced_online_claim_using(
            &mut initial,
            key,
            &expected,
            "claim-historical",
            &|record| {
                if record.current_runtime_agent_id.is_some() {
                    anyhow::bail!("newer external occurrence is present")
                }
                Ok(())
            },
            || Ok(current),
            |_, _| {
                persisted.set(persisted.get() + 1);
                Ok(())
            },
        )
        .expect_err("newer occurrence must fence before claim persistence");
        assert!(error.to_string().contains("newer external occurrence"));
        assert_eq!(persisted.get(), 0);
        assert!(initial.sessions[key].app_server_launch_claim_id.is_none());
    }

    #[test]
    fn production_fenced_claim_rejects_newer_registration_before_online_without_effect() {
        let key = "session";
        let expected = record(None);
        let mut initial = cutex::session::model::CutexSessionStore::default();
        initial.sessions.insert(key.to_string(), expected.clone());
        let mut current = cutex::session::model::CutexSessionStore::default();
        current.sessions.insert(key.to_string(), expected.clone());
        current.store_revision.set(initial.store_revision.get() + 1);
        let validated = std::cell::Cell::new(0usize);
        let persisted = std::cell::Cell::new(0usize);

        let error = commit_fenced_online_claim_using(
            &mut initial,
            key,
            &expected,
            "claim-historical",
            &|_| {
                validated.set(validated.get() + 1);
                Ok(())
            },
            || Ok(current),
            |_, _| {
                persisted.set(persisted.get() + 1);
                Ok(())
            },
        )
        .expect_err("newer durable registration must fence before online claim");
        assert!(error
            .to_string()
            .contains("runtime occurrence changed before the fenced launch claim"));
        assert_eq!(validated.get(), 0);
        assert_eq!(persisted.get(), 0);
        assert!(initial.sessions[key].app_server_launch_claim_id.is_none());
    }

    fn live_remote_tui_account() -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            claude_code: Default::default(),
            id: "account-live-remote-tui".to_string(),
            name: "live-remote-tui".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: None,
            runtime: cutex::profiles::model::RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: cutex::profiles::model::CliKind::Codex,
            default_cli_args: vec!["--model".to_string(), "profile-model".to_string()],
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        }
    }

    #[test]
    fn fresh_live_remote_tui_launch_preserves_profile_endpoint_and_auth_args() {
        let _environment = crate::cli_app::test_home::environment_lock()
            .lock()
            .unwrap();
        let thread_id = format!("fresh-live-no-rollout-{}", Uuid::new_v4());
        assert!(
            !cutex::runtime::lifecycle::codex_session_exists_in_home(&thread_id)
                .expect("rollout lookup")
        );
        let mut record = CutexSessionRecord::new_at(
            "cutex.fresh-live-remote-tui".to_string(),
            Some(thread_id.clone()),
            "host".to_string(),
            "/tmp/fresh-live-remote-tui".to_string(),
            Some("live-remote-tui".to_string()),
            "2026-08-27T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.default_cli_args = vec!["--no-alt-screen".to_string()];
        let account = live_remote_tui_account();

        let runtime_path = cutex::config::paths::runtime_dir()
            .expect("runtime root")
            .join("app-server")
            .join(format!("live-remote-tui-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&runtime_path).expect("test runtime directory");
        let token_path = runtime_path.join("capability-token");
        fs::write(&token_path, "exact-test-token\n").expect("test capability token");
        let endpoint = "ws://127.0.0.1:32145";
        let binding = CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::LoopbackWebSocket,
            endpoint: endpoint.to_string(),
            pid: 4242,
            runtime_dir: runtime_path.display().to_string(),
            launched_profile: Some("live-remote-tui".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: Some(token_path.display().to_string()),
            diagnostic_journal_path: runtime_path.join("events.jsonl").display().to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "test".to_string(),
            started_at: "2026-08-27T00:00:00Z".to_string(),
        };
        let layout = AppServerRuntimeLayout::from_binding(&binding).expect("live binding layout");
        let files = MaterializedAccountFiles {
            auth_path: runtime_path.join("auth.json"),
            config_path: runtime_path.join("config.toml"),
            model_catalog_path: runtime_path.join("models.json"),
            custom_status_items_path: runtime_path.join("status.json"),
        };

        let launch =
            remote_tui_launch_command_with_profile(&record, &account, &layout, Some(&files))
                .expect("fresh live thread remote TUI launch");
        assert!(launch
            .args
            .windows(2)
            .any(|args| args == ["--model", "profile-model"]));
        assert!(launch.args.iter().any(|arg| arg == "--no-alt-screen"));
        assert!(launch
            .args
            .windows(4)
            .any(|args| args == ["resume", "-c", "tui.resume_cwd=\"current\"", thread_id.as_str()]));
        assert!(launch
            .args
            .windows(2)
            .any(|args| args == ["--remote", endpoint]));
        assert!(launch.args.windows(2).any(|args| args
            == [
                "--remote-auth-token-env",
                cutex::app_server::runtime::CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR,
            ]));
        assert_eq!(
            launch
                .envs
                .iter()
                .rev()
                .find(|(key, _)| {
                    key == cutex::app_server::runtime::CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR
                })
                .map(|(_, value)| value.as_str()),
            Some("exact-test-token")
        );

        layout.cleanup_files().expect("cleanup test runtime layout");
    }

    #[test]
    fn explicit_session_profile_wins_over_global_default() {
        let record = record(Some("session-profile"));
        let config = CodezConfig {
            default_profile: Some("global-profile".to_string()),
            ..CodezConfig::default()
        };

        assert_eq!(
            effective_session_profile_name(&record, &config).expect("effective profile"),
            "session-profile"
        );
    }

    #[test]
    fn inherited_session_profile_requires_and_uses_global_default() {
        let record = record(None);
        let config = CodezConfig {
            default_profile: Some("global-profile".to_string()),
            ..CodezConfig::default()
        };
        assert_eq!(
            effective_session_profile_name(&record, &config).expect("effective profile"),
            "global-profile"
        );

        let error = effective_session_profile_name(&record, &CodezConfig::default())
            .expect_err("missing global default must fail");
        assert_eq!(
            error.to_string(),
            "cutex session follows the global default, but no global default profile is set"
        );
    }

    #[test]
    fn failed_start_rollback_accepts_only_its_own_attempted_claim() {
        let mut before = record(None);
        before.pending_launch_id = Some("legacy-heartbeat-launch".to_string());
        let failed_binding = CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/app.sock".to_string(),
            pid: 4242,
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: Some("profile-a".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-07T00:00:00Z".to_string(),
        };
        let mut attempted = before.clone();
        attempted.runtime_generation = 1;
        attempted.app_server_runtime = Some(failed_binding.clone());
        attempted.current_runtime_agent_id = Some("runtime-attempt".to_string());
        attempted.runtime_pid = Some(4242);
        attempted.alden_pid = Some(4343);
        attempted.app_server_launch_claim_id = Some("launch-attempt".to_string());
        assert!(runtime_claim_belongs_to_failed_start(
            &attempted,
            &before,
            &failed_binding,
            "runtime-attempt",
            Some(4343),
            "launch-attempt",
        ));
        let mut concurrent = attempted.clone();
        concurrent.current_runtime_agent_id = Some("runtime-other".to_string());
        assert!(!runtime_claim_belongs_to_failed_start(
            &concurrent,
            &before,
            &failed_binding,
            "runtime-attempt",
            Some(4343),
            "launch-attempt",
        ));

        let mut profile_changed = attempted.clone();
        profile_changed.profile = Some("profile-b".to_string());
        assert!(!runtime_claim_belongs_to_failed_start(
            &profile_changed,
            &before,
            &failed_binding,
            "runtime-attempt",
            Some(4343),
            "launch-attempt",
        ));
    }

    #[test]
    fn successful_start_accepts_its_agent_bus_registration_update() {
        let before = record(None);
        let binding = runtime_binding(Some("profile-a"));
        let mut registered = before.clone();
        registered.runtime_generation = 1;
        registered.current_runtime_agent_id = Some("runtime-attempt".to_string());
        registered.runtime_pid = Some(4242);
        registered.app_server_launch_claim_id = Some("launch-attempt".to_string());

        assert!(runtime_claim_belongs_to_started_attempt(
            &registered,
            &before,
            &binding,
            "runtime-attempt",
            Some(4343),
            "launch-attempt",
        ));
    }

    #[test]
    fn successful_start_rejects_a_replaced_launch_claim() {
        let before = record(None);
        let binding = runtime_binding(Some("profile-a"));
        let mut concurrent = before.clone();
        concurrent.runtime_generation = 1;
        concurrent.current_runtime_agent_id = Some("runtime-attempt".to_string());
        concurrent.runtime_pid = Some(4242);
        concurrent.app_server_launch_claim_id = Some("launch-other".to_string());

        assert!(!runtime_claim_belongs_to_started_attempt(
            &concurrent,
            &before,
            &binding,
            "runtime-attempt",
            Some(4343),
            "launch-attempt",
        ));
    }

    #[test]
    fn successful_remote_tui_launcher_exit_keeps_waiting_for_registration() {
        let success = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("test binary should run");
        assert!(success.success());
        assert!(!remote_tui_launcher_failed(&success));
    }

    #[test]
    fn failed_remote_tui_launcher_exit_remains_terminal() {
        let failure = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg("--definitely-not-a-valid-test-argument")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("test binary should run");
        assert!(!failure.success());
        assert!(remote_tui_launcher_failed(&failure));
    }

    fn runtime_agent(id: &str, profile: &str, pid: u32) -> AgentBusAgent {
        AgentBusAgent {
            id: id.to_string(),
            name: "runtime.agent".to_string(),
            base_name: Some("runtime".to_string()),
            thread_name: None,
            path_key: None,
            session_id: Some("thread-profile-resolution".to_string()),
            cutex_session_id: None,
            profile: profile.to_string(),
            cwd: "/tmp/project".to_string(),
            pid,
            host_id: Some("devbox".to_string()),
            groups: Vec::new(),
            registration_class: cutex::agent_bus::model::AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 42,
        }
    }

    fn runtime_binding(profile: Option<&str>) -> CutexAppServerRuntimeBinding {
        CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/app.sock".to_string(),
            pid: 4242,
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: profile.map(str::to_string),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-08T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn durable_runtime_profile_wins_over_live_registration_metadata() {
        let mut record = record(None);
        record.current_runtime_agent_id = Some("runtime-1".to_string());
        let binding = runtime_binding(Some("durable-profile"));
        let agents = vec![runtime_agent("runtime-1", "observed-profile", 4242)];

        assert_eq!(
            runtime_occurrence_profile_name(&record, &binding, &agents).as_deref(),
            Some("durable-profile")
        );
    }

    #[test]
    fn live_runtime_registration_recovers_profile_removed_by_an_old_store_writer() {
        let mut record = record(None);
        record.current_runtime_agent_id = Some("runtime-1".to_string());
        let binding = runtime_binding(None);
        let agents = vec![runtime_agent("runtime-1", "orchid", 4242)];

        assert_eq!(
            runtime_occurrence_profile_name(&record, &binding, &agents).as_deref(),
            Some("orchid")
        );
    }

    #[test]
    fn runtime_profile_recovery_rejects_placeholder_and_mismatched_occurrences() {
        let mut record = record(None);
        record.current_runtime_agent_id = Some("runtime-1".to_string());
        let binding = runtime_binding(None);

        assert_eq!(
            runtime_occurrence_profile_name(
                &record,
                &binding,
                &[runtime_agent("runtime-1", "-", 4242)]
            ),
            None
        );
        assert_eq!(
            runtime_occurrence_profile_name(
                &record,
                &binding,
                &[runtime_agent("runtime-other", "orchid", 4242)]
            ),
            None
        );
        assert_eq!(
            runtime_occurrence_profile_name(
                &record,
                &binding,
                &[runtime_agent("runtime-1", "orchid", 4343)]
            ),
            None
        );
    }

    #[test]
    fn runtime_stop_revision_conflict_retries_only_the_same_claim() {
        let mut expected = record(Some("profile-a"));
        expected.runtime_generation = 7;
        expected.runtime_pid = Some(4242);
        expected.current_runtime_agent_id = Some("runtime-7".to_string());
        expected.app_server_runtime = Some(runtime_binding(Some("profile-a")));

        let mut heartbeat_only = expected.clone();
        heartbeat_only.last_seen_at = Some("2026-08-30T01:00:00Z".to_string());
        heartbeat_only.updated_at = "2026-08-30T01:00:00Z".to_string();
        assert_eq!(
            reconcile_runtime_stop_revision_conflict(&expected, &heartbeat_only),
            RuntimeStopRevisionReconciliation::SameClaim
        );

        let mut offline = heartbeat_only.clone();
        offline.pending_launch_id = None;
        offline.app_server_launch_claim_id = None;
        offline.alden_pid = None;
        offline.runtime_pid = None;
        offline.app_server_runtime = None;
        offline.current_runtime_agent_id = None;
        assert_eq!(
            reconcile_runtime_stop_revision_conflict(&expected, &offline),
            RuntimeStopRevisionReconciliation::ProvenOffline
        );
    }

    #[test]
    fn runtime_stop_revision_conflict_performs_one_bounded_fresh_cas_retry() {
        let mut expected = record(Some("profile-a"));
        expected.runtime_generation = 7;
        expected.runtime_pid = Some(4242);
        expected.current_runtime_agent_id = Some("runtime-7".to_string());
        expected.app_server_runtime = Some(runtime_binding(Some("profile-a")));
        let mut initial = cutex::session::model::CutexSessionStore::default();
        initial
            .sessions
            .insert("target".to_string(), expected.clone());
        let persist_calls = std::cell::Cell::new(0usize);

        persist_runtime_stop_with_reconciliation_using(
            initial,
            "target",
            &expected,
            |store, key| {
                assert!(runtime_claim_is_offline(&store.sessions[key]));
                let call = persist_calls.get();
                persist_calls.set(call + 1);
                if call == 0 {
                    Err(CutexSessionStoreRevisionConflict {
                        expected: 4,
                        actual: 5,
                    }
                    .into())
                } else {
                    Ok(())
                }
            },
            || {
                let mut current = cutex::session::model::CutexSessionStore::default();
                let mut heartbeat = expected.clone();
                heartbeat.last_seen_at = Some("2026-08-30T01:00:00Z".to_string());
                heartbeat.updated_at = "2026-08-30T01:00:00Z".to_string();
                current.sessions.insert("target".to_string(), heartbeat);
                Ok(current)
            },
        )
        .unwrap();
        assert_eq!(persist_calls.get(), 2);
    }

    #[test]
    fn runtime_stop_revision_conflict_fences_identity_occurrence_and_partial_drift() {
        let mut expected = record(Some("profile-a"));
        expected.runtime_generation = 7;
        expected.runtime_pid = Some(4242);
        expected.current_runtime_agent_id = Some("runtime-7".to_string());
        expected.app_server_runtime = Some(runtime_binding(Some("profile-a")));

        let mut spec_changed = expected.clone();
        spec_changed.profile = Some("profile-b".to_string());
        assert!(matches!(
            reconcile_runtime_stop_revision_conflict(&expected, &spec_changed),
            RuntimeStopRevisionReconciliation::Fence(_)
        ));

        let mut generation_changed = expected.clone();
        generation_changed.runtime_generation = 8;
        assert!(matches!(
            reconcile_runtime_stop_revision_conflict(&expected, &generation_changed),
            RuntimeStopRevisionReconciliation::Fence(_)
        ));

        let mut partial = expected.clone();
        partial.app_server_runtime = None;
        assert!(matches!(
            reconcile_runtime_stop_revision_conflict(&expected, &partial),
            RuntimeStopRevisionReconciliation::Fence(_)
        ));

        let mut retired = expected;
        retired.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        retired.retired_at = Some("2026-08-30T01:00:00Z".to_string());
        assert!(matches!(
            reconcile_runtime_stop_revision_conflict(&retired.clone(), &retired),
            RuntimeStopRevisionReconciliation::Fence(_)
        ));
    }
}
