use super::*;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage_index_archive_ready(
  file_id INTEGER PRIMARY KEY,
  checkpoint INTEGER NOT NULL,
  fingerprint BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS usage_index_archive_samples(
  entry_id TEXT NOT NULL,
  file_id INTEGER NOT NULL,
  month TEXT NOT NULL,
  session_id TEXT NOT NULL,
  thread_id TEXT NOT NULL,
  turn_id TEXT NOT NULL,
  input_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  total_tokens INTEGER NOT NULL,
  observed_ms INTEGER NOT NULL,
  model TEXT,
  PRIMARY KEY(entry_id,file_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS usage_index_archive_session ON usage_index_archive_samples(session_id);
CREATE INDEX IF NOT EXISTS usage_index_archive_file ON usage_index_archive_samples(file_id);
";

pub(super) struct Sample {
    session: String,
    thread: String,
    turn: String,
    input: u64,
    output: u64,
    total: u64,
    model: Option<String>,
}

impl Sample {
    pub(super) fn from_usage(sample: super::super::model::UsageSample) -> Self {
        Self {
            session: sample.cutex_session_id,
            thread: sample.thread_id,
            turn: sample.turn_id,
            input: sample.tokens.input_tokens,
            output: sample.tokens.output_tokens,
            total: sample.tokens.total_tokens,
            model: sample
                .attribution
                .request_model
                .or(sample.attribution.model),
        }
    }

    pub(super) fn insert(
        &self,
        statement: &mut rusqlite::Statement<'_>,
        entry: &str,
        file: i64,
        month: &str,
        observed: i64,
    ) -> anyhow::Result<()> {
        statement.execute(params![
            entry,
            file,
            month,
            self.session,
            self.thread,
            self.turn,
            i64::try_from(self.input)?,
            i64::try_from(self.output)?,
            i64::try_from(self.total)?,
            observed,
            self.model
        ])?;
        Ok(())
    }
}

pub(super) fn inserter(sql: &Connection) -> rusqlite::Result<rusqlite::CachedStatement<'_>> {
    sql.prepare_cached(
        "INSERT OR IGNORE INTO usage_index_archive_samples
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )
}

pub(super) fn mark_catalog_ready(sql: &Connection) -> rusqlite::Result<()> {
    sql.execute(
        "INSERT OR IGNORE INTO usage_index_meta VALUES ('archive_catalog_ready',1)",
        [],
    )?;
    Ok(())
}

pub(super) fn mark_ready(
    sql: &Connection,
    file: i64,
    checkpoint: u64,
    fingerprint: &[u8],
) -> rusqlite::Result<()> {
    sql.execute(
        "INSERT OR REPLACE INTO usage_index_archive_ready VALUES (?1,?2,?3)",
        params![file, checkpoint as i64, fingerprint],
    )?;
    Ok(())
}

pub(super) fn drop_file(sql: &Connection, file: i64) -> rusqlite::Result<()> {
    sql.execute(
        "DELETE FROM usage_index_archive_ready WHERE file_id=?1",
        [file],
    )?;
    sql.execute(
        "DELETE FROM usage_index_archive_samples WHERE file_id=?1",
        [file],
    )?;
    Ok(())
}

pub(super) fn ready(sql: &Connection, state: &FileState) -> rusqlite::Result<bool> {
    sql.query_row(
        "SELECT EXISTS(SELECT 1 FROM usage_index_archive_ready
        WHERE file_id=?1 AND checkpoint=?2 AND fingerprint=?3)",
        params![state.file_id, state.checkpoint as i64, state.fingerprint],
        |r| r.get(0),
    )
}

const BACKFILL_BATCH_LINES: usize = 512;
const BACKFILL_BATCH_BYTES: u64 = 1024 * 1024;

impl UsageIndex {
    /// Backfill the captured prefix in batches, without locking the usage
    /// store. The ordinary index has already projected any concurrent tail.
    pub(in crate::management::v2::usage) fn backfill_archives(
        &self,
        root: &Path,
    ) -> anyhow::Result<bool> {
        self.backfill_archives_with(root, || {})
    }

    fn backfill_archives_with(
        &self,
        root: &Path,
        mut before_write: impl FnMut(),
    ) -> anyhow::Result<bool> {
        let files = self.db.read(|tx| {
            let sql = tx.sql();
            let mut pending = Vec::new();
            for (key, state) in known_files(sql)? {
                if key.source == OWN_SOURCE && !ready(sql, &state)? {
                    pending.push((key, state));
                }
            }
            Ok(pending)
        })?;
        let mut complete = true;
        for (key, state) in files {
            let mut file = match File::open(key.path(root)) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    complete = false;
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            if file.metadata()?.len() < state.checkpoint
                || fingerprint(&mut file, state.checkpoint)? != state.fingerprint
            {
                complete = false;
                continue;
            }
            file.seek(SeekFrom::Start(0))?;
            let mut rows = Vec::with_capacity(BACKFILL_BATCH_LINES);
            let mut reader = BufReader::new(file.take(state.checkpoint));
            let mut line = Vec::new();
            let mut remaining = state.checkpoint;
            'batches: loop {
                let before_batch = remaining;
                for _ in 0..BACKFILL_BATCH_LINES {
                    if remaining == 0 || before_batch - remaining >= BACKFILL_BATCH_BYTES {
                        break;
                    }
                    line.clear();
                    let read = reader.read_until(b'\n', &mut line)?;
                    if read == 0 {
                        complete = false;
                        break 'batches;
                    }
                    remaining -= read as u64;
                    if let Ok(Some(row)) = index_line(&line) {
                        if let Some(sample) = row.archive {
                            rows.push((row.entry_id, row.observed_ms, sample));
                        }
                    }
                }
                before_write();
                let committed = self.db.write(|tx| {
                    let sql = tx.sql();
                    let current = sql
                        .query_row(
                            "SELECT checkpoint, fingerprint
                             FROM usage_index_files WHERE file_id = ?1 AND checkpoint >= ?2",
                            params![state.file_id, state.checkpoint as i64],
                            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, Vec<u8>>(1)?)),
                        )
                        .optional()?;
                    let Some((checkpoint, fingerprint)) = current else {
                        return Ok(false);
                    };
                    if checkpoint == state.checkpoint && fingerprint != state.fingerprint {
                        return Ok(false);
                    }
                    let mut statement = inserter(sql)?;
                    for (entry, observed, sample) in rows.drain(..) {
                        sample.insert(
                            &mut statement,
                            &entry,
                            state.file_id,
                            &key.month,
                            observed,
                        )?;
                    }
                    if remaining == 0 {
                        mark_ready(sql, state.file_id, checkpoint, &fingerprint)?;
                    }
                    Ok(true)
                })?;
                complete &= committed;
                if !committed || remaining == 0 {
                    break;
                }
            }
        }
        Ok(complete)
    }
}

