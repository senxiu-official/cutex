use chrono::Utc;

use cutex::agent_bus::model::AgentBusAgent;
use cutex::agent_bus::server::{
    RegistrationCommit, RegistrationDisposition,
};
use cutex::im::registry::ImRegistry;
use cutex::management::v2::session_coordination::{
    session_lane, ReservationError,
};
use cutex::management::v2::model::CutexMessage;
use cutex::management::v2::model::EventCorrelation;
use cutex::management::v2::model::EventSource;
use cutex::management::v2::model::PendingEvent;
use cutex::management::v2::repository::management_v2_repository;
use cutex::platform::host::current_host_name;
use cutex::session::model::CutexSessionReconcileOutcome;
use cutex::session::runtime_reconciliation::{
    reconcile_cutex_session_store_for_registration, registration_cutex_session_id,
    registration_is_recent_heartbeat,
};
use cutex::session::service::{
    reconcile_cutex_session_store_from_agent, reconcile_cutex_session_store_from_im_registration,
};
use cutex::session::store::{
    cutex_sessions_path, with_cutex_session_store, with_locked_session_rows,
};

// The writers below run the whole-store reconciliation on a sessions-only
// copy under the store lock and commit just the changed session rows
// (session store Phase 2b): no copy or comparison of the launch receipts.

pub(crate) fn reconcile_cutex_session_from_agent(agent: &AgentBusAgent) -> anyhow::Result<()> {
    let timestamp = Utc::now().to_rfc3339();
    let host_id = current_host_name();
    let outcome = with_locked_session_rows(&cutex_sessions_path()?, |rows| {
        if rows.sessions().any(|(_, r)| {
            r.claude_session
                .as_ref()
                .is_some_and(|c| agent.session_id.as_deref() == Some(c.thread_id.as_str()))
        }) {
            return Ok(None);
        }
        let mut store = rows.sessions_copy();
        let Some(outcome) =
            reconcile_cutex_session_store_from_agent(&mut store, agent, &host_id, &timestamp)?
        else {
            return Ok(None);
        };
        rows.replace_sessions(&store)?;
        rows.commit()?;
        Ok(Some(outcome))
    })?;
    if let Some(outcome) = outcome {
        append_cutex_session_reconcile_events(&outcome, agent, &timestamp)?;
    }
    Ok(())
}

