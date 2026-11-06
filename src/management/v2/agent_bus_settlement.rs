//! Owner HTTP settlement controller. Runtime observations never come from JSON.
use std::net::TcpStream;

use chrono::{DateTime, Utc};
use serde_json::json;

use super::agent_bus_settlement_model::{
    self as model, CloseOutcome, CloseRequest, Evidence, Observation, Refusal,
};
use super::agent_bus_state::{AgentBusMessageRepository, AgentBusMessageSnapshot};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const EVIDENCE_PATH: &str = "/v2/agent-bus/stuck-messages/evidence";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EvidenceRequest {
    message_id: String,
}

/// Explicit owner demand. Updates only rebuildable evidence; never submits or closes input.
fn evidence_with(
    repository: &AgentBusMessageRepository,
    message_id: &str,
    now: DateTime<Utc>,
    collector: impl FnOnce(&AgentBusMessageSnapshot) -> anyhow::Result<Observation>,
) -> anyhow::Result<Result<serde_json::Value, Refusal>> {
    let Some(snapshot) = repository.snapshot_by_message_id(message_id)? else {
        return Ok(Err(Refusal::new(
            404,
            "message_not_found",
            "message not found",
        )));
    };
    if let Some(closed) = repository
        .settlement_view_by_id(message_id, now)?
        .filter(|view| view.closure.is_some())
    {
        return Ok(Ok(json!({"messageId":message_id,"settlement":closed})));
    }
    let lane = super::session_coordination::session_lane(&snapshot.to_cutex_session_id)?;
    let _reservation = lane.reserve()?;
    let evidence = match collector(&snapshot) {
        Ok(Observation::Terminal(evidence)) => Some(evidence),
        Ok(Observation::Ineligible) => None,
        Err(_) => {
            repository.observe_settlement(message_id, None, now)?;
            return Ok(Err(Refusal::new(
                503,
                "settlement_observation_unavailable",
                "trusted runtime observation unavailable",
            )));
        }
    };
    // Collection timestamps its observation after RPC/history reads. The request's initial
    // clock must not classify that newly collected evidence as coming from the future.
    let refreshed_at = now.max(Utc::now());
    repository.observe_settlement(message_id, evidence, refreshed_at)?;
    Ok(Ok(
        json!({"messageId":message_id,"settlement":repository.settlement_view_by_id(message_id, refreshed_at)?}),
    ))
}

