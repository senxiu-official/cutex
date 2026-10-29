//! Owner HTTP settlement controller. Runtime observations never come from JSON.
use std::net::TcpStream;

use chrono::{DateTime, Utc};
use serde_json::json;

use super::agent_bus_settlement_model::{
    self as model, CloseOutcome, CloseRequest, Evidence, Observation, Refusal,
};
use super::agent_bus_state::{AgentBusMessageRepository, AgentBusMessageSnapshot};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub fn parse(body: &[u8]) -> Result<CloseRequest, Refusal> {
    let request: CloseRequest = serde_json::from_slice(body).map_err(|_| {
        Refusal::new(
            400,
            "invalid_request",
            "expected only messageId, expectedEvidenceToken, requestId and reason",
        )
    })?;
    request.validate()?;
    Ok(request)
}

/// `actor` is authenticated Management identity, never a request field. Replay
/// precedes observation so a committed request remains replayable offline.
pub fn close_with_fence(
    repository: &AgentBusMessageRepository,
    request: &CloseRequest,
    actor: &str,
    now: DateTime<Utc>,
    collector: impl FnOnce(&AgentBusMessageSnapshot) -> anyhow::Result<Observation>,
    fence: &dyn Fn(&Evidence) -> anyhow::Result<bool>,
) -> anyhow::Result<Result<CloseOutcome, Refusal>> {
    if let Err(refusal) = request.validate() {
        return Ok(Err(refusal));
    }
    let result = match repository.settlement_replay(request, actor)? {
        Ok(Some(outcome)) => Ok(outcome),
        Err(refusal) => return Ok(Err(refusal)),
        Ok(None) => {
            let snapshot = match repository.settlement_close_candidate(request, now)? {
                Ok(snapshot) => snapshot,
                Err(refusal) => return Ok(Err(refusal)),
            };
            let evidence = match collector(&snapshot) {
                Ok(Observation::Terminal(evidence)) => evidence,
                Ok(Observation::Ineligible) => {
                    repository.observe_settlement(&request.message_id, None, now)?;
                    return Ok(Err(Refusal::new(
                        409,
                        "settlement_not_closable",
                        "trusted observation does not permit closure",
                    )));
                }
                Err(_) => {
                    repository.observe_settlement(&request.message_id, None, now)?;
                    return Ok(Err(Refusal::new(
                        503,
                        "settlement_observation_unavailable",
                        "trusted runtime observation unavailable",
                    )));
                }
            };
            let outcome =
                repository.close_settlement_fenced(request, actor, &evidence, now, fence)?;
            if outcome.as_ref().is_err_and(|refusal| {
                (refusal.status == 409 && refusal.code != "request_id_conflict")
                    || refusal.status == 503
            }) {
                // Fresh facts no longer match the owner's review. Drop the old
                // projection rather than leave its close button enabled. The
                // repository preserves any closure committed concurrently.
                repository.observe_settlement(&request.message_id, None, now)?;
            }
            outcome
        }
    };
    if result.is_ok() {
        repair_outbox(repository);
    }
    Ok(result)
}

#[cfg(test)]
fn close_with(
    repository: &AgentBusMessageRepository,
    request: &CloseRequest,
    actor: &str,
    now: DateTime<Utc>,
    collector: impl FnOnce(&AgentBusMessageSnapshot) -> anyhow::Result<Observation>,
) -> anyhow::Result<Result<CloseOutcome, Refusal>> {
    close_with_fence(repository, request, actor, now, collector, &|_| Ok(true))
}

/// A notification failure cannot undo an already committed close. Durable
/// outbox entries survive and are retried on reads, replay and background work.
pub fn repair_outbox(repository: &AgentBusMessageRepository) {
    if let Err(error) = repository.flush_settlement_outbox() {
        eprintln!("warning: settlement outbox publication deferred: {error:#}");
    }
}

fn refusal(stream: &mut TcpStream, error: &Refusal) -> anyhow::Result<()> {
    let reason = match error.status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Service Unavailable",
    };
    super::server::write_v2_error(
        stream,
        error.status,
        reason,
        &error.code,
        &error.message,
        error.status >= 500,
        json!({}),
    )
}

