use super::{model::*, Error};
use crate::personal_assistants::recurring::Template;
use anyhow::{ensure, Context};
use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::path::PathBuf;
const PAGE_BYTES: usize = 1024 * 1024;
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    publish_host_events: bool,
}
impl Store {
    pub fn open_default() -> anyhow::Result<Self> {
        let mut store = Self::open(crate::config::paths::runtime_dir()?.join("schedules"))?;
        store.publish_host_events = true;
        Ok(store)
    }
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let store = Self {
            root: root.into(),
            publish_host_events: false,
        };
        crate::management::client_tokens::ensure_private_dir(&store.root)?;
        let mut conn = store.connection()?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version <= 2, "schedules database needs a newer cutex");
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
        if version == 2 {
            ensure_template_maintenance(&tx)?;
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
        self.create_linked(a, None, now, zone, validate)
    }
    pub(crate) fn create_linked<T: TimeZone>(
        &self,
        a: CreateArgs,
        template: Option<Template>,
        now: DateTime<Utc>,
        zone: &T,
        validate: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<Schedule> {
        validate_fields(&a.action_id, &a.target_session_id, &a.message, &a.rule)
            .map_err(invalid)?;
        let digest = match &template {
            Some(t) => digest("confirmTemplate", "", &(&a, t))?,
            None => digest("create", "", &a)?,
        };
        let mut conn = self.connection()?;
        if let Some(row) = replay(&conn, &a.action_id, &digest)? {
            return Ok(row);
        }
        validate(&a.target_session_id)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(row) = replay(&tx, &a.action_id, &digest)? {
            return Ok(row);
        }
        let now = if template.is_some() {
            enable_templates(&tx)?;
            DateTime::from_timestamp_millis(now.timestamp_millis())
                .context("invalid template timestamp")?
        } else {
            now
        };
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
            assistant_template: template,
        };
        tx.execute("INSERT INTO schedules(id,revision,target,enabled,next_due,body) VALUES(?1,?2,?3,?4,?5,?6)",params![row.id,signed(row.revision)?,row.target_session_id,row.enabled,row.next_run_at.map(|d|d.timestamp_millis()),serde_json::to_string(&row)?])?;
        save_action(&tx, &a.action_id, &digest, &row)?;
        log_template(&tx, &a.action_id, "confirm", None, &row)?;
        self.commit_schedule_change(tx, row)
    }
    pub fn update<T: TimeZone>(
        &self,
        id: &str,
        a: UpdateArgs,
        now: DateTime<Utc>,
        zone: &T,
        validate: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<Schedule> {
        self.update_linked(id, a, None, now, zone, validate)
    }
    pub(crate) fn update_linked<T: TimeZone>(
        &self,
        id: &str,
        a: UpdateArgs,
        template: Option<Template>,
        now: DateTime<Utc>,
        zone: &T,
        validate: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<Schedule> {
        validate_fields(&a.action_id, &a.target_session_id, &a.message, &a.rule)
            .map_err(invalid)?;
        let digest = match &template {
            Some(t) => digest("confirmTemplate", id, &(&a, t))?,
            None => digest("update", id, &a)?,
        };
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
        if row.assistant_template.as_ref().map(|t| &t.assistant_id)
            != template.as_ref().map(|t| &t.assistant_id)
        {
            return Err(Error::Conflict("use the assistant's confirmed template".into()).into());
        }
        check_revision(&row, a.expected_revision)?;
        let prior = row.assistant_template.as_ref().map(|_| row.clone());
        row.assistant_template = template;
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
        log_template(&tx, &a.action_id, "confirm", prior.as_ref(), &row)?;
        self.commit_schedule_change(tx, row)
    }
    pub fn delete(&self, id: &str, a: DeleteArgs, now: DateTime<Utc>) -> anyhow::Result<Schedule> {
        self.stop(id, a, now, None, true)
    }
    pub(crate) fn stop(
        &self,
        id: &str,
        a: DeleteArgs,
        now: DateTime<Utc>,
        assistant: Option<&str>,
        cancel: bool,
    ) -> anyhow::Result<Schedule> {
        crate::agent_management::AgentActionId::new(&a.action_id)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let digest = match assistant {
            Some(key) => digest(
                if cancel {
                    "cancelTemplate"
                } else {
                    "pauseTemplate"
                },
                id,
                &(key, &a),
            )?,
            None => digest("delete", id, &a)?,
        };
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(row) = replay(&tx, &a.action_id, &digest)? {
            return Ok(row);
        }
        let mut row = read_schedule(&tx, id)?;
        if row
            .assistant_template
            .as_ref()
            .map(|t| t.assistant_id.as_str())
            != assistant
        {
            return Err(Error::Conflict("use the assistant's template controls".into()).into());
        }
        check_revision(&row, a.expected_revision)?;
        let prior = row.assistant_template.as_ref().map(|_| row.clone());
        row.revision = row.revision.checked_add(1).context("revision overflow")?;
        row.enabled = false;
        row.next_run_at = None;
        row.deleted_at = cancel.then_some(now);
        row.updated_at = now;
        cancel_unreserved(&tx, id, now)?;
        row.last_run = read_last_run(&tx, id)?;
        save_schedule(&tx, &row)?;
        save_action(&tx, &a.action_id, &digest, &row)?;
        log_template(
            &tx,
            &a.action_id,
            if cancel { "cancel" } else { "pause" },
            prior.as_ref(),
            &row,
        )?;
        self.commit_schedule_change(tx, row)
    }
    fn commit_schedule_change(
        &self,
        tx: Transaction<'_>,
        row: Schedule,
    ) -> anyhow::Result<Schedule> {
        tx.commit()?;
        if let (true, Some(template)) = (self.publish_host_events, &row.assistant_template) {
            crate::management::v2::documents::publish_host_event(
                crate::personal_assistants::CHANGED_EVENT,
                serde_json::json!({"assistantId": template.assistant_id, "areas": ["templates"]}),
            );
        }
        Ok(row)
    }
    pub(crate) fn template(&self, assistant: &str, id: &str) -> anyhow::Result<Schedule> {
        let row = read_schedule(&self.connection()?, id)?;
        if row
            .assistant_template
            .as_ref()
            .is_none_or(|t| t.assistant_id != assistant)
        {
            return Err(Error::NotFound.into());
        }
        Ok(row)
    }
    pub(crate) fn templates(
        &self,
        assistant: &str,
        after: &str,
        limit: usize,
    ) -> anyhow::Result<Page<Schedule>> {
        let conn = self.connection()?;
        let mut data = Vec::new();
        let mut next_cursor = None;
        if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? >= 2 {
            let mut q=conn.prepare("SELECT id FROM schedules WHERE json_extract(body,'$.assistantTemplate.assistantId')=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
            for id in q.query_map(params![assistant, after, limit as i64 + 1], |r| {
                r.get::<_, String>(0)
            })? {
                let id = id?;
                if data.len() == limit {
                    next_cursor = data.last().map(|r: &Schedule| r.id.clone());
                    break;
                }
                data.push(read_schedule(&conn, &id)?);
            }
        }
        Ok(Page { data, next_cursor })
    }
    pub(crate) fn template_history(
        &self,
        assistant: &str,
        id: &str,
        before: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        self.template(assistant, id)?;
        let conn = self.connection()?;
        let mut q=conn.prepare("SELECT seq,body FROM schedule_template_events WHERE schedule_id=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3")?;
        let rows = q.query_map(params![id, before, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (seq, body) = row?;
            let mut event: serde_json::Value = serde_json::from_str(&body)?;
            event["seq"] = seq.into();
            Ok(event)
        })
        .collect()
    }
    // Lock order is the project directory, this IMMEDIATE schedules transaction, then the
    // assistant inbox transaction in admit. Keep the seat and lease stable through the inbox
    // commit. A restart between the two commits recovers the inbox receipt by occurrence ID.
    // Never take the directory or schedules writer lock from inside an inbox transaction.
    pub(crate) fn with_template_run<R>(
        &self,
        run: &Run,
        admit: impl FnOnce(Option<&Template>) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease = read_run(&tx, run.sequence)?;
        let row = read_schedule(&tx, &run.schedule_id)?;
        let current = row.assistant_template.as_ref().filter(|t| {
            row.enabled
                && row.deleted_at.is_none()
                && row.revision == run.schedule_revision
                && lease.outcome == "dispatching"
                && lease.lease_token == run.lease_token
                && lease.id == run.id
                && run.assistant_template.as_ref() == Some(t)
        });
        let result = admit(current)?;
        tx.commit()?;
        Ok(result)
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
            let open:Option<i64>=tx.query_row("SELECT json_extract(body,'$.scheduleRevision') FROM schedule_runs WHERE schedule_id=?1 AND outcome IN('pending','held','retrying','dispatching')",[&id],|r|r.get(0)).optional()?;
            if row.assistant_template.is_some()
                && open.is_some_and(|revision| u64::try_from(revision).ok() != Some(row.revision))
            {
                // Reconcile the old receipt before consuming the replacement's first slot.
                continue;
            }
            let next = row.rule.next_after(zone, row.created_at, now)?;
            tx.execute(
                "UPDATE schedules SET next_due=?1 WHERE id=?2",
                params![next.timestamp_millis(), id],
            )?;
            if open.is_some() && row.assistant_template.is_some() {
                tx.execute("UPDATE schedule_runs SET body=json_set(body,'$.coalescedUntil',?2) WHERE schedule_id=?1 AND outcome IN('pending','held','retrying','dispatching')",
                    params![id,now.to_rfc3339()])?;
            }
            if open.is_none() {
                let occurrence_revision = row
                    .assistant_template
                    .as_ref()
                    .map_or(row.revision, |t| t.template_revision);
                let mut run = Run {
                    id: format!(
                        "schedule-run-{}",
                        crate::asks::sha256_hex(&serde_json::to_vec(&(
                            &id,
                            occurrence_revision,
                            due
                        ))?)
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
                    request_id: None,
                    coalesced_until: if row.assistant_template.is_some()
                        && row.rule.next_after(zone, row.created_at, due)? <= now
                    {
                        Some(now)
                    } else {
                        None
                    },
                    slot_local: row
                        .assistant_template
                        .as_ref()
                        .map(|_| due.with_timezone(zone).fixed_offset().to_rfc3339()),
                    assistant_template: row.assistant_template,
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
        receipt_id: Option<String>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        ensure!(
            ["held", "retrying", "refused", "handedOff", "cancelled"].contains(&outcome),
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
        if outcome == "refused" {
            if let (Some(template), Some(reason @ ("confirmation_stale" | "director_changed"))) =
                (&current.assistant_template, current.last_error.as_deref())
            {
                let inserted=tx.execute("INSERT OR IGNORE INTO schedule_template_reviews(schedule_id,revision,session,reason) VALUES(?1,?2,?3,?4)",
                    params![current.schedule_id,i64::try_from(template.template_revision)?,current.target_session_id,reason])?;
                if inserted != 0 {
                    let event = serde_json::json!({"actor":"system","verb":"reconfirmationRequired","at":now,
                        "templateRevision":template.template_revision,"reason":reason,"projectRef":template.project_ref,"occurrenceKey":current.id});
                    tx.execute(
                        "INSERT INTO schedule_template_events(schedule_id,body) VALUES(?1,?2)",
                        params![current.schedule_id, event.to_string()],
                    )?;
                    prune_template_events(&tx, &current.schedule_id, now)?;
                }
            }
        }
        if current.assistant_template.is_some() {
            current.request_id = receipt_id;
        } else {
            current.message_id = receipt_id;
        }
        current.finished_at = if matches!(outcome, "handedOff" | "refused" | "cancelled") {
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
    pub(crate) fn deliver_template_reviews(
        &self,
        mut deliver: impl FnMut(&crate::owner_notify::store::TemplateReview, &str) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let conn = self.connection()?;
        if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? < 2 {
            return Ok(());
        }
        let pending = {
            let mut q=conn.prepare("SELECT n.schedule_id,n.revision,n.session,n.reason,json_extract(s.body,'$.assistantTemplate.assistantId')
                FROM schedule_template_reviews n JOIN schedules s ON s.id=n.schedule_id WHERE n.delivered=0 ORDER BY n.schedule_id,n.revision LIMIT 16")?;
            let rows = q.query_map([], |r| {
                Ok((
                    crate::owner_notify::store::TemplateReview {
                        schedule_id: r.get(0)?,
                        template_revision: r.get(1)?,
                        reason: r.get(3)?,
                        assistant_id: r.get(4)?,
                    },
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (review, session) in pending {
            deliver(&review, &session)?;
            conn.execute("UPDATE schedule_template_reviews SET delivered=1 WHERE schedule_id=?1 AND revision=?2",params![review.schedule_id,review.template_revision])?;
        }
        Ok(())
    }

    pub(crate) fn maintain_templates(
        &self,
        after: &str,
        now: DateTime<Utc>,
        mut prune_requests: impl FnMut(&str, &str) -> anyhow::Result<()>,
    ) -> anyhow::Result<String> {
        let mut conn = self.connection()?;
        if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? < 2 {
            return Ok(String::new());
        }
        let entries = {
            let mut q = conn.prepare(
                "SELECT id,json_extract(body,'$.assistantTemplate.assistantId') FROM schedules WHERE id>?1
                 AND json_type(body,'$.assistantTemplate')='object' ORDER BY id LIMIT 8",
            )?;
            let rows = q.query_map([after], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (schedule, assistant) in &entries {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            prune_template_events(&tx, schedule, now)?;
            tx.execute("DELETE FROM schedule_template_reviews WHERE rowid IN (
                SELECT rowid FROM schedule_template_reviews WHERE schedule_id=?1 AND delivered=1
                AND revision < (SELECT revision FROM schedules WHERE id=?1) ORDER BY revision LIMIT ?2)",
                params![schedule,super::PRUNE_BATCH])?;
            tx.commit()?;
            prune_requests(assistant, schedule)?;
        }
        Ok(if entries.len() == 8 {
            entries.last().unwrap().0.clone()
        } else {
            String::new()
        })
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
fn enable_templates(tx: &Transaction<'_>) -> anyhow::Result<()> {
    if tx.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? == 1 {
        tx.execute_batch("CREATE INDEX schedules_by_assistant ON schedules(json_extract(body,'$.assistantTemplate.assistantId'),id);
            CREATE TABLE schedule_template_events(seq INTEGER PRIMARY KEY, schedule_id TEXT NOT NULL REFERENCES schedules(id), body TEXT NOT NULL);
            CREATE INDEX schedule_template_history ON schedule_template_events(schedule_id,seq DESC);
            PRAGMA user_version=2;")?;
        ensure_template_maintenance(tx)?;
    }
    Ok(())
}
fn ensure_template_maintenance(tx: &Transaction<'_>) -> anyhow::Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS schedule_template_reviews(
        schedule_id TEXT NOT NULL REFERENCES schedules(id),revision INTEGER NOT NULL,session TEXT NOT NULL,reason TEXT NOT NULL,
        delivered INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(schedule_id,revision));
        CREATE INDEX IF NOT EXISTS schedule_reviews_pending ON schedule_template_reviews(delivered,schedule_id,revision) WHERE delivered=0;
        CREATE INDEX IF NOT EXISTS schedule_template_maintenance ON schedules(id) WHERE json_type(body,'$.assistantTemplate')='object';
        CREATE UNIQUE INDEX IF NOT EXISTS schedule_retention_summary ON schedule_template_events(schedule_id) WHERE json_extract(body,'$.verb')='historyPruned';")?;
    Ok(())
}
fn log_template(
    tx: &Transaction<'_>,
    action: &str,
    verb: &str,
    before: Option<&Schedule>,
    after: &Schedule,
) -> anyhow::Result<()> {
    if after.assistant_template.is_some() {
        let view = |row: &Schedule| serde_json::json!({"template":row.assistant_template,"rule":row.rule,"revision":row.revision,"enabled":row.enabled,"deletedAt":row.deleted_at});
        let event = serde_json::json!({"actor":"owner","actionId":action,"verb":verb,"at":after.updated_at,
            "before":before.map(view),"after":view(after)});
        tx.execute(
            "INSERT INTO schedule_template_events(schedule_id,body) VALUES(?1,?2)",
            params![after.id, serde_json::to_string(&event)?],
        )?;
        prune_template_events(tx, &after.id, after.updated_at)?;
    }
    Ok(())
}
fn prune_template_events(
    tx: &Transaction<'_>,
    schedule: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let count=tx.execute("DELETE FROM schedule_template_events WHERE seq IN (
        SELECT seq FROM schedule_template_events WHERE schedule_id=?1 AND json_extract(body,'$.verb')<>'historyPruned'
        ORDER BY seq DESC LIMIT ?2 OFFSET ?3)",params![schedule,super::PRUNE_BATCH,super::TEMPLATE_HISTORY_KEEP])?;
    if count != 0 {
        let prior:Option<i64>=tx.query_row("DELETE FROM schedule_template_events WHERE schedule_id=?1 AND json_extract(body,'$.verb')='historyPruned'
            RETURNING json_extract(body,'$.deletedTotal')",[schedule],|r|r.get(0)).optional()?;
        let body = serde_json::json!({"actor":"system","verb":"historyPruned","at":now,"deleted":count,
            "deletedTotal":prior.unwrap_or(0)+i64::try_from(count)?,"keep":super::TEMPLATE_HISTORY_KEEP});
        tx.execute(
            "INSERT INTO schedule_template_events(schedule_id,body) VALUES(?1,?2)",
            params![schedule, body.to_string()],
        )?;
    }
    Ok(())
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
        // A recovered attempt may already have committed its inbox receipt.
        // Leave it eligible for reconciliation before declaring it cancelled.
        if run.assistant_template.is_some() && run.attempts > 0 {
            run.next_attempt_at = now;
            return save_run(tx, &run, None);
        }
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
