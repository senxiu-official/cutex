//! Receipt order shared by business rows that can enter the automatic inbox.
//!
//! An ordinal describes the commit order of arrivals. It grants no permission
//! to submit, start a turn, or deliver a receipt.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use std::time::Duration;

/// Unlike MAX(ordinal), this value does not move backwards after a deletion.
pub(crate) fn watermark(path: &Path) -> anyhow::Result<i64> {
    if !path.try_exists()? {
        return Ok(0);
    }
    let db = super::read_connection(path)?;
    if !super::table_exists(&db, "sqlite_sequence")? {
        return Ok(0);
    }
    Ok(db
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='automatic_input_order'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Result,
    Bus,
}

impl Source {
    fn key(self) -> (&'static str, &'static str) {
        match self {
            Self::Result => ("asks", "deliveries"),
            Self::Bus => ("agent_bus", "message"),
        }
    }
}

/// An already-authorized, unsubmitted candidate. Its time is the recipient's
/// durable receive time, including for bus messages originally sent elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub source: Source,
    pub id: String,
    pub received_at: DateTime<Utc>,
    pub may_wake: bool,
}

/// Allocate no sequence on reads. A legacy row without an ordinal precedes
/// the post-upgrade sequence; ties within legacy rows have a stable identity
/// order. Rank lookup is keyed and never loads the underlying message body.
pub fn order(path: &Path, candidates: Vec<Candidate>) -> anyhow::Result<Vec<Candidate>> {
    let mut ranked = Vec::with_capacity(candidates.len());
    if path.try_exists()? && !candidates.is_empty() {
        let mut db = super::read_connection(path)?;
        let tx = db.transaction()?;
        if super::table_exists(&tx, "automatic_input_order")? {
            let mut lookup = tx.prepare(
                "SELECT ordinal FROM automatic_input_order WHERE store=?1 AND collection=?2 AND row_key=?3",
            )?;
            for candidate in candidates {
                let (store, collection) = candidate.source.key();
                let ordinal: Option<i64> = lookup
                    .query_row(params![store, collection, candidate.id], |row| row.get(0))
                    .optional()?;
                ranked.push((ordinal, candidate));
            }
        } else {
            ranked.extend(candidates.into_iter().map(|candidate| (None, candidate)));
        }
    } else {
        ranked.extend(candidates.into_iter().map(|candidate| (None, candidate)));
    }
    ranked.sort_by(|(a, left), (b, right)| {
        a.cmp(b).then_with(|| {
            (left.received_at, left.source, &left.id).cmp(&(
                right.received_at,
                right.source,
                &right.id,
            ))
        })
    });
    Ok(ranked.into_iter().map(|(_, candidate)| candidate).collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub id: i64,
    /// None means only a legal turn start can claim the waiting batch.
    deadline: Option<DateTime<Utc>>,
    anchor: Option<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Wait,
    ToolGap,
    TurnStart { may_wake: bool },
}

impl Window {
    pub fn deadline(&self) -> Option<DateTime<Utc>> {
        self.deadline
    }

    pub(crate) fn anchored_in(&self, candidates: &[Candidate]) -> bool {
        self.anchor.as_ref().is_some_and(|(source, id)| {
            candidates
                .iter()
                .any(|candidate| candidate.source.key().0 == source.as_str() && &candidate.id == id)
        })
    }
    pub fn readiness(&self, active_turn: bool, now: DateTime<Utc>, may_wake: bool) -> Readiness {
        if !active_turn {
            return Readiness::TurnStart { may_wake };
        }
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            Readiness::ToolGap
        } else {
            Readiness::Wait
        }
    }
}