pub(crate) fn reconcile_cutex_session_registration(
    agent: &mut AgentBusAgent,
    commit_roster: &mut RegistrationCommit<'_>,
) -> anyhow::Result<RegistrationDisposition> {
    let timestamp = Utc::now().to_rfc3339();
    let host_id = current_host_name();
    // Claude's native thread ID is not its Codex session ID, so its exact
    // durable mapping must be resolved by the no-write precheck before the
    // generic runtime-identity resolver. Nothing is published until lane
    // admission below.
    let fast_path = registration_without_store_write(agent, &host_id, &timestamp)?;
    let target = match fast_path.as_ref() {
        Some(fast_path) => Some(fast_path.cutex_session_id.clone()),
        None => with_cutex_session_store(|store| {
            registration_cutex_session_id(store, agent)
        })??,
    };
    let Some(target) = target else {
        anyhow::ensure!(!agent.id.starts_with("claude."), "Claude registration has no saved session");
        return with_locked_session_rows(&cutex_sessions_path()?, |rows| {
            if registration_cutex_session_id(rows.committed(), agent)?.is_some() {
                return Ok(RegistrationDisposition::Deferred);
            }
            commit_roster(agent)?;
            Ok(RegistrationDisposition::Committed)
        });
    };
    let lane = session_lane(&target)?;
    let reservation = match lane.try_lock() {
        Ok(reservation) => Some(reservation),
        Err(ReservationError::WouldBlock) => None,
        Err(error) => return Err(error.into()),
    };
    let fast_path_was_resolved = fast_path.is_some();
    let fast_path = if reservation.is_some() && fast_path.is_some() {
        // The lane may have changed hands after the optimistic snapshot. Once
        // admitted, revalidate before publishing a no-write roster refresh.
        registration_without_store_write(agent, &host_id, &timestamp)?
    } else {
        fast_path
    };
    if fast_path_was_resolved && fast_path.is_none() {
        return Ok(RegistrationDisposition::Deferred);
    }
    if let Some(fast_path) = fast_path {
        anyhow::ensure!(
            fast_path.cutex_session_id == target,
            "registration target changed during heartbeat admission"
        );
        if reservation.is_none() && !fast_path.busy_callback_safe {
            return Ok(RegistrationDisposition::Deferred);
        }
        commit_roster(agent)?;
        drop(reservation);
        return Ok(RegistrationDisposition::Committed);
    }
    anyhow::ensure!(!agent.id.starts_with("claude."), "Claude registration has no saved session");
    let result = with_locked_session_rows(&cutex_sessions_path()?, |rows| {
        if registration_cutex_session_id(rows.committed(), agent)?.as_deref()
            != Some(target.as_str())
        {
            return Ok(RegistrationStoreResult::Deferred);
        }
        let reviewed = cutex::session::reviewed_registration::preserve_reviewed_groups(
            rows.committed(),
            agent,
            &host_id,
        )?;
        let pending_occurrence = registration_can_commit_while_session_busy(
            rows.committed(),
            agent,
            &host_id,
            reviewed.as_ref(),
        );
        if reservation.is_none() && !pending_occurrence {
            return Ok(RegistrationStoreResult::Deferred);
        }
        let bundle = reviewed
            .as_ref()
            .map(|(_, running_contract)| {
                cutex::launch::stock::StockBundle::load_running(running_contract)
            })
            .transpose()?;
        if let Some((record, _)) = &reviewed {
            super::stock_lifecycle::verify_stock_process_with_bundle(
                record
                    .app_server_runtime
                    .as_ref()
                    .expect("validated binding"),
                bundle.as_ref().expect("validated bundle"),
            )?;
        }
        let mut store = rows.sessions_copy();
        let reconciliation = reconcile_cutex_session_store_for_registration(
            &mut store, agent, &host_id, &timestamp,
        )?;
        if !reconciliation.store_fence_required {
            return Ok(RegistrationStoreResult::Deferred);
        }
        if let Some((record, _)) = &reviewed {
            super::stock_lifecycle::verify_stock_process_with_bundle(
                record
                    .app_server_runtime
                    .as_ref()
                    .expect("validated binding"),
                bundle.as_ref().expect("validated bundle"),
            )?;
        }
        rows.replace_sessions(&store)?;
        rows.commit()?;
        Ok(RegistrationStoreResult::Reconciled(reconciliation.outcome))
    })?;
    let RegistrationStoreResult::Reconciled(outcome) = result else {
        return Ok(RegistrationDisposition::Deferred);
    };
    if let Some(outcome) = outcome {
        append_cutex_session_reconcile_events(&outcome, agent, &timestamp)?;
    }
    commit_roster(agent)?;
    drop(reservation);
    Ok(RegistrationDisposition::Committed)
}

enum RegistrationStoreResult {
    Reconciled(Option<CutexSessionReconcileOutcome>),
    Deferred,
}

fn registration_matches_pending_occurrence(
    record: &cutex::session::model::CutexSessionRecord,
    agent: &AgentBusAgent,
    host_id: &str,
) -> bool {
    let Some(binding) = record.app_server_runtime.as_ref() else {
        return false;
    };
    record.app_server_launch_claim_id.is_some()
        && record.runtime_generation > 0
        && record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
        && record.runtime_pid == Some(agent.pid)
        && binding.pid == agent.pid
        && record.codex_session_id.as_deref() == agent.session_id.as_deref()
        && agent.profile
            == binding
                .launched_profile
                .as_deref()
                .unwrap_or("-")
        && cutex::session::metadata::normalize_session_cwd(&agent.cwd)
            == cutex::session::metadata::normalize_session_cwd(
                cutex::session::service::cutex_session_launch_cwd(record),
            )
        && agent.registration_class == record.registration_class
        && cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            host_id,
        )
        && agent.host_id.as_deref().is_some_and(|agent_host| {
            cutex::runtime::lifecycle::cutex_session_host_is_local(
                agent_host,
                host_id,
            )
        })
}

