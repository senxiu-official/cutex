//! Same-row durable evidence, close audit and recoverable per-session outbox.
use super::super::agent_bus_settlement_model::{
    CloseOutcome, CloseRequest, Closure, Evidence, Refusal, SettlementRecord, SettlementView,
    EVENT_METHOD,
};
use super::*;

pub(super) const EVIDENCE_STORE: &str = "agent_bus_settlement_evidence";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationRow {
    version: u8,
    record: SettlementRecord,
}

fn current_record(stored: &StoredAgentBusMessage) -> Option<&SettlementRecord> {
    stored
        .settlement
        .as_ref()
        .or(stored.observed_settlement.as_ref())
}

/// Evidence is rebuildable and kept in its own namespace. Older writers can
/// ignore it without changing a single original agent_bus row or version.
pub(super) fn refresh_observations(cache: &mut CachedStore, database: &Path) -> anyhow::Result<()> {
    let generation = match super::super::hot_store::generation(database, EVIDENCE_STORE) {
        Ok(generation) => generation,
        Err(error) => {
            if !cache.store.observation_unavailable {
                eprintln!("settlement observation cache unavailable (business state remains authoritative): {error:#}");
            }
            cache.store.observation_unavailable = true;
            for stored in cache.store.messages.values_mut() {
                stored.observed_settlement = None;
            }
            return Ok(());
        }
    };
    if !cache.store.observation_unavailable && cache.store.observation_generation == generation {
        return Ok(());
    }
    for stored in cache.store.messages.values_mut() {
        stored.observed_settlement = None;
    }
    let result = (|| -> anyhow::Result<()> {
        if let Some(loaded) = super::super::hot_store::verified_rows(database, EVIDENCE_STORE)? {
            for ((collection, message), bytes) in loaded.rows {
                anyhow::ensure!(
                    collection == "evidence",
                    "unknown settlement evidence collection"
                );
                let row: ObservationRow = serde_json::from_slice(&bytes)?;
                row.record.evidence.validate_shape()?;
                anyhow::ensure!(
                    row.version == 1
                        && row.record.closure.is_none()
                        && row.record.request.is_none()
                        && row.record.audit.is_none()
                        && row.record.pending_sessions.is_empty(),
                    "invalid observation sidecar version or close data"
                );
                let Some(stored) = cache.store.messages.get_mut(&message) else {
                    continue;
                };
                if stored.settlement.is_some() {
                    continue;
                }
                // A rebuildable observation may outlive retention, an older writer
                // or changed native identity. It cannot block original delivery.
                if validate_settlement_record(stored, &row.record).is_err() {
                    continue;
                }
                stored.observed_settlement = Some(row.record);
            }
        }
        Ok(())
    })();
    cache.store.observation_unavailable = result.is_err();
    if let Err(error) = result {
        for stored in cache.store.messages.values_mut() {
            stored.observed_settlement = None;
        }
        eprintln!("settlement observation cache rejected (business state remains authoritative): {error:#}");
    }
    cache.store.observation_generation = generation;
    Ok(())
}

pub(super) fn settlement_closed(stored: &StoredAgentBusMessage) -> bool {
    stored
        .settlement
        .as_ref()
        .is_some_and(|record| record.closure.is_some())
}

fn ordinary(stored: &StoredAgentBusMessage) -> bool {
    stored.canonical_envelope.as_ref().is_some_and(|message| {
        message.kind == crate::agent_bus::model::AgentBusEnvelopeKind::Message
            && message.sender_kind.is_agent()
            && message.control_type.is_none()
            && message.external_action_id.is_none()
            && message.external_message_id.is_none()
    }) && stored.snapshot.from_cutex_session_id.is_some()
        && stored
            .snapshot
            .external_input
            .as_ref()
            .is_some_and(|envelope| {
                envelope.message.event_type == "agent_message"
                    && envelope.message.source.kind
                        == crate::app_server::external_input::SourceKind::Agent
                    && envelope.message.delivery
                        != crate::app_server::external_input::Delivery::Passive
            })
}

fn eligible(stored: &StoredAgentBusMessage) -> bool {
    ordinary(stored)
        && stored.snapshot.state == "inbox_accepted"
        && stored.snapshot.a4_receipt.is_none()
        && stored.snapshot.external_input_receipt.is_none()
        && stored
            .meta
            .as_ref()
            .is_none_or(|meta| meta.delivered_at.is_none())
        && stored
            .snapshot
            .external_input_last_observed
            .as_ref()
            .is_some_and(|status| {
                status.delivery_state == crate::app_server::external_input::DeliveryState::Pending
                    && status.receipt.is_none()
            })
}

pub(super) fn settlement_view(
    stored: &StoredAgentBusMessage,
    now: DateTime<Utc>,
) -> Option<SettlementView> {
    current_record(stored).map(|record| {
        let eligible = record.closure.is_none()
            && eligible(stored)
            && evidence_bound(stored, &record.evidence)
            && super::super::agent_bus_settlement_model::runtime_matches(&record.evidence)
                .unwrap_or(false);
        record.view(eligible, now)
    })
}

