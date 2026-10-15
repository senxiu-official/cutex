use super::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DB_FILE: &str = "notes.sqlite3";

/// A separate database, not the session store. Each call opens its own bounded
/// transaction, making CAS and nonce handling safe across daemon processes.
#[derive(Clone)]
pub struct NotesStore {
    root: PathBuf,
    publish: Arc<dyn Fn(NoteChange) + Send + Sync>,
}

impl NotesStore {
    /// Lifecycle hooks have nothing to mark before the first note exists, and
    /// must not create `notes/` as a side effect of a revert or deletion (a
    /// binary rollback then leaves no new state behind). An unreadable path
    /// counts as existing so the hook still reports its storage error.
    pub fn default_exists() -> bool {
        match crate::config::paths::config_dir() {
            Ok(dir) => !matches!(
                std::fs::symlink_metadata(dir.join("notes").join(DB_FILE)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            Err(_) => true,
        }
    }

    pub fn open_default() -> NotesResult<Self> {
        Self::open(crate::config::paths::config_dir()?.join("notes"))
    }

    pub fn open(root: impl Into<PathBuf>) -> NotesResult<Self> {
        let store = Self {
            root: root.into(),
            publish: Arc::new(publish_change),
        };
        crate::management::client_tokens::ensure_private_dir(&store.root)?;
        let mut connection = store.connection()?;
        // Read the version under the migration lock, including on concurrent first opens.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 2 {
            return Err(NotesError::Storage(anyhow::anyhow!(
                "notes database requires a newer cutex"
            )));
        }
        if version == 0 {
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS notes (
               id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL,
               scope_kind TEXT NOT NULL, scope_id TEXT NOT NULL, anchor TEXT,
               revision INTEGER NOT NULL CHECK (revision > 0),
               created_at TEXT NOT NULL, updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS notes_updated ON notes(updated_at DESC, id DESC);
             CREATE INDEX IF NOT EXISTS notes_scope ON notes(scope_kind, scope_id);
             CREATE TABLE IF NOT EXISTS note_pins (
               note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
               kind TEXT NOT NULL, scope_id TEXT NOT NULL,
               PRIMARY KEY(note_id, kind, scope_id)
             );
             CREATE INDEX IF NOT EXISTS note_pin_scope ON note_pins(kind, scope_id, note_id);
             CREATE TABLE IF NOT EXISTS note_nonces (
               nonce TEXT PRIMARY KEY, request_hash TEXT NOT NULL, note_id TEXT NOT NULL
             );
             PRAGMA user_version=1;",
            )?;
        }
        if version < 2 {
            transaction.execute_batch(
                "ALTER TABLE notes ADD COLUMN anchor_state TEXT NOT NULL DEFAULT 'ok'
                   CHECK(anchor_state IN ('ok','reverted','sessionDeleted'));
                 ALTER TABLE notes ADD COLUMN anchor_state_at TEXT;
                 CREATE INDEX notes_anchor_session ON notes(json_extract(anchor,'$.sessionId'));
                 PRAGMA user_version=2;",
            )?;
        }
        transaction.commit()?;
        store.secure_files()?;
        Ok(store)
    }

    fn connection(&self) -> NotesResult<Connection> {
        let path = self.root.join(DB_FILE);
        secure_file(&path, true)?;
        // Check pre-existing sidecars too, before SQLite follows their paths.
        self.secure_files()?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        enable_wal(&connection)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        self.secure_files()?;
        Ok(connection)
    }

    fn secure_files(&self) -> NotesResult<()> {
        for name in [
            DB_FILE.to_string(),
            format!("{DB_FILE}-wal"),
            format!("{DB_FILE}-shm"),
        ] {
            let path = self.root.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => secure_file(&path, false)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(NotesError::Storage(error.into())),
            }
        }
        Ok(())
    }