fn registration_can_commit_while_session_busy(
    store: &cutex::session::model::CutexSessionStore,
    agent: &AgentBusAgent,
    host_id: &str,
    reviewed: Option<&(
        cutex::session::model::CutexSessionRecord,
        cutex::agent_management::ExplicitLaunchContract,
    )>,
) -> bool {
    reviewed
        .is_some_and(|(record, _)| record.app_server_launch_claim_id.is_some())
        || store.sessions.values().any(|record| {
            registration_matches_pending_occurrence(record, agent, host_id)
        })
}

struct RegistrationFastPath {
    cutex_session_id: String,
    /// A lane-owning start may be waiting for this exact registration. Other
    /// no-write refreshes defer while busy so stop/rebind cannot republish an
    /// occurrence after its lifecycle operation removed it from the roster.
    busy_callback_safe: bool,
}

enum RegistrationPrecheck {
    /// Claude runtime registration: the agent fields its saved session pins.
    Claude {
        cutex_session_id: String,
        name: String,
        groups: Vec<String>,
        registration_class: cutex::agent_bus::model::AgentRegistrationClass,
    },
    /// Refresh that changes only a recent heartbeat; nothing to write.
    Heartbeat {
        agent: AgentBusAgent,
        reviewed: Option<(
            cutex::session::model::CutexSessionRecord,
            cutex::agent_management::ExplicitLaunchContract,
        )>,
        cutex_session_id: String,
        busy_callback_safe: bool,
    },
    Write,
}

/// Registration refreshes arrive every 30 s per agent. Decide from the cached
/// store (no clone of the whole store, no write) whether this one can be
/// answered without the load / reconcile / save cycle: Claude registrations
/// never write, and a refresh that only moves a recent `last_seen_at` is not
/// persisted (see `registration_is_recent_heartbeat`). Returns `Some` when
/// the registration can use this path. The returned durable identity must remain
/// covered by the caller's session reservation through roster persistence.
fn registration_without_store_write(
    agent: &mut AgentBusAgent,
    host_id: &str,
    timestamp: &str,
) -> anyhow::Result<Option<RegistrationFastPath>> {
    let precheck = with_cutex_session_store(|store| -> anyhow::Result<RegistrationPrecheck> {
        if let Some(record) = store.sessions.values().find(|r| {
            r.claude_session
                .as_ref()
                .is_some_and(|c| agent.session_id.as_deref() == Some(c.thread_id.as_str()))
        }) {
            anyhow::ensure!(
                registration_matches_current_claude_occurrence(record, agent, host_id)
                    && cutex::platform::process::process_is_running(agent.pid),
                "Claude registration does not match its current local runtime"
            );
            let name = cutex::session::service::cutex_session_display_name(record);
            return Ok(RegistrationPrecheck::Claude {
                cutex_session_id: record.cutex_session_id.clone(),
                name,
                groups: record.agent_groups.clone(),
                registration_class: record.registration_class,
            });
        }
        if agent.id.starts_with("claude.") {
            return Ok(RegistrationPrecheck::Write);
        }
        let mut candidate = agent.clone();
        let Ok(reviewed) = cutex::session::reviewed_registration::preserve_reviewed_groups(
            store,
            &mut candidate,
            host_id,
        ) else {
            return Ok(RegistrationPrecheck::Write);
        };
        if !registration_is_recent_heartbeat(store, &candidate, host_id, timestamp) {
            return Ok(RegistrationPrecheck::Write);
        }
        let cutex_session_id = registration_cutex_session_id(store, &candidate)?
            .ok_or_else(|| anyhow::anyhow!("heartbeat registration lost its durable session"))?;
        let busy_callback_safe = registration_can_commit_while_session_busy(
            store,
            &candidate,
            host_id,
            reviewed.as_ref(),
        );
        Ok(RegistrationPrecheck::Heartbeat {
            agent: candidate,
            reviewed,
            cutex_session_id,
            busy_callback_safe,
        })
    })??;
    match precheck {
        RegistrationPrecheck::Claude {
            cutex_session_id,
            name,
            groups,
            registration_class,
        } => {
            agent.cutex_session_id = Some(cutex_session_id.clone());
            agent.base_name = Some(name.clone());
            agent.name = name;
            agent.groups = groups;
            agent.registration_class = registration_class;
            Ok(Some(RegistrationFastPath {
                cutex_session_id,
                // Claude start persists its exact connection + PID before its
                // synchronous observer registers the bridge, while still
                // holding the lane. Close clears the row before disconnecting
                // that bridge, so an exact current callback is safe here.
                busy_callback_safe: true,
            }))
        }
        RegistrationPrecheck::Heartbeat {
            agent: candidate,
            reviewed,
            cutex_session_id,
            busy_callback_safe,
        } => {
            if let Some((record, contract)) = &reviewed {
                let bundle = cutex::launch::stock::StockBundle::load_running(contract)?;
                super::stock_lifecycle::verify_stock_process_with_bundle(
                    record
                        .app_server_runtime
                        .as_ref()
                        .expect("validated binding"),
                    &bundle,
                )?;
            }
            *agent = candidate;
            Ok(Some(RegistrationFastPath {
                cutex_session_id,
                busy_callback_safe,
            }))
        }
        RegistrationPrecheck::Write => Ok(None),
    }
}

