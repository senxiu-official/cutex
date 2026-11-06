//! S10 repository. Whole-state values exist only for migration/export and pure
//! business transitions over the rows selected by an indexed predicate.
use super::store::*;
use crate::store::{self, ChangeKind, OpCounts, Scope, StoreDb, WriteTx};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, types::Value as SqlValue, Connection, OptionalExtension};
use std::sync::Arc;

const ID: &str = "owner-notify";
const TOPIC: &str = "owner-notify.notice";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notifications (
 id TEXT PRIMARY KEY, agent TEXT NOT NULL, origin TEXT NOT NULL, created TEXT NOT NULL, created_ms INTEGER,
 pending INTEGER NOT NULL, alerting INTEGER NOT NULL, state TEXT NOT NULL,
 safety_id TEXT, safety_open INTEGER NOT NULL, replied INTEGER NOT NULL,
 final_at TEXT, final_ms INTEGER, final_sub INTEGER, is_final INTEGER NOT NULL, data BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS notifications_time ON notifications(created DESC,id DESC);
CREATE INDEX IF NOT EXISTS notifications_agent_time ON notifications(agent,created DESC,id DESC);
CREATE INDEX IF NOT EXISTS notifications_agent_recent ON notifications(agent,origin,created_ms);
CREATE INDEX IF NOT EXISTS notifications_pending_time ON notifications(pending,created DESC,id DESC);
CREATE INDEX IF NOT EXISTS notifications_alerting_time ON notifications(alerting,created DESC,id DESC);
CREATE INDEX IF NOT EXISTS notifications_agent_state ON notifications(agent,state,id);
CREATE INDEX IF NOT EXISTS notifications_agent_pending ON notifications(agent,pending,created DESC,id DESC);
CREATE INDEX IF NOT EXISTS notifications_agent_alerting ON notifications(agent,alerting,created DESC,id DESC);
CREATE INDEX IF NOT EXISTS notifications_safety ON notifications(agent,safety_id);
CREATE INDEX IF NOT EXISTS notifications_safety_time ON notifications(safety_open,created DESC,id DESC) WHERE safety_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS notifications_safety_all ON notifications(created DESC,id DESC) WHERE safety_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS notifications_agent_safety ON notifications(agent,safety_open,created DESC,id DESC) WHERE safety_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS notifications_agent_safety_all ON notifications(agent,created DESC,id DESC) WHERE safety_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS notifications_replies ON notifications(replied,final_at,id);
CREATE INDEX IF NOT EXISTS notifications_retention ON notifications(replied,is_final,final_ms,final_sub,id);
CREATE INDEX IF NOT EXISTS notifications_cap ON notifications(replied,is_final DESC,created,id);
CREATE TABLE IF NOT EXISTS notification_actions (id TEXT PRIMARY KEY, at TEXT NOT NULL, at_ms INTEGER, at_sub INTEGER, data BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS notification_actions_time ON notification_actions(at_ms,at_sub,id);
CREATE TABLE IF NOT EXISTS notification_meta (id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL, legacy_import INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS notification_count (id INTEGER PRIMARY KEY CHECK(id=1), n INTEGER NOT NULL);
INSERT OR IGNORE INTO notification_count(id,n) VALUES (1,0);
CREATE TRIGGER IF NOT EXISTS notifications_insert_count AFTER INSERT ON notifications BEGIN UPDATE notification_count SET n=n+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS notifications_delete_count AFTER DELETE ON notifications BEGIN UPDATE notification_count SET n=n-1 WHERE id=1; END;
CREATE TABLE IF NOT EXISTS notification_outbox_count(id INTEGER PRIMARY KEY CHECK(id=1),n INTEGER NOT NULL);
INSERT OR IGNORE INTO notification_outbox_count(id,n) VALUES (1,0);
CREATE TRIGGER IF NOT EXISTS notice_insert_count AFTER INSERT ON store_outbox WHEN NEW.topic='owner-notify.notice' BEGIN UPDATE notification_outbox_count SET n=n+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS notice_delete_count AFTER DELETE ON store_outbox WHEN OLD.topic='owner-notify.notice' BEGIN UPDATE notification_outbox_count SET n=n-1 WHERE id=1; END;
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

fn selected(
    sql: &Connection,
    predicate: &str,
    args: &[SqlValue],
) -> anyhow::Result<OwnerNotifyState> {
    note_query();
    let mut state = OwnerNotifyState::default();
    let query = format!("SELECT data FROM notifications WHERE {predicate}");
    let mut statement = sql.prepare(&query)?;
    let mut rows = statement.query(rusqlite::params_from_iter(args))?;
    while let Some(row) = rows.next()? {
        let record: OwnerNotification = decode(row.get(0)?)?;
        state
            .notifications
            .insert(record.notification_id.clone(), record);
    }
    Ok(state)
}

fn put(tx: &mut WriteTx<'_>, record: &OwnerNotification) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(record)?;
    store::metrics::note(
        ID,
        OpCounts {
            bytes_encoded: bytes.len() as u64,
            bytes_written: bytes.len() as u64,
            ..Default::default()
        },
    );
    tx.sql().execute("INSERT INTO notifications(id,agent,created,pending,alerting,state,safety_id,safety_open,replied,final_at,data,created_ms,final_ms,is_final,origin,final_sub)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
        ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,created=excluded.created,pending=excluded.pending,
        alerting=excluded.alerting,state=excluded.state,safety_id=excluded.safety_id,safety_open=excluded.safety_open,
        replied=excluded.replied,final_at=excluded.final_at,data=excluded.data,created_ms=excluded.created_ms,
        final_ms=excluded.final_ms,is_final=excluded.is_final,origin=excluded.origin,final_sub=excluded.final_sub",
        params![record.notification_id, record.agent_id, record.created_at, record.pending(), record.alerting(),
            serde_json::to_string(&record.state)?, record.safety_block.as_ref().map(|b| b.block_id.as_str()),
            record.safety_block.as_ref().is_some_and(SafetyBlock::is_open), record.has_owner_reply(),
            record.final_at().map(|at| at.to_rfc3339()), bytes,
            crate::asks::parse_time(&record.created_at).map(|at| at.timestamp_millis()),
            record.final_at().map(|at| at.timestamp_millis()),record.is_final(),
            serde_json::to_value(record.origin)?.as_str().unwrap_or("agent"),
            record.final_at().map(|at|i64::from(at.timestamp_subsec_nanos()%1_000_000))])?;
    let spec = store::spec(ID);
    let revision = tx.head(spec)?.seq + 1;
    tx.record_change(
        spec,
        &Scope::Host,
        &record.notification_id,
        revision,
        ChangeKind::Put,
        None,
    )?;
    Ok(())
}

fn enqueue(tx: &mut WriteTx<'_>, notice: &Notice) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(notice)?;
    store::metrics::note(
        ID,
        OpCounts {
            bytes_encoded: bytes.len() as u64,
            bytes_written: bytes.len() as u64,
            ..Default::default()
        },
    );
    tx.outbox(
        store::spec(ID),
        &store::OutboxItem {
            topic: TOPIC,
            idem_key: uuid::Uuid::new_v4().to_string(),
            payload: bytes,
        },
    )?;
    Ok(())
}

/// Retention examines indexed metadata and encodes no surviving body. Replies
/// remain until the documents archive verifies them (the previous policy).
fn prune(tx: &mut WriteTx<'_>, now: DateTime<Utc>) -> anyhow::Result<()> {
    let cutoff = now - Duration::days(RETENTION_DAYS);
    loop {
        let mut stmt = tx.sql().prepare(
            "SELECT id FROM notifications WHERE replied=0 AND is_final=1 AND (final_ms,final_sub)<=(?1,?2) LIMIT 128",
        )?;
        let rows = stmt.query_map(
            [
                cutoff.timestamp_millis(),
                i64::from(cutoff.timestamp_subsec_nanos() % 1_000_000),
            ],
            |r| r.get::<_, String>(0),
        )?;
        let ids = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        if ids.is_empty() {
            break;
        }
        for id in ids {
            delete_row(tx, &id)?;
        }
    }
    let count: i64 =
        tx.sql()
            .query_row("SELECT n FROM notification_count WHERE id=1", [], |r| {
                r.get(0)
            })?;
    if count > MAX_RECORDS as i64 {
        let ids = {
            let mut stmt = tx.sql().prepare("SELECT id FROM notifications WHERE replied=0 ORDER BY is_final DESC,created,id LIMIT ?1")?;
            let rows = stmt.query_map([count - MAX_RECORDS as i64], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in ids {
            delete_row(tx, &id)?;
        }
    }
    let cutoff = now - Duration::days(ACTION_RETENTION_DAYS);
    tx.sql().execute(
        "DELETE FROM notification_actions WHERE (at_ms,at_sub)<=(?1,?2)",
        [
            cutoff.timestamp_millis(),
            i64::from(cutoff.timestamp_subsec_nanos() % 1_000_000),
        ],
    )?;
    Ok(())
}

fn trim_notices(tx: &mut WriteTx<'_>) -> anyhow::Result<()> {
    let count: i64 = tx.sql().query_row(
        "SELECT n FROM notification_outbox_count WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    if count > 2000 {
        tx.sql().execute("DELETE FROM store_outbox WHERE id IN (SELECT id FROM store_outbox WHERE topic=?1 ORDER BY id LIMIT ?2)",params![TOPIC,count-2000])?;
    }
    Ok(())
}

fn delete_row(tx: &mut WriteTx<'_>, id: &str) -> anyhow::Result<()> {
    tx.sql()
        .execute("DELETE FROM notifications WHERE id=?1", [id])?;
    let revision = tx.head(store::spec(ID))?.seq + 1;
    tx.record_change(
        store::spec(ID),
        &Scope::Host,
        id,
        revision,
        ChangeKind::Delete,
        None,
    )?;
    Ok(())
}

impl OwnerNotifyStore {
    pub(crate) fn database(&self) -> anyhow::Result<Arc<StoreDb>> {
        if let Some(db) = self.db.get() {
            return Ok(db.clone());
        }
        crate::management::client_tokens::ensure_private_dir(&self.dir)?;
        let db = StoreDb::open_at(
            &store::domain::OWNER_NOTIFY,
            &self.dir.join("owner-notify.sqlite3"),
        )?;
        db.register(store::spec(ID))?;
        // The import marker is committed with every imported row. A crash never
        // makes a partial import authoritative. Never consult stale JSON again.
        db.write(|tx| {
            tx.sql().execute_batch(SCHEMA)?;
            let marker: Option<bool> = tx
                .sql()
                .query_row(
                    "SELECT legacy_import FROM notification_meta WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if marker != Some(false) {
                if marker == Some(true) {
                    anyhow::ensure!(
                        self.state_path().is_file(),
                        "rollback notification JSON is missing; leave SQL untouched"
                    );
                }
                let state = Self::read_state(&self.state_path())?;
                if marker == Some(true) {
                    tx.reset_epoch(store::spec(ID))?;
                    tx.sql().execute("DELETE FROM notifications", [])?;
                    tx.sql().execute("DELETE FROM notification_actions", [])?;
                    tx.sql()
                        .execute("DELETE FROM store_outbox WHERE topic=?1", [TOPIC])?;
                }
                for record in state.notifications.values() {
                    put(tx, record)?;
                }
                for (key, action) in &state.actions {
                    tx.sql().execute(
                        "INSERT INTO notification_actions(id,at,data,at_ms,at_sub) VALUES (?1,?2,?3,?4,?5)",
                        params![
                            key,
                            action.at,
                            serde_json::to_vec(action)?,
                            crate::asks::parse_time(&action.at).map(|at| at.timestamp_millis()),
                            crate::asks::parse_time(&action.at).map(|at|i64::from(at.timestamp_subsec_nanos()%1_000_000))
                        ],
                    )?;
                }
                for notice in &state.notices {
                    enqueue(tx, notice)?;
                }
                tx.sql().execute(
                    "INSERT INTO notification_meta(id,revision,legacy_import) VALUES (1,?1,0)
                    ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,legacy_import=0",
                    [i64::try_from(state.revision)?],
                )?;
                if marker.is_none() {
                    tx.record_migration(store::spec(ID), 1, None)?;
                }
            }
            Ok(())
        })?;
        let _ = self.db.set(db.clone());
        Ok(db)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<OwnerNotification>> {
        self.database()?.read(|tx| {
            note_query();
            tx.sql()
                .query_row("SELECT data FROM notifications WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
                .map(decode)
                .transpose()
        })
    }

    pub fn list(
        &self,
        filter: &str,
        agent: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<OwnerNotification>> {
        let predicate = match filter {
            "pending" => "pending=1",
            "alerting" => "alerting=1",
            _ => "1",
        };
        self.query_page(predicate, agent, limit)
    }

    pub fn safety_list(
        &self,
        filter: &str,
        agent: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<OwnerNotification>> {
        self.query_page(
            if filter == "all" {
                "safety_id IS NOT NULL"
            } else {
                "safety_id IS NOT NULL AND safety_open=1"
            },
            agent,
            limit,
        )
    }

    fn query_page(
        &self,
        predicate: &str,
        agent: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<OwnerNotification>> {
        self.database()?.read(|tx| {
            note_query();
            let mut args = Vec::new();
            let mut query = format!("SELECT data FROM notifications WHERE {predicate}");
            if let Some(agent) = agent {
                query.push_str(" AND agent=?");
                args.push(SqlValue::Text(agent.into()));
            }
            query.push_str(" ORDER BY created DESC,id DESC LIMIT ?");
            args.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
            let mut stmt = tx.sql().prepare(&query)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
            let mut result = Vec::new();
            while let Some(row) = rows.next()? {
                result.push(decode(row.get(0)?)?);
            }
            Ok(result)
        })
    }

    /// Bounded public page. The legacy internal list remains available to
    /// reducers that explicitly need every matching pending record.
    pub fn notification_page(
        &self,
        filter: &str,
        agent: Option<&str>,
        safety: bool,
        after: Option<&str>,
        budget: store::PageBudget,
    ) -> anyhow::Result<(Vec<OwnerNotification>, Option<String>)> {
        anyhow::ensure!(
            budget.max_rows > 0 && budget.max_rows <= 500 && budget.max_bytes > 0,
            "invalid notification page budget"
        );
        let predicate = match (safety, filter) {
            (true, "all") => "safety_id IS NOT NULL",
            (true, _) => "safety_id IS NOT NULL AND safety_open=1",
            (false, "pending") => "pending=1",
            (false, "alerting") => "alerting=1",
            _ => "1",
        };
        let filter_id = match (safety, filter) {
            (true, "all") => 4,
            (true, _) => 5,
            (false, "pending") => 1,
            (false, "alerting") => 2,
            _ => 3,
        };
        let scope = agent
            .map(|id| Scope::Session(id.into()))
            .unwrap_or(Scope::Host);
        let spec = store::spec(ID);
        let cursor = after
            .map(|text| store::ListCursor::decode(spec, text))
            .transpose()?;
        let db = self.database()?;
        db.read(|tx| {
            let head = tx.head(spec)?;
            if let Some(cursor) = &cursor {
                cursor.check(db.instance_id(), spec, &head, &scope, filter_id)?;
            }
            let mut args = Vec::new();
            let mut query =
                format!("SELECT created,id,length(data),data FROM notifications WHERE {predicate}");
            if let Some(agent) = agent {
                query.push_str(" AND agent=?");
                args.push(SqlValue::Text(agent.into()));
            }
            if let Some(cursor) = &cursor {
                let (created, id): (String, String) = serde_json::from_str(cursor.after_key())
                    .map_err(|_| {
                        store::StoreError::Invalid("invalid notification page key".into())
                    })?;
                query.push_str(" AND (created,id)<(?,?)");
                args.extend([SqlValue::Text(created), SqlValue::Text(id)]);
            }
            query.push_str(" ORDER BY created DESC,id DESC LIMIT ?");
            args.push(SqlValue::Integer(i64::try_from(budget.max_rows + 1)?));
            note_query();
            let mut statement = tx.sql().prepare(&query)?;
            let mut rows = statement.query(rusqlite::params_from_iter(args))?;
            let mut result = Vec::new();
            let mut used = 0;
            let mut last = None;
            let mut more = false;
            while let Some(row) = rows.next()? {
                let bytes = usize::try_from(row.get::<_, i64>(2)?)?;
                if !budget.admits(result.len(), used, bytes) {
                    more = true;
                    break;
                }
                last = Some((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
                used += bytes;
                result.push(decode(row.get(3)?)?);
            }
            let next = if more {
                last.map(|key| {
                    serde_json::to_string(&key).map(|key| {
                        store::ListCursor::new(
                            db.instance_id(),
                            spec,
                            &head,
                            &scope,
                            filter_id,
                            key,
                        )
                        .encode()
                    })
                })
                .transpose()?
            } else {
                None
            };
            Ok((result, next))
        })
    }

    /// Transitions see only explicitly selected rows; diffs encode changed rows.
    pub(crate) fn edit_selected<T>(
        &self,
        predicate: &str,
        args: &[SqlValue],
        action: Option<&str>,
        mutate: impl FnOnce(&mut OwnerNotifyState) -> T,
    ) -> anyhow::Result<T> {
        self.edit_selected_inner(predicate, args, action, None, mutate)
    }

    fn edit_selected_inner<T>(
        &self,
        predicate: &str,
        args: &[SqlValue],
        action: Option<&str>,
        maintenance: Option<DateTime<Utc>>,
        mutate: impl FnOnce(&mut OwnerNotifyState) -> T,
    ) -> anyhow::Result<T> {
        self.database()?.write(|tx| {
            // A replay/conflict needs only its immutable receipt, never the
            // sender's recent notification bodies used for a fresh rate check.
            let mut replay_action=None;
            if let Some(key) = action {
                note_query();
                let bytes: Option<Vec<u8>> = tx.sql().query_row("SELECT data FROM notification_actions WHERE id=?1", [key], |r| r.get(0)).optional()?;
                if let Some(bytes) = bytes { replay_action=Some((key.to_string(),decode(bytes)?)); }
            }
            let mut state=if replay_action.is_some(){OwnerNotifyState::default()}else{selected(tx.sql(),predicate,args)?};
            state.selected_rows=true;
            if let Some((key,record))=replay_action {state.actions.insert(key,record);}
            let before = state.clone(); // only selected rows, never the store
            let result = mutate(&mut state);
            let mut changed = false;
            for (id, record) in &state.notifications {
                if before.notifications.get(id) != Some(record) { put(tx, record)?; changed = true; }
            }
            for id in before.notifications.keys().filter(|id| !state.notifications.contains_key(*id)) {
                tx.sql().execute("DELETE FROM notifications WHERE id=?1", [id])?;
                let revision = tx.head(store::spec(ID))?.seq + 1;
                tx.record_change(store::spec(ID), &Scope::Host, id, revision, ChangeKind::Delete, None)?;
                changed = true;
            }
            for (id, record) in &state.actions {
                if before.actions.get(id) != Some(record) {
                    let bytes = serde_json::to_vec(record)?;
                    store::metrics::note(ID, OpCounts { bytes_encoded: bytes.len() as u64, bytes_written: bytes.len() as u64, ..Default::default() });
                    tx.sql().execute("INSERT INTO notification_actions(id,at,data,at_ms,at_sub) VALUES (?1,?2,?3,?4,?5)
                        ON CONFLICT(id) DO UPDATE SET at=excluded.at,data=excluded.data,at_ms=excluded.at_ms,at_sub=excluded.at_sub", params![id,record.at,bytes,crate::asks::parse_time(&record.at).map(|at| at.timestamp_millis()),crate::asks::parse_time(&record.at).map(|at|i64::from(at.timestamp_subsec_nanos()%1_000_000))])?;
                    changed = true;
                }
            }
            for notice in &state.notices { enqueue(tx, notice)?; changed = true; }
            if !state.notices.is_empty(){trim_notices(tx)?;}
            if let Some(now) = maintenance.filter(|_| !state.notices.is_empty()) {
                prune(tx, now)?;
            }
            if changed { tx.sql().execute("UPDATE notification_meta SET revision=revision+1 WHERE id=1", [])?; }
            Ok(result)
        })
    }

    pub fn edit_record<T>(
        &self,
        id: &str,
        mutate: impl FnOnce(&mut OwnerNotifyState) -> T,
    ) -> anyhow::Result<T> {
        self.edit_selected("id=?", &[SqlValue::Text(id.into())], None, mutate)
    }

    pub fn edit_agent_open<T>(
        &self,
        agent: &str,
        mutate: impl FnOnce(&mut OwnerNotifyState) -> T,
    ) -> anyhow::Result<T> {
        self.edit_selected(
            "agent=? AND state='\"open\"'",
            &[SqlValue::Text(agent.into())],
            None,
            mutate,
        )
    }

    pub fn edit_safety<T>(
        &self,
        agent: &str,
        blocks: &[String],
        mutate: impl FnOnce(&mut OwnerNotifyState) -> T,
    ) -> anyhow::Result<T> {
        let predicate = format!(
            "agent=? AND safety_id IN ({})",
            vec!["?"; blocks.len()].join(",")
        );
        let mut args = vec![SqlValue::Text(agent.into())];
        args.extend(blocks.iter().cloned().map(SqlValue::Text));
        self.edit_selected(&predicate, &args, None, mutate)
    }

    pub fn create_action<T>(
        &self,
        agent: &str,
        action: Option<&str>,
        now: DateTime<Utc>,
        mutate: impl FnOnce(&mut OwnerNotifyState) -> T,
    ) -> anyhow::Result<T> {
        let key = action.map(|id| OwnerNotifyState::action_key(agent, id));
        self.edit_selected_inner(
            "agent=? AND origin='agent' AND created_ms>=?",
            &[
                SqlValue::Text(agent.into()),
                SqlValue::Integer((now - Duration::hours(1)).timestamp_millis()),
            ],
            key.as_deref(),
            Some(now),
            mutate,
        )
    }
    pub fn create_system(
        &self,
        new: NewNotification,
        kind: Option<&str>,
        settings: &crate::config::owner_notify::OwnerNotifySettings,
        evaluator: &crate::notification_levels::Evaluator,
        now: DateTime<Utc>,
    ) -> anyhow::Result<OwnerNotification> {
        self.edit_selected_inner("0", &[], None, Some(now), |state| {
            state.create_decided(new, Origin::System, kind, settings, evaluator, None, now)
        })
    }

    pub fn claim_notices(&self, limit: usize) -> anyhow::Result<Vec<(i64, String, Notice)>> {
        let db = self.database()?;
        // An empty scheduler tick performs a bounded read and no encode/write.
        let pending = db.read(|tx| {
            note_query();
            Ok(tx.sql().query_row(
                "SELECT EXISTS(SELECT 1 FROM store_outbox WHERE topic=?1)",
                [TOPIC],
                |r| r.get::<_, bool>(0),
            )?)
        })?;
        if !pending {
            return Ok(Vec::new());
        }
        db.write(|tx|{
            let now=Utc::now().timestamp_millis();
            let first:Option<Option<i64>>=tx.sql().query_row("SELECT claimed_until FROM store_outbox WHERE topic=?1 ORDER BY id LIMIT 1",[TOPIC],|r|r.get(0)).optional()?;
            if first.flatten().is_some_and(|until|until>=now){return Ok(Vec::new());}
            let mut selected=Vec::new();let mut used=0;
            let budget=store::PageBudget{max_rows:limit.min(100),max_bytes:256*1024};
            {
                let mut stmt=tx.sql().prepare("SELECT id,idem_key,claimed_until,length(payload) FROM store_outbox WHERE topic=?1 ORDER BY id LIMIT ?2")?;
                let mut rows=stmt.query(params![TOPIC,i64::try_from(limit.min(100))?])?;
                while let Some(row)=rows.next()? {
                    if row.get::<_,Option<i64>>(2)?.is_some_and(|until|until>=now){break;}
                    let bytes=usize::try_from(row.get::<_,i64>(3)?)?;
                    if !budget.admits(selected.len(),used,bytes){break;}
                    used+=bytes;selected.push((row.get::<_,i64>(0)?,row.get::<_,String>(1)?));
                }
            }
            let mut result=Vec::new();
            for (id,key) in selected {
                let payload=tx.sql().query_row("SELECT payload FROM store_outbox WHERE id=?1",[id],|r|r.get::<_,Vec<u8>>(0))?;
                result.push((id,key,decode(payload)?));
                tx.sql().execute("UPDATE store_outbox SET claimed_until=?1 WHERE id=?2",params![now+30_000,id])?;
            }
            Ok(result)
        })
    }
    pub fn ack_notice(&self, id: i64) -> anyhow::Result<()> {
        store::outbox::ack(self.database()?.as_ref(), &[id])
    }
    pub fn release_notice(&self, id: i64) -> anyhow::Result<()> {
        store::outbox::release(self.database()?.as_ref(), &[id])
    }
    pub fn release_notices(&self, ids: &[i64]) -> anyhow::Result<()> {
        store::outbox::release(self.database()?.as_ref(), ids)
    }

    pub fn safety_record(
        &self,
        agent: &str,
        block: &str,
    ) -> anyhow::Result<Option<OwnerNotification>> {
        self.database()?.read(|tx| {
            tx.sql()
                .query_row(
                    "SELECT data FROM notifications WHERE agent=?1 AND safety_id=?2 LIMIT 1",
                    params![agent, block],
                    |r| r.get(0),
                )
                .optional()?
                .map(decode)
                .transpose()
        })
    }

    pub fn check_reply(
        &self,
        agent: &str,
        id: &str,
    ) -> anyhow::Result<Result<OwnerNotification, OwnerError>> {
        let mut state = OwnerNotifyState::default();
        if let Some(record) = self.get(id)? {
            state.notifications.insert(id.into(), record);
        }
        Ok(state.check_reply(agent, id))
    }

    pub fn visit_replied_final_before(
        &self,
        cutoff: DateTime<Utc>,
        mut visit: impl FnMut(OwnerNotification) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let db = self.database()?;
        let mut after = (i64::MIN, 0i64, String::new());
        loop {
            let (records, next) = db.read(|tx| {
                let mut stmt = tx.sql().prepare(
                    "SELECT final_ms,final_sub,id,length(data),data FROM notifications
                    WHERE replied=1 AND is_final=1 AND (final_ms,final_sub)<(?1,?2) AND (final_ms,final_sub,id)>(?3,?4,?5)
                    ORDER BY final_ms,final_sub,id LIMIT 65",
                )?;
                let mut rows = stmt.query(params![cutoff.timestamp_millis(),i64::from(cutoff.timestamp_subsec_nanos()%1_000_000),after.0,after.1,after.2])?;
                let budget = store::PageBudget {
                    max_rows: 64,
                    max_bytes: 256 * 1024,
                };
                let mut page = Vec::new();
                let mut used = 0;
                let mut next = None;
                while let Some(row) = rows.next()? {
                    let len = usize::try_from(row.get::<_, i64>(3)?)?;
                    if !budget.admits(page.len(), used, len) {
                        break;
                    }
                    used += len;
                    next = Some((row.get::<_, i64>(0)?,row.get::<_,i64>(1)?,row.get::<_, String>(2)?));
                    page.push(decode(row.get(4)?)?);
                }
                Ok((page, next))
            })?;
            let Some(next) = next else { return Ok(()) };
            for record in records {
                visit(record)?;
            }
            after = next;
        }
    }

    pub fn remove_archived(&self, archived: &[OwnerNotification]) -> anyhow::Result<usize> {
        let mut removed = 0;
        for record in archived {
            removed += self.edit_record(&record.notification_id, |state| {
                state.remove_archived(std::slice::from_ref(record))
            })?;
        }
        Ok(removed)
    }

    pub fn export_state(&self) -> anyhow::Result<OwnerNotifyState> {
        self.database()?.read(|tx| {
            let mut state = selected(tx.sql(), "1", &[])?;
            state.revision = u64::try_from(tx.sql().query_row(
                "SELECT revision FROM notification_meta WHERE id=1",
                [],
                |r| r.get::<_, i64>(0),
            )?)?;
            let mut stmt = tx
                .sql()
                .prepare("SELECT id,data FROM notification_actions ORDER BY id")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                state.actions.insert(row.get(0)?, decode(row.get(1)?)?);
            }
            let mut stmt = tx
                .sql()
                .prepare("SELECT payload FROM store_outbox WHERE topic=?1 ORDER BY id")?;
            let mut rows = stmt.query([TOPIC])?;
            while let Some(row) = rows.next()? {
                state.notices.push(decode(row.get(0)?)?);
            }
            Ok(state)
        })
    }

    /// R1: export current rows and every unacked notice to the previous JSON.
    /// The kit must stop writers before invoking this.
    pub fn export_previous(&self) -> anyhow::Result<()> {
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.state_path(),
            &self.export_state()?,
            "owner notify R1",
        )
    }

    /// Offline R1 activation: export the current previous-build JSON and arm a
    /// one-time re-import of whatever that previous binary writes afterwards.
    pub fn prepare_rollback(&self) -> anyhow::Result<()> {
        let db = self.database()?;
        let expected = db.read(|tx| Ok(tx.head(store::spec(ID))?.seq))?;
        self.export_previous()?;
        db.write(|tx| {
            anyhow::ensure!(
                tx.head(store::spec(ID))?.seq == expected,
                "notification writers changed during rollback export; stop writers and retry"
            );
            tx.sql().execute(
                "UPDATE notification_meta SET legacy_import=1 WHERE id=1",
                [],
            )?;
            Ok(())
        })
    }
    pub fn migrate(&self) -> anyhow::Result<serde_json::Value> {
        self.database()?.read(|tx|Ok(serde_json::json!({"store":ID,"rows":tx.sql().query_row("SELECT n FROM notification_count WHERE id=1",[],|r|r.get::<_,i64>(0))?,"seq":tx.head(store::spec(ID))?.seq})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::owner_notify::OwnerNotifySettings;
    use crate::owner_notify::store::{Level, NewNotification, Reply};

    fn fixture(history: usize) -> (OwnerNotifyStore, std::path::PathBuf, OwnerNotification) {
        let root =
            std::env::temp_dir().join(format!("s10-notify-{}", uuid::Uuid::new_v4().simple()));
        let store = OwnerNotifyStore::at(&root);
        let now = Utc::now();
        let mut state = OwnerNotifyState::default();
        let new = NewNotification {
            agent_id: "selected".into(),
            agent_name: None,
            text: "selected body".into(),
            level: Level::Attention,
            needs_reply: true,
            delegation: None,
            job: None,
            turn_id: None,
        };
        let record = state.create(new, &OwnerNotifySettings::default(), now);
        for i in 0..history {
            let mut old = record.clone();
            old.notification_id = format!("history-{i:08}");
            old.agent_id = "unrelated".into();
            old.created_at = crate::asks::now_rfc3339(now - Duration::days(100));
            old.state = State::Answered;
            old.reply = Some(Reply {
                text: "answered historical body".repeat(20),
                replied_at: old.created_at.clone(),
                client_user_message_id: format!("reply-{i}"),
            });
            old.closed_at = Some(old.created_at.clone());
            state.notifications.insert(old.notification_id.clone(), old);
        }
        state.remember(
            "selected",
            "old-action",
            "digest",
            &serde_json::json!({"original":true}),
            now,
        );
        state.revision = 29;
        crate::config::atomic::write_private_pretty_json_atomic(
            &store.state_path(),
            &state,
            "S10 fixture",
        )
        .unwrap();
        (store, root, record)
    }

    #[test]
    fn notification_pages_bound_decoding_and_reject_cross_filter_cursors() {
        let (store, root, _) = fixture(210);
        store.migrate().unwrap();
        let budget = store::PageBudget {
            max_rows: 7,
            max_bytes: 4096,
        };
        let mut cursor = None;
        let mut ids = std::collections::BTreeSet::new();
        loop {
            let ((page, next), counts) = store::metrics::measure(|| {
                store
                    .notification_page("all", None, false, cursor.as_deref(), budget)
                    .unwrap()
            });
            assert!(page.len() <= 7);
            assert_eq!(counts.rows_materialized, page.len() as u64);
            for record in page {
                assert!(ids.insert(record.notification_id));
            }
            if let Some(next) = next {
                assert!(store
                    .notification_page("pending", None, false, Some(&next), budget)
                    .is_err());
                assert!(store
                    .notification_page("all", Some("selected"), false, Some(&next), budget)
                    .is_err());
                assert!(store
                    .notification_page("all", None, true, Some(&next), budget)
                    .is_err());
                cursor = Some(next);
            } else {
                break;
            }
        }
        assert_eq!(ids.len(), 211);
        assert!(store
            .notification_page("all", None, false, Some("malformed"), budget)
            .is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn action_receipt_retention_matches_exact_submillisecond_boundary() {
        let (store, root, _) = fixture(0);
        let cutoff = chrono::DateTime::parse_from_rfc3339("2026-11-03T12:00:00.000500Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = cutoff + Duration::nanoseconds(100);
        store
            .with_state(|state| {
                state.remember(
                    "selected",
                    "just-after",
                    "digest",
                    &serde_json::json!({"survives":true}),
                    at,
                );
                state.remember(
                    "selected",
                    "at-cutoff",
                    "digest",
                    &serde_json::json!({"expires":true}),
                    cutoff,
                );
                // Legacy JSON accepts RFC3339 precision beyond the current
                // producer's millisecond formatter; migration must preserve it.
                state
                    .actions
                    .get_mut(&OwnerNotifyState::action_key("selected", "just-after"))
                    .unwrap()
                    .at = at.to_rfc3339();
                state
                    .actions
                    .get_mut(&OwnerNotifyState::action_key("selected", "at-cutoff"))
                    .unwrap()
                    .at = cutoff.to_rfc3339();
            })
            .unwrap();
        store
            .database()
            .unwrap()
            .write(|tx| prune(tx, cutoff + Duration::days(ACTION_RETENTION_DAYS)))
            .unwrap();
        let state = store.export_state().unwrap();
        assert!(state
            .replay("selected", "just-after", "digest")
            .unwrap()
            .is_some());
        assert!(state
            .replay("selected", "at-cutoff", "digest")
            .unwrap()
            .is_none());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn notification_writer_child() {
        let Some(root) = std::env::var_os("CUTEX_S10_NOTIFY_CHILD_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("s10-notify-"));
        let store = OwnerNotifyStore::at(root);
        let now = Utc::now();
        store
            .create_action("selected", Some("same-action"), now, |state| {
                if state
                    .replay("selected", "same-action", "same-digest")
                    .unwrap()
                    .is_some()
                {
                    return;
                }
                let record = state.create(
                    NewNotification {
                        agent_id: "selected".into(),
                        agent_name: None,
                        text: "two writers, one receipt".into(),
                        level: Level::Attention,
                        needs_reply: false,
                        delegation: None,
                        job: None,
                        turn_id: None,
                    },
                    &OwnerNotifySettings::default(),
                    now,
                );
                state.remember(
                    "selected",
                    "same-action",
                    "same-digest",
                    &serde_json::json!({"notificationId":record.notification_id}),
                    now,
                );
            })
            .unwrap();
    }

    #[test]
    fn two_process_writers_import_once_and_commit_one_idempotent_notification() {
        let (store, root, _) = fixture(0);
        let spawn = || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "owner_notify::sql::tests::notification_writer_child",
                    "--nocapture",
                ])
                .env("CUTEX_S10_NOTIFY_CHILD_ROOT", &root)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let first = spawn();
        let second = spawn();
        for child in [first, second] {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let state = store.export_state().unwrap();
        assert_eq!(state.notifications.len(), 2);
        assert_eq!(state.actions.len(), 2);
        assert_eq!(state.notices.len(), 2);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_reads_only_its_receipt_even_with_ten_times_recent_agent_history() {
        let mut observations = Vec::new();
        for history in [200, 2000] {
            let (store, root, _) = fixture(history);
            let now = Utc::now();
            store
                .with_state(|state| {
                    for record in state.notifications.values_mut() {
                        record.agent_id = "selected".into();
                        record.created_at = crate::asks::now_rfc3339(now - Duration::minutes(1));
                        if let Some(reply) = record.reply.as_mut() {
                            reply.replied_at = crate::asks::now_rfc3339(now);
                            record.closed_at = Some(reply.replied_at.clone());
                        }
                    }
                })
                .unwrap();
            let (result, counts) = store::metrics::measure(|| {
                store
                    .create_action("selected", Some("old-action"), now, |state| {
                        state
                            .replay("selected", "old-action", "digest")
                            .unwrap()
                            .unwrap()
                    })
                    .unwrap()
            });
            assert_eq!(result, serde_json::json!({"original":true}));
            assert_eq!(counts.rows_materialized, 1);
            assert_eq!(counts.bytes_encoded, 0);
            assert_eq!(counts.bytes_written, 0);
            observations.push(counts);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
        assert_eq!(observations[0], observations[1]);
    }

    #[test]
    fn reply_archive_keyset_preserves_submillisecond_cutoff_order() {
        let (store, root, record) = fixture(0);
        store
            .edit_record(&record.notification_id, |state| {
                let row = state
                    .notifications
                    .get_mut(&record.notification_id)
                    .unwrap();
                row.created_at = "2026-11-03T11:00:00Z".into();
                row.closed_at = Some("2026-11-03T12:00:00.000400Z".into());
                row.state = State::Answered;
                row.reply = Some(Reply {
                    text: "retained".into(),
                    replied_at: row.closed_at.clone().unwrap(),
                    client_user_message_id: "sub-ms".into(),
                });
            })
            .unwrap();
        let at = crate::asks::parse_time("2026-11-03T12:00:00.000400Z").unwrap();
        let mut same = Vec::new();
        store
            .visit_replied_final_before(at, |row| {
                same.push(row);
                Ok(())
            })
            .unwrap();
        assert!(same.is_empty());
        let mut later = Vec::new();
        store
            .visit_replied_final_before(at + Duration::nanoseconds(1), |row| {
                later.push(row.notification_id);
                Ok(())
            })
            .unwrap();
        assert_eq!(later, vec![record.notification_id]);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn import_and_r1_preserve_answers_actions_and_pending_notices() {
        let (store, root, record) = fixture(200);
        let legacy = OwnerNotifyStore::read_state(&store.state_path()).unwrap();
        assert_eq!(store.export_state().unwrap(), legacy);
        store
            .edit_record(&record.notification_id, |state| {
                state.acknowledge("selected", &record.notification_id, Utc::now())
            })
            .unwrap()
            .unwrap();
        let current = store.export_state().unwrap();
        assert_eq!(current.notifications.len(), 201);
        assert_eq!(current.notices.len(), 2);
        assert_eq!(current.actions, legacy.actions);
        assert_eq!(
            current.notifications["history-00000000"],
            legacy.notifications["history-00000000"]
        );
        store.export_previous().unwrap();
        assert_eq!(
            OwnerNotifyStore::read_state(&store.state_path()).unwrap(),
            current
        );
        // Stale legacy JSON cannot replace SQL after its import marker commits.
        std::fs::write(store.state_path(), b"invalid stale JSON").unwrap();
        assert_eq!(OwnerNotifyStore::at(&root).export_state().unwrap(), current);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn warm_get_page_update_and_empty_outbox_are_flat_with_unrelated_answers() {
        let mut observations = Vec::new();
        for history in [200, 2000] {
            let (store, root, record) = fixture(history);
            store.database().unwrap(); // migration is outside the warm budget
            let (_, get) = store::metrics::measure(|| store.get(&record.notification_id).unwrap());
            let (rows, page) =
                store::metrics::measure(|| store.list("pending", Some("selected"), 20).unwrap());
            assert_eq!(rows, vec![record.clone()]);
            let (_, update) = store::metrics::measure(|| {
                store
                    .edit_record(&record.notification_id, |state| {
                        state.acknowledge(
                            "selected",
                            &record.notification_id,
                            DateTime::from_timestamp(1_799_000_000, 0).unwrap(),
                        )
                    })
                    .unwrap()
                    .unwrap()
            });
            for (id, _, _) in store.claim_notices(100).unwrap() {
                store.ack_notice(id).unwrap();
            }
            let (empty, tick) = store::metrics::measure(|| store.claim_notices(100).unwrap());
            assert!(empty.is_empty());
            assert_eq!(tick.bytes_encoded, 0);
            assert_eq!(tick.bytes_written, 0);
            assert_eq!(
                get.whole_state_copies + page.whole_state_copies + update.whole_state_copies,
                0
            );
            observations.push((get, page, update, tick));
            drop(store);
            let _ = std::fs::remove_dir_all(root);
        }
        assert_eq!(observations[0], observations[1]);
    }

    #[test]
    fn notices_survive_failed_delivery_and_claimed_rows_are_exported() {
        let (store, root, _) = fixture(0);
        let claims = store.claim_notices(10).unwrap();
        assert_eq!(claims.len(), 1);
        assert!(store.claim_notices(10).unwrap().is_empty());
        assert_eq!(store.export_state().unwrap().notices.len(), 1);
        store.release_notice(claims[0].0).unwrap();
        let retry = store.claim_notices(10).unwrap();
        assert_eq!(retry[0].0, claims[0].0);
        store.ack_notice(retry[0].0).unwrap();
        assert!(store.claim_notices(10).unwrap().is_empty());
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn notice_claims_budget_payload_bytes_before_materializing_them() {
        let (store, root, _) = fixture(0);
        for (id, _, _) in store.claim_notices(100).unwrap() {
            store.ack_notice(id).unwrap();
        }
        store
            .with_state(|state| {
                state.notices.clear();
                for i in 0..4 {
                    state.notices.push(Notice {
                        method: super::super::CREATED_EVENT.into(),
                        session: "selected".into(),
                        params: serde_json::json!({"i":i,"text":"x".repeat(80*1024)}),
                    });
                }
                state.notices.push(Notice {
                    method: super::super::CREATED_EVENT.into(),
                    session: "selected".into(),
                    params: serde_json::json!({"i":4,"text":"x".repeat(400*1024)}),
                });
            })
            .unwrap();
        let (first, counts) = store::metrics::measure(|| store.claim_notices(100).unwrap());
        assert_eq!(first.len(), 3);
        assert!(counts.bytes_decoded <= 256 * 1024);
        assert!(
            store.claim_notices(100).unwrap().is_empty(),
            "a byte-limited batch still leases its FIFO head"
        );
        for (id, _, _) in first {
            store.ack_notice(id).unwrap();
        }
        let next = store.claim_notices(100).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].2.params["i"], 3);
        store.ack_notice(next[0].0).unwrap();
        let oversized = store.claim_notices(100).unwrap();
        assert_eq!(oversized.len(), 1);
        assert_eq!(oversized[0].2.params["i"], 4);
        store.ack_notice(oversized[0].0).unwrap();
        assert!(store.claim_notices(100).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leased_head_keeps_notices_fifo_and_queue_cap_applies_to_updates_too() {
        let (store, root, _) = fixture(0);
        store
            .with_state(|state| {
                state.notices = (0..2000)
                    .map(|n| Notice {
                        method: crate::owner_notify::UPDATED_EVENT.into(),
                        session: "selected".into(),
                        params: serde_json::json!({"n":n}),
                    })
                    .collect();
            })
            .unwrap();
        let exported = store.export_state().unwrap();
        assert_eq!(exported.notices.len(), 2000);
        assert_eq!(exported.notices[0].params, serde_json::json!({"n":0}));
        let first = store.claim_notices(1).unwrap();
        assert_eq!(first.len(), 1);
        assert!(
            store.claim_notices(100).unwrap().is_empty(),
            "an active head lease cannot be skipped"
        );
        store.release_notice(first[0].0).unwrap();
        let retry = store.claim_notices(2).unwrap();
        assert_eq!(retry[0].1, first[0].1);
        for (id, _, _) in retry {
            store.ack_notice(id).unwrap();
        }
        let count: i64 = store
            .database()
            .unwrap()
            .read(|tx| {
                Ok(tx.sql().query_row(
                    "SELECT n FROM notification_outbox_count WHERE id=1",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, 1998);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn r1_reimports_previous_writes_with_replies_receipts_and_notices_once() {
        let (store, root, record) = fixture(200);
        store.prepare_rollback().unwrap();
        let mut previous = OwnerNotifyStore::read_state(&store.state_path()).unwrap();
        previous
            .answered(
                "selected",
                &record.notification_id,
                Reply {
                    text: "previous build reply".into(),
                    replied_at: crate::asks::now_rfc3339(Utc::now()),
                    client_user_message_id: "previous-reply".into(),
                },
                Utc::now(),
            )
            .unwrap();
        previous.remember(
            "selected",
            "previous-action",
            "previous-digest",
            &serde_json::json!({"fromPrevious":true}),
            Utc::now(),
        );
        previous.revision += 1;
        crate::config::atomic::write_private_pretty_json_atomic(
            &store.state_path(),
            &previous,
            "previous writer fixture",
        )
        .unwrap();
        let next = OwnerNotifyStore::at(&root);
        assert_eq!(next.export_state().unwrap(), previous);
        std::fs::write(store.state_path(), b"stale JSON after reimport").unwrap();
        assert_eq!(
            OwnerNotifyStore::at(&root).export_state().unwrap(),
            previous
        );
        drop(next);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
