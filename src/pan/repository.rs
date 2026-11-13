//! Pan task repository. Task state and its audit entry commit together in
//! pan/pan.sqlite; callers receive domain values, never database handles.
use super::store::create_private;
use super::*;
use rusqlite::{Connection, OptionalExtension};

#[cfg(test)]
#[test]
#[ignore = "requires an explicitly marked release business copy"]
fn release_data_copy_pan_validate_and_reopen() {
    let home = PathBuf::from(std::env::var_os("CUTEX_RELEASE_DATA_COPY").expect("explicit copy"));
    assert!(home.is_absolute() && home.join(".cutex-release-data-copy").is_file());
    assert_eq!(crate::config::paths::config_dir().unwrap().canonicalize().unwrap(), home.canonicalize().unwrap());
    let root = home.join("pan");
    if !root.join("pan.sqlite").exists() { eprintln!("RELEASE DATA COPY Pan absent"); return; }
    assert!(!root.join("key").exists() && !root.join("credentials").exists());
    for pass in 0..2 {
        let service = Service::open(root.clone(), provider::Provider::fixture("http://127.0.0.1:9")).unwrap();
        let db = service.db().unwrap();
        let mut tasks = db.prepare("SELECT body FROM pan_downloads").unwrap();
        let mut count = 0;
        for body in tasks.query_map([], |row| row.get::<_, String>(0)).unwrap() {
            let _: Download = serde_json::from_str(&body.unwrap()).unwrap();
            count += 1;
        }
        let mut log = db.prepare("SELECT body FROM pan_log").unwrap();
        for body in log.query_map([], |row| row.get::<_, String>(0)).unwrap() {
            let _: Value = serde_json::from_str(&body.unwrap()).unwrap();
        }
        eprintln!("RELEASE DATA COPY Pan pass={pass} tasks={count}");
    }
}