#[derive(Debug)]
pub(in crate::management::v2) struct ArchiveTotals {
    pub input: u64,
    pub output: u64,
    pub total: u64,
    pub turns: u64,
    pub model: Option<String>,
}

const SELECTED: &str = "
WITH selected AS (
  SELECT a.* FROM usage_index_archive_samples a INDEXED BY usage_index_archive_session WHERE a.session_id=?1
    AND EXISTS (SELECT 1 FROM usage_index_files f WHERE f.file_id=a.file_id AND f.source='' AND f.month=a.month)
    AND NOT EXISTS (SELECT 1 FROM usage_index_archive_samples earlier
                   JOIN usage_index_files ef ON ef.file_id=earlier.file_id AND ef.source='' AND ef.month=earlier.month
                   WHERE earlier.entry_id=a.entry_id AND earlier.month<a.month)
) ";

/// Opens an existing SQLite snapshot read-only. No usage-store lock, file
/// sync, ledger decode or cache creation occurs on the archived-list path.
pub(in crate::management::v2) fn read_totals<'a>(
    root: &Path,
    ids: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<HashMap<&'a str, ArchiveTotals>> {
    let db =
        Connection::open_with_flags(index_path(root), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(50))?;
    let tx = db.unchecked_transaction()?;
    let complete: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM usage_index_meta WHERE key='archive_catalog_ready')
         AND NOT EXISTS(SELECT 1 FROM usage_index_files f
         WHERE f.source='' AND NOT EXISTS(SELECT 1 FROM usage_index_archive_ready r WHERE r.file_id=f.file_id AND r.checkpoint=f.checkpoint AND r.fingerprint=f.fingerprint))",
        [], |row| row.get(0)
    )?;
    anyhow::ensure!(complete, "archive usage backfill pending");
    let mut aggregate = tx.prepare(&format!("{SELECTED}
        SELECT coalesce(sum(input),0), coalesce(sum(output),0), coalesce(sum(total),0), count(*)
        FROM (
              SELECT sum(input_tokens) AS input,sum(output_tokens) AS output,sum(total_tokens) AS total
              FROM selected
              GROUP BY thread_id,turn_id)"))?;
    let mut latest = tx.prepare(&format!("{SELECTED}
        SELECT model FROM selected WHERE model IS NOT NULL ORDER BY observed_ms DESC,entry_id DESC LIMIT 1"))?;
    let mut result = HashMap::new();
    for id in ids {
        let (input, output, total, turns) = aggregate.query_row([id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        result.insert(
            id,
            ArchiveTotals {
                input: input.try_into()?,
                output: output.try_into()?,
                total: total.try_into()?,
                turns: turns.try_into()?,
                model: latest.query_row([id], |row| row.get(0)).optional()?,
            },
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::waste_runtime_scale::Root;
    use serde_json::json;

    #[test]
    fn archived_totals_select_requested_sessions_and_never_open_the_ledger() {
        let root = Root::new("archive-index");
        let index = UsageIndex::open(root.path()).unwrap();
        index.db.write(|tx| {
            let sql = tx.sql();
            sql.execute_batch("
                INSERT INTO usage_index_files VALUES(1,'','2020-01',0,0,0,x'');
                INSERT INTO usage_index_files VALUES(2,'','2026-01',0,0,0,x'');
                WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<12000)
                INSERT INTO usage_index_archive_samples
                    SELECT i,2,'2026-01','cutex.other','thread','turn',1,2,3,i,'other' FROM n;
                INSERT INTO usage_index_archive_samples VALUES('a',1,'2020-01','cutex.chosen','t','same',3,4,7,1,'old');
                INSERT INTO usage_index_archive_samples VALUES('b',2,'2026-01','cutex.chosen','t','same',5,6,11,2,'new');
                INSERT INTO usage_index_archive_samples VALUES('a',2,'2026-01','cutex.chosen','t','duplicate',99,99,198,3,'duplicate');
            ")?;
            mark_ready(sql, 1, 0, &[])?;
            mark_ready(sql, 2, 0, &[])?;
            mark_catalog_ready(sql)?;
            Ok(())
        }).unwrap();
        // No ledger directories exist. The request uses only the SQL index.
        let totals = read_totals(root.path(), ["cutex.chosen"]).unwrap();
        let chosen = &totals["cutex.chosen"];
        assert_eq!(
            (chosen.input, chosen.output, chosen.total, chosen.turns),
            (8, 10, 18, 1)
        );
        assert_eq!(chosen.model.as_deref(), Some("new"));
        assert_eq!(totals.len(), 1);
        index
            .db
            .read(|tx| {
                let mut query = tx
                    .sql()
                    .prepare(&format!("{SELECTED} SELECT count(*) FROM selected"))?;
                assert_eq!(
                    query.query_row(["cutex.chosen"], |row| row.get::<_, i64>(0))?,
                    2
                );
                assert!(query.get_status(rusqlite::StatementStatus::VmStep) < 500);
                Ok(())
            })
            .unwrap();
    }

    fn sample(id: &str, total: u64) -> serde_json::Value {
        json!({
            "kind":"usage","entryId":id,"eventId":id,"observedAt":"2026-01-01T00:00:00Z","hostId":"host",
            "cutexSessionId":"cutex.x","threadId":"thread","turnId":id,"runtimeGeneration":1,
            "tokenSource":"native_last","tokens":{"inputTokens":total-1,"outputTokens":1,"totalTokens":total},"cumulativeTotal":{}
        })
    }

    fn pending_file(root: &Path) -> (UsageIndex, LedgerFileKey) {
        let key = LedgerFileKey {
            source: OWN_SOURCE.into(),
            month: "2026-01".into(),
        };
        let file = key.path(root);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let rows = (0..=BACKFILL_BATCH_LINES)
            .map(|i| sample(&i.to_string(), 4).to_string())
            .collect::<Vec<_>>();
        fs::write(&file, rows.join("\n")).unwrap();
        let index = UsageIndex::open(root).unwrap();
        index.sync(root, true).unwrap();
        index.db.write(|tx| {
            tx.sql().execute_batch("DELETE FROM usage_index_archive_samples; DELETE FROM usage_index_archive_ready;")?;
            Ok(())
        }).unwrap();
        (index, key)
    }

    fn append(root: &Path, index: &UsageIndex, key: &LedgerFileKey, rows: &[serde_json::Value]) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(key.path(root))
            .unwrap();
        for row in rows {
            write!(file, "\n{row}").unwrap();
        }
        drop(file);
        index.sync_files(root, std::slice::from_ref(key)).unwrap();
    }

    #[test]
    fn archive_backfill_keeps_concurrent_appends_and_publishes_after_all_batches() {
        let root = Root::new("archive-append");
        let root = root.path();
        let (index, key) = pending_file(root);
        let mut writes = 0;
        assert!(
            index
                .backfill_archives_with(root, || {
                    assert!(read_totals(root, ["cutex.x"]).is_err());
                    if writes == 0 {
                        // The duplicate's first occurrence is in the next batch.
                        append(
                            root,
                            &index,
                            &key,
                            &[
                                sample("tail", 5),
                                sample(&BACKFILL_BATCH_LINES.to_string(), 99),
                            ],
                        );
                    } else {
                        append(root, &index, &key, &[sample("later", 6)]);
                    }
                    writes += 1;
                })
                .unwrap()
        );
        assert_eq!(writes, 2);
        let expected = (BACKFILL_BATCH_LINES as u64 + 1) * 4 + 5 + 6;
        assert_eq!(
            read_totals(root, ["cutex.x"]).unwrap()["cutex.x"].total,
            expected
        );
        append(root, &index, &key, &[sample("new", 7)]);
        assert_eq!(
            read_totals(root, ["cutex.x"]).unwrap()["cutex.x"].total,
            expected + 7
        );
    }

    #[test]
    fn archive_backfill_discards_a_replaced_file_between_batches() {
        let root = Root::new("archive-replace");
        let root = root.path();
        let (index, key) = pending_file(root);
        let old_id = index
            .db
            .read(|tx| Ok(known_files(tx.sql())?[&key].file_id))
            .unwrap();
        let mut writes = 0;
        assert!(
            !index
                .backfill_archives_with(root, || {
                    writes += 1;
                    if writes == 2 {
                        // The replacement is larger, so a reused id would pass the
                        // append fence even though the old rows belong to another file.
                        let path = key.path(root);
                        let size = fs::metadata(&path).unwrap().len() as usize;
                        fs::write(
                            path,
                            format!("{}\n{}", sample("replacement", 9), " ".repeat(size)),
                        )
                        .unwrap();
                        index.sync_files(root, std::slice::from_ref(&key)).unwrap();
                    }
                })
                .unwrap()
        );
        let new_id = index
            .db
            .read(|tx| Ok(known_files(tx.sql())?[&key].file_id))
            .unwrap();
        assert!(new_id > old_id);
        assert!(index.backfill_archives(root).unwrap());
        assert_eq!(read_totals(root, ["cutex.x"]).unwrap()["cutex.x"].total, 9);
    }
}
