//! The owner pins database (`runtime/owner-pins/owner-pins.sqlite3`): one
//! bounded immediate transaction per call, so limits, key de-duplication and
//! action-id replays hold across daemon processes. Every list is bounded
//! (open pins are capped per host; resolved ones are paged).

use super::*;
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(test)]
#[test]
#[ignore = "requires an explicitly marked release business copy"]
fn release_data_copy_owner_pins_validate_and_reopen() {
    let home = PathBuf::from(std::env::var_os("CUTEX_RELEASE_DATA_COPY").expect("explicit copy"));
    assert!(home.is_absolute() && home.join(".cutex-release-data-copy").is_file());
    assert_eq!(crate::config::paths::config_dir().unwrap().canonicalize().unwrap(), home.canonicalize().unwrap());
    let root = home.join("runtime/owner-pins");
    if !root.join(DB_FILE).exists() { eprintln!("RELEASE DATA COPY owner pins absent"); return; }
    for pass in 0..2 {
        let store = OwnerPinsStore::open(&root).unwrap();
        let connection = store.connection().unwrap();
        let mut statement = connection.prepare(&format!("SELECT {COLUMNS} FROM pins")).unwrap();
        let count = statement.query_map([], read_pin).unwrap().map(|row| row.unwrap()).count();
        let mut actions = connection.prepare("SELECT result FROM pin_actions").unwrap();
        for result in actions.query_map([], |row| row.get::<_, String>(0)).unwrap() {
            let _: Value = serde_json::from_str(&result.unwrap()).unwrap();
        }
        eprintln!("RELEASE DATA COPY owner pins pass={pass} pins={count}");
    }
}

const DB_FILE: &str = "owner-pins.sqlite3";
const SCHEMA_VERSION: i64 = 1;
/// Idempotency receipts of agent calls (not owner data) are kept this long.
const ACTION_RETENTION_DAYS: i64 = 7;
const COLUMNS: &str = "pin_id, pin_key, author, agent_id, agent_name, kind, title, detail, links, turn_id, state, resolved_by, resolution, created_at, updated_at, resolved_at, revision";

/// What an agent asked for (already validated by the tool).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentRequest {
    pub pin_id: Option<String>,
    pub key: Option<String>,
    pub kind: Option<Kind>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub links: Option<Vec<Link>>,
    /// done / dropped: resolve the addressed pin.
    pub resolve: Option<State>,
    pub note: Option<String>,
}

/// The caller as the bus knows it.
#[derive(Clone, Debug)]
pub struct AgentCaller {
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerPinList {
    pub pins: Vec<OwnerPin>,
    /// Open pins on this host (all agents), whatever the filter.
    pub open_count: usize,
}

#[derive(Clone, Debug)]
pub struct OwnerPinsStore {
    root: PathBuf,
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl OwnerPinsStore {
    pub fn open_default() -> PinResult<Self> {
        Self::open(crate::config::paths::runtime_dir()?.join("owner-pins"))
    }

    pub fn open(root: impl Into<PathBuf>) -> PinResult<Self> {
        let store = Self { root: root.into() };
        crate::management::client_tokens::ensure_private_dir(&store.root)?;
        let mut connection = store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(PinError::Storage(anyhow::anyhow!("owner pins database requires a newer cutex")));
        }
        if version == 0 {
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS pins (
                   pin_id TEXT PRIMARY KEY, pin_key TEXT, author TEXT NOT NULL,
                   agent_id TEXT NOT NULL, agent_name TEXT, kind TEXT NOT NULL,
                   title TEXT NOT NULL, detail TEXT, links TEXT NOT NULL, turn_id TEXT,
                   state TEXT NOT NULL, resolved_by TEXT, resolution TEXT,
                   created_at TEXT NOT NULL, updated_at TEXT NOT NULL, resolved_at TEXT,
                   revision INTEGER NOT NULL CHECK (revision > 0)
                 );
                 CREATE INDEX IF NOT EXISTS pins_state_time ON pins(state, created_at DESC);
                 CREATE INDEX IF NOT EXISTS pins_agent_state ON pins(agent_id, state, created_at DESC);
                 CREATE INDEX IF NOT EXISTS pins_resolved ON pins(state, resolved_at DESC);
                 CREATE UNIQUE INDEX IF NOT EXISTS pins_open_key ON pins(agent_id, pin_key) WHERE state = 'open' AND pin_key IS NOT NULL;
                 CREATE TABLE IF NOT EXISTS pin_actions (
                   agent_id TEXT NOT NULL, action_id TEXT NOT NULL, digest TEXT NOT NULL,
                   result TEXT NOT NULL, at TEXT NOT NULL, PRIMARY KEY(agent_id, action_id)
                 );
                 CREATE INDEX IF NOT EXISTS pin_actions_time ON pin_actions(agent_id, at);",
            )?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        transaction.commit()?;
        Ok(store)
    }