impl Service {
    pub(super) fn initialize(&self) -> Result<()> {
        let path = self.root.join("pan.sqlite");
        if !path.exists() {
            match create_private(&path) {
                Ok(_) => {}
                Err(_) if path.exists() => {}
                Err(e) => return Err(Error::storage(e)),
            }
        }
        let db = Connection::open(path).map_err(Error::storage)?;
        db.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(Error::storage)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS pan_downloads(id TEXT PRIMARY KEY, caller TEXT NOT NULL, request_id TEXT NOT NULL,
            remote TEXT NOT NULL, target_dir TEXT NOT NULL, state TEXT NOT NULL, fingerprint TEXT NOT NULL, body TEXT NOT NULL,
            UNIQUE(caller,request_id));
            CREATE TABLE IF NOT EXISTS pan_log(seq INTEGER PRIMARY KEY, download_id TEXT NOT NULL, at TEXT NOT NULL, body TEXT NOT NULL);")
            .map_err(Error::storage)?;
        Ok(())
    }
    fn db(&self) -> Result<Connection> {
        let db = Connection::open(self.root.join("pan.sqlite")).map_err(Error::storage)?;
        db.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(Error::storage)?;
        Ok(db)
    }
    #[cfg(test)]
    pub(super) fn test_db(&self) -> Result<Connection> {
        self.db()
    }
    pub(super) fn task(&self, id: &str) -> Result<Download> {
        let db = self.db()?;
        let value: Option<(String, String)> = db
            .query_row(
                "SELECT body,fingerprint FROM pan_downloads WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(Error::storage)?;
        let (body, fingerprint) =
            value.ok_or_else(|| Error::new("pan_not_found", "Download not found", false))?;
        let mut task: Download = serde_json::from_str(&body).map_err(Error::storage)?;
        task.fingerprint = fingerprint;
        Ok(task)
    }
    pub(super) fn list_tasks(&self, limit: usize) -> Result<Vec<Download>> {
        if !(1..=500).contains(&limit) {
            return Err(Error::invalid("limit must be 1 to 500"));
        }
        let db = self.db()?;
        let mut q = db
            .prepare("SELECT body,fingerprint FROM pan_downloads ORDER BY rowid DESC LIMIT ?1")
            .map_err(Error::storage)?;
        let rows = q
            .query_map([limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(Error::storage)?;
        rows.map(|row| {
            let (body, fp) = row.map_err(Error::storage)?;
            let mut d: Download = serde_json::from_str(&body).map_err(Error::storage)?;
            d.fingerprint = fp;
            Ok(d)
        })
        .collect()
    }
    pub(super) fn changed_tasks(&self) -> Result<Vec<Download>> {
        let db = self.db()?;
        let mut q=db.prepare("SELECT body FROM pan_downloads ORDER BY json_extract(body,'$.updatedAt') DESC LIMIT 500").map_err(Error::storage)?;
        let rows = q
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(Error::storage)?;
        rows.map(|r| serde_json::from_str(&r.map_err(Error::storage)?).map_err(Error::storage))
            .collect()
    }
    pub(super) fn mutate(&self, id: &str, f: impl FnOnce(&mut Download)) -> Result<Download> {
        let mut db = self.db()?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(Error::storage)?;
        let raw: Option<String> = tx
            .query_row("SELECT body FROM pan_downloads WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Error::storage)?;
        let mut task: Download = serde_json::from_str(
            &raw.ok_or_else(|| Error::new("pan_not_found", "Download not found", false))?,
        )
        .map_err(Error::storage)?;
        let previous = task.state.clone();
        f(&mut task);
        task.updated_at = now();
        let body = serde_json::to_string(&task).map_err(Error::storage)?;
        tx.execute(
            "UPDATE pan_downloads SET state=?1,body=?2 WHERE id=?3",
            rusqlite::params![task.state, body, id],
        )
        .map_err(Error::storage)?;
        if previous != task.state {
            tx.execute(
                "INSERT INTO pan_log(download_id,at,body) VALUES(?1,?2,?3)",
                rusqlite::params![id, task.updated_at, body],
            )
            .map_err(Error::storage)?;
        }
        tx.commit().map_err(Error::storage)?;
        Ok(task)
    }
    pub(super) fn request_task(
        &self,
        caller: &Caller,
        request_id: &str,
    ) -> Result<Option<Download>> {
        let owner = serde_json::to_string(caller).map_err(Error::storage)?;
        let id: Option<String> = self
            .db()?
            .query_row(
                "SELECT id FROM pan_downloads WHERE caller=?1 AND request_id=?2",
                rusqlite::params![owner, request_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(Error::storage)?;
        id.map(|id| self.task(&id)).transpose()
    }

    /// Recheck idempotency and reserve the output name under the same write
    /// transaction as the task and initial audit row. Metadata fetching stays
    /// outside the transaction.
    pub(super) fn insert_task(&self, mut task: Download, name: &str) -> Result<Download> {
        let owner = serde_json::to_string(&task.caller).map_err(Error::storage)?;
        let mut db = self.db()?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(Error::storage)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT id FROM pan_downloads WHERE caller=?1 AND request_id=?2",
                rusqlite::params![owner, task.request_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(Error::storage)?;
        if let Some(id) = old {
            drop(tx);
            return self.task(&id);
        }
        let target = Path::new(&task.target_dir);
        let mut output = target.join(name);
        let reserved: i64 = tx
            .query_row(
                "SELECT count(*) FROM pan_downloads WHERE json_extract(body,'$.targetPath')=?1",
                [output.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .map_err(Error::storage)?;
        if output.exists() || reserved > 0 {
            output = target.join(format!("{}-{}", &task.id[..8], name));
        }
        task.target_path = crate::agent_management::strip_verbatim_prefix(
            output
                .to_str()
                .ok_or_else(|| Error::invalid("invalid output path"))?,
        )
        .to_string();
        let body = serde_json::to_string(&task).map_err(Error::storage)?;
        tx.execute("INSERT INTO pan_downloads(id,caller,request_id,remote,target_dir,state,fingerprint,body) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![task.id,owner,task.request_id,task.remote,task.target_dir,task.state,task.fingerprint,body]).map_err(Error::storage)?;
        tx.execute(
            "INSERT INTO pan_log(download_id,at,body) VALUES(?1,?2,?3)",
            rusqlite::params![task.id, task.created_at, body],
        )
        .map_err(Error::storage)?;
        tx.commit().map_err(Error::storage)?;
        Ok(task)
    }

    pub(super) fn interrupted_tasks(&self) -> Result<Vec<String>> {
        let db = self.db()?;
        let mut query = db
            .prepare(
                "SELECT id FROM pan_downloads WHERE state='downloading' ORDER BY rowid LIMIT 100",
            )
            .map_err(Error::storage)?;
        let rows = query.query_map([], |r| r.get(0)).map_err(Error::storage)?;
        rows.collect::<std::result::Result<_, _>>().map_err(Error::storage)
    }

    pub(super) fn next_queued_task(&self) -> Result<Option<String>> {
        self.db()?
            .query_row(
                "SELECT id FROM pan_downloads WHERE state='queued' ORDER BY rowid LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(Error::storage)
    }

    pub(super) fn cancellable_tasks(&self) -> Result<Vec<String>> {
        let db = self.db()?;
        let mut query = db.prepare("SELECT id FROM pan_downloads WHERE state IN ('queued','downloading','paused','failed') ORDER BY rowid LIMIT 100")
            .map_err(Error::storage)?;
        let rows = query.query_map([], |r| r.get(0)).map_err(Error::storage)?;
        rows.collect::<std::result::Result<_, _>>().map_err(Error::storage)
    }
}