/// Open (or recover) a recipient's collecting window. The first eligible
/// candidate pins both its deadline and the owner's wait choice. Later
/// observations can change candidate membership, never an open deadline.
/// Invoke from the same admission lane that settles the window.
pub fn window(
    path: &Path,
    session: &str,
    candidates: &[Candidate],
    wait: Option<Duration>,
) -> anyhow::Result<Window> {
    let first = candidates
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty automatic input window"))?;
    if path.try_exists()? {
        let db = super::read_connection(path)?;
        if super::table_exists(&db, "automatic_input_windows")? {
            if let Some(existing) =
                read_window(&db, session)?.filter(|window| window.anchored_in(candidates))
            {
                return Ok(existing);
            }
        }
    }
    let mut db = super::connection(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = read_window(&tx, session)? {
        if existing.anchored_in(candidates) {
            return Ok(existing);
        }
        tx.execute(
            "DELETE FROM automatic_input_windows WHERE id=?1",
            [existing.id],
        )?;
    }
    let deadline = wait
        .map(|delay| {
            let delay = chrono::Duration::from_std(delay)?;
            first
                .received_at
                .checked_add_signed(delay)
                .ok_or_else(|| anyhow::anyhow!("automatic input deadline out of range"))
        })
        .transpose()?;
    tx.execute(
        "INSERT INTO automatic_input_windows(session, deadline)
        SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM automatic_input_windows WHERE session=?1)",
        params![session, deadline.map(|at| at.to_rfc3339())],
    )?;
    tx.execute(
        "INSERT INTO automatic_input_window_anchors(window_id,source,input_id)
        SELECT id,?2,?3 FROM automatic_input_windows WHERE session=?1",
        params![session, first.source.key().0, first.id],
    )?;
    let result = read_window(&tx, session)?
        .ok_or_else(|| anyhow::anyhow!("automatic input window disappeared"))?;
    tx.commit()?;
    Ok(result)
}

fn read_window(db: &Connection, session: &str) -> anyhow::Result<Option<Window>> {
    let row: Option<(i64, Option<String>)> = db
        .query_row(
            "SELECT id,deadline FROM automatic_input_windows WHERE session=?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(id, deadline)| {
        Ok(Window {
            id,
            deadline: deadline.map(|value| value.parse()).transpose()?,
            anchor: if super::table_exists(db, "automatic_input_window_anchors")? {
                db.query_row(
                    "SELECT source,input_id FROM automatic_input_window_anchors WHERE window_id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
            } else {
                None
            },
        })
    })
    .transpose()
}

pub(crate) fn current_window(path: &Path, session: &str) -> anyhow::Result<Option<Window>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let db = super::read_connection(path)?;
    if !super::table_exists(&db, "automatic_input_windows")? {
        return Ok(None);
    }
    read_window(&db, session)
}

/// Called in the receiving lane after finding no eligible candidates.
pub(crate) fn clear_window(path: &Path, session: &str) -> anyhow::Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    let db = super::read_connection(path)?;
    if super::table_exists(&db, "automatic_input_windows")? {
        if let Some(window) = read_window(&db, session)? {
            retire_window(path, session, window.id)?;
        }
    }
    Ok(())
}

/// Retire a window only after promoting its reserved prefix or validating
/// that no pending member remains. A stale attempt cannot close a later one.
pub fn retire_window(path: &Path, session: &str, id: i64) -> anyhow::Result<bool> {
    let db = super::connection(path)?;
    Ok(db.execute(
        "DELETE FROM automatic_input_windows WHERE session=?1 AND id=?2",
        params![session, id],
    )? == 1)
}