    fn connection(&self) -> PinResult<Connection> {
        let path = self.root.join(DB_FILE);
        secure_file(&path, true)?;
        self.secure_files()?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        enable_wal(&connection)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        self.secure_files()?;
        Ok(connection)
    }

    fn secure_files(&self) -> PinResult<()> {
        for name in [DB_FILE.to_string(), format!("{DB_FILE}-wal"), format!("{DB_FILE}-shm")] {
            let path = self.root.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => secure_file(&path, false)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(PinError::Storage(error.into())),
            }
        }
        Ok(())
    }

    /// `owner_pin`: create, update (by pin id or key) or resolve one of the
    /// caller's pins. Returns the tool result and the changed pin (to publish).
    pub fn agent_write(&self, caller: &AgentCaller, request: AgentRequest, action_id: &str, digest: &str, now: DateTime<Utc>) -> PinResult<(Value, Option<OwnerPin>)> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let agent = caller.agent_id.as_str();
        if let Some((old_digest, result)) = tx
            .query_row("SELECT digest, result FROM pin_actions WHERE agent_id=?1 AND action_id=?2", params![agent, action_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .optional()?
        {
            if old_digest != digest {
                return Err(PinError::IdempotencyConflict);
            }
            return Ok((serde_json::from_str(&result)?, None));
        }
        let hour_ago = stamp(now - ChronoDuration::hours(1));
        let recent: i64 = tx.query_row("SELECT COUNT(*) FROM pin_actions WHERE agent_id=?1 AND at>?2", params![agent, hour_ago], |row| row.get(0))?;
        if recent as usize >= MAX_WRITES_PER_HOUR {
            return Err(PinError::RateLimited);
        }
        let at = stamp(now);
        // Address: an explicit pin id (any state, own only), else the open pin with this key.
        let addressed = match (&request.pin_id, &request.key) {
            (Some(id), _) => Some(get_own(&tx, agent, id)?.ok_or(PinError::NotFound)?),
            (None, Some(key)) => tx
                .query_row(&format!("SELECT {COLUMNS} FROM pins WHERE agent_id=?1 AND pin_key=?2 AND state='open'"), params![agent, key], read_pin)
                .optional()?,
            (None, None) => None,
        };
        let (pin, created) = if let Some(state) = request.resolve {
            let mut pin = addressed.ok_or_else(|| if request.pin_id.is_none() && request.key.is_none() { invalid("state: give pin_id or key of the pin to resolve") } else { PinError::NotFound })?;
            if pin.state != State::Open {
                return Err(PinError::NotOpen);
            }
            pin.state = state;
            pin.resolved_by = Some("agent".into());
            pin.resolution = request.note.clone();
            pin.resolved_at = Some(at.clone());
            pin.updated_at = at.clone();
            pin.revision += 1;
            write_pin(&tx, &pin)?;
            (pin, false)
        } else if let Some(mut pin) = addressed {
            if pin.state != State::Open {
                return Err(PinError::NotOpen);
            }
            if let Some(kind) = request.kind {
                pin.kind = kind;
            }
            if let Some(title) = &request.title {
                pin.title = title.clone();
            }
            if request.detail.is_some() {
                pin.detail = request.detail.clone();
            }
            if let Some(links) = &request.links {
                pin.links = links.clone();
            }
            if pin.key.is_none() {
                pin.key = request.key.clone();
            }
            if caller.agent_name.is_some() {
                pin.agent_name = caller.agent_name.clone();
            }
            pin.updated_at = at.clone();
            pin.revision += 1;
            write_pin(&tx, &pin)?;
            (pin, false)
        } else {
            if request.pin_id.is_some() {
                return Err(PinError::NotFound);
            }
            let (Some(kind), Some(title)) = (request.kind, request.title.clone()) else {
                return Err(invalid("a new pin needs kind and title"));
            };
            // The same open title from the same agent is the same pin.
            if let Some(existing) = tx
                .query_row(&format!("SELECT {COLUMNS} FROM pins WHERE agent_id=?1 AND state='open' AND title=?2 LIMIT 1"), params![agent, title], read_pin)
                .optional()?
            {
                let result = tool_result(&existing, false);
                remember(&tx, agent, action_id, digest, &result, &at)?;
                tx.commit()?;
                return Ok((result, None));
            }
            let open_mine: i64 = tx.query_row("SELECT COUNT(*) FROM pins WHERE agent_id=?1 AND state='open'", [agent], |row| row.get(0))?;
            if open_mine as usize >= MAX_OPEN_PER_AGENT {
                return Err(PinError::TooManyOpen(open_mine as usize));
            }
            let open_all: i64 = tx.query_row("SELECT COUNT(*) FROM pins WHERE state='open'", [], |row| row.get(0))?;
            if open_all as usize >= MAX_OPEN_PER_HOST {
                return Err(PinError::HostFull);
            }
            let pin = OwnerPin {
                pin_id: format!("pin-{}", uuid::Uuid::new_v4()),
                key: request.key.clone(),
                author: "agent".into(),
                agent_id: agent.to_string(),
                agent_name: caller.agent_name.clone(),
                kind,
                title,
                detail: request.detail.clone(),
                links: request.links.clone().unwrap_or_default(),
                turn_id: caller.turn_id.clone(),
                state: State::Open,
                resolved_by: None,
                resolution: None,
                created_at: at.clone(),
                updated_at: at.clone(),
                resolved_at: None,
                revision: 1,
            };
            insert_pin(&tx, &pin)?;
            (pin, true)
        };
        let result = tool_result(&pin, created);
        remember(&tx, agent, action_id, digest, &result, &at)?;
        tx.execute("DELETE FROM pin_actions WHERE agent_id=?1 AND at<?2", params![agent, stamp(now - ChronoDuration::days(ACTION_RETENTION_DAYS))])?;
        tx.commit()?;
        Ok((result, Some(pin)))
    }

    /// `owner_pins`: the caller's own pins, open first then newest resolved.
    pub fn agent_list(&self, agent_id: &str, all: bool) -> PinResult<Vec<OwnerPin>> {
        let connection = self.connection()?;
        let sql = if all {
            format!("SELECT {COLUMNS} FROM pins WHERE agent_id=?1 ORDER BY state='open' DESC, updated_at DESC LIMIT ?2")
        } else {
            format!("SELECT {COLUMNS} FROM pins WHERE agent_id=?1 AND state='open' ORDER BY created_at DESC LIMIT ?2")
        };
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params![agent_id, AGENT_LIST_LIMIT as i64], read_pin)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The owner's view: every open pin (bounded by the host cap), plus up to
    /// `resolved` resolved ones newest first when `all`; optionally one agent's.
    pub fn owner_list(&self, all: bool, agent_id: Option<&str>, resolved: usize) -> PinResult<OwnerPinList> {
        let connection = self.connection()?;
        let agent = agent_id.unwrap_or("");
        let mut pins: Vec<OwnerPin> = {
            let mut statement = connection.prepare(&format!("SELECT {COLUMNS} FROM pins WHERE state='open' AND (?1='' OR agent_id=?1) ORDER BY created_at DESC LIMIT ?2"))?;
            let rows = statement.query_map(params![agent, (MAX_OPEN_PER_HOST * 2) as i64], read_pin)?;
            rows.collect::<Result<_, _>>()?
        };
        if all && resolved > 0 {
            let mut statement = connection.prepare(&format!("SELECT {COLUMNS} FROM pins WHERE state<>'open' AND (?1='' OR agent_id=?1) ORDER BY resolved_at DESC, updated_at DESC LIMIT ?2"))?;
            let rows = statement.query_map(params![agent, resolved as i64], read_pin)?;
            for row in rows {
                pins.push(row?);
            }
        }
        let open_count: i64 = connection.query_row("SELECT COUNT(*) FROM pins WHERE state='open'", [], |row| row.get(0))?;
        Ok(OwnerPinList { pins, open_count: open_count as usize })
    }

    /// The owner resolves (done / dropped) or reopens any pin. Reopening is
    /// not limited: the owner decides.
    pub fn owner_set_state(&self, pin_id: &str, state: State, expected_revision: Option<u64>, now: DateTime<Utc>) -> PinResult<OwnerPin> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pin = tx
            .query_row(&format!("SELECT {COLUMNS} FROM pins WHERE pin_id=?1"), [pin_id], read_pin)
            .optional()?
            .ok_or(PinError::NotFound)?;
        if let Some(expected) = expected_revision {
            if expected != pin.revision {
                return Err(PinError::RevisionConflict(pin.revision));
            }
        }
        if pin.state == state {
            return Ok(pin);
        }
        if state == State::Open && pin.key.is_some() {
            // Keys are unique among open pins: a reopened pin whose key is taken drops its key.
            let taken: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pins WHERE agent_id=?1 AND pin_key=?2 AND state='open')", params![pin.agent_id, pin.key], |row| row.get(0))?;
            if taken {
                pin.key = None;
            }
        }
        let at = stamp(now);
        pin.state = state;
        if state == State::Open {
            pin.resolved_by = None;
            pin.resolved_at = None;
            pin.resolution = None;
        } else {
            pin.resolved_by = Some("owner".into());
            pin.resolved_at = Some(at.clone());
        }
        pin.updated_at = at;
        pin.revision += 1;
        write_pin(&tx, &pin)?;
        tx.commit()?;
        Ok(pin)
    }

    pub fn owner_delete(&self, pin_id: &str) -> PinResult<()> {
        let connection = self.connection()?;
        if connection.execute("DELETE FROM pins WHERE pin_id=?1", [pin_id])? == 0 {
            return Err(PinError::NotFound);
        }
        Ok(())
    }
}

