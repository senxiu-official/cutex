//! Shared reversible archive and owner-confirmed permanent retirement transaction.
use super::*;

impl AgentManagementProvider {
    pub(super) fn execute_agent_archive_locked(
        &self,
        path: &Path,
        request: &AgentArchiveRequest,
        runtime: &mut dyn AgentArchiveRuntime,
        occurrence: Option<&ReviewedArchiveOccurrence>,
    ) -> Result<AgentArchiveReceipt, AgentManagementError> {
        let state = self.store().snapshot()?;
        let id = &request.review.cutex_session_id;
        let restoring = request.review.operation == AgentArchiveOperation::Restore;
        let permanent = request.review.operation == AgentArchiveOperation::PermanentRetire;
        let finishing_permanent = permanent
            && state.agent_archive_audit.values().any(|r| {
                r.request.action_id == request.action_id
                    && r.request.review.cutex_session_id == *id
                    && r.stage == AgentArchiveStage::PermanentRetirementCommitted
            });
        if policy::is_permanently_retired(&state, id) && !finishing_permanent {
            return Err(conflict("permanent_retirement_is_not_restorable"));
        }
        if !restoring {
            if let Some(blocked) = archive_blockers_in(&state, id).conflict() {
                return Err(blocked);
            }
        }
        let outcome = (|| -> anyhow::Result<AgentArchiveReceipt> {
            // Native runtime adapters update the session store themselves.
            // Keep provider locks, but do not hold the session lock during IO.
            let sessions = crate::session::store::load_cutex_session_store_from_path(path)?;
            let before = sessions
                .sessions
                .get(id.as_str())
                .ok_or_else(|| anyhow::anyhow!("durable_agent_missing"))?
                .clone();
            anyhow::ensure!(
                before.agent_enabled
                    && before.registration_class
                        == crate::agent_bus::model::AgentRegistrationClass::Persistent,
                "requires_exact_persistent_durable_agent"
            );
            anyhow::ensure!(
                before.app_server_launch_claim_id.is_none(),
                "archive_runtime_launch_in_progress"
            );
            if permanent {
                occurrence.ok_or_else(|| anyhow::anyhow!("archive_confirmation_occurrence_missing"))?.validate(&before)?;
                anyhow::ensure!(before.runtime_generation == request.review.runtime_generation,
                    "archive_confirmation_occurrence_changed");
                if !finishing_permanent && before.durable_revision() != request.review.revision {
                    return Err(conflict("archive_confirmation_stale").into());
                }
                let current_name = before
                    .formal_agent_name
                    .as_deref()
                    .or_else(|| state.agents.get(id).map(|a| a.spec.name.as_str()))
                    .ok_or_else(|| anyhow::anyhow!("permanent_retirement_name_unavailable"))?;
                policy::validate_permanent_retirement_name(
                    current_name,
                    &request.review.formal_name,
                )?;
            }
            if restoring {
                if policy::legacy_archive_ambiguous(&state, &before) {
                    return Err(conflict("legacy_close_requires_owner_review").into());
                }
                crate::session::archive::validate_restore_preconditions(
                    &before,
                    before.durable_revision(),
                )?;
                require_free_restore_name(&sessions, &before)?;
            } else if !(permanent && before.is_retired()) {
                crate::session::archive::validate_retire_preconditions(
                    &before,
                    before.durable_revision(),
                    before.runtime_generation,
                )?;
            }
            runtime.prepare(&before, restoring)?;
            let mut receipt = AgentArchiveReceipt {
                request: request.clone(),
                request_sha256: super::super::store::request_sha256(request)?,
                stage: AgentArchiveStage::Prepared,
                result: None,
                error: None,
                committed_at: super::super::store::now(),
                archive_kind: None,
                restorable: None,
            };
            if !finishing_permanent {
                self.save_agent_archive_receipt(&receipt)?;
            }
            if !restoring {
                if permanent {
                    // Keep the reviewed thread through the stop boundary, even if prepare changed storage.
                    let current = crate::session::store::load_cutex_session_store_from_path(path)?;
                    let current = current.sessions.get(id.as_str())
                        .ok_or_else(|| anyhow::anyhow!("durable_agent_missing"))?;
                    occurrence.ok_or_else(|| anyhow::anyhow!("archive_confirmation_occurrence_missing"))?.validate(current)?;
                }
                if let Err(error) = runtime.stop_and_verify(&before) {
                    receipt.stage = AgentArchiveStage::Uncertain;
                    receipt.error = Some(format!("stop_not_proven: {error:#}"));
                    self.save_agent_archive_receipt(&receipt)?;
                    return Ok(receipt);
                }
                receipt.stage = AgentArchiveStage::Stopped;
                receipt.result = Some(before.clone());
                if !finishing_permanent {
                    self.save_agent_archive_receipt(&receipt)?;
                }
            }
            if let Err(error) = runtime.verify_offline(&before) {
                receipt.stage = if finishing_permanent {
                    AgentArchiveStage::PermanentRetirementCommitted
                } else {
                    AgentArchiveStage::StoppedNotArchived
                };
                receipt.error = Some(format!("stopped_not_archived: {error:#}"));
                self.save_agent_archive_receipt(&receipt)?;
                return Ok(receipt);
            }
            crate::session::store::with_locked_session_store(path, |sessions| {
                let current = sessions
                    .sessions
                    .get(id.as_str())
                    .ok_or_else(|| anyhow::anyhow!("durable_agent_missing"))?;
                anyhow::ensure!(
                    current.app_server_thread_id() == before.app_server_thread_id()
                        && current.runtime_backend == before.runtime_backend
                        && current.runtime_generation == before.runtime_generation
                        && current.archive_state == before.archive_state
                        && current.app_server_launch_claim_id.is_none(),
                    "archive_identity_or_state_changed"
                );
                if permanent {
                    anyhow::ensure!(
                        current.formal_agent_name == before.formal_agent_name,
                        "permanent_retirement_name_confirmation_required"
                    );
                }
                if restoring {
                    require_free_restore_name(sessions, current)?;
                }
                let revision = current.durable_revision();
                crate::session::runtime_reconciliation::clear_cutex_session_runtime_record(
                    sessions,
                    id.as_str(),
                    true,
                )?;
                let record = sessions
                    .sessions
                    .get_mut(id.as_str())
                    .expect("locked exact record");
                if permanent {
                    // Authority tombstone precedes the session projection. A
                    // crash can leave an active projection, never restoration.
                    receipt.stage = AgentArchiveStage::PermanentRetirementCommitted;
                    receipt.archive_kind = Some(AgentArchiveKind::Permanent);
                    receipt.restorable = Some(false);
                    receipt.result = Some(record.clone());
                    self.store().with_state(true, |mut state| {
                        policy::mark_archive_reader_requirement(&mut state);
                        if let Some(agent) = state.agents.get_mut(id) {
                            agent
                                .retired_at
                                .get_or_insert_with(|| receipt.committed_at.clone());
                        }
                        state
                            .agent_archive_actions
                            .insert(request.action_id.clone(), receipt.clone());
                        state.agent_archive_audit.insert(
                            format!("{}/PermanentRetirementCommitted", request.action_id),
                            receipt.clone(),
                        );
                        Ok((state, (), true))
                    })?;
                }
                if restoring {
                    crate::session::archive::commit_restore(
                        record,
                        revision,
                        receipt.committed_at.as_str().to_string(),
                    )?;
                } else if !record.is_retired() {
                    crate::session::archive::commit_retire(
                        record,
                        revision,
                        before.runtime_generation,
                        true,
                        receipt.committed_at.as_str().to_string(),
                    )?;
                }
                anyhow::ensure!(
                    !record_has_runtime_claim(record),
                    "archive_runtime_claim_remaining"
                );
                receipt.stage = AgentArchiveStage::Committed;
                receipt.archive_kind = Some(if permanent {
                    AgentArchiveKind::Permanent
                } else if restoring {
                    AgentArchiveKind::Active
                } else {
                    AgentArchiveKind::Soft
                });
                receipt.restorable = Some(!permanent && !restoring);
                receipt.result = Some(record.clone());
                sessions
                    .agent_archive_receipts
                    .insert(request.action_id.to_string(), receipt.clone());
                if let Err(error) = crate::session::store::save_locked_session_store(path, sessions)
                {
                    receipt.stage = AgentArchiveStage::Uncertain;
                    receipt.error = Some(format!(
                        "durable_commit_uncertain_replay_same_action: {error:#}"
                    ));
                }
                self.save_agent_archive_receipt(&receipt)?;
                Ok(receipt)
            })
        })();
        outcome.map_err(|error| {
            error
                .downcast_ref::<AgentManagementError>()
                .cloned()
                .unwrap_or_else(|| AgentManagementError::OwnerActionRequired(error.to_string()))
        })
    }
}

fn require_free_restore_name(
    sessions: &crate::session::model::CutexSessionStore,
    record: &CutexSessionRecord,
) -> anyhow::Result<()> {
    if let Some(holder) = crate::session::metadata::live_formal_name_holder(sessions, record) {
        return Err(
            crate::session::archive::CutexSessionArchiveError::AgentNameTaken {
                name: record.formal_agent_name.clone().unwrap_or_default(),
                holder: holder.cutex_session_id.clone(),
            }
            .into(),
        );
    }
    Ok(())
}