fn registration_matches_current_claude_occurrence(
    record: &cutex::session::model::CutexSessionRecord,
    agent: &AgentBusAgent,
    host_id: &str,
) -> bool {
    let Some(claude) = record.claude_session.as_ref() else {
        return false;
    };
    let Some(connection) = claude.connection_id.as_deref() else {
        return false;
    };
    !cutex::agent_management::agent_execution_blocked(&record)
        && (record.agent_enabled || record.is_owned_session())
        && record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
        && agent.id == format!("claude.{connection}")
        && record.runtime_pid == Some(agent.pid)
        && agent.session_id.as_deref() == Some(claude.thread_id.as_str())
        && cutex::session::metadata::normalize_session_cwd(&agent.cwd)
            == cutex::session::metadata::normalize_session_cwd(&record.cwd)
        && agent.profile == record.profile.as_deref().unwrap_or("Claude")
        && agent.registration_class == record.registration_class
        && cutex::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            host_id,
        )
        && agent.host_id.as_deref().is_some_and(|agent_host| {
            cutex::runtime::lifecycle::cutex_session_host_is_local(
                agent_host,
                host_id,
            )
        })
}

pub(crate) fn mirror_im_registry_into_cutex_session_store(
    registry: &ImRegistry,
) -> anyhow::Result<()> {
    if registry.sessions.is_empty() {
        return Ok(());
    }
    let timestamp = Utc::now().to_rfc3339();
    with_locked_session_rows(&cutex_sessions_path()?, |rows| {
        let mut store = rows.sessions_copy();
        let mut changed = false;
        for entry in registry.sessions.values() {
            changed |=
                reconcile_cutex_session_store_from_im_registration(&mut store, entry, &timestamp)?;
        }
        if changed {
            rows.replace_sessions(&store)?;
            rows.commit()?;
        }
        Ok(())
    })
}

