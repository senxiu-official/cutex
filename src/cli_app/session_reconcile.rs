use chrono::Utc;

use cutex::agent_bus::model::AgentBusAgent;
use cutex::im::registry::ImRegistry;
use cutex::management::v2::model::CutexMessage;
use cutex::management::v2::model::EventCorrelation;
use cutex::management::v2::model::EventSource;
use cutex::management::v2::model::PendingEvent;
use cutex::management::v2::repository::management_v2_repository;
use cutex::platform::host::current_host_name;
use cutex::session::model::CutexSessionReconcileOutcome;
use cutex::session::runtime_reconciliation::{
    reconcile_cutex_session_store_for_registration, registration_is_recent_heartbeat,
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
) -> anyhow::Result<()> {
    let timestamp = Utc::now().to_rfc3339();
    let host_id = current_host_name();
    if registration_without_store_write(agent, &host_id, &timestamp)? {
        return Ok(());
    }
    anyhow::ensure!(!agent.id.starts_with("claude."), "Claude registration has no saved session");
    let outcome = with_locked_session_rows(&cutex_sessions_path()?, |rows| {
        let reviewed = cutex::session::reviewed_registration::preserve_reviewed_groups(
            rows.committed(),
            agent,
            &host_id,
        )?;
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
            return Ok(None);
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
        Ok(reconciliation.outcome)
    })?;
    if let Some(outcome) = outcome {
        append_cutex_session_reconcile_events(&outcome, agent, &timestamp)?;
    }
    Ok(())
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
    },
    Write,
}

/// Registration refreshes arrive every 30 s per agent. Decide from the cached
/// store (no clone of the whole store, no write) whether this one can be
/// answered without the load / reconcile / save cycle: Claude registrations
/// never write, and a refresh that only moves a recent `last_seen_at` is not
/// persisted (see `registration_is_recent_heartbeat`). Returns true when the
/// registration is fully handled.
fn registration_without_store_write(
    agent: &mut AgentBusAgent,
    host_id: &str,
    timestamp: &str,
) -> anyhow::Result<bool> {
    let precheck = with_cutex_session_store(|store| -> anyhow::Result<RegistrationPrecheck> {
        if let Some(record) = store.sessions.values().find(|r| {
            r.claude_session
                .as_ref()
                .is_some_and(|c| agent.session_id.as_deref() == Some(c.thread_id.as_str()))
        }) {
            anyhow::ensure!(!record.is_retired() && (record.agent_enabled || record.is_owned_session())
                && record.current_runtime_agent_id.as_deref() == Some(agent.id.as_str())
                && record.runtime_pid == Some(agent.pid)
                && record.host_id == host_id
                && agent.host_id.as_deref() == Some(host_id)
                && cutex::platform::process::process_is_running(agent.pid),
                "Claude registration does not match its current local runtime");
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
        Ok(RegistrationPrecheck::Heartbeat {
            agent: candidate,
            reviewed,
        })
    })??;
    match precheck {
        RegistrationPrecheck::Claude {
            cutex_session_id,
            name,
            groups,
            registration_class,
        } => {
            agent.cutex_session_id = Some(cutex_session_id);
            agent.base_name = Some(name.clone());
            agent.name = name;
            agent.groups = groups;
            agent.registration_class = registration_class;
            Ok(true)
        }
        RegistrationPrecheck::Heartbeat {
            agent: candidate,
            reviewed,
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
            Ok(true)
        }
        RegistrationPrecheck::Write => Ok(false),
    }
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
    use super::endpoint_app_server_connected;

    #[test]
    fn endpoint_connected_prefers_in_process_status_then_binding_liveness() {
        assert!(endpoint_app_server_connected(Some(true), None, |_| false));
        assert!(!endpoint_app_server_connected(Some(false), Some(7), |_| true));
        assert!(endpoint_app_server_connected(None, Some(7), |pid| pid == 7));
        assert!(!endpoint_app_server_connected(None, Some(7), |_| false));
        assert!(!endpoint_app_server_connected(None, None, |_| true));
    }
}
