//! Read-only evidence from the pinned adapter's existing schema. A timeline
//! input alone is staged before the model reads it; require a model output in
//! the same turn too. Never modify adapter state or synthesize a receipt.
use anyhow::{ensure, Context};
use cutex::app_server::external_input::{Envelope, Receipt, Response, Status, DeliveryState, ProcessingStatus, ProcessingState};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::path::Path;

#[cfg(test)]
#[path = "copied_state_tests.rs"]
mod copied_state_tests;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Evidence { Unknown, Persisted(String), FailedUnconfirmed(String) }

#[cfg(test)]
pub(super) fn persisted_turn(path: &Path, expected: &Envelope, owner: Option<&str>) -> anyhow::Result<Option<String>> {
    Ok(match observe(path, expected, owner)? { Evidence::Persisted(turn) => Some(turn), _ => None })
}

pub(super) fn observe(path: &Path, expected: &Envelope, owner: Option<&str>) -> anyhow::Result<Evidence> {
    expected.validate()?;
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(100))?;
    let tx = db.transaction()?;
    let row = tx.query_row(
        "SELECT e.envelope, n.turn_id, i.payload, e.receipt, i.seq, n.payload, t.state FROM external_inputs e
         JOIN notes n ON n.thread_id=e.thread_id AND n.message_id=e.message_id
         JOIN items i ON i.id=e.message_id AND i.turn_id=n.turn_id
         JOIN turns t ON t.id=n.turn_id AND t.thread_id=e.thread_id AND t.reverted=0
         WHERE e.thread_id=?1 AND e.message_id=?2 AND n.state='injected'",
        [&expected.thread_id, &expected.message.id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, i64>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?)),
    ).optional()?;
    let Some((raw, turn, raw_item, receipt, input_seq, raw_note, turn_state)) = row else { return Ok(Evidence::Unknown); };
    // Compare the complete known envelope shape, including original generation
    // and the re-derived digest. Extra stored fields do not change identity.
    let stored: Value = serde_json::from_str(&raw)?;
    let expected_json = serde_json::to_value(expected)?;
    ensure!(expected_json.as_object().context("envelope object")?.iter()
        .all(|(key, value)| stored.get(key) == Some(value)), "durable owner envelope binding mismatch");
    ensure!(!turn.is_empty(), "empty durable owner context turn");
    let item: Value = serde_json::from_str(&raw_item)?;
    let output: Value = serde_json::from_str(item["output"].as_str().context("external output missing")?)?;
    ensure!(item["id"] == expected.message.id && item["type"] == "functionCallOutput"
        && item["name"] == "external_event" && item["namespace"] == "external"
        && item.get("externalInputView") == expected_json.get("view")
        && output["source"] == expected_json["message"]["source"]
        && output["type"] == expected.message.event_type && output["text"] == expected.message.text,
        "durable owner native item mismatch");
    if let Some(raw) = receipt {
        let receipt: Receipt = serde_json::from_str(&raw)?;
        ensure!(receipt.turn_id == turn, "durable owner receipt turn mismatch");
        let response = Response { version: 1, owner_id: expected.owner_id.clone(), thread_id: expected.thread_id.clone(),
            runtime_generation: expected.runtime_generation, statuses: vec![Status {
                message_id: expected.message.id.clone(), semantic_sha256: expected.semantic_sha256.clone(),
                delivery_state: DeliveryState::ContextPersisted, receipt: Some(receipt),
                processing: ProcessingStatus { state: ProcessingState::None, attempt_id: None, reason: None },
            }] };
        response.validate(&cutex::launch::stock::ExternalInputBinding {
            version: 1, owner_id: expected.owner_id.clone(), thread_id: expected.thread_id.clone(),
            runtime_generation: expected.runtime_generation, canonical_byte_limit: Default::default(),
        }, &[expected.key()])?;
        return Ok(Evidence::Persisted(turn));
    }
    // Missing receipts can only be inferred for a carrier's own start. Busy
    // hook items are staged before the next model request; even a later tool
    // output could belong to the request preceding that hook.
    let Some(owner) = owner else { return Ok(Evidence::Unknown); };
    use sha2::{Digest, Sha256};
    let suffix = if owner.len() <= 200 { owner.to_string() } else { format!("{:x}",Sha256::digest(owner.as_bytes())) };
    let carrier_id = format!("owner-{suffix}");
    let note: Value = serde_json::from_str(&raw_note)?;
    if note["metadata"]["ownerCarrier"] != carrier_id { return Ok(Evidence::Unknown); }
    ensure!(note["source"] == expected_json["message"]["source"]
        && note["eventType"] == expected.message.event_type && note["text"] == expected.message.text
        && note.get("view") == expected_json.get("view"), "durable injected note content mismatch");
    let carrier: Option<(String, String)> = tx.query_row(
        "SELECT payload,state FROM inputs WHERE thread_id=?1 AND message_id=?2 AND turn_id=?3",
        rusqlite::params![expected.thread_id, carrier_id, turn], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
    let Some((carrier, carrier_state)) = carrier else { return Ok(Evidence::Unknown); };
    let carrier: Value = serde_json::from_str(&carrier)?;
    ensure!(carrier["source"]["kind"] == "user" && carrier["source"]["id"] == "local-controller"
        && carrier["eventType"] == "user.message" && carrier["clientId"] == owner
        && carrier["metadata"]["ownerCarries"].as_array().is_some_and(|carries|
            carries.iter().any(|carry| expected_json.as_object().expect("envelope object").iter()
                .all(|(key,value)| carry.get(key) == Some(value)))), "durable carrier input binding mismatch");
    // These are the model's output kinds in the existing adapter, rather than
    // treating an arbitrary unknown future input kind as proof of model read.
    let answered: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM items WHERE turn_id=?1 AND seq>?2
         AND json_extract(payload,'$.type') IN ('agentMessage','reasoning','mcpToolCall','commandExecution'))",
        rusqlite::params![turn, input_seq], |row| row.get(0))?;
    if answered { return Ok(Evidence::Persisted(turn)); }
    // Only the exact original owner input's durable terminal failure releases
    // admission. A busy/staged input or an unrelated failed turn is not proof.
    if turn_state == "failed" && carrier_state == "failed" {
        return Ok(Evidence::FailedUnconfirmed(turn));
    }
    Ok(Evidence::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture { path: std::path::PathBuf, envelope: Envelope }
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); } }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("owner-proof-{}.sqlite3", uuid::Uuid::new_v4()));
            let submission = super::super::Submission {
                message_id: "cutex-ask-owner-proof".into(), source_agent: None,
                event_type: "ANSWER", text: "original answer\n第二个结果".into(),
                asks: vec!["delegation-one".into(), "delegation-two".into()], attempt: None,
                view: Some(json!({"schema":"cutex.delegation-results.v1","data":{"items":[]}})),
            };
            let envelope = super::super::owner_note_envelope("cutex.private", "thread-original", 31, &submission).unwrap();
            let db = Connection::open(&path).unwrap();
            // Actual existing adapter schema/shape, deliberately no receipt.
            db.execute_batch("CREATE TABLE external_inputs(thread_id TEXT,message_id TEXT,envelope TEXT,receipt TEXT,last_status TEXT,PRIMARY KEY(thread_id,message_id));
                CREATE TABLE notes(thread_id TEXT,message_id TEXT,turn_id TEXT,state TEXT,payload TEXT,UNIQUE(thread_id,message_id));
                CREATE TABLE turns(id TEXT PRIMARY KEY,thread_id TEXT,state TEXT,reverted INTEGER DEFAULT 0);
                CREATE TABLE inputs(thread_id TEXT,message_id TEXT,turn_id TEXT,payload TEXT,state TEXT DEFAULT 'completed');
                CREATE TABLE items(seq INTEGER PRIMARY KEY AUTOINCREMENT,turn_id TEXT,id TEXT UNIQUE,payload TEXT);").unwrap();
            db.execute("INSERT INTO turns(id,thread_id,state) VALUES('carrier-turn','thread-original','interrupted')", []).unwrap();
            db.execute("INSERT INTO external_inputs VALUES(?1,?2,?3,NULL,?4)", rusqlite::params![envelope.thread_id, envelope.message.id,
                serde_json::to_string(&envelope).unwrap(), "{\"deliveryState\":\"pending\",\"processing\":{\"state\":\"held\",\"reason\":\"interrupted\"}}"] ).unwrap();
            db.execute("INSERT INTO notes VALUES(?1,?2,'carrier-turn','injected',?3)", rusqlite::params![envelope.thread_id,envelope.message.id,json!({"metadata":{"ownerCarrier":"owner-carrier"},"source":envelope.message.source,"eventType":envelope.message.event_type,"text":envelope.message.text,"view":envelope.view}).to_string()]).unwrap();
            db.execute("INSERT INTO inputs(thread_id,message_id,turn_id,payload) VALUES(?1,'owner-carrier','carrier-turn',?2)", rusqlite::params![envelope.thread_id,json!({"source":{"kind":"user","id":"local-controller"},"eventType":"user.message","clientId":"carrier","metadata":{"ownerCarries":[envelope]}}).to_string()]).unwrap();
            let item = json!({"id":envelope.message.id,"type":"functionCallOutput","name":"external_event","namespace":"external",
                "externalInputView":envelope.view,"output":serde_json::to_string(&json!({"source":envelope.message.source,
                    "type":envelope.message.event_type,"text":envelope.message.text})).unwrap()});
            db.execute("INSERT INTO items(turn_id,id,payload) VALUES('carrier-turn',?1,?2)", rusqlite::params![envelope.message.id,item.to_string()]).unwrap();
            db.execute("INSERT INTO items(turn_id,id,payload) VALUES('carrier-turn','model-proof',?1)", [json!({"type":"mcpToolCall","id":"model-proof"}).to_string()]).unwrap();
            Self { path, envelope }
        }
        fn change(&self, sql: &str) { Connection::open(&self.path).unwrap().execute_batch(sql).unwrap(); }
    }

    #[test]
    fn interrupted_original_generation_with_model_output_is_durable_and_read_only() {
        let f = Fixture::new();
        f.change("ALTER TABLE external_inputs ADD COLUMN future_column TEXT DEFAULT 'keep'; UPDATE external_inputs SET envelope=json_set(envelope,'$.futureField','keep');");
        let before = std::fs::read(&f.path).unwrap();
        assert_eq!(persisted_turn(&f.path,&f.envelope,Some("carrier")).unwrap(),Some("carrier-turn".into()));
        assert_eq!(std::fs::read(&f.path).unwrap(),before);
        let mut current_claim = f.envelope.clone(); current_claim.runtime_generation = 36;
        assert!(persisted_turn(&f.path,&current_claim,Some("carrier")).is_err(), "a current occurrence cannot attest to gen31");
    }

    #[test]
    fn missing_staged_forged_rebound_and_replaced_evidence_never_proves_read() {
        for mutation in [
            "DELETE FROM items WHERE id='model-proof'",
            "UPDATE items SET seq=0 WHERE id='model-proof'",
            "DELETE FROM external_inputs",
            "UPDATE notes SET state='pending'",
            "UPDATE notes SET turn_id='other-turn'",
            "UPDATE notes SET payload=json_set(payload,'$.metadata.ownerCarrier','owner-forged')",
            "UPDATE notes SET payload=json_set(payload,'$.text','forged')",
            "UPDATE inputs SET payload=json_set(payload,'$.clientId','wrong-owner')",
            "UPDATE inputs SET payload=json_set(payload,'$.metadata.ownerCarries',json('[]'))",
            "UPDATE inputs SET turn_id='reserved-busy-turn'",
            "UPDATE turns SET thread_id='replacement-thread'",
            "UPDATE turns SET reverted=1",
            "UPDATE external_inputs SET envelope=json_set(envelope,'$.runtimeGeneration',30)",
            "UPDATE external_inputs SET envelope=json_set(envelope,'$.ownerId','cutex.other')",
            "UPDATE external_inputs SET envelope=json_set(envelope,'$.semanticSha256','forged')",
            "UPDATE items SET payload=json_set(payload,'$.output','{}') WHERE id<>'model-proof'",
            "UPDATE items SET payload=json_set(payload,'$.externalInputView.data.items',json('[1]')) WHERE id<>'model-proof'",
            "UPDATE items SET payload=json_set(payload,'$.type','unknownFutureInput') WHERE id='model-proof'",
        ] {
            let f=Fixture::new(); f.change(mutation);
            assert!(!matches!(persisted_turn(&f.path,&f.envelope,Some("carrier")),Ok(Some(_))), "{mutation}");
        }
    }

    #[test]
    fn busy_hook_with_later_completion_is_unknown_without_a_typed_receipt() {
        let f=Fixture::new();
        f.change("UPDATE inputs SET message_id='steer-carrier',turn_id='reserved-steer-turn';
            UPDATE notes SET payload=json_set(payload,'$.metadata.ownerCarrier','steer-carrier');
            UPDATE items SET payload=json_set(payload,'$.type','commandExecution') WHERE id='model-proof';");
        assert_eq!(persisted_turn(&f.path,&f.envelope,Some("carrier")).unwrap(),None);
        let mut receipt=Receipt { schema:"codex.external-input-receipt.v1".into(),receipt_id:String::new(),
            owner_id:f.envelope.owner_id.clone(),thread_id:f.envelope.thread_id.clone(),message_id:f.envelope.message.id.clone(),
            semantic_sha256:f.envelope.semantic_sha256.clone(),response_item_id:f.envelope.message.id.clone(),
            turn_id:"carrier-turn".into(),ordinal:1 };
        receipt.receipt_id=receipt.digest_id();
        let db=Connection::open(&f.path).unwrap();
        db.execute("UPDATE external_inputs SET receipt=?1",[serde_json::to_string(&receipt).unwrap()]).unwrap();
        assert_eq!(persisted_turn(&f.path,&f.envelope,Some("carrier")).unwrap(),Some("carrier-turn".into()));
        db.execute("UPDATE external_inputs SET receipt=json_set(receipt,'$.receiptId','forged')",[]).unwrap();
        assert!(persisted_turn(&f.path,&f.envelope,Some("carrier")).is_err());
    }

    #[test]
    fn failed_owner_carry_exact_failed_input_is_unknown_not_read() {
        let f = Fixture::new();
        f.change("DELETE FROM items WHERE id='model-proof'; UPDATE turns SET state='failed'; UPDATE inputs SET state='failed';");
        let before = std::fs::read(&f.path).unwrap();
        assert_eq!(observe(&f.path, &f.envelope, Some("carrier")).unwrap(),
            Evidence::FailedUnconfirmed("carrier-turn".into()));
        assert_eq!(persisted_turn(&f.path, &f.envelope, Some("carrier")).unwrap(), None);
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
    }

    #[test]
    fn failed_owner_carry_inflight_rebound_and_forged_evidence_cannot_unlock() {
        for mutation in [
            "UPDATE turns SET state='inProgress'",
            "UPDATE inputs SET state='processing'",
            "UPDATE inputs SET state='queued'",
            "UPDATE inputs SET turn_id='another-turn'",
            "UPDATE inputs SET payload=json_set(payload,'$.clientId','another-owner')",
            "UPDATE inputs SET payload=json_set(payload,'$.metadata.ownerCarries',json('[]'))",
            "UPDATE notes SET payload=json_set(payload,'$.metadata.ownerCarrier','owner-other')",
            "UPDATE turns SET reverted=1",
            "UPDATE turns SET thread_id='another-thread'",
            "UPDATE external_inputs SET envelope=json_set(envelope,'$.runtimeGeneration',30)",
            "UPDATE external_inputs SET envelope=json_set(envelope,'$.semanticSha256','forged')",
            "UPDATE notes SET state='reserved'",
            "DELETE FROM external_inputs",
        ] {
            let f = Fixture::new();
            f.change("DELETE FROM items WHERE id='model-proof'; UPDATE turns SET state='failed'; UPDATE inputs SET state='failed';");
            f.change(mutation);
            assert!(!matches!(observe(&f.path, &f.envelope, Some("carrier")), Ok(Evidence::FailedUnconfirmed(_))), "{mutation}");
        }
    }

    #[test]
    fn failed_owner_carry_actual_read_proof_still_wins() {
        let f = Fixture::new();
        f.change("UPDATE turns SET state='failed'; UPDATE inputs SET state='failed';");
        assert_eq!(observe(&f.path, &f.envelope, Some("carrier")).unwrap(), Evidence::Persisted("carrier-turn".into()));
    }

    #[test]
    fn failed_owner_carry_copied_rover_evidence_optional_read_only_probe() {
        let Ok(path) = std::env::var("CUTEX_TEST_FAILED_OWNER_COPY") else { return; };
        let before = std::fs::read(&path).unwrap();
        let db = Connection::open_with_flags(&path,OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let raw:String = db.query_row("SELECT envelope FROM external_inputs WHERE message_id='cutex-ask-owner-5632d1277cb942a891b7d6932837ff15'",[],|row|row.get(0)).unwrap();
        let expected:Envelope = serde_json::from_str(&raw).unwrap();
        assert_eq!(expected.runtime_generation,2);
        assert_eq!(observe(Path::new(&path),&expected,Some("127fda09-7b81-45c0-9591-324e96d9ff68")).unwrap(),
            Evidence::FailedUnconfirmed("b4ff1bb1-0ce8-48ef-8f69-a7879963419c".into()));
        assert_eq!(std::fs::read(&path).unwrap(),before);
    }

    #[test]
    fn copied_adapter_evidence_optional_read_only_probe() {
        let Ok(path) = std::env::var("CUTEX_TEST_OWNER_NOTE_COPY") else { return; };
        let db = Connection::open_with_flags(&path,OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let raw: String = db.query_row("SELECT envelope FROM external_inputs WHERE message_id='cutex-ask-owner-42441186895e4839865fd2859744f493'",[],|row|row.get(0)).unwrap();
        let envelope: Envelope=serde_json::from_str(&raw).unwrap();
        assert_eq!(envelope.runtime_generation,31);
        assert_eq!(persisted_turn(Path::new(&path),&envelope,Some("a68d8dd8-c848-4422-9ba8-a3912334f5b0")).unwrap(),Some("217f7b75-7fbb-4928-8a1a-60846ac06924".into()));
    }
}
