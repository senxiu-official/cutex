//! Permanent formal-name ledger. These tables have no pruning or session FK:
//! deleting a session must not release another agent's old-name reservation.
use super::*;
use crate::agent_names::{
    NameClaim, NameError, NameHistoryEvent, NameHistoryPage, NameScope, RenameCommit, RenameRequest,
};
use rusqlite::OptionalExtension;

const EVENT_COLUMNS: &str = "sequence,event_id,request_id,session_id,old_name,new_name,scope_json,committed_at,actor,previous_revision,revision";

pub(in crate::session::store) fn install(connection: &mut Connection) -> anyhow::Result<()> {
    let installed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='agent_names_v1')",
        [],
        |row| row.get(0),
    )?;
    if installed {
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch("
        CREATE TABLE IF NOT EXISTS agent_name_events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id TEXT NOT NULL UNIQUE, request_id TEXT NOT NULL UNIQUE,
            request_sha256 TEXT NOT NULL, session_id TEXT NOT NULL,
            old_name TEXT NOT NULL, new_name TEXT NOT NULL,
            scope_json TEXT NOT NULL, committed_at TEXT NOT NULL, actor TEXT NOT NULL,
            previous_revision INTEGER NOT NULL, revision INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS agent_name_events_session ON agent_name_events(session_id,sequence DESC);
        CREATE TABLE IF NOT EXISTS agent_name_claims (
            scope_key TEXT NOT NULL, normalized_name TEXT NOT NULL, session_id TEXT NOT NULL,
            PRIMARY KEY(scope_key,normalized_name,session_id)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS agent_name_claims_name ON agent_name_claims(normalized_name,scope_key,session_id);
        CREATE TABLE IF NOT EXISTS agent_name_outbox (
            event_id TEXT NOT NULL, recipient_session_id TEXT NOT NULL, text TEXT NOT NULL,
            committed_at TEXT NOT NULL, delivered_at TEXT,
            PRIMARY KEY(event_id,recipient_session_id)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS agent_name_outbox_pending ON agent_name_outbox(committed_at,event_id,recipient_session_id)
            WHERE delivered_at IS NULL;
        INSERT OR IGNORE INTO meta(key,value) VALUES('agent_names_v1','1');
    ")?;
    transaction.commit()?;
    Ok(())
}

fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    value
        .try_into()
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn event(row: &rusqlite::Row<'_>) -> rusqlite::Result<NameHistoryEvent> {
    let scope_json: String = row.get(6)?;
    let scope = serde_json::from_str(&scope_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(NameHistoryEvent {
        sequence: unsigned(row, 0)?,
        event_id: row.get(1)?,
        request_id: row.get(2)?,
        cutex_session_id: row.get(3)?,
        old_name: row.get(4)?,
        new_name: row.get(5)?,
        scope,
        committed_at: row.get(7)?,
        actor: row.get(8)?,
        previous_revision: unsigned(row, 9)?,
        revision: unsigned(row, 10)?,
    })
}

fn conflict(code: &'static str, message: &str, holders: Vec<String>) -> anyhow::Error {
    let mut error = NameError::new(code, message);
    error.details = serde_json::json!({"holderSessionIds":holders});
    error.into()
}

pub(in crate::session::store) fn replay_at(
    path: &Path,
    session_id: &str,
    request: &RenameRequest,
) -> anyhow::Result<Option<RenameCommit>> {
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    let digest = crate::management::v2::native_requests::canonical_request_sha256(
        &serde_json::json!({"sessionId":session_id,"request":request}),
    )?;
    with_connection(&db, |connection| {
        let previous: Option<(String, String)> = connection
            .query_row(
                "SELECT session_id,request_sha256 FROM agent_name_events WHERE request_id=?1",
                [&request.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((bound, sha256)) = previous else {
            return Ok(None);
        };
        if bound != session_id || sha256 != digest {
            return Err(NameError::new(
                "idempotency_conflict",
                "requestId is already bound to a different rename",
            )
            .into());
        }
        let event = connection.query_row(
            &format!("SELECT {EVENT_COLUMNS} FROM agent_name_events WHERE request_id=?1"),
            [&request.request_id],
            event,
        )?;
        Ok(Some(RenameCommit {
            event,
            replayed: true,
        }))
    })
}

fn assert_available(
    connection: &Connection,
    scope: &NameScope,
    name: &str,
    own: Option<&str>,
) -> anyhow::Result<()> {
    let mut query = connection.prepare_cached(
        "SELECT session_id FROM agent_name_claims WHERE scope_key=?1 AND normalized_name=?2 AND (?3 IS NULL OR session_id<>?3) ORDER BY session_id",
    )?;
    let holders = query
        .query_map(
            params![
                scope.storage_key(),
                crate::agent_names::normalized_name(name),
                own
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !holders.is_empty() {
        return Err(conflict(
            "agent_name_reserved",
            "that old name is reserved to another agent in this scope",
            holders,
        ));
    }
    Ok(())
}

/// Final commit-time guard, shared by every current session writer. Legacy
/// imports predate the additive ledger and must preserve their existing rows.
pub(super) fn validate_session_change(
    connection: &Connection,
    before: Option<&CutexSessionRecord>,
    after: &CutexSessionRecord,
) -> anyhow::Result<()> {
    let name_changed =
        before.is_some_and(|before| before.formal_agent_name != after.formal_agent_name);
    let restores = before.is_some_and(|before| before.is_retired() && !after.is_retired());
    if before.is_some() && !name_changed && !restores {
        return Ok(());
    }
    let installed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='agent_names_v1')",
        [],
        |row| row.get(0),
    )?;
    if !installed {
        return Ok(());
    }
    if name_changed {
        let audited: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_name_events WHERE session_id=?1)",
            [&after.cutex_session_id],
            |row| row.get(0),
        )?;
        if audited {
            return Err(NameError::new(
                "formal_name_owner_rename_required",
                "an audited formal name must change through the owner rename transaction",
            )
            .into());
        }
    }
    let Some(name) = after.formal_agent_name.as_deref() else {
        return Ok(());
    };
    if after.is_retired() {
        return Ok(());
    }
    // Creation may spend time bootstrapping a native thread after its first
    // name check. Serialize publication against a concurrent owner rename;
    // imported legacy rows remain grandfathered, never silently rewritten.
    if after.creation_action.is_some()||restores||name_changed {
        let mut query=connection.prepare_cached("SELECT cutex_session_id FROM sessions WHERE archive_state='active' AND formal_agent_name=?1 COLLATE NOCASE AND cutex_session_id<>?2 ORDER BY cutex_session_id")?;
        let holders=query.query_map(params![name,after.cutex_session_id],|row|row.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !holders.is_empty() {
            return Err(conflict("agent_name_conflict","that formal name belongs to another active agent on this host",holders));
        }
    }
    assert_available(
        connection,
        &crate::agent_names::mutation_scope::current(),
        name,
        Some(&after.cutex_session_id),
    )?;
    Ok(())
}

/// Called only with the host authority fence, session lane, then session file
/// lock held. The name, audit, claim and quiet-note intents commit together.
pub(in crate::session::store) fn rename_locked(
    path: &Path,
    session_id: &str,
    request: &RenameRequest,
    scope: &NameScope,
    director: Option<&str>,
    actor: &str,
) -> anyhow::Result<RenameCommit> {
    request.validate_typed()?;
    let db = database_path(path);
    let digest = crate::management::v2::native_requests::canonical_request_sha256(
        &serde_json::json!({"sessionId":session_id,"request":request}),
    )?;
    let _refresh = REFRESH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = with_connection(&db, |connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, String)> = transaction
            .query_row(
                "SELECT session_id,request_sha256 FROM agent_name_events WHERE request_id=?1",
                [&request.request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((bound, sha256)) = previous {
            if bound != session_id || sha256 != digest {
                return Err(NameError::new(
                    "idempotency_conflict",
                    "requestId is already bound to a different rename",
                )
                .into());
            }
            let event = transaction.query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM agent_name_events WHERE request_id=?1"),
                [&request.request_id],
                event,
            )?;
            transaction.finish()?;
            return Ok(RenameCommit {
                event,
                replayed: true,
            });
        }
        let stored: Option<(String,String)> = transaction.query_row(
            "SELECT key,record_json FROM sessions WHERE cutex_session_id=?1 ORDER BY key LIMIT 1", [session_id],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        let (key, json) = stored.ok_or_else(|| {
            NameError::new("session_not_found", "the target session no longer exists")
        })?;
        let mut record: CutexSessionRecord = serde_json::from_str(&json)?;
        if record.durable_revision() != request.expected_revision {
            return Err(NameError::new(
                "stale_session_revision",
                "the session changed; reload before renaming",
            )
            .into());
        }
        let old = record
            .formal_agent_name
            .clone()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                NameError::new("not_formal_agent", "the target has no formal agent name")
            })?;
        if old == request.name {
            return Err(
                NameError::new("agent_name_unchanged", "the formal name is unchanged").into(),
            );
        }
        let mut current = transaction.prepare_cached(
            "SELECT cutex_session_id FROM sessions WHERE archive_state='active' AND formal_agent_name=?1 COLLATE NOCASE AND cutex_session_id<>?2 ORDER BY cutex_session_id",
        )?;
        let holders = current
            .query_map(params![request.name, session_id], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(current);
        if !holders.is_empty() {
            return Err(conflict(
                "agent_name_conflict",
                "that formal name belongs to another active agent on this host",
                holders,
            ));
        }
        assert_available(&transaction, scope, &request.name, Some(session_id))?;
        let previous_revision = record.durable_revision();
        record.formal_agent_name = Some(request.name.clone());
        record.bump_durable_revision()?;
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        record.updated_at = at.clone();
        let revision = read_revision(&transaction)?
            .checked_add(1)
            .context("session store revision exhausted")?;
        let changed = transaction.execute(
            "UPDATE sessions SET formal_agent_name=?1,record_json=?2,rev=?3 WHERE key=?4",
            params![
                request.name,
                serde_json::to_string(&record)?,
                i64::try_from(revision)?,
                key
            ],
        )?;
        anyhow::ensure!(
            changed == 1,
            "renamed session disappeared under transaction"
        );
        let event_id = format!("rename-{}", uuid::Uuid::new_v4());
        transaction.execute(
            "INSERT INTO agent_name_events(event_id,request_id,request_sha256,session_id,old_name,new_name,scope_json,committed_at,actor,previous_revision,revision)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![event_id,request.request_id,digest,session_id,old,request.name,serde_json::to_string(scope)?,at,actor,i64::try_from(previous_revision)?,i64::try_from(record.durable_revision())?],
        )?;
        // Only names relinquished by a rename become permanent old-name
        // reservations. An archived current name keeps the existing reuse rule.
        transaction.execute(
            "INSERT OR IGNORE INTO agent_name_claims(scope_key,normalized_name,session_id) VALUES(?1,?2,?3)",
            params![scope.storage_key(),crate::agent_names::normalized_name(&old),session_id],
        )?;
        let note = format!("The owner renamed {old} to {} (id {session_id}). Use {} from now on. Session identity and queued work are unchanged.",request.name,request.name);
        for recipient in std::iter::once(session_id).chain(director.filter(|id| *id != session_id))
        {
            transaction.execute(
                "INSERT OR IGNORE INTO agent_name_outbox(event_id,recipient_session_id,text,committed_at) VALUES(?1,?2,?3,?4)",
                params![event_id,recipient,note,at],
            )?;
        }
        transaction.execute(
            "UPDATE meta SET value=?1 WHERE key='store_revision'",
            [revision.to_string()],
        )?;
        let event = transaction.query_row(
            &format!("SELECT {EVENT_COLUMNS} FROM agent_name_events WHERE event_id=?1"),
            [&event_id],
            event,
        )?;
        transaction.commit()?;
        Ok(RenameCommit {
            event,
            replayed: false,
        })
    });
    // Standard row revision and catalog/change-feed triggers ran in the same
    // transaction. Readers rebuild their snapshot at that committed revision.
    if result.as_ref().is_ok_and(|result| !result.replayed) {
        let _ = take_cached(&db);
    }
    result
}

pub(in crate::session::store) fn history_at(
    path: &Path,
    session_id: &str,
    before: Option<u64>,
    limit: usize,
) -> anyhow::Result<NameHistoryPage> {
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    with_connection(&db, |connection| {
        let limit = limit.clamp(1, 100);
        let mut query = connection.prepare_cached(&format!(
            "SELECT {EVENT_COLUMNS} FROM agent_name_events WHERE session_id=?1 AND (?2 IS NULL OR sequence<?2) ORDER BY sequence DESC LIMIT ?3"))?;
        let mut events = query
            .query_map(
                params![
                    session_id,
                    before.map(i64::try_from).transpose()?,
                    (limit + 1) as i64
                ],
                event,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = events.len() > limit;
        events.truncate(limit);
        let next = more.then(|| events.last().unwrap().sequence);
        Ok(NameHistoryPage { events, next })
    })
}

pub(in crate::session::store) fn claims_at(
    path: &Path,
    scope: Option<&NameScope>,
    name: &str,
) -> anyhow::Result<(u64, Vec<NameClaim>)> {
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    with_connection(&db, |connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let revision = read_revision(&transaction)?;
        let scope_key = scope.map(NameScope::storage_key);
        let mut query = transaction.prepare_cached("SELECT scope_key,normalized_name,session_id FROM agent_name_claims WHERE normalized_name=?1 AND (?2 IS NULL OR scope_key=?2) ORDER BY scope_key,session_id")?;
        let claims = query
            .query_map(
                params![crate::agent_names::normalized_name(name), scope_key],
                |row| {
                    let key: String = row.get(0)?;
                    let scope = key
                        .strip_prefix("project:")
                        .map(|id| NameScope::for_project(Some(id)))
                        .unwrap_or(NameScope::Unassigned);
                    Ok(NameClaim {
                        scope,
                        normalized_name: row.get(1)?,
                        cutex_session_id: row.get(2)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        transaction.finish()?;
        Ok((revision, claims))
    })
}

pub(in crate::session::store) fn available_at(
    path: &Path,
    scope: &NameScope,
    name: &str,
    own: Option<&str>,
) -> anyhow::Result<()> {
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    with_connection(&db, |connection| {
        assert_available(connection, scope, name, own)
    })
}

#[derive(Clone, Debug)]
pub struct PendingRenameNote {
    pub event_id: String,
    pub recipient_session_id: String,
    pub text: String,
}

pub(in crate::session::store) fn note_at(path:&Path,event:&str,recipient:&str)->anyhow::Result<Option<PendingRenameNote>> {
    let db=ensure_ready(path,true,false)?.context("session database missing")?;
    with_connection(&db,|connection| {
        connection.query_row("SELECT event_id,recipient_session_id,text FROM agent_name_outbox WHERE event_id=?1 AND recipient_session_id=?2",
            params![event,recipient],|row|Ok(PendingRenameNote {event_id:row.get(0)?,recipient_session_id:row.get(1)?,text:row.get(2)?})).optional().map_err(Into::into)
    })
}

pub(in crate::session::store) fn pending_for_at(path:&Path,recipients:&[String],limit:usize)->anyhow::Result<Vec<PendingRenameNote>> {
    if recipients.is_empty() {return Ok(Vec::new())}
    let db=ensure_ready(path,true,false)?.context("session database missing")?;
    with_connection(&db,|connection| {
        // JSON1 avoids SQLite's variable-count limit for a large online roster.
        let mut query=connection.prepare_cached("SELECT event_id,recipient_session_id,text FROM agent_name_outbox WHERE delivered_at IS NULL AND recipient_session_id IN (SELECT value FROM json_each(?1)) ORDER BY committed_at,event_id,recipient_session_id LIMIT ?2")?;
        let rows=query.query_map(params![serde_json::to_string(recipients)?,limit.clamp(1,100) as i64],|row|Ok(PendingRenameNote {event_id:row.get(0)?,recipient_session_id:row.get(1)?,text:row.get(2)?}))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub(in crate::session::store) fn pending_at(
    path: &Path,
    limit: usize,
) -> anyhow::Result<Vec<PendingRenameNote>> {
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    with_connection(&db, |connection| {
        let mut query = connection.prepare_cached("SELECT event_id,recipient_session_id,text FROM agent_name_outbox WHERE delivered_at IS NULL ORDER BY committed_at,event_id,recipient_session_id LIMIT ?1")?;
        let notes = query
            .query_map([limit.clamp(1, 100) as i64], |row| {
                Ok(PendingRenameNote {
                    event_id: row.get(0)?,
                    recipient_session_id: row.get(1)?,
                    text: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    })
}

pub(in crate::session::store) fn delivered_at(
    path: &Path,
    event: &str,
    recipient: &str,
) -> anyhow::Result<()> {
    let db = ensure_ready(path, true, false)?.context("session database missing")?;
    with_connection(&db, |connection| {
        connection.execute("UPDATE agent_name_outbox SET delivered_at=?1 WHERE event_id=?2 AND recipient_session_id=?3 AND delivered_at IS NULL",
            params![chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),event,recipient])?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "name_ledger_tests.rs"]
mod tests;