fn write_outcome(
    stream: &mut TcpStream,
    outcome: anyhow::Result<Result<CloseOutcome, Refusal>>,
) -> anyhow::Result<()> {
    match outcome {
        Ok(Ok(outcome)) => write_json_response(stream, 200, "OK", &serde_json::to_value(outcome)?),
        Ok(Err(error)) => refusal(stream, &error),
        Err(_) => refusal(
            stream,
            &Refusal::new(
                503,
                "agent_bus_state_unavailable",
                "message settlement state unavailable",
            ),
        ),
    }
}

/// Entry after the server has authenticated the owner and checked device scope.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    handle_with(
        stream,
        request,
        super::agent_bus_state::agent_bus_message_repository,
        &crate::management::users::current_actor(),
        Utc::now(),
        model::collect,
        &model::runtime_matches,
    )
}

fn handle_with<'a>(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    repository: impl FnOnce() -> anyhow::Result<&'a AgentBusMessageRepository>,
    actor: &str,
    now: DateTime<Utc>,
    collector: impl FnOnce(&AgentBusMessageSnapshot) -> anyhow::Result<Observation>,
    fence: &dyn Fn(&Evidence) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    if request.method != "POST" {
        return refusal(stream, &Refusal::new(405, "method_not_allowed", "use POST"));
    }
    let parsed = match parse(&request.body) {
        Ok(parsed) => parsed,
        Err(error) => return refusal(stream, &error),
    };
    let repository = match repository() {
        Ok(repository) => repository,
        Err(_) => {
            return refusal(
                stream,
                &Refusal::new(
                    503,
                    "agent_bus_state_unavailable",
                    "message settlement state unavailable",
                ),
            )
        }
    };
    write_outcome(
        stream,
        close_with_fence(repository, &parsed, actor, now, collector, fence),
    )
}

#[cfg(test)]
mod tests {
    use super::super::agent_bus_state::AgentBusQueuedMessage;
    use super::*;
    use crate::agent_bus::model::{AgentBusEnvelopeKind, AgentBusMessage, AgentMessageKind};
    use std::io::Read;
    use std::net::TcpListener;
    use std::path::PathBuf;

    const TO: &str = "cutex.11111111-1111-4111-8111-111111111111";
    const FROM: &str = "cutex.22222222-2222-4222-8222-222222222222";

