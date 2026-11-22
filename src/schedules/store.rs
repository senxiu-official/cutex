use super::{model::*, Error};
use anyhow::{ensure, Context};
use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::path::PathBuf;
const PAGE_BYTES: usize = 1024 * 1024;
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}
impl Store {
    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(crate::config::paths::runtime_dir()?.join("schedules"))
    }
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let store = Self { root: root.into() };
        crate::management::client_tokens::ensure_private_dir(&store.root)?;
        let mut conn = store.connection()?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version <= 1, "schedules database needs a newer cutex");
        if version == 0 {
            tx.execute_batch("CREATE TABLE schedules(id TEXT PRIMARY KEY,revision INTEGER NOT NULL,target TEXT NOT NULL,enabled INTEGER NOT NULL,next_due INTEGER,deleted_at INTEGER,last_run INTEGER,body TEXT NOT NULL);
                CREATE INDEX schedules_due ON schedules(enabled,deleted_at,next_due);
                CREATE INDEX schedules_active ON schedules(id) WHERE deleted_at IS NULL;
                CREATE INDEX schedules_target ON schedules(target,id) WHERE deleted_at IS NULL;
                CREATE TABLE schedule_runs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,schedule_id TEXT NOT NULL REFERENCES schedules(id),outcome TEXT NOT NULL,next_attempt INTEGER NOT NULL,lease_until INTEGER,body TEXT NOT NULL);
                CREATE INDEX schedule_runs_history ON schedule_runs(schedule_id,seq DESC);
                CREATE INDEX schedule_runs_due ON schedule_runs(outcome,next_attempt,lease_until);
                CREATE UNIQUE INDEX schedule_one_open_run ON schedule_runs(schedule_id) WHERE outcome IN('pending','held','retrying','dispatching');
                CREATE TABLE schedule_actions(action_id TEXT PRIMARY KEY,digest TEXT NOT NULL,reply TEXT NOT NULL);
                PRAGMA user_version=1;")?;
        }
        tx.commit()?;
        Ok(store)
    }
    fn connection(&self) -> anyhow::Result<Connection> {
        let conn = Connection::open(self.root.join("schedules-v1.sqlite3"))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(conn)
    }
    pub fn worker_lock(&self) -> anyhow::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("worker.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("a recurring-schedules worker already runs")?;
        Ok(file)
    }
    pub fn get(&self, id: &str) -> anyhow::Result<Schedule> {
        let row = read_schedule(&self.connection()?, id)?;
        if row.deleted_at.is_some() {
            return Err(Error::NotFound.into());
        }
        Ok(row)
    }
    pub fn create<T: TimeZone>(
        &self,
        a: CreateArgs,
        now: DateTime<Utc>,
        zone: &T,
        validate: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<Schedule> {
        validate_fields(&a.action_id, &a.target_session_id, &a.message, &a.rule)
            .map_err(invalid)?;
        let digest = digest("create", "", &a)?;
        let mut conn = self.connection()?;
        if let Some(row) = replay(&conn, &a.action_id, &digest)? {
            return Ok(row);
        }
        validate(&a.target_session_id)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(row) = replay(&tx, &a.action_id, &digest)? {
            return Ok(row);
        }
        let row = Schedule {
            id: format!("schedule-{}", uuid::Uuid::new_v4().simple()),
            revision: 1,
            target_session_id: a.target_session_id,
            message: a.message,
            next_run_at: if a.enabled {
                Some(a.rule.next_after(zone, now, now).map_err(invalid)?)
            } else {
                None
            },
            rule: a.rule,
            timezone: "host".into(),
            enabled: a.enabled,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            last_run: None,
        };
        tx.execute("INSERT INTO schedules(id,revision,target,enabled,next_due,body) VALUES(?1,?2,?3,?4,?5,?6)",params![row.id,signed(row.revision)?,row.target_session_id,row.enabled,row.next_run_at.map(|d|d.timestamp_millis()),serde_json::to_string(&row)?])?;
        save_action(&tx, &a.action_id, &digest, &row)?;
        tx.commit()?;
        Ok(row)
    }
    pub fn update<T: TimeZone>(
        &self,
        id: &str,
        a: UpdateArgs,
        now: DateTime<Utc>,
        zone: &T,
        validate: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<Schedule> {
        validate_fields(&a.action_id, &a.target_session_id, &a.message, &a.rule)
            .map_err(invalid)?;
        let digest = digest("update", id, &a)?;
        let mut conn = self.connection()?;
        if let Some(row) = replay(&conn, &a.action_id, &digest)? {
            return Ok(row);
        }
        validate(&a.target_session_id)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(row) = replay(&tx, &a.action_id, &digest)? {
            return Ok(row);
        }
        let mut row = read_schedule(&tx, id)?;
        check_revision(&row, a.expected_revision)?;
        row.revision = row.revision.checked_add(1).context("revision overflow")?;
        row.target_session_id = a.target_session_id;
        row.message = a.message;
        row.rule = a.rule;
        row.enabled = a.enabled;
        row.updated_at = now;
        row.next_run_at = if row.enabled {
            Some(
                row.rule
                    .next_after(zone, row.created_at, now)
                    .map_err(invalid)?,
            )
        } else {
            None
        };
        cancel_unreserved(&tx, id, now)?;
        row.last_run = read_last_run(&tx, id)?;
        save_schedule(&tx, &row)?;
        save_action(&tx, &a.action_id, &digest, &row)?;
        tx.commit()?;
        Ok(row)
    }
    pub fn delete(&self, id: &str, a: DeleteArgs, now: DateTime<Utc>) -> anyhow::Result<Schedule> {
        crate::agent_management::AgentActionId::new(&a.action_id)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let digest = digest("delete", id, &a)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(row) = replay(&tx, &a.action_id, &digest)? {
            return Ok(row);
        }
        let mut row = read_schedule(&tx, id)?;
        check_revision(&row, a.expected_revision)?;
        row.revision = row.revision.checked_add(1).context("revision overflow")?;
        row.enabled = false;
        row.next_run_at = None;
        row.deleted_at = Some(now);
        row.updated_at = now;
        cancel_unreserved(&tx, id, now)?;
        row.last_run = read_last_run(&tx, id)?;
        save_schedule(&tx, &row)?;
        save_action(&tx, &a.action_id, &digest, &row)?;
        tx.commit()?;
        Ok(row)
    }
    pub fn list(
        &self,
        target: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Page<Schedule>> {
        let conn = self.connection()?;
        let limit = limit.clamp(1, 100);
        let (sql, key) = if let Some(target) = target {
            ("SELECT id FROM schedules WHERE deleted_at IS NULL AND target=?1 AND id>?2 ORDER BY id LIMIT ?3",target)
        } else {
            ("SELECT id FROM schedules WHERE deleted_at IS NULL AND ?1='' AND id>?2 ORDER BY id LIMIT ?3","")
        };
        let mut stmt = conn.prepare(sql)?;
        let ids = stmt.query_map(params![key, after.unwrap_or(""), (limit + 1) as i64], |r| {
            r.get::<_, String>(0)
        })?;
        let mut data = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for id in ids {
            let row = read_schedule(&conn, &id?)?;
            let size = serde_json::to_vec(&row)?.len();
            if data.len() == limit || (!data.is_empty() && bytes + size > PAGE_BYTES) {
                more = true;
                break;
            }
            bytes += size;
            data.push(row);
        }
        let next_cursor = if more {
            data.last().map(|r| r.id.clone())
        } else {
            None
        };
        Ok(Page { data, next_cursor })
    }
    pub fn runs(&self, id: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Page<Run>> {
        let conn = self.connection()?;
        read_schedule(&conn, id)?;
        let limit = limit.clamp(1, 100);
        let mut stmt=conn.prepare("SELECT body FROM schedule_runs WHERE schedule_id=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3")?;
        let rows = stmt.query_map(
            params![
                id,
                signed(before.unwrap_or(i64::MAX as u64))?,
                (limit + 1) as i64
            ],
            |r| r.get::<_, String>(0),
        )?;
        let mut data = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for raw in rows {
            let row: Run = serde_json::from_str(&raw?)?;
            let size = serde_json::to_vec(&row)?.len();
            if data.len() == limit || (!data.is_empty() && bytes + size > PAGE_BYTES) {
                more = true;
                break;
            }
            bytes += size;
            data.push(row);
        }
        let next_cursor = if more {
            data.last().map(|r| r.sequence.to_string())
        } else {
            None
        };
        Ok(Page { data, next_cursor })
    }
    /// Atomically coalesce missed slots, then lease bounded pending work. No runtime I/O here.
    pub fn claim<T: TimeZone>(
        &self,
        now: DateTime<Utc>,
        zone: &T,
        limit: usize,
    ) -> anyhow::Result<Vec<Run>> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let limit = limit.clamp(1, 32);
        let ids = {
            let mut q=tx.prepare("SELECT id FROM schedules WHERE enabled=1 AND deleted_at IS NULL AND next_due<=?1 ORDER BY next_due,id LIMIT ?2")?;
            let found = q
                .query_map(params![now.timestamp_millis(), limit as i64], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            found
        };
        for id in ids {
            let row = read_schedule(&tx, &id)?;
            let due = row.next_run_at.context("due schedule without next run")?;
            let next = row.rule.next_after(zone, row.created_at, now)?;
            tx.execute(
                "UPDATE schedules SET next_due=?1 WHERE id=?2",
                params![next.timestamp_millis(), id],
            )?;
            let open:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM schedule_runs WHERE schedule_id=?1 AND outcome IN('pending','held','retrying','dispatching'))",params![id],|r|r.get(0))?;
            if !open {
                let mut run = Run {
                    id: format!(
                        "schedule-run-{}",
                        crate::asks::sha256_hex(&serde_json::to_vec(&(&id, row.revision, due))?)
                    ),
                    sequence: 0,
                    schedule_id: id.clone(),
                    schedule_revision: row.revision,
                    target_session_id: row.target_session_id,
                    message: row.message,
                    scheduled_at: due,
                    started_at: None,
                    finished_at: None,
                    outcome: "pending".into(),
                    attempts: 0,
                    next_attempt_at: now,
                    last_error: None,
                    message_id: None,
                    lease_token: None,
                };
                tx.execute("INSERT INTO schedule_runs(id,schedule_id,outcome,next_attempt,body) VALUES(?1,?2,'pending',?3,?4)",params![run.id,id,now.timestamp_millis(),serde_json::to_string(&run)?])?;
                run.sequence =
                    u64::try_from(tx.last_insert_rowid()).context("invalid run sequence")?;
                save_run(&tx, &run, None)?;
                tx.execute(
                    "UPDATE schedules SET last_run=?1 WHERE id=?2",
                    params![signed(run.sequence)?, id],
                )?;
            }
        }
        let mut leased = Vec::new();
        {
            let mut q=tx.prepare("SELECT body FROM schedule_runs WHERE next_attempt<=?1 AND (outcome IN('pending','held','retrying') OR (outcome='dispatching' AND lease_until<=?1)) ORDER BY seq LIMIT ?2")?;
            let rows = q.query_map(params![now.timestamp_millis(), limit as i64], |r| {
                r.get::<_, String>(0)
            })?;
            for raw in rows {
                let mut run: Run = serde_json::from_str(&raw?)?;
                run.outcome = "dispatching".into();
                run.started_at.get_or_insert(now);
                run.attempts = run.attempts.saturating_add(1);
                run.lease_token = Some(uuid::Uuid::new_v4().to_string());
                leased.push(run);
            }
        }
        for run in &leased {
            save_run(
                &tx,
                run,
                Some((now + Duration::minutes(5)).timestamp_millis()),
            )?;
        }
        tx.commit()?;
        Ok(leased)
    }
    pub fn finish(
        &self,
        run: &Run,
        outcome: &str,
        error: Option<String>,
        message_id: Option<String>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        ensure!(
            ["held", "retrying", "refused", "handedOff"].contains(&outcome),
            "invalid run outcome"
        );
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut current = read_run(&tx, run.sequence)?;
        if current.outcome != "dispatching" || current.lease_token != run.lease_token {
            return Ok(false);
        }
        current.outcome = outcome.into();
        current.last_error = error.map(|s| s.chars().take(2048).collect());
        current.message_id = message_id;
        current.finished_at = if matches!(outcome, "handedOff" | "refused") {
            Some(now)
        } else {
            None
        };
        let delay = 30_i64.saturating_mul(1_i64 << current.attempts.saturating_sub(1).min(5));
        current.next_attempt_at = now + Duration::seconds(delay.min(600));
        current.lease_token = None;
        save_run(&tx, &current, None)?;
        tx.commit()?;
        Ok(true)
    }
    /// Only called after taking worker.lock, proving no previous worker survives.
    pub fn recover_dispatching(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        let mut count = 0;
        loop {
            let mut conn = self.connection()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut rows = {
                let mut q=tx.prepare("SELECT body FROM schedule_runs WHERE outcome='dispatching' ORDER BY seq LIMIT 32")?;
                let result = q
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                result
            };
            if rows.is_empty() {
                break;
            }
            for raw in rows.drain(..) {
                let mut run: Run = serde_json::from_str(&raw)?;
                run.outcome = "retrying".into();
                run.lease_token = None;
                run.next_attempt_at = now;
                save_run(&tx, &run, None)?;
                count += 1;
            }
            tx.commit()?;
        }
        Ok(count)
    }
}
fn invalid(e: impl std::fmt::Display) -> anyhow::Error {
    Error::Invalid(e.to_string()).into()
}
fn digest(verb: &str, id: &str, value: &impl serde::Serialize) -> anyhow::Result<String> {
    Ok(crate::asks::sha256_hex(&serde_json::to_vec(&(
        verb, id, value,
    ))?))
}
fn replay(conn: &Connection, action: &str, digest: &str) -> anyhow::Result<Option<Schedule>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT digest,reply FROM schedule_actions WHERE action_id=?1",
            params![action],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((old, reply)) if old == digest => Ok(Some(serde_json::from_str(&reply)?)),
        Some(_) => Err(Error::Conflict("actionId reused with different request".into()).into()),
    }
}
fn save_action(
    tx: &Transaction<'_>,
    action: &str,
    digest: &str,
    row: &Schedule,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO schedule_actions(action_id,digest,reply) VALUES(?1,?2,?3)",
        params![action, digest, serde_json::to_string(row)?],
    )?;
    Ok(())
}
fn read_schedule(conn: &Connection, id: &str) -> anyhow::Result<Schedule> {
    let raw: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT body,next_due FROM schedules WHERE id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (body, next) = raw.ok_or(Error::NotFound)?;
    let mut row: Schedule = serde_json::from_str(&body)?;
    row.next_run_at = next
        .map(|n| DateTime::from_timestamp_millis(n).context("invalid next run"))
        .transpose()?;
    row.last_run = read_last_run(conn, id)?;
    Ok(row)
}
fn read_last_run(conn: &Connection, id: &str) -> anyhow::Result<Option<Run>> {
    let body: Option<String> = conn
        .query_row(
            "SELECT r.body FROM schedules s JOIN schedule_runs r ON r.seq=s.last_run WHERE s.id=?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    body.map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(Into::into)
}
fn read_run(conn: &Connection, seq: u64) -> anyhow::Result<Run> {
    let body: String = conn.query_row(
        "SELECT body FROM schedule_runs WHERE seq=?1",
        params![signed(seq)?],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&body)?)
}
fn save_run(tx: &Transaction<'_>, run: &Run, lease: Option<i64>) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE schedule_runs SET outcome=?1,next_attempt=?2,lease_until=?3,body=?4 WHERE seq=?5",
        params![
            run.outcome,
            run.next_attempt_at.timestamp_millis(),
            lease,
            serde_json::to_string(run)?,
            signed(run.sequence)?
        ],
    )?;
    Ok(())
}
fn save_schedule(tx: &Transaction<'_>, row: &Schedule) -> anyhow::Result<()> {
    tx.execute("UPDATE schedules SET revision=?1,target=?2,enabled=?3,next_due=?4,deleted_at=?5,body=?6 WHERE id=?7",params![signed(row.revision)?,row.target_session_id,row.enabled,row.next_run_at.map(|n|n.timestamp_millis()),row.deleted_at.map(|n|n.timestamp_millis()),serde_json::to_string(row)?,row.id])?;
    Ok(())
}
fn check_revision(row: &Schedule, expected: u64) -> anyhow::Result<()> {
    if row.deleted_at.is_some() {
        return Err(Error::NotFound.into());
    }
    if row.revision != expected {
        return Err(Error::Conflict("schedule revision mismatch".into()).into());
    }
    Ok(())
}
fn cancel_unreserved(tx: &Transaction<'_>, id: &str, now: DateTime<Utc>) -> anyhow::Result<()> {
    let raw:Option<String>=tx.query_row("SELECT body FROM schedule_runs WHERE schedule_id=?1 AND outcome IN('pending','held','retrying')",params![id],|r|r.get(0)).optional()?;
    if let Some(raw) = raw {
        let mut run: Run = serde_json::from_str(&raw)?;
        run.outcome = "cancelled".into();
        run.last_error = Some("schedule changed or disabled before dispatch".into());
        run.finished_at = Some(now);
        run.lease_token = None;
        save_run(tx, &run, None)?;
    }
    Ok(())
}

fn signed(value: u64) -> anyhow::Result<i64> {
    i64::try_from(value).context("schedule integer exceeds SQLite range")
}