pub(crate) fn append_cutex_session_reconcile_events(
    outcome: &CutexSessionReconcileOutcome,
    _agent: &AgentBusAgent,
    timestamp: &str,
) -> anyhow::Result<()> {
    if outcome.events.is_empty() {
        return Ok(());
    }
    let record = with_cutex_session_store(|store| {
        store
            .sessions
            .values()
            .find(|record| record.cutex_session_id == outcome.cutex_session_id)
            .cloned()
    })?
    .ok_or_else(|| anyhow::anyhow!("reconciled cutex session disappeared"))?;
    let app_server_connected = endpoint_app_server_connected(
        super::app_server_runtime::runtime_manager()
            .status(&record.cutex_session_id)?
            .map(|status| status.connected),
        record.app_server_runtime.as_ref().map(|binding| binding.pid),
        cutex::platform::process::process_is_running,
    );
    management_v2_repository()?.append(PendingEvent {
        cutex_session_id: record.cutex_session_id.clone(),
        host_id: current_host_name(),
        source: EventSource::Cutex,
        schema: None,
        correlation: EventCorrelation {
            thread_id: record.codex_session_id.clone(),
            ..Default::default()
        },
        native: None,
        cutex: Some(CutexMessage {
            method: "cutex/runtime/endpointChanged".to_string(),
            params: serde_json::json!({
                "runtimeGeneration": record.runtime_generation,
                "appServerConnected": app_server_connected,
                "changedAt": timestamp,
            }),
        }),
    })?;
    Ok(())
}

