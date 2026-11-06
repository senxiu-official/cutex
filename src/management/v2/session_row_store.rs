//! S7: independent session-keyed activity and presentation repositories.
//! A validated transaction is the authority switch; legacy JSON is never written
//! by the new runtime. Delivery/receipt state belongs to other repositories.
use crate::store::{self, ChangeKind, OpCounts, Scope, StoreDb, WriteTx};
use anyhow::Context;
use rusqlite::{params, OptionalExtension};
use serde::{
    de::{DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    Serialize,
};
use std::{
    fs::File,
    io::{BufReader, Read},
    marker::PhantomData,
    path::Path,
    sync::Arc,
};

pub(super) struct Repository<R> {
    db: Arc<StoreDb>,
    id: &'static str,
    validate: fn(&R) -> anyhow::Result<()>,
    _row: PhantomData<R>,
}

// Keep the two live domain handles warm. Bounded by path, never by row count.
// Tests deliberately release handles so Windows can remove each private fixture.
#[cfg(not(test))]
static OPEN: std::sync::OnceLock<
    std::sync::Mutex<std::collections::VecDeque<(std::path::PathBuf, Arc<StoreDb>)>>,
> = std::sync::OnceLock::new();

fn fingerprint(path: &Path) -> anyhow::Result<String> {
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut file = File::open(path)?;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

struct HashRead<'a> {
    file: File,
    hash: &'a mut ring::digest::Context,
}
impl Read for HashRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

impl<R: Serialize + DeserializeOwned + Clone + PartialEq> Repository<R> {
    pub(super) fn open(
        root: &Path,
        id: &'static str,
        legacy: &str,
        validate: fn(&R) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let spec = store::spec(id);
        let filename = Path::new(spec.domain.path)
            .file_name()
            .context("S7 domain filename")?;
        let path = root.join(filename);
        #[cfg(not(test))]
        if let Some(db) = OPEN
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == &path)
            .map(|(_, db)| db.clone())
        {
            return Ok(Self {
                db,
                id,
                validate,
                _row: PhantomData,
            });
        }
        crate::management::client_tokens::ensure_private_dir(root)?;
        let db = StoreDb::open_at(spec.domain, &path)?;
        db.register(spec)?;
        let repo = Self {
            db,
            id,
            validate,
            _row: PhantomData,
        };
        repo.db.write(|tx| {
            tx.sql().execute_batch("CREATE TABLE IF NOT EXISTS session_rows (
                id TEXT PRIMARY KEY, deleted INTEGER NOT NULL, data BLOB);
                CREATE INDEX IF NOT EXISTS session_rows_live ON session_rows(deleted,id);
                CREATE TABLE IF NOT EXISTS session_rows_ready (id INTEGER PRIMARY KEY CHECK(id=1), source TEXT NOT NULL);")?;
            if id == "session-activity" {
                tx.sql().execute_batch("CREATE TABLE IF NOT EXISTS turn_error_notices(profile TEXT PRIMARY KEY, at TEXT NOT NULL);
                    CREATE INDEX IF NOT EXISTS session_auth_error_profiles ON session_rows
                    (coalesce(json_extract(CAST(data AS TEXT),'$.lastTurnError.profileId'),json_extract(CAST(data AS TEXT),'$.lastTurnError.profileName'),''))
                    WHERE deleted=0 AND json_extract(CAST(data AS TEXT),'$.lastTurnError.code') IN ('unauthorized','refresh_token_reused');")?;
            }
            let ready: bool = tx.sql().query_row("SELECT EXISTS(SELECT 1 FROM session_rows_ready WHERE id=1)", [], |r| r.get(0))?;
            if ready { return Ok(()); }
            // Startup is under the kit's stopped-writer boundary. A malformed
            // source rolls the whole import back, including the ready marker.
            tx.sql().execute_batch("CREATE TEMP TABLE session_import_deleted(id TEXT PRIMARY KEY)")?;
            let source = root.join(legacy);
            let mut source_digest=String::from("absent");
            match File::open(&source) {
                Ok(file) => {
                    let mut hash=ring::digest::Context::new(&ring::digest::SHA256);
                    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(HashRead{file,hash:&mut hash}));
                    Import { repo: &repo, tx }.deserialize(&mut deserializer)?;
                    deserializer.end()?;
                    drop(deserializer);
                    source_digest=hash.finish().as_ref().iter().map(|b|format!("{b:02x}")).collect();
                    anyhow::ensure!(source_digest==fingerprint(&source)?,"S7 source changed during import");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {tx.sql().execute_batch("DROP TABLE session_import_deleted")?;},
                Err(e) => return Err(e.into()),
            }
            tx.record_migration(spec, 1, Some(&source_digest))?;
            tx.sql().execute("INSERT INTO session_rows_ready VALUES(1,?1)", [source.to_string_lossy().as_ref()])?;
            Ok(())
        })?;
        #[cfg(not(test))]
        {
            let mut open = OPEN.get_or_init(Default::default).lock().unwrap();
            if open.len() >= 4 {
                open.pop_front();
            }
            open.push_back((path, repo.db.clone()));
        }
        Ok(repo)
    }

    fn read(
        &self,
        sql: &rusqlite::Connection,
        id: &str,
    ) -> anyhow::Result<Option<(bool, Option<R>)>> {
        let raw: Option<(bool, Option<Vec<u8>>)> = sql
            .query_row(
                "SELECT deleted,data FROM session_rows WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        store::metrics::note(
            self.id,
            OpCounts {
                sql_statements: 1,
                rows_visited: u64::from(raw.is_some()),
                ..Default::default()
            },
        );
        raw.map(|(deleted, bytes)| Ok((deleted, bytes.map(|b| self.decode(&b)).transpose()?)))
            .transpose()
    }

    fn decode(&self, bytes: &[u8]) -> anyhow::Result<R> {
        store::metrics::note(
            self.id,
            OpCounts {
                bytes_read: bytes.len() as u64,
                bytes_decoded: bytes.len() as u64,
                rows_materialized: 1,
                ..Default::default()
            },
        );
        let row = serde_json::from_slice(bytes)?;
        (self.validate)(&row)?;
        Ok(row)
    }

    pub(super) fn get(&self, id: &str) -> anyhow::Result<Option<R>> {
        self.db.read(|tx| {
            Ok(self
                .read(tx.sql(), id)?
                .and_then(|(deleted, row)| if deleted { None } else { row }))
        })
    }

    fn put(&self, tx: &mut WriteTx<'_>, id: &str, row: Option<&R>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !id.is_empty() && id.len() <= crate::store::db::MAX_KEY_BYTES,
            "invalid session row key"
        );
        let bytes = row
            .map(|row| {
                (self.validate)(row)?;
                Ok::<_, anyhow::Error>(serde_json::to_vec(row)?)
            })
            .transpose()?;
        anyhow::ensure!(
            bytes
                .as_ref()
                .is_none_or(|b| b.len() <= store::spec(self.id).max_row_bytes),
            "session row exceeds byte budget"
        );
        tx.sql().execute(
            "INSERT INTO session_rows(id,deleted,data) VALUES(?1,?2,?3)
            ON CONFLICT(id) DO UPDATE SET deleted=excluded.deleted,data=excluded.data",
            params![id, row.is_none(), bytes],
        )?;
        store::metrics::note(
            self.id,
            OpCounts {
                bytes_encoded: bytes.as_ref().map_or(0, |b| b.len() as u64),
                bytes_written: bytes.as_ref().map_or(0, |b| b.len() as u64),
                sql_statements: 1,
                ..Default::default()
            },
        );
        let spec = store::spec(self.id);
        let revision = tx.head(spec)?.seq + 1;
        tx.record_change(
            spec,
            &Scope::Session(id.into()),
            id,
            revision,
            if row.is_some() {
                ChangeKind::Put
            } else {
                ChangeKind::Delete
            },
            None,
        )?;
        Ok(())
    }

    /// The action sees only this session; error/no-op commits no changed row.
    pub(super) fn update<T>(
        &self,
        id: &str,
        action: impl FnOnce(bool, &mut Option<R>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.update_then(id, action, |_, result| Ok(result))
    }

    /// Commit the row and domain metadata together; the callback sees the new row.
    pub(super) fn update_then<T, U>(
        &self,
        id: &str,
        action: impl FnOnce(bool, &mut Option<R>) -> anyhow::Result<T>,
        then: impl FnOnce(&rusqlite::Connection, T) -> anyhow::Result<U>,
    ) -> anyhow::Result<U> {
        self.db.write(|tx| {
            let (deleted, before) = self.read(tx.sql(), id)?.unwrap_or((false, None));
            let mut after = before.clone();
            let result = action(deleted, &mut after)?;
            if before != after {
                self.put(tx, id, after.as_ref())?;
            }
            then(tx.sql(), result)
        })
    }

    pub(super) fn forget(&self, id: &str) -> anyhow::Result<()> {
        self.db.write(|tx| {
            if self.read(tx.sql(), id)?.is_none_or(|(deleted, _)| !deleted) {
                self.put(tx, id, None)?;
            }
            Ok(())
        })
    }

    /// Bounded keyset page, including deletion fences for offline R1 export.
    pub(super) fn page(
        &self,
        after: &str,
        limit: usize,
        budget: usize,
    ) -> anyhow::Result<Vec<(String, Option<R>)>> {
        anyhow::ensure!(
            limit > 0 && limit <= 1000 && budget > 0,
            "invalid session page budget"
        );
        self.db.read(|tx| {
            let mut stmt = tx.sql().prepare(
                "SELECT id,deleted,length(data),data FROM session_rows WHERE id>?1 ORDER BY id LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![after, limit as i64])?;
            let mut result = Vec::new();
            let mut bytes = 0;
            while let Some(row) = rows.next()? {
                let data_len: Option<i64> = row.get(2)?;
                let key: String = row.get(0)?;
                store::metrics::note(
                    self.id,
                    OpCounts {
                        rows_visited: 1,
                        ..Default::default()
                    },
                );
                let len = key.len() + usize::try_from(data_len.unwrap_or(0))?;
                if !result.is_empty() && bytes + len > budget {
                    break;
                }
                bytes += len;
                let data: Option<Vec<u8>> = row.get(3)?;
                result.push((key, data.map(|b| self.decode(&b)).transpose()?));
            }
            store::metrics::note(
                self.id,
                OpCounts {
                    sql_statements: 1,
                    ..Default::default()
                },
            );
            Ok(result)
        })
    }
}

// Stream the two legacy v1 documents one session at a time. Unknown fields
// retain the previous serde behaviour; duplicate keys are refused to avoid loss.
struct Import<'a, 't, R> {
    repo: &'a Repository<R>,
    tx: &'a mut WriteTx<'t>,
}
impl<'de, R: Serialize + DeserializeOwned + Clone + PartialEq> DeserializeSeed<'de>
    for Import<'_, '_, R>
{
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de, R: Serialize + DeserializeOwned + Clone + PartialEq> Visitor<'de> for Import<'_, '_, R> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("v1 session store")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        use serde::de::Error;
        let mut fields = std::collections::BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !fields.insert(key.clone()) {
                return Err(M::Error::custom("duplicate store field"));
            }
            match key.as_str() {
                "version" => {
                    if map.next_value::<u8>()? != 1 {
                        return Err(M::Error::custom("unsupported session store version"));
                    }
                }
                "sessions" => map.next_value_seed(SessionMap {
                    repo: self.repo,
                    tx: self.tx,
                })?,
                "deletedSessions" if self.repo.id == "session-activity" => map.next_value_seed(Deleted {
                    repo: self.repo,
                    tx: self.tx,
                })?,
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        // Never discard a conflicting source row during migration.
        let conflict:bool=self.tx.sql().query_row("SELECT EXISTS(SELECT 1 FROM session_rows r JOIN session_import_deleted d ON r.id=d.id)",[],|r|r.get(0)).map_err(M::Error::custom)?;
        if conflict {
            return Err(M::Error::custom("live/deleted session conflict in source"));
        }
        self.tx
            .sql()
            .execute(
                "INSERT OR REPLACE INTO session_rows SELECT id,1,NULL FROM session_import_deleted",
                [],
            )
            .map_err(M::Error::custom)?;
        self.tx
            .sql()
            .execute_batch("DROP TABLE session_import_deleted")
            .map_err(M::Error::custom)?;
        Ok(())
    }
}
struct SessionMap<'a, 't, R> {
    repo: &'a Repository<R>,
    tx: &'a mut WriteTx<'t>,
}
impl<'de, R: Serialize + DeserializeOwned + Clone + PartialEq> DeserializeSeed<'de>
    for SessionMap<'_, '_, R>
{
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de, R: Serialize + DeserializeOwned + Clone + PartialEq> Visitor<'de>
    for SessionMap<'_, '_, R>
{
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("session rows")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        use serde::de::Error;
        while let Some(id) = map.next_key::<String>()? {
            let row = map.next_value::<R>()?;
            if self
                .repo
                .read(self.tx.sql(), &id)
                .map_err(M::Error::custom)?
                .is_some()
            {
                return Err(M::Error::custom("duplicate session"));
            }
            self.repo
                .put(self.tx, &id, Some(&row))
                .map_err(M::Error::custom)?;
            let decoded = self
                .repo
                .read(self.tx.sql(), &id)
                .map_err(M::Error::custom)?
                .and_then(|(_, r)| r);
            if decoded.as_ref() != Some(&row) {
                return Err(M::Error::custom("import row validation mismatch"));
            }
        }
        Ok(())
    }
}
struct Deleted<'a, 't, R> {
    repo: &'a Repository<R>,
    tx: &'a mut WriteTx<'t>,
}
impl<'de, R: Serialize + DeserializeOwned + Clone + PartialEq> DeserializeSeed<'de>
    for Deleted<'_, '_, R>
{
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}
impl<'de, R: Serialize + DeserializeOwned + Clone + PartialEq> Visitor<'de> for Deleted<'_, '_, R> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("deleted sessions")
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        use serde::de::Error;
        while let Some(id) = seq.next_element::<String>()? {
            if id.is_empty() {
                return Err(S::Error::custom("empty deleted session"));
            }
            self.tx
                .sql()
                .execute(
                    "INSERT OR IGNORE INTO session_import_deleted VALUES(?1)",
                    [id],
                )
                .map_err(S::Error::custom)?;
        }
        let _ = self.repo;
        Ok(())
    }
}

#[cfg(test)]
#[path = "session_row_store_tests.rs"]
mod tests;