pub(super) fn install(db: &Connection) -> anyhow::Result<()> {
    // Both sources write hot_store_rows in the same database. The ordinal
    // consequently becomes visible with the source row, or both roll back.
    // Updates keep their original ordinal. Legacy import runs unverified and
    // therefore cannot put an arbitrary import order ahead of live arrivals.
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS automatic_input_order (
            ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
            store TEXT NOT NULL,
            collection TEXT NOT NULL,
            row_key TEXT NOT NULL,
            UNIQUE (store, collection, row_key),
            FOREIGN KEY (store, collection, row_key)
                REFERENCES hot_store_rows(store, collection, row_key) ON DELETE CASCADE
         );
         CREATE TRIGGER IF NOT EXISTS automatic_input_arrival
         AFTER INSERT ON hot_store_rows
         WHEN ((NEW.store = 'asks' AND NEW.collection = 'deliveries')
            OR (NEW.store = 'agent_bus' AND NEW.collection = 'message'))
          AND EXISTS(SELECT 1 FROM hot_store_meta WHERE store = NEW.store AND verified = 1)
         BEGIN
            INSERT INTO automatic_input_order(store, collection, row_key)
            VALUES (NEW.store, NEW.collection, NEW.row_key);
         END;
         CREATE TABLE IF NOT EXISTS automatic_input_windows (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session TEXT NOT NULL UNIQUE,
            deadline TEXT
         );
         CREATE TABLE IF NOT EXISTS automatic_input_window_anchors (
            window_id INTEGER PRIMARY KEY REFERENCES automatic_input_windows(id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            input_id TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS automatic_input_publications (
            batch TEXT PRIMARY KEY,
            session TEXT NOT NULL,
            thread TEXT NOT NULL,
            generation TEXT NOT NULL,
            plan BLOB NOT NULL,
            phase INTEGER NOT NULL CHECK(phase IN (0,1,2,3)),
            next_check_ms INTEGER NOT NULL DEFAULT 0
         );
         CREATE UNIQUE INDEX IF NOT EXISTS automatic_input_active_publication
         ON automatic_input_publications(session) WHERE phase<>3;
         CREATE INDEX IF NOT EXISTS automatic_input_reconciliation
         ON automatic_input_publications(session,thread,generation,next_check_ms) WHERE phase=3;",
    )?;
    Ok(())
}

/// The earlier hot/cold rollback validator can preserve this exact schema
/// when it carries no ordered input state. Compare against the writer's DDL.
pub(crate) fn validate_empty_rollback_schema(db: &Connection) -> anyhow::Result<()> {
    fn schema(db: &Connection) -> rusqlite::Result<Vec<(String, String, String, Option<String>)>> {
        let mut statement = db.prepare(
            "SELECT type,name,tbl_name,sql FROM sqlite_schema
             WHERE tbl_name GLOB 'automatic_input_*' OR name GLOB 'automatic_input_*'
                OR name='sqlite_sequence' ORDER BY type,name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect()
    }

    let expected = Connection::open_in_memory()?;
    expected.execute_batch("CREATE TABLE hot_store_rows(dummy INTEGER);")?;
    install(&expected)?;
    anyhow::ensure!(schema(db)? == schema(&expected)?, "unknown management hot-store schema");
    let has_rows: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM automatic_input_order LIMIT 1)
             OR EXISTS(SELECT 1 FROM automatic_input_windows LIMIT 1)
             OR EXISTS(SELECT 1 FROM automatic_input_window_anchors LIMIT 1)
             OR EXISTS(SELECT 1 FROM automatic_input_publications LIMIT 1)
             OR EXISTS(SELECT 1 FROM sqlite_sequence WHERE name NOT IN
                ('automatic_input_order','automatic_input_windows') LIMIT 1)",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(!has_rows, "management hot-store contains ordered input state");
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;

    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE hot_store_meta(store TEXT PRIMARY KEY, verified INTEGER NOT NULL);
             CREATE TABLE hot_store_rows (
                store TEXT, collection TEXT, row_key TEXT, value_json BLOB,
                PRIMARY KEY (store, collection, row_key)
             ) WITHOUT ROWID;
             INSERT INTO hot_store_meta VALUES ('asks', 0), ('agent_bus', 1);",
        )
        .unwrap();
        install(&db).unwrap();
        db
    }

    fn put(db: &Connection, store: &str, collection: &str, id: &str) {
        db.execute(
            "INSERT INTO hot_store_rows VALUES (?1, ?2, ?3, x'7b7d')
             ON CONFLICT (store, collection, row_key) DO UPDATE SET value_json = x'7b7d'",
            params![store, collection, id],
        )
        .unwrap();
    }

    fn arrivals(db: &Connection) -> Vec<(i64, String)> {
        let mut query = db
            .prepare("SELECT ordinal, row_key FROM automatic_input_order ORDER BY ordinal")
            .unwrap();
        query
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn sources_share_one_sequence_without_reordering_on_updates_or_import() {
        let db = fixture();
        put(&db, "asks", "deliveries", "legacy");
        db.execute("UPDATE hot_store_meta SET verified=1", [])
            .unwrap();
        put(&db, "asks", "deliveries", "result-1");
        put(&db, "asks", "deliveries", "result-2");
        put(&db, "agent_bus", "message", "note");
        put(&db, "asks", "deliveries", "legacy");
        put(&db, "asks", "deliveries", "result-1");
        put(&db, "asks", "asks", "request");
        assert_eq!(
            arrivals(&db),
            vec![
                (1, "result-1".into()),
                (2, "result-2".into()),
                (3, "note".into())
            ]
        );
    }

    #[test]
    fn source_rollback_and_deletion_do_not_leave_order_rows() {
        let mut db = fixture();
        put(&db, "agent_bus", "message", "first");
        {
            let tx = db.transaction().unwrap();
            put(&tx, "agent_bus", "message", "rolled-back");
            tx.rollback().unwrap();
        }
        db.execute("DELETE FROM hot_store_rows", []).unwrap();
        assert!(arrivals(&db).is_empty());
        put(&db, "agent_bus", "message", "later");
        assert_eq!(arrivals(&db), vec![(2, "later".into())]);
    }

    #[test]
    fn authoritative_ask_and_bus_write_paths_use_one_order() {
        use super::super::{commit, RowChange, DATABASE_FILE};

        let root = std::env::temp_dir().join(format!("cutex-auto-order-{}", uuid::Uuid::new_v4()));
        let path = root.join(DATABASE_FILE);
        let row = |collection: &str, key: &str| RowChange::Put {
            collection: collection.into(),
            key: key.into(),
            value: b"{}".to_vec(),
        };

        commit(&path, "asks", 0, vec![row("deliveries", "result-1")]).unwrap();
        commit(&path, "agent_bus", 0, vec![row("message", "note")]).unwrap();
        assert!(commit(&path, "asks", 0, vec![row("deliveries", "stale")]).is_err());
        commit(&path, "asks", 1, vec![row("deliveries", "result-2")]).unwrap();
        let db = super::super::read_connection(&path).unwrap();
        assert_eq!(
            arrivals(&db),
            vec![
                (1, "result-1".into()),
                (2, "note".into()),
                (3, "result-2".into())
            ]
        );
        drop(db);
        let earlier = DateTime::from_timestamp(1, 0).unwrap();
        let later = DateTime::from_timestamp(2, 0).unwrap();
        let candidate = |source, id: &str, received_at| Candidate {
            source,
            id: id.into(),
            received_at,
            may_wake: source == Source::Result,
        };
        let ranked = order(
            &path,
            vec![
                candidate(Source::Result, "result-2", earlier),
                candidate(Source::Bus, "note", later),
                candidate(Source::Result, "result-1", later),
                candidate(Source::Bus, "legacy-note", later),
            ],
        )
        .unwrap();
        assert_eq!(
            ranked.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["legacy-note", "result-1", "note", "result-2"]
        );
        assert_eq!(watermark(&path).unwrap(), 3);
        let db = super::super::connection(&path).unwrap();
        db.execute("DELETE FROM hot_store_rows WHERE row_key='result-2'", [])
            .unwrap();
        drop(db);
        assert_eq!(watermark(&path).unwrap(), 3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collecting_window_keeps_first_deadline_and_survives_reopen() {
        let root = std::env::temp_dir().join(format!("cutex-auto-window-{}", uuid::Uuid::new_v4()));
        let path = root.join(super::super::DATABASE_FILE);
        let base = DateTime::from_timestamp(100, 0).unwrap();
        let first = Candidate {
            source: Source::Result,
            id: "first".into(),
            received_at: base,
            may_wake: true,
        };
        let later = Candidate {
            source: Source::Bus,
            id: "later".into(),
            received_at: base + chrono::Duration::seconds(80),
            may_wake: false,
        };
        let initially = window(
            &path,
            "receiver",
            std::slice::from_ref(&first),
            Some(Duration::from_secs(120)),
        )
        .unwrap();
        assert_eq!(
            initially,
            window(&path, "receiver", &[first.clone(), later.clone()], None).unwrap()
        );
        assert_eq!(
            initially.readiness(true, base + chrono::Duration::seconds(119), true),
            Readiness::Wait
        );
        assert_eq!(
            initially.readiness(true, base + chrono::Duration::seconds(120), true),
            Readiness::ToolGap
        );
        assert_eq!(
            initially.readiness(false, base, true),
            Readiness::TurnStart { may_wake: true }
        );

        let reanchored = window(
            &path,
            "receiver",
            std::slice::from_ref(&later),
            Some(Duration::from_secs(10)),
        )
        .unwrap();
        assert_ne!(reanchored.id, initially.id);
        assert_eq!(
            reanchored.deadline(),
            Some(later.received_at + chrono::Duration::seconds(10))
        );
        assert!(retire_window(&path, "receiver", reanchored.id).unwrap());
        let forever = window(&path, "receiver", std::slice::from_ref(&later), None).unwrap();
        assert_eq!(
            forever.readiness(true, base + chrono::Duration::hours(24), false),
            Readiness::Wait
        );
        assert_eq!(
            forever.readiness(false, base, false),
            Readiness::TurnStart { may_wake: false }
        );
        assert!(!retire_window(&path, "receiver", initially.id).unwrap());

        assert!(retire_window(&path, "receiver", forever.id).unwrap());
        let immediate = window(
            &path,
            "receiver",
            std::slice::from_ref(&later),
            Some(Duration::ZERO),
        )
        .unwrap();
        assert_eq!(
            immediate.readiness(true, later.received_at, false),
            Readiness::ToolGap
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
