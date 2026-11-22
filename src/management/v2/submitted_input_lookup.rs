//! Restart-safe direct lookup of submitted input fingerprints. The journal
//! remains authoritative; index rows and the validated byte checkpoint commit
//! together. No ledger or journal lock is held while parsing the snapshot.
//! Registered repository cache: store::manifest::submitted-input-index.
//! Kits include a consistent SQLite snapshot; an absent cache rebuilds from
//! the retained authoritative journal without touching delivery/source rows.
use super::{EventRepository, LookupCheckpoint};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub(super) fn latest(repository: &EventRepository, session: &str, id: &str)
    -> anyhow::Result<(Option<String>, usize)>
{
    let mut db = Connection::open(repository.root().join("submitted-input-index.sqlite3"))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS submitted_inputs(session TEXT NOT NULL,input_id TEXT NOT NULL,sha256 TEXT NOT NULL,PRIMARY KEY(session,input_id));
        CREATE TABLE IF NOT EXISTS submitted_checkpoint(singleton INTEGER PRIMARY KEY CHECK(singleton=1),stream TEXT NOT NULL,cursor TEXT NOT NULL,sequence INTEGER NOT NULL,byte_offset INTEGER NOT NULL);")?;
    // Only lookups acquire this database lock. Snapshot construction takes the
    // journal lock briefly; journal appends never acquire this database lock.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let snapshot = repository.integration_read_snapshot()?;
    let checkpoint: Option<LookupCheckpoint> = tx.query_row(
        "SELECT stream,cursor,sequence,byte_offset FROM submitted_checkpoint WHERE singleton=1", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get::<_,i64>(2)? as u64,row.get::<_,i64>(3)? as u64)),
    ).optional()?;
    let checkpoint = if checkpoint.as_ref().is_some_and(|cp| cp.0 != snapshot.metadata.stream_id
        || snapshot.metadata.latest.as_ref().is_none_or(|last| cp.2 > last.sequence
            || (cp.2 == last.sequence && cp.1 != last.cursor))) {
        tx.execute("DELETE FROM submitted_inputs", [])?;
        tx.execute("DELETE FROM submitted_checkpoint", [])?;
        None
    } else { checkpoint };
    let mut reader = snapshot.reader(checkpoint.as_ref())?;
    let mut scanned = 0;
    while let Some((events, end)) = reader.batch(256)? {
        scanned += events.len();
        for event in events {
            let Some(message) = event.cutex.filter(|m| m.method == "cutex/userInput/submitted") else { continue };
            let Some(input_id) = message.params["clientUserMessageId"].as_str() else { continue };
            let hash = super::super::native_requests::canonical_request_sha256(&message.params["input"])?;
            tx.execute("INSERT INTO submitted_inputs(session,input_id,sha256) VALUES (?1,?2,?3)
                ON CONFLICT(session,input_id) DO UPDATE SET sha256=excluded.sha256",
                params![event.cutex_session_id,input_id,hash])?;
        }
        tx.execute("INSERT INTO submitted_checkpoint(singleton,stream,cursor,sequence,byte_offset) VALUES (1,?1,?2,?3,?4)
            ON CONFLICT(singleton) DO UPDATE SET stream=excluded.stream,cursor=excluded.cursor,sequence=excluded.sequence,byte_offset=excluded.byte_offset",
            params![end.0,end.1,i64::try_from(end.2)?,i64::try_from(end.3)?])?;
    }
    let result = tx.query_row("SELECT sha256 FROM submitted_inputs WHERE session=?1 AND input_id=?2",
        params![session,id], |row| row.get(0)).optional()?;
    tx.commit()?;
    Ok((result, scanned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::model::{CutexMessage,EventCorrelation,EventSource,PendingEvent};
    use serde_json::json;

    #[test]
    fn latest_submitted_is_direct_after_restart_and_only_indexes_the_new_tail() {
        let root = std::env::temp_dir().join(format!("submitted-index-{}",uuid::Uuid::new_v4()));
        let events = EventRepository::open(&root,"test").unwrap();
        let append = |events: &EventRepository,session:&str,id:&str,text:&str| events.append(PendingEvent {
            cutex_session_id:session.into(),host_id:"test".into(),source:EventSource::Cutex,
            schema:None,correlation:EventCorrelation::default(),native:None,
            cutex:Some(CutexMessage {method:"cutex/userInput/submitted".into(),params:json!({
                "managementRequestId":"index-test","clientUserMessageId":id,"origin":{"kind":"other","clientId":"desktop"},
                "input":[{"type":"text","text":text}],"disposition":"steered","nativeRequestId":"fake",
                "nativeMethod":"turn/steer","appServerAccepted":true,"submittedAt":chrono::Utc::now().to_rfc3339()
            })}),
        }).unwrap();
        append(&events,"s1","same","old");append(&events,"s2","same","other session");
        let old = super::super::super::native_requests::canonical_request_sha256(&json!([{"type":"text","text":"old"}])).unwrap();
        assert_eq!(latest(&events,"s1","same").unwrap(),(Some(old.clone()),2));
        assert_eq!(latest(&events,"s1","missing").unwrap(),(None,0));
        drop(events);
        let events = EventRepository::open(&root,"test").unwrap();
        assert_eq!(latest(&events,"s1","same").unwrap(),(Some(old),0),"restart uses the durable direct index");
        append(&events,"s1","same","edited");
        let edited = super::super::super::native_requests::canonical_request_sha256(&json!([{"type":"text","text":"edited"}])).unwrap();
        assert_eq!(latest(&events,"s1","same").unwrap(),(Some(edited.clone()),1));
        assert_eq!(latest(&events,"s2","same").unwrap().1,0);
        drop(events);
        std::fs::remove_file(root.join("submitted-input-index.sqlite3")).unwrap();
        let events=EventRepository::open(&root,"test").unwrap();
        assert_eq!(latest(&events,"s1","same").unwrap(),(Some(edited),3),"a missing advisory cache rebuilds from the journal");
        drop(events);let _=std::fs::remove_dir_all(root);
    }
}
