//! S5 owner queue repository. The 054 ledger owns presentation/delivery state;
//! these rows retain the existing queue, native-send reservation and dedupe facts.
use super::*;
use crate::store::{self, ChangeKind, OpCounts, Scope, StoreDb, WriteTx};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Arc;

const ID: &str = "user-input";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS owner_sessions(
 session TEXT PRIMARY KEY, revision INTEGER NOT NULL, admission_revision INTEGER NOT NULL, asks_revision INTEGER NOT NULL,
 queue_count INTEGER NOT NULL, data BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS owner_sessions_admission ON owner_sessions(admission_revision,session);
CREATE INDEX IF NOT EXISTS owner_sessions_asks ON owner_sessions(asks_revision,session);
CREATE INDEX IF NOT EXISTS owner_sessions_queued ON owner_sessions(session) WHERE queue_count>0;
CREATE TABLE IF NOT EXISTS owner_inputs(
 session TEXT NOT NULL REFERENCES owner_sessions(session) ON DELETE CASCADE,
 id TEXT NOT NULL, client_id TEXT NOT NULL, queue_order INTEGER NOT NULL, data BLOB NOT NULL,
 PRIMARY KEY(session,id), UNIQUE(session,client_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS owner_inputs_order ON owner_inputs(session,queue_order);
CREATE TABLE IF NOT EXISTS owner_identities(
 session TEXT NOT NULL REFERENCES owner_sessions(session) ON DELETE CASCADE,
 id TEXT NOT NULL, delivery TEXT NOT NULL, expires_at INTEGER, expires_sub INTEGER, data BLOB NOT NULL,
 PRIMARY KEY(session,id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS owner_identities_delivery ON owner_identities(session,delivery,id);
CREATE INDEX IF NOT EXISTS owner_identities_expiry ON owner_identities(expires_at,expires_sub,session,id) WHERE expires_at IS NOT NULL;
";

fn query() {
    store::metrics::note(
        ID,
        OpCounts {
            sql_statements: 1,
            ..Default::default()
        },
    );
}
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
fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
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

pub(super) fn open(root: &Path) -> anyhow::Result<Arc<StoreDb>> {
    let db = StoreDb::open_at(
        &store::domain::OWNER_INPUT,
        &root.join("owner-input.sqlite3"),
    )?;
    let ready = db.read(|tx| {
        Ok(tx.sql().query_row(
            "SELECT EXISTS(SELECT 1 FROM store_migrations WHERE store=?1 AND version=1)",
            [ID],
            |r| r.get::<_, bool>(0),
        )?)
    })?;
    if ready {
        return Ok(db);
    }
    // The old lock fences one-time import against another new opener. Normal
    // access uses SQLite snapshots/transactions, with no file cache or JSON IO.
    let lock = open_private_lock(&root.join(USER_INPUT_LOCK_FILE))?;
    lock.lock()?;
    let result = (|| {
        db.register(store::spec(ID))?;
        db.write(|tx| {
            tx.sql().execute_batch(SCHEMA)?;
            let migrated: bool = tx.sql().query_row(
                "SELECT EXISTS(SELECT 1 FROM store_migrations WHERE store=?1 AND version=1)",
                [ID],
                |r| r.get(0),
            )?;
            if migrated {
                return Ok(());
            }
            // R1 hands authority back by removing the ready marker. On the
            // next upgrade rebuild from the current previous-version JSON,
            // never from the stale SQL rows left behind by rollback.
            tx.sql().execute("DELETE FROM owner_sessions", [])?;
            tx.reset_epoch(store::spec(ID))?;
            let path = root.join(USER_INPUT_STATE_FILE);
            let fingerprint = if path.exists() {
                use sha2::{Digest, Sha256};
                let mut source = File::open(&path)?;
                let mut hash = Sha256::new();
                std::io::copy(&mut source, &mut hash)?;
                let fingerprint = format!("{:x}", hash.finalize());
                // Import all records, including accepted/unresolved identities,
                // without pruning, resetting delivery, or waking runtimes.
                import(tx, File::open(&path)?)?;
                fingerprint
            } else {
                "absent".into()
            };
            tx.record_migration(store::spec(ID), 1, Some(&fingerprint))?;
            Ok(())
        })?;
        secure_file(db.path())?;
        Ok(db)
    })();
    lock.unlock()?;
    result
}

/// Parse one session at a time; no host-sized map or JSON byte buffer. Every
/// row and the ready marker commit together, so a failed import is invisible.
fn import(tx: &mut WriteTx<'_>, file: File) -> anyhow::Result<()> {
    use serde::de::{DeserializeSeed, Error, MapAccess, Visitor};
    struct Sessions<'a, 'b>(&'a mut WriteTx<'b>);
    impl<'de> DeserializeSeed<'de> for Sessions<'_, '_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Sessions<'_, '_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("session map")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
            while let Some((id, state)) = map.next_entry::<String, SessionInputState>()? {
                if revision(self.0.sql(), &id).map_err(M::Error::custom)? != 0 {
                    return Err(M::Error::custom("duplicate owner-input session"));
                }
                apply(self.0, &id, 0, None, Some(&state)).map_err(M::Error::custom)?;
            }
            Ok(())
        }
    }
    struct Document<'a, 'b>(&'a mut WriteTx<'b>);
    impl<'de> Visitor<'de> for Document<'_, '_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("owner input document")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
            let mut version = None;
            let mut sessions = false;
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "version" => {
                        if version.is_some() {
                            return Err(M::Error::custom("duplicate version"));
                        }
                        version = Some(map.next_value::<u8>()?);
                    }
                    "sessions" => {
                        if sessions {
                            return Err(M::Error::custom("duplicate sessions"));
                        }
                        sessions = true;
                        map.next_value_seed(Sessions(self.0))?;
                    }
                    _ => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
            }
            if version != Some(1) {
                return Err(M::Error::custom("unsupported owner-input version"));
            }
            Ok(())
        }
    }
    let mut decoder = serde_json::Deserializer::from_reader(std::io::BufReader::new(file));
    serde::Deserializer::deserialize_map(&mut decoder, Document(tx))?;
    decoder.end()?;
    Ok(())
}