fn get_own(tx: &Connection, agent: &str, pin_id: &str) -> PinResult<Option<OwnerPin>> {
    Ok(tx
        .query_row(&format!("SELECT {COLUMNS} FROM pins WHERE pin_id=?1 AND agent_id=?2"), params![pin_id, agent], read_pin)
        .optional()?)
}

fn tool_result(pin: &OwnerPin, created: bool) -> Value {
    json!({"ok": true, "pinId": pin.pin_id, "created": created, "state": pin.state.as_str(), "title": pin.title, "kind": pin.kind.as_str(), "key": pin.key})
}

fn remember(tx: &Connection, agent: &str, action_id: &str, digest: &str, result: &Value, at: &str) -> PinResult<()> {
    tx.execute(
        "INSERT INTO pin_actions(agent_id, action_id, digest, result, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![agent, action_id, digest, serde_json::to_string(result)?, at],
    )?;
    Ok(())
}

fn insert_pin(tx: &Connection, pin: &OwnerPin) -> PinResult<()> {
    tx.execute(
        &format!("INSERT INTO pins({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"),
        params![
            pin.pin_id, pin.key, pin.author, pin.agent_id, pin.agent_name, pin.kind.as_str(), pin.title, pin.detail,
            serde_json::to_string(&pin.links)?, pin.turn_id, pin.state.as_str(), pin.resolved_by, pin.resolution,
            pin.created_at, pin.updated_at, pin.resolved_at, pin.revision as i64
        ],
    )?;
    Ok(())
}