fn evidence_bound(stored: &StoredAgentBusMessage, evidence: &Evidence) -> bool {
    let Some(envelope) = &stored.snapshot.external_input else {
        return false;
    };
    let Some(local) = &stored.snapshot.local_inbox_receipt else {
        return false;
    };
    let Some(status) = &stored.snapshot.external_input_last_observed else {
        return false;
    };
    evidence.validate_shape().is_ok()
        && evidence.schema_version == 1
        && evidence.runtime == "claude"
        && evidence.recipient_id == stored.owner_cutex_session_id
        && evidence.recipient_id == stored.snapshot.to_cutex_session_id
        && evidence.recipient_id == envelope.owner_id
        && evidence.thread_id == envelope.thread_id
        && evidence.runtime_generation == envelope.runtime_generation
        && evidence.message_id == stored.snapshot.message_id
        && evidence.message_id == envelope.message.id
        && evidence.semantic_sha256 == envelope.semantic_sha256
        && local["threadId"] == evidence.thread_id
        && local["messageId"] == evidence.message_id
        && local["semanticSha256"] == evidence.local_semantic_sha256
        && status.message_id == evidence.message_id
        && status.semantic_sha256 == evidence.semantic_sha256
        && status.processing.attempt_id.as_deref() == Some(evidence.attempt_id.as_str())
        && match evidence.processing_state.as_str() {
            "held" => {
                status.processing.state == crate::app_server::external_input::ProcessingState::Held
            }
            "output_observed" => {
                status.processing.state
                    == crate::app_server::external_input::ProcessingState::OutputObserved
            }
            _ => false,
        }
        && !evidence.actual_turn_id.trim().is_empty()
        && uuid::Uuid::parse_str(&evidence.attempt_id).is_ok_and(|id| !id.is_nil())
        && matches!(
            evidence.processing_state.as_str(),
            "held" | "output_observed"
        )
        && matches!(
            evidence.terminal_status.as_str(),
            "interrupted" | "failed" | "completed"
        )
        && evidence.observation_sha256.len() == 64
        && evidence
            .observation_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

pub(super) fn validate_settlement_record(
    stored: &StoredAgentBusMessage,
    record: &SettlementRecord,
) -> anyhow::Result<()> {
    // Delivery may advance after close, but original frozen identity cannot.
    let envelope = stored
        .snapshot
        .external_input
        .as_ref()
        .context("settlement has no frozen native identity")?;
    record.evidence.validate_shape()?;
    anyhow::ensure!(
        record.evidence.schema_version == 1
            && record.evidence.message_id == stored.snapshot.message_id
            && record.evidence.recipient_id == stored.owner_cutex_session_id
            && record.evidence.thread_id == envelope.thread_id
            && record.evidence.semantic_sha256 == envelope.semantic_sha256
            && uuid::Uuid::parse_str(&record.evidence_token).is_ok(),
        "invalid settlement binding"
    );
    match (&record.closure, &record.request, &record.audit) {
        (Some(closure), Some(request), Some(audit)) => {
            anyhow::ensure!(
                request.message_id == stored.snapshot.message_id
                    && request.expected_evidence_token == record.evidence_token
                    && closure.request_id == request.request_id
                    && !closure.actor.is_empty()
                    && closure.reason == request.reason.trim()
                    && audit["requestId"] == request.request_id
                    && audit["actor"] == closure.actor
                    && audit["messageId"] == request.message_id,
                "invalid settlement closure audit"
            );
        }
        (None, None, None) => anyhow::ensure!(
            record.pending_sessions.is_empty(),
            "unclosed settlement has an outbox"
        ),
        _ => anyhow::bail!("partial settlement close/audit"),
    }
    Ok(())
}

fn conflict(code: &str, message: &str) -> Refusal {
    Refusal::new(409, code, message)
}

fn check_candidate(
    stored: &StoredAgentBusMessage,
    request: &CloseRequest,
    now: DateTime<Utc>,
) -> Result<(), Refusal> {
    if !ordinary(stored) {
        return Err(conflict(
            "message_protected",
            "only ordinary active agent messages with bound terminal evidence may be closed",
        ));
    }
    if !eligible(stored) || settlement_closed(stored) {
        return Err(conflict(
            "message_not_closable",
            "message delivery or follow-up changed; refresh this item",
        ));
    }
    let record = current_record(stored).ok_or_else(|| {
        conflict(
            "settlement_evidence_missing",
            "terminal evidence is unavailable",
        )
    })?;
    if record.evidence_token != request.expected_evidence_token {
        return Err(conflict(
            "settlement_evidence_changed",
            "terminal evidence changed; refresh this item",
        ));
    }
    if !record.evidence.fresh(now) {
        return Err(conflict(
            "settlement_evidence_expired",
            "terminal evidence expired; wait for reconciliation and refresh",
        ));
    }
    if !evidence_bound(stored, &record.evidence) {
        return Err(conflict(
            "settlement_evidence_changed",
            "message identity or processing attempt changed",
        ));
    }
    Ok(())
}

fn replay(
    store: &AgentBusMessageStore,
    request: &CloseRequest,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<Option<CloseOutcome>, Refusal> {
    for stored in store.messages.values() {
        let Some(record) = &stored.settlement else {
            continue;
        };
        let Some(prior) = &record.request else {
            continue;
        };
        if prior.request_id != request.request_id {
            continue;
        }
        if prior != request
            || record
                .closure
                .as_ref()
                .is_none_or(|closure| closure.actor != actor)
        {
            return Err(conflict(
                "request_id_conflict",
                "requestId was already used with a different request or actor",
            ));
        }
        return Ok(Some(CloseOutcome {
            message_id: request.message_id.clone(),
            changed: false,
            settlement: record.view(false, now),
        }));
    }
    Ok(None)
}

impl AgentBusMessageRepository {
    /// Source-derived kit guards: raw row bytes from an already-open read-only
    /// WAL snapshot. They do not open repositories, create locks, or deserialize
    /// business rows through a decoder that ignores future fields.
    pub(crate) fn validate_settlement_observation_rows(
        rows: &super::super::hot_store::Rows,
    ) -> anyhow::Result<()> {
        for ((collection, message), bytes) in rows {
            anyhow::ensure!(
                collection == "evidence",
                "unknown settlement evidence collection"
            );
            let row: ObservationRow = serde_json::from_slice(bytes)?;
            row.record.evidence.validate_shape()?;
            anyhow::ensure!(
                row.version == 1
                    && row.record.closure.is_none()
                    && row.record.request.is_none()
                    && row.record.audit.is_none()
                    && row.record.pending_sessions.is_empty()
                    && row.record.evidence.message_id == *message
                    && row.record.evidence.schema_version == 1
                    && uuid::Uuid::parse_str(&row.record.evidence_token).is_ok(),
                "invalid observation-only settlement namespace"
            );
        }
        Ok(())
    }

    pub(crate) fn validate_settlement_rollback_business_rows(
        rows: &super::super::hot_store::Rows,
        legacy_json: Option<&[u8]>,
    ) -> anyhow::Result<()> {
        let check_row = |row: &Value| -> anyhow::Result<()> {
            anyhow::ensure!(
                row.is_object(),
                "invalid message row during settlement rollback check"
            );
            anyhow::ensure!(
                row.get("settlement").is_none_or(Value::is_null)
                    && row
                        .pointer("/snapshot/settlement")
                        .is_none_or(Value::is_null),
                "message follow-up closure/evidence in business row requires the new reader"
            );
            Ok(())
        };
        if let Some(bytes) = rows.get(&("state".into(), "version".into())) {
            let version: Value = serde_json::from_slice(bytes)?;
            anyhow::ensure!(
                version
                    .as_u64()
                    .is_some_and(|version| (2..8).contains(&version)),
                "agent_bus format v8 or unknown format cannot be rolled back"
            );
        }
        for ((collection, _), bytes) in rows {
            if collection == "message" {
                check_row(&serde_json::from_slice(bytes)?)?;
            }
        }
        if let Some(bytes) = legacy_json {
            let legacy: Value = serde_json::from_slice(bytes)?;
            anyhow::ensure!(
                legacy["version"]
                    .as_u64()
                    .is_some_and(|version| (1..8).contains(&version)),
                "legacy agent_bus format v8 or unknown format cannot be rolled back"
            );
            for row in legacy["messages"]
                .as_object()
                .context("legacy agent_bus messages missing")?
                .values()
            {
                check_row(row)?;
            }
        }
        Ok(())
    }

    /// Kit allow-list guard for the rebuildable, non-business namespace.
    pub fn validate_settlement_observations_read_only(root: &Path) -> anyhow::Result<()> {
        let database = root.join(super::super::hot_store::DATABASE_FILE);
        if let Some(loaded) = super::super::hot_store::verified_rows(&database, EVIDENCE_STORE)? {
            Self::validate_settlement_observation_rows(&loaded.rows)?;
        }
        Ok(())
    }

    pub fn observe_settlement(
        &self,
        message_id: &str,
        evidence: Option<Evidence>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let changed = self.with_lock(|path| {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("agent-bus cache lock poisoned"))?;
            let (store, _) = fresh_store(&mut cache, path)?;
            let Some(stored) = store.messages.get(message_id) else {
                return Ok(false);
            };
            // Historical closure/audit stays even after runtime failure or a
            // valid late receipt. Reconciliation never reopens closed work.
            if settlement_closed(stored) {
                return Ok(false);
            }
            let evidence = evidence.filter(|evidence| {
                eligible(stored) && evidence_bound(stored, evidence) && evidence.fresh(now)
            });
            let prior = stored.observed_settlement.as_ref();
            let record = evidence.map(|evidence| {
                let token = prior
                    .filter(|old| old.evidence.same_identity(&evidence))
                    .map(|old| old.evidence_token.clone())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                SettlementRecord {
                    evidence_token: token,
                    evidence,
                    closure: None,
                    request: None,
                    audit: None,
                    pending_sessions: Vec::new(),
                }
            });
            let database = bus_database_path(path);
            let invalidated_raw_row =
                if prior.is_none() && record.is_none() && store.observation_unavailable {
                    super::super::hot_store::verified_rows(&database, EVIDENCE_STORE)?.is_some_and(
                        |loaded| {
                            loaded
                                .rows
                                .contains_key(&("evidence".into(), message_id.into()))
                        },
                    )
                } else {
                    false
                };
            if prior == record.as_ref() && !invalidated_raw_row {
                return Ok(false);
            }
            let change = match record {
                Some(record) => super::super::hot_store::RowChange::Put {
                    collection: "evidence".into(),
                    key: message_id.into(),
                    value: serde_json::to_vec(&ObservationRow { version: 1, record })?,
                },
                None => super::super::hot_store::RowChange::Delete {
                    collection: "evidence".into(),
                    key: message_id.into(),
                },
            };
            let generation =
                super::super::hot_store::generation(&database, EVIDENCE_STORE)?.unwrap_or(0);
            super::super::hot_store::commit(&database, EVIDENCE_STORE, generation, vec![change])?;
            Ok(true)
        })?;
        if changed {
            // Rebuild the two-generation index and wake this process's admission
            // worker. Other processes detect the sidecar generation separately.
            self.refresh_admission_if_changed()?;
            // Observation notifications contain no business side effects. A
            // failed publication is repaired by the next controlled refresh
            // or authoritative bootstrap/list read; close uses its durable
            // per-session outbox below.
            let event = self.read(|store| Ok(store.messages.get(message_id).map(|stored| {
                let view = settlement_view(stored, Utc::now());
                let notification_id = view.as_ref().and_then(|view| view.closure.as_ref().map(|closure| closure.request_id.clone()))
                    .or_else(|| view.as_ref().map(|view| view.evidence_token.clone())).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                (both_streams(stored), message_id.to_string(), EVENT_METHOD, json!({"messageId":message_id,"requestId":notification_id,"settlement":view}))
            })))?;
            if let Err(error) = emit_both(event) {
                eprintln!(
                    "settlement observation notification will repair on refresh/read: {error:#}"
                );
            }
        }
        Ok(changed)
    }

    pub fn settlement_replay(
        &self,
        request: &CloseRequest,
        actor: &str,
    ) -> anyhow::Result<Result<Option<CloseOutcome>, Refusal>> {
        request
            .validate()
            .map_err(|error| anyhow::anyhow!(error.message))?;
        self.read(|store| Ok(replay(store, request, actor, Utc::now())))
    }

    pub fn settlement_close_candidate(
        &self,
        request: &CloseRequest,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Result<AgentBusMessageSnapshot, Refusal>> {
        self.read(|store| {
            if !store.messages.contains_key(&request.message_id) {
                return Ok(Err(Refusal::new(
                    404,
                    "message_not_found",
                    "no durable message has this id",
                )));
            }
            if store.observation_unavailable
                && store
                    .messages
                    .get(&request.message_id)
                    .is_some_and(eligible)
            {
                return Ok(Err(Refusal::new(
                    503,
                    "settlement_observation_unavailable",
                    "settlement observation cache is unavailable",
                )));
            }
            Ok(match store.messages.get(&request.message_id) {
                Some(stored) => {
                    check_candidate(stored, request, now).map(|()| stored.snapshot.clone())
                }
                None => Err(Refusal::new(
                    404,
                    "message_not_found",
                    "no durable message has this id",
                )),
            })
        })
    }

    #[cfg(test)]
    pub fn close_settlement(
        &self,
        request: &CloseRequest,
        actor: &str,
        fresh: &Evidence,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Result<CloseOutcome, Refusal>> {
        self.close_settlement_fenced(request, actor, fresh, now, &|_| Ok(true))
    }

    pub fn close_settlement_fenced(
        &self,
        request: &CloseRequest,
        actor: &str,
        fresh: &Evidence,
        now: DateTime<Utc>,
        runtime_fence: &dyn Fn(&Evidence) -> anyhow::Result<bool>,
    ) -> anyhow::Result<Result<CloseOutcome, Refusal>> {
        self.close_settlement_with_clock(
            request,
            actor,
            fresh,
            &|| now.max(Utc::now()),
            runtime_fence,
        )
    }

    fn close_settlement_with_clock(
        &self,
        request: &CloseRequest,
        actor: &str,
        fresh: &Evidence,
        clock: &dyn Fn() -> DateTime<Utc>,
        runtime_fence: &dyn Fn(&Evidence) -> anyhow::Result<bool>,
    ) -> anyhow::Result<Result<CloseOutcome, Refusal>> {
        if let Err(error) = request.validate() {
            return Ok(Err(error));
        }
        if actor.trim().is_empty() {
            return Ok(Err(Refusal::new(
                403,
                "unauthorized",
                "an authenticated owner actor is required",
            )));
        }
        self.update_message(&request.message_id, |store| {
            let now = clock();
            match replay(store, request, actor, now) {
                Ok(Some(outcome)) => return Ok((Ok(outcome), false)),
                Err(error) => return Ok((Err(error), false)),
                Ok(None) => {}
            }
            if store.observation_unavailable {
                return Ok((Err(Refusal::new(503, "settlement_observation_unavailable", "settlement observation cache is unavailable")), false));
            }
            let Some(stored) = store.messages.get_mut(&request.message_id) else {
                return Ok((Err(Refusal::new(404, "message_not_found", "no durable message has this id")), false));
            };
            if let Err(error) = check_candidate(stored, request, now) { return Ok((Err(error), false)); }
            let record = current_record(stored).expect("candidate checked");
            if !fresh.fresh(now) || !evidence_bound(stored, fresh) || !record.evidence.same_identity(fresh) {
                return Ok((Err(conflict("settlement_evidence_changed", "fresh runtime evidence differs; refresh this item")), false));
            }
            match runtime_fence(fresh) {
                Ok(true) => {}
                Ok(false) => return Ok((Err(conflict("settlement_runtime_changed", "runtime identity changed before close commit")), false)),
                Err(_) => return Ok((Err(Refusal::new(503, "settlement_observation_unavailable", "runtime identity unavailable before close commit")), false)),
            }
            let now = now.max(clock());
            if !record.evidence.fresh(now) || !fresh.fresh(now) {
                return Ok((Err(conflict("settlement_evidence_expired", "terminal evidence expired before close commit")), false));
            }
            let closure = Closure { actor: actor.into(), closed_at: now, reason: request.reason.trim().into(), request_id: request.request_id.clone() };
            use sha2::Digest;
            let evidence_digest = format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(fresh)?));
            let request_digest = format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(request)?));
            let audit = json!({
                "event": "owner_message_followup_closed", "actor": actor, "closedAt": now,
                "messageId": request.message_id, "requestId": request.request_id,
                "request": request, "reason": closure.reason, "previousState": stored.snapshot.state,
                "requestSha256": request_digest, "evidenceSha256": evidence_digest,
                "canonicalSemanticSha256": stored.snapshot.semantic_sha256,
                "nativeSemanticSha256": fresh.semantic_sha256, "localSemanticSha256": fresh.local_semantic_sha256,
                "evidenceToken": record.evidence_token, "evidence": fresh,
                "fromCutexSessionId": stored.snapshot.from_cutex_session_id,
                "toCutexSessionId": stored.snapshot.to_cutex_session_id,
            });
            let mut sessions = vec![stored.owner_cutex_session_id.clone()];
            if let Some(sender) = &stored.snapshot.from_cutex_session_id { sessions.push(sender.clone()); }
            sessions.sort(); sessions.dedup();
            stored.settlement = Some(current_record(stored).expect("candidate checked").clone());
            let record = stored.settlement.as_mut().expect("candidate checked");
            record.evidence = fresh.clone();
            record.closure = Some(closure);
            record.request = Some(request.clone());
            record.audit = Some(audit);
            record.pending_sessions = sessions;
            stored.updated_at = now.to_rfc3339();
            let outcome = CloseOutcome { message_id: request.message_id.clone(), changed: true, settlement: record.view(false, now) };
            store.version = store.version.max(8);
            Ok((Ok(outcome), true))
        })
    }

    pub fn settlement_view_by_id(
        &self,
        message_id: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<SettlementView>> {
        self.read(|store| {
            Ok(store
                .messages
                .get(message_id)
                .and_then(|stored| settlement_view(stored, now)))
        })
    }

    /// Per-target acknowledgment allows recovery after only one session stream
    /// was published. Publication may repeat after a crash; requestId is stable.
    pub fn drain_settlement_outbox(
        &self,
        publish: &dyn Fn(&str, &str, Value) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let pending = self.read(|store| {
            Ok(store
                .messages
                .values()
                .filter_map(|stored| {
                    let record = stored.settlement.as_ref()?;
                    if record.pending_sessions.is_empty() {
                        return None;
                    }
                    let closure = record.closure.as_ref()?;
                    Some((
                        stored.snapshot.message_id.clone(),
                        closure.request_id.clone(),
                        record.pending_sessions.clone(),
                        record.view(false, Utc::now()),
                    ))
                })
                .collect::<Vec<_>>())
        })?;
        let mut errors = Vec::new();
        for (message, request_id, sessions, view) in pending {
            for session in sessions {
                let params =
                    json!({"messageId": message, "requestId": request_id, "settlement": view});
                if let Err(error) = publish(&session, &message, params) {
                    errors.push(error.to_string());
                    continue;
                }
                self.update_message(&message, |store| {
                    let record = store
                        .messages
                        .get_mut(&message)
                        .and_then(|stored| stored.settlement.as_mut())
                        .context("settlement outbox disappeared")?;
                    anyhow::ensure!(
                        record
                            .closure
                            .as_ref()
                            .is_some_and(|closure| closure.request_id == request_id),
                        "settlement outbox changed"
                    );
                    let before = record.pending_sessions.len();
                    record
                        .pending_sessions
                        .retain(|pending| pending != &session);
                    Ok(((), before != record.pending_sessions.len()))
                })?;
            }
        }
        anyhow::ensure!(
            errors.is_empty(),
            "settlement notifications pending: {}",
            errors.join("; ")
        );
        Ok(())
    }

    pub fn flush_settlement_outbox(&self) -> anyhow::Result<()> {
        self.drain_settlement_outbox(&|session, message, params| {
            append_event(session, message, EVENT_METHOD, params)
        })
    }

    /// r263/r264 cannot preserve format-v8 settlement. This check never opens
    /// a writer or migrates state. Kits must run it before any rollback writes.
    pub fn validate_settlement_rollback_read_only(root: &Path) -> anyhow::Result<()> {
        let path = root.join(AGENT_BUS_STATE_FILE);
        let loaded =
            super::super::hot_store::verified_rows(&bus_database_path(&path), "agent_bus")?;
        let legacy = if path.exists() {
            Some(fs::read(path)?)
        } else {
            None
        };
        Self::validate_settlement_rollback_business_rows(
            &loaded.map(|loaded| loaded.rows).unwrap_or_default(),
            legacy.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::external_input::*;
    use crate::management::v2::hot_store;
    const TO: &str = "cutex.11111111-1111-4111-8111-111111111111";
    const FROM: &str = "cutex.22222222-2222-4222-8222-222222222222";

    struct Fixture {
        repo: AgentBusMessageRepository,
        root: PathBuf,
        now: DateTime<Utc>,
    }
    impl Fixture {
        fn new() -> Self {
            super::super::super::agent_bus_settlement_model::register_runtime_fence(|evidence| {
                Ok(evidence.actual_turn_id != "unavailable")
            });
            let root =
                std::env::temp_dir().join(format!("settlement-state-{}", uuid::Uuid::new_v4()));
            Self {
                repo: AgentBusMessageRepository::open(&root).unwrap(),
                root,
                now: Utc::now(),
            }
        }
        fn seed(&self, id: &str, terminal: &str, output: bool) -> Evidence {
            let message: AgentBusMessage = serde_json::from_value(json!({
                "id": id, "from":FROM,"to":TO,"fromCutexSessionId":FROM,"toCutexSessionId":TO,
                "content":"fixture original body","deliveryMode":"after_turn","triggerTurn":true,
                "createdAtEpochSecs":1,"senderKind":"agent"
            }))
            .unwrap();
            self.repo
                .record_queued_isolated(AgentBusQueuedMessage {
                    owner_cutex_session_id: TO.into(),
                    message_id: id.into(),
                    from_cutex_session_id: Some(FROM.into()),
                    to_cutex_session_id: TO.into(),
                    from_runtime_agent_id: Some("sender".into()),
                    to_runtime_agent_id: Some("recipient".into()),
                    delivery_mode: "after_turn".into(),
                    content: message.content.clone(),
                    queued_at: self.now,
                    canonical_envelope: message,
                    semantic_sha256: "c".repeat(64),
                })
                .unwrap();
            let envelope = self
                .repo
                .freeze_external_input(TO, id, |message| {
                    let mut envelope = Envelope {
                        version: 1,
                        owner_id: TO.into(),
                        thread_id: "thread".into(),
                        runtime_generation: 7,
                        message: Message {
                            id: id.into(),
                            source: Source {
                                kind: SourceKind::Agent,
                                id: FROM.into(),
                            },
                            event_type: "agent_message".into(),
                            delivery: Delivery::AfterTurn,
                            text: message.content.clone(),
                        },
                        view: None,
                        semantic_sha256: String::new(),
                    };
                    envelope.semantic_sha256 = envelope.digest();
                    Ok(envelope)
                })
                .unwrap();
            self.repo.record_local_inbox(TO,id,&json!({"messageId":id,"threadId":"thread","turnId":"reserved",
                "semanticSha256":"b".repeat(64),"durable":true,"receiptScope":"local-inbox","state":"queued"})).unwrap();
            let attempt = uuid::Uuid::new_v4().to_string();
            let status = Status {
                message_id: id.into(),
                semantic_sha256: envelope.semantic_sha256.clone(),
                delivery_state: DeliveryState::Pending,
                receipt: None,
                processing: ProcessingStatus {
                    state: if output {
                        ProcessingState::OutputObserved
                    } else {
                        ProcessingState::Held
                    },
                    attempt_id: Some(attempt.clone()),
                    reason: if output {
                        None
                    } else {
                        Some(HoldReason::RequestUncertain)
                    },
                },
            };
            self.repo
                .observe_external_input(id, &envelope, &status)
                .unwrap();
            let evidence = Evidence {
                observing_generation: None,
                schema_version: 1,
                runtime: "claude".into(),
                recipient_id: TO.into(),
                runtime_generation: 7,
                thread_id: "thread".into(),
                message_id: id.into(),
                semantic_sha256: envelope.semantic_sha256,
                local_semantic_sha256: "b".repeat(64),
                attempt_id: attempt,
                processing_state: if output { "output_observed" } else { "held" }.into(),
                actual_turn_id: "actual".into(),
                terminal_status: terminal.into(),
                observed_at: self.now,
                observation_sha256: "a".repeat(64),
            };
            assert!(self
                .repo
                .observe_settlement(id, Some(evidence.clone()), self.now)
                .unwrap());
            evidence
        }
        fn request(&self, id: &str, key: &str) -> CloseRequest {
            CloseRequest {
                message_id: id.into(),
                expected_evidence_token: self
                    .repo
                    .settlement_view_by_id(id, self.now)
                    .unwrap()
                    .unwrap()
                    .evidence_token,
                request_id: key.into(),
                reason: "Owner ended follow-up; outcome remains unknown".into(),
            }
        }
        fn stored(&self, id: &str) -> StoredAgentBusMessage {
            self.repo.get(id).unwrap().unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn observation_sidecar_preserves_original_version_rows_and_rollback_until_owner_close() {
        let f = Fixture::new();
        let evidence = f.seed("m", "failed", false);
        let loaded = hot_store::verified_rows(&f.root.join(hot_store::DATABASE_FILE), "agent_bus")
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<u8>(&loaded.rows[&("state".into(), "version".into())])
                .unwrap(),
            3
        );
        let row: Value =
            serde_json::from_slice(&loaded.rows[&("message".into(), "m".into())]).unwrap();
        assert!(row.get("settlement").is_none());
        assert!(row.get("observedSettlement").is_none());
        assert!(row["snapshot"].get("settlement").is_none());
        assert!(
            hot_store::verified_rows(&f.root.join(hot_store::DATABASE_FILE), EVIDENCE_STORE)
                .unwrap()
                .is_some()
        );
        let mut renewed = evidence.clone();
        renewed.observed_at = f.now + chrono::Duration::seconds(1);
        f.repo
            .observe_settlement("m", Some(renewed), f.now + chrono::Duration::seconds(1))
            .unwrap();
        let after = hot_store::verified_rows(&f.root.join(hot_store::DATABASE_FILE), "agent_bus")
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.rows, after.rows,
            "automatic observations do not change any original business row"
        );
        assert_eq!(loaded.generation, after.generation);
        AgentBusMessageRepository::validate_settlement_observations_read_only(&f.root).unwrap();
        AgentBusMessageRepository::validate_settlement_rollback_read_only(&f.root).unwrap();
        let request = f.request("m", "close-1");
        f.repo
            .close_settlement(
                &request,
                "authenticated-owner",
                &evidence,
                f.now + chrono::Duration::seconds(1),
            )
            .unwrap()
            .unwrap();
        assert!(
            AgentBusMessageRepository::validate_settlement_rollback_read_only(&f.root).is_err()
        );
        assert!(f.repo.verify_legacy_export().is_err());
        assert!(f.repo.export_legacy_json().is_err());
        assert_eq!(f.repo.read(|store| Ok(store.version)).unwrap(), 8);
    }

    #[test]
    fn evidence_states_projection_lease_and_identity_conflicts() {
        for (id, state, output) in [
            ("interrupted", "interrupted", true),
            ("failed", "failed", false),
            ("completed", "completed", false),
        ] {
            let f = Fixture::new();
            let evidence = f.seed(id, state, output);
            let request = f.request(id, "close");
            let view = f.repo.settlement_view_by_id(id, f.now).unwrap().unwrap();
            assert_eq!(view.state, "outcome_unknown");
            assert!(view.can_close);
            assert_eq!(
                f.repo.snapshot(TO).unwrap()[0]["settlement"],
                serde_json::to_value(&view).unwrap()
            );
            assert_eq!(
                f.repo.stuck_view_by_id(id, 1).unwrap().unwrap().settlement,
                Some(view)
            );
            let steps = f
                .repo
                .pending_admission_messages(TO, &crate::asks::store::AskState::default())
                .unwrap();
            assert_eq!(
                steps[0].settlement.as_ref().unwrap().state,
                "outcome_unknown"
            );
            assert!(
                !f.repo
                    .settlement_view_by_id(id, f.now + chrono::Duration::seconds(181))
                    .unwrap()
                    .unwrap()
                    .can_close
            );
            assert_eq!(
                f.repo
                    .settlement_close_candidate(&request, f.now + chrono::Duration::seconds(181))
                    .unwrap()
                    .unwrap_err()
                    .status,
                409
            );
            for mut changed in [
                evidence.clone(),
                evidence.clone(),
                evidence.clone(),
                evidence.clone(),
            ]
            .into_iter()
            .enumerate()
            {
                match changed.0 {
                    0 => changed.1.runtime_generation += 1,
                    1 => changed.1.actual_turn_id = "reserved".into(),
                    2 => changed.1.semantic_sha256 = "0".repeat(64),
                    _ => changed.1.attempt_id = uuid::Uuid::new_v4().to_string(),
                }
                assert_eq!(
                    f.repo
                        .close_settlement(&request, "owner", &changed.1, f.now)
                        .unwrap()
                        .unwrap_err()
                        .status,
                    409
                );
            }
            f.repo.observe_settlement(id, None, f.now).unwrap();
            assert!(f.repo.settlement_view_by_id(id, f.now).unwrap().is_none());
        }
    }

    #[test]
    fn close_is_atomic_audited_idempotent_and_restored_without_resend() {
        let f = Fixture::new();
        let evidence = f.seed("m", "interrupted", true);
        let request = f.request("m", "stable-key");
        let before = f.stored("m");
        let out = f
            .repo
            .close_settlement(&request, "device-owner", &evidence, f.now)
            .unwrap()
            .unwrap();
        assert!(out.changed);
        let after = f.stored("m");
        assert_eq!(before.snapshot, after.snapshot);
        assert!(f.repo.suppress_native_delivery(TO, "m").unwrap());
        assert!(f.repo.pending_v2().unwrap().is_empty());
        assert!(f
            .repo
            .pending_admission_messages(TO, &crate::asks::store::AskState::default())
            .unwrap()
            .is_empty());
        assert!(f.repo.stuck_messages(0, 1).unwrap().is_empty());
        assert_eq!(
            f.repo.pending_native_envelopes(TO).unwrap().len(),
            1,
            "late receipt reconciliation stays available"
        );
        let audit = after.settlement.as_ref().unwrap().audit.as_ref().unwrap();
        assert_eq!(audit["actor"], "device-owner");
        assert_eq!(audit["previousState"], "inbox_accepted");
        assert_eq!(audit["evidenceSha256"].as_str().unwrap().len(), 64);
        assert_eq!(audit["requestSha256"].as_str().unwrap().len(), 64);
        assert_eq!(after.settlement.as_ref().unwrap().pending_sessions.len(), 2);
        let reopened = AgentBusMessageRepository::open(&f.root).unwrap();
        assert_eq!(
            reopened
                .settlement_view_by_id("m", f.now + chrono::Duration::days(99))
                .unwrap()
                .unwrap()
                .state,
            "closed"
        );
        assert!(
            !reopened
                .settlement_replay(&request, "device-owner")
                .unwrap()
                .unwrap()
                .unwrap()
                .changed
        );
        let mut wrong = request.clone();
        wrong.reason = "different".into();
        assert_eq!(
            reopened
                .settlement_replay(&wrong, "device-owner")
                .unwrap()
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            reopened
                .settlement_replay(&request, "different-device")
                .unwrap()
                .unwrap_err()
                .status,
            409
        );
        assert!(
            f.repo
                .finished_before(f.now + chrono::Duration::days(99))
                .unwrap()
                .is_empty(),
            "immutable audit is not aged out"
        );
    }

    #[test]
    fn transaction_failure_leaves_no_closure_audit_outbox_or_format_change_and_same_key_recovers() {
        let f = Fixture::new();
        let evidence = f.seed("m", "failed", false);
        let request = f.request("m", "retry-key");
        let db = rusqlite::Connection::open(f.root.join(hot_store::DATABASE_FILE)).unwrap();
        db.execute_batch("CREATE TRIGGER fail_close BEFORE UPDATE OF value_json ON hot_store_rows WHEN NEW.store='agent_bus' AND NEW.collection='state' BEGIN SELECT RAISE(ABORT,'injected format write failure'); END;").unwrap();
        assert!(f
            .repo
            .close_settlement(&request, "owner", &evidence, f.now)
            .is_err());
        assert!(f.stored("m").settlement.is_none());
        assert_eq!(f.repo.read(|store| Ok(store.version)).unwrap(), 3);
        assert!(f
            .repo
            .settlement_replay(&request, "owner")
            .unwrap()
            .unwrap()
            .is_none());
        db.execute_batch("DROP TRIGGER fail_close").unwrap();
        assert!(
            f.repo
                .close_settlement(&request, "owner", &evidence, f.now)
                .unwrap()
                .unwrap()
                .changed
        );
    }

    #[test]
    fn partial_session_notification_failure_restarts_and_repairs_without_business_rewrite() {
        let f = Fixture::new();
        let evidence = f.seed("m", "failed", false);
        let request = f.request("m", "outbox-key");
        f.repo
            .close_settlement(&request, "owner", &evidence, f.now)
            .unwrap()
            .unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        assert!(f
            .repo
            .drain_settlement_outbox(&|session, _, params| {
                calls.borrow_mut().push((session.to_string(), params));
                if session == TO {
                    anyhow::bail!("injected event failure")
                }
                Ok(())
            })
            .is_err());
        assert_eq!(calls.borrow().len(), 2);
        assert_eq!(
            f.stored("m").settlement.unwrap().pending_sessions,
            vec![TO.to_string()]
        );
        let reopened = AgentBusMessageRepository::open(&f.root).unwrap();
        let recovered = std::cell::RefCell::new(Vec::new());
        reopened
            .drain_settlement_outbox(&|session, _, params| {
                recovered.borrow_mut().push((session.to_string(), params));
                Ok(())
            })
            .unwrap();
        assert_eq!(recovered.borrow().len(), 1);
        assert_eq!(recovered.borrow()[0].0, TO);
        assert_eq!(recovered.borrow()[0].1["requestId"], request.request_id);
        reopened
            .drain_settlement_outbox(&|_, _, _| panic!("already delivered outbox repeated"))
            .unwrap();
        assert!(
            !reopened
                .settlement_replay(&request, "owner")
                .unwrap()
                .unwrap()
                .unwrap()
                .changed
        );
    }

    #[test]
    fn valid_receipt_before_close_conflicts_and_late_receipt_preserves_closed_audit() {
        let f = Fixture::new();
        let evidence = f.seed("before", "failed", false);
        let request = f.request("before", "before-key");
        let _ = super::super::capture_events(|| {
            f.repo
                .record_context_injected(TO, "before", "actual")
                .unwrap()
        });
        assert_eq!(
            f.repo
                .close_settlement(&request, "owner", &evidence, f.now)
                .unwrap()
                .unwrap_err()
                .status,
            409
        );
        let evidence = f.seed("after", "interrupted", true);
        let request = f.request("after", "after-key");
        f.repo
            .close_settlement(&request, "owner", &evidence, f.now)
            .unwrap()
            .unwrap();
        let audit = f.stored("after").settlement.unwrap().audit;
        let (_, events) = super::super::capture_events(|| {
            f.repo
                .record_context_injected(TO, "after", "actual")
                .unwrap()
        });
        assert_eq!(events.len(), 2);
        assert_eq!(f.stored("after").snapshot.state, "delivered");
        assert_eq!(f.stored("after").settlement.unwrap().audit, audit);
        f.repo.observe_settlement("after", None, f.now).unwrap();
        assert_eq!(
            f.repo
                .settlement_view_by_id("after", f.now)
                .unwrap()
                .unwrap()
                .state,
            "closed"
        );
        assert!(f
            .repo
            .pending_admission_messages(TO, &crate::asks::store::AskState::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn commit_clock_rechecks_lease_after_collection_lock_wait_and_runtime_fence() {
        let f = Fixture::new();
        let mut evidence = f.seed("fresh", "failed", false);
        let request = f.request("fresh", "fresh-key");
        // The collector finishes after the HTTP request began. Its observation
        // is fresh relative to commit, not a future timestamp versus request.
        evidence.observed_at = f.now + chrono::Duration::seconds(2);
        let out = f
            .repo
            .close_settlement_with_clock(
                &request,
                "owner",
                &evidence,
                &|| f.now + chrono::Duration::seconds(3),
                &|_| Ok(true),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            out.settlement.closure.unwrap().closed_at,
            f.now + chrono::Duration::seconds(3)
        );

        let evidence = f.seed("waited", "failed", false);
        let request = f.request("waited", "waited-key");
        let error = f
            .repo
            .close_settlement_with_clock(
                &request,
                "owner",
                &evidence,
                &|| f.now + chrono::Duration::seconds(181),
                &|_| panic!("expired before runtime fence"),
            )
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "settlement_evidence_expired");
        assert!(f.stored("waited").settlement.is_none());

        let clock_calls = std::cell::Cell::new(0);
        let error = f
            .repo
            .close_settlement_with_clock(
                &request,
                "owner",
                &evidence,
                &|| {
                    let call = clock_calls.get();
                    clock_calls.set(call + 1);
                    f.now + chrono::Duration::seconds(if call == 0 { 0 } else { 181 })
                },
                &|_| Ok(true),
            )
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "settlement_evidence_expired");
        assert_eq!(clock_calls.get(), 2);
        assert!(f.stored("waited").settlement.is_none());
    }

    #[test]
    fn observations_publish_both_session_projections_and_clear_without_erasing_closure() {
        let f = Fixture::new();
        let (evidence, events) = super::super::capture_events(|| f.seed("m", "failed", false));
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|(_, method, params)| method == EVENT_METHOD
                && params["settlement"]["state"] == "outcome_unknown"));
        for (_, method, params) in &events {
            super::super::super::contract_validation::validate_cutex_event_message(
                &json!({"method":method,"params":params}),
            )
            .unwrap();
        }
        let received = f.repo.snapshot(TO).unwrap()[0]["settlement"].clone();
        assert_eq!(
            f.repo.sent_snapshot(FROM, 20).unwrap()[0]["settlement"],
            received
        );
        let (_, cleared) =
            super::super::capture_events(|| f.repo.observe_settlement("m", None, f.now).unwrap());
        assert_eq!(cleared.len(), 2);
        assert!(cleared
            .iter()
            .all(|(_, _, params)| params["settlement"].is_null()));
        for (_, method, params) in &cleared {
            super::super::super::contract_validation::validate_cutex_event_message(
                &json!({"method":method,"params":params}),
            )
            .unwrap();
        }
        assert!(f.repo.sent_snapshot(FROM, 20).unwrap()[0]
            .get("settlement")
            .is_none());
        let _ = super::super::capture_events(|| {
            f.repo
                .observe_settlement("m", Some(evidence.clone()), f.now)
                .unwrap()
        });
        let request = f.request("m", "closed-observation");
        f.repo
            .close_settlement(&request, "owner", &evidence, f.now)
            .unwrap()
            .unwrap();
        let (_, events) =
            super::super::capture_events(|| f.repo.observe_settlement("m", None, f.now).unwrap());
        assert!(events.is_empty());
        assert_eq!(
            f.repo.sent_snapshot(FROM, 20).unwrap()[0]["settlement"]["state"],
            "closed"
        );
    }

    #[test]
    fn changed_or_unavailable_runtime_under_write_lock_refuses_without_close() {
        let f = Fixture::new();
        let evidence = f.seed("m", "failed", false);
        let request = f.request("m", "generation-key");
        assert_eq!(
            f.repo
                .close_settlement_fenced(&request, "owner", &evidence, f.now, &|_| Ok(false))
                .unwrap()
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            f.repo
                .close_settlement_fenced(&request, "owner", &evidence, f.now, &|_| anyhow::bail!(
                    "runtime disappeared"
                ))
                .unwrap()
                .unwrap_err()
                .status,
            503
        );
        assert!(f.stored("m").settlement.is_none());
        assert_eq!(f.repo.read(|store| Ok(store.version)).unwrap(), 3);
    }

    #[test]
    fn independent_sidecar_generation_invalidates_other_repository_admission_cache() {
        let f = Fixture::new();
        let evidence = f.seed("m", "failed", false);
        let other = AgentBusMessageRepository::open(&f.root).unwrap();
        assert!(f
            .repo
            .pending_admission_messages(TO, &crate::asks::store::AskState::default())
            .unwrap()[0]
            .settlement
            .is_some());
        let generation =
            hot_store::generation(&f.root.join(hot_store::DATABASE_FILE), "agent_bus").unwrap();
        other.observe_settlement("m", None, f.now).unwrap();
        assert_eq!(
            hot_store::generation(&f.root.join(hot_store::DATABASE_FILE), "agent_bus").unwrap(),
            generation
        );
        assert!(f.repo.refresh_admission_if_changed().unwrap());
        assert!(!f.repo.refresh_admission_if_changed().unwrap());
        assert!(f
            .repo
            .pending_admission_messages(TO, &crate::asks::store::AskState::default())
            .unwrap()[0]
            .settlement
            .is_none());
        other
            .observe_settlement("m", Some(evidence.clone()), f.now)
            .unwrap();
        assert!(f.repo.refresh_admission_if_changed().unwrap());
        let request = f.request("m", "close-other-writer");
        other
            .close_settlement(&request, "owner", &evidence, f.now)
            .unwrap()
            .unwrap();
        assert!(f.repo.refresh_admission_if_changed().unwrap());
        assert!(f
            .repo
            .pending_admission_messages(TO, &crate::asks::store::AskState::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn damaged_rebuildable_observation_does_not_block_original_receipts_and_can_be_invalidated() {
        let f = Fixture::new();
        f.seed("m", "failed", false);
        let request = f.request("m", "damaged-cache");
        let database = f.root.join(hot_store::DATABASE_FILE);
        let loaded = hot_store::verified_rows(&database, EVIDENCE_STORE)
            .unwrap()
            .unwrap();
        let mut row: Value =
            serde_json::from_slice(&loaded.rows[&("evidence".into(), "m".into())]).unwrap();
        row["record"]["evidence"]["observationSha256"] = json!("invalid-cache-digest");
        hot_store::commit(
            &database,
            EVIDENCE_STORE,
            loaded.generation,
            vec![hot_store::RowChange::Put {
                collection: "evidence".into(),
                key: "m".into(),
                value: serde_json::to_vec(&row).unwrap(),
            }],
        )
        .unwrap();
        assert!(f.repo.snapshot(TO).unwrap()[0].get("settlement").is_none());
        assert_eq!(
            f.repo
                .settlement_close_candidate(&request, f.now)
                .unwrap()
                .unwrap_err()
                .status,
            503
        );
        let (_, events) = super::super::capture_events(|| {
            f.repo.record_context_injected(TO, "m", "actual").unwrap()
        });
        assert_eq!(events.len(), 2);
        assert_eq!(f.stored("m").snapshot.state, "delivered");
        assert_eq!(f.repo.read(|store| Ok(store.version)).unwrap(), 3);
        let _ =
            super::super::capture_events(|| f.repo.observe_settlement("m", None, f.now).unwrap());
        assert!(!f
            .repo
            .read(|store| Ok(store.observation_unavailable))
            .unwrap());
        AgentBusMessageRepository::validate_settlement_observations_read_only(&f.root).unwrap();
    }

    #[test]
    fn unavailable_observation_recovers_without_a_new_sidecar_generation() {
        let f=Fixture::new();
        f.seed("m","failed",false);
        assert!(f.repo.snapshot(TO).unwrap()[0].get("settlement").is_some());
        let database=f.root.join(hot_store::DATABASE_FILE);
        let generation=hot_store::generation(&database,EVIDENCE_STORE).unwrap();
        {
            let mut cache=f.repo.cache.lock().unwrap();
            let cached=cache.as_mut().unwrap();
            // Exact state retained by a transient probe/read failure.
            cached.store.observation_unavailable=true;
            for record in cached.store.messages.values_mut(){record.observed_settlement=None;}
            refresh_observations(cached,&database).unwrap();
            assert!(!cached.store.observation_unavailable);
            assert!(cached.store.messages["m"].observed_settlement.is_some());
            assert_eq!(cached.store.observation_generation,generation);
        }
        assert_eq!(hot_store::generation(&database,EVIDENCE_STORE).unwrap(),generation);
    }

    #[test]
    fn historical_submission_and_current_observer_are_separately_bound() {
        let f=Fixture::new();
        let mut evidence=f.seed("m","interrupted",true);
        let frozen=f.stored("m").snapshot.external_input.unwrap();
        let old=evidence.clone();
        evidence.observing_generation=Some(9);
        assert_eq!(evidence.runtime_generation,7);
        assert_eq!(evidence.observer_generation(),9);
        assert!(!evidence.same_identity(&old));
        assert!(evidence_bound(&f.stored("m"),&evidence));
        f.repo.observe_settlement("m",Some(evidence.clone()),f.now).unwrap();
        assert_eq!(f.stored("m").snapshot.external_input.as_ref(),Some(&frozen));
        evidence.runtime_generation=9;
        assert!(!evidence_bound(&f.stored("m"),&evidence));
    }

    #[test]
    fn actual_process_restart_replays_offline_and_recovers_only_unpublished_target() {
        let f = Fixture::new(); let evidence = f.seed("m", "failed", false); let request = f.request("m", "process-restart-key");
        f.repo.close_settlement(&request,"owner",&evidence,f.now).unwrap().unwrap();
        assert!(f.repo.drain_settlement_outbox(&|session,_,_| {
            if session == TO { anyhow::bail!("publication interrupted before recipient") } Ok(())
        }).is_err());
        fs::write(f.root.join("restart-request.json"),serde_json::to_vec(&request).unwrap()).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored","--exact","management::v2::agent_bus_state::settlement_state::tests::subprocess_restart_reader","--test-threads=1"])
            .env("HERON_SETTLEMENT_RESTART_FIXTURE",&f.root).output().unwrap();
        assert!(result.status.success(),"child restart failed: {} {}",String::from_utf8_lossy(&result.stdout),String::from_utf8_lossy(&result.stderr));
        let report:Value=serde_json::from_slice(&fs::read(f.root.join("restart-result.json")).unwrap()).unwrap();
        assert_eq!(report["changed"],false);assert_eq!(report["state"],"closed");assert_eq!(report["published"],json!([TO]));
        assert!(f.stored("m").settlement.unwrap().pending_sessions.is_empty());
        assert_eq!(f.stored("m").snapshot.state,"inbox_accepted");
    }

    #[test]
    #[ignore = "invoked only by actual_process_restart fixture"]
    fn subprocess_restart_reader() {
        let root=PathBuf::from(std::env::var_os("HERON_SETTLEMENT_RESTART_FIXTURE").expect("parent fixture path"));
        assert!(root.canonicalize().unwrap().starts_with(std::env::temp_dir().canonicalize().unwrap()));
        let request:CloseRequest=serde_json::from_slice(&fs::read(root.join("restart-request.json")).unwrap()).unwrap();
        let repo=AgentBusMessageRepository::open(&root).unwrap();
        // This is a fresh process: neither collector nor runtime fence is
        // registered. A committed close must still replay and repair offline.
        let replay=repo.settlement_replay(&request,"owner").unwrap().unwrap().unwrap();
        let published=std::cell::RefCell::new(Vec::new());
        repo.drain_settlement_outbox(&|session,_,params| {
            assert_eq!(params["requestId"],request.request_id);published.borrow_mut().push(session.to_string());Ok(())
        }).unwrap();
        fs::write(root.join("restart-result.json"),serde_json::to_vec(&json!({"changed":replay.changed,"state":replay.settlement.state,"published":published.into_inner()})).unwrap()).unwrap();
    }

    #[test]
    fn raw_snapshot_guards_reject_hidden_business_closure_unknown_sidecar_fields_and_versions() {
        let f = Fixture::new();
        f.seed("m", "failed", false);
        let database = f.root.join(hot_store::DATABASE_FILE);
        let observations = hot_store::verified_rows(&database, EVIDENCE_STORE)
            .unwrap()
            .unwrap()
            .rows;
        AgentBusMessageRepository::validate_settlement_observation_rows(&observations).unwrap();
        for pointer in [
            "/version",
            "/record/futureCloseFlag",
            "/record/evidence/schemaVersion",
        ] {
            let mut rows = observations.clone();
            let bytes = rows.get_mut(&("evidence".into(), "m".into())).unwrap();
            let mut row: Value = serde_json::from_slice(bytes).unwrap();
            if pointer == "/record/futureCloseFlag" {
                row["record"]["futureCloseFlag"] = json!(true);
            } else {
                *row.pointer_mut(pointer).unwrap() = json!(2);
            }
            *bytes = serde_json::to_vec(&row).unwrap();
            assert!(
                AgentBusMessageRepository::validate_settlement_observation_rows(&rows).is_err()
            );
        }
        let rows = hot_store::verified_rows(&database, "agent_bus")
            .unwrap()
            .unwrap()
            .rows;
        AgentBusMessageRepository::validate_settlement_rollback_business_rows(&rows, None).unwrap();
        let mut bad = rows.clone();
        let mut row: Value = serde_json::from_slice(&bad[&("message".into(), "m".into())]).unwrap();
        row["settlement"] = json!({"closure":{"actor":"owner"}});
        bad.insert(
            ("message".into(), "m".into()),
            serde_json::to_vec(&row).unwrap(),
        );
        assert!(
            AgentBusMessageRepository::validate_settlement_rollback_business_rows(&bad, None)
                .is_err()
        );
        let legacy = serde_json::to_vec(&json!({"version":7,"messages":{"m":row}})).unwrap();
        assert!(
            AgentBusMessageRepository::validate_settlement_rollback_business_rows(
                &rows,
                Some(&legacy)
            )
            .is_err()
        );
    }

    #[test]
    fn competing_closes_commit_one_business_transition_and_request_keys_are_global() {
        let f = Fixture::new();
        let evidence = f.seed("m", "failed", false);
        let one = f.request("m", "one");
        let mut two = one.clone();
        two.request_id = "two".into();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for request in [one.clone(), two] {
            let root = f.root.clone();
            let evidence = evidence.clone();
            let now = f.now;
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let repo = AgentBusMessageRepository::open(&root).unwrap();
                barrier.wait();
                repo.close_settlement(&request, "owner", &evidence, now)
                    .unwrap()
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_ref().is_ok_and(|out| out.changed))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_ref().is_err_and(|error| error.status == 409))
                .count(),
            1
        );
        let key = f
            .stored("m")
            .settlement
            .unwrap()
            .closure
            .unwrap()
            .request_id;
        let evidence = f.seed("other", "failed", false);
        let request = f.request("other", &key);
        assert_eq!(
            f.repo
                .close_settlement(&request, "owner", &evidence, f.now)
                .unwrap()
                .unwrap_err()
                .status,
            409
        );
    }
}
