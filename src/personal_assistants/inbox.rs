use super::*;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

pub(super) const MEMORY_KEYS: i64 = 1000;
pub(super) const OPEN_REQUESTS: i64 = 50;

pub(super) struct DirectorInboxItem {
    pub(super) request: DirectorRequest,
    pub(super) clarification_full: bool,
}

const DIRECTOR_REQUESTS: &str =
    "SELECT admission_seq,body FROM pa_requests WHERE director=?1 AND state NOT IN ('awaitingOwner','cancelled')
     AND admission_seq>?2 ORDER BY admission_seq LIMIT ?3";

fn request_page(
    conn: &Connection,
    sql: &str,
    scope: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<(i64, DirectorRequest)>> {
    let mut q = conn.prepare(sql)?;
    let rows = q.query_map(params![scope, after, limit as i64], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|r| {
        let (seq, data) = r?;
        Ok((seq, serde_json::from_str(&data)?))
    })
    .collect()
}

fn open_requests_full(conn: &Connection, assistant: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pa_requests WHERE assistant=?1 AND state IN ('awaitingOwner','clarification') LIMIT 1 OFFSET ?2)",
        params![assistant, OPEN_REQUESTS - 1], |r| r.get(0),
    )?)
}

fn require_open_slot(tx: &Transaction<'_>, assistant: &str) -> Result<()> {
    require(!open_requests_full(tx, assistant)?, "open_request_limit")
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Draft {
    pub project_id: String,
    pub outcome: String,
    #[serde(default)]
    pub constraints: String,
    pub source_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Response {
    Accept,
    Clarify,
    Decline,
    Progress,
    Report,
}

fn load(conn: &Connection, request: &str) -> Result<DirectorRequest> {
    let data: String = conn
        .query_row("SELECT body FROM pa_requests WHERE id=?1", [request], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| fault("request_not_found"))?;
    Ok(serde_json::from_str(&data)?)
}
fn save(tx: &Transaction<'_>, r: &DirectorRequest) -> Result<()> {
    tx.execute("INSERT INTO pa_requests(id,assistant,director,project,state,body,due_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(id) DO UPDATE SET director=excluded.director,state=excluded.state,body=excluded.body,due_ms=excluded.due_ms",
        params![r.id,r.assistant_id,r.director.session_id,r.project_id,serde_json::to_value(r.state)?.as_str(),serde_json::to_string(r)?,r.due_at.map(|dt|dt.timestamp_millis())])?;
    if let Some(occurrence) = &r.occurrence {
        if matches!(
            r.state,
            RequestState::Completed | RequestState::Declined | RequestState::Cancelled
        ) {
            Store::prune_closed_in(tx, &r.assistant_id, &occurrence.schedule_id)?;
        }
    }
    Ok(())
}
fn notice(tx: &Transaction<'_>, r: &DirectorRequest, recipient: &str, message: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO pa_notices(id,recipient,message,retry_at) VALUES(?1,?2,?3,?4)",
        params![
            format!("{}-{}", r.id, r.revision),
            recipient,
            message,
            chrono::Utc::now().timestamp()
        ],
    )?;
    Ok(())
}
fn updated(r: &mut DirectorRequest, state: RequestState, note: Option<&str>) {
    r.revision += 1;
    r.state = state;
    r.detail = note.map(str::to_owned);
    r.updated_at = crate::management::instance::now_utc();
}
fn enqueue(tx: &Transaction<'_>, r: &DirectorRequest, recipient: &str) -> Result<()> {
    save(tx, r)?;
    tx.execute("UPDATE pa_requests SET admission_seq=(SELECT coalesce(max(admission_seq),0)+1 FROM pa_requests) WHERE id=?1",[&r.id])?;
    notice(tx,r,recipient,"A Personal Assistant request is ready in your Director inbox. Read it with director_request_list.")
}

impl Store {
    pub fn prepare(
        &self,
        caller: &str,
        action: &str,
        draft: &Draft,
        seat: &Seat,
    ) -> Result<DirectorRequest> {
        self.for_session(caller)?.scoped(&draft.project_id)?;
        bounded(&draft.outcome, 8192)?;
        bounded(&draft.source_ref, 256)?;
        require(draft.constraints.len() <= 4096, "invalid_text")?;
        self.guarded_action(
            caller,
            action,
            "prepare",
            draft,
            |tx| Self::by_session(tx, caller)?.scoped(&draft.project_id),
            |tx| {
                let row = Self::by_session(tx, caller)?;
                require_open_slot(tx, &row.id)?;
                let r = DirectorRequest {
                    id: id("dr"),
                    assistant_id: row.id.clone(),
                    project_id: draft.project_id.clone(),
                    project_ref: Some(row.config.project_ref(&draft.project_id)),
                    due_at: draft.due_at,
                    occurrence: None,
                    outcome: draft.outcome.clone(),
                    constraints: draft.constraints.clone(),
                    source_ref: draft.source_ref.clone(),
                    director: seat.clone(),
                    assistant_revision: row.revision,
                    state: RequestState::AwaitingOwner,
                    revision: 1,
                    updated_at: crate::management::instance::now_utc(),
                    detail: None,
                };
                save(tx, &r)?;
                Ok((row.id, r.clone(), json!({"requestId":r.id})))
            },
        )
    }
    pub fn request(&self, id: &str) -> Result<DirectorRequest> {
        load(&self.connection()?, id)
    }
    pub fn confirm(
        &self,
        actor: &str,
        assistant: &str,
        request: &str,
        action: &str,
        revision: u64,
        assistant_revision: u64,
        reviewed: &Seat,
        current: &Seat,
    ) -> Result<DirectorRequest> {
        self.action(
            actor,
            action,
            "confirm",
            &(assistant, request, revision, assistant_revision, reviewed),
            |tx| {
                let mut r = load(tx, request)?;
                require(r.assistant_id == assistant, "request_not_found")?;
                require(r.revision == revision, "revision_conflict")?;
                let row = Self::read(tx, assistant)?;
                row.scoped(&r.project_id)?;
                require(row.revision == assistant_revision, "confirmation_stale")?;
                require(reviewed == current, "director_changed")?;
                let recipient = current
                    .session_id
                    .as_deref()
                    .ok_or_else(|| fault("director_missing"))?;
                let seat_changed = r.director != *reviewed;
                let needs_confirmation = match r.state {
                    RequestState::AwaitingOwner => true,
                    RequestState::Queued => {
                        seat_changed || r.assistant_revision != assistant_revision
                    }
                    RequestState::Accepted | RequestState::Clarification => seat_changed,
                    _ => false,
                };
                require(needs_confirmation, "invalid_transition")?;
                let prior = r.clone();
                r.director = current.clone();
                r.assistant_revision = assistant_revision;
                updated(&mut r, RequestState::Queued, prior.detail.as_deref());
                enqueue(tx, &r, recipient)?;
                Ok((
                    assistant.into(),
                    r.clone(),
                    json!({"requestId":r.id,"before":prior,"after":r}),
                ))
            },
        )
    }
    pub(super) fn occurrence_receipt(&self, run: &crate::schedules::Run) -> Result<Option<String>> {
        let id = format!("dr-{}", run.id);
        Ok(self
            .connection()?
            .query_row("SELECT id FROM pa_requests WHERE id=?1", [id], |r| r.get(0))
            .optional()?)
    }
    pub(super) fn admit_occurrence(
        &self,
        run: &crate::schedules::Run,
        template: &recurring::Template,
        current: &Seat,
    ) -> Result<String> {
        let occurrence = recurring::Occurrence::for_run(run, template);
        self.action(
            "scheduler",
            &run.id,
            "scheduledRequest",
            &(&occurrence, template),
            |tx| {
                let request_id = format!("dr-{}", run.id);
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pa_requests WHERE id=?1)",
                    [&request_id],
                    |r| r.get(0),
                )?;
                if exists {
                    let receipt = json!({"requestId":request_id,"occurrence":occurrence});
                    return Ok((template.assistant_id.clone(), request_id, receipt));
                }
                let row = Self::read(tx, &template.assistant_id)?;
                require(
                    template.project_ref.instance_id == row.config.home_instance_id,
                    "destination_scope_required",
                )?;
                row.scoped(&template.project_ref.project_id)?;
                require(
                    row.current_scope(
                        &template.project_ref.project_id,
                        template.assistant_revision,
                    ) && row.session_id.as_deref() == Some(&run.target_session_id),
                    "confirmation_stale",
                )?;
                require(template.confirmed_seat == *current, "director_changed")?;
                let recipient = current
                    .session_id
                    .as_deref()
                    .ok_or_else(|| fault("director_missing"))?;
                let request = DirectorRequest {
                    id: request_id,
                    assistant_id: row.id.clone(),
                    project_id: template.project_ref.project_id.clone(),
                    project_ref: Some(template.project_ref.clone()),
                    outcome: template.task.description().into(),
                    constraints: "Read-only coordination report.".into(),
                    source_ref: run.schedule_id.clone(),
                    director: current.clone(),
                    assistant_revision: row.revision,
                    state: RequestState::Queued,
                    revision: 1,
                    updated_at: crate::management::instance::now_utc(),
                    detail: None,
                    due_at: Some(run.scheduled_at),
                    occurrence: Some(occurrence.clone()),
                };
                enqueue(tx, &request, recipient)?;
                let audit = json!({"requestId":request.id,"occurrence":occurrence,
                "projectRef":template.project_ref,"confirmedSeat":current});
                Ok((row.id, request.id, audit))
            },
        )
    }
    pub fn cancel(
        &self,
        actor: &str,
        assistant: &str,
        request: &str,
        action: &str,
        revision: u64,
    ) -> Result<DirectorRequest> {
        self.action(
            actor,
            action,
            "cancel",
            &(assistant, request, revision),
            |tx| {
                let mut r = load(tx, request)?;
                require(r.assistant_id == assistant, "request_not_found")?;
                require(r.revision == revision, "revision_conflict")?;
                require(
                    !matches!(
                        r.state,
                        RequestState::Declined | RequestState::Completed | RequestState::Cancelled
                    ),
                    "invalid_transition",
                )?;
                updated(&mut r, RequestState::Cancelled, None);
                save(tx, &r)?;
                Ok((assistant.into(), r.clone(), json!({"requestId":r.id})))
            },
        )
    }
    pub fn respond(
        &self,
        caller: &str,
        request: &str,
        action: &str,
        revision: u64,
        response: Response,
        note: &str,
        current: &Seat,
    ) -> Result<DirectorRequest> {
        let prior = self.request(request)?;
        require(
            current.session_id.as_deref() == Some(caller) && prior.director == *current,
            "director_changed",
        )?;
        require(
            note.len() <= 8192 && (response == Response::Accept || !note.trim().is_empty()),
            "invalid_text",
        )?;
        self.guarded_action(caller, action, "directorResponse", &(request,revision,response,note),
          |tx| {
            let r = load(tx, request)?;
            require(r.director == *current, "director_changed")?;
            let row = Self::read(tx, &r.assistant_id)?;
            // A project may finish work it already accepted. Future admissions
            // still require current assistant scope and confirmation.
            if !matches!(r.state, RequestState::Accepted | RequestState::Completed) {
                row.scoped(&r.project_id)?;
                require(row.request_revision_current(&r), "confirmation_stale")?;
            }
            Ok(())
          }, |tx| {
            let mut r = load(tx,request)?;
            let row = Self::read(tx,&r.assistant_id)?;
            require(r.revision == revision, "revision_conflict")?;
            let next=match (r.state,response) {
                (RequestState::Queued,Response::Accept)=>RequestState::Accepted,
                (RequestState::Queued|RequestState::Accepted,Response::Clarify)=>RequestState::Clarification,
                (RequestState::Queued|RequestState::Clarification,Response::Decline)=>RequestState::Declined,
                (RequestState::Accepted,Response::Progress)=>RequestState::Accepted,
                (RequestState::Accepted,Response::Report)=>RequestState::Completed,
                _=>return Err(fault("invalid_transition")),
            };
            if next == RequestState::Clarification {
                require_open_slot(tx, &row.id)?;
            }
            updated(&mut r,next,Some(note));
            save(tx,&r)?;
            if row.scoped(&r.project_id).is_ok() {
                notice(tx, &r, row.session_id.as_deref().ok_or_else(||fault("assistant_disabled"))?,
                    "A Director updated one of your requests. Read the current state with pa_request_list.")?;
            }
            Ok((r.assistant_id.clone(),r.clone(),json!({"requestId":r.id,"response":response,"detail":note})))
        })
    }
    /// A clarification or stale destination creates a fresh owner decision. The
    /// original outcome stays in the audit history and the old inbox entry closes.
    pub fn revise(
        &self,
        caller: &str,
        request: &str,
        action: &str,
        revision: u64,
        draft: &Draft,
        seat: &Seat,
    ) -> Result<DirectorRequest> {
        self.for_session(caller)?.scoped(&draft.project_id)?;
        bounded(&draft.outcome, 8192)?;
        bounded(&draft.source_ref, 256)?;
        require(draft.constraints.len() <= 4096, "invalid_text")?;
        self.guarded_action(
            caller,
            action,
            "revise",
            &(request, revision, draft),
            |tx| Self::by_session(tx, caller)?.scoped(&draft.project_id),
            |tx| {
                let row = Self::by_session(tx, caller)?;
                row.scoped(&draft.project_id)?;
                let mut r = load(tx, request)?;
                require(r.assistant_id == row.id, "request_not_found")?;
                require(r.revision == revision, "revision_conflict")?;
                require(r.project_id == draft.project_id, "project_mismatch")?;
                let stale = r.director != *seat || !row.request_revision_current(&r);
                require(
                    matches!(
                        r.state,
                        RequestState::AwaitingOwner | RequestState::Clarification
                    ) || (r.state == RequestState::Queued && stale),
                    "invalid_transition",
                )?;
                if r.state == RequestState::Queued {
                    require_open_slot(tx, &row.id)?;
                }
                let prior = r.clone();
                r.outcome = draft.outcome.clone();
                r.constraints = draft.constraints.clone();
                r.source_ref = draft.source_ref.clone();
                r.due_at = draft.due_at;
                r.director = seat.clone();
                r.assistant_revision = row.revision;
                updated(&mut r, RequestState::AwaitingOwner, None);
                save(tx, &r)?;
                Ok((
                    row.id,
                    r.clone(),
                    json!({"requestId":r.id,"before":prior,"after":r}),
                ))
            },
        )
    }
    pub fn requests(
        &self,
        assistant: Option<&str>,
        director: Option<&str>,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, DirectorRequest)>> {
        require(assistant.is_some() ^ director.is_some(), "invalid_scope")?;
        require((1..=100).contains(&limit), "invalid_limit")?;
        let (sql, filter) = if let Some(id) = assistant {
            ("SELECT seq,body FROM pa_requests WHERE assistant=?1 AND seq>?2 ORDER BY seq LIMIT ?3",id)
        } else {
            (DIRECTOR_REQUESTS, director.unwrap_or_default())
        };
        request_page(&self.connection()?, sql, filter, after, limit)
    }
    pub(super) fn due_requests(
        &self,
        row: &Assistant,
        start: i64,
        end: i64,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<DirectorRequest>, Option<String>)> {
        let (ms, seq) = match after {
            None => (i64::MIN, 0i64),
            Some(cursor) => {
                let (a, b) = cursor
                    .split_once('|')
                    .ok_or_else(|| fault("invalid_cursor"))?;
                (
                    a.parse::<i64>().map_err(|_| fault("invalid_cursor"))?,
                    b.parse::<i64>().map_err(|_| fault("invalid_cursor"))?,
                )
            }
        };
        let conn = self.connection()?;
        let mut q=conn.prepare("SELECT due_ms,seq,body FROM pa_requests WHERE assistant=?1 AND due_ms>=?2 AND due_ms<?3
          AND (due_ms,seq)>(?4,?5) AND state NOT IN ('completed','declined','cancelled')
          AND json_extract(body,'$.projectRef.instanceId')=?6 AND project IN (SELECT value FROM json_each(?7)) ORDER BY due_ms,seq LIMIT ?8")?;
        let mut rows = q.query(params![
            row.id,
            start,
            end,
            ms,
            seq,
            row.config.home_instance_id,
            serde_json::to_string(&row.config.project_ids)?,
            limit as i64 + 1
        ])?;
        let mut items = Vec::new();
        let mut last = String::new();
        while let Some(r) = rows.next()? {
            if items.len() == limit {
                return Ok((items, Some(last)));
            }
            last = format!("{}|{}", r.get::<_, i64>(0)?, r.get::<_, i64>(1)?);
            items.push(serde_json::from_str(&r.get::<_, String>(2)?)?);
        }
        Ok((items, None))
    }

    pub(super) fn director_inbox(
        &self,
        director: &str,
        after: i64,
        limit: usize,
        holds_seat: impl Fn(&DirectorRequest) -> bool,
    ) -> Result<(Vec<DirectorInboxItem>, Option<i64>)> {
        require((1..=100).contains(&limit), "invalid_limit")?;
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let rows = request_page(&tx, DIRECTOR_REQUESTS, director, after, limit)?;
        let cursor = rows.last().map(|(seq, _)| *seq);
        let mut items = Vec::new();
        for (_, request) in rows {
            let source = Self::read(&tx, &request.assistant_id)?;
            let admitted = matches!(
                request.state,
                RequestState::Accepted | RequestState::Completed
            );
            let in_scope = admitted
                || (source.request_revision_current(&request)
                    && source.scoped(&request.project_id).is_ok());
            if !holds_seat(&request) || !in_scope {
                continue;
            }
            let clarification_full =
                matches!(request.state, RequestState::Queued | RequestState::Accepted)
                    && open_requests_full(&tx, &request.assistant_id)?;
            items.push(DirectorInboxItem {
                request,
                clarification_full,
            });
        }
        tx.commit()?;
        Ok((items, cursor))
    }
    pub fn memory_write(
        &self,
        session: &str,
        action: &str,
        key: &str,
        revision: u64,
        content: &str,
        source: &str,
    ) -> Result<Value> {
        self.for_session(session)?.require_active()?;
        bounded(key, 160)?;
        bounded(content, 8192)?;
        bounded(source, 256)?;
        self.guarded_action(session,action,"memoryWrite",&(key,revision,content,source),
          |tx| Self::by_session(tx,session)?.require_active(), |tx|{
            let row=Self::by_session(tx,session)?;row.require_active()?;
            let prior = tx.query_row(
                "SELECT revision FROM pa_memory WHERE assistant=?1 AND key=?2",
                params![row.id, key], |r| r.get::<_, i64>(0)
            ).optional()?.unwrap_or(0);
            require(u64::try_from(prior)? == revision, "revision_conflict")?;
            if prior == 0 {
                let keys: i64 = tx.query_row("SELECT count(*) FROM pa_memory WHERE assistant=?1",
                    [&row.id], |r| r.get(0))?;
                require(keys < MEMORY_KEYS, "memory_full")?;
            }
            let next = prior.checked_add(1).ok_or_else(|| fault("revision_conflict"))?;
            tx.execute("INSERT INTO pa_memory(assistant,key,revision,content,source) VALUES(?1,?2,?3,?4,?5)
              ON CONFLICT(assistant,key) DO UPDATE SET revision=excluded.revision,content=excluded.content,source=excluded.source",
                params![row.id,key,next,content,source])?;
            Ok((row.id,json!({"key":key,"revision":next}),json!({"key":key,"revision":next})))
        })
    }
    pub fn memory(
        &self,
        session: &str,
        query: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<Value>> {
        require(
            (1..=100).contains(&limit) && query.len() <= 256,
            "invalid_limit",
        )?;
        let conn = self.connection()?;
        let row = Self::by_session(&conn, session)?;
        row.require_active()?;
        Self::note_page(&conn, &row.id, query, after, limit)
    }
    pub(super) fn note_page(
        conn: &Connection,
        assistant: &str,
        query: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let mut q = conn.prepare(
            "SELECT key,revision,content,source FROM pa_memory WHERE assistant=?1 AND key>?2
            AND (instr(key,?3)>0 OR instr(content,?3)>0) ORDER BY key LIMIT ?4",
        )?;
        let rows = q.query_map(params![assistant, after, query, limit as i64], |r| {
            Ok(
                json!({"key":r.get::<_,String>(0)?, "revision":r.get::<_,i64>(1)?,
                "content":r.get::<_,String>(2)?, "sourceRef":r.get::<_,String>(3)?}),
            )
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