/// `appServerConnected` for `cutex/runtime/endpointChanged`. Registrations
/// are usually reconciled in the Agent Bus service, a separate process whose
/// runtime manager never holds the Management server's app-server
/// connections, so "this process has no runtime" must not be reported as
/// disconnected (that made every runtime start look offline for a moment,
/// e.g. after an offline submit). Without an in-process runtime the
/// persisted binding's process liveness is the best evidence: the Management
/// server connects before the runtime registers on the bus.
fn endpoint_app_server_connected(
    in_process: Option<bool>,
    binding_pid: Option<u32>,
    process_is_running: impl Fn(u32) -> bool,
) -> bool {
    match in_process {
        Some(connected) => connected,
        None => binding_pid.is_some_and(process_is_running),
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::{
        endpoint_app_server_connected, registration_can_commit_while_session_busy,
        registration_matches_current_claude_occurrence,
        registration_matches_pending_occurrence,
    };

    #[test]
    fn endpoint_connected_prefers_in_process_status_then_binding_liveness() {
        assert!(endpoint_app_server_connected(Some(true), None, |_| false));
        assert!(!endpoint_app_server_connected(Some(false), Some(7), |_| true));
        assert!(endpoint_app_server_connected(None, Some(7), |pid| pid == 7));
        assert!(!endpoint_app_server_connected(None, Some(7), |_| false));
        assert!(!endpoint_app_server_connected(None, None, |_| true));
    }

    #[test]
    fn pending_registration_requires_the_exact_durable_occurrence_tuple() {
        use cutex::agent_bus::model::{AgentBusAgent, AgentRegistrationClass};
        use cutex::session::model::{
            CutexAppServerRuntimeBinding, CutexAppServerTransport,
            CutexSessionRecord,
        };

        let host = cutex::platform::host::current_host_name();
        let native = "018e0000-0000-7000-8000-0000b0009ae4";
        let mut record = CutexSessionRecord::new_at(
            "cutex.pending-registration".into(),
            Some(native.into()),
            host.clone(),
            "/tmp".into(),
            None,
            "2026-10-25T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_generation = 4;
        record.app_server_launch_claim_id = Some("claim-4".into());
        record.current_runtime_agent_id = Some("runtime-4".into());
        record.runtime_pid = Some(4242);
        record.registration_class = AgentRegistrationClass::Persistent;
        record.app_server_runtime = Some(CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::LoopbackWebSocket,
            endpoint: "ws://127.0.0.1:1".into(),
            pid: 4242,
            runtime_dir: "/tmp/runtime-4".into(),
            launched_profile: Some("default".into()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime-4/journal".into(),
            schema_version: "test".into(),
            schema_sha256: "a".repeat(64),
            started_at: "2026-10-25T00:00:00Z".into(),
        });
        let agent = AgentBusAgent {
            id: "runtime-4".into(),
            name: "runtime-4".into(),
            base_name: None,
            thread_name: None,
            path_key: None,
            session_id: Some(native.into()),
            cutex_session_id: None,
            profile: "default".into(),
            cwd: "/tmp".into(),
            pid: 4242,
            host_id: Some(host.clone()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 1,
        };
        assert!(registration_matches_pending_occurrence(
            &record, &agent, &host
        ));
        let mut store = cutex::session::model::CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        assert!(registration_can_commit_while_session_busy(
            &store, &agent, &host, None
        ));
        store
            .sessions
            .get_mut(&record.cutex_session_id)
            .unwrap()
            .app_server_launch_claim_id = None;
        assert!(
            !registration_can_commit_while_session_busy(&store, &agent, &host, None),
            "an ordinary exact heartbeat must defer while lifecycle owns the lane"
        );

        for changed in 0..9 {
            let mut stale_record = record.clone();
            let mut stale_agent = agent.clone();
            match changed {
                0 => stale_agent.id = "runtime-stale".into(),
                1 => stale_agent.pid += 1,
                2 => stale_agent.session_id = Some("old-native".into()),
                3 => stale_agent.host_id = Some("other-host".into()),
                4 => stale_record.app_server_launch_claim_id = None,
                5 => stale_record.app_server_runtime.as_mut().unwrap().pid += 1,
                6 => stale_agent.profile = "other-profile".into(),
                7 => stale_agent.cwd = "/other-cwd".into(),
                _ => stale_agent.registration_class = AgentRegistrationClass::Ephemeral,
            }
            assert!(
                !registration_matches_pending_occurrence(
                    &stale_record,
                    &stale_agent,
                    &host
                ),
                "changed occurrence field {changed} must fail closed"
            );
        }
    }

    #[test]
    fn busy_claude_callback_requires_the_persisted_current_occurrence() {
        use cutex::agent_bus::model::{AgentBusAgent, AgentRegistrationClass};
        use cutex::runtime::claude::{ClaudeDeployment, ClaudeSession};
        use cutex::session::model::CutexSessionRecord;

        let host = cutex::platform::host::current_host_name();
        let thread = "018a0000-0000-7000-8000-000000000001";
        let mut record = CutexSessionRecord::new_at(
            "cutex.claude-busy".into(),
            None,
            host.clone(),
            "/work".into(),
            Some("claude-profile".into()),
            "2026-10-25T00:00:00Z".into(),
        )
        .unwrap();
        record.runtime_pid = Some(4242);
        record.current_runtime_agent_id = Some("claude.connection-1".into());
        record.agent_enabled = true;
        record.registration_class = AgentRegistrationClass::Persistent;
        record.claude_session = Some(ClaudeSession {
            deployment: ClaudeDeployment {
                python: "/python".into(),
                adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(),
                claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: thread.into(),
            state_dir: "/state".into(),
            model: "sonnet".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false,
            write_explicit: false,
            exec_explicit: false,
            native_session_id: None,
            connection_id: Some("connection-1".into()),
            initial_permission_mode: None,
        });
        let agent = AgentBusAgent {
            id: "claude.connection-1".into(),
            name: "Claude".into(),
            base_name: None,
            thread_name: None,
            path_key: None,
            session_id: Some(thread.into()),
            cutex_session_id: None,
            profile: "claude-profile".into(),
            cwd: "/work".into(),
            pid: 4242,
            host_id: Some(host.clone()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 1,
        };
        assert!(registration_matches_current_claude_occurrence(
            &record, &agent, &host
        ));
        for changed in 0..7 {
            let mut stale = agent.clone();
            match changed {
                0 => stale.id = "claude.old-connection".into(),
                1 => stale.pid += 1,
                2 => stale.session_id = Some("old-thread".into()),
                3 => stale.cwd = "/other".into(),
                4 => stale.profile = "other".into(),
                5 => stale.host_id = Some("other-host".into()),
                _ => stale.registration_class = AgentRegistrationClass::Ephemeral,
            }
            assert!(
                !registration_matches_current_claude_occurrence(&record, &stale, &host),
                "changed Claude occurrence field {changed} must fail closed"
            );
        }
    }
}
