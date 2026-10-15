use anyhow::{anyhow, Context};

use cutex::agent_bus::client::agent_bus_fetch_agents_if_healthy;
use cutex::agent_bus::model::AgentBusAgent;
use cutex::config::store::load_codez_config;
use cutex::runtime::alden::{cute_alden_sessions, CuteAldenSession};
use cutex::session::model::{CutexSessionRecord, CutexSessionStore, CutexSessionUserAction};
use cutex::session::projection::{
    cutex_session_is_attachable, cutex_session_lifecycle_state_with_agents,
};
use cutex::session::service::cutex_session_display_name;
use cutex::session::store::load_cutex_session_store;

use super::session_tui::SessionTuiIntent;
use super::session_tui_actions::{session_tui_actions_for_record, SessionTuiAction};
use super::{root_wizard, session, session_attach};

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionTuiDispatchPlan {
    Claude { id: String, action: SessionTuiAction },
    RecoverRuntime {
        id: String,
    },
    ResumeAttach {
        id: String,
        launch_profile: Option<String>,
    },
    AttachExisting {
        key: String,
        name: String,
    },
    StockAttach {
        key: String,
        id: String,
    },
    StockRuntime {
        key: String,
        id: String,
        action: super::stock_lifecycle::ReviewedStockRuntimeAction,
    },
    RuntimePackageUpgrade {
        key: String,
        action: super::stock_lifecycle::ReviewedRuntimePackageUpgrade,
    },
    TakeoverExisting {
        key: String,
        id: String,
    },
    OpenTui {
        id: String,
        launch_profile: Option<String>,
    },
    Online {
        key: String,
        id: String,
        launch_profile: Option<String>,
    },
    ResumeHere {
        key: String,
        record: CutexSessionRecord,
    },
    ResumeManaged {
        id: String,
    },
    CloseAndRestart {
        id: String,
        launch_profile: Option<String>,
    },
    CloseRuntime {
        id: String,
    },
    RepairInterruptedHistory {
        id: String,
    },
    RetireSession {
        id: String,
    },
    RestoreSession {
        id: String,
    },
}

impl SessionTuiDispatchPlan {
    fn recorded_user_action(&self) -> Option<(&str, CutexSessionUserAction)> {
        match self {
            Self::AttachExisting { key, .. } => Some((key, CutexSessionUserAction::Attach)),
            Self::TakeoverExisting { key, .. } => Some((key, CutexSessionUserAction::Takeover)),
            Self::Online { key, .. } => Some((key, CutexSessionUserAction::Online)),
            Self::ResumeHere { key, .. } => Some((key, CutexSessionUserAction::ResumeHere)),
            Self::Claude { .. }
            | Self::RecoverRuntime { .. }
            | Self::ResumeAttach { .. }
            | Self::StockAttach { .. }
            | Self::StockRuntime { .. }
            | Self::RuntimePackageUpgrade { .. }
            | Self::OpenTui { .. }
            | Self::ResumeManaged { .. }
            | Self::CloseAndRestart { .. }
            | Self::CloseRuntime { .. }
            | Self::RepairInterruptedHistory { .. }
            | Self::RetireSession { .. }
            | Self::RestoreSession { .. } => None,
        }
    }
}

pub(super) fn dispatch_session_tui_intent(intent: SessionTuiIntent) -> anyhow::Result<()> {
    let output =
        runtime_close_output_for_surface(&intent, SessionTuiDispatchSurface::PostTerminal)?;
    dispatch_session_tui_intent_with_close_output(intent, output)
}

