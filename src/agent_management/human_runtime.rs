//! Human recovery of interrupted runtime actions, independent of migration plans.
use super::*;
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::role_revision::CutexSessionId;
use crate::session::store::{
    load_cutex_session_store_from_path, save_locked_session_store, with_locked_session_store,
};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HumanRuntimeRecovery {
    pub action_id: AgentActionId,
    pub cutex_session_id: CutexSessionId,
    pub original_action_id: AgentActionId,
    pub claim_id: String,
    pub completed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeActionStatus {
    pub action_id: AgentActionId,
    pub state: String,
    pub receipt: Option<StockRuntimeReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<HumanRuntimeRecovery>,
}

fn recovery_path(path: &Path, action: &AgentActionId) -> anyhow::Result<PathBuf> {
    Ok(path
        .parent()
        .context("session store parent missing")?
        .join("runtime/human-actions")
        .join(format!(
            "{}.json",
            super::store::request_sha256(action)?.as_str()
        )))
}

impl AgentManagementProvider {
    pub fn runtime_action_status(
        &self,
        path: &Path,
        action: &AgentActionId,
    ) -> anyhow::Result<RuntimeActionStatus> {
        match std::fs::read(recovery_path(path, action)?) {
            Ok(bytes) => {
                let recovery: HumanRuntimeRecovery = serde_json::from_slice(&bytes)?;
                return Ok(RuntimeActionStatus {
                    action_id: action.clone(),
                    state: if recovery.completed {
                        "succeeded"
                    } else {
                        "in_progress"
                    }
                    .into(),
                    receipt: None,
                    recovery: Some(recovery),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let sessions = load_cutex_session_store_from_path(path)?;
        let receipt = match crate::session::receipt_retention::explicit_launch_receipt(&sessions, action.as_str())? {
            Some(ExplicitLaunchActionReceipt::Runtime(r)) => Some(r.clone()),
            Some(_) => anyhow::bail!("action is not a runtime start; use its specific status"),
            None => None,
        };
        let state = match &receipt {
            None => "not_started",
            Some(r) if r.stage == StockRuntimeStage::Ready => "succeeded",
            Some(r) if r.error.is_some() => "failed",
            Some(_) => "in_progress",
        };
        Ok(RuntimeActionStatus {
            action_id: action.clone(),
            state: state.into(),
            receipt,
            recovery: None,
        })
    }

    pub fn recover_failed_runtime(
        &self,
        _principal: &HumanManagementPrincipal,
        path: &Path,
        id: &CutexSessionId,
        action: &AgentActionId,
        runtime: &mut dyn StockRuntimeExecutor,
    ) -> anyhow::Result<HumanRuntimeRecovery> {
        let _mutation = self.store().lock_mutations()?;
        let journal = recovery_path(path, action)?;
        let previous: Option<HumanRuntimeRecovery> = match std::fs::read(&journal) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(previous) = &previous {
            ensure!(
                &previous.cutex_session_id == id && &previous.action_id == action,
                "recovery action conflicts with another request"
            );
            if previous.completed {
                return Ok(previous.clone());
            }
        }
        with_locked_session_store(path, |sessions| {
            let record = sessions
                .sessions
                .get(id.as_str())
                .context("agent not found")?;
            // A crash after the atomic session update leaves a prepared journal.
            // Finishing that journal must never affect a subsequent runtime.
            if let Some(previous) = &previous {
                if record.app_server_launch_claim_id.as_deref() != Some(&previous.claim_id) {
                    let mut done = previous.clone();
                    done.completed = true;
                    write_private_pretty_json_atomic(&journal, &done, "human runtime recovery")?;
                    return Ok(done);
                }
            }
            let claim = record
                .app_server_launch_claim_id
                .as_deref()
                .context("no interrupted start claim; refresh status or attach")?;
            let receipt = sessions
                .explicit_launch_receipts
                .values()
                .find_map(|r| match r {
                    ExplicitLaunchActionReceipt::Runtime(r)
                        if r.claim_id == claim && &r.review.subject.cutex_session_id == id =>
                    {
                        Some(r.clone())
                    }
                    _ => None,
                })
                .context("interrupted start receipt missing; inspect before repair")?;
            ensure!(
                receipt.stage != StockRuntimeStage::Ready,
                "runtime reached Ready; attach or stop it instead"
            );
            ensure!(
                record.runtime_pid.is_none()
                    && record.app_server_runtime.is_none()
                    && record.current_runtime_agent_id.is_none(),
                "agent has a recorded runtime; stop or reconcile that runtime first"
            );
            ensure!(
                receipt.publication.is_some(),
                "publication evidence missing; cannot determine child absence"
            );
            // Keep the original creator/child lease until the state update is durable.
            runtime.publication(&receipt)?;
            if receipt.binding.is_some() {
                ensure!(
                    runtime.published_owner_absent(&receipt)?,
                    "original runtime still exists; attach or stop it instead"
                );
            }
            let mut recovery = HumanRuntimeRecovery {
                action_id: action.clone(),
                cutex_session_id: id.clone(),
                original_action_id: receipt.action_id.clone(),
                claim_id: claim.into(),
                completed: false,
            };
            write_private_pretty_json_atomic(&journal, &recovery, "human runtime recovery")?;
            let record = sessions.sessions.get_mut(id.as_str()).unwrap();
            record.app_server_launch_claim_id = None;
            record.updated_at = chrono::Utc::now().to_rfc3339();
            // Keep the original failure receipt, generation, identity and history intact.
            save_locked_session_store(path, sessions)?;
            recovery.completed = true;
            write_private_pretty_json_atomic(&journal, &recovery, "human runtime recovery")?;
            Ok(recovery)
        })
    }
}

/// Liveness probe for an unresolved launch claim. Returns `Ok(true)` when the
/// claimed creator or child may still be running (the claim then protects a
/// real launch), `Ok(false)` when both are proven gone, and an error when the
/// evidence is ambiguous.
pub type LaunchClaimOwnerProbe<'a> = dyn FnMut(&crate::session::model::CutexSessionRecord, Option<&StockRuntimeReceipt>) -> anyhow::Result<bool>
    + 'a;

/// Stops one recorded runtime occurrence (the executor's exact stop: PID +
/// start time verified, then the runtime fields are cleared) for a launch
/// whose readiness already failed. Production: `native_stop::stop_and_commit`.
pub type WedgedLaunchStop<'a> = dyn FnMut(&crate::session::model::CutexSessionRecord) -> anyhow::Result<()> + 'a;

/// A launch whose creator already gave up: its child was spawned and
/// committed (stage Spawned), readiness failed (the receipt carries the
/// error) and nothing will ever finish it. With the provider execution lock
/// held no launch can be running, so a live child is abandoned, not in
/// progress (2026-10-13: such a child blocked every later start forever).
fn wedged_after_readiness(
    record: &crate::session::model::CutexSessionRecord,
    receipt: &StockRuntimeReceipt,
) -> bool {
    receipt.stage == StockRuntimeStage::Spawned
        && receipt.error.is_some()
        && receipt.binding.is_some()
        && record.app_server_runtime == receipt.binding
}

enum ClaimCleanup {
    Nothing,
    Cleared,
    StopWedged(crate::session::model::CutexSessionRecord, AgentActionId),
}

impl AgentManagementProvider {
    /// Clear a launch claim (and stale `Prepared` start intents) left behind
    /// by a crashed launch, so the owner never has to replay or repair it by
    /// hand. Holds the provider execution and mutation locks, so no launch in
    /// this process can be in flight; `owner_alive` covers a spawned child or a
    /// creator elsewhere. A live owner keeps its claim (`launch_in_progress`),
    /// except a child whose launch already failed readiness: `stop_wedged`
    /// stops it, then the claim is released and the action Abandoned.
    /// Returns whether anything was cleared.
    pub fn clear_dead_launch_claim(
        &self,
        path: &Path,
        id: &CutexSessionId,
        owner_alive: &mut LaunchClaimOwnerProbe<'_>,
        stop_wedged: &mut WedgedLaunchStop<'_>,
    ) -> anyhow::Result<bool> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| anyhow::anyhow!("stock execution lock unavailable"))?;
        let _mutation = self.store().lock_mutations()?;
        let outcome = with_locked_session_store(path, |sessions| {
            let Some(record) = sessions.sessions.get(id.as_str()).cloned() else {
                return Ok(ClaimCleanup::Nothing);
            };
            let claim = record.app_server_launch_claim_id.clone();
            let claimed = claim.as_deref().and_then(|claim| {
                sessions
                    .explicit_launch_receipts
                    .values()
                    .find_map(|r| match r {
                        ExplicitLaunchActionReceipt::Runtime(r)
                            if r.claim_id == claim && &r.review.subject.cutex_session_id == id =>
                        {
                            Some(r.clone())
                        }
                        _ => None,
                    })
            });
            // A Prepared intent without a claim never spawned anything; with
            // the execution lock held it cannot be in progress.
            let stale_prepared: Vec<String> = sessions
                .explicit_launch_receipts
                .iter()
                .filter_map(|(action, r)| match r {
                    ExplicitLaunchActionReceipt::Runtime(r)
                        if r.stage == StockRuntimeStage::Prepared
                            && &r.review.subject.cutex_session_id == id
                            && claim.as_deref() != Some(r.claim_id.as_str()) =>
                    {
                        Some(action.clone())
                    }
                    _ => None,
                })
                .collect();
            if claim.is_none() && stale_prepared.is_empty() {
                return Ok(ClaimCleanup::Nothing);
            }
            if let Some(claim) = &claim {
                if owner_alive(&record, claimed.as_ref())? {
                    if let Some(receipt) = claimed.as_ref().filter(|r| wedged_after_readiness(&record, r)) {
                        // Stopped outside this store lock (the stop commits
                        // through the store itself); the locks above stay held.
                        return Ok(ClaimCleanup::StopWedged(record.clone(), receipt.action_id.clone()));
                    }
                    anyhow::bail!(
                        "launch_in_progress: agent {} has a live launch (claim {claim}); wait for it or stop that runtime",
                        id.as_str()
                    );
                }
            }
            let note = "launch owner absent; claim cleared automatically".to_string();
            let now = chrono::Utc::now().to_rfc3339();
            let current = sessions.sessions.get_mut(id.as_str()).unwrap();
            if claim.is_some() {
                current.app_server_launch_claim_id = None;
                // A spawned-but-never-registered child of this claim is dead:
                // drop its binding so the next start sees an offline agent.
                if let Some(binding) = claimed.as_ref().and_then(|r| r.binding.as_ref()) {
                    if current.app_server_runtime.as_ref() == Some(binding) {
                        current.app_server_runtime = None;
                        current.runtime_pid = None;
                        if let Some(receipt) = &claimed {
                            if current.current_runtime_agent_id.as_deref() == Some(receipt.runtime_agent_id.as_str()) {
                                current.current_runtime_agent_id = None;
                            }
                        }
                    }
                }
                current.updated_at = now.clone();
            }
            let mut abandoned = stale_prepared;
            if let Some(receipt) = &claimed {
                abandoned.push(receipt.action_id.to_string());
            }
            mark_abandoned(sessions, &abandoned, &note, &now);
            save_locked_session_store(path, sessions)?;
            Ok(ClaimCleanup::Cleared)
        })?;
        let (record, action) = match outcome {
            ClaimCleanup::Nothing => return Ok(false),
            ClaimCleanup::Cleared => return Ok(true),
            ClaimCleanup::StopWedged(record, action) => (record, action),
        };
        stop_wedged(&record)
            .with_context(|| format!("stopping the runtime of failed launch {}", action.as_str()))?;
        with_locked_session_store(path, |sessions| {
            let now = chrono::Utc::now().to_rfc3339();
            if let Some(current) = sessions.sessions.get_mut(id.as_str()) {
                if current.app_server_launch_claim_id == record.app_server_launch_claim_id {
                    current.app_server_launch_claim_id = None;
                    current.updated_at = now.clone();
                }
            }
            mark_abandoned(
                sessions,
                &[action.to_string()],
                "readiness had failed; its live runtime was stopped and the claim cleared automatically",
                &now,
            );
            save_locked_session_store(path, sessions)?;
            Ok(true)
        })
    }
}

fn mark_abandoned(
    sessions: &mut crate::session::model::CutexSessionStore,
    actions: &[String],
    note: &str,
    now: &str,
) {
    for action in actions {
        if let Some(ExplicitLaunchActionReceipt::Runtime(r)) =
            sessions.explicit_launch_receipts.get_mut(action)
        {
            if r.stage != StockRuntimeStage::Ready {
                r.stage = StockRuntimeStage::Abandoned;
                // Keep the original failure visible next to the cleanup note.
                r.error = Some(match r.error.take() {
                    Some(previous) if previous != note => format!("{note} (was: {previous})"),
                    _ => note.to_string(),
                });
                r.updated_at = now.to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::stock::StockBundle;
    use crate::session::model::{CutexAppServerRuntimeBinding, CutexSessionRecord};
    use serde_json::json;

    struct Executor {
        busy: bool,
    }
    impl StockRuntimeExecutor for Executor {
        fn publication(&mut self, r: &StockRuntimeReceipt) -> anyhow::Result<StockPublication> {
            ensure!(!self.busy, "publication lease busy");
            Ok(r.publication.clone().unwrap())
        }
        fn published_owner_absent(&mut self, _: &StockRuntimeReceipt) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn stop(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
            panic!("recovery must not stop")
        }
        fn spawn(
            &mut self,
            _: &CutexSessionRecord,
            _: &StockBundle,
            _: &StockRuntimeReceipt,
        ) -> anyhow::Result<CutexAppServerRuntimeBinding> {
            panic!("recovery must not spawn")
        }
        fn connect(
            &mut self,
            _: &CutexSessionRecord,
            _: &StockRuntimeReceipt,
        ) -> anyhow::Result<()> {
            panic!("recovery must not connect")
        }
        fn cleanup_owned(&mut self) -> anyhow::Result<()> {
            panic!("recovery must not signal")
        }
        fn retain_owner(&mut self) {
            panic!("no owner")
        }
    }
    struct Fixture {
        root: PathBuf,
        path: PathBuf,
        provider: AgentManagementProvider,
        id: CutexSessionId,
        original: StockRuntimeReceipt,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("cutex-recovery-{}", uuid::Uuid::new_v4()));
        let path = root.join("cutex-sessions.json");
        let provider = AgentManagementProvider::open(root.join("provider")).unwrap();
        let mut record = CutexSessionRecord::new(
            "cutex.recovery-test".into(),
            Some(uuid::Uuid::new_v4().to_string()),
            "private".into(),
            "/private".into(),
            None,
        )
        .unwrap();
        let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
        record.app_server_launch_claim_id = Some("claim-original".into());
        let original: StockRuntimeReceipt = serde_json::from_value(json!({
            "action_id":"original-start", "stage":"claimed", "claim_id":"claim-original", "runtime_agent_id":"runtime-original", "expected_generation":1,
            "binding":null,"publication":{"path":"/private/claim","device":1,"inode":2},"error":"spawn failed: TMPDIR missing","updated_at":"2026-09-13T00:00:00Z",
            "review":{
                "subject":{"cutex_session_id":record.cutex_session_id,"formal_name":"Recovery test","durable_sha256":"a".repeat(64),"authority_sha256":"b".repeat(64),"current_project_id":null,"revision":0,"runtime_generation":0},
                "contract":{"version":2,"native_id":record.codex_session_id,"native_home":"/private","bundle_manifest":"/private/manifest","bundle_sha256":"c".repeat(64)},
                "configuration":{"profile_name":"alpha","profile_id":"private-profile","inherited":false,"profile_sha256":"c".repeat(64),"account_sha256":"d".repeat(64),"model":"private-model","reasoning":null,"model_provider":"private","provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses","requires_openai_auth":false,"supports_websockets":false},"sandbox":"read-only","approval":"on-request"},"restart":false
            }
        })).unwrap();
        with_locked_session_store(&path, |s| {
            s.sessions.insert(id.as_str().into(), record);
            s.explicit_launch_receipts.insert(
                original.action_id.as_str().into(),
                ExplicitLaunchActionReceipt::Runtime(original.clone()),
            );
            save_locked_session_store(&path, s)
        })
        .unwrap();
        Fixture {
            root,
            path,
            provider,
            id,
            original,
        }
    }
    #[test]
    fn recovery_clears_only_absent_owner_claim_and_is_idempotent() {
        let f = fixture();
        let action = AgentActionId::new("human-recovery-test").unwrap();
        let before = load_cutex_session_store_from_path(&f.path).unwrap();
        let done = f
            .provider
            .recover_failed_runtime(
                &HumanManagementPrincipal::authenticated(),
                &f.path,
                &f.id,
                &action,
                &mut Executor { busy: false },
            )
            .unwrap();
        assert!(done.completed);
        let recovery_status = f.provider.runtime_action_status(&f.path, &action).unwrap();
        assert_eq!(recovery_status.state, "succeeded");
        assert_eq!(recovery_status.recovery, Some(done.clone()));
        let after = load_cutex_session_store_from_path(&f.path).unwrap();
        let r = &after.sessions[f.id.as_str()];
        assert!(r.app_server_launch_claim_id.is_none());
        assert_eq!(
            r.codex_session_id,
            before.sessions[f.id.as_str()].codex_session_id
        );
        assert_eq!(
            r.runtime_generation,
            before.sessions[f.id.as_str()].runtime_generation
        );
        assert_eq!(
            serde_json::to_value(&after.explicit_launch_receipts).unwrap(),
            serde_json::to_value(&before.explicit_launch_receipts).unwrap()
        );
        assert_eq!(
            done,
            f.provider
                .recover_failed_runtime(
                    &HumanManagementPrincipal::authenticated(),
                    &f.path,
                    &f.id,
                    &action,
                    &mut Executor { busy: true }
                )
                .unwrap()
        );
        let state = f
            .provider
            .runtime_action_status(&f.path, &f.original.action_id)
            .unwrap();
        assert_eq!(state.state, "failed");
    }
    #[test]
    fn dead_launch_claim_is_cleared_automatically_and_action_is_abandoned() {
        let f = fixture();
        let before = load_cutex_session_store_from_path(&f.path).unwrap();
        let mut probed = 0;
        let cleared = f
            .provider
            .clear_dead_launch_claim(
                &f.path,
                &f.id,
                &mut |record, receipt| {
                    probed += 1;
                    assert_eq!(record.app_server_launch_claim_id.as_deref(), Some("claim-original"));
                    assert_eq!(receipt.map(|r| r.action_id.as_str()), Some("original-start"));
                    Ok(false)
                },
                &mut |_| panic!("dead owner: nothing to stop"),
            )
            .unwrap();
        assert!(cleared);
        assert_eq!(probed, 1);
        let after = load_cutex_session_store_from_path(&f.path).unwrap();
        let record = &after.sessions[f.id.as_str()];
        assert!(record.app_server_launch_claim_id.is_none());
        assert_eq!(
            record.runtime_generation,
            before.sessions[f.id.as_str()].runtime_generation
        );
        let ExplicitLaunchActionReceipt::Runtime(receipt) =
            &after.explicit_launch_receipts[f.original.action_id.as_str()]
        else {
            panic!("runtime receipt expected")
        };
        assert_eq!(receipt.stage, StockRuntimeStage::Abandoned);
        assert_eq!(
            f.provider
                .runtime_action_status(&f.path, &f.original.action_id)
                .unwrap()
                .state,
            "failed"
        );
        // Nothing left to clear: no probe, no write.
        let bytes = crate::session::store::session_store_json_bytes(&f.path).unwrap();
        assert!(!f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| panic!("no claim to probe"), &mut |_| panic!("nothing to stop"))
            .unwrap());
        assert_eq!(bytes, crate::session::store::session_store_json_bytes(&f.path).unwrap());
    }

    fn binding(pid: u32) -> CutexAppServerRuntimeBinding {
        serde_json::from_value(json!({
            "transport":"unix_socket","endpoint":"unix:///private/native.sock","pid":pid,
            "runtime_dir":"/private/runtime","diagnostic_journal_path":"/private/journal",
            "schema_version":"stock","schema_sha256":"e".repeat(64),"started_at":"2026-10-13T00:00:00Z"
        }))
        .unwrap()
    }

    /// The store as a failed-readiness launch leaves it (2026-10-13): claim,
    /// committed binding of a live child, receipt Spawned with the error.
    fn wedge(f: &Fixture) -> StockRuntimeReceipt {
        let mut receipt = f.original.clone();
        receipt.stage = StockRuntimeStage::Spawned;
        receipt.binding = Some(binding(4242));
        receipt.error = Some("readiness incomplete: app-server request failed with -32600: unreadable external input history; replay exact action".into());
        with_locked_session_store(&f.path, |s| {
            let record = s.sessions.get_mut(f.id.as_str()).unwrap();
            record.app_server_runtime = receipt.binding.clone();
            record.runtime_pid = Some(4242);
            s.explicit_launch_receipts.insert(
                receipt.action_id.to_string(),
                ExplicitLaunchActionReceipt::Runtime(receipt.clone()),
            );
            save_locked_session_store(&f.path, s)
        })
        .unwrap();
        receipt
    }

    fn receipt_of(f: &Fixture) -> StockRuntimeReceipt {
        let store = load_cutex_session_store_from_path(&f.path).unwrap();
        let ExplicitLaunchActionReceipt::Runtime(receipt) =
            &store.explicit_launch_receipts[f.original.action_id.as_str()]
        else {
            panic!("runtime receipt expected")
        };
        receipt.clone()
    }

    #[test]
    fn live_child_of_failed_readiness_is_stopped_and_claim_cleared() {
        let f = fixture();
        wedge(&f);
        let mut stopped = Vec::new();
        let path = f.path.clone();
        let cleared = f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| Ok(true), &mut |record| {
                stopped.push(record.app_server_runtime.as_ref().map(|b| b.pid));
                // What the production stop commits after its exact stop.
                commit_stock_runtime_stop(&path, record)
            })
            .unwrap();
        assert!(cleared);
        assert_eq!(stopped, vec![Some(4242)]);
        let store = load_cutex_session_store_from_path(&f.path).unwrap();
        let record = &store.sessions[f.id.as_str()];
        assert!(record.app_server_launch_claim_id.is_none());
        assert!(record.app_server_runtime.is_none() && record.runtime_pid.is_none());
        let receipt = receipt_of(&f);
        assert_eq!(receipt.stage, StockRuntimeStage::Abandoned);
        assert!(receipt.error.as_deref().unwrap().contains("unreadable external input history"));
        // Healed: the next attempt has nothing to probe or stop.
        assert!(!f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| panic!("no claim"), &mut |_| panic!("nothing to stop"))
            .unwrap());
    }

    #[test]
    fn failed_stop_of_wedged_child_keeps_the_claim() {
        let f = fixture();
        wedge(&f);
        let before = crate::session::store::session_store_json_bytes(&f.path).unwrap();
        let error = f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| Ok(true), &mut |_| {
                anyhow::bail!("process identity mismatch")
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("process identity mismatch"), "{error:#}");
        assert_eq!(before, crate::session::store::session_store_json_bytes(&f.path).unwrap());
    }

    #[test]
    fn live_spawned_child_without_error_is_still_in_progress() {
        let f = fixture();
        let mut receipt = wedge(&f);
        receipt.error = None;
        crate::session::store::update_receipt_row::<crate::session::store::ExplicitLaunchReceipts>(
            &f.path,
            &receipt.action_id.to_string(),
            Some(ExplicitLaunchActionReceipt::Runtime(receipt.clone())),
        )
        .unwrap();
        let error = f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| Ok(true), &mut |_| panic!("must not stop"))
            .unwrap_err();
        assert!(error.to_string().contains("launch_in_progress"), "{error:#}");
    }

    struct StoppingExecutor {
        path: PathBuf,
        fail_stop: bool,
        stopped: u32,
        retained: u32,
        cleaned: u32,
    }
    impl StockRuntimeExecutor for StoppingExecutor {
        fn publication(&mut self, r: &StockRuntimeReceipt) -> anyhow::Result<StockPublication> {
            Ok(r.publication.clone().unwrap())
        }
        fn published_owner_absent(&mut self, _: &StockRuntimeReceipt) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn stop(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()> {
            self.stopped += 1;
            anyhow::ensure!(!self.fail_stop, "stop scope unproven");
            commit_stock_runtime_stop(&self.path, record)
        }
        fn spawn(
            &mut self,
            _: &CutexSessionRecord,
            _: &StockBundle,
            _: &StockRuntimeReceipt,
        ) -> anyhow::Result<CutexAppServerRuntimeBinding> {
            panic!("no spawn")
        }
        fn connect(&mut self, _: &CutexSessionRecord, _: &StockRuntimeReceipt) -> anyhow::Result<()> {
            panic!("no connect")
        }
        fn cleanup_owned(&mut self) -> anyhow::Result<()> {
            self.cleaned += 1;
            Ok(())
        }
        fn retain_owner(&mut self) {
            self.retained += 1;
        }
    }

    #[test]
    fn readiness_failure_stops_the_spawned_child_and_releases_the_claim() {
        let f = fixture();
        let receipt = wedge(&f);
        let mut runtime = StoppingExecutor { path: f.path.clone(), fail_stop: false, stopped: 0, retained: 0, cleaned: 0 };
        let receipt = super::super::stock_runtime::abandon_unready_spawn(
            &f.path,
            &f.id,
            receipt,
            &mut runtime,
            anyhow::anyhow!("app-server request failed with -32600: unreadable external input history"),
        )
        .unwrap();
        assert_eq!((runtime.stopped, runtime.cleaned, runtime.retained), (1, 1, 0));
        assert_eq!(receipt.stage, StockRuntimeStage::Abandoned);
        assert!(receipt.error.as_deref().unwrap().starts_with("readiness failed: app-server request failed"));
        assert_eq!(receipt_of(&f), receipt);
        let store = load_cutex_session_store_from_path(&f.path).unwrap();
        let record = &store.sessions[f.id.as_str()];
        assert!(!crate::session::archive::record_has_runtime_claim(record), "{record:?}");
        // The next start's cleanup has nothing left to do.
        assert!(!f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| panic!("no claim"), &mut |_| panic!("nothing to stop"))
            .unwrap());
    }

    #[test]
    fn readiness_failure_with_failed_stop_retains_child_and_records_both_errors() {
        let f = fixture();
        let receipt = wedge(&f);
        let mut runtime = StoppingExecutor { path: f.path.clone(), fail_stop: true, stopped: 0, retained: 0, cleaned: 0 };
        let receipt = super::super::stock_runtime::abandon_unready_spawn(
            &f.path,
            &f.id,
            receipt,
            &mut runtime,
            anyhow::anyhow!("published stock owner not ready"),
        )
        .unwrap();
        assert_eq!((runtime.stopped, runtime.cleaned, runtime.retained), (1, 0, 1));
        assert_eq!(receipt.stage, StockRuntimeStage::Spawned);
        let error = receipt.error.clone().unwrap();
        assert!(error.contains("published stock owner not ready") && error.contains("stop scope unproven"), "{error}");
        let store = load_cutex_session_store_from_path(&f.path).unwrap();
        assert_eq!(store.sessions[f.id.as_str()].app_server_launch_claim_id.as_deref(), Some("claim-original"));
        // ... and the next start's cleanup treats it as wedged.
        assert!(f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| Ok(true), &mut |record| {
                commit_stock_runtime_stop(&f.path, record)
            })
            .unwrap());
    }

    #[test]
    fn live_launch_claim_is_kept() {
        let f = fixture();
        let before = crate::session::store::session_store_json_bytes(&f.path).unwrap();
        let error = f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| Ok(true), &mut |_| panic!("nothing to stop"))
            .unwrap_err();
        assert!(error.to_string().contains("launch_in_progress"), "{error:#}");
        assert_eq!(before, crate::session::store::session_store_json_bytes(&f.path).unwrap());
        // Ambiguous evidence also keeps the claim.
        assert!(f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| anyhow::bail!("ambiguous"), &mut |_| panic!("nothing to stop"))
            .is_err());
        assert_eq!(before, crate::session::store::session_store_json_bytes(&f.path).unwrap());
    }

    #[test]
    fn stale_prepared_intent_no_longer_defers_delivery() {
        let f = fixture();
        with_locked_session_store(&f.path, |s| {
            let record = s.sessions.get_mut(f.id.as_str()).unwrap();
            record.app_server_launch_claim_id = None;
            record.explicit_launch = Some(f.original.review.contract.clone());
            let mut prepared = f.original.clone();
            prepared.action_id = AgentActionId::new("crashed-prepare").unwrap();
            prepared.claim_id = "claim-never-written".into();
            prepared.stage = StockRuntimeStage::Prepared;
            prepared.publication = None;
            prepared.error = None;
            s.explicit_launch_receipts.insert(
                prepared.action_id.to_string(),
                ExplicitLaunchActionReceipt::Runtime(prepared),
            );
            save_locked_session_store(&f.path, s)
        })
        .unwrap();
        let store = load_cutex_session_store_from_path(&f.path).unwrap();
        assert!(native_runtime_transition_pending(
            &store,
            &store.sessions[f.id.as_str()]
        ));
        assert!(f
            .provider
            .clear_dead_launch_claim(&f.path, &f.id, &mut |_, _| panic!("no claim to probe"), &mut |_| panic!("nothing to stop"))
            .unwrap());
        let store = load_cutex_session_store_from_path(&f.path).unwrap();
        assert!(!native_runtime_transition_pending(
            &store,
            &store.sessions[f.id.as_str()]
        ));
    }

    #[test]
    fn busy_publication_or_recorded_process_cannot_be_recovered() {
        let f = fixture();
        let action = AgentActionId::new("blocked-recovery").unwrap();
        let before = crate::session::store::session_store_json_bytes(&f.path).unwrap();
        assert!(f
            .provider
            .recover_failed_runtime(
                &HumanManagementPrincipal::authenticated(),
                &f.path,
                &f.id,
                &action,
                &mut Executor { busy: true }
            )
            .is_err());
        assert_eq!(before, crate::session::store::session_store_json_bytes(&f.path).unwrap());
        assert!(!recovery_path(&f.path, &action).unwrap().exists());
        with_locked_session_store(&f.path, |s| {
            s.sessions.get_mut(f.id.as_str()).unwrap().runtime_pid = Some(1234);
            save_locked_session_store(&f.path, s)
        })
        .unwrap();
        assert!(f
            .provider
            .recover_failed_runtime(
                &HumanManagementPrincipal::authenticated(),
                &f.path,
                &f.id,
                &action,
                &mut Executor { busy: false }
            )
            .is_err());
    }
    #[test]
    fn prepared_recovery_replay_does_not_clear_a_later_claim() {
        let f = fixture();
        let action = AgentActionId::new("interrupted-human-recovery").unwrap();
        let prepared = HumanRuntimeRecovery {
            action_id: action.clone(),
            cutex_session_id: f.id.clone(),
            original_action_id: f.original.action_id.clone(),
            claim_id: f.original.claim_id.clone(),
            completed: false,
        };
        write_private_pretty_json_atomic(
            &recovery_path(&f.path, &action).unwrap(),
            &prepared,
            "test",
        )
        .unwrap();
        with_locked_session_store(&f.path, |s| {
            let r = s.sessions.get_mut(f.id.as_str()).unwrap();
            r.app_server_launch_claim_id = Some("new-claim".into());
            r.runtime_pid = Some(5678);
            save_locked_session_store(&f.path, s)
        })
        .unwrap();
        let before = crate::session::store::session_store_json_bytes(&f.path).unwrap();
        assert!(
            f.provider
                .recover_failed_runtime(
                    &HumanManagementPrincipal::authenticated(),
                    &f.path,
                    &f.id,
                    &action,
                    &mut Executor { busy: true }
                )
                .unwrap()
                .completed
        );
        assert_eq!(before, crate::session::store::session_store_json_bytes(&f.path).unwrap());
    }
}
