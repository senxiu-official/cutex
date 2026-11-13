//! 057 source reservations, explicit holds and native receipt reconciliation.
use super::*;
use crate::app_server::external_input::{Delivery, Envelope, Message, Receipt, Source, SourceKind};
use crate::management::v2::input_ledger::{InputLedger, InputState};
use crate::management::v2::input_ledger_bus::{self as ledger, NativeObserved, Target};

const SESSION: &str = "cutex.11111111-1111-4111-8111-111111111111";
const SENDER: &str = "cutex.22222222-2222-4222-8222-222222222222";

struct Fixture {
    root: PathBuf,
    repository: AgentBusMessageRepository,
    ledger: &'static InputLedger,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cutex-owner-carry-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(root.join("bus")).unwrap();
        let ledger = Box::leak(Box::new(InputLedger::open(root.join("ledger.db"))));
        Self {
            root,
            repository,
            ledger,
        }
    }
    fn on<T>(&self, body: impl FnOnce() -> T) -> T {
        super::capture_events(|| {
            super::super::input_ledger::capture_events(|| {
                ledger::with_test_ledger(self.ledger, body)
            })
            .0
        })
        .0
    }
    fn queue(&self, id: &str) {
        let message: AgentBusMessage = serde_json::from_value(json!({
            "id": id, "from": SENDER, "to": "runtime-to",
            "fromCutexSessionId": SENDER, "toCutexSessionId": SESSION,
            "content": "notice", "deliveryMode": "after_turn", "triggerTurn": true,
            "createdAtEpochSecs": 1
        }))
        .unwrap();
        self.repository
            .record_queued_isolated(AgentBusQueuedMessage {
                owner_cutex_session_id: SENDER.into(),
                message_id: id.into(),
                from_cutex_session_id: Some(SENDER.into()),
                to_cutex_session_id: SESSION.into(),
                from_runtime_agent_id: Some("runtime-from".into()),
                to_runtime_agent_id: Some("runtime-to".into()),
                delivery_mode: "after_turn".into(),
                content: message.content.clone(),
                queued_at: Utc::now(),
                canonical_envelope: message,
                semantic_sha256: "a".repeat(64),
            })
            .unwrap();
    }
    fn target(&self, id: &str) -> Target {
        let (_, facts, _) = self.repository.input_ledger_view(id).unwrap().unwrap();
        ledger::target_of(&facts)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn envelope(message: &AgentBusMessage) -> anyhow::Result<Envelope> {
    let mut e = Envelope {
        version: 1,
        owner_id: SESSION.into(),
        thread_id: "native-thread".into(),
        runtime_generation: 7,
        message: Message {
            id: message.id.clone(),
            source: Source {
                kind: SourceKind::Agent,
                id: SENDER.into(),
            },
            event_type: "message".into(),
            delivery: Delivery::AfterTurn,
            text: message.content.clone(),
        },
        view: None,
        semantic_sha256: String::new(),
    };
    e.semantic_sha256 = e.digest();
    e.validate()?;
    Ok(e)
}

#[test]
fn hold_is_durable_and_only_explicit_release_allows_carry() {
    let f = Fixture::new();
    f.on(|| {
        f.queue("hold");
        assert_eq!(
            f.target("hold"),
            Target::Waiting {
                reason: "afterTurn",
                details: Some(json!({"ownerHold": false}))
            }
        );
        assert!(f.repository.set_owner_hold(SESSION, "hold", true).unwrap());
        assert_eq!(f.repository.read(|store| Ok(store.version)).unwrap(),8,
            "additive owner policy must remain readable by exact r275");
        // This fixture's row owner is SENDER; policy belongs to its recipient.
        assert!(!f.repository.owner_delivery_blocked(SENDER, "hold").unwrap());
        assert!(!f.repository.set_owner_hold(SESSION, "hold", true).unwrap());
        assert_eq!(
            f.target("hold"),
            Target::Waiting {
                reason: "ownerPause",
                details: Some(json!({"ownerHold": true}))
            }
        );
        let reopened = AgentBusMessageRepository::open(f.root.join("bus")).unwrap();
        assert!(reopened.owner_delivery_blocked(SESSION, "hold").unwrap());
        assert!(reopened
            .reserve_owner_carries(SESSION, "owner-1", 8, |_| panic!("held builder"))
            .unwrap()
            .is_empty());
        assert!(reopened
            .freeze_external_input(SESSION, "hold", envelope)
            .is_err());
        assert!(reopened.set_owner_hold(SESSION, "hold", false).unwrap());
        assert_eq!(
            reopened
                .reserve_owner_carries(SESSION, "owner-1", 8, envelope)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(reopened.read(|store| Ok(store.version)).unwrap(),8);
    });
}

#[test]
fn frozen_accepted_stale_passive_and_other_session_are_excluded() {
    let f = Fixture::new();
    f.on(|| {
        for id in [
            "frozen", "accepted", "stale", "passive", "foreign", "eligible",
        ] {
            f.queue(id);
        }
        f.repository
            .freeze_external_input(SESSION, "frozen", envelope)
            .unwrap();
        f.repository
            .mutate(|store| {
                store.messages.get_mut("accepted").unwrap().snapshot.state =
                    "inbox_accepted".into();
                store.messages.get_mut("stale").unwrap().snapshot.state = "stale_held".into();
                store
                    .messages
                    .get_mut("passive")
                    .unwrap()
                    .canonical_envelope
                    .as_mut()
                    .unwrap()
                    .delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
                store
                    .messages
                    .get_mut("foreign")
                    .unwrap()
                    .snapshot
                    .to_cutex_session_id = SENDER.into();
                store
                    .messages
                    .get_mut("foreign")
                    .unwrap()
                    .canonical_envelope
                    .as_mut()
                    .unwrap()
                    .to_cutex_session_id = Some(SENDER.into());
                Ok(())
            })
            .unwrap();
        let carries = f
            .repository
            .reserve_owner_carries(SESSION, "owner-2", 8, |m| {
                assert_eq!(m.id, "eligible");
                envelope(m)
            })
            .unwrap();
        assert_eq!(carries.len(), 1);
        for id in ["frozen", "accepted", "stale", "passive", "foreign"] {
            assert!(f.repository.set_owner_hold(SESSION, id, true).is_err());
            let (recipient, facts, _) = f.repository.input_ledger_view(id).unwrap().unwrap();
            if id == "foreign" {
                assert_eq!(recipient, SENDER);
                assert!(
                    matches!(
                        facts.observed,
                        Some(NativeObserved::OwnerPolicy {
                            hold: Some(false),
                            ..
                        })
                    ),
                    "foreign input remains holdable by its own recipient"
                );
            } else {
                assert!(!matches!(
                    facts.observed,
                    Some(NativeObserved::OwnerPolicy { hold: Some(_), .. })
                ));
            }
        }
    });
}

#[test]
fn reserved_identity_is_not_resubmitted_and_survives_restart_until_proven_refusal() {
    let f = Fixture::new();
    f.on(|| {
        f.queue("reserved");
        let first = f
            .repository
            .reserve_owner_carries(SESSION, "owner-3", 8, envelope)
            .unwrap();
        assert_eq!(first.len(), 1);
        assert!(f
            .repository
            .owner_delivery_blocked(SESSION, "reserved")
            .unwrap());
        assert!(!f
            .repository
            .owner_delivery_blocked(SENDER, "reserved")
            .unwrap());
        assert!(f
            .repository
            .owner_carry_ids(SENDER, "owner-3")
            .unwrap()
            .is_empty());
        // Already queued sweep may retrieve the frozen identity to query status,
        // but it must check owner_delivery_blocked before any independent submit.
        assert_eq!(
            f.repository
                .freeze_external_input(SESSION, "reserved", |_| panic!("no rebuild"))
                .unwrap(),
            first[0]
        );
        assert!(f
            .repository
            .reserve_owner_carries(SESSION, "owner-other", 8, |_| panic!("reserved builder"))
            .unwrap()
            .is_empty());
        let reopened = AgentBusMessageRepository::open(f.root.join("bus")).unwrap();
        assert_eq!(
            reopened
                .reserve_owner_carries(SESSION, "owner-3", 8, |_| panic!("retry builder"))
                .unwrap(),
            first
        );
        assert_eq!(
            reopened.owner_carry_ids(SESSION, "owner-3").unwrap(),
            ["reserved"]
        );
        reopened.release_owner_carries(SESSION, "owner-3").unwrap();
        reopened.release_owner_carries(SESSION, "owner-3").unwrap();
        assert!(!reopened
            .owner_delivery_blocked(SESSION, "reserved")
            .unwrap());
        assert_eq!(
            reopened
                .reserve_owner_carries(SESSION, "owner-retry", 8, envelope)
                .unwrap()
                .len(),
            1
        );
    });
}

#[test]
fn builder_error_does_not_block_later_notice_or_owner_and_limit_is_respected() {
    let f = Fixture::new();
    f.on(|| {
        for id in ["a-oversize", "b-fits", "c-later"] {
            f.queue(id);
        }
        let carries = f
            .repository
            .reserve_owner_carries(SESSION, "owner-4", 1, |m| {
                if m.id == "a-oversize" {
                    anyhow::bail!("receiver quota exhausted");
                }
                assert_eq!(m.id, "b-fits");
                envelope(m)
            })
            .unwrap();
        assert_eq!(carries.len(), 1);
        assert_eq!(carries[0].message.id, "b-fits");
        assert!(!f
            .repository
            .owner_delivery_blocked(SESSION, "c-later")
            .unwrap());
    });
}

#[test]
fn competing_native_freeze_and_owner_reservation_choose_one_identity() {
    let f = Fixture::new();
    f.on(|| f.queue("race"));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let bus_root = f.root.join("bus");
    let other_root = bus_root.clone();
    let native_barrier = barrier.clone();
    let test_ledger = f.ledger;
    let native = std::thread::spawn(move || {
        let repo = AgentBusMessageRepository::open(other_root).unwrap();
        super::capture_events(|| {
            super::super::input_ledger::capture_events(|| {
                ledger::with_test_ledger(test_ledger, || {
                    native_barrier.wait();
                    repo.freeze_external_input(SESSION, "race", envelope)
                        .unwrap()
                })
            })
            .0
        })
        .0
    });
    let carries = f.on(|| {
        barrier.wait();
        f.repository
            .reserve_owner_carries(SESSION, "owner-race", 1, envelope)
            .unwrap()
    });
    let frozen = native.join().unwrap();
    if carries.is_empty() {
        assert!(!f
            .repository
            .owner_delivery_blocked(SESSION, "race")
            .unwrap());
        assert!(f.repository.set_owner_hold(SESSION, "race", true).is_err());
    } else {
        // Native sweep retrieved the reserved immutable identity for status
        // recovery; it is fenced off from independent submission.
        assert_eq!(carries, [frozen]);
        assert!(f
            .repository
            .owner_delivery_blocked(SESSION, "race")
            .unwrap());
    }
}

#[test]
fn carry_reconciles_only_real_receipt_and_keeps_reason_and_consuming_turn() {
    let f = Fixture::new();
    f.on(|| {
        f.queue("receipt");
        let e = f
            .repository
            .reserve_owner_carries(SESSION, "owner-5", 8, envelope)
            .unwrap()
            .remove(0);
        let mut status: crate::app_server::external_input::Status = serde_json::from_value(json!({
            "messageId": e.message.id, "semanticSha256": e.semantic_sha256,
            "deliveryState": "unknown", "receipt": null,
            "processing": {"state":"none", "attemptId":null, "reason":null}
        }))
        .unwrap();
        f.repository
            .observe_external_input("receipt", &e, &status)
            .unwrap();
        assert!(f
            .repository
            .owner_delivery_blocked(SESSION, "receipt")
            .unwrap());
        status.delivery_state = crate::app_server::external_input::DeliveryState::Pending;
        f.repository
            .observe_external_input("receipt", &e, &status)
            .unwrap();
        let accepted = f
            .ledger
            .get(SESSION, &["receipt".into()])
            .unwrap()
            .records
            .remove(0);
        assert_eq!(accepted.state, InputState::Accepted);
        assert_eq!(accepted.reason.as_deref(), Some("carriedByOwner"));
        assert!(f
            .repository
            .release_owner_carries(SESSION, "owner-5")
            .is_err());
        let mut receipt = Receipt {
            schema: "codex.external-input-receipt.v1".into(),
            receipt_id: String::new(),
            owner_id: SESSION.into(),
            thread_id: e.thread_id.clone(),
            message_id: e.message.id.clone(),
            semantic_sha256: e.semantic_sha256.clone(),
            response_item_id: e.message.id.clone(),
            turn_id: "actual-consuming-turn".into(),
            ordinal: 1,
        };
        receipt.receipt_id = receipt.digest_id();
        let mut wrong = receipt.clone();
        wrong.semantic_sha256 = "b".repeat(64);
        wrong.receipt_id = wrong.digest_id();
        assert!(f
            .repository
            .record_external_input_delivered(SESSION, "receipt", &e, &wrong, 7)
            .is_err());
        let mut wrong_item = receipt.clone();
        wrong_item.response_item_id = "foreign-native-item".into();
        wrong_item.receipt_id = wrong_item.digest_id();
        assert!(f
            .repository
            .record_external_input_delivered(SESSION, "receipt", &e, &wrong_item, 7)
            .is_err());
        f.repository
            .record_external_input_delivered(SESSION, "receipt", &e, &receipt, 7)
            .unwrap();
        let read = f
            .ledger
            .get(SESSION, &["receipt".into()])
            .unwrap()
            .records
            .remove(0);
        assert_eq!(read.state, InputState::InContext);
        assert_eq!(read.reason.as_deref(), Some("carriedByOwner"));
        assert_eq!(read.turn_id.as_deref(), Some("actual-consuming-turn"));
        assert_eq!(
            read.timeline_item_id.as_deref(),
            Some(e.message.id.as_str())
        );
        assert!(f
            .repository
            .release_owner_carries(SESSION, "owner-5")
            .is_err());
    });
}

#[test]
fn explicit_recipient_retry_releases_unknown_owner_reservation_only_when_requested() {
    let f = Fixture::new();
    f.on(|| {
        // Fixture source owner is SENDER, while SESSION is the recipient.
        f.queue("explicit-retry");
        let frozen = f
            .repository
            .reserve_owner_carries(SESSION, "owner-unknown", 1, envelope)
            .unwrap()
            .remove(0);
        let unknown: crate::app_server::external_input::Status = serde_json::from_value(json!({
            "messageId": frozen.message.id, "semanticSha256": frozen.semantic_sha256,
            "deliveryState": "unknown", "receipt": null,
            "processing": {"state":"none", "attemptId":null, "reason":null}
        }))
        .unwrap();
        f.repository
            .observe_external_input("explicit-retry", &frozen, &unknown)
            .unwrap();
        assert!(f
            .repository
            .owner_delivery_blocked(SESSION, "explicit-retry")
            .unwrap());
        assert_eq!(
            f.repository
                .freeze_external_input(SESSION, "explicit-retry", |_| panic!(
                    "status recovery uses same identity"
                ))
                .unwrap(),
            frozen
        );
        assert!(
            !f.repository
                .input_ledger_redeliver(SENDER, "explicit-retry", Utc::now())
                .unwrap(),
            "originating source owner cannot retry the recipient's input"
        );
        assert!(f
            .repository
            .owner_delivery_blocked(SESSION, "explicit-retry")
            .unwrap());
        assert!(
            f.repository
                .input_ledger_redeliver(SESSION, "explicit-retry", Utc::now())
                .unwrap(),
            "explicit retry belongs to snapshot recipient, not the row's originating owner"
        );
        assert!(!f
            .repository
            .owner_delivery_blocked(SESSION, "explicit-retry")
            .unwrap());
        assert!(f
            .repository
            .owner_carry_ids(SESSION, "owner-unknown")
            .unwrap()
            .is_empty());
        let retried = f.repository.snapshot_by_message_id("explicit-retry").unwrap().unwrap().external_input.unwrap();
        assert_ne!(retried.message.id, frozen.message.id, "retry gets a new native attempt");
        assert_eq!(retried.message.text, frozen.message.text);
        assert_eq!(retried.message.source, frozen.message.source);
        assert_eq!(retried.message.delivery, frozen.message.delivery);
        retried.validate().unwrap();
        assert_eq!(f.repository.source_id_for_native(SESSION, &retried.message.id).unwrap().as_deref(), Some("explicit-retry"));
        assert!(f.repository.source_id_for_native(SESSION, &frozen.message.id).unwrap().is_none());
        assert!(f.repository.source_id_for_native(SENDER, &retried.message.id).unwrap().is_none());
        let same = f.repository.freeze_external_input(SESSION,"explicit-retry", |_| panic!("already frozen retry")).unwrap();
        assert_eq!(same,retried,"automatic redrive reuses the retry attempt");
        let old_receipt = json!({"messageId":frozen.message.id,"durable":true,"receiptScope":"local-inbox"});
        assert!(f.repository.record_local_inbox(SESSION,"explicit-retry",&old_receipt).is_err());
        let new_receipt = json!({"messageId":retried.message.id,"durable":true,"receiptScope":"local-inbox"});
        f.repository.record_local_inbox(SESSION,"explicit-retry",&new_receipt).unwrap();
        assert_eq!(f.repository.canonical_message("explicit-retry").unwrap().id,"explicit-retry");
        assert!(!f.repository.record_native_context_injected(SESSION,"explicit-retry",&frozen,"old-turn").unwrap(),"late original attempt cannot settle the retry");
        assert!(f.repository.record_native_context_injected(SESSION,"explicit-retry",&retried,"retry-turn").unwrap());
        let delivered = f.repository.snapshot_by_message_id("explicit-retry").unwrap().unwrap();
        assert_eq!(delivered.state,"delivered");
        assert_eq!(f.ledger.get(SESSION,&["explicit-retry".into()]).unwrap().records.len(),1,"source identity stays stable");
    });
}