fn write_pin(tx: &Connection, pin: &OwnerPin) -> PinResult<()> {
    tx.execute(
        "UPDATE pins SET pin_key=?2, agent_name=?3, kind=?4, title=?5, detail=?6, links=?7, state=?8, resolved_by=?9, resolution=?10, updated_at=?11, resolved_at=?12, revision=?13 WHERE pin_id=?1",
        params![
            pin.pin_id, pin.key, pin.agent_name, pin.kind.as_str(), pin.title, pin.detail, serde_json::to_string(&pin.links)?,
            pin.state.as_str(), pin.resolved_by, pin.resolution, pin.updated_at, pin.resolved_at, pin.revision as i64
        ],
    )?;
    Ok(())
}

fn read_pin(row: &Row<'_>) -> rusqlite::Result<OwnerPin> {
    let kind: String = row.get(5)?;
    let state: String = row.get(10)?;
    let links: String = row.get(8)?;
    let bad = |index, what: &str| rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, anyhow::anyhow!("bad {what}").into());
    Ok(OwnerPin {
        pin_id: row.get(0)?,
        key: row.get(1)?,
        author: row.get(2)?,
        agent_id: row.get(3)?,
        agent_name: row.get(4)?,
        kind: Kind::parse(&kind).ok_or_else(|| bad(5, "kind"))?,
        title: row.get(6)?,
        detail: row.get(7)?,
        links: serde_json::from_str(&links).map_err(|_| bad(8, "links"))?,
        turn_id: row.get(9)?,
        state: State::parse(&state).ok_or_else(|| bad(10, "state"))?,
        resolved_by: row.get(11)?,
        resolution: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        resolved_at: row.get(15)?,
        revision: row.get::<_, i64>(16)? as u64,
    })
}

