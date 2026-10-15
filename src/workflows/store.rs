//! Durable runs, the step journal and logs (design §4.1).
//!
//! `~/.cutex/runtime/workflows/runs.sqlite3` (WAL; back up with SQLite
//! `.backup`, never by copying db + wal + shm) plus content-addressed scripts
//! in `scripts/<sha256>.js`. Created lazily on first use; an older cutex never
//! reads it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{params, Connection, OptionalExtension};

use super::model::*;

const DB_FILE: &str = "runs.sqlite3";
const SCHEMA_VERSION: i64 = 1;

pub struct WorkflowStore {
    root: PathBuf,
    connection: Mutex<Connection>,
}

#[derive(Clone, Debug, Default)]
pub struct RunFilter {
    /// Some(true) = open runs only, Some(false) = finished only.
    pub open: Option<bool>,
    pub author_session: Option<String>,
    pub project_id: Option<String>,
    pub limit: usize,
}

fn private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    let _ = path;
}

impl WorkflowStore {
    pub fn default_root() -> anyhow::Result<PathBuf> {
        Ok(crate::config::paths::runtime_dir()?.join("workflows"))
    }

    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        private_dir(&root)?;
        private_dir(&root.join("scripts"))?;
        let path = root.join(DB_FILE);
        let connection = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        private_file(&path);
        connection.busy_timeout(Duration::from_secs(5))?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        anyhow::ensure!(mode.eq_ignore_ascii_case("wal"), "workflow store requires WAL mode");
        connection.pragma_update(None, "synchronous", "FULL")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        anyhow::ensure!(version <= SCHEMA_VERSION, "workflow store requires a newer cutex");
        if version == 0 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS workflow_run (
                   run_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, state TEXT NOT NULL,
                   author_session TEXT, project_id TEXT, created_at TEXT NOT NULL,
                   stop_request TEXT, data TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS workflow_run_seq ON workflow_run(seq DESC);
                 CREATE TABLE IF NOT EXISTS workflow_step (
                   run_id TEXT NOT NULL, step_key TEXT NOT NULL, call_index INTEGER NOT NULL,
                   state TEXT NOT NULL, session_id TEXT, data TEXT NOT NULL,
                   PRIMARY KEY(run_id, step_key)
                 );
                 CREATE INDEX IF NOT EXISTS workflow_step_session ON workflow_step(session_id);
                 CREATE TABLE IF NOT EXISTS workflow_log (
                   run_id TEXT NOT NULL, seq INTEGER NOT NULL, at TEXT NOT NULL,
                   level TEXT NOT NULL, text TEXT NOT NULL,
                   PRIMARY KEY(run_id, seq)
                 );
                 PRAGMA user_version=1;",
            )?;
        }
        for sidecar in ["-wal", "-shm"] {
            private_file(&root.join(format!("{DB_FILE}{sidecar}")));
        }
        Ok(Self { root, connection: Mutex::new(connection) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Store a script by content; returns its sha256.
    pub fn put_script(&self, text: &str) -> anyhow::Result<String> {
        let sha = sha256_hex(text.as_bytes());
        let path = self.root.join("scripts").join(format!("{sha}.js"));
        if !path.exists() {
            crate::config::atomic::write_private_bytes_atomic(&path, text.as_bytes())?;
        }
        Ok(sha)
    }

    pub fn read_script(&self, sha: &str) -> anyhow::Result<String> {
        anyhow::ensure!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "bad script digest");
        Ok(std::fs::read_to_string(self.root.join("scripts").join(format!("{sha}.js")))?)
    }

    /// Insert a new run; assigns and returns its host-wide `seq`.
    pub fn insert_run(&self, run: &RunRecord) -> anyhow::Result<u64> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let max: Option<i64> = tx.query_row("SELECT MAX(seq) FROM workflow_run", [], |row| row.get(0))?;
        let seq = max.unwrap_or(0) as u64 + 1;
        let mut run = run.clone();
        run.seq = seq;
        tx.execute(
            "INSERT INTO workflow_run(run_id, seq, state, author_session, project_id, created_at, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.run_id,
                seq as i64,
                run.state.as_str(),
                run.author.session(),
                run.project_id,
                run.created_at,
                serde_json::to_string(&run)?
            ],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// A stop asked for by another process (Agent Bus `workflow_stop`).
    pub fn request_stop(&self, run_id: &str, reason: StopReason) -> anyhow::Result<bool> {
        let reason = serde_json::to_value(reason)?;
        Ok(self.db().execute("UPDATE workflow_run SET stop_request=?2 WHERE run_id=?1", params![run_id, reason.as_str()])? == 1)
    }

    pub fn stop_request(&self, run_id: &str) -> anyhow::Result<Option<StopReason>> {
        let value: Option<Option<String>> = self
            .db()
            .query_row("SELECT stop_request FROM workflow_run WHERE run_id=?1", [run_id], |row| row.get(0))
            .optional()?;
        Ok(value.flatten().and_then(|text| serde_json::from_value(serde_json::Value::String(text)).ok()))
    }

    pub fn clear_stop_request(&self, run_id: &str) -> anyhow::Result<()> {
        self.db().execute("UPDATE workflow_run SET stop_request=NULL WHERE run_id=?1", [run_id])?;
        Ok(())
    }

    /// The session is a workflow step session (no workflow tools for it).
    pub fn is_step_session(&self, session_id: &str) -> anyhow::Result<bool> {
        let found: Option<i64> = self
            .db()
            .query_row("SELECT 1 FROM workflow_step WHERE session_id=?1 LIMIT 1", [session_id], |row| row.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// Save a run (revision + 1). Returns the saved record.
    pub fn save_run(&self, run: &mut RunRecord) -> anyhow::Result<()> {
        run.revision += 1;
        let changed = self.db().execute(
            "UPDATE workflow_run SET state=?2, data=?3 WHERE run_id=?1",
            params![run.run_id, run.state.as_str(), serde_json::to_string(run)?],
        )?;
        anyhow::ensure!(changed == 1, "workflow run {} not found", run.run_id);
        Ok(())
    }

    pub fn run(&self, run_id: &str) -> anyhow::Result<Option<RunRecord>> {
        let data: Option<String> = self
            .db()
            .query_row("SELECT data FROM workflow_run WHERE run_id=?1", [run_id], |row| row.get(0))
            .optional()?;
        data.map(|text| serde_json::from_str(&text).context("workflow run record")).transpose()
    }

    pub fn runs(&self, filter: &RunFilter) -> anyhow::Result<Vec<RunRecord>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT data FROM workflow_run
             WHERE (?1 IS NULL OR author_session = ?1) AND (?2 IS NULL OR project_id = ?2)
             ORDER BY seq DESC",
        )?;
        let rows = statement.query_map(params![filter.author_session, filter.project_id], |row| row.get::<_, String>(0))?;
        let limit = if filter.limit == 0 { 100 } else { filter.limit };
        let mut out = Vec::new();
        for row in rows {
            let run: RunRecord = serde_json::from_str(&row?)?;
            if let Some(open) = filter.open {
                if open == run.state.is_terminal() {
                    continue;
                }
            }
            out.push(run);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn open_runs(&self) -> anyhow::Result<Vec<RunRecord>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT data FROM workflow_run WHERE state IN ('pendingApproval','running','stopping') ORDER BY seq",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn save_step(&self, step: &StepRecord) -> anyhow::Result<()> {
        let state = serde_json::to_value(step.state)?;
        self.db().execute(
            "INSERT INTO workflow_step(run_id, step_key, call_index, state, session_id, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(run_id, step_key) DO UPDATE SET call_index=excluded.call_index, state=excluded.state,
               session_id=excluded.session_id, data=excluded.data",
            params![step.run_id, step.step_key, step.call_index as i64, state.as_str().unwrap_or(""), step.session_id, serde_json::to_string(step)?],
        )?;
        Ok(())
    }

    pub fn steps(&self, run_id: &str) -> anyhow::Result<Vec<StepRecord>> {
        let db = self.db();
        let mut statement = db.prepare("SELECT data FROM workflow_step WHERE run_id=?1 ORDER BY call_index, step_key")?;
        let rows = statement.query_map([run_id], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn step(&self, run_id: &str, step_key: &str) -> anyhow::Result<Option<StepRecord>> {
        let data: Option<String> = self
            .db()
            .query_row("SELECT data FROM workflow_step WHERE run_id=?1 AND step_key=?2", [run_id, step_key], |row| row.get(0))
            .optional()?;
        data.map(|text| Ok(serde_json::from_str(&text)?)).transpose()
    }

    /// Append a log line; None once the run has `MAX_LOG_LINES`.
    pub fn append_log(&self, run_id: &str, level: &str, text: &str) -> anyhow::Result<Option<LogLine>> {
        let db = self.db();
        let count: i64 = db.query_row("SELECT COUNT(*) FROM workflow_log WHERE run_id=?1", [run_id], |row| row.get(0))?;
        if count as usize >= MAX_LOG_LINES {
            return Ok(None);
        }
        let mut text = text.to_string();
        if text.len() > MAX_LOG_BYTES {
            let mut cut = MAX_LOG_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push('…');
        }
        let line = LogLine { seq: count as u64 + 1, at: now_rfc3339(), level: level.to_string(), text };
        db.execute(
            "INSERT INTO workflow_log(run_id, seq, at, level, text) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, line.seq as i64, line.at, line.level, line.text],
        )?;
        Ok(Some(line))
    }

    pub fn logs(&self, run_id: &str, after: u64, limit: usize) -> anyhow::Result<Vec<LogLine>> {
        let db = self.db();
        let mut statement = db.prepare("SELECT seq, at, level, text FROM workflow_log WHERE run_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let rows = statement.query_map(params![run_id, after as i64, limit as i64], |row| {
            Ok(LogLine { seq: row.get::<_, i64>(0)? as u64, at: row.get(1)?, level: row.get(2)?, text: row.get(3)? })
        })?;
        rows.map(|row| Ok(row?)).collect()
    }

    /// The last `n` log lines.
    pub fn log_tail(&self, run_id: &str, n: usize) -> anyhow::Result<Vec<LogLine>> {
        let total: i64 = self.db().query_row("SELECT COUNT(*) FROM workflow_log WHERE run_id=?1", [run_id], |row| row.get(0))?;
        self.logs(run_id, (total as u64).saturating_sub(n as u64), n)
    }

    pub fn delete_run(&self, run_id: &str) -> anyhow::Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM workflow_log WHERE run_id=?1", [run_id])?;
        tx.execute("DELETE FROM workflow_step WHERE run_id=?1", [run_id])?;
        tx.execute("DELETE FROM workflow_run WHERE run_id=?1", [run_id])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn sample_run(id: &str, seq: u64) -> RunRecord {
        RunRecord {
            run_id: id.into(),
            seq,
            name: "n".into(),
            description: "d".into(),
            phases: vec![],
            author: Author::Agent { cutex_session_id: "cutex.a".into(), name: None },
            project_id: None,
            cwd: "/tmp".into(),
            script_sha256: "0".repeat(64),
            args: json!(null),
            args_sha256: String::new(),
            limits: Limits::default(),
            default_profile: None,
            step_profiles: vec![],
            models: vec![],
            approval_id: None,
            state: RunState::Running,
            stop_reason: None,
            error: None,
            current_phase: None,
            counts: StepCounts::default(),
            tokens: 0,
            result: None,
            resumed_from: None,
            draft_path: None,
            warnings: vec![],
            created_at: now_rfc3339(),
            started_at: None,
            ended_at: None,
            start_action_id: None,
            notified: false,
            revision: 0,
        }
    }

    #[test]
    fn runs_steps_logs_and_scripts_round_trip() {
        let dir = tempfile_dir("store");
        let store = WorkflowStore::open(&dir).unwrap();
        let mut run = sample_run("wr-1", 1);
        assert_eq!(store.insert_run(&run).unwrap(), 1);
        run.state = RunState::Completed;
        store.save_run(&mut run).unwrap();
        assert_eq!(store.run("wr-1").unwrap().unwrap().state, RunState::Completed);
        assert_eq!(store.insert_run(&sample_run("wr-2", 0)).unwrap(), 2);
        assert!(store.request_stop("wr-2", StopReason::Model).unwrap());
        assert_eq!(store.stop_request("wr-2").unwrap(), Some(StopReason::Model));
        store.clear_stop_request("wr-2").unwrap();
        assert_eq!(store.stop_request("wr-2").unwrap(), None);
        store.delete_run("wr-2").unwrap();
        assert!(store.open_runs().unwrap().is_empty());
        assert_eq!(store.runs(&RunFilter { open: Some(false), ..Default::default() }).unwrap().len(), 1);
        let sha = store.put_script("return 1").unwrap();
        assert_eq!(store.read_script(&sha).unwrap(), "return 1");
        for i in 0..3 {
            store.append_log("wr-1", "info", &format!("line {i}")).unwrap();
        }
        assert_eq!(store.log_tail("wr-1", 2).unwrap().iter().map(|l| l.seq).collect::<Vec<_>>(), vec![2, 3]);
        let step = StepRecord {
            run_id: "wr-1".into(),
            step_key: "k#1".into(),
            call_index: 0,
            phase: None,
            label: None,
            prompt: "p".into(),
            opts: json!({}),
            profile: None,
            session_id: None,
            ask_id: None,
            ask_ids: vec![],
            state: StepState::Done,
            result: Some(json!(1)),
            error: None,
            tokens: 3,
            repairs: 0,
            cached_from: None,
            activity: None,
            created_at: now_rfc3339(),
            started_at: None,
            ended_at: None,
        };
        store.save_step(&step).unwrap();
        assert_eq!(store.steps("wr-1").unwrap(), vec![step]);
        store.delete_run("wr-1").unwrap();
        assert!(store.run("wr-1").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    pub fn tempfile_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-workflows-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