    pub fn create(&self, request: CreateNote) -> NotesResult<Note> {
        let request = request.validated()?;
        let hash = digest(&serde_json::to_vec(&request)?);
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = transaction
            .query_row(
                "SELECT request_hash, note_id FROM note_nonces WHERE nonce=?1",
                [&request.client_nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((old_hash, id)) = existing {
            if old_hash != hash {
                return Err(NotesError::NonceConflict);
            }
            return match read_note(&transaction, &id) {
                Err(NotesError::NotFound) => Err(NotesError::Deleted),
                result => result,
            };
        }
        let timestamp = now();
        let note = Note {
            id: format!("note-{}", uuid::Uuid::new_v4()),
            title: request.title,
            body: request.body,
            scope: request.scope,
            pins: request.pins,
            anchor: request.anchor,
            anchor_state: AnchorState::Ok,
            anchor_state_at: None,
            revision: 1,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        let (kind, id) = note.scope.columns();
        transaction.execute(
            "INSERT INTO notes(id,title,body,scope_kind,scope_id,anchor,revision,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![note.id, note.title, note.body, kind, id, note.anchor.as_ref().map(serde_json::to_string).transpose()?,
                note.revision as i64, note.created_at, note.updated_at],
        )?;
        write_pins(&transaction, &note)?;
        transaction.execute(
            "INSERT INTO note_nonces(nonce,request_hash,note_id) VALUES(?1,?2,?3)",
            params![request.client_nonce, hash, note.id],
        )?;
        transaction.commit()?;
        (self.publish)(NoteChange::new(&note, NoteChangeKind::Created));
        Ok(note)
    }

    pub fn get(&self, id: &str) -> NotesResult<Note> {
        validate_note_id(id)?;
        let mut connection = self.connection()?;
        // Read the record and its pins from the same snapshot.
        let transaction = connection.transaction()?;
        let note = read_note(&transaction, id)?;
        transaction.commit()?;
        Ok(note)
    }

    pub fn patch(&self, id: &str, request: PatchNote) -> NotesResult<Note> {
        validate_note_id(id)?;
        let request = request.validated()?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut note = read_note(&transaction, id)?;
        check_revision(&note, request.expected_revision)?;
        if let Some(title) = request.title {
            note.title = title;
        }
        if let Some(body) = request.body {
            note.body = body;
        }
        if let Some(pins) = request.pins {
            note.pins = pins;
        }
        note.revision = note
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
            .ok_or_else(|| invalid("note revision is exhausted"))?;
        note.updated_at = now();
        transaction.execute(
            "UPDATE notes SET title=?2,body=?3,revision=?4,updated_at=?5 WHERE id=?1",
            params![
                id,
                note.title,
                note.body,
                note.revision as i64,
                note.updated_at
            ],
        )?;
        write_pins(&transaction, &note)?;
        transaction.commit()?;
        (self.publish)(NoteChange::new(&note, NoteChangeKind::Updated));
        Ok(note)
    }

    pub fn delete(&self, id: &str, expected_revision: u64) -> NotesResult<()> {
        validate_note_id(id)?;
        validate_revision(expected_revision)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let note = read_note(&transaction, id)?;
        check_revision(&note, expected_revision)?;
        transaction.execute("DELETE FROM notes WHERE id=?1", [id])?;
        transaction.commit()?;
        (self.publish)(NoteChange::new(&note, NoteChangeKind::Deleted));
        Ok(())
    }

    /// The confirmed removed ids include the selected turn and all newer turns.
    pub fn mark_reverted(
        &self,
        session_id: &str,
        removed_turn_ids: &[String],
    ) -> NotesResult<usize> {
        validate_identifier(session_id, "session id")?;
        if removed_turn_ids.is_empty() {
            return Ok(0);
        }
        self.mark_anchors(session_id, Some(removed_turn_ids), AnchorState::Reverted)
    }

    /// The note remains readable after its source disappears. An explicit anchor
    /// takes precedence over scope; a scope-only session note also loses its source.
    pub fn mark_session_deleted(&self, session_id: &str) -> NotesResult<usize> {
        validate_identifier(session_id, "session id")?;
        self.mark_anchors(session_id, None, AnchorState::SessionDeleted)
    }

    fn mark_anchors(
        &self,
        session_id: &str,
        removed: Option<&[String]>,
        state: AnchorState,
    ) -> NotesResult<usize> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM notes WHERE
                  (?2 IS NOT NULL AND anchor_state='ok'
                    AND json_extract(anchor,'$.sessionId')=?1
                    AND json_extract(anchor,'$.turnId') IN (SELECT value FROM json_each(?2)))
                  OR (?2 IS NULL AND anchor_state!='sessionDeleted'
                    AND (json_extract(anchor,'$.sessionId')=?1
                      OR (anchor IS NULL AND scope_kind='session' AND scope_id=?1)))",
            )?;
            let result = statement
                .query_map(
                    params![session_id, removed.map(serde_json::to_string).transpose()?],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            result
        };
        let timestamp = now();
        let mut changes = Vec::with_capacity(ids.len());
        for id in ids {
            let mut note = read_note(&transaction, &id)?;
            note.revision = note
                .revision
                .checked_add(1)
                .filter(|revision| *revision <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                .ok_or_else(|| invalid("note revision is exhausted"))?;
            note.anchor_state = state;
            note.anchor_state_at = Some(timestamp.clone());
            note.updated_at = timestamp.clone();
            transaction.execute(
                "UPDATE notes SET anchor_state=?2,anchor_state_at=?3,revision=?4,updated_at=?3 WHERE id=?1",
                params![id, state.as_str(), timestamp, note.revision as i64],
            )?;
            changes.push(NoteChange::new(&note, NoteChangeKind::Anchor));
        }
        transaction.commit()?;
        let count = changes.len();
        for change in changes {
            (self.publish)(change);
        }
        Ok(count)
    }

    /// Membership comes from the current Agent Management snapshot, never pins.
    /// Accepting it as data keeps these store tests independent of all agents.
    pub fn list(&self, query: &NotesQuery, project_sessions: &[String]) -> NotesResult<NotesPage> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let (kind, id) = query.scope.columns();
        let cursor_time = query
            .cursor
            .as_ref()
            .map(|cursor| cursor.updated_at.as_str());
        let cursor_id = query.cursor.as_ref().map(|cursor| cursor.id.as_str());
        let mut statement = transaction.prepare(
            "SELECT n.id FROM notes n WHERE
             ((NOT ?3 AND (?1='host' OR (n.scope_kind=?1 AND n.scope_id=?2)
               OR (?1='project' AND n.scope_kind='session'
                 AND n.scope_id IN (SELECT value FROM json_each(?4)))))
               OR EXISTS(SELECT 1 FROM note_pins p WHERE p.note_id=n.id AND p.kind=?1 AND p.scope_id=?2))
             AND (?5='' OR instr(lower(n.title),lower(?5))>0 OR instr(lower(n.body),lower(?5))>0)
             AND (?6 IS NULL OR (n.updated_at,n.id)<(?6,?7))
             ORDER BY n.updated_at DESC,n.id DESC LIMIT ?8",
        )?;
        let ids = statement
            .query_map(
                params![
                    kind,
                    id,
                    query.pinned_only,
                    serde_json::to_string(project_sessions)?,
                    query.query,
                    cursor_time,
                    cursor_id,
                    (query.limit + 1) as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = ids.len() > query.limit;
        let notes = ids
            .iter()
            .take(query.limit)
            .map(|id| read_note(&transaction, id))
            .collect::<NotesResult<Vec<_>>>()?;
        let next_cursor = if has_more {
            notes
                .last()
                .map(|note| {
                    serde_json::to_vec(&Cursor {
                        filter: query.fingerprint(),
                        updated_at: note.updated_at.clone(),
                        id: note.id.clone(),
                    })
                    .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
                })
                .transpose()?
        } else {
            None
        };
        drop(statement);
        transaction.commit()?;
        Ok(NotesPage { notes, next_cursor })
    }
}

fn enable_wal(connection: &Connection) -> NotesResult<()> {
    // Concurrent first opens may hit SQLite's lock-upgrade deadlock avoidance:
    // journal_mode returns SQLITE_BUSY immediately even with a busy timeout.
    // Retry only this idempotent initialization, never a note mutation.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0)) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(_) => {
                return Err(NotesError::Storage(anyhow::anyhow!(
                    "notes database requires WAL mode"
                )))
            }
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn secure_file(path: &Path, create: bool) -> NotesResult<()> {
    #[cfg(windows)]
    {
        match crate::platform::private_fs::open_private_file_path(path, create, true) {
            Ok(_) => (),
            // A concurrent connection may close and remove its WAL sidecars.
            Err(error) if !create && error.io_kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(NotesError::Storage(error.into())),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(NotesError::Storage(error.into())),
        };
        if !file
            .metadata()
            .map_err(|error| NotesError::Storage(error.into()))?
            .is_file()
        {
            return Err(NotesError::Storage(anyhow::anyhow!(
                "notes database is not a regular file"
            )));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| NotesError::Storage(error.into()))?;
    }
    Ok(())
}

fn check_revision(note: &Note, expected: u64) -> NotesResult<()> {
    if note.revision != expected {
        return Err(NotesError::RevisionConflict {
            current_revision: note.revision,
        });
    }
    Ok(())
}

fn write_pins(connection: &Connection, note: &Note) -> NotesResult<()> {
    connection.execute("DELETE FROM note_pins WHERE note_id=?1", [&note.id])?;
    for pin in &note.pins {
        let (kind, id) = pin.columns();
        connection.execute(
            "INSERT INTO note_pins(note_id,kind,scope_id) VALUES(?1,?2,?3)",
            params![note.id, kind, id],
        )?;
    }
    Ok(())
}

fn stored_scope(kind: String, id: String) -> NotesResult<NoteScope> {
    match kind.as_str() {
        "session" => Ok(NoteScope::Session { id }),
        "project" => Ok(NoteScope::Project { id }),
        "host" if id.is_empty() => Ok(NoteScope::Host {}),
        _ => Err(NotesError::Storage(anyhow::anyhow!(
            "invalid stored note scope"
        ))),
    }
}

fn read_note(connection: &Connection, id: &str) -> NotesResult<Note> {
    let row = connection.query_row(
        "SELECT title,body,scope_kind,scope_id,anchor,revision,created_at,updated_at,anchor_state,anchor_state_at FROM notes WHERE id=?1", [id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?, row.get::<_, i64>(5)?, row.get::<_, String>(6)?, row.get::<_, String>(7)?,
            row.get::<_, String>(8)?, row.get::<_, Option<String>>(9)?)),
    ).optional()?.ok_or(NotesError::NotFound)?;
    if row.5 <= 0 || row.5 as u64 > crate::role_revision::MAX_JSON_SAFE_INTEGER {
        return Err(NotesError::Storage(anyhow::anyhow!(
            "invalid stored note revision"
        )));
    }
    let mut statement =
        connection.prepare("SELECT kind,scope_id FROM note_pins WHERE note_id=?1")?;
    let pin_rows = statement
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut pins = pin_rows
        .into_iter()
        .map(|(kind, id)| stored_scope(kind, id))
        .collect::<NotesResult<Vec<_>>>()?;
    pins.sort();
    Ok(Note {
        id: id.to_string(),
        title: row.0,
        body: row.1,
        scope: stored_scope(row.2, row.3)?,
        pins,
        anchor: row
            .4
            .map(|anchor| serde_json::from_str(&anchor))
            .transpose()?,
        revision: row.5 as u64,
        created_at: row.6,
        updated_at: row.7,
        anchor_state: match row.8.as_str() {
            "ok" => AnchorState::Ok,
            "reverted" => AnchorState::Reverted,
            "sessionDeleted" => AnchorState::SessionDeleted,
            _ => {
                return Err(NotesError::Storage(anyhow::anyhow!(
                    "invalid stored anchor state"
                )))
            }
        },
        anchor_state_at: row.9,
    })
}

#[cfg(test)]
#[path = "store/tests.rs"]
mod tests;