pub(super) fn dispatch_session_tui_intent_in_selector(
    intent: SessionTuiIntent,
) -> anyhow::Result<()> {
    let output = runtime_close_output_for_surface(&intent, SessionTuiDispatchSurface::Selector)?;
    dispatch_session_tui_intent_with_close_output(intent, output)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionTuiDispatchSurface {
    PostTerminal,
    Selector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeCloseOutput {
    Print,
    Suppress,
}

fn runtime_close_output_for_surface(
    intent: &SessionTuiIntent,
    surface: SessionTuiDispatchSurface,
) -> anyhow::Result<RuntimeCloseOutput> {
    match surface {
        SessionTuiDispatchSurface::PostTerminal => Ok(RuntimeCloseOutput::Print),
        SessionTuiDispatchSurface::Selector
            if matches!(
                intent.action,
                SessionTuiAction::CloseRuntime
                    | SessionTuiAction::UpgradeRuntimePackage
                    | SessionTuiAction::RepairInterruptedHistory
                    | SessionTuiAction::RetireSession
                    | SessionTuiAction::RestoreSession
            ) =>
        {
            Ok(RuntimeCloseOutput::Suppress)
        }
        SessionTuiDispatchSurface::Selector => {
            anyhow::bail!("only Close runtime, Repair interrupted history, Retire session, or Restore session may be dispatched inside the session selector")
        }
    }
}

fn dispatch_session_tui_intent_with_close_output(
    mut intent: SessionTuiIntent,
    close_output: RuntimeCloseOutput,
) -> anyhow::Result<()> {
    if intent.action == SessionTuiAction::StockStart && intent.stock_runtime.is_none() {
        intent.stock_runtime = Some(super::stock_lifecycle::ReviewedStockRuntimeAction::review(
            &intent.key,
            false,
        )?);
    }
    let store = load_cutex_session_store()?;
    let alden_sessions = cute_alden_sessions().unwrap_or_default();
    let config = load_codez_config();
    let live_agents = agent_bus_fetch_agents_if_healthy(&config);
    let plan = dispatch_plan_for_intent(&intent, &store, &alden_sessions, &live_agents)?;
    if !matches!(
        intent.action,
        SessionTuiAction::RetireSession | SessionTuiAction::RestoreSession
    ) {
        root_wizard::set_codez_codex_home(true)
            .context("Failed to prepare Cutex runtime environment for the selected action")?;
    }
    execute_dispatch_plan(plan, close_output)
}

fn dispatch_plan_for_intent(
    intent: &SessionTuiIntent,
    store: &CutexSessionStore,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
) -> anyhow::Result<SessionTuiDispatchPlan> {
    let record = store.sessions.get(&intent.key).cloned().ok_or_else(|| {
        anyhow!(
            "selected Cutex session is no longer available: {}",
            intent.key
        )
    })?;
    if record.is_retired() && intent.action != SessionTuiAction::RestoreSession {
        anyhow::bail!(
            "selected Cutex session is retired: {}; refresh the session list before acting",
            intent.key
        );
    }
    let available = session_tui_actions_for_record(&record, alden_sessions, live_agents);
    if record.runtime_backend == cutex::session::model::CutexSessionRuntimeBackend::ClaudeSubscription {
        anyhow::ensure!(available.iter().any(|item| item.action == intent.action), "action not supported by the Claude backend");
        anyhow::ensure!(intent.launch_profile.is_none() && intent.stock_runtime.is_none(), "Claude uses its saved subscription settings, not a Codex profile");
        return Ok(SessionTuiDispatchPlan::Claude { id: record.cutex_session_id, action: intent.action });
    }
    if intent.action == SessionTuiAction::RestoreSession {
        if !record.is_retired() {
            anyhow::bail!(
                "selected Cutex session is already active; refresh the archive workspace"
            );
        }
        return Ok(SessionTuiDispatchPlan::RestoreSession {
            id: record.cutex_session_id,
        });
    }
    if intent.action == SessionTuiAction::RetireSession
        && !cutex::session::service::cutex_session_is_managed(&record)
    {
        anyhow::bail!("Retire session is only available for managed Cutex sessions");
    }
    if intent.action != SessionTuiAction::RetireSession
        && !available.iter().any(|item| item.action == intent.action)
    {
        anyhow::bail!(
            "{} is no longer available for {}; reopen `cutex tui` to refresh actions",
            intent.action.label(),
            cutex_session_display_name(&record)
        );
    }
    if intent.launch_profile.is_some()
        && !intent.action.supports_launch_profile(
            cutex_session_lifecycle_state_with_agents(&record, alden_sessions, live_agents),
            cutex_session_is_attachable(&record, alden_sessions),
        )
    {
        anyhow::bail!(
            "{} can no longer apply a one-launch profile for {}",
            intent.action.label(),
            cutex_session_display_name(&record)
        );
    }
    let reviewed_stock = if matches!(
        intent.action,
        SessionTuiAction::StockStart | SessionTuiAction::StockRestart
    ) {
        let reviewed = intent
            .stock_runtime
            .clone()
            .context("reviewed stock runtime action missing; review again")?;
        anyhow::ensure!(
            reviewed.review.subject.cutex_session_id.as_str() == record.cutex_session_id
                && reviewed.review.contract
                    == record.explicit_launch.clone().context(
                        "selected Agent no longer has an explicit stock launch contract"
                    )?
                && reviewed.review.restart == (intent.action == SessionTuiAction::StockRestart),
            "reviewed stock runtime target or operation changed; review again"
        );
        Some(reviewed)
    } else {
        anyhow::ensure!(
            intent.stock_runtime.is_none(),
            "reviewed stock runtime payload supplied for a different action"
        );
        None
    };
    let reviewed_package = if intent.action == SessionTuiAction::UpgradeRuntimePackage {
        let reviewed = intent
            .runtime_package
            .clone()
            .context("reviewed runtime package action missing; review again")?;
        anyhow::ensure!(
            reviewed.review.subject.cutex_session_id.as_str() == record.cutex_session_id,
            "reviewed runtime package target changed; review again"
        );
        Some(reviewed)
    } else {
        anyhow::ensure!(
            intent.runtime_package.is_none(),
            "reviewed runtime package payload supplied for a different action"
        );
        None
    };

    let id = record
        .codex_session_id
        .as_deref()
        .unwrap_or(record.cutex_session_id.as_str())
        .to_string();
    let plan = match intent.action {
        SessionTuiAction::RevertHistory => anyhow::bail!("open history editor from the Cutex TUI"),
        SessionTuiAction::RecoverRuntime => SessionTuiDispatchPlan::RecoverRuntime {
            id: record.cutex_session_id.clone(),
        },
        SessionTuiAction::ResumeAttach => SessionTuiDispatchPlan::ResumeAttach {
            id,
            launch_profile: intent.launch_profile.clone(),
        },
        SessionTuiAction::AttachExisting => {
            let name = record
                .alden_session_name
                .clone()
                .context("selected Cutex session has no cute-alden session name")?;
            SessionTuiDispatchPlan::AttachExisting {
                key: intent.key.clone(),
                name,
            }
        }
        SessionTuiAction::StockAttach => SessionTuiDispatchPlan::StockAttach {
            key: intent.key.clone(),
            id: record.cutex_session_id.clone(),
        },
        SessionTuiAction::StockStart | SessionTuiAction::StockRestart => {
            SessionTuiDispatchPlan::StockRuntime {
                key: intent.key.clone(),
                id: record.cutex_session_id.clone(),
                action: reviewed_stock.expect("validated reviewed stock action"),
            }
        }
        SessionTuiAction::UpgradeRuntimePackage => {
            SessionTuiDispatchPlan::RuntimePackageUpgrade {
                key: intent.key.clone(),
                action: reviewed_package.expect("validated reviewed package action"),
            }
        }
        SessionTuiAction::TakeoverExisting => SessionTuiDispatchPlan::TakeoverExisting {
            key: intent.key.clone(),
            id,
        },
        SessionTuiAction::OpenTui => SessionTuiDispatchPlan::OpenTui {
            id,
            launch_profile: intent.launch_profile.clone(),
        },
        SessionTuiAction::Online => SessionTuiDispatchPlan::Online {
            key: intent.key.clone(),
            id,
            launch_profile: intent.launch_profile.clone(),
        },
        SessionTuiAction::ResumeHere => SessionTuiDispatchPlan::ResumeHere {
            key: intent.key.clone(),
            record,
        },
        SessionTuiAction::ResumeManaged => SessionTuiDispatchPlan::ResumeManaged { id },
        SessionTuiAction::CloseAndRestart => SessionTuiDispatchPlan::CloseAndRestart {
            id,
            launch_profile: intent.launch_profile.clone(),
        },
        SessionTuiAction::CloseRuntime => SessionTuiDispatchPlan::CloseRuntime { id },
        SessionTuiAction::RepairInterruptedHistory => {
            SessionTuiDispatchPlan::RepairInterruptedHistory { id }
        }
        SessionTuiAction::RetireSession => SessionTuiDispatchPlan::RetireSession {
            id: record.cutex_session_id,
        },
        SessionTuiAction::RestoreSession => unreachable!("handled above"),
    };
    Ok(plan)
}

fn execute_dispatch_plan(
    plan: SessionTuiDispatchPlan,
    close_output: RuntimeCloseOutput,
) -> anyhow::Result<()> {
    if let Some((key, action)) = plan.recorded_user_action() {
        session::record_cutex_session_user_action(key, action)?;
    }

    match plan {
        SessionTuiDispatchPlan::Claude { id, action } => {
            use cutex::runtime::claude;
            match action {
                SessionTuiAction::Online => { super::claude_runtime::lifecycle(&id, true)?; Ok(()) }
                SessionTuiAction::OpenTui => { super::claude_runtime::lifecycle(&id, true)?; claude::attach(&id) }
                SessionTuiAction::CloseRuntime => { super::claude_runtime::lifecycle(&id, false)?; Ok(()) }
                _ => anyhow::bail!("unsupported Claude action"),
            }
        }
        SessionTuiDispatchPlan::RecoverRuntime { id } => {
            super::human::recover(&id, None).map(|_| ())
        }
        SessionTuiDispatchPlan::ResumeAttach { id, launch_profile } => {
            session::cmd_session_resume_alden_with_profile(&id, launch_profile.as_deref())
        }
        SessionTuiDispatchPlan::AttachExisting { name, .. } => {
            session_attach::cmd_session_attach(&name, false)
        }
        SessionTuiDispatchPlan::StockAttach { key, id } => {
            super::stock_lifecycle::attach(&id)?;
            session::record_cutex_session_user_action(&key, CutexSessionUserAction::Attach)
        }
        SessionTuiDispatchPlan::StockRuntime { key, id, action } => {
            let receipt = action.execute()?;
            anyhow::ensure!(
                receipt.stage == cutex::agent_management::StockRuntimeStage::Ready
                    && receipt.error.is_none()
                    && receipt.binding.is_some(),
                "Runtime startup incomplete ({:?}): {}. Resume this action with: cutex human action {} --resume",
                receipt.stage,
                receipt.error.as_deref().unwrap_or("runtime binding is missing"),
                receipt.action_id
            );
            session::record_cutex_session_user_action(&key, CutexSessionUserAction::Online)?;
            super::stock_lifecycle::attach(&id)
                .context("stock runtime is Ready, but foreground attach failed")?;
            session::record_cutex_session_user_action(&key, CutexSessionUserAction::Attach)
        }
        SessionTuiDispatchPlan::RuntimePackageUpgrade { key: _, action } => {
            let receipt = action.execute()?;
            if close_output == RuntimeCloseOutput::Print {
                println!(
                    "Runtime package upgraded to {} (action {}); agent remains offline",
                    receipt
                        .review
                        .target_runtime_version
                        .as_deref()
                        .unwrap_or("selected package"),
                    receipt.action_id
                );
            }
            Ok(())
        }
        SessionTuiDispatchPlan::TakeoverExisting { id, .. } => session::cmd_session_takeover(&id),
        SessionTuiDispatchPlan::OpenTui { id, launch_profile } => {
            session::cmd_session_foreground_with_profile(&id, launch_profile.as_deref())
        }
        SessionTuiDispatchPlan::Online {
            id, launch_profile, ..
        } => session::cmd_session_online_with_profile(&id, launch_profile.as_deref(), true)
            .map(|_| ()),
        SessionTuiDispatchPlan::ResumeManaged { id } => session::cmd_session_foreground(&id),
        SessionTuiDispatchPlan::ResumeHere { record, .. } => {
            session::cmd_session_resume_foreground(&record, None)
        }
        SessionTuiDispatchPlan::CloseAndRestart { id, launch_profile } => {
            session::cmd_session_close_and_restart_with_profile(
                &id,
                launch_profile.as_deref(),
                true,
            )
            .map(|_| ())
        }
        SessionTuiDispatchPlan::CloseRuntime { id } => match close_output {
            RuntimeCloseOutput::Print => session::cmd_session_close_and_wait(&id).map(|_| ()),
            RuntimeCloseOutput::Suppress => {
                session::cmd_session_close_and_wait_quiet(&id).map(|_| ())
            }
        },
        SessionTuiDispatchPlan::RepairInterruptedHistory { id } => {
            session::repair_interrupted_history(&id).map(|_| ())
        }
        SessionTuiDispatchPlan::RetireSession { .. }
        | SessionTuiDispatchPlan::RestoreSession { .. } => {
            anyhow::bail!(
                "Archive/Restore requires the original typed Human confirmation; review again"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_actions_do_not_enter_stock_review_or_codex_resume() {
        let mut record = record(CutexSessionRuntimeBackend::ClaudeSubscription);
        record.codex_session_id = None;
        let store = store_with(record);
        for action in [SessionTuiAction::OpenTui, SessionTuiAction::Online, SessionTuiAction::CloseRuntime] {
            let plan = dispatch_plan_for_intent(&intent(action), &store, &[], &[]).unwrap();
            assert_eq!(plan, SessionTuiDispatchPlan::Claude { id: "cutex.dispatch".into(), action });
        }
        assert!(dispatch_plan_for_intent(&intent(SessionTuiAction::StockStart), &store, &[], &[]).is_err());
        let mut with_profile = intent(SessionTuiAction::OpenTui);
        with_profile.launch_profile = Some("orchid".into());
        assert!(dispatch_plan_for_intent(&with_profile, &store, &[], &[]).is_err());
    }

    use cutex::agent_bus::model::AgentRegistrationClass;
    use cutex::session::model::{
        CutexAppServerRuntimeBinding, CutexAppServerTransport, CutexSessionRuntimeBackend,
    };

    fn record(backend: CutexSessionRuntimeBackend) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.dispatch".to_string(),
            Some("019e-dispatch".to_string()),
            "devbox".to_string(),
            "/tmp/dispatch".to_string(),
            Some("orchid".to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("dispatch-agent".to_string());
        record.runtime_backend = backend;
        record.registration_class = AgentRegistrationClass::Persistent;
        record
    }

    fn store_with(record: CutexSessionRecord) -> CutexSessionStore {
        let mut store = CutexSessionStore::default();
        store.sessions.insert("durable-key".to_string(), record);
        store
    }

    fn intent(action: SessionTuiAction) -> SessionTuiIntent {
        SessionTuiIntent {
            key: "durable-key".to_string(),
            action,
            launch_profile: None,
            stock_runtime: None,
            runtime_package: None,
        }
    }

    fn intent_with_profile(action: SessionTuiAction, profile: &str) -> SessionTuiIntent {
        SessionTuiIntent {
            key: "durable-key".to_string(),
            action,
            launch_profile: Some(profile.to_string()),
            stock_runtime: None,
            runtime_package: None,
        }
    }

    fn mark_explicit_stock(record: &mut CutexSessionRecord) {
        record.explicit_launch = Some(cutex::agent_management::ExplicitLaunchContract {
            version: 1,
            migration_action_id: None,
            native_id: "019e-dispatch".to_string(),
            native_home: "/tmp/stock-home".into(),
            bundle_manifest: "/tmp/stock-bundle.json".into(),
            bundle_sha256: cutex::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
    }

    fn reviewed_stock(
        record: &CutexSessionRecord,
        restart: bool,
    ) -> super::super::stock_lifecycle::ReviewedStockRuntimeAction {
        super::super::stock_lifecycle::ReviewedStockRuntimeAction {
            action_id: cutex::agent_management::AgentActionId::new("tui-stock-test").unwrap(),
            review: serde_json::from_value(serde_json::json!({
                "digest_version": 2,
                "subject": {
                    "cutex_session_id": record.cutex_session_id,
                    "formal_name": "dispatch-agent",
                    "durable_sha256": "b".repeat(64),
                    "authority_sha256": "c".repeat(64),
                    "current_project_id": null,
                    "revision": record.revision,
                    "runtime_generation": record.runtime_generation
                },
                "contract": record.explicit_launch,
                "configuration": {
                    "profile_name": "orchid",
                    "profile_id": "test-profile",
                    "inherited": false,
                    "profile_sha256": "d".repeat(64),
                    "account_sha256": "e".repeat(64),
                    "model": "test-model",
                    "reasoning": null,
                    "model_provider": "test",
                    "provider": {
                        "name": "test",
                        "base_url": "http://127.0.0.1:1/v1",
                        "wire_api": "responses",
                        "requires_openai_auth": false,
                        "supports_websockets": false
                    },
                    "sandbox": "danger-full-access",
                    "approval": "never"
                },
                "restart": restart
            }))
            .unwrap(),
        }
    }

    #[test]
    fn offline_explicit_stock_dispatch_requires_and_preserves_exact_review() {
        let mut record = record(CutexSessionRuntimeBackend::Host);
        mark_explicit_stock(&mut record);
        let action = reviewed_stock(&record, false);
        let store = store_with(record);
        let mut start = intent(SessionTuiAction::StockStart);
        assert!(dispatch_plan_for_intent(&start, &store, &[], &[]).is_err());
        start.stock_runtime = Some(action.clone());

        assert_eq!(
            dispatch_plan_for_intent(&start, &store, &[], &[]).unwrap(),
            SessionTuiDispatchPlan::StockRuntime {
                key: "durable-key".to_string(),
                id: "cutex.dispatch".to_string(),
                action,
            }
        );
    }

    #[test]
    fn explicit_stock_dispatch_uses_durable_id_and_rejects_generic_stale_intents() {
        let mut record = record(CutexSessionRuntimeBackend::Host);
        mark_explicit_stock(&mut record);
        record.current_runtime_agent_id = Some("stock.dispatch.runtime".to_string());
        record.app_server_runtime = Some(CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/stock.sock".to_string(),
            pid: std::process::id(),
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: Some("orchid".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-08T00:00:00Z".to_string(),
        });
        let live_agents = vec![AgentBusAgent {
            id: "stock.dispatch.runtime".to_string(),
            name: "stock-dispatch".to_string(),
            base_name: Some("stock-dispatch".to_string()),
            thread_name: None,
            path_key: None,
            session_id: record.codex_session_id.clone(),
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: record.cwd.clone(),
            pid: std::process::id(),
            host_id: Some(cutex::platform::host::current_host_name()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 42,
        }];
        let store = store_with(record);

        assert_eq!(
            dispatch_plan_for_intent(
                &intent(SessionTuiAction::StockAttach),
                &store,
                &[],
                &live_agents,
            )
            .expect("stock attach plan"),
            SessionTuiDispatchPlan::StockAttach {
                key: "durable-key".to_string(),
                id: "cutex.dispatch".to_string(),
            }
        );
        assert!(dispatch_plan_for_intent(
            &intent(SessionTuiAction::Online),
            &store,
            &[],
            &live_agents,
        )
        .is_err());
        assert!(dispatch_plan_for_intent(
            &intent(SessionTuiAction::RepairInterruptedHistory),
            &store,
            &[],
            &live_agents,
        )
        .is_err());
    }

    #[test]
    fn live_alden_intents_map_to_existing_attach_takeover_and_lifecycle_routes() {
        let mut record = record(CutexSessionRuntimeBackend::CuteAlden);
        record.alden_session_name = Some("cutex.dispatch.runtime".to_string());
        record.alden_pid = Some(std::process::id());
        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: record.alden_session_name.clone(),
        }];
        let store = store_with(record);

        assert_eq!(
            dispatch_plan_for_intent(
                &intent(SessionTuiAction::ResumeAttach),
                &store,
                &alden_sessions,
                &[],
            )
            .expect("resume-attach plan"),
            SessionTuiDispatchPlan::ResumeAttach {
                id: "019e-dispatch".to_string(),
                launch_profile: None,
            }
        );
        assert_eq!(
            dispatch_plan_for_intent(
                &intent(SessionTuiAction::AttachExisting),
                &store,
                &alden_sessions,
                &[],
            )
            .expect("attach plan"),
            SessionTuiDispatchPlan::AttachExisting {
                key: "durable-key".to_string(),
                name: "cutex.dispatch.runtime".to_string(),
            }
        );
        assert_eq!(
            dispatch_plan_for_intent(
                &intent(SessionTuiAction::TakeoverExisting),
                &store,
                &alden_sessions,
                &[],
            )
            .expect("takeover plan"),
            SessionTuiDispatchPlan::TakeoverExisting {
                key: "durable-key".to_string(),
                id: "019e-dispatch".to_string(),
            }
        );
        assert!(matches!(
            dispatch_plan_for_intent(
                &intent(SessionTuiAction::Online),
                &store,
                &alden_sessions,
                &[],
            )
            .expect("online plan"),
            SessionTuiDispatchPlan::Online { .. }
        ));
        assert!(matches!(
            dispatch_plan_for_intent(
                &intent_with_profile(SessionTuiAction::CloseAndRestart, "beta"),
                &store,
                &alden_sessions,
                &[],
            )
            .expect("close-and-restart plan"),
            SessionTuiDispatchPlan::CloseAndRestart {
                launch_profile: Some(ref profile),
                ..
            } if profile == "beta"
        ));
        assert!(matches!(
            dispatch_plan_for_intent(
                &intent(SessionTuiAction::CloseRuntime),
                &store,
                &alden_sessions,
                &[],
            )
            .expect("close plan"),
            SessionTuiDispatchPlan::CloseRuntime { .. }
        ));
    }

    #[test]
    fn managed_host_intents_keep_current_and_managed_cwd_routes_distinct() {
        let record = record(CutexSessionRuntimeBackend::Host);
        let store = store_with(record.clone());

        let online = dispatch_plan_for_intent(&intent(SessionTuiAction::Online), &store, &[], &[])
            .expect("online plan");
        assert_eq!(
            online.recorded_user_action(),
            Some(("durable-key", CutexSessionUserAction::Online))
        );

        let resume_here =
            dispatch_plan_for_intent(&intent(SessionTuiAction::ResumeHere), &store, &[], &[])
                .expect("resume-here plan");
        assert_eq!(
            resume_here,
            SessionTuiDispatchPlan::ResumeHere {
                key: "durable-key".to_string(),
                record,
            }
        );
        assert_eq!(
            resume_here.recorded_user_action(),
            Some(("durable-key", CutexSessionUserAction::ResumeHere))
        );

        assert_eq!(
            dispatch_plan_for_intent(&intent(SessionTuiAction::ResumeManaged), &store, &[], &[],)
                .expect("resume-managed plan"),
            SessionTuiDispatchPlan::ResumeManaged {
                id: "019e-dispatch".to_string(),
            }
        );
    }

    #[test]
    fn host_foreground_intent_maps_to_open_tui_and_never_takeover() {
        let record = record(CutexSessionRuntimeBackend::HostForeground);
        let store = store_with(record);

        assert_eq!(
            dispatch_plan_for_intent(&intent(SessionTuiAction::OpenTui), &store, &[], &[],)
                .expect("open-TUI plan"),
            SessionTuiDispatchPlan::OpenTui {
                id: "019e-dispatch".to_string(),
                launch_profile: None,
            }
        );
        assert!(dispatch_plan_for_intent(
            &intent(SessionTuiAction::TakeoverExisting),
            &store,
            &[],
            &[],
        )
        .is_err());
    }

    #[test]
    fn detached_alden_intent_maps_open_tui_to_the_existing_restore_route() {
        let mut record = record(CutexSessionRuntimeBackend::CuteAlden);
        record.current_runtime_agent_id = Some("cutex.dispatch.runtime".to_string());
        record.app_server_runtime = Some(CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/app.sock".to_string(),
            pid: std::process::id(),
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: Some("orchid".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-08T00:00:00Z".to_string(),
        });
        let live_agents = vec![AgentBusAgent {
            id: "cutex.dispatch.runtime".to_string(),
            name: "dispatch.runtime".to_string(),
            base_name: Some("dispatch".to_string()),
            thread_name: None,
            path_key: None,
            session_id: record.codex_session_id.clone(),
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: record.cwd.clone(),
            pid: std::process::id(),
            host_id: Some(cutex::platform::host::current_host_name()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 42,
        }];
        let store = store_with(record);

        assert_eq!(
            dispatch_plan_for_intent(
                &intent(SessionTuiAction::OpenTui),
                &store,
                &[],
                &live_agents,
            )
            .expect("detached TUI restore plan"),
            SessionTuiDispatchPlan::OpenTui {
                id: "019e-dispatch".to_string(),
                launch_profile: None,
            }
        );
    }

    #[test]
    fn stale_attach_intent_is_rejected_before_user_action_recording() {
        let mut record = record(CutexSessionRuntimeBackend::CuteAlden);
        record.alden_session_name = Some("cutex.dispatch.runtime".to_string());
        let store = store_with(record);

        let error =
            dispatch_plan_for_intent(&intent(SessionTuiAction::AttachExisting), &store, &[], &[])
                .expect_err("stale attach must fail");

        assert!(error.to_string().contains("no longer available"));
    }

    #[test]
    fn stale_intent_for_retired_record_is_rejected_before_dispatch() {
        let mut record = record(CutexSessionRuntimeBackend::Host);
        record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        record.current_runtime_agent_id = Some("stale-runtime".to_string());
        let store = store_with(record);

        let error =
            dispatch_plan_for_intent(&intent(SessionTuiAction::CloseRuntime), &store, &[], &[])
                .expect_err("retired session must reject stale action");

        assert!(error.to_string().contains("is retired"));
    }

    #[test]
    fn archive_intents_use_the_existing_transaction_without_launch_routes() {
        let active = record(CutexSessionRuntimeBackend::Host);
        let retire = dispatch_plan_for_intent(
            &intent(SessionTuiAction::RetireSession),
            &store_with(active),
            &[],
            &[],
        )
        .expect("retire plan");
        assert_eq!(
            retire,
            SessionTuiDispatchPlan::RetireSession {
                id: "cutex.dispatch".to_string(),
            }
        );

        let mut retired = record(CutexSessionRuntimeBackend::Host);
        retired.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
        retired.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        let restore = dispatch_plan_for_intent(
            &intent(SessionTuiAction::RestoreSession),
            &store_with(retired),
            &[],
            &[],
        )
        .expect("restore plan");
        assert_eq!(
            restore,
            SessionTuiDispatchPlan::RestoreSession {
                id: "cutex.dispatch".to_string(),
            }
        );
    }

    #[test]
    fn graceful_close_plan_has_no_incompatible_user_action_record() {
        let mut record = record(CutexSessionRuntimeBackend::Host);
        record.current_runtime_agent_id = Some("volatile-runtime".to_string());
        let store = store_with(record);

        let close =
            dispatch_plan_for_intent(&intent(SessionTuiAction::CloseRuntime), &store, &[], &[])
                .expect("close plan");

        assert_eq!(
            close,
            SessionTuiDispatchPlan::CloseRuntime {
                id: "019e-dispatch".to_string(),
            }
        );
        assert_eq!(close.recorded_user_action(), None);

        let restart = dispatch_plan_for_intent(
            &intent_with_profile(SessionTuiAction::CloseAndRestart, "beta"),
            &store,
            &[],
            &[],
        )
        .expect("restart plan");
        assert_eq!(
            restart,
            SessionTuiDispatchPlan::CloseAndRestart {
                id: "019e-dispatch".to_string(),
                launch_profile: Some("beta".to_string()),
            }
        );
        assert_eq!(restart.recorded_user_action(), None);
    }

    #[test]
    fn offline_history_repair_uses_the_explicit_non_launching_plan() {
        let store = store_with(record(CutexSessionRuntimeBackend::Host));
        let repair = dispatch_plan_for_intent(
            &intent(SessionTuiAction::RepairInterruptedHistory),
            &store,
            &[],
            &[],
        )
        .expect("history repair plan");

        assert_eq!(
            repair,
            SessionTuiDispatchPlan::RepairInterruptedHistory {
                id: "019e-dispatch".to_string(),
            }
        );
        assert_eq!(repair.recorded_user_action(), None);
    }

    #[test]
    fn selector_dispatch_suppresses_output_for_selector_owned_operations() {
        let close = intent(SessionTuiAction::CloseRuntime);
        assert_eq!(
            runtime_close_output_for_surface(&close, SessionTuiDispatchSurface::PostTerminal)
                .expect("post-terminal close output"),
            RuntimeCloseOutput::Print
        );
        assert_eq!(
            runtime_close_output_for_surface(&close, SessionTuiDispatchSurface::Selector)
                .expect("selector close output"),
            RuntimeCloseOutput::Suppress
        );

        let upgrade = intent(SessionTuiAction::UpgradeRuntimePackage);
        assert_eq!(
            runtime_close_output_for_surface(&upgrade, SessionTuiDispatchSurface::Selector)
                .expect("selector runtime package output"),
            RuntimeCloseOutput::Suppress
        );

        let repair = intent(SessionTuiAction::RepairInterruptedHistory);
        assert_eq!(
            runtime_close_output_for_surface(&repair, SessionTuiDispatchSurface::Selector)
                .expect("selector repair output"),
            RuntimeCloseOutput::Suppress
        );

        let restart = intent(SessionTuiAction::CloseAndRestart);
        assert_eq!(
            runtime_close_output_for_surface(&restart, SessionTuiDispatchSurface::PostTerminal)
                .expect("post-terminal restart output"),
            RuntimeCloseOutput::Print
        );
        assert!(
            runtime_close_output_for_surface(&restart, SessionTuiDispatchSurface::Selector)
                .is_err()
        );
    }

    #[test]
    fn one_launch_profile_is_carried_only_when_fresh_state_can_start_a_process() {
        let offline = store_with(record(CutexSessionRuntimeBackend::CuteAlden));
        assert_eq!(
            dispatch_plan_for_intent(
                &intent_with_profile(SessionTuiAction::ResumeAttach, "beta"),
                &offline,
                &[],
                &[],
            )
            .expect("offline Alden launch plan"),
            SessionTuiDispatchPlan::ResumeAttach {
                id: "019e-dispatch".to_string(),
                launch_profile: Some("beta".to_string()),
            }
        );

        let mut live_record = record(CutexSessionRuntimeBackend::CuteAlden);
        live_record.alden_session_name = Some("cutex.dispatch.runtime".to_string());
        live_record.alden_pid = Some(std::process::id());
        let live_session = CuteAldenSession {
            pid: std::process::id(),
            name: live_record.alden_session_name.clone(),
        };
        let live = store_with(live_record);
        let error = dispatch_plan_for_intent(
            &intent_with_profile(SessionTuiAction::ResumeAttach, "beta"),
            &live,
            &[live_session],
            &[],
        )
        .expect_err("existing Alden process must reject profile override");
        assert!(error.to_string().contains("can no longer apply"));
    }
}