fn enable_wal(connection: &Connection) -> PinResult<()> {
    // Concurrent first opens may get SQLITE_BUSY from journal_mode even with a
    // busy timeout; retry only this idempotent initialization.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0)) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(_) => return Err(PinError::Storage(anyhow::anyhow!("owner pins database requires WAL mode"))),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn secure_file(path: &Path, create: bool) -> PinResult<()> {
    #[cfg(windows)]
    {
        match crate::platform::private_fs::open_private_file_path(path, create, true) {
            Ok(_) => (),
            Err(error) if !create && error.io_kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(PinError::Storage(error.into())),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = match std::fs::OpenOptions::new().read(true).write(true).create(create).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path) {
            Ok(file) => file,
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(PinError::Storage(error.into())),
        };
        if !file.metadata().map_err(|error| PinError::Storage(error.into()))?.is_file() {
            return Err(PinError::Storage(anyhow::anyhow!("owner pins database is not a regular file")));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|error| PinError::Storage(error.into()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (OwnerPinsStore, PathBuf) {
        let dir = super::test_root("owner-pins-test");
        (OwnerPinsStore::open(dir.join("owner-pins")).unwrap(), dir)
    }
    fn caller(id: &str) -> AgentCaller {
        AgentCaller { agent_id: id.into(), agent_name: Some("Alden".into()), turn_id: Some("t1".into()) }
    }
    fn new(kind: Kind, title: &str) -> AgentRequest {
        AgentRequest { kind: Some(kind), title: Some(title.into()), ..Default::default() }
    }
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn create_update_by_key_resolve_and_replay() {
        let (s, dir) = store();
        let a = caller("cutex.a");
        let (r, pin) = s.agent_write(&a, AgentRequest { key: Some("k".into()), ..new(Kind::Decide, "pick 4?") }, "x1", "d1", now()).unwrap();
        assert_eq!(r["created"], true);
        let pin = pin.unwrap();
        assert_eq!((pin.turn_id.as_deref(), pin.revision), (Some("t1"), 1));
        // Same action id replays the first result; a different request under it is refused.
        assert_eq!(s.agent_write(&a, new(Kind::Todo, "other"), "x1", "d1", now()).unwrap().0, r);
        assert!(matches!(s.agent_write(&a, new(Kind::Todo, "other"), "x1", "d2", now()), Err(PinError::IdempotencyConflict)));
        // Same key updates the open pin in place.
        let (r2, p2) = s.agent_write(&a, AgentRequest { key: Some("k".into()), title: Some("pick 4 or auto?".into()), ..Default::default() }, "x2", "d3", now()).unwrap();
        assert_eq!((r2["created"].clone(), r2["pinId"].clone()), (json!(false), r["pinId"].clone()));
        assert_eq!(p2.unwrap().revision, 2);
        // Same title again = the same pin, nothing new.
        let (r3, p3) = s.agent_write(&a, new(Kind::Decide, "pick 4 or auto?"), "x3", "d4", now()).unwrap();
        assert!(p3.is_none());
        assert_eq!(r3["pinId"], r["pinId"]);
        // Another agent cannot address it.
        let id = r["pinId"].as_str().unwrap().to_string();
        assert!(matches!(s.agent_write(&caller("cutex.b"), AgentRequest { pin_id: Some(id.clone()), resolve: Some(State::Done), ..Default::default() }, "y", "e", now()), Err(PinError::NotFound)));
        let (_, done) = s.agent_write(&a, AgentRequest { key: Some("k".into()), resolve: Some(State::Dropped), note: Some("you chose in chat".into()), ..Default::default() }, "x4", "d5", now()).unwrap();
        let done = done.unwrap();
        assert_eq!((done.state, done.resolved_by.as_deref(), done.resolution.as_deref()), (State::Dropped, Some("agent"), Some("you chose in chat")));
        assert!(matches!(s.agent_write(&a, AgentRequest { pin_id: Some(id), resolve: Some(State::Done), ..Default::default() }, "x5", "d6", now()), Err(PinError::NotOpen)));
        // A key is free again once its pin is resolved.
        assert_eq!(s.agent_write(&a, AgentRequest { key: Some("k".into()), ..new(Kind::Todo, "next") }, "x6", "d7", now()).unwrap().0["created"], true);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn limits_hold() {
        let (s, dir) = store();
        let a = caller("cutex.a");
        for i in 0..MAX_OPEN_PER_AGENT {
            s.agent_write(&a, new(Kind::Note, &format!("n{i}")), &format!("a{i}"), "d", now()).unwrap();
        }
        assert!(matches!(s.agent_write(&a, new(Kind::Note, "one more"), "over", "d", now()), Err(PinError::TooManyOpen(20))));
        // Writes per hour: 20 creates so far (the refused one is not counted); updates fill the rest.
        let list = s.agent_list("cutex.a", false).unwrap();
        let mut writes = MAX_OPEN_PER_AGENT;
        for (i, pin) in list.iter().enumerate() {
            if writes >= MAX_WRITES_PER_HOUR {
                break;
            }
            s.agent_write(&a, AgentRequest { pin_id: Some(pin.pin_id.clone()), detail: Some("more".into()), ..Default::default() }, &format!("u{i}"), "d", now()).unwrap();
            writes += 1;
        }
        assert!(matches!(s.agent_write(&a, AgentRequest { pin_id: Some(list[0].pin_id.clone()), resolve: Some(State::Done), ..Default::default() }, "late", "d", now()), Err(PinError::RateLimited)));
        // An hour later it works again.
        assert!(s.agent_write(&a, AgentRequest { pin_id: Some(list[0].pin_id.clone()), resolve: Some(State::Done), ..Default::default() }, "later", "d", now() + ChronoDuration::minutes(61)).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn owner_resolves_reopens_and_deletes_any_pin() {
        let (s, dir) = store();
        let (r, _) = s.agent_write(&caller("cutex.a"), new(Kind::Todo, "plug the phone in"), "x", "d", now()).unwrap();
        let id = r["pinId"].as_str().unwrap();
        let done = s.owner_set_state(id, State::Done, Some(1), now()).unwrap();
        assert_eq!((done.state, done.resolved_by.as_deref(), done.revision), (State::Done, Some("owner"), 2));
        assert!(matches!(s.owner_set_state(id, State::Open, Some(1), now()), Err(PinError::RevisionConflict(2))));
        let list = s.owner_list(true, None, 10).unwrap();
        assert_eq!((list.pins.len(), list.open_count), (1, 0));
        assert_eq!(s.owner_list(false, None, 10).unwrap().pins.len(), 0);
        assert_eq!(s.owner_set_state(id, State::Open, None, now()).unwrap().resolved_by, None);
        assert_eq!(s.owner_list(false, Some("cutex.a"), 0).unwrap().pins.len(), 1);
        assert_eq!(s.owner_list(false, Some("cutex.b"), 0).unwrap().pins.len(), 0);
        s.owner_delete(id).unwrap();
        assert!(matches!(s.owner_delete(id), Err(PinError::NotFound)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