/// Called only after the same owner/full-device authentication as close.
pub fn handle_evidence(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    if request.method != "POST" {
        return refusal(stream, &Refusal::new(405, "method_not_allowed", "use POST"));
    }
    let parsed = match serde_json::from_slice::<EvidenceRequest>(&request.body) {
        Ok(parsed) if !parsed.message_id.trim().is_empty() && parsed.message_id.len() <= 512 => {
            parsed
        }
        _ => {
            return refusal(
                stream,
                &Refusal::new(400, "invalid_request", "expected only a bounded messageId"),
            )
        }
    };
    let result = super::agent_bus_state::agent_bus_message_repository().and_then(|repository| {
        evidence_with(repository, &parsed.message_id, Utc::now(), model::collect)
    });
    match result {
        Ok(Ok(value)) => write_json_response(stream, 200, "OK", &value),
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
            // Same per-session execution lane as submit/stop/revert. Retain its reservation
            // across fresh history collection AND the close commit; no mutex is held during IO.
            let lane = super::session_coordination::session_lane(&snapshot.to_cutex_session_id)?;
            let _reservation = lane.reserve()?;
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
/// outbox entries survive and are retried on explicit close replay and the
/// observer's independent background recovery loop (including offline hosts).
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

    #[test]
    fn explicit_evidence_demand_cannot_close_resend_or_invent_receipts() {
        let fixture = Fixture::new();
        let now = Utc::now();
        fixture.queued("candidate", false);
        let before = fixture
            .repository
            .snapshot_by_message_id("candidate")
            .unwrap();
        let value = evidence_with(&fixture.repository, "candidate", now, |_| {
            Ok(Observation::Ineligible)
        })
        .unwrap()
        .unwrap();
        assert_eq!(value, json!({"messageId":"candidate","settlement":null}));
        assert_eq!(
            before,
            fixture
                .repository
                .snapshot_by_message_id("candidate")
                .unwrap()
        );
        let missing = evidence_with(&fixture.repository, "missing", now, |_| {
            panic!("missing identity must not RPC")
        })
        .unwrap()
        .unwrap_err();
        assert_eq!(missing.status, 404);
        let unavailable = evidence_with(&fixture.repository, "candidate", now, |_| {
            anyhow::bail!("disconnected")
        })
        .unwrap()
        .unwrap_err();
        assert_eq!(unavailable.status, 503);
        assert_eq!(
            before,
            fixture
                .repository
                .snapshot_by_message_id("candidate")
                .unwrap()
        );
    }

    #[test]
    fn explicit_evidence_demand_returns_current_token_and_replays_closed_state_offline() {
        let fixture = Fixture::new();
        let now = Utc::now();
        let evidence = fixture.reviewed("reviewed-demand", now, "held");
        let before = fixture
            .repository
            .snapshot_by_message_id("reviewed-demand")
            .unwrap();
        let value = evidence_with(&fixture.repository, "reviewed-demand", now, |_| {
            Ok(Observation::Terminal(evidence.clone()))
        })
        .unwrap()
        .unwrap();
        assert_eq!(value["settlement"]["canClose"], true);
        assert_eq!(value["settlement"]["state"], "outcome_unknown");
        assert_eq!(
            before,
            fixture
                .repository
                .snapshot_by_message_id("reviewed-demand")
                .unwrap()
        );
        let request = fixture.request("reviewed-demand");
        close_with(&fixture.repository, &request, "owner", now, |_| {
            Ok(Observation::Terminal(evidence))
        })
        .unwrap()
        .unwrap();
        let closed = evidence_with(&fixture.repository, "reviewed-demand", now, |_| {
            panic!("committed closure must be readable offline")
        })
        .unwrap()
        .unwrap();
        assert_eq!(closed["settlement"]["state"], "closed");
        assert_eq!(closed["settlement"]["canClose"], false);
    }

    #[test]
    fn explicit_evidence_uses_post_collection_clock_without_accepting_future_evidence() {
        let fixture = Fixture::new();
        let start = Utc::now() - chrono::Duration::seconds(2);
        let evidence = fixture.reviewed("new-observation", Utc::now(), "held");
        let value = evidence_with(&fixture.repository, "new-observation", start, |_| {
            Ok(Observation::Terminal(evidence.clone()))
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            value["settlement"]["canClose"], true,
            "an observation after request start is not future evidence"
        );
        let mut impossible = evidence;
        impossible.observed_at = Utc::now() + chrono::Duration::minutes(10);
        let value = evidence_with(&fixture.repository, "new-observation", start, |_| {
            Ok(Observation::Terminal(impossible))
        })
        .unwrap()
        .unwrap();
        assert!(
            value["settlement"].is_null(),
            "genuinely future evidence remains ineligible"
        );
    }

    #[test]
    fn owner_close_serializes_history_proof_and_commit_with_same_session_revert_lane() {
        let fixture = Fixture::new();
        let now = Utc::now();
        let evidence = fixture.reviewed("lane-proof", now, "held");
        let request = fixture.request("lane-proof");
        let lane = super::super::session_coordination::session_lane(TO).unwrap();
        let assert_revert_blocked = || {
            let other = lane.clone();
            assert!(std::thread::spawn(move || matches!(
                other.try_lock(),
                Err(super::super::session_coordination::ReservationError::WouldBlock)
            ))
            .join()
            .unwrap());
        };
        let result = close_with_fence(
            &fixture.repository,
            &request,
            "owner",
            now,
            |_| {
                assert_revert_blocked();
                Ok(Observation::Terminal(evidence.clone()))
            },
            &|_| {
                assert_revert_blocked();
                Ok(true)
            },
        )
        .unwrap()
        .unwrap();
        assert!(result.changed);
        assert!(lane.try_lock().is_ok(), "the reservation ends after commit");
    }

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
                stale_ack: None,
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
    fn stuck_get_does_not_publish_or_repair_pending_settlement_outbox() {
        let fixture = Fixture::new();
        let now = Utc::now();
        let evidence = fixture.reviewed("read-only", now, "held");
        fixture.repository.close_settlement_fenced(&fixture.request("read-only"), "owner", &evidence, now, &|_| Ok(true))
            .unwrap().unwrap();
        use super::super::hot_store;
        let db = fixture.root.join(hot_store::DATABASE_FILE);
        let before = hot_store::verified_rows(&db, "agent_bus").unwrap().unwrap();
        let before_evidence = hot_store::verified_rows(&db, "agent_bus_settlement_evidence").unwrap();
        struct Offline;
        impl super::super::agent_bus_stuck::LiveBus for Offline {
            fn queued(&self) -> anyhow::Result<Vec<crate::agent_bus::queue::QueuedAgentBusMessage>> { Ok(vec![]) }
            fn withdraw(&self, _: &[String], _: &str) -> anyhow::Result<Vec<(String,String)>> { panic!("GET cannot withdraw") }
        }
        let (_, events) = super::super::agent_bus_state::capture_events(|| {
            for _ in 0..2 {
                super::super::agent_bus_stuck::list_view(&fixture.repository, &Offline, 0, None, 200, now.timestamp() as u64, &|_| None).unwrap();
            }
        });
        let after = hot_store::verified_rows(&db, "agent_bus").unwrap().unwrap();
        assert_eq!(before.generation, after.generation, "GET flushed a durable outbox");
        assert_eq!(before.rows, after.rows);
        let after_evidence = hot_store::verified_rows(&db, "agent_bus_settlement_evidence").unwrap();
        assert_eq!(before_evidence.map(|v|(v.generation,v.rows)), after_evidence.map(|v|(v.generation,v.rows)));
        assert!(events.is_empty(), "GET published an event");
        // Recovery still belongs to the explicit/background writer.
        let (_, repaired) = super::super::agent_bus_state::capture_events(|| repair_outbox(&fixture.repository));
        assert!(!repaired.is_empty());
    }

    #[test]
    fn read_only_stuck_view_never_initializes_or_migrates_a_legacy_store() {
        let fixture = Fixture::new();
        let root = fixture.root.join("cold");
        let reader = AgentBusMessageRepository::open_read_only(&root);
        assert!(reader.stuck_messages(0, 100).unwrap().is_empty());
        assert!(!root.exists(), "diagnostics initialized a repository");
        std::fs::create_dir(&root).unwrap();
        let legacy = root.join("agent-bus-message-state.json");
        let bytes = br#"{"version":2,"messages":{}}"#;
        std::fs::write(&legacy, bytes).unwrap();
        assert!(reader.stuck_messages(0, 100).unwrap().is_empty());
        assert!(reader.stuck_view_by_id("absent", 100).unwrap().is_none());
        assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "GET created a lock or SQLite migration");
        let sessions = root.join("cutex-sessions.json");
        assert!(super::super::agent_bus_stuck::read_session_names(&sessions).unwrap().is_empty());
        assert!(!sessions.exists());
        std::fs::write(&sessions, br#"{"sessions":{}}"#).unwrap();
        assert!(super::super::agent_bus_stuck::read_session_names(&sessions).unwrap().is_empty());
        assert!(!root.join("cutex-sessions.sqlite3").exists());
    }

    #[test]
    fn failed_observation_is_visible_idempotent_and_never_a_delivery_receipt() {
        let fixture = Fixture::new();
        let evidence = fixture.reviewed("unknown", Utc::now(), "held");
        let before = fixture.repository.snapshot_by_message_id("unknown").unwrap().unwrap();
        assert!(fixture.repository.record_settlement_observation_failure(&before, 9).unwrap());
        let failed = fixture.repository.snapshot_by_message_id("unknown").unwrap().unwrap();
        assert_eq!(failed.state, before.state);
        assert_eq!(failed.external_input_receipt, before.external_input_receipt);
        assert!(AgentBusMessageRepository::settlement_observation_failed(&failed, 9));
        assert!(!AgentBusMessageRepository::settlement_observation_failed(&failed, 10));
        assert!(!fixture.repository.record_settlement_observation_failure(&before, 9).unwrap(), "stale CAS changed a newer row");
        assert!(!fixture.repository.record_settlement_observation_failure(&failed, 9).unwrap(), "same failure rewrote storage");
        let row = fixture.repository.stuck_view_by_id("unknown", Utc::now().timestamp() as u64).unwrap().unwrap();
        assert_eq!(row.error.unwrap()["details"]["observationState"], "failed");
        assert!(row.reasons.iter().any(|r|r.contains("settlement_observation_unavailable")));
        // Changed status, not elapsed time, permits another collection.
        let mut changed = failed.clone();
        changed.external_input_last_observed.as_mut().unwrap().processing.reason = Some(crate::app_server::external_input::HoldReason::RequestUncertain);
        assert!(!AgentBusMessageRepository::settlement_observation_failed(&changed, 9));
        assert!(fixture.repository.clear_settlement_observation_failure(&failed).unwrap());
        fixture.repository.observe_settlement("unknown", Some(evidence), Utc::now()).unwrap();
        assert!(fixture.repository.snapshot_by_message_id("unknown").unwrap().unwrap().error.is_none());
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