    struct Fixture {
        repository: AgentBusMessageRepository,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            model::register_runtime_fence(|evidence| Ok(evidence.actual_turn_id != "unavailable"));
            let root =
                std::env::temp_dir().join(format!("settlement-handler-{}", uuid::Uuid::new_v4()));
            Self {
                repository: AgentBusMessageRepository::open(&root).unwrap(),
                root,
            }
        }
        fn queued(&self, id: &str, protected: bool) {
            let content = "owner-reviewed message".to_string();
            let envelope = AgentBusMessage {
                id: id.into(),
                kind: AgentBusEnvelopeKind::Message,
                from: FROM.into(),
                to: TO.into(),
                from_cutex_session_id: Some(FROM.into()),
                to_cutex_session_id: Some(TO.into()),
                content: content.clone(),
                delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn,
                trigger_turn: true,
                created_at_epoch_secs: 1,
                sender_kind: if protected {
                    AgentMessageKind::JobServiceSystem
                } else {
                    AgentMessageKind::Agent
                },
                display_source: None,
                submit_mode: None,
                control_type: protected.then(|| crate::agent_bus::claude_task::CONTROL_TYPE.into()),
                control_payload: None,
                external_action_id: None,
                external_message_id: None,
            };
            self.repository
                .record_queued_isolated(AgentBusQueuedMessage {
                    owner_cutex_session_id: TO.into(),
                    message_id: id.into(),
                    from_cutex_session_id: Some(FROM.into()),
                    to_cutex_session_id: TO.into(),
                    from_runtime_agent_id: Some("sender".into()),
                    to_runtime_agent_id: Some("recipient".into()),
                    delivery_mode: "after_turn".into(),
                    semantic_sha256: crate::role_revision::sha256_bytes(content.as_bytes())
                        .as_str()
                        .to_string(),
                    content,
                    queued_at: Utc::now(),
                    canonical_envelope: envelope,
                })
                .unwrap();
        }
        fn reviewed(
            &self,
            id: &str,
            now: DateTime<Utc>,
            processing_state: &str,
        ) -> model::Evidence {
            self.queued(id, false);
            let frozen = self
                .repository
                .freeze_external_input(TO, id, |message| {
                    use crate::app_server::external_input::*;
                    let mut envelope = Envelope {
                        version: 1,
                        owner_id: TO.into(),
                        thread_id: "11111111-1111-4111-8111-111111111111".into(),
                        runtime_generation: 1,
                        message: Message {
                            id: message.id.clone(),
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
            let evidence = model::Evidence {
                observing_generation: None,
                schema_version: 1,
                runtime: "claude".into(),
                recipient_id: TO.into(),
                runtime_generation: 1,
                thread_id: frozen.thread_id.clone(),
                message_id: id.into(),
                semantic_sha256: frozen.semantic_sha256.clone(),
                local_semantic_sha256: frozen.semantic_sha256.clone(),
                attempt_id: "33333333-3333-4333-8333-333333333333".into(),
                processing_state: processing_state.into(),
                actual_turn_id: "actual-turn-1".into(),
                terminal_status: "completed".into(),
                observed_at: now,
                observation_sha256: "a".repeat(64),
            };
            // Seed native/local facts only; use the real observer API to record
            // rebuildable evidence without upgrading the original Bus format.
            use super::super::hot_store::{self, RowChange};
            let database = self.root.join(hot_store::DATABASE_FILE);
            let loaded = hot_store::verified_rows(&database, "agent_bus")
                .unwrap()
                .unwrap();
            let mut row: serde_json::Value =
                serde_json::from_slice(&loaded.rows[&("message".into(), id.into())]).unwrap();
            row["snapshot"]["state"] = json!("inbox_accepted");
            row["snapshot"]["localInboxReceipt"] = json!({
                "threadId": evidence.thread_id, "messageId": id,
                "semanticSha256": evidence.local_semantic_sha256,
                "durable": true, "contextPersisted": false,
            });
            row["snapshot"]["externalInputLastObserved"] = json!({
                "messageId": id, "semanticSha256": evidence.semantic_sha256,
                "deliveryState": "pending", "receipt": null,
                "processing": {"state": processing_state, "attemptId": evidence.attempt_id,
                    "reason": if processing_state == "held" { json!("interrupted") } else { json!(null) }},
            });
            hot_store::commit(
                &database,
                "agent_bus",
                loaded.generation,
                vec![RowChange::Put {
                    collection: "message".into(),
                    key: id.into(),
                    value: serde_json::to_vec(&row).unwrap(),
                }],
            )
            .unwrap();
            assert!(self
                .repository
                .observe_settlement(id, Some(evidence.clone()), now)
                .unwrap());
            let after = hot_store::verified_rows(&database, "agent_bus")
                .unwrap()
                .unwrap();
            assert_eq!(
                after.generation,
                loaded.generation + 1,
                "observation must not rewrite Bus rows"
            );
            assert_eq!(
                after.rows[&("state".into(), "version".into())],
                loaded.rows[&("state".into(), "version".into())]
            );
            evidence
        }

        fn request(&self, id: &str) -> CloseRequest {
            let mut request = request(id);
            if let Some(view) = self
                .repository
                .settlement_view_by_id(id, Utc::now())
                .unwrap()
            {
                request.expected_evidence_token = view.evidence_token;
            }
            request
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn request(id: &str) -> CloseRequest {
        CloseRequest {
            message_id: id.into(),
            expected_evidence_token: "44444444-4444-4444-8444-444444444444".into(),
            request_id: "close-request".into(),
            reason: "owner reviewed terminal assignment".into(),
        }
    }

    fn socket_response(
        fixture: &Fixture,
        body: &[u8],
        collector: impl FnOnce(&AgentBusMessageSnapshot) -> anyhow::Result<Observation>,
    ) -> (u16, serde_json::Value) {
        socket_response_with_fence(fixture, body, collector, &|_| Ok(true))
    }

    fn socket_response_with_fence(
        fixture: &Fixture,
        body: &[u8],
        collector: impl FnOnce(&AgentBusMessageSnapshot) -> anyhow::Result<Observation>,
        fence: &dyn Fn(&Evidence) -> anyhow::Result<bool>,
    ) -> (u16, serde_json::Value) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let request = SimpleHttpRequest {
            method: "POST".into(),
            path: model::CLOSE_PATH.into(),
            headers: Default::default(),
            body: body.to_vec(),
        };
        handle_with(
            &mut server,
            &request,
            || Ok(&fixture.repository),
            "authenticated-owner-device",
            Utc::now(),
            collector,
            fence,
        )
        .unwrap();
        drop(server);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (
            head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            serde_json::from_str(body).unwrap(),
        )
    }

    #[test]
    fn strict_json_rejects_forged_observation_and_actor_fields() {
        let fixture = Fixture::new();
        let valid = serde_json::to_value(fixture.request("unknown")).unwrap();
        assert!(parse(&serde_json::to_vec(&valid).unwrap()).is_ok());
        for field in [
            "actor",
            "terminalStatus",
            "actualTurnId",
            "runtimeGeneration",
            "evidence",
            "allowProtected",
            "changed",
        ] {
            let mut forged = valid.clone();
            forged[field] = json!("forged");
            let (status, _) =
                socket_response(&fixture, &serde_json::to_vec(&forged).unwrap(), |_| {
                    panic!("invalid request must not observe")
                });
            assert_eq!(status, 400, "{field}");
        }
        for body in [b"{".as_slice(), b"[]", b"null", b"{}",
            br#"{"messageId":"m","messageId":"m","expectedEvidenceToken":"t","requestId":"r","reason":"reviewed"}"#] {
            assert_eq!(socket_response(&fixture, body, |_| panic!("invalid request must not observe")).0, 400);
        }
        for field in ["messageId", "expectedEvidenceToken", "requestId", "reason"] {
            let mut empty = valid.clone();
            empty[field] = json!("  ");
            assert_eq!(
                socket_response(&fixture, &serde_json::to_vec(&empty).unwrap(), |_| panic!(
                    "invalid request must not observe"
                ))
                .0,
                400
            );
        }
    }

    #[test]
    fn unknown_protected_and_unreviewed_requests_refuse_before_observation() {
        let fixture = Fixture::new();
        fixture.queued("protected", true);
        fixture.queued("unreviewed", false);
        for (id, expected) in [("unknown", 404), ("protected", 409), ("unreviewed", 409)] {
            let before = fixture.repository.snapshot_by_message_id(id).unwrap();
            let (status, _) = socket_response(
                &fixture,
                &serde_json::to_vec(&fixture.request(id)).unwrap(),
                |_| panic!("refused candidate must not observe"),
            );
            assert_eq!(status, expected, "{id}");
            assert_eq!(
                fixture.repository.snapshot_by_message_id(id).unwrap(),
                before
            );
        }
    }

    #[test]
    fn collection_crossing_one_second_uses_commit_clock_not_request_clock() {
        let fixture = Fixture::new();
        let request_now = Utc::now();
        let mut evidence = fixture.reviewed("reviewed", request_now, "held");
        let request = fixture.request("reviewed");
        let outcome = close_with_fence(
            &fixture.repository,
            &request,
            "authenticated-owner",
            request_now,
            |_| {
                std::thread::sleep(std::time::Duration::from_millis(1100));
                evidence.observed_at = Utc::now();
                Ok(Observation::Terminal(evidence))
            },
            &|fresh| {
                assert!(fresh.observed_at >= request_now + chrono::Duration::seconds(1));
                Ok(true)
            },
        )
        .unwrap()
        .unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.settlement.state, "closed");
        assert!(
            outcome.settlement.closure.unwrap().closed_at
                >= outcome.settlement.evidence.observed_at
        );
    }

    #[test]
    fn runtime_change_after_collection_is_rejected_by_commit_fence() {
        for unavailable in [false, true] {
            let fixture = Fixture::new();
            let now = Utc::now();
            let evidence = fixture.reviewed("reviewed", now, "held");
            let body = serde_json::to_vec(&fixture.request("reviewed")).unwrap();
            let before = fixture
                .repository
                .snapshot_by_message_id("reviewed")
                .unwrap();
            let generation = std::cell::Cell::new(evidence.runtime_generation);
            let fenced = std::cell::Cell::new(false);
            let (status, _) = socket_response_with_fence(
                &fixture,
                &body,
                |_| {
                    generation.set(evidence.runtime_generation + 1);
                    Ok(Observation::Terminal(evidence))
                },
                &|fresh| {
                    fenced.set(true);
                    if unavailable {
                        anyhow::bail!("runtime identity unavailable")
                    }
                    Ok(generation.get() == fresh.runtime_generation)
                },
            );
            assert!(
                fenced.get(),
                "commit must check runtime identity after collection"
            );
            assert_eq!(status, if unavailable { 503 } else { 409 });
            assert_eq!(
                fixture
                    .repository
                    .snapshot_by_message_id("reviewed")
                    .unwrap(),
                before
            );
            assert!(fixture
                .repository
                .settlement_view_by_id("reviewed", now)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn fresh_observation_failures_clear_the_old_close_projection() {
        for unavailable in [false, true] {
            let fixture = Fixture::new();
            let now = Utc::now();
            fixture.reviewed("reviewed", now, "held");
            let request = fixture.request("reviewed");
            let before = fixture
                .repository
                .snapshot_by_message_id("reviewed")
                .unwrap();
            assert!(
                fixture
                    .repository
                    .settlement_view_by_id("reviewed", now)
                    .unwrap()
                    .unwrap()
                    .can_close
            );
            let called = std::cell::Cell::new(false);
            let (status, _) =
                socket_response(&fixture, &serde_json::to_vec(&request).unwrap(), |_| {
                    called.set(true);
                    if unavailable {
                        anyhow::bail!("adapter unavailable")
                    }
                    Ok(Observation::Ineligible)
                });
            assert!(called.get());
            assert_eq!(status, if unavailable { 503 } else { 409 });
            assert!(fixture
                .repository
                .settlement_view_by_id("reviewed", now)
                .unwrap()
                .is_none());
            assert!(fixture.repository.snapshot(TO).unwrap()[0]
                .get("settlement")
                .is_none());
            let steps = fixture
                .repository
                .pending_admission_messages(TO, &crate::asks::store::AskState::default())
                .unwrap();
            assert_eq!(steps.len(), 1);
            assert!(steps[0].settlement.is_none());
            assert_eq!(
                fixture
                    .repository
                    .snapshot_by_message_id("reviewed")
                    .unwrap(),
                before
            );
        }
    }

    #[test]
    fn observation_invalidation_storage_failure_returns_503() {
        let fixture = Fixture::new();
        let now = Utc::now();
        fixture.reviewed("reviewed", now, "held");
        let body = serde_json::to_vec(&fixture.request("reviewed")).unwrap();
        let database =
            rusqlite::Connection::open(fixture.root.join(super::super::hot_store::DATABASE_FILE))
                .unwrap();
        database.execute_batch("CREATE TRIGGER fail_observation_delete BEFORE DELETE ON hot_store_rows WHEN OLD.store='agent_bus_settlement_evidence' BEGIN SELECT RAISE(ABORT,'injected sidecar write failure'); END;").unwrap();
        assert_eq!(
            socket_response(&fixture, &body, |_| Ok(Observation::Ineligible)).0,
            503
        );
        assert!(
            fixture
                .repository
                .settlement_view_by_id("reviewed", now)
                .unwrap()
                .unwrap()
                .can_close
        );
    }

    #[test]
    fn trusted_observation_is_required_and_cannot_be_supplied_in_body() {
        let fixture = Fixture::new();
        let evidence = fixture.reviewed("reviewed", Utc::now(), "held");
        let body = serde_json::to_vec(&fixture.request("reviewed")).unwrap();
        let before = fixture
            .repository
            .snapshot_by_message_id("reviewed")
            .unwrap();
        let (status, outcome) = socket_response(&fixture, &body, |snapshot| {
            assert_eq!(snapshot.message_id, "reviewed");
            Ok(Observation::Terminal(evidence))
        });
        assert_eq!(status, 200, "{outcome}");
        assert_eq!(outcome["changed"], true);
        assert_eq!(
            outcome["settlement"]["closure"]["actor"],
            "authenticated-owner-device"
        );
        assert_eq!(
            fixture
                .repository
                .snapshot_by_message_id("reviewed")
                .unwrap(),
            before,
            "closing owner follow-up must preserve every delivery and receipt fact"
        );
    }

    #[test]
    fn stale_or_changed_tokens_and_changed_terminal_facts_are_conflicts() {
        let fixture = Fixture::new();
        let now = Utc::now();
        let evidence = fixture.reviewed("current", now, "held");
        fixture.reviewed(
            "expired",
            now - chrono::Duration::seconds(model::EVIDENCE_TTL_SECS + 1),
            "held",
        );
        let mut changed = fixture.request("current");
        changed.expected_evidence_token = uuid::Uuid::new_v4().to_string();
        for request in [changed, fixture.request("expired")] {
            assert_eq!(
                close_with(&fixture.repository, &request, "owner", now, |_| panic!(
                    "stale candidate must not observe"
                ))
                .unwrap()
                .unwrap_err()
                .status,
                409
            );
        }
        let mut changed = evidence;
        changed.actual_turn_id = "another-terminal-turn".into();
        let (status, _) = socket_response(
            &fixture,
            &serde_json::to_vec(&fixture.request("current")).unwrap(),
            |_| Ok(Observation::Terminal(changed)),
        );
        assert_eq!(status, 409);
        assert!(fixture
            .repository
            .settlement_view_by_id("current", now)
            .unwrap()
            .is_none());
    }

    #[test]
    fn controller_replay_is_offline_and_preserves_exact_reason_payload() {
        let fixture = Fixture::new();
        let now = Utc::now();
        let evidence = fixture.reviewed("replay", now, "output_observed");
        let mut request = fixture.request("replay");
        request.reason = "  reviewed by owner  ".into();
        let first = close_with(
            &fixture.repository,
            &request,
            "authenticated-owner",
            now,
            |_| Ok(Observation::Terminal(evidence)),
        )
        .unwrap()
        .unwrap();
        assert!(first.changed);
        assert_eq!(
            first.settlement.closure.as_ref().unwrap().reason,
            "reviewed by owner"
        );
        let replay = close_with_fence(
            &fixture.repository,
            &request,
            "authenticated-owner",
            now + chrono::Duration::seconds(600),
            |_| panic!("committed replay must not contact runtime"),
            &|_| panic!("committed replay must not consult runtime fence"),
        )
        .unwrap()
        .unwrap();
        assert!(!replay.changed);
        assert_eq!(replay.settlement, first.settlement);
        struct StillQueued;
        impl super::super::agent_bus_stuck::LiveBus for StillQueued {
            fn queued(
                &self,
            ) -> anyhow::Result<Vec<crate::agent_bus::queue::QueuedAgentBusMessage>> {
                Ok(vec![crate::agent_bus::queue::QueuedAgentBusMessage {
                    message_id: "replay".into(),
                    agent_id: TO.into(),
                    agent_name: None,
                    to_cutex_session_id: Some(TO.into()),
                    created_at_epoch_secs: 1,
                    sender: FROM.into(),
                    sender_kind: "agent".into(),
                    control_type: None,
                    content_preview: "original message".into(),
                }])
            }
            fn withdraw(&self, _: &[String], _: &str) -> anyhow::Result<Vec<(String, String)>> {
                panic!("listing must never withdraw or replay a message")
            }
        }
        let view = super::super::agent_bus_stuck::list_view(
            &fixture.repository,
            &StillQueued,
            0,
            None,
            200,
            now.timestamp() as u64,
            &|_| None,
        )
        .unwrap();
        assert_eq!(
            view["total"], 0,
            "a lingering live queue must not resurrect closed follow-up"
        );
        request.reason = request.reason.trim().to_string();
        assert_eq!(
            close_with(
                &fixture.repository,
                &request,
                "authenticated-owner",
                now,
                |_| panic!("key conflict must not contact runtime")
            )
            .unwrap()
            .unwrap_err()
            .status,
            409
        );
    }
}