pub(super) fn revision(sql: &Connection, session: &str) -> anyhow::Result<u64> {
    query();
    Ok(sql
        .query_row(
            "SELECT revision FROM owner_sessions WHERE session=?1",
            [session],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0) as u64)
}

pub(super) fn identity(
    sql: &Connection,
    session: &str,
    id: &str,
) -> anyhow::Result<Option<ClientInputIdentity>> {
    query();
    let bytes = sql
        .query_row(
            "SELECT data FROM owner_identities WHERE session=?1 AND id=?2",
            params![session, id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    bytes.map(decode).transpose()
}

/// Load the requested session's live queue and only its queued/explicit
/// identities. Retained accepted and dedupe history never enters this view.
pub(super) fn selected(
    sql: &Connection,
    session: &str,
    ids: &[&str],
    queue: bool,
) -> anyhow::Result<(Option<SessionInputState>, u64)> {
    query();
    let meta: Option<(i64, Vec<u8>)> = sql
        .query_row(
            "SELECT revision,data FROM owner_sessions WHERE session=?1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((revision, bytes)) = meta else {
        return Ok((None, 0));
    };
    let mut state: SessionInputState = decode(bytes)?;
    if queue {
        query();
        let mut stmt = sql.prepare("SELECT q.data,i.data FROM owner_inputs q LEFT JOIN owner_identities i ON i.session=q.session AND i.id=q.client_id WHERE q.session=?1 ORDER BY q.queue_order")?;
        let mut rows = stmt.query([session])?;
        while let Some(row) = rows.next()? {
            let item: StoredQueueItem = decode(row.get(0)?)?;
            if let Some(bytes) = row.get::<_, Option<Vec<u8>>>(1)? {
                state
                    .identities
                    .insert(item.item.client_user_message_id.clone(), decode(bytes)?);
            }
            state.items.push(item);
        }
    }
    for id in ids {
        if state.identities.contains_key(*id) {
            continue;
        }
        query();
        if let Some(bytes) = sql
            .query_row(
                "SELECT data FROM owner_identities WHERE session=?1 AND id=?2",
                params![session, id],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?
        {
            state.identities.insert((*id).into(), decode(bytes)?);
        }
    }
    Ok((Some(state), revision as u64))
}

fn metadata(state: &SessionInputState) -> SessionInputState {
    let mut meta = SessionInputState {
        queue_revision: state.queue_revision,
        admission: state.admission.clone(),
        pause: state.pause.clone(),
        admission_asks_revision: state.admission_asks_revision,
        ..Default::default()
    };
    for step in &mut meta.admission.snapshot.next_step {
        step.eligible = false;
    }
    meta
}

fn identity_put(
    sql: &Connection,
    session: &str,
    id: &str,
    value: &ClientInputIdentity,
) -> anyhow::Result<()> {
    let (delivery, at) = match &value.delivery {
        Some(InputDelivery::Sending { .. }) => ("sending", None),
        Some(InputDelivery::Unknown { .. }) => ("unknown", None),
        Some(InputDelivery::Accepted { accepted_at, .. }) => ("accepted", Some(accepted_at)),
        None => ("none", Some(&value.first_seen_at)),
    };
    let retention_at = at
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .and_then(|at| at.checked_add_signed(chrono::Duration::days(IDENTITY_RETENTION_DAYS)));
    let expires_at = retention_at.map(|at| at.timestamp());
    let expires_sub = retention_at.map(|at| i64::from(at.timestamp_subsec_nanos()));
    query();
    sql.execute("INSERT INTO owner_identities(session,id,delivery,expires_at,expires_sub,data) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session,id) DO UPDATE SET delivery=excluded.delivery,expires_at=excluded.expires_at,expires_sub=excluded.expires_sub,data=excluded.data",
        params![session,id,delivery,expires_at,expires_sub,encode(value)?])?;
    Ok(())
}

/// Compare and write the selected rows only. A session revision covers all its
/// queue/admission/identity changes, so stale preparation cannot replace them.
pub(super) fn apply(
    tx: &mut WriteTx<'_>,
    session: &str,
    expected: u64,
    before: Option<&SessionInputState>,
    after: Option<&SessionInputState>,
) -> anyhow::Result<bool> {
    let actual = revision(tx.sql(), session)?;
    if actual != expected {
        return Err(QueueRevisionConflict {
            expected,
            current: actual,
        }
        .into());
    }
    let changed = match (before, after) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            metadata(a) != metadata(b) || a.items != b.items || a.identities != b.identities
        }
        _ => true,
    };
    if !changed {
        return Ok(false);
    }
    let spec = store::spec(ID);
    let rev = tx.head(spec)?.seq + 1;
    let Some(after) = after else {
        query();
        tx.sql()
            .execute("DELETE FROM owner_sessions WHERE session=?1", [session])?;
        tx.record_change(
            spec,
            &Scope::Session(session.into()),
            session,
            rev,
            ChangeKind::Delete,
            None,
        )?;
        return Ok(true);
    };
    let meta_changed = before.is_none_or(|before| metadata(before) != metadata(after));
    if meta_changed {
        let meta = metadata(after);
        query();
        tx.sql().execute("INSERT INTO owner_sessions(session,revision,admission_revision,asks_revision,queue_count,data) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session) DO UPDATE SET revision=excluded.revision,admission_revision=excluded.admission_revision,asks_revision=excluded.asks_revision,data=excluded.data",
            params![session,i64::try_from(rev)?,i64::try_from(meta.admission.snapshot.revision)?,i64::try_from(meta.admission_asks_revision)?,i64::try_from(after.items.len())?,encode(&meta)?])?;
    } else {
        query();
        tx.sql().execute(
            "UPDATE owner_sessions SET revision=?2 WHERE session=?1",
            params![session, i64::try_from(rev)?],
        )?;
    }
    let empty = SessionInputState::default();
    let before = before.unwrap_or(&empty);
    for old in &before.items {
        if !after
            .items
            .iter()
            .any(|new| new.item.queue_id == old.item.queue_id)
        {
            query();
            tx.sql().execute(
                "DELETE FROM owner_inputs WHERE session=?1 AND id=?2",
                params![session, old.item.queue_id],
            )?;
        }
    }
    for (position, item) in after.items.iter().enumerate() {
        let old = before
            .items
            .iter()
            .enumerate()
            .find(|(_, old)| old.item.queue_id == item.item.queue_id);
        if old.is_none_or(|(old_position, old)| old_position != position || old != item) {
            query();
            // Moving changes an index column, never serializes an unchanged body.
            if old.is_some_and(|(_, old)| old == item) {
                tx.sql().execute(
                    "UPDATE owner_inputs SET queue_order=?3 WHERE session=?1 AND id=?2",
                    params![session, item.item.queue_id, i64::try_from(position)?],
                )?;
            } else {
                tx.sql().execute("INSERT INTO owner_inputs(session,id,client_id,queue_order,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session,id) DO UPDATE SET client_id=excluded.client_id,queue_order=excluded.queue_order,data=excluded.data",
                    params![session,item.item.queue_id,item.item.client_user_message_id,i64::try_from(position)?,encode(item)?])?;
            }
        }
    }
    if before.items.len() != after.items.len() {
        query();
        tx.sql().execute(
            "UPDATE owner_sessions SET queue_count=?2 WHERE session=?1",
            params![session, i64::try_from(after.items.len())?],
        )?;
    }
    for id in before
        .identities
        .keys()
        .filter(|id| !after.identities.contains_key(*id))
    {
        query();
        tx.sql().execute(
            "DELETE FROM owner_identities WHERE session=?1 AND id=?2",
            params![session, id],
        )?;
    }
    for (id, value) in &after.identities {
        if before.identities.get(id) != Some(value) {
            identity_put(tx.sql(), session, id, value)?;
        }
    }
    tx.record_change(
        spec,
        &Scope::Session(session.into()),
        session,
        rev,
        ChangeKind::Put,
        None,
    )?;
    Ok(true)
}

pub(super) fn migration_report(sql: &Connection) -> anyhow::Result<Value> {
    let fingerprint: Option<String> = sql
        .query_row(
            "SELECT source_fingerprint FROM store_migrations WHERE store=?1 AND version=1",
            [ID],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let count = |table: &str| -> anyhow::Result<i64> {
        Ok(
            sql.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?,
        )
    };
    Ok(
        serde_json::json!({"ready":fingerprint.is_some(),"sourceFingerprint":fingerprint,
        "sessions":count("owner_sessions")?,"inputs":count("owner_inputs")?,"identities":count("owner_identities")?}),
    )
}

pub(super) fn has_queue(sql: &Connection) -> anyhow::Result<bool> {
    query();
    Ok(sql.query_row(
        "SELECT EXISTS(SELECT 1 FROM owner_sessions WHERE queue_count>0)",
        [],
        |r| r.get(0),
    )?)
}
pub(super) fn queued_sessions(sql: &Connection) -> anyhow::Result<Vec<String>> {
    query();
    let mut stmt =
        sql.prepare("SELECT session FROM owner_sessions WHERE queue_count>0 ORDER BY session")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    store::metrics::note(
        ID,
        OpCounts {
            rows_visited: ids.len() as u64,
            rows_materialized: ids.len() as u64,
            ..Default::default()
        },
    );
    Ok(ids)
}
#[cfg(test)]
pub(super) use tests::test_dump;

#[cfg(test)]
mod tests;

/// Preserve the existing 30-day bookkeeping policy. Only expired, resolved,
/// unqueued identities are deleted. No retained body is decoded or reencoded.
/// Import and read paths never prune. All cleanup rolls back if the mutation fails.
pub(super) fn prune(
    tx: &mut WriteTx<'_>,
    now: chrono::DateTime<Utc>,
) -> anyhow::Result<Vec<String>> {
    let predicate = "expires_at<=?1 AND (expires_at<?1 OR expires_sub<?2) AND NOT EXISTS(SELECT 1 FROM owner_inputs q WHERE q.session=owner_identities.session AND q.client_id=owner_identities.id)";
    query();
    let ids = {
        let mut stmt = tx.sql().prepare(&format!(
            "SELECT DISTINCT session FROM owner_identities WHERE {predicate}"
        ))?;
        let ids = stmt
            .query_map(
                params![now.timestamp(), i64::from(now.timestamp_subsec_nanos())],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        ids
    };
    if ids.is_empty() {
        return Ok(ids);
    }
    query();
    let deleted = tx.sql().execute(
        &format!("DELETE FROM owner_identities WHERE {predicate}"),
        params![now.timestamp(), i64::from(now.timestamp_subsec_nanos())],
    )?;
    store::metrics::note(
        ID,
        OpCounts {
            rows_visited: deleted as u64,
            ..Default::default()
        },
    );
    for session in &ids {
        let rev = tx.head(store::spec(ID))?.seq + 1;
        query();
        tx.sql().execute(
            "UPDATE owner_sessions SET revision=?2 WHERE session=?1",
            params![session, i64::try_from(rev)?],
        )?;
        tx.record_change(
            store::spec(ID),
            &Scope::Session(session.clone()),
            session,
            rev,
            ChangeKind::Put,
            None,
        )?;
    }
    Ok(ids)
}
