use super::*;
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(super) const DB: &str = "assistants-v1.sqlite3";
const SCHEMA: &str = include_str!("schema.sql");
const CLOSED_SCHEDULE_INDEX: &str="CREATE INDEX IF NOT EXISTS pa_closed_schedule
    ON pa_requests(assistant,json_extract(body,'$.occurrence.scheduleId'),seq)
    WHERE state IN ('completed','declined','cancelled') AND json_extract(body,'$.occurrence.scheduleId') IS NOT NULL;";
const SCHEMA_V2: &str = include_str!("schema-v2.sql");
const SCHEMA_V3: &str = include_str!("schema-v3.sql");
pub(super) const HISTORY_LIMIT: i64 = 512;
const KEY_PAGE_SIZE: usize = 100;

// All three maintenance cursors use immutable, nonempty IDs. Finish reading
// each page before callbacks can delete work or open another write transaction.
fn visit_keys(
    conn: &Connection,
    sql: &str,
    mut visit: impl FnMut(String) -> Result<()>,
) -> Result<()> {
    let mut q = conn.prepare(sql)?;
    let mut after = String::new();
    loop {
        let keys = q
            .query_map(params![after, KEY_PAGE_SIZE as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let last_page = keys.len() < KEY_PAGE_SIZE;
        if let Some(last) = keys.last() {
            after.clone_from(last);
        }
        for key in keys {
            visit(key)?;
        }
        if last_page {
            return Ok(());
        }
    }
}

// Keep the generated fields of the initial prepare reply with its request.
// The retry supplies the original draft, checked against its digest. Unlike
// revision-checked edits, preparing and creating have no revision to prevent
// an old client retry from creating a second object. Create and bind each
// occupy at most one permanent receipt per assistant.
fn trim_history(tx: &Transaction<'_>, assistant: &str, owner_action: bool) -> Result<()> {
    let cutoff: Option<i64> = tx
        .query_row(
            "SELECT seq FROM pa_audit WHERE assistant=?1 AND owner_action=?2 ORDER BY seq DESC LIMIT 1 OFFSET ?3",
            params![assistant, owner_action, HISTORY_LIMIT],
            |r| r.get(0),
        )
        .optional()?;
    let Some(cutoff) = cutoff else { return Ok(()) };
    tx.execute(
        "UPDATE pa_requests SET prepare_actor=p.actor, prepare_action=p.action,
             prepare_digest=p.digest,
             prepare_origin=json_remove(p.reply,'$.projectId','$.outcome','$.constraints','$.sourceRef')
         FROM pa_audit a JOIN pa_actions p ON (p.actor,p.action)=(a.actor,a.action)
         WHERE a.assistant=?1 AND a.seq<=?2 AND a.owner_action=?3 AND a.verb='prepare'
             AND pa_requests.id=json_extract(a.detail,'$.requestId')",
        params![assistant, cutoff, owner_action],
    )?;
    tx.execute(
        "DELETE FROM pa_actions WHERE (actor,action) IN
             (SELECT actor,action FROM pa_audit WHERE assistant=?1 AND seq<=?2
                 AND owner_action=?3 AND verb NOT IN ('create','bind'))",
        params![assistant, cutoff, owner_action],
    )?;
    tx.execute(
        "DELETE FROM pa_audit WHERE assistant=?1 AND seq<=?2 AND owner_action=?3",
        params![assistant, cutoff, owner_action],
    )?;
    Ok(())
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    publish_host_events: bool,
}
impl Store {
    pub fn default_root() -> Result<PathBuf> {
        Ok(crate::config::paths::runtime_dir()?.join("personal-assistants"))
    }
    pub fn open_default() -> Result<Self> {
        let mut store = Self::open(&Self::default_root()?)?;
        store.publish_host_events = true;
        Ok(store)
    }
    pub fn open(root: &Path) -> Result<Self> {
        crate::management::client_tokens::ensure_private_dir(root)?;
        let store = Self {
            root: root.into(),
            publish_host_events: false,
        };
        let mut conn = store.connection()?;
        if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? == 4 {
            if !conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='pa_closed_schedule')",
                [],
                |r| r.get::<_, bool>(0),
            )? {
                conn.execute_batch(CLOSED_SCHEDULE_INDEX)?;
            }
            return Ok(store);
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= 4,
            "personal assistant state needs a newer daemon"
        );
        if version == 0 {
            tx.execute_batch(SCHEMA)?;
        }
        if version < 2 {
            tx.execute_batch(SCHEMA_V2)?;
        }
        if version < 3 {
            tx.execute_batch(SCHEMA_V3)?;
            visit_keys(
                &tx,
                "SELECT id FROM pa_assistants WHERE id>?1 ORDER BY id LIMIT ?2",
                |id| {
                    trim_history(&tx, &id, false)?;
                    trim_history(&tx, &id, true)
                },
            )?;
        }
        if version < 4 {
            tx.execute_batch(include_str!("schema-v4.sql"))?;
        }
        tx.execute_batch(CLOSED_SCHEDULE_INDEX)?;
        tx.commit()?;
        Ok(store)
    }
    pub fn present() -> Result<bool> {
        Ok(Self::default_root()?.join(DB).try_exists()?)
    }
    pub(crate) fn prune_closed_schedule(&self, assistant: &str, schedule: &str) -> Result<usize> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count = Self::prune_closed_in(&tx, assistant, schedule)?;
        tx.commit()?;
        if count > 0 && self.publish_host_events {
            crate::management::v2::documents::publish_host_event(
                super::CHANGED_EVENT,
                json!({"assistantId":assistant,"areas":["requests","audit"]}),
            );
        }
        Ok(count)
    }
    pub(super) fn prune_closed_in(
        tx: &Transaction<'_>,
        assistant: &str,
        schedule: &str,
    ) -> Result<usize> {
        // A schedule has at most one unacknowledged occurrence. Retaining the newest closed
        // admissions also retains that receipt if the scheduler crashed after inbox commit.
        let count=tx.execute("DELETE FROM pa_requests WHERE seq IN (
            SELECT seq FROM pa_requests WHERE assistant=?1 AND json_extract(body,'$.occurrence.scheduleId')=?2
            AND state IN ('completed','declined','cancelled') ORDER BY seq DESC LIMIT ?3 OFFSET ?4)",
            params![assistant,schedule,crate::schedules::PRUNE_BATCH,crate::schedules::TEMPLATE_HISTORY_KEEP])?;
        if count > 0 {
            tx.execute("INSERT INTO pa_audit(assistant,actor,action,verb,at,detail) VALUES(?1,'system',?2,'pruneScheduledRequests',?3,?4)",
                params![assistant,id("prune"),crate::management::instance::now_utc(),
                    json!({"scheduleId":schedule,"deleted":count,"keep":crate::schedules::TEMPLATE_HISTORY_KEEP}).to_string()])?;
            trim_history(tx, assistant, true)?;
        }
        Ok(count)
    }
    fn read_only(root: &Path) -> Result<Connection> {
        let conn =
            Connection::open_with_flags(root.join(DB), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::ZERO)?;
        Ok(conn)
    }
    pub(super) fn visit_bound_sessions(root: &Path, mut visit: impl FnMut(String)) -> Result<()> {
        let mut conn = Self::read_only(root)?;
        let snapshot = conn.transaction()?;
        visit_keys(
            &snapshot,
            "SELECT session FROM pa_assistants WHERE session>?1 ORDER BY session LIMIT ?2",
            |session| {
                visit(session);
                Ok(())
            },
        )?;
        snapshot.commit()?;
        Ok(())
    }

    pub(super) fn reminders_disabled(session: &str) -> Result<bool> {
        let conn = Self::read_only(&Self::default_root()?)?;
        Ok(!Self::by_session(&conn, session)?.enabled)
    }
    pub(super) fn connection(&self) -> Result<Connection> {
        let conn = Connection::open(self.root.join(DB))?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(conn)
    }
    pub(super) fn action<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        actor: &str,
        action: &str,
        verb: &str,
        input: &impl Serialize,
        change: impl FnOnce(&Transaction<'_>) -> Result<(String, T, Value)>,
    ) -> Result<T> {
        self.guarded_action(actor, action, verb, input, |_| Ok(()), change)
    }
    pub(super) fn guarded_action<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        actor: &str,
        action: &str,
        verb: &str,
        input: &impl Serialize,
        authorize: impl FnOnce(&Transaction<'_>) -> Result<()>,
        change: impl FnOnce(&Transaction<'_>) -> Result<(String, T, Value)>,
    ) -> Result<T> {
        bounded(action, 160)?;
        let digest = crate::approvals::request_digest(&json!([verb, input]));
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize(&tx)?;
        let prior = tx
            .query_row(
                "SELECT digest,reply,0 FROM pa_actions WHERE actor=?1 AND action=?2
                 UNION ALL
                 SELECT prepare_digest,prepare_origin,1 FROM pa_requests WHERE prepare_actor=?1 AND prepare_action=?2
                 LIMIT 1",
                params![actor, action],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((prior_digest, reply, compact_prepare)) = prior {
            require(digest == prior_digest, "action_conflict")?;
            let mut value = if compact_prepare {
                let mut draft = serde_json::to_value(input)?;
                draft
                    .as_object_mut()
                    .context("expected the original Director request draft")?
                    .extend(serde_json::from_str::<serde_json::Map<String, Value>>(
                        &reply,
                    )?);
                draft
            } else {
                serde_json::from_str(&reply)?
            };
            // Qualify historic local receipts at read time, preserving the recorded consent and digest.
            if let (Some(id), Some(project)) =
                (value["assistantId"].as_str(), value["projectId"].as_str())
            {
                if value["projectRef"].is_null() {
                    value["projectRef"] = json!(Self::read(&tx, id)?.config.project_ref(project));
                }
            }
            if let (Some(home), Some(project)) = (
                value
                    .pointer("/config/homeInstanceId")
                    .and_then(Value::as_str),
                value["coordinationProjectId"].as_str(),
            ) {
                if value["coordinationProjectRef"].is_null() {
                    value["coordinationProjectRef"] =
                        json!({"instanceId":home,"projectId":project});
                }
            }
            return Ok(serde_json::from_value(value)?);
        }
        let (assistant, reply, details) = change(&tx)?;
        tx.execute(
            "INSERT INTO pa_actions(actor,action,digest,reply) VALUES (?1,?2,?3,?4)",
            params![actor, action, digest, serde_json::to_string(&reply)?],
        )?;
        let owner_action = tx.query_row(
            "INSERT INTO pa_audit(assistant,actor,action,verb,at,detail) VALUES (?1,?2,?3,?4,?5,?6) RETURNING owner_action",
            params![assistant,actor,action,verb,crate::management::instance::now_utc(),serde_json::to_string(&details)?],
            |r| r.get(0),
        )?;
        trim_history(&tx, &assistant, owner_action)?;
        tx.commit()?;
        if self.publish_host_events {
            // Only new commits invalidate owner views. Event replay carries identifiers only.
            let area = match verb {
                "create" | "bind" | "configure" => "assistant",
                "memoryWrite" => "notes",
                "calendarSnapshot" => "calendar",
                _ => "requests",
            };
            let mut event = json!({"assistantId":assistant, "areas":[area,"audit"]});
            if let Some(request) = details.get("requestId") {
                event["requestId"] = request.clone();
            }
            crate::management::v2::documents::publish_host_event(super::CHANGED_EVENT, event);
        }
        Ok(reply)
    }
    pub(super) fn read(conn: &Connection, id: &str) -> Result<Assistant> {
        let data: String = conn
            .query_row("SELECT body FROM pa_assistants WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| fault("assistant_not_found"))?;
        Ok(serde_json::from_str(&data)?)
    }
    fn save(tx: &Transaction<'_>, row: &Assistant) -> Result<()> {
        tx.execute(
            "INSERT INTO pa_assistants(id,session,body) VALUES(?1,?2,?3)
            ON CONFLICT(id) DO UPDATE SET session=excluded.session, body=excluded.body",
            params![row.id, row.session_id, serde_json::to_string(row)?],
        )?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Assistant> {
        Self::read(&self.connection()?, id)
    }
    pub fn list(&self, after: &str, limit: usize) -> Result<Vec<Assistant>> {
        require((1..=100).contains(&limit), "invalid_limit")?;
        let conn = self.connection()?;
        let mut q =
            conn.prepare("SELECT body FROM pa_assistants WHERE id>?1 ORDER BY id LIMIT ?2")?;
        let rows = q.query_map(params![after, limit as i64], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub(super) fn by_session(conn: &Connection, session: &str) -> Result<Assistant> {
        let id: String = conn
            .query_row(
                "SELECT id FROM pa_assistants WHERE session=?1",
                [session],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| fault("assistant_required"))?;
        Self::read(conn, &id)
    }
    pub fn for_session(&self, session: &str) -> Result<Assistant> {
        Self::by_session(&self.connection()?, session)
    }
    /// Serialize a self-reminder mutation with owner revocation. The alarm
    /// store uses its own stable alarm ID for retry after an interrupted call.
    pub(super) fn while_active<T>(
        &self,
        session: &str,
        write: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::by_session(&tx, session)?.require_active()?;
        let result = write()?;
        tx.commit()?;
        Ok(result)
    }
    pub fn begin(
        &self,
        actor: &str,
        action: &str,
        cfg: Config,
        validate: impl FnOnce(&Config) -> Result<()>,
    ) -> Result<Assistant> {
        cfg.validate()?;
        // Stable r295 creation digest. The qualified local IDs are derived from these fields.
        let original = json!({"name":cfg.name,"profile":cfg.profile,"homeInstanceId":cfg.home_instance_id,"projectIds":cfg.project_ids});
        self.action(actor, action, "create", &original, |tx| {
            validate(&cfg)?;
            let id = id("pa");
            let coordination_project_id = format!("{id}-home");
            let row = Assistant {
                coordination_project_ref: Some(cfg.project_ref(&coordination_project_id)),
                coordination_project_id,
                id: id.clone(),
                revision: 1,
                config: cfg.clone(),
                grants: Grants::default(),
                scope_revisions: Vec::new(),
                session_id: None,
                enabled: true,
            };
            Self::save(tx, &row)?;
            Ok((id, row, json!({"state":"provisioning"})))
        })
    }
    pub fn bind(&self, id: &str, session: &str) -> Result<Assistant> {
        bounded(session, 180)?;
        let row = self.action(
            "system",
            &format!("bind-{id}"),
            "bind",
            &(id, session),
            |tx| {
                let mut row = Self::read(tx, id)?;
                require(row.session_id.is_none() && row.enabled, "binding_conflict")?;
                let taken: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pa_assistants WHERE session=?1)",
                    [session],
                    |r| r.get(0),
                )?;
                require(!taken, "binding_conflict")?;
                row.session_id = Some(session.into());
                row.revision += 1;
                Self::save(tx, &row)?;
                Ok((id.into(), row, json!({"sessionId":session})))
            },
        )?;
        super::bindings::remember(&self.root, session);
        Ok(row)
    }
    pub fn provisioning_lock(&self, id: &str) -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(format!("provision-{id}.lock")))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
            {
                return Err(fault("provisioning_in_progress"));
            }
            Err(e) => return Err(e.into()),
        }
        Ok(file)
    }
    pub fn configure(
        &self,
        actor: &str,
        id: &str,
        action: &str,
        revision: u64,
        change: &Update,
        validate: impl FnOnce(&Config) -> Result<()>,
    ) -> Result<Assistant> {
        if matches!(change, Update::Grants { grants } if grants.full_owner) {
            return Err(fault("full_owner_native_confirmation_required"));
        }
        let row = self.action(actor, action, "configure", &(id, revision, change), |tx| {
            let mut row = Self::read(tx, id)?;
            require(row.revision == revision, "revision_conflict")?;
            require(
                row.session_id.is_some() || matches!(change, Update::Enabled { .. }),
                "provisioning_in_progress",
            )?;
            let prior = row.clone();
            change.apply(&mut row)?;
            row.config.validate()?;
            validate(&row.config)?;
            if let Some(session) = row.session_id.as_deref() {
                if prior.enabled && !row.enabled {
                    tx.execute(
                        "INSERT OR IGNORE INTO pa_reminder_cancellations(session) VALUES(?1)",
                        [session],
                    )?;
                } else if !prior.enabled && row.enabled {
                    Self::finish_reminder_cancellation(tx, session)?;
                }
            }
            row.revision += 1;
            row.track_scope_change(&prior);
            Self::save(tx, &row)?;
            Ok((id.into(), row.clone(), json!({"before":prior,"after":row})))
        })?;
        if matches!(change, Update::Enabled { enabled: false }) {
            if let Some(session) = row.session_id.as_deref() {
                // Disable is durable even if the alarm file is busy. The worker
                // retries the intent, and re-enable consumes it before new writes.
                if let Err(e) = self.cancel_reminders_for(session) {
                    eprintln!(
                        "Personal Assistant reminder cancellation pending for {session}: {e}"
                    );
                }
            }
        }
        Ok(row)
    }

    fn finish_reminder_cancellation(tx: &Transaction<'_>, session: &str) -> Result<()> {
        if tx.execute(
            "DELETE FROM pa_reminder_cancellations WHERE session=?1",
            [session],
        )? != 0
        {
            crate::agent_bus::alarms::cancel_pending_self_reminders(session)?;
        }
        Ok(())
    }

    fn cancel_reminders_for(&self, session: &str) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::finish_reminder_cancellation(&tx, session)?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn retry_reminder_cancellations(&self) -> Result<()> {
        let conn = self.connection()?;
        visit_keys(
            &conn,
            "SELECT session FROM pa_reminder_cancellations WHERE session>?1 ORDER BY session LIMIT ?2",
            |session| self.cancel_reminders_for(&session),
        )
    }
    pub fn replace_calendar(
        &self,
        id: &str,
        action: &str,
        revision: u64,
        snapshot: Option<&calendar::Snapshot>,
    ) -> Result<Value> {
        if let Some(s) = snapshot {
            s.validate()?;
        }
        self.action("owner", action, "calendarSnapshot", &(id, revision, snapshot), |tx| {
            Self::read(tx, id)?;
            let prior = calendar_info(tx, id)?;
            require(prior["revision"] == revision, "revision_conflict")?;
            let next = revision.checked_add(1).ok_or_else(|| fault("revision_conflict"))?;
            tx.execute(
                "INSERT INTO pa_calendar_sources(assistant,revision,source,observed_at) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(assistant) DO UPDATE SET revision=excluded.revision,
                    source=excluded.source,observed_at=excluded.observed_at",
                params![id, i64::try_from(next)?, snapshot.map(|s| &s.source), snapshot.map(|s| s.observed_at.to_rfc3339())],
            )?;
            tx.execute("DELETE FROM pa_calendar_events WHERE assistant=?1", [id])?;
            if let Some(snapshot) = snapshot {
                let mut insert = tx.prepare(
                    "INSERT INTO pa_calendar_events(assistant,event_id,start_ms,end_ms,body) VALUES(?1,?2,?3,?4,?5)",
                )?;
                for event in &snapshot.events {
                    insert.execute(params![id, event.id, event.starts_at.timestamp_millis(),
                        event.ends_at.timestamp_millis(), serde_json::to_string(event)?])?;
                }
            }
            let current = calendar_info(tx, id)?;
            let audit = json!({"before":prior,"after":current,"eventCount":snapshot.map_or(0,|s|s.events.len())});
            Ok((id.into(), current, audit))
        })
    }
    pub fn calendar_info(&self, id: &str) -> Result<Value> {
        let conn = self.connection()?;
        Self::read(&conn, id)?;
        calendar_info(&conn, id)
    }
    pub(super) fn calendar_day(
        &self,
        id: &str,
        start: i64,
        end: i64,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut source = calendar_info(&tx, id)?;
        let revision = source["revision"].as_u64().context("calendar revision")?;
        let (after_ms, after_id) = match after {
            None => (i64::MIN, ""),
            Some(value) => {
                let mut parts = value.splitn(3, '|');
                let rev = parts
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| fault("invalid_cursor"))?;
                require(rev == revision, "calendar_changed")?;
                (
                    parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| fault("invalid_cursor"))?,
                    parts.next().ok_or_else(|| fault("invalid_cursor"))?,
                )
            }
        };
        let mut q=tx.prepare("SELECT start_ms,event_id,body FROM pa_calendar_events WHERE assistant=?1 AND start_ms<?2 AND end_ms>?3
            AND (start_ms,event_id)>(?4,?5) ORDER BY start_ms,event_id LIMIT ?6")?;
        let mut rows = q.query(params![
            id,
            end,
            start,
            after_ms,
            after_id,
            limit as i64 + 1
        ])?;
        let mut items = Vec::new();
        let mut last = String::new();
        let mut next = None;
        while let Some(r) = rows.next()? {
            if items.len() == limit {
                next = Some(last);
                break;
            }
            last = format!(
                "{}|{}|{}",
                revision,
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?
            );
            items.push(serde_json::from_str::<calendar::Event>(
                &r.get::<_, String>(2)?,
            )?);
        }
        source["items"] = json!(items);
        source["nextCursor"] = json!(next);
        Ok(source)
    }
    pub fn overlap(&self, row: &Assistant) -> Result<Vec<Value>> {
        let conn = self.connection()?;
        let mut query = conn.prepare(
            "SELECT a.id, json_extract(a.body,'$.config.name'), p.value
             FROM pa_assistants AS a, json_each(a.body,'$.config.projectIds') AS p
             WHERE a.id<>?1 AND json_extract(a.body,'$.enabled')=1 AND json_extract(a.body,'$.config.homeInstanceId')=?3
               AND p.value IN (SELECT value FROM json_each(?2)) ORDER BY a.id,p.value LIMIT 101",
        )?;
        let rows = query.query_map(params![row.id,serde_json::to_string(&row.config.project_ids)?,row.config.home_instance_id],
            |r| { let project=r.get::<_,String>(2)?; Ok(json!({"assistantId":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"projectId":project,"projectRef":row.config.project_ref(&project)})) })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub fn audit(&self, id: &str, before: i64, limit: usize) -> Result<Vec<Value>> {
        require((1..=100).contains(&limit), "invalid_limit")?;
        let conn = self.connection()?;
        let mut q = conn.prepare("SELECT seq,actor,action,verb,at,detail FROM pa_audit WHERE assistant=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3")?;
        let rows = q.query_map(params![id, before, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        rows.map(|r| {
            let (seq, actor, action, verb, at, data) = r?;
            Ok(json!({"seq":seq,"actor":actor,"actionId":action,
            "verb":verb,"at":at,"detail":serde_json::from_str::<Value>(&data)?}))
        })
        .collect()
    }
    pub fn deliver_notices(&self, send: impl FnMut(&str, &str, &str) -> Result<()>) -> Result<()> {
        self.deliver_notices_at(chrono::Utc::now().timestamp(), send)
    }

    pub(super) fn deliver_notices_at(
        &self,
        now: i64,
        mut send: impl FnMut(&str, &str, &str) -> Result<()>,
    ) -> Result<()> {
        let conn = self.connection()?;
        let rows = {
            let mut query =
                conn.prepare("SELECT id,recipient,message,attempts FROM pa_notices WHERE retry_at<=?1 ORDER BY retry_at,seq LIMIT 32")?;
            let mut rows = query.query([now])?;
            let mut page = Vec::<(String, String, String, u32)>::new();
            while let Some(r) = rows.next()? {
                page.push((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?));
            }
            page
        };
        for (id, recipient, message, attempts) in rows {
            // set_system_notice deduplicates by this durable transition id, including after a crash.
            match send(&id, &recipient, &message) {
                Ok(()) => {
                    conn.execute("DELETE FROM pa_notices WHERE id=?1", [id])?;
                }
                Err(e) => {
                    if host::notice_recipient_gone(&recipient, u64::from(attempts) + 1) {
                        if conn.execute(
                            "DELETE FROM pa_notices WHERE id=?1 AND attempts=?2",
                            params![id, attempts],
                        )? == 1
                        {
                            eprintln!("Dropped Personal Assistant notice {id} after repeated delivery failure: recipient {recipient} no longer exists");
                        }
                        continue;
                    }
                    conn.execute(
                        "UPDATE pa_notices SET retry_at=?2+min(3600,5*(1<<min(attempts,10))),
                             attempts=min(attempts+1,10) WHERE id=?1 AND attempts=?3",
                        params![id, now, attempts],
                    )?;
                    eprintln!("Personal Assistant notice {id} deferred: {e}");
                }
            }
        }
        Ok(())
    }
}

fn calendar_info(conn: &Connection, id: &str) -> Result<Value> {
    let (rev, source, at) = conn
        .query_row(
            "SELECT revision,source,observed_at FROM pa_calendar_sources WHERE assistant=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
        .unwrap_or_default();
    Ok(
        json!({"revision":rev,"status":if source.is_some() {"ready"} else {"disconnected"},"source":source,"observedAt":at}),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Update {
    Settings {
        name: String,
        #[serde(rename = "projectIds")]
        project_ids: BTreeSet<String>,
    },
    ScopedSettings {
        name: String,
        #[serde(rename = "projectRefs")]
        project_refs: BTreeSet<ProjectRef>,
    },
    Grants {
        grants: Grants,
    },
    Enabled {
        enabled: bool,
    },
}
impl Update {
    fn apply(&self, row: &mut Assistant) -> Result<()> {
        match self {
            Self::Settings { name, project_ids } => {
                row.config.name.clone_from(name);
                row.config.project_ids.clone_from(project_ids);
            }
            Self::ScopedSettings { name, project_refs } => {
                require(
                    project_refs
                        .iter()
                        .all(|r| r.instance_id == row.config.home_instance_id),
                    "destination_scope_required",
                )?;
                row.config.name.clone_from(name);
                row.config.project_ids =
                    project_refs.iter().map(|r| r.project_id.clone()).collect();
            }
            Self::Grants { grants } => row.grants.clone_from(grants),
            Self::Enabled { enabled } => row.enabled = *enabled,
        }
        Ok(())
    }
}
