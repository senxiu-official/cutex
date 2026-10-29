//! Reversible Human Archive. Permanent roster Close is a separate operation.
use super::*;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::role_revision::{CutexSessionId, Sha256};
use crate::session::{archive::record_has_runtime_claim, model::CutexSessionRecord};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[path = "archive_policy.rs"]
mod policy;
pub use policy::*;
#[path = "permanent_archive.rs"]
mod permanent;
pub use permanent::*;
#[path = "archive_transaction.rs"]
mod transaction;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentArchiveOperation {
    Archive,
    Restore,
    /// Accepted only by the dedicated owner confirmation entry.
    PermanentRetire,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentArchiveReviewRequest {
    pub cutex_session_id: CutexSessionId,
    pub operation: AgentArchiveOperation,
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("archive_r264_tests.rs");
    use crate::session::store::{
        load_cutex_session_store_from_path, save_cutex_session_store_to_path,
    };
    struct Offline {
        stops: usize,
        reject: bool,
    }
    impl AgentArchiveRuntime for Offline {
        fn prepare(&mut self, _: &CutexSessionRecord, _: bool) -> anyhow::Result<()> {
            anyhow::ensure!(!self.reject, "unsupported_stop_proof");
            Ok(())
        }
        fn stop_and_verify(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
            self.stops += 1;
            Ok(())
        }
        fn verify_offline(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
            Ok(())
        }
    }
    struct Fixture {
        root: std::path::PathBuf,
        path: std::path::PathBuf,
        provider: AgentManagementProvider,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("cutex-archive-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("cutex-sessions.json");
            let provider = AgentManagementProvider::open(root.join("provider")).unwrap();
            let mut sessions = crate::session::model::CutexSessionStore::default();
            let mut record = CutexSessionRecord::new_at(
                "cutex.archive-test".into(),
                Some("native-test".into()),
                crate::platform::host::current_host_name(),
                root.to_string_lossy().into_owned(),
                None,
                "2026-09-09T00:00:00Z".into(),
            )
            .unwrap();
            record.agent_enabled = true;
            record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
            record.formal_agent_name = Some("Formal Agent".into());
            record.thread_name = Some("Never a formal name".into());
            sessions
                .sessions
                .insert(record.cutex_session_id.clone(), record);
            save_cutex_session_store_to_path(&path, &sessions).unwrap();
            Self {
                root,
                path,
                provider,
            }
        }
        fn request(&self, operation: AgentArchiveOperation, action: &str) -> AgentArchiveRequest {
            AgentArchiveRequest {
                reason: None,
                action_id: AgentActionId::new(action).unwrap(),
                review: self
                    .provider
                    .review_agent_archive(
                        &HumanManagementPrincipal::authenticated(),
                        &self.path,
                        &CutexSessionId::new("cutex.archive-test").unwrap(),
                        operation,
                    )
                    .unwrap(),
            }
        }
        fn execute(
            &self,
            request: &AgentArchiveRequest,
            runtime: &mut Offline,
        ) -> Result<AgentArchiveReceipt, AgentManagementError> {
            self.provider.execute_agent_archive(
                &HumanManagementPrincipal::authenticated(),
                &self.path,
                request,
                runtime,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn make_claude_record(native_session_id: Option<&str>) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.archive-test".into(),
            None,
            crate::platform::host::current_host_name(),
            "/tmp/cutex-archive-claude".into(),
            None,
            "2026-09-09T00:00:00Z".into(),
        )
        .unwrap();
        record.agent_enabled = true;
        record.registration_class =
            crate::agent_bus::model::AgentRegistrationClass::Persistent;
        record.formal_agent_name = Some("Formal Agent".into());
        record.thread_name = Some("Never a formal name".into());
        record.runtime_backend =
            crate::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
        record.claude_session = Some(crate::runtime::claude::ClaudeSession {
            deployment: crate::runtime::claude::ClaudeDeployment {
                python: "/python".into(),
                adapter_source: "/adapter".into(),
                claude_cli: "/claude".into(),
                claude_home: "/claude-home".into(),
                tui_binary: "/claude-tui".into(),
            },
            thread_id: "adapter-thread".into(),
            state_dir: "/state".into(),
            model: "claude-model".into(),
            effort: None,
            instructions: String::new(),
            allow_write: false,
            allow_exec: false, write_explicit: false, exec_explicit: false,
            native_session_id: native_session_id.map(str::to_string),
            connection_id: Some("connection".into()),
            initial_permission_mode: None,
        });
        record
    }

    #[test]
    fn d1r2_archive_restore_identity_and_exact_historical_replay() {
        let f = Fixture::new();
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        let request = f.request(AgentArchiveOperation::Archive, "archive-test");
        let receipt = f.execute(&request, &mut runtime).unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
        assert!(receipt.result.as_ref().unwrap().is_retired());
        let restore = f.request(AgentArchiveOperation::Restore, "restore-test");
        let restored = f.execute(&restore, &mut runtime).unwrap();
        let record = restored.result.unwrap();
        assert!(record.is_active());
        assert!(!record_has_runtime_claim(&record));
        assert_eq!(record.codex_session_id.as_deref(), Some("native-test"));
        assert_eq!(record.formal_agent_name.as_deref(), Some("Formal Agent"));
        assert_eq!(record.profile, None);
        assert_eq!(f.execute(&request, &mut runtime).unwrap(), receipt);
        assert_eq!(runtime.stops, 1);
        assert!(f.provider.store().snapshot().unwrap().agents.is_empty());
        // The action id alone is the idempotency key.
        let mut changed = request.clone();
        changed.review.formal_name = "changed".into();
        assert_eq!(f.execute(&changed, &mut runtime).unwrap(), receipt);
    }

    /// An archived Agent's name may be reused (r244); restoring the archived
    /// one while a live Agent holds the name is refused, nothing changes.
    #[test]
    fn restore_is_refused_while_a_live_agent_holds_the_name() {
        let f = Fixture::new();
        let mut runtime = Offline { stops: 0, reject: false };
        let archive = f.request(AgentArchiveOperation::Archive, "archive-name");
        assert_eq!(f.execute(&archive, &mut runtime).unwrap().stage, AgentArchiveStage::Committed);
        let restore = f.request(AgentArchiveOperation::Restore, "restore-name");
        let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
        let mut successor = sessions.sessions["cutex.archive-test"].clone();
        successor.cutex_session_id = "cutex.successor".into();
        successor.codex_session_id = Some("native-successor".into());
        successor.archive_state = crate::session::model::CutexSessionArchiveState::Active;
        successor.retired_at = None;
        sessions.sessions.insert("cutex.successor".into(), successor);
        save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
        let error = f.execute(&restore, &mut runtime).unwrap_err().to_string();
        assert!(error.contains("agent_name_taken") && error.contains("cutex.successor"), "{error}");
        let after = load_cutex_session_store_from_path(&f.path).unwrap();
        assert!(after.sessions["cutex.archive-test"].is_retired());
    }

    /// A committed archive settles the agent's open delegations as target
    /// (default asks store of the private test home): running → stopped,
    /// queued → failed `recipient_archived`.
    #[test]
    fn archive_settles_the_agents_delegations_as_target() {
        use crate::asks::model::*;
        let f = Fixture::new();
        let target = "cutex.archive-test";
        let store = crate::asks::store::AskStore::open_default().unwrap();
        let now = chrono::Utc::now();
        let stamp = crate::asks::now_rfc3339(now);
        let tag = uuid::Uuid::new_v4().simple().to_string();
        // One transaction, so a concurrent archive test sees both or none.
        let (queued, running) = store
            .with_state(|state| {
                let mut queued = crate::asks::store::tests::new_ask(&format!("q-{tag}"), "x");
                queued.target_cutex_session_id = target.into();
                let mut running = crate::asks::store::tests::new_ask(&format!("r-{tag}"), "y");
                running.target_cutex_session_id = target.into();
                let queued = state.create_ask(queued, now).unwrap().0.ask_id;
                let running = state.create_ask(running, now).unwrap().0.ask_id;
                state.update_ask(&running, now, |ask| {
                    let mut record = AttemptRecord::submitting(1, format!("m-{tag}"), 0, stamp.clone());
                    record.state = AttemptState::Running;
                    record.turn_id = Some(format!("turn-{tag}"));
                    ask.attempts.push(record);
                    ask.execution = ExecutionState::Running;
                });
                state.leases.insert(
                    target.into(),
                    AdmissionLease {
                        cutex_session_id: target.into(),
                        holder: LeaseHolder::Ask { ask_id: running.clone(), attempt: 1 },
                        native_message_id: format!("m-{tag}"),
                        runtime_generation: 0,
                        turn_id: Some(format!("turn-{tag}")),
                        since: stamp.clone(),
                    },
                );
                (queued, running)
            })
            .unwrap();
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        let request = f.request(AgentArchiveOperation::Archive, &format!("archive-asks-{tag}"));
        let receipt = f.execute(&request, &mut runtime).unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
        let state = store.snapshot().unwrap();
        let record = &state.asks[&running];
        assert_eq!(record.execution, ExecutionState::Interrupted);
        assert_eq!(record.closed, Some(CloseOutcome::Stopped));
        assert_eq!(record.attempts[0].interrupt_reason, Some(InterruptReason::Stopped));
        assert!(!state.leases.get(target).is_some_and(|lease| matches!(
            &lease.holder,
            LeaseHolder::Ask { ask_id, .. } if *ask_id == running
        )));
        assert_eq!(state.asks[&queued].execution, ExecutionState::Failed);
        assert!(state.asks[&queued].closed.is_some());
    }

    #[test]
    fn archive_accepts_claude_native_identity() {
        let f = Fixture::new();
        let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
        sessions.sessions.insert(
            "cutex.archive-test".into(),
            make_claude_record(Some(
                "cbb1b0f7-60ec-4795-9d70-952626942cf0",
            )),
        );
        save_cutex_session_store_to_path(&f.path, &sessions).unwrap();

        let request = f.request(AgentArchiveOperation::Archive, "archive-claude");
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        let receipt = f.execute(&request, &mut runtime).unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
        assert!(receipt.result.as_ref().unwrap().is_retired());
    }

    #[test]
    fn archive_accepts_missing_claude_native_identity() {
        let f = Fixture::new();
        let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
        sessions
            .sessions
            .insert("cutex.archive-test".into(), make_claude_record(None));
        save_cutex_session_store_to_path(&f.path, &sessions).unwrap();

        let request = f.request(AgentArchiveOperation::Archive, "archive-claude-missing");
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        let receipt = f.execute(&request, &mut runtime).unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
    }

    #[test]
    fn unsupported_stop_has_no_effect_and_later_edits_do_not_go_stale() {
        let f = Fixture::new();
        let request = f.request(AgentArchiveOperation::Archive, "archive-unsupported");
        let before = crate::session::store::session_store_json_bytes(&f.path).unwrap();
        let mut runtime = Offline {
            stops: 0,
            reject: true,
        };
        assert!(f.execute(&request, &mut runtime).is_err());
        assert_eq!(crate::session::store::session_store_json_bytes(&f.path).unwrap(), before);
        assert!(f
            .provider
            .store()
            .snapshot()
            .unwrap()
            .agent_archive_actions
            .is_empty());
        let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
        sessions
            .sessions
            .get_mut("cutex.archive-test")
            .unwrap()
            .profile = Some("new-config".into());
        save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
        runtime.reject = false;
        // A profile edit between review and execute is not a conflict.
        let receipt = f.execute(&request, &mut runtime).unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
        assert_eq!(runtime.stops, 1);
    }

    #[test]
    fn director_guard_applies_without_roster_membership_and_operator_grant_does_not_block() {
        let f = Fixture::new();
        let request = f.request(AgentArchiveOperation::Archive, "role-protected-archive");
        let project = ProjectId::new("project").unwrap();
        f.provider
            .store()
            .with_state(true, |mut state| {
                state.projects.insert(
                    project.clone(),
                    ProjectAuthority {
                        project_id: project.clone(),
                        authorized_director_session: request.review.cutex_session_id.clone(),
                        authority_epoch: 1,
                        updated_at: super::super::store::now(),
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        let state = f.provider.store().snapshot().unwrap();
        assert_eq!(
            runtime_guard(&state, &request.review.cutex_session_id).unwrap(),
            Some(project.clone())
        );
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        assert!(matches!(
            f.execute(&request, &mut runtime),
            Err(AgentManagementError::Conflict(
                "archive_requires_explicit_director_rotation"
            ))
        ));
        assert_eq!(runtime.stops, 0);
        let blockers = f
            .provider
            .agent_archive_blockers(&request.review.cutex_session_id)
            .unwrap();
        assert_eq!(blockers.director_projects, vec![project.clone()]);
        // An archived project does not need a Director.
        let set_lifecycle = |lifecycle: ProjectLifecycle| {
            f.provider
                .store()
                .with_state(true, |mut state| {
                    state.project_states.insert(
                        project.clone(),
                        ProjectStateRecord {
                            project_id: project.clone(),
                            lifecycle,
                            revision: 1,
                            created_at: super::super::store::now(),
                            updated_at: super::super::store::now(),
                            archived_at: None,
                            workspace_root: None,
                            last_director_epoch: None,
                        },
                    );
                    Ok((state, (), true))
                })
                .unwrap();
        };
        set_lifecycle(ProjectLifecycle::Archived);
        assert!(f
            .provider
            .agent_archive_blockers(&request.review.cutex_session_id)
            .unwrap()
            .is_empty());
        set_lifecycle(ProjectLifecycle::Active);
        f.provider
            .store()
            .with_state(true, |mut state| {
                let director = CutexSessionId::new("cutex.other-director").unwrap();
                state
                    .projects
                    .get_mut(&project)
                    .unwrap()
                    .authorized_director_session = director.clone();
                state
                    .operator_grants
                    .entry(project.clone())
                    .or_default()
                    .insert(
                        request.review.cutex_session_id.clone(),
                        AgentOperatorGrant {
                            project_id: project.clone(),
                            operator_cutex_session_id: request.review.cutex_session_id.clone(),
                            grant_revision: 1,
                            granted_at: super::super::store::now(),
                            granted_by_primary_director_session: director,
                            performed_by_human_management: true,
                        },
                    );
                Ok((state, (), true))
            })
            .unwrap();
        // r219: the Director is the Agent Management authority only. Once it
        // names another session, nothing else keeps this agent a Director,
        // and an Operator grant does not block Archive.
        assert!(f
            .provider
            .agent_archive_blockers(&request.review.cutex_session_id)
            .unwrap()
            .is_empty());
        let receipt = f.execute(&request, &mut runtime).unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
    }

    #[test]
    fn d1r2_stopped_failure_is_not_success_and_atomic_receipt_recovers() {
        struct FailedProof;
        impl AgentArchiveRuntime for FailedProof {
            fn prepare(&mut self, _: &CutexSessionRecord, _: bool) -> anyhow::Result<()> {
                Ok(())
            }
            fn stop_and_verify(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
                Ok(())
            }
            fn verify_offline(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
                anyhow::bail!("injected post-stop observation failure")
            }
        }
        let f = Fixture::new();
        let request = f.request(AgentArchiveOperation::Archive, "post-stop-failure");
        let receipt = f
            .provider
            .execute_agent_archive(
                &HumanManagementPrincipal::authenticated(),
                &f.path,
                &request,
                &mut FailedProof,
            )
            .unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::StoppedNotArchived);
        assert!(load_cutex_session_store_from_path(&f.path)
            .unwrap()
            .sessions["cutex.archive-test"]
            .is_active());
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        assert_eq!(f.execute(&request, &mut runtime).unwrap(), receipt);
        assert_eq!(runtime.stops, 0);
        let next = f.request(AgentArchiveOperation::Archive, "atomic-success");
        let committed = f.execute(&next, &mut runtime).unwrap();
        // Simulate loss before the provider's final journal write, while the
        // durable transition+receipt survived. No historical result relabeling.
        f.provider
            .store()
            .with_state(true, |mut state| {
                state.agent_archive_actions.remove(&next.action_id);
                Ok((state, (), true))
            })
            .unwrap();
        let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
        sessions
            .sessions
            .get_mut("cutex.archive-test")
            .unwrap()
            .formal_agent_name = Some("New current name".into());
        save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
        assert_eq!(f.execute(&next, &mut runtime).unwrap(), committed);
        assert_eq!(runtime.stops, 1);
    }

    #[test]
    fn d1r2_corrupt_sources_do_not_become_empty_success() {
        let f = Fixture::new();
        let request = f.request(AgentArchiveOperation::Archive, "corrupt-source");
        // A corrupt store (the SQLite database since session store Phase 2).
        std::fs::write(crate::session::store::session_database_path(&f.path), b"{broken").unwrap();
        let mut runtime = Offline {
            stops: 0,
            reject: false,
        };
        assert!(f.execute(&request, &mut runtime).is_err());
        assert_eq!(runtime.stops, 0);
    }

    #[test]
    fn d1r2_imported_member_missing_project_and_permanent_history_conflict() {
        let f = Fixture::new();
        let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
        sessions
            .sessions
            .get_mut("cutex.archive-test")
            .unwrap()
            .agent_groups = vec!["test".into()];
        save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
        let principal = HumanManagementPrincipal::authenticated();
        let candidate = f
            .provider
            .durable_agent_candidates(&principal, &f.path)
            .unwrap()
            .remove(0);
        f.provider
            .import_durable_agent(
                &principal,
                &f.path,
                &DurableImportRequest {
                    action_id: AgentActionId::new("import-archive-member").unwrap(),
                    candidate,
                    confirmed_formal_name: "Formal Agent".into(),
                    assignment: None,
                    detach: None,
                },
            )
            .unwrap();
        let id = CutexSessionId::new("cutex.archive-test").unwrap();
        f.provider
            .store()
            .with_state(true, |mut state| {
                state.current_project_memberships.insert(
                    id.clone(),
                    CurrentProjectMembership {
                        cutex_session_id: id.clone(),
                        project_id: Some(ProjectId::new("missing-project").unwrap()),
                        revision: 1,
                        updated_at: super::super::store::now(),
                    },
                );
                Ok((state, (), true))
            })
            .unwrap();
        // A dangling project membership does not block Archive review.
        assert!(f
            .provider
            .review_agent_archive(&principal, &f.path, &id, AgentArchiveOperation::Archive)
            .is_ok());
        f.provider
            .store()
            .with_state(true, |mut state| {
                state.agents.get_mut(&id).unwrap().retired_at = Some(super::super::store::now());
                Ok((state, (), true))
            })
            .unwrap();
        let before = serde_json::to_value(f.provider.store().snapshot().unwrap()).unwrap();
        assert!(matches!(
            f.provider.review_agent_archive(
                &principal,
                &f.path,
                &id,
                AgentArchiveOperation::Restore
            ),
            Err(AgentManagementError::Conflict(
                "permanent_retirement_is_not_restorable"
            ))
        ));
        assert_eq!(
            serde_json::to_value(f.provider.store().snapshot().unwrap()).unwrap(),
            before
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn d1r2_real_owned_process_tree_stop_before_archive_preserves_unrelated_process() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        struct ProcessOracle {
            root: u32,
            child: u32,
            reaper: Option<std::thread::JoinHandle<std::process::ExitStatus>>,
            unrelated: std::process::Child,
        }
        impl AgentArchiveRuntime for ProcessOracle {
            fn prepare(&mut self, record: &CutexSessionRecord, _: bool) -> anyhow::Result<()> {
                anyhow::ensure!(
                    record.runtime_pid == Some(self.root) && record.runtime_generation == 1,
                    "oracle occurrence mismatch"
                );
                anyhow::ensure!(
                    unsafe { libc::getpgid(self.root as i32) } == self.root as i32,
                    "oracle group mismatch"
                );
                Ok(())
            }
            fn stop_and_verify(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
                anyhow::ensure!(
                    unsafe { libc::kill(-(self.root as i32), libc::SIGTERM) } == 0,
                    "owned group stop failed"
                );
                self.reaper.take().unwrap().join().unwrap();
                Ok(())
            }
            fn verify_offline(&mut self, _: &CutexSessionRecord) -> anyhow::Result<()> {
                anyhow::ensure!(
                    !crate::platform::process::process_is_running(self.root)
                        && !crate::platform::process::process_is_running(self.child),
                    "owned tree survived"
                );
                anyhow::ensure!(
                    self.unrelated.try_wait()?.is_none(),
                    "unrelated process disturbed"
                );
                Ok(())
            }
        }
        impl Drop for ProcessOracle {
            fn drop(&mut self) {
                if let Some(reaper) = self.reaper.take() {
                    unsafe {
                        libc::kill(-(self.root as i32), libc::SIGKILL);
                    }
                    let _ = reaper.join();
                }
                let _ = self.unrelated.kill();
                let _ = self.unrelated.wait();
            }
        }
        let f = Fixture::new();
        let mut root = Command::new("python3").process_group(0).args(["-c",
            "import os,time,signal\npid=os.fork()\nif pid == 0:\n time.sleep(30)\n os._exit(0)\ndef stop(*args):\n os.waitpid(pid,0)\n os._exit(0)\nsignal.signal(signal.SIGTERM,stop)\nprint(pid,flush=True)\ntime.sleep(30)"])
            .stdout(Stdio::piped()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(root.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let root_pid = root.id();
        let mut oracle = ProcessOracle {
            root: root_pid,
            child: line.trim().parse().unwrap(),
            reaper: Some(std::thread::spawn(move || root.wait().unwrap())),
            unrelated: Command::new("sleep").arg("30").spawn().unwrap(),
        };
        let mut sessions = load_cutex_session_store_from_path(&f.path).unwrap();
        let record = sessions.sessions.get_mut("cutex.archive-test").unwrap();
        record.runtime_pid = Some(root_pid);
        record.runtime_generation = 1;
        save_cutex_session_store_to_path(&f.path, &sessions).unwrap();
        let request = f.request(AgentArchiveOperation::Archive, "real-tree-archive");
        let receipt = f
            .provider
            .execute_agent_archive(
                &HumanManagementPrincipal::authenticated(),
                &f.path,
                &request,
                &mut oracle,
            )
            .unwrap();
        assert_eq!(receipt.stage, AgentArchiveStage::Committed);
        assert!(receipt.result.unwrap().is_retired());
        assert!(oracle.unrelated.try_wait().unwrap().is_none());
    }
}

/// Informational summary of the agent an Archive/Restore applies to. Execute
/// re-reads the current state under its locks; nothing here is a fence.
/// Older clients may still send the retired digest fields, so unknown fields
/// are accepted (and stored receipts written before this change still load).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct AgentArchiveReview {
    pub cutex_session_id: CutexSessionId,
    pub formal_name: String,
    pub operation: AgentArchiveOperation,
    #[serde(default)]
    pub current_project_id: Option<ProjectId>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub runtime_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentArchiveRequest {
    #[serde(default)]
    pub reason: Option<String>,
    pub action_id: AgentActionId,
    pub review: AgentArchiveReview,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentArchiveStage {
    Prepared,
    Stopped,
    Committed,
    StoppedNotArchived,
    Uncertain,
    PermanentRetirementCommitted,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentArchiveReceipt {
    pub request: AgentArchiveRequest,
    pub request_sha256: Sha256,
    pub stage: AgentArchiveStage,
    pub result: Option<CutexSessionRecord>,
    pub error: Option<String>,
    pub committed_at: crate::role_revision::Rfc3339,
    #[serde(default, rename = "archiveKind", skip_serializing_if = "Option::is_none")]
    pub archive_kind: Option<AgentArchiveKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restorable: Option<bool>,
}

/// Runtime hooks around the durable transition. Callers normally stop the
/// runtime before archiving; `stop_and_verify`/`verify_offline` then only
/// confirm that nothing is left running for this agent.
// Internal occurrence proof; never accepted from the archive wire request.
pub(super) struct ReviewedArchiveOccurrence { generation: u64, thread: Option<String> }
impl ReviewedArchiveOccurrence {
    fn validate(&self, record: &CutexSessionRecord) -> anyhow::Result<()> {
        anyhow::ensure!(record.runtime_generation == self.generation
            && record.app_server_thread_id() == self.thread.as_deref(),
            "archive_confirmation_occurrence_changed");
        Ok(())
    }
}
#[cfg(test)]
thread_local! {
    static AFTER_ARCHIVE_AUTHORIZE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}

pub trait AgentArchiveRuntime {
    fn prepare(&mut self, record: &CutexSessionRecord, restoring: bool) -> anyhow::Result<()>;
    fn stop_and_verify(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()>;
    fn verify_offline(&mut self, record: &CutexSessionRecord) -> anyhow::Result<()>;
}

/// The real reasons an Archive is refused. Empty means the agent can be
/// archived.
#[derive(Clone, Debug, Default, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentArchiveBlockers {
    /// Active projects whose Director (the Agent Management authority) is
    /// this agent.
    pub director_projects: Vec<ProjectId>,
}

impl AgentArchiveBlockers {
    pub fn is_empty(&self) -> bool {
        self.director_projects.is_empty()
    }

    fn conflict(&self) -> Option<AgentManagementError> {
        (!self.director_projects.is_empty())
            .then(|| conflict("archive_requires_explicit_director_rotation"))
    }
}

fn conflict(code: &'static str) -> AgentManagementError {
    AgentManagementError::Conflict(code)
}

/// Replaying an old archive after Restore must not stop the new delegations.
pub(super) fn settle_archive_if_current(path: &Path, receipt: &AgentArchiveReceipt) {
    let Some(saved) = receipt.result.as_ref() else { return };
    let Ok(sessions) = crate::session::store::load_cutex_session_store_from_path(path) else { return };
    if sessions.sessions.get(&saved.cutex_session_id).is_some_and(|current|
        current.is_retired() && current.durable_revision() == saved.durable_revision()) {
        crate::asks::lifecycle::settle_retired_target_default(&saved.cutex_session_id);
    }
}

fn project_is_active(state: &AgentManagementSnapshot, project: &ProjectId) -> bool {
    state.projects.contains_key(project)
        && !state.project_tombstones.contains_key(project)
        && state
            .project_states
            .get(project)
            .is_none_or(|p| p.lifecycle == ProjectLifecycle::Active)
}

pub(super) fn authority_digest(
    state: &AgentManagementSnapshot,
    id: &CutexSessionId,
) -> Result<Sha256, AgentManagementError> {
    // Roles may exist without a roster row. Inspect all explicit authorities.
    let project = state
        .agents
        .get(id)
        .and_then(|a| current_project_id(state, a));
    super::store::request_sha256(&(
        state.agents.get(id),
        state.current_project_memberships.get(id),
        project.as_ref().and_then(|p| state.projects.get(p)),
        project.as_ref().and_then(|p| state.project_states.get(p)),
        project
            .as_ref()
            .and_then(|p| state.operator_grant_revisions.get(p)),
        state
            .projects
            .values()
            .filter(|p| &p.authorized_director_session == id)
            .collect::<Vec<_>>(),
        state
            .operator_grants
            .iter()
            .filter_map(|(p, g)| g.get(id).map(|g| (p, g)))
            .collect::<Vec<_>>(),
    ))
}

/// Starting a runtime does not change project data, so it is gated only on
/// permanent retirement. Returns the agent's project (roster membership,
/// else the project it directs) for display.
pub(super) fn runtime_guard(
    state: &AgentManagementSnapshot,
    id: &CutexSessionId,
) -> Result<Option<ProjectId>, AgentManagementError> {
    let agent = state.agents.get(id);
    if is_permanently_retired(state, id) {
        return Err(conflict("permanent_retirement_is_not_runnable"));
    }
    Ok(agent
        .and_then(|a| current_project_id(state, a))
        .or_else(|| {
            state
                .projects
                .iter()
                .find(|(_, authority)| &authority.authorized_director_session == id)
                .map(|(project, _)| project.clone())
        }))
}

pub(super) fn guard(
    state: &AgentManagementSnapshot,
    id: &CutexSessionId,
) -> Result<Option<ProjectId>, AgentManagementError> {
    if state
        .projects
        .values()
        .any(|p| &p.authorized_director_session == id)
    {
        return Err(conflict("archive_requires_explicit_director_rotation"));
    }
    if state.operator_grants.values().any(|g| g.contains_key(id)) {
        return Err(conflict("archive_requires_explicit_operator_revoke"));
    }
    let agent = state.agents.get(id);
    if is_permanently_retired(state, id) {
        return Err(conflict("permanent_retirement_is_not_restorable"));
    }
    let project = agent.and_then(|a| current_project_id(state, a));
    if let Some(project) = &project {
        if !state.projects.contains_key(project) {
            return Err(conflict("archive_current_project_missing"));
        }
        if state.project_tombstones.contains_key(project)
            || state
                .project_states
                .get(project)
                .is_some_and(|p| p.lifecycle != ProjectLifecycle::Active)
        {
            return Err(conflict("restore_current_project_before_agent_lifecycle"));
        }
    }
    Ok(project)
}

/// Active projects whose Agent Management authority names `id`. r219: the
/// Task Service Director seat copy is gone; only active projects need a
/// Director.
fn director_projects_in(state: &AgentManagementSnapshot, id: &CutexSessionId) -> Vec<ProjectId> {
    let mut director_projects = state
        .projects
        .iter()
        .filter(|(_, authority)| &authority.authorized_director_session == id)
        .map(|(project, _)| project.clone())
        .filter(|project| project_is_active(state, project))
        .collect::<Vec<_>>();
    director_projects.sort();
    director_projects.dedup();
    director_projects
}

fn archive_blockers_in(state: &AgentManagementSnapshot, id: &CutexSessionId) -> AgentArchiveBlockers {
    AgentArchiveBlockers {
        director_projects: director_projects_in(state, id),
    }
}

impl AgentManagementProvider {
    /// Why an Archive of `id` would be refused right now (for error details).
    pub fn agent_archive_blockers(
        &self,
        id: &CutexSessionId,
    ) -> Result<AgentArchiveBlockers, AgentManagementError> {
        Ok(archive_blockers_in(&self.store().snapshot()?, id))
    }

    pub fn review_agent_archive(
        &self,
        _principal: &HumanManagementPrincipal,
        path: &Path,
        id: &CutexSessionId,
        operation: AgentArchiveOperation,
    ) -> Result<AgentArchiveReview, AgentManagementError> {
        if operation == AgentArchiveOperation::PermanentRetire {
            return Err(conflict("permanent_retirement_requires_owner_confirmation"));
        }
        let state = self.store().snapshot()?;
        let sessions = crate::session::store::load_cutex_session_store_from_path(path)
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let record = sessions
            .sessions
            .get(id.as_str())
            .ok_or(conflict("durable_agent_missing"))?;
        if !record.agent_enabled
            || record.registration_class
                != crate::agent_bus::model::AgentRegistrationClass::Persistent
        {
            return Err(conflict("requires_exact_persistent_durable_agent"));
        }
        if is_permanently_retired(&state, id) {
            return Err(conflict("permanent_retirement_is_not_restorable"));
        }
        if operation == AgentArchiveOperation::Restore && policy::legacy_archive_ambiguous(&state, record) {
            return Err(conflict("legacy_close_requires_owner_review"));
        }
        if record.is_retired() != (operation == AgentArchiveOperation::Restore) {
            return Err(conflict("archive_state_conflict"));
        }
        if operation == AgentArchiveOperation::Archive && !director_projects_in(&state, id).is_empty()
        {
            return Err(conflict("archive_requires_explicit_director_rotation"));
        }
        let formal_name = record
            .formal_agent_name
            .clone()
            .or_else(|| state.agents.get(id).map(|a| a.spec.name.clone()))
            .or_else(|| record.thread_name.clone())
            .unwrap_or_else(|| id.as_str().to_string());
        Ok(AgentArchiveReview {
            cutex_session_id: id.clone(),
            formal_name,
            operation,
            current_project_id: state
                .agents
                .get(id)
                .and_then(|a| current_project_id(&state, a)),
            revision: record.durable_revision(),
            runtime_generation: record.runtime_generation,
        })
    }

    /// Single-call Archive/Restore keyed by the durable session id: builds
    /// the summary and executes it under a fresh action id. Reached from the
    /// owner-only Management session mutation path (`cutex/session/retire`).
    pub fn archive_agent(
        &self,
        path: &Path,
        id: &CutexSessionId,
        operation: AgentArchiveOperation,
        reason: Option<String>,
        runtime: &mut dyn AgentArchiveRuntime,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        let principal = HumanManagementPrincipal::authenticated();
        let review = self.review_agent_archive(&principal, path, id, operation)?;
        let request = AgentArchiveRequest {
            reason,
            action_id: AgentActionId::new(format!("archive-{}", uuid::Uuid::new_v4()))
                .map_err(|_| AgentManagementError::InvalidRequest("archive action id"))?,
            review,
        };
        self.execute_agent_archive(&principal, path, &request, runtime)
    }

    /// Guards are evaluated against the state current at execute time (under
    /// the provider execution and mutation locks); the action id is only an
    /// idempotency key.
    ///
    /// A committed archive settles the agent's open delegations (asks) as
    /// target, after every lock taken here is released (lock order: provider
    /// execution → Agent Management mutation → session store, all dropped;
    /// then the asks store alone).
    pub fn execute_agent_archive(
        &self,
        principal: &HumanManagementPrincipal,
        path: &Path,
        request: &AgentArchiveRequest,
        runtime: &mut dyn AgentArchiveRuntime,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        let _ = principal;
        self.execute_authorized_agent_archive(path, request, runtime, |_, _, _| Ok(()))
    }

    /// Authenticate and authorize the caller under execution/mutation locks,
    /// before receipt disclosure or effects. Agents never manufacture an owner.
    pub(crate) fn execute_authorized_agent_archive(
        &self, path: &Path, request: &AgentArchiveRequest,
        runtime: &mut dyn AgentArchiveRuntime,
        authorize: impl FnOnce(&AgentManagementSnapshot, &CutexSessionId, AgentArchiveOperation) -> Result<(), AgentManagementError>,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        if request.review.operation == AgentArchiveOperation::PermanentRetire {
            return Err(AgentManagementError::Unauthorized);
        }
        let receipt = self.execute_agent_archive_guarded(path, request, runtime, None, authorize)?;
        if receipt.request.review.operation == AgentArchiveOperation::Archive
            && receipt.stage == AgentArchiveStage::Committed
        {
            settle_archive_if_current(path, &receipt);
        }
        Ok(receipt)
    }

    fn execute_agent_archive_guarded(
        &self,
        path: &Path,
        request: &AgentArchiveRequest,
        runtime: &mut dyn AgentArchiveRuntime,
        occurrence: Option<&ReviewedArchiveOccurrence>,
        authorize: impl FnOnce(&AgentManagementSnapshot, &CutexSessionId, AgentArchiveOperation) -> Result<(), AgentManagementError>,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        let _execution = super::provider::provider_execution_lock()
            .lock()
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        let _mutation = self.store().lock_mutations()?;
        authorize(&self.store().snapshot()?, &request.review.cutex_session_id, request.review.operation)?;
        #[cfg(test)]
        AFTER_ARCHIVE_AUTHORIZE.with(|hook| { if let Some(run) = hook.borrow_mut().take() { run(); } });
        self.execute_agent_archive_under_locks(path, request, runtime, occurrence)
    }

    /// Caller holds execution/mutation locks. Settle asks only after releasing them.
    pub(super) fn execute_agent_archive_under_locks(
        &self, path: &Path, request: &AgentArchiveRequest,
        runtime: &mut dyn AgentArchiveRuntime,
        occurrence: Option<&ReviewedArchiveOccurrence>,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        if let Some(receipt) = self.store().snapshot()?.agent_archive_actions.get(&request.action_id) {
            validate_archive_replay_target(request, receipt)?;
        }
        if let Some(receipt) = self
            .store()
            .snapshot()?
            .agent_archive_actions
            .get(&request.action_id)
        {
            if matches!(
                receipt.stage,
                AgentArchiveStage::Committed | AgentArchiveStage::StoppedNotArchived
            ) {
                return Ok(receipt.clone());
            }
        }
        let (committed, launch_claimed) =
            crate::session::store::with_locked_session_store(path, |sessions| {
                Ok((
                    sessions
                        .agent_archive_receipts
                        .get(request.action_id.as_str())
                        .cloned(),
                    sessions
                        .sessions
                        .get(request.review.cutex_session_id.as_str())
                        .is_some_and(|r| r.app_server_launch_claim_id.is_some()),
                ))
            })
            .map_err(|_| AgentManagementError::PersistenceUnavailable)?;
        if let Some(receipt) = committed {
            validate_archive_replay_target(request, &receipt)?;
            self.save_agent_archive_receipt(&receipt)?;
            return Ok(receipt);
        }
        if launch_claimed {
            return Err(conflict("archive_runtime_launch_in_progress"));
        }
        self.execute_agent_archive_locked(path, request, runtime, occurrence)
    }

    pub(super) fn save_agent_archive_receipt(
        &self,
        receipt: &AgentArchiveReceipt,
    ) -> Result<(), AgentManagementError> {
        self.store().with_state(true, |mut state| {
            policy::mark_archive_reader_requirement(&mut state);
            let key = format!("{}/{:?}", receipt.request.action_id, receipt.stage);
            state
                .agent_archive_audit
                .entry(key)
                .or_insert_with(|| receipt.clone());
            state
                .agent_archive_actions
                .insert(receipt.request.action_id.clone(), receipt.clone());
            Ok((state, (), true))
        })
    }
}

#[cfg(test)]
#[path = "archive_r263_compat_tests.rs"]
mod r263_compat_tests;
