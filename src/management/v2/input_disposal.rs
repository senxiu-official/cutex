//! Durable owner decisions are source authority, not a ledger projection.
//! The sibling database survives a ledger rebuild. Its short file fence orders
//! decisions and projection writes; it is never held over native/source IO.
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const METHOD: &str = "cutex/input/dispose";
pub const EFFECT: &str = "Stop waiting and prevent further delivery or resend; content already in flight may still be read.";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Disposition {
    pub input_id: String,
    pub cutex_session_id: String,
    pub disposition_id: String,
    pub requested_at: String,
    pub applied_at: Option<String>,
    pub effect: String,
}

pub(super) struct Guard {
    connection: Option<Connection>,
    _file: std::fs::File,
    path: PathBuf,
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
pub(super) fn default_ledger_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?
        .join("management-v2")
        .join(super::input_ledger::DATABASE_FILE))
}
fn configure(db: &Connection) -> anyhow::Result<()> {
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA synchronous=FULL;")?;
    if !db.table_exists(None, "owner_dispositions")? {
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE owner_dispositions(session TEXT NOT NULL,input_id TEXT NOT NULL,disposition_id TEXT NOT NULL,requested_at TEXT NOT NULL,applied_at TEXT,PRIMARY KEY(session,input_id));")?;
    }
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        db.execute_batch("BEGIN IMMEDIATE; ALTER TABLE owner_dispositions ADD COLUMN next_check_ms INTEGER NOT NULL DEFAULT 0; ALTER TABLE owner_dispositions ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0; CREATE INDEX IF NOT EXISTS owner_dispositions_pending ON owner_dispositions(next_check_ms,requested_at) WHERE applied_at IS NULL; PRAGMA user_version=1; COMMIT;")?;
    }
    anyhow::ensure!(version <= 1, "unsupported owner disposition store version");
    Ok(())
}
impl Guard {
    pub(super) fn open(ledger: &Path, create: bool) -> anyhow::Result<Self> {
        Self::open_notifying(ledger, create, || {})
    }
    pub(super) fn open_notifying(
        ledger: &Path,
        create: bool,
        on_wait: impl FnOnce(),
    ) -> anyhow::Result<Self> {
        let root = parent(ledger);
        std::fs::create_dir_all(root)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(root.join("input-dispositions.lock"))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if crate::platform::file_lock::lock_contended(&error) => {
                on_wait();
                fs2::FileExt::lock_exclusive(&file)?;
            }
            Err(error) => return Err(error.into()),
        }
        let path = root.join("input-dispositions.sqlite3");
        let connection = if create || path.try_exists()? {
            let db = Connection::open(&path)?;
            configure(&db)?;
            Some(db)
        } else {
            None
        };
        Ok(Self {
            connection,
            _file: file,
            path,
        })
    }
    pub(super) fn get(&self, session: &str, id: &str) -> anyhow::Result<Option<Disposition>> {
        let Some(db) = &self.connection else {
            return Ok(None);
        };
        Ok(db.query_row("SELECT disposition_id,requested_at,applied_at FROM owner_dispositions WHERE session=?1 AND input_id=?2",
            params![session,id],|row|Ok(Disposition{input_id:id.into(),cutex_session_id:session.into(),disposition_id:row.get(0)?,
                requested_at:row.get(1)?,applied_at:row.get(2)?,effect:EFFECT.into()})).optional()?)
    }
    pub(super) fn request(
        &mut self,
        session: &str,
        id: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Disposition> {
        anyhow::ensure!(
            !session.is_empty() && !id.is_empty() && id.len() <= 4096,
            "session and inputId must be non-empty bounded identifiers"
        );
        if self.connection.is_none() {
            let db = Connection::open(&self.path)?;
            configure(&db)?;
            self.connection = Some(db);
        }
        self.connection.as_mut().expect("created decision store").execute(
            "INSERT OR IGNORE INTO owner_dispositions(session,input_id,disposition_id,requested_at) VALUES(?1,?2,?3,?4)",
            params![session,id,uuid::Uuid::new_v4().to_string(),now.to_rfc3339()])?;
        self.connection.as_ref().expect("decision store").execute("UPDATE owner_dispositions SET next_check_ms=0 WHERE session=?1 AND input_id=?2 AND applied_at IS NULL",params![session,id])?;
        Ok(self.get(session, id)?.expect("durable decision"))
    }
    fn pending(&self) -> anyhow::Result<Vec<Disposition>> {
        let Some(db) = &self.connection else {
            return Ok(Vec::new());
        };
        let mut statement=db.prepare("SELECT session,input_id,disposition_id,requested_at FROM owner_dispositions WHERE applied_at IS NULL AND next_check_ms<=?1 ORDER BY next_check_ms,requested_at LIMIT 1000")?;
        let rows = statement.query_map([Utc::now().timestamp_millis()], |row| {
            Ok(Disposition {
                cutex_session_id: row.get(0)?,
                input_id: row.get(1)?,
                disposition_id: row.get(2)?,
                requested_at: row.get(3)?,
                applied_at: None,
                effect: EFFECT.into(),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
    fn defer(&self, decision: &Disposition, missing: bool) -> anyhow::Result<()> {
        if let Some(db) = &self.connection {
            db.execute("UPDATE owner_dispositions SET next_check_ms=?3+CASE WHEN ?4 THEN min(30000,500*(1<<min(attempts,6))) ELSE 500 END,attempts=min(attempts+1,20) WHERE session=?1 AND input_id=?2",params![decision.cutex_session_id,decision.input_id,Utc::now().timestamp_millis(),missing])?;
        }
        Ok(())
    }
    fn applied(&self, decision: &Disposition) -> anyhow::Result<()> {
        if let Some(db) = &self.connection {
            db.execute("UPDATE owner_dispositions SET applied_at=COALESCE(applied_at,?3) WHERE session=?1 AND input_id=?2",params![decision.cutex_session_id,decision.input_id,Utc::now().to_rfc3339()])?;
        }
        Ok(())
    }
}

pub fn requested(session: &str, id: &str) -> anyhow::Result<bool> {
    let path = default_ledger_path()?;
    if !parent(&path)
        .join("input-dispositions.sqlite3")
        .try_exists()?
    {
        return Ok(false);
    }
    Ok(Guard::open(&path, false)?.get(session, id)?.is_some())
}

pub(super) fn suppress(
    mut record: super::input_ledger::InputRecord,
    decision: &Disposition,
) -> super::input_ledger::InputRecord {
    use super::input_ledger::{InputState, Surface};
    if matches!(
        record.state,
        InputState::InContext
            | InputState::Discarded
            | InputState::Superseded
            | InputState::Rejected
    ) {
        return record;
    }
    record.state = InputState::Discarded;
    record.reason = Some("ownerDiscarded".into());
    record.surface = Surface::None;
    record.deadline = None;
    record.wake_schedule = None;
    record.actions.clear();
    record.stalled = None;
    record.stage = None;
    record.order = None;
    record.since = decision.requested_at.clone();
    if !record.details.is_object() {
        record.details = serde_json::json!({});
    }
    record.details["ownerDisposition"] = serde_json::json!({"dispositionId":decision.disposition_id,"requestedAt":decision.requested_at,"effect":decision.effect});
    record
}

/// Independent id-only receipt: no record enum, payload, revision or lane is
/// required to durably accept the owner's decision (including pre-admission).
pub fn request_at(
    ledger: &Path,
    session: &str,
    id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Disposition> {
    Guard::open(ledger, true)?.request(session, id, now)
}
pub fn request(session: &str, id: &str) -> anyhow::Result<Disposition> {
    request_at(&default_ledger_path()?, session, id, Utc::now())
}
pub(super) fn parse_request(params: &Value) -> anyhow::Result<&str> {
    let object = params
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("params must be an object"))?;
    anyhow::ensure!(object.len() == 1, "dispose accepts exactly inputId");
    object
        .get("inputId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 4096)
        .ok_or_else(|| anyhow::anyhow!("inputId must be a non-empty bounded string"))
}

/// No native IO and no waiting on an admission lane. A receipt is durable
/// before any source cleanup is attempted; interrupted cleanup is replayable.
pub(super) fn drain_once() -> anyhow::Result<()> {
    let path = default_ledger_path()?;
    if !parent(&path)
        .join("input-dispositions.sqlite3")
        .try_exists()?
    {
        return Ok(());
    }
    let ledger = super::input_ledger::default_ledger()?;
    drain_on(ledger, &path, settle_local).map(|_| ())
}
fn settle_local(record: &super::input_ledger::InputRecord) -> Result<(), String> {
    use super::input_ledger::InputKind;
    match record.kind {
        InputKind::Owner => super::input_ledger_owner::dismiss(record),
        InputKind::AgentMessage
        | InputKind::PeerMessage
        | InputKind::Alarm
        | InputKind::SystemNotice => super::agent_bus_state::agent_bus_message_repository()
            .map_err(|e| format!("{e:#}"))
            .and_then(|store| {
                super::input_ledger_bus::dismiss_with(store, record, Utc::now()).map(|_| ())
            }),
        _ => super::input_ledger_asks::dismiss(record).map(|_| ()),
    }
}
pub(super) fn drain_on(
    ledger: &super::input_ledger::InputLedger,
    path: &Path,
    mut settle: impl FnMut(&super::input_ledger::InputRecord) -> Result<(), String>,
) -> anyhow::Result<usize> {
    let pending = Guard::open(path, false)?.pending()?;
    let mut count = 0;
    for decision in pending {
        let record = ledger
            .get(&decision.cutex_session_id, &[decision.input_id.clone()])
            .ok()
            .and_then(|mut found| found.records.pop());
        let result = if let Some(record) = &record {
            let projected = match ledger.transition(
                &decision.cutex_session_id,
                &decision.input_id,
                super::input_ledger::Expect::ANY,
                super::input_ledger::Next::discarded("ownerDiscarded"),
            ) {
                Ok(projection) => super::input_ledger::publish(&projection).is_ok(),
                Err(_) => false,
            };
            // Projection failure must not delay source withdrawal. Keep the
            // intent pending until both independent effects can be confirmed.
            settle(record).map(|_| projected)
        } else {
            settle_missing(&decision.cutex_session_id, &decision.input_id)
        };
        let guard = Guard::open(path, false)?;
        if matches!(result, Ok(true)) {
            guard.applied(&decision)?;
            count += 1;
        } else {
            guard.defer(&decision, record.is_none())?;
        }
    }
    Ok(count)
}

fn settle_missing(session: &str, id: &str) -> Result<bool, String> {
    if super::input_ledger_owner::dismiss_id(session, id)? {
        return Ok(true);
    }
    let bus =
        super::agent_bus_state::agent_bus_message_repository().map_err(|e| format!("{e:#}"))?;
    if bus
        .snapshot_by_message_id(id)
        .map_err(|e| format!("{e:#}"))?
        .is_some()
    {
        super::input_ledger_bus::dismiss_id_with(bus, session, id, Utc::now())?;
        return Ok(true);
    }
    let store = crate::asks::store::AskStore::open_default().map_err(|e| format!("{e:#}"))?;
    crate::asks::ledger::dismiss_input_id(&store, session, id, Utc::now())
}

pub(super) fn ensure_started() {
    static EXPIRY: std::sync::Once = std::sync::Once::new();
    EXPIRY.call_once(|| {
        if let Err(error) = std::thread::Builder::new()
            .name("cutex-input-expiry".into())
            .spawn(|| loop {
                if super::input_ledger::enabled() {
                    if let Ok(ledger) = super::input_ledger::default_ledger() {
                        if let Err(error) =
                            super::input_ledger::expire_observations(ledger, Utc::now(), 1000)
                        {
                            eprintln!("cutex observation expiry: {error}");
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            })
        {
            eprintln!("cutex observation expiry: thread not started: {error}");
        }
    });

    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        if let Err(error) = std::thread::Builder::new()
            .name("cutex-owner-disposal".into())
            .spawn(|| loop {
                if let Err(error) = drain_once() {
                    eprintln!("cutex owner disposal: {error:#}");
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            })
        {
            eprintln!("cutex owner disposal: thread not started: {error}");
        }
    });
}

/// Called in the recipient admission lane before claiming native dispatch.
/// All members are checked together so a frozen batch is never changed under
/// its digest identity. Already claimed IO is explicitly covered by EFFECT.
pub fn check<'a>(session: &str, ids: impl IntoIterator<Item = &'a str>) -> anyhow::Result<()> {
    check_at(&default_ledger_path()?, session, ids)
}
pub(super) fn check_at<'a>(
    path: &Path,
    session: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<()> {
    if !parent(path)
        .join("input-dispositions.sqlite3")
        .try_exists()?
    {
        return Ok(());
    }
    let guard = Guard::open(path, false)?;
    for id in ids {
        anyhow::ensure!(
            guard.get(session, id)?.is_none(),
            "input disposed by owner; further delivery is disabled"
        );
    }
    Ok(())
}

pub fn check_submission(
    session: &str,
    submission: &crate::asks::scheduler::Submission,
) -> anyhow::Result<()> {
    let path = default_ledger_path()?;
    if !parent(&path)
        .join("input-dispositions.sqlite3")
        .try_exists()?
    {
        return Ok(());
    }
    let ids=crate::asks::store::AskStore::open_default()?.project_snapshot(|state|->anyhow::Result<Vec<String>> {
        if let Some(n)=submission.attempt {
            submission.asks.iter().map(|id| {
                let ask=state.asks.get(id).ok_or_else(||anyhow::anyhow!("request source no longer exists"))?;
                let attempt=ask.current_attempt().filter(|attempt|attempt.n==n&&attempt.native_message_id==submission.message_id)
                    .ok_or_else(||anyhow::anyhow!("request admission attempt changed"))?;
                anyhow::ensure!(ask.is_open()&&ask.remote.is_none()&&ask.target_cutex_session_id==session&&attempt.admission_occurrence==ask.admission_occurrence,"request admission occurrence changed");
                anyhow::ensure!(state.leases.get(session).is_some_and(|lease|matches!(&lease.holder,crate::asks::model::LeaseHolder::Ask{ask_id,attempt} if ask_id==id&&*attempt==n)),"request reservation changed");
                Ok(crate::asks::model::request_occurrence_id(id,attempt.admission_occurrence))
            }).collect()
        } else {
            Ok(state.events.values().find(|event|event.native_message_id==submission.message_id&&event.cutex_session_id==session)
                .map(|event|event.delivery_ids.clone()).unwrap_or_else(||vec![submission.message_id.clone()]))
        }
    })??;
    check(session, ids.iter().map(String::as_str))
}

/// A frozen external event may contain several logical deliveries.
pub fn check_envelope(
    session: &str,
    envelope: &crate::app_server::external_input::Envelope,
) -> anyhow::Result<()> {
    check(session, [envelope.message.id.as_str()])?;
    let path = default_ledger_path()?;
    if !parent(&path)
        .join("input-dispositions.sqlite3")
        .try_exists()?
    {
        return Ok(());
    }
    let ids = crate::asks::store::AskStore::open_default()?.project_snapshot(|state| {
        state
            .events
            .values()
            .find(|event| {
                event.native_message_id == envelope.message.id && event.cutex_session_id == session
            })
            .map(|event| event.delivery_ids.clone())
            .unwrap_or_default()
    })?;
    check(session, ids.iter().map(String::as_str))
}
