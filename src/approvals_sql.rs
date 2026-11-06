//! S10 typed SQL repository for approvals, rules, confirmation nonces and audit.
use super::*;
use crate::store::{self, ChangeKind, OpCounts, Scope, StoreDb, WriteTx};
use rusqlite::{params, types::Value as SqlValue, Connection, OptionalExtension};
use std::sync::Arc;

const ID: &str = "approvals";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS approval_meta(id INTEGER PRIMARY KEY CHECK(id=1),rules_revision INTEGER NOT NULL,legacy_import INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS approval_rows(
 ordinal INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,
 requester TEXT NOT NULL,digest TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('pending','approved','denied','consumed','expired')),kind TEXT NOT NULL,
 created_ms INTEGER,expires_ms INTEGER,wait_until INTEGER,wait_id TEXT,wait_end TEXT,orphan_queued INTEGER NOT NULL DEFAULT 0,data BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS approval_state ON approval_rows(state,ordinal DESC);
CREATE INDEX IF NOT EXISTS approval_request_ordered ON approval_rows(requester,digest,ordinal,state);
CREATE INDEX IF NOT EXISTS approval_agent ON approval_rows(requester,ordinal);
CREATE INDEX IF NOT EXISTS approval_kind ON approval_rows(kind,ordinal);
CREATE INDEX IF NOT EXISTS approval_expiry ON approval_rows(expires_ms,id);
CREATE INDEX IF NOT EXISTS approval_retention ON approval_rows(created_ms,id) WHERE state!='pending';
CREATE INDEX IF NOT EXISTS approval_retirable ON approval_rows(ordinal) WHERE state IN ('consumed','expired');
CREATE INDEX IF NOT EXISTS approval_wait_requester ON approval_rows(requester,ordinal) WHERE wait_until IS NOT NULL;
CREATE INDEX IF NOT EXISTS approval_orphans_due ON approval_rows(orphan_queued,wait_until,id) WHERE wait_until IS NOT NULL AND state IN ('approved','denied');
CREATE TABLE IF NOT EXISTS approval_count(id INTEGER PRIMARY KEY CHECK(id=1),n INTEGER NOT NULL);
INSERT OR IGNORE INTO approval_count(id,n) VALUES (1,0);
CREATE TRIGGER IF NOT EXISTS approval_insert_count AFTER INSERT ON approval_rows BEGIN UPDATE approval_count SET n=n+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS approval_delete_count AFTER DELETE ON approval_rows BEGIN UPDATE approval_count SET n=n-1 WHERE id=1; END;
CREATE TABLE IF NOT EXISTS approval_rules(id TEXT PRIMARY KEY,subject TEXT NOT NULL,data BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS approval_rule_subject ON approval_rules(subject,id);
CREATE TABLE IF NOT EXISTS approval_rule_count(id INTEGER PRIMARY KEY CHECK(id=1),n INTEGER NOT NULL);
INSERT OR IGNORE INTO approval_rule_count(id,n) VALUES (1,0);
CREATE TRIGGER IF NOT EXISTS approval_rule_insert_count AFTER INSERT ON approval_rules BEGIN UPDATE approval_rule_count SET n=n+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS approval_rule_delete_count AFTER DELETE ON approval_rules BEGIN UPDATE approval_rule_count SET n=n-1 WHERE id=1; END;
CREATE TABLE IF NOT EXISTS approval_audit(id TEXT PRIMARY KEY,data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS approval_audit_count(id INTEGER PRIMARY KEY CHECK(id=1),n INTEGER NOT NULL);
INSERT OR IGNORE INTO approval_audit_count(id,n) VALUES (1,0);
CREATE TRIGGER IF NOT EXISTS approval_audit_insert_count AFTER INSERT ON approval_audit BEGIN UPDATE approval_audit_count SET n=n+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS approval_audit_delete_count AFTER DELETE ON approval_audit BEGIN UPDATE approval_audit_count SET n=n-1 WHERE id=1; END;
CREATE TABLE IF NOT EXISTS approval_nonces(id TEXT PRIMARY KEY,expires_ms INTEGER,consumed INTEGER NOT NULL,data BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS approval_nonce_expiry ON approval_nonces(expires_ms,id);
CREATE INDEX IF NOT EXISTS approval_nonce_consumed ON approval_nonces(consumed,expires_ms,id);
CREATE TABLE IF NOT EXISTS approval_overrides(id TEXT PRIMARY KEY,data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS approval_orphan_seen(id TEXT PRIMARY KEY);
";

fn decode<T: serde::de::DeserializeOwned>(bytes: Vec<u8>) -> anyhow::Result<T> {
    store::metrics::note(
        ID,
        OpCounts {
            bytes_read: bytes.len() as u64,
            bytes_decoded: bytes.len() as u64,
            rows_visited: 1,
            rows_materialized: 1,
            ..Default::default()
        },
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn note_query() {
    store::metrics::note(
        ID,
        OpCounts {
            sql_statements: 1,
            ..Default::default()
        },
    );
}
fn encode<T: Serialize>(record: &T) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(record)?;
    store::metrics::note(
        ID,
        OpCounts {
            bytes_encoded: bytes.len() as u64,
            bytes_written: bytes.len() as u64,
            ..Default::default()
        },
    );
    Ok(bytes)
}
fn text<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .context("approval enum is not text")?
        .into())
}
fn ms(value: &str) -> Option<i64> {
    timestamp(value).map(|at| at.timestamp_millis())
}

fn approval_put(
    tx: &mut WriteTx<'_>,
    record: &ApprovalRecord,
    importing: bool,
) -> anyhow::Result<()> {
    let previous = tx
        .sql()
        .query_row(
            "SELECT state,wait_id,wait_end FROM approval_rows WHERE id=?1",
            [&record.approval_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(state, id, end)| -> anyhow::Result<ApprovalSeen> {
            Ok(ApprovalSeen {
                state: serde_json::from_value(Value::String(state))?,
                wait: id.map(|id| (id, end)),
            })
        })
        .transpose()?;
    if !importing
        && matches!(
            record.kind,
            ApprovalKind::BrowserClaim | ApprovalKind::BrowserOpen
        )
    {
        let exists: bool = tx.sql().query_row(
            "SELECT EXISTS(SELECT 1 FROM approval_rows WHERE id=?1)",
            [&record.approval_id],
            |r| r.get(0),
        )?;
        if !exists {
            let count: i64 =
                tx.sql()
                    .query_row("SELECT n FROM approval_count WHERE id=1", [], |r| r.get(0))?;
            if count >= MAX_RECORDS as i64 {
                let remove = (count - MAX_RECORDS as i64 + 1) as usize;
                let ids = {
                    let mut stmt=tx.sql().prepare("SELECT id FROM approval_rows WHERE state IN ('consumed','expired') ORDER BY ordinal LIMIT ?1")?;
                    let rows =
                        stmt.query_map([i64::try_from(remove)?], |r| r.get::<_, String>(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                anyhow::ensure!(ids.len() == remove, "browser approval capacity");
                for id in ids {
                    tx.sql()
                        .execute("DELETE FROM approval_rows WHERE id=?1", [&id])?;
                    change(tx, &id, ChangeKind::Delete)?;
                }
            }
        }
    }
    let expiry = match record.state {
        ApprovalState::Pending => timestamp(&record.created_at),
        ApprovalState::Approved | ApprovalState::Denied => record
            .decided_at
            .as_deref()
            .and_then(timestamp)
            .or_else(|| timestamp(&record.created_at)),
        _ => None,
    }
    .map(|at| (at + Duration::hours(APPROVAL_TTL_HOURS)).timestamp_millis());
    tx.sql().execute("INSERT INTO approval_rows(id,requester,digest,state,kind,created_ms,expires_ms,wait_until,data,wait_id,wait_end)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(id) DO UPDATE SET requester=excluded.requester,
        digest=excluded.digest,state=excluded.state,kind=excluded.kind,created_ms=excluded.created_ms,
        expires_ms=excluded.expires_ms,wait_until=excluded.wait_until,data=excluded.data,
        orphan_queued=CASE WHEN approval_rows.wait_id IS excluded.wait_id AND approval_rows.state=excluded.state THEN approval_rows.orphan_queued ELSE 0 END,
        wait_id=excluded.wait_id,wait_end=excluded.wait_end",
        params![record.approval_id,record.requester_cutex_session_id,record.request_sha256,text(&record.state)?,
            text(&record.kind)?,ms(&record.created_at),expiry,
            record.wait.as_ref().filter(|w|w.end.is_none()).and_then(|w|ms(&w.until)),encode(record)?,
            record.wait.as_ref().map(|w|w.id.as_str()),record.wait.as_ref().and_then(|w|w.end.as_deref())])?;
    let summary = serde_json::to_vec(&previous)?;
    let revision = tx.head(store::spec(ID))?.seq + 1;
    tx.record_change(
        store::spec(ID),
        &Scope::Host,
        &record.approval_id,
        revision,
        ChangeKind::Put,
        Some(&summary),
    )?;
    Ok(())
}
fn change(tx: &mut WriteTx<'_>, key: &str, kind: ChangeKind) -> anyhow::Result<()> {
    let revision = tx.head(store::spec(ID))?.seq + 1;
    tx.record_change(store::spec(ID), &Scope::Host, key, revision, kind, None)?;
    Ok(())
}
fn generic_put<T: Serialize>(
    tx: &mut WriteTx<'_>,
    table: &str,
    key: &str,
    record: &T,
) -> anyhow::Result<()> {
    let bytes = encode(record)?;
    tx.sql().execute(&format!("INSERT INTO {table}(id,data) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data"),params![key,bytes])?;
    change(tx, &format!("{table}:{key}"), ChangeKind::Put)
}
fn rule_put(
    tx: &mut WriteTx<'_>,
    rule: &crate::approval_rules::ApprovalRule,
) -> anyhow::Result<()> {
    tx.sql().execute(
        "INSERT INTO approval_rules(id,subject,data) VALUES (?1,?2,?3)
        ON CONFLICT(id) DO UPDATE SET subject=excluded.subject,data=excluded.data",
        params![rule.rule_id, rule.subject.cutex_session_id, encode(rule)?],
    )?;
    change(tx, &format!("rule:{}", rule.rule_id), ChangeKind::Put)
}
fn nonce_put(
    tx: &mut WriteTx<'_>,
    nonce: &crate::approval_rules::ConfirmationToken,
) -> anyhow::Result<()> {
    let wire = serde_json::to_value(nonce)?;
    let key = wire["token"]
        .as_str()
        .context("confirmation token missing")?;
    tx.sql().execute("INSERT INTO approval_nonces(id,expires_ms,data,consumed) VALUES (?1,?2,?3,?4)
        ON CONFLICT(id) DO UPDATE SET expires_ms=excluded.expires_ms,data=excluded.data,consumed=excluded.consumed",
        params![key,wire["expiresAt"].as_str().and_then(ms),encode(nonce)?,wire["consumed"].as_bool().unwrap_or(false)])?;
    change(tx, &format!("nonce:{key}"), ChangeKind::Put)
}
fn read_rows<T: serde::de::DeserializeOwned>(
    sql: &Connection,
    query: &str,
    args: &[SqlValue],
) -> anyhow::Result<Vec<T>> {
    note_query();
    let mut stmt = sql.prepare(query)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(decode(row.get(0)?)?);
    }
    Ok(result)
}

impl ApprovalStore {
    /// Offline discovery must neither migrate nor expire state. `None` means
    /// no SQL authority exists yet, so the caller may inspect legacy JSON.
    pub fn existing_active_obligation(
        path: &Path,
        mut matches: impl FnMut(&ApprovalRecord) -> bool,
    ) -> anyhow::Result<Option<bool>> {
        let Some(mut db) = existing_database(path)? else {
            return Ok(None);
        };
        let tx = db.transaction()?;
        if existing_header(&tx)? {
            return Ok(Some(true));
        }
        let mut stmt=tx.prepare("SELECT data FROM approval_rows WHERE state IN ('pending','approved') ORDER BY state,ordinal")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let record = decode(row.get(0)?)?;
            if matches(&record) {
                return Ok(Some(true));
            }
        }
        Ok(Some(false))
    }
    pub fn existing_rule_blockers(path: &Path) -> anyhow::Result<Option<bool>> {
        let Some(mut db) = existing_database(path)? else {
            return Ok(None);
        };
        let tx = db.transaction()?;
        if existing_header(&tx)? {
            return Ok(Some(true));
        }
        let blocked = tx.query_row(
            "SELECT (SELECT rules_revision FROM approval_meta WHERE id=1)>0
            OR EXISTS(SELECT 1 FROM approval_rules) OR EXISTS(SELECT 1 FROM approval_audit)
            OR EXISTS(SELECT 1 FROM approval_nonces) OR EXISTS(SELECT 1 FROM approval_overrides)",
            [],
            |r| r.get(0),
        )?;
        Ok(Some(blocked))
    }
    pub fn peek_id(&self, id: &str) -> anyhow::Result<Option<ApprovalRecord>> {
        self.database()?.read(|tx| {
            note_query();
            tx.sql()
                .query_row("SELECT data FROM approval_rows WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
                .map(decode)
                .transpose()
        })
    }
    pub fn list_page(
        &self,
        include_decided: bool,
        after: Option<&str>,
        budget: store::PageBudget,
        now: DateTime<Utc>,
    ) -> anyhow::Result<(Vec<ApprovalRecord>, Option<String>)> {
        anyhow::ensure!(
            budget.max_rows > 0 && budget.max_rows <= 500 && budget.max_bytes > 0,
            "invalid approval page budget"
        );
        let db = self.database()?;
        let cursor = after
            .map(|text| store::ListCursor::decode(store::spec(ID), text))
            .transpose()?;
        db.write(|tx|{
            prune_metadata(tx,now)?;
            let spec=store::spec(ID);let head=tx.head(spec)?;
            if let Some(cursor)=&cursor {cursor.check(db.instance_id(),spec,&head,&Scope::Host,u64::from(include_decided))?;}
            let before=cursor.as_ref().map(|c|c.after_key().parse::<i64>().map_err(|_|store::StoreError::Invalid("invalid approval page key".into()))).transpose()?.unwrap_or(i64::MAX);
            let mut state=StoreFile::default();let mut last=None;let mut more=false;let mut used=0;
            {
                let query=if include_decided {"SELECT ordinal,length(data),data FROM approval_rows WHERE ordinal<?1 ORDER BY ordinal DESC LIMIT ?2"}
                    else {"SELECT ordinal,length(data),data FROM approval_rows WHERE state='pending' AND ordinal<?1 ORDER BY ordinal DESC LIMIT ?2"};
                let mut stmt=tx.sql().prepare(query)?;
                let mut rows=stmt.query(params![before,i64::try_from(budget.max_rows+1)?])?;
                while let Some(row)=rows.next()? {
                    let len=usize::try_from(row.get::<_,i64>(1)?)?;
                    if !budget.admits(state.approvals.len(),used,len){more=true;break;}
                    last=Some(row.get::<_,i64>(0)?);used+=len;
                    state.approvals.push(decode(row.get(2)?)?);
                }
            }
            let original=state.clone();
            expire_and_prune(&mut state.approvals,now);
            persist(tx,&original,&state,true)?;
            let next=if more {last.map(|key|store::ListCursor::new(db.instance_id(),spec,&head,&Scope::Host,u64::from(include_decided),key.to_string()).encode())} else {None};
            if !include_decided {state.approvals.retain(|r|r.state==ApprovalState::Pending);}
            Ok((state.approvals,next))
        })
    }

    /// Bounded SQL change page. Initial connection primes at the current head;
    /// reconnecting never replays old approvals or wakes a runtime.
    pub(crate) fn observe(
        &self,
        after: Option<&store::ChangeCursor>,
    ) -> anyhow::Result<(
        store::ChangeCursor,
        Vec<(String, Option<ApprovalRecord>, Option<ApprovalSeen>)>,
    )> {
        let db = self.database()?;
        db.read(|tx| {
            let spec = store::spec(ID);
            let head = tx.head(spec)?;
            let Some(after) = after else {
                return Ok((
                    store::ChangeCursor::new(
                        db.instance_id(),
                        spec,
                        head.epoch,
                        &Scope::Host,
                        head.seq,
                    ),
                    Vec::new(),
                ));
            };
            if let Some(reason) = after.resync_reason(db.instance_id(), spec, &head, &Scope::Host) {
                return Err(store::StoreError::Resync(reason).into());
            }
            let changes = tx.changes(
                spec,
                &Scope::Host,
                after.after_seq(),
                store::PageBudget {
                    max_rows: 128,
                    max_bytes: 128 * 1024,
                },
            )?;
            let seq = changes.rows.last().map_or(after.after_seq(), |r| r.seq);
            let mut latest = BTreeMap::new();
            for change in changes.rows {
                latest.insert(change.key, (change.seq, change.kind, change.summary));
            }
            let mut records = Vec::new();
            for (key, (seq, kind, summary)) in latest {
                if kind == ChangeKind::Delete {
                    records.push((seq, key, None, None));
                    continue;
                }
                let bytes: Option<Vec<u8>> = tx
                    .sql()
                    .query_row("SELECT data FROM approval_rows WHERE id=?1", [&key], |r| {
                        r.get(0)
                    })
                    .optional()?;
                if let Some(bytes) = bytes {
                    let previous = summary
                        .map(|bytes| serde_json::from_slice::<Option<ApprovalSeen>>(&bytes))
                        .transpose()?
                        .flatten();
                    records.push((seq, key, Some(decode(bytes)?), previous));
                }
            }
            records.sort_by_key(|r| r.0);
            Ok((
                store::ChangeCursor::new(db.instance_id(), spec, head.epoch, &Scope::Host, seq),
                records
                    .into_iter()
                    .map(|(_, key, r, seen)| (key, r, seen))
                    .collect(),
            ))
        })
    }

    /// Expired unclosed holds become durable, idempotent notice work. The SQL
    /// identity and outbox commit together; delivery runs outside its writer.
    pub(crate) fn claim_orphans(
        &self,
        overdue: DateTime<Utc>,
    ) -> anyhow::Result<Vec<(i64, ApprovalRecord)>> {
        let db = self.database()?;
        db.write(|tx| {
            let rows = due_orphans(tx.sql(), overdue)?;
            for record in rows {
                let key = format!(
                    "{}:{}:{}",
                    record.approval_id,
                    text(&record.state)?,
                    record
                        .wait
                        .as_ref()
                        .map(|w| w.id.as_str())
                        .unwrap_or_default()
                );
                tx.sql().execute(
                    "INSERT OR IGNORE INTO approval_orphan_seen(id) VALUES (?1)",
                    [&key],
                )?;
                tx.sql().execute(
                    "UPDATE approval_rows SET orphan_queued=1 WHERE id=?1",
                    [&record.approval_id],
                )?;
                tx.outbox(
                    store::spec(ID),
                    &store::OutboxItem {
                        topic: "approvals.orphan",
                        idem_key: key,
                        payload: encode(&record)?,
                    },
                )?;
            }
            Ok(())
        })?;
        claim_orphan_page(&db)
    }
    pub(crate) fn finish_orphan(&self, id: i64, success: bool) -> anyhow::Result<()> {
        let db = self.database()?;
        if success {
            store::outbox::ack(&db, &[id])
        } else {
            store::outbox::release(&db, &[id])
        }
    }

    pub(crate) fn database(&self) -> anyhow::Result<Arc<StoreDb>> {
        if let Some(db) = self.db.get() {
            return Ok(db.clone());
        }
        let parent = self
            .path
            .parent()
            .context("approval store has no directory")?;
        crate::management::client_tokens::ensure_private_dir(parent)?;
        let db = StoreDb::open_at(
            &store::domain::APPROVALS,
            &self.path.with_extension("sqlite3"),
        )?;
        db.register(store::spec(ID))?;
        db.write(|tx|{
            tx.sql().execute_batch(SCHEMA)?;
            let marker:Option<bool>=tx.sql().query_row("SELECT legacy_import FROM approval_meta WHERE id=1",[],|r|r.get(0)).optional()?;
            if marker!=Some(false) {
                if marker==Some(true) {anyhow::ensure!(self.path.is_file(),"rollback approval JSON is missing; leave SQL untouched");}
                let state=match std::fs::read(&self.path) {
                    Ok(bytes)=>serde_json::from_slice::<StoreFile>(&bytes).context("approval store is corrupt")?,
                    Err(e) if e.kind()==std::io::ErrorKind::NotFound=>StoreFile::default(),
                    Err(e)=>return Err(e.into()),
                };
                if marker==Some(true) {
                    tx.reset_epoch(store::spec(ID))?;
                    tx.sql().execute_batch("DELETE FROM approval_rows; DELETE FROM approval_rules; DELETE FROM approval_audit; DELETE FROM approval_nonces; DELETE FROM approval_overrides; DELETE FROM approval_orphan_seen;")?;
                    tx.sql().execute("DELETE FROM store_outbox WHERE topic='approvals.orphan'",[])?;
                }
                for record in &state.approvals {approval_put(tx,record,true)?;}
                for rule in &state.approval_rules {rule_put(tx,rule)?;}
                for audit in &state.approval_rule_audit {generic_put(tx,"approval_audit",&audit.audit_id,audit)?;}
                for nonce in &state.approval_rule_confirmations {nonce_put(tx,nonce)?;}
                for (id,value) in &state.director_rule_overrides {generic_put(tx,"approval_overrides",id,value)?;}
                // Do not silently collapse a duplicate legacy ID into one row.
                // Failure rolls the entire import back and preserves its JSON.
                for (table,expected) in [("approval_rows",state.approvals.len()),("approval_rules",state.approval_rules.len()),
                    ("approval_audit",state.approval_rule_audit.len()),("approval_nonces",state.approval_rule_confirmations.len())] {
                    let actual:i64=tx.sql().query_row(&format!("SELECT COUNT(*) FROM {table}"),[],|r|r.get(0))?;
                    anyhow::ensure!(usize::try_from(actual)?==expected,"duplicate legacy IDs in {table}; source preserved");
                }
                tx.sql().execute("INSERT INTO approval_meta(id,rules_revision,legacy_import) VALUES (1,?1,0)
                    ON CONFLICT(id) DO UPDATE SET rules_revision=excluded.rules_revision,legacy_import=0",[i64::try_from(state.rules_revision)?])?;
                if marker.is_none(){tx.record_migration(store::spec(ID),1,None)?;}
            }
            Ok(())
        })?;
        let _ = self.db.set(db.clone());
        Ok(db)
    }

    /// Reuse pure transitions over selected records, and selected authority only.
    pub(crate) fn edit_sql<T>(
        &self,
        predicate: &str,
        args: &[SqlValue],
        rules: Option<&str>,
        nonce: Option<&str>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.edit_sql_inner(None, predicate, args, rules, nonce, mutate)
    }
    pub(crate) fn edit_sql_at<T>(
        &self,
        now: DateTime<Utc>,
        predicate: &str,
        args: &[SqlValue],
        rules: Option<&str>,
        nonce: Option<&str>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.edit_sql_inner(Some(now), predicate, args, rules, nonce, mutate)
    }
    fn edit_sql_inner<T>(
        &self,
        now: Option<DateTime<Utc>>,
        predicate: &str,
        args: &[SqlValue],
        rules: Option<&str>,
        nonce: Option<&str>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.database()?.write(|tx| {
            if let Some(now) = now {
                prune_metadata(tx, now)?;
            }
            let mut state = load(tx.sql(), predicate, args, rules, nonce, false)?;
            let before = state.clone();
            let result = mutate(&mut state);
            persist(tx, &before, &state, true)?;
            Ok(result)
        })
    }
    pub(crate) fn edit_id<T>(
        &self,
        id: &str,
        now: DateTime<Utc>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.edit_sql_at(
            now,
            "id=?",
            &[SqlValue::Text(id.into())],
            None,
            None,
            mutate,
        )
    }
    pub(crate) fn edit_request<T>(
        &self,
        requester: &str,
        digest: &str,
        now: DateTime<Utc>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.edit_sql_at(
            now,
            "requester=? AND digest=? AND state IN ('pending','approved','denied')",
            &[
                SqlValue::Text(requester.into()),
                SqlValue::Text(digest.into()),
            ],
            Some(requester),
            None,
            mutate,
        )
    }
    pub(crate) fn edit_rules<T>(
        &self,
        nonce: Option<&str>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.edit_sql("0", &[], None, nonce, mutate)
    }
    pub(crate) fn new_confirmation<T>(
        &self,
        now: DateTime<Utc>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.database()?.write(|tx|{
            loop {
                let ids={
                    let mut stmt=tx.sql().prepare("SELECT id FROM approval_nonces WHERE consumed=1 OR expires_ms IS NULL OR expires_ms<?1 LIMIT 128")?;
                    let rows=stmt.query_map([now.timestamp_millis()],|r|r.get::<_,String>(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                if ids.is_empty(){break;}
                for id in ids {tx.sql().execute("DELETE FROM approval_nonces WHERE id=?1",[&id])?;change(tx,&format!("nonce:{id}"),ChangeKind::Delete)?;}
            }
            let mut state=load(tx.sql(),"0",&[],None,None,false)?;let before=state.clone();
            let result=mutate(&mut state);persist(tx,&before,&state,true)?;Ok(result)
        })
    }
    pub(crate) fn edit_rule<T>(
        &self,
        id: &str,
        nonce: Option<&str>,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.edit_sql("0", &[], Some(&format!("id:{id}")), nonce, mutate)
    }
    pub(crate) fn edit_director_rule<T>(
        &self,
        subject: &str,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.edit_sql("0", &[], Some(&format!("override:{subject}")), None, mutate)
    }
    pub(crate) fn edit_all_rules<T>(
        &self,
        subjects: &[&str],
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.database()?.write(|tx| {
            let mut state = load(tx.sql(), "0", &[], Some("*"), None, false)?;
            read_overrides(tx.sql(), subjects, &mut state)?;
            let before = state.clone();
            let result = mutate(&mut state);
            persist(tx, &before, &state, true)?;
            Ok(result)
        })
    }
    pub(crate) fn sql_rule_snapshot(&self, subjects: &[&str]) -> anyhow::Result<RuleListSnapshot> {
        self.database()?.read(|tx| {
            let mut data = load(tx.sql(), "0", &[], Some("*"), None, false)?;
            read_overrides(tx.sql(), subjects, &mut data)?;
            Ok(RuleListSnapshot {
                revision: data.rules_revision,
                rules: data.approval_rules,
                director_overrides: data.director_rule_overrides,
            })
        })
    }
    pub(crate) fn export_state(&self) -> anyhow::Result<StoreFile> {
        self.database()?
            .read(|tx| load(tx.sql(), "1", &[], Some("*"), Some("*"), true))
    }
    #[cfg(test)]
    pub(crate) fn edit_all_fixture<T>(
        &self,
        mutate: impl FnOnce(&mut StoreFile) -> T,
    ) -> anyhow::Result<T> {
        self.database()?.write(|tx| {
            let mut state = load(tx.sql(), "1", &[], Some("*"), Some("*"), true)?;
            let before = state.clone();
            let result = mutate(&mut state);
            // This helper models an explicit whole fixture replacement, not a
            // production append. Keep it exact for the legacy-model oracles.
            for row in &before.approval_rule_audit {
                if !state
                    .approval_rule_audit
                    .iter()
                    .any(|r| r.audit_id == row.audit_id)
                {
                    tx.sql()
                        .execute("DELETE FROM approval_audit WHERE id=?1", [&row.audit_id])?;
                }
            }
            persist(tx, &before, &state, false)?;
            Ok(result)
        })
    }
    pub fn export_previous(&self) -> anyhow::Result<()> {
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.path,
            &self.export_state()?,
            "approval R1",
        )
    }
    pub fn prepare_rollback(&self) -> anyhow::Result<()> {
        let db = self.database()?;
        let expected = db.read(|tx| Ok(tx.head(store::spec(ID))?.seq))?;
        self.export_previous()?;
        db.write(|tx| {
            anyhow::ensure!(
                tx.head(store::spec(ID))?.seq == expected,
                "approval writers changed during rollback export; stop writers and retry"
            );
            tx.sql()
                .execute("UPDATE approval_meta SET legacy_import=1 WHERE id=1", [])?;
            Ok(())
        })
    }
    pub fn migrate(&self) -> anyhow::Result<Value> {
        self.database()?.read(|tx|Ok(json!({"store":ID,"rows":tx.sql().query_row("SELECT n FROM approval_count WHERE id=1",[],|r|r.get::<_,i64>(0))?,"seq":tx.head(store::spec(ID))?.seq})))
    }
    pub(crate) fn sql_audit(
        &self,
        cursor: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<crate::approval_rules::ApprovalRuleAudit>, Option<usize>)> {
        self.database()?.read(|tx| {
            let mut rows = read_rows(
                tx.sql(),
                "SELECT data FROM approval_audit ORDER BY rowid DESC LIMIT ? OFFSET ?",
                &[
                    SqlValue::Integer(i64::try_from(limit.saturating_add(1))?),
                    SqlValue::Integer(i64::try_from(cursor)?),
                ],
            )?;
            let more = rows.len() > limit;
            rows.truncate(limit);
            let next = more.then_some(cursor + rows.len());
            Ok((rows, next))
        })
    }
    pub fn rule_audit_page(
        &self,
        after: Option<&str>,
        budget: store::PageBudget,
    ) -> anyhow::Result<(
        Vec<crate::approval_rules::ApprovalRuleAudit>,
        Option<String>,
    )> {
        anyhow::ensure!(
            (1..=200).contains(&budget.max_rows) && budget.max_bytes > 0,
            "invalid audit page budget"
        );
        let db = self.database()?;
        let spec = store::spec(ID);
        let cursor = after
            .map(|s| store::ListCursor::decode(spec, s))
            .transpose()?;
        db.read(|tx|{
            let head=tx.head(spec)?;
            if let Some(cursor)=&cursor {cursor.check(db.instance_id(),spec,&head,&Scope::Host,2)?;}
            let before=cursor.as_ref().map(|c|c.after_key().parse::<i64>().map_err(|_|store::StoreError::Invalid("invalid approval page key".into()))).transpose()?.unwrap_or(i64::MAX);
            let mut stmt=tx.sql().prepare("SELECT rowid,length(data),data FROM approval_audit WHERE rowid<?1 ORDER BY rowid DESC LIMIT ?2")?;
            let mut rows=stmt.query(params![before,i64::try_from(budget.max_rows+1)?])?;
            let mut result=Vec::new();let mut used=0;let mut last=None;let mut more=false;
            while let Some(row)=rows.next()? {
                let len=usize::try_from(row.get::<_,i64>(1)?)?;
                if !budget.admits(result.len(),used,len){more=true;break;}
                used+=len;last=Some(row.get::<_,i64>(0)?);result.push(decode(row.get(2)?)?);
            }
            let next=if more {last.map(|key|store::ListCursor::new(db.instance_id(),spec,&head,&Scope::Host,2,key.to_string()).encode())} else {None};
            Ok((result,next))
        })
    }
}

fn due_orphans(sql: &Connection, overdue: DateTime<Utc>) -> anyhow::Result<Vec<ApprovalRecord>> {
    let budget = store::PageBudget {
        max_rows: 128,
        max_bytes: 256 * 1024,
    };
    let mut selected = Vec::new();
    let mut used = 0;
    {
        let mut query=sql.prepare("SELECT id,length(data) FROM approval_rows WHERE orphan_queued=0 AND wait_until IS NOT NULL AND wait_until<?1 AND state IN ('approved','denied') ORDER BY wait_until LIMIT 128")?;
        let mut rows = query.query([overdue.timestamp_millis()])?;
        while let Some(row) = rows.next()? {
            let bytes = usize::try_from(row.get::<_, i64>(1)?)?;
            if !budget.admits(selected.len(), used, bytes) {
                break;
            }
            used += bytes;
            selected.push(row.get::<_, String>(0)?);
        }
    }
    let mut result = Vec::new();
    for id in selected {
        let body = sql.query_row("SELECT data FROM approval_rows WHERE id=?1", [id], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        result.push(decode(body)?);
    }
    Ok(result)
}

fn claim_orphan_page(db: &StoreDb) -> anyhow::Result<Vec<(i64, ApprovalRecord)>> {
    db.write(|tx| {
        let now=Utc::now().timestamp_millis();let budget=store::PageBudget{max_rows:128,max_bytes:256*1024};
        let mut selected=Vec::new();let mut used=0;
        {
            let mut query=tx.sql().prepare("SELECT id,length(payload) FROM store_outbox WHERE topic='approvals.orphan' AND (claimed_until IS NULL OR claimed_until<?1) ORDER BY id LIMIT 128")?;
            let mut rows=query.query([now])?;
            while let Some(row)=rows.next()? {
                let bytes=usize::try_from(row.get::<_,i64>(1)?)?;
                if !budget.admits(selected.len(),used,bytes){break;}
                used+=bytes;selected.push(row.get::<_,i64>(0)?);
            }
        }
        let mut result=Vec::new();
        for id in selected {
            let payload=tx.sql().query_row("SELECT payload FROM store_outbox WHERE id=?1",[id],|row|row.get::<_,Vec<u8>>(0))?;
            result.push((id,decode(payload)?));
            tx.sql().execute("UPDATE store_outbox SET claimed_until=?1 WHERE id=?2",params![now+30_000,id])?;
        }
        Ok(result)
    })
}

fn existing_database(path: &Path) -> anyhow::Result<Option<Connection>> {
    let file = path.with_extension("sqlite3");
    if !file.try_exists()? {
        return Ok(None);
    }
    let db = Connection::open_with_flags(
        file,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(Some(db))
}
fn existing_header(sql: &Connection) -> anyhow::Result<bool> {
    let version: i64 = sql.query_row(
        "SELECT schema_version FROM store_head WHERE store=?1",
        [ID],
        |r| r.get(0),
    )?;
    anyhow::ensure!(version == 1, "unknown approval store schema");
    Ok(sql.query_row(
        "SELECT legacy_import FROM approval_meta WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}

fn load(
    sql: &Connection,
    predicate: &str,
    args: &[SqlValue],
    rules: Option<&str>,
    nonce: Option<&str>,
    audit: bool,
) -> anyhow::Result<StoreFile> {
    let mut state = StoreFile::default();
    state.approvals = read_rows(
        sql,
        &format!("SELECT data FROM approval_rows WHERE {predicate} ORDER BY ordinal"),
        args,
    )?;
    state.rules_revision = u64::try_from(sql.query_row(
        "SELECT rules_revision FROM approval_meta WHERE id=1",
        [],
        |r| r.get::<_, i64>(0),
    )?)?;
    store::metrics::note(
        ID,
        OpCounts {
            sql_statements: 1,
            bytes_read: 8,
            ..Default::default()
        },
    );
    if !audit {
        state.rule_count = usize::try_from(sql.query_row(
            "SELECT n FROM approval_rule_count WHERE id=1",
            [],
            |r| r.get::<_, i64>(0),
        )?)?;
        store::metrics::note(
            ID,
            OpCounts {
                sql_statements: 1,
                bytes_read: 8,
                ..Default::default()
            },
        );
    }
    if let Some(subject) = rules {
        state.approval_rules = if subject == "*" {
            read_rows(sql, "SELECT data FROM approval_rules ORDER BY rowid", &[])?
        } else if let Some(id) = subject.strip_prefix("id:") {
            read_rows(
                sql,
                "SELECT data FROM approval_rules WHERE id=?",
                &[SqlValue::Text(id.into())],
            )?
        } else if subject.starts_with("override:") {
            Vec::new()
        } else {
            read_rows(
                sql,
                "SELECT data FROM approval_rules WHERE subject=? ORDER BY rowid",
                &[SqlValue::Text(subject.into())],
            )?
        };
        if subject == "*" && audit {
            let mut stmt = sql.prepare("SELECT id,data FROM approval_overrides ORDER BY id")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                state
                    .director_rule_overrides
                    .insert(row.get(0)?, decode(row.get(1)?)?);
            }
        } else if subject != "*" && !subject.starts_with("id:") {
            read_overrides(
                sql,
                &[subject.strip_prefix("override:").unwrap_or(subject)],
                &mut state,
            )?;
        }
    }
    if let Some(token) = nonce {
        state.approval_rule_confirmations = if token == "*" {
            read_rows(sql, "SELECT data FROM approval_nonces ORDER BY rowid", &[])?
        } else {
            read_rows(
                sql,
                "SELECT data FROM approval_nonces WHERE id=?",
                &[SqlValue::Text(token.into())],
            )?
        };
    }
    if audit {
        state.approval_rule_audit =
            read_rows(sql, "SELECT data FROM approval_audit ORDER BY rowid", &[])?;
    }
    Ok(state)
}

fn read_overrides(
    sql: &Connection,
    subjects: &[&str],
    state: &mut StoreFile,
) -> anyhow::Result<()> {
    let mut stmt = sql.prepare("SELECT data FROM approval_overrides WHERE id=?1")?;
    for subject in subjects {
        if let Some(bytes) = stmt
            .query_row([subject], |r| r.get::<_, Vec<u8>>(0))
            .optional()?
        {
            state
                .director_rule_overrides
                .insert((*subject).into(), decode(bytes)?);
        }
    }
    Ok(())
}

/// Preserve the previous TTL, seven-day retention and terminal-row cap using
/// indexed metadata. Only rows whose business state actually expires decode.
/// The importer deliberately never invokes maintenance.
fn prune_metadata(tx: &mut WriteTx<'_>, now: DateTime<Utc>) -> anyhow::Result<()> {
    loop {
        let due: Vec<ApprovalRecord> = read_rows(
            tx.sql(),
            "SELECT data FROM approval_rows WHERE expires_ms<?1 ORDER BY expires_ms,id LIMIT 64",
            &[SqlValue::Integer(now.timestamp_millis())],
        )?;
        if due.is_empty() {
            break;
        }
        for mut record in due {
            record.state = ApprovalState::Expired;
            approval_put(tx, &record, false)?;
        }
    }
    loop {
        let ids = {
            let mut stmt=tx.sql().prepare("SELECT id FROM approval_rows WHERE state!='pending' AND created_ms<?1 ORDER BY created_ms,id LIMIT 128")?;
            let rows = stmt.query_map(
                [(now - Duration::days(RETENTION_DAYS)).timestamp_millis()],
                |r| r.get::<_, String>(0),
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if ids.is_empty() {
            break;
        }
        for id in ids {
            tx.sql()
                .execute("DELETE FROM approval_rows WHERE id=?1", [&id])?;
            change(tx, &id, ChangeKind::Delete)?;
        }
    }
    let count: i64 = tx
        .sql()
        .query_row("SELECT n FROM approval_count WHERE id=1", [], |r| r.get(0))?;
    let mut excess = count.saturating_sub(MAX_RECORDS as i64);
    while excess > 0 {
        let ids = {
            let mut stmt=tx.sql().prepare("SELECT id FROM approval_rows WHERE state IN ('consumed','expired') ORDER BY ordinal LIMIT ?1")?;
            let rows = stmt.query_map([excess.min(128)], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if ids.is_empty() {
            break;
        }
        excess -= ids.len() as i64;
        for id in ids {
            tx.sql()
                .execute("DELETE FROM approval_rows WHERE id=?1", [&id])?;
            change(tx, &id, ChangeKind::Delete)?;
        }
    }
    Ok(())
}

fn persist(
    tx: &mut WriteTx<'_>,
    before: &StoreFile,
    after: &StoreFile,
    maintain_audit: bool,
) -> anyhow::Result<()> {
    for record in &after.approvals {
        if before
            .approvals
            .iter()
            .find(|r| r.approval_id == record.approval_id)
            != Some(record)
        {
            approval_put(tx, record, false)?;
        }
    }
    for record in &before.approvals {
        if !after
            .approvals
            .iter()
            .any(|r| r.approval_id == record.approval_id)
        {
            tx.sql().execute(
                "DELETE FROM approval_rows WHERE id=?1",
                [&record.approval_id],
            )?;
            change(tx, &record.approval_id, ChangeKind::Delete)?;
        }
    }
    for rule in &after.approval_rules {
        if before
            .approval_rules
            .iter()
            .find(|r| r.rule_id == rule.rule_id)
            != Some(rule)
        {
            rule_put(tx, rule)?;
        }
    }
    for rule in &before.approval_rules {
        if !after
            .approval_rules
            .iter()
            .any(|r| r.rule_id == rule.rule_id)
        {
            tx.sql()
                .execute("DELETE FROM approval_rules WHERE id=?1", [&rule.rule_id])?;
            change(tx, &format!("rule:{}", rule.rule_id), ChangeKind::Delete)?;
        }
    }
    let mut appended = false;
    for audit in &after.approval_rule_audit {
        if !before
            .approval_rule_audit
            .iter()
            .any(|r| r.audit_id == audit.audit_id)
        {
            generic_put(tx, "approval_audit", &audit.audit_id, audit)?;
            appended = true;
        }
    }
    if appended && maintain_audit {
        prune_audit(tx)?;
    }
    for nonce in &after.approval_rule_confirmations {
        if !before.approval_rule_confirmations.contains(nonce) {
            nonce_put(tx, nonce)?;
        }
    }
    for (id, value) in &after.director_rule_overrides {
        if before.director_rule_overrides.get(id) != Some(value) {
            generic_put(tx, "approval_overrides", id, value)?;
        }
    }
    if before.rules_revision != after.rules_revision {
        tx.sql().execute(
            "UPDATE approval_meta SET rules_revision=?1 WHERE id=1",
            [i64::try_from(after.rules_revision)?],
        )?;
    }
    Ok(())
}

fn prune_audit(tx: &mut WriteTx<'_>) -> anyhow::Result<()> {
    let count: i64 =
        tx.sql()
            .query_row("SELECT n FROM approval_audit_count WHERE id=1", [], |r| {
                r.get(0)
            })?;
    let mut excess = count.saturating_sub(2000);
    while excess > 0 {
        let ids = {
            let mut stmt = tx
                .sql()
                .prepare("SELECT id FROM approval_audit ORDER BY rowid LIMIT ?1")?;
            let rows = stmt.query_map([excess.min(128)], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if ids.is_empty() {
            break;
        }
        excess -= ids.len() as i64;
        for id in ids {
            tx.sql()
                .execute("DELETE FROM approval_audit WHERE id=?1", [&id])?;
            change(tx, &format!("approval_audit:{id}"), ChangeKind::Delete)?;
        }
    }
    Ok(())
}
