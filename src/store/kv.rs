//! `KvStore`: a generic keyed store on the layer for stores that need no
//! custom tables. Rows live in `store_kv`; secondary indexes in
//! `store_kv_index` are maintained in the same transaction as the row.
//! The revision of a row is the commit seq that wrote it, so a revision is
//! never reused for a key, even after delete and re-create.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::cursor::{ChangeCursor, ListCursor};
use super::db::{StoreDb, StoreHead};
use super::keyed::{
    ApplyBatch, Change, ChangeKind, ChangesOutcome, Commit, Expect, IndexedFilter, KeyedStore,
    Many, Mutation, Page, PageBudget, Scope, StoreError, StoreKey, Versioned,
};
use super::manifest::StoreSpec;
use super::metrics::{self, OpCounts};
use super::{Revision, Seq};

/// A secondary index: `extract` returns the index keys of a record.
pub struct IndexSpec<R> {
    pub name: &'static str,
    pub extract: fn(&R) -> Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KvFilter {
    All,
    /// Rows whose index `name` contains `value`, in row-key order.
    Index { name: &'static str, value: String },
}

impl IndexedFilter for KvFilter {
    fn fingerprint(&self) -> u64 {
        match self {
            Self::All => 0,
            Self::Index { .. } => {
                let mut hasher = DefaultHasher::new();
                self.hash(&mut hasher);
                hasher.finish() | 1
            }
        }
    }
}

pub struct KvStore<K, R: 'static> {
    db: Arc<StoreDb>,
    spec: &'static StoreSpec,
    indexes: &'static [IndexSpec<R>],
    _key: PhantomData<fn() -> K>,
}

fn store_error(error: anyhow::Error) -> StoreError {
    StoreError::from(error)
}

fn decode_value<R: DeserializeOwned>(spec: &StoreSpec, bytes: &[u8]) -> Result<R, StoreError> {
    metrics::note(
        spec.id,
        OpCounts {
            bytes_read: bytes.len() as u64,
            bytes_decoded: bytes.len() as u64,
            rows_materialized: 1,
            ..OpCounts::default()
        },
    );
    serde_json::from_slice(bytes)
        .map_err(|error| StoreError::Other(anyhow::anyhow!("decode {} row: {error}", spec.id)))
}

fn u64_of(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::Invalid("negative value in store row".into()))
}

fn current_revision(
    sql: &Connection,
    spec: &StoreSpec,
    scope: &str,
    key: &str,
) -> Result<Revision, StoreError> {
    super::db::note_sql(spec, 1, 1);
    let revision: Option<i64> = sql
        .prepare_cached("SELECT revision FROM store_kv WHERE store = ?1 AND scope = ?2 AND row_key = ?3")?
        .query_row(params![spec.id, scope, key], |row| row.get(0))
        .optional()?;
    revision.map_or(Ok(0), u64_of)
}

fn check_expect(key: &str, expect: Expect, actual: Revision) -> Result<(), StoreError> {
    let ok = match expect {
        Expect::Any => true,
        Expect::Absent => actual == 0,
        Expect::Revision(revision) => actual == revision,
    };
    if ok {
        Ok(())
    } else {
        Err(StoreError::Conflict {
            key: key.to_owned(),
            expected: expect,
            actual,
        })
    }
}

impl<K: StoreKey, R: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> KvStore<K, R> {
    /// Open the store described by its manifest entry inside `db`.
    pub fn open(
        db: Arc<StoreDb>,
        spec: &'static StoreSpec,
        indexes: &'static [IndexSpec<R>],
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            super::manifest::find(spec.id).is_some_and(|registered| std::ptr::eq(registered, spec))
                || cfg!(test),
            "store {} is not a manifest entry",
            spec.id
        );
        anyhow::ensure!(
            spec.domain.id == db.domain().id || cfg!(test),
            "store {} belongs to domain {}, not {}",
            spec.id,
            spec.domain.id,
            db.domain().id
        );
        db.register(spec)?;
        Ok(Self {
            db,
            spec,
            indexes,
            _key: PhantomData,
        })
    }

    pub fn db(&self) -> &Arc<StoreDb> {
        &self.db
    }

    fn head(&self, sql: &Connection) -> Result<StoreHead, StoreError> {
        let head = super::db::read_head(sql, self.spec)?;
        Ok(head)
    }

    fn write_indexes(
        &self,
        sql: &Connection,
        scope: &str,
        key: &str,
        value: Option<&R>,
    ) -> Result<(), StoreError> {
        if self.indexes.is_empty() {
            return Ok(());
        }
        for index in self.indexes {
            sql.prepare_cached(
                "DELETE FROM store_kv_index WHERE store = ?1 AND index_name = ?2 AND scope = ?3 AND row_key = ?4",
            )?
            .execute(params![self.spec.id, index.name, scope, key])?;
            if let Some(value) = value {
                let mut insert = sql.prepare_cached(
                    "INSERT OR IGNORE INTO store_kv_index(store, index_name, scope, index_key, row_key)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for index_key in (index.extract)(value) {
                    insert.execute(params![self.spec.id, index.name, scope, index_key, key])?;
                }
            }
        }
        Ok(())
    }

    fn list_in(
        &self,
        sql: &Connection,
        scope: &Scope,
        filter: &KvFilter,
        after: Option<&ListCursor>,
        budget: PageBudget,
    ) -> Result<Page<K, R>, StoreError> {
        let head = self.head(sql)?;
        let fingerprint = filter.fingerprint();
        if let Some(cursor) = after {
            cursor.check(self.db.instance_id(), self.spec, &head, scope, fingerprint)?;
        }
        let after_key = after.map(|cursor| cursor.after_key().to_owned());
        let scope_text = scope.encode();
        let limit = i64::try_from(budget.max_rows.saturating_add(1)).unwrap_or(i64::MAX);
        let mut statement;
        let mut rows = match filter {
            KvFilter::All => {
                statement = sql.prepare_cached(
                    "SELECT row_key, revision, seq, length(value), value FROM store_kv
                     WHERE store = ?1 AND scope = ?2 AND (?3 IS NULL OR row_key > ?3)
                     ORDER BY row_key LIMIT ?4",
                )?;
                statement.query(params![self.spec.id, scope_text, after_key, limit])?
            }
            KvFilter::Index { name, value } => {
                if !self.indexes.iter().any(|index| index.name == *name) {
                    return Err(StoreError::Invalid(format!("{} has no index {name:?}", self.spec.id)));
                }
                statement = sql.prepare_cached(
                    "SELECT kv.row_key, kv.revision, kv.seq, length(kv.value), kv.value
                     FROM store_kv_index AS ix
                     JOIN store_kv AS kv ON kv.store = ix.store AND kv.scope = ix.scope AND kv.row_key = ix.row_key
                     WHERE ix.store = ?1 AND ix.index_name = ?2 AND ix.scope = ?3 AND ix.index_key = ?4
                       AND (?5 IS NULL OR ix.row_key > ?5)
                     ORDER BY ix.row_key LIMIT ?6",
                )?;
                statement.query(params![self.spec.id, name, scope_text, value, after_key, limit])?
            }
        };
        let mut page = Vec::new();
        let mut used = 0usize;
        let mut visited = 0u64;
        let mut more = false;
        let mut oversize_first = false;
        while let Some(row) = rows.next()? {
            visited += 1;
            let length = usize::try_from(row.get::<_, i64>(3)?).unwrap_or(usize::MAX);
            if !budget.admits(page.len(), used, length) {
                more = true;
                break;
            }
            if page.is_empty() && length > budget.max_bytes {
                oversize_first = true;
            }
            used = used.saturating_add(length);
            let key_text: String = row.get(0)?;
            let bytes: Vec<u8> = row.get(4)?;
            page.push(Versioned {
                key: K::decode(&key_text).map_err(store_error)?,
                revision: u64_of(row.get(1)?)?,
                seq: u64_of(row.get(2)?)?,
                value: decode_value(self.spec, &bytes)?,
            });
        }
        super::db::note_sql(self.spec, 1, visited);
        let next = more.then(|| {
            let last = page.last().expect("a page with more rows is not empty");
            ListCursor::new(
                self.db.instance_id(),
                self.spec,
                &head,
                scope,
                fingerprint,
                last.key.encode(),
            )
        });
        Ok(Page {
            rows: page,
            next,
            snapshot_seq: head.seq,
            oversize_first,
        })
    }
}

impl<K: StoreKey, R: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> KeyedStore
    for KvStore<K, R>
{
    type Key = K;
    type Record = R;
    type Filter = KvFilter;

    fn spec(&self) -> &'static StoreSpec {
        self.spec
    }

    fn get(&self, scope: &Scope, key: &K) -> Result<Option<Versioned<K, R>>, StoreError> {
        let key_text = key.encode();
        self.db
            .read(|tx| {
                super::db::note_sql(self.spec, 1, 1);
                let row: Option<(i64, i64, Vec<u8>)> = tx
                    .sql()
                    .prepare_cached(
                        "SELECT revision, seq, value FROM store_kv WHERE store = ?1 AND scope = ?2 AND row_key = ?3",
                    )?
                    .query_row(params![self.spec.id, scope.encode(), key_text], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .optional()?;
                Ok(row)
            })
            .map_err(store_error)?
            .map(|(revision, seq, bytes)| {
                Ok(Versioned {
                    key: key.clone(),
                    revision: u64_of(revision)?,
                    seq: u64_of(seq)?,
                    value: decode_value(self.spec, &bytes)?,
                })
            })
            .transpose()
    }

    fn get_many(&self, scope: &Scope, keys: &[K], max_bytes: usize) -> Result<Many<K, R>, StoreError> {
        let scope_text = scope.encode();
        let (raw, snapshot_seq) = self
            .db
            .read(|tx| {
                let head = super::db::read_head(tx.sql(), self.spec)?;
                let mut statement = tx.sql().prepare_cached(
                    "SELECT revision, seq, length(value), value FROM store_kv
                     WHERE store = ?1 AND scope = ?2 AND row_key = ?3",
                )?;
                let mut out = Vec::with_capacity(keys.len());
                let mut used = 0usize;
                let mut exhausted = false;
                for key in keys {
                    if exhausted {
                        out.push((key.clone(), RawSlot::Deferred));
                        continue;
                    }
                    let mut rows = statement.query(params![self.spec.id, scope_text, key.encode()])?;
                    super::db::note_sql(self.spec, 1, 1);
                    let Some(row) = rows.next()? else {
                        out.push((key.clone(), RawSlot::Missing));
                        continue;
                    };
                    let length = usize::try_from(row.get::<_, i64>(2)?).unwrap_or(usize::MAX);
                    if used > 0 && used.saturating_add(length) > max_bytes {
                        exhausted = true;
                        out.push((key.clone(), RawSlot::Deferred));
                        continue;
                    }
                    used = used.saturating_add(length);
                    out.push((
                        key.clone(),
                        RawSlot::Found(row.get(0)?, row.get(1)?, row.get(3)?),
                    ));
                }
                Ok((out, head.seq))
            })
            .map_err(store_error)?;
        let mut many = Many {
            found: Vec::new(),
            missing: Vec::new(),
            deferred: Vec::new(),
            snapshot_seq,
        };
        for (key, slot) in raw {
            match slot {
                RawSlot::Found(revision, seq, bytes) => many.found.push(Versioned {
                    key,
                    revision: u64_of(revision)?,
                    seq: u64_of(seq)?,
                    value: decode_value(self.spec, &bytes)?,
                }),
                RawSlot::Missing => many.missing.push(key),
                RawSlot::Deferred => many.deferred.push(key),
            }
        }
        Ok(many)
    }

    fn list(
        &self,
        scope: &Scope,
        filter: &KvFilter,
        after: Option<&ListCursor>,
        budget: PageBudget,
    ) -> Result<Page<K, R>, StoreError> {
        let mut result = None;
        self.db
            .read(|tx| {
                result = Some(self.list_in(tx.sql(), scope, filter, after, budget));
                Ok(())
            })
            .map_err(store_error)?;
        result.expect("read ran")
    }

    fn apply(&self, batch: ApplyBatch<K, R>) -> Result<Commit<K>, StoreError> {
        let scope_text = batch.scope.encode();
        // Encode outside the writer slot.
        let mut encoded = Vec::with_capacity(batch.mutations.len());
        for mutation in batch.mutations {
            match mutation {
                Mutation::Put { key, value, expect } => {
                    let bytes = serde_json::to_vec(&value).map_err(|error| {
                        StoreError::Other(anyhow::anyhow!("encode {} row: {error}", self.spec.id))
                    })?;
                    metrics::note(
                        self.spec.id,
                        OpCounts {
                            bytes_encoded: bytes.len() as u64,
                            ..OpCounts::default()
                        },
                    );
                    if bytes.len() > self.spec.max_row_bytes {
                        return Err(StoreError::Invalid(format!(
                            "{} row of {} bytes exceeds max_row_bytes {}; store the body by reference",
                            self.spec.id,
                            bytes.len(),
                            self.spec.max_row_bytes
                        )));
                    }
                    encoded.push((key, Some((value, bytes)), expect));
                }
                Mutation::Delete { key, expect } => encoded.push((key, None, expect)),
            }
        }
        let spec = self.spec;
        let mut conflict = None;
        let result = self.db.write(|tx| {
            let mut revisions = Vec::with_capacity(encoded.len());
            let mut last = super::db::read_head(tx.sql(), spec)?.seq;
            for (key, put, expect) in &encoded {
                let key_text = key.encode();
                let actual = current_revision(tx.sql(), spec, &scope_text, &key_text)?;
                if let Err(error) = check_expect(&key_text, *expect, actual) {
                    conflict = Some(error);
                    anyhow::bail!("store CAS conflict");
                }
                // Revision = commit seq of this write.
                let next = super::db::read_head(tx.sql(), spec)?.seq + 1;
                match put {
                    Some((value, bytes)) => {
                        let seq = tx.record_change(spec, &batch.scope, &key_text, next, ChangeKind::Put, None)?;
                        debug_assert_eq!(seq, next);
                        tx.sql()
                            .prepare_cached(
                                "INSERT INTO store_kv(store, scope, row_key, revision, seq, value) VALUES (?1, ?2, ?3, ?4, ?4, ?5)
                                 ON CONFLICT(store, scope, row_key) DO UPDATE SET
                                   revision = excluded.revision, seq = excluded.seq, value = excluded.value",
                            )?
                            .execute(params![spec.id, scope_text, key_text, seq as i64, bytes])?;
                        metrics::note(
                            spec.id,
                            OpCounts {
                                sql_statements: 2,
                                bytes_written: bytes.len() as u64,
                                ..OpCounts::default()
                            },
                        );
                        self.write_indexes(tx.sql(), &scope_text, &key_text, Some(value))
                            .map_err(anyhow::Error::from)?;
                        revisions.push((key.clone(), seq));
                        last = seq;
                    }
                    None if actual == 0 => revisions.push((key.clone(), 0)),
                    None => {
                        let seq = tx.record_change(spec, &batch.scope, &key_text, next, ChangeKind::Delete, None)?;
                        debug_assert_eq!(seq, next);
                        tx.sql()
                            .prepare_cached("DELETE FROM store_kv WHERE store = ?1 AND scope = ?2 AND row_key = ?3")?
                            .execute(params![spec.id, scope_text, key_text])?;
                        super::db::note_sql(spec, 2, 0);
                        self.write_indexes(tx.sql(), &scope_text, &key_text, None)
                            .map_err(anyhow::Error::from)?;
                        revisions.push((key.clone(), 0));
                        last = seq;
                    }
                }
            }
            for item in &batch.outbox {
                tx.outbox(spec, item)?;
            }
            Ok(Commit {
                seq: last,
                revisions,
            })
        });
        match (result, conflict) {
            (_, Some(conflict)) => Err(conflict),
            (Ok(commit), None) => Ok(commit),
            (Err(error), None) => Err(store_error(error)),
        }
    }

    fn changes(
        &self,
        scope: &Scope,
        after: &ChangeCursor,
        budget: PageBudget,
    ) -> Result<ChangesOutcome<K>, StoreError> {
        let (head, raw) = self
            .db
            .read(|tx| {
                let head = super::db::read_head(tx.sql(), self.spec)?;
                if let Some(reason) = after.resync_reason(self.db.instance_id(), self.spec, &head, scope) {
                    return Ok((head, Err(reason)));
                }
                Ok((head, Ok(tx.changes(self.spec, scope, after.after_seq(), budget)?)))
            })
            .map_err(store_error)?;
        let raw = match raw {
            Ok(raw) => raw,
            Err(reason) => {
                return Ok(ChangesOutcome::ResyncRequired {
                    reason,
                    watermark: head.seq,
                })
            }
        };
        let mut changes = Vec::with_capacity(raw.rows.len());
        for row in raw.rows {
            changes.push(Change {
                seq: row.seq,
                scope: Scope::decode(&row.scope)?,
                key: K::decode(&row.key).map_err(store_error)?,
                revision: row.revision,
                kind: row.kind,
                summary: row.summary,
            });
        }
        let next_seq: Seq = if raw.more {
            changes.last().map_or(after.after_seq(), |change| change.seq)
        } else {
            head.seq
        };
        Ok(ChangesOutcome::Page {
            changes,
            next: ChangeCursor::new(self.db.instance_id(), self.spec, head.epoch, scope, next_seq),
            watermark: head.seq,
            more: raw.more,
        })
    }

    fn change_head(&self, scope: &Scope) -> Result<ChangeCursor, StoreError> {
        let head = self
            .db
            .read(|tx| super::db::read_head(tx.sql(), self.spec))
            .map_err(store_error)?;
        Ok(ChangeCursor::new(self.db.instance_id(), self.spec, head.epoch, scope, head.seq))
    }
}

enum RawSlot {
    Found(i64, i64, Vec<u8>),
    Missing,
    Deferred,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::db::tests::temp_db;
    use crate::store::manifest::{Access, Canonical, Retention, Rollback, StoreClass, DEFAULT_MAX_ROW_BYTES};
    use crate::store::outbox::{self, OutboxItem};
    use crate::store::ResyncReason;
    use serde::Deserialize;

    pub(crate) const fn spec_for_tests(id: &'static str, max_rows: u64, max_bytes: u64) -> StoreSpec {
        StoreSpec {
            id,
            owner: &["src/store/kv.rs"],
            canonical: Canonical::Sqlite {
                file: "store-test.sqlite3",
                tables: &["store_kv"],
            },
            class: StoreClass::MutableState,
            domain: &crate::store::db::tests::TEST_DOMAIN,
            scopes: &[],
            key: "test",
            indexes: &[],
            page: PageBudget::LIST_DEFAULT,
            max_row_bytes: DEFAULT_MAX_ROW_BYTES,
            change_retention: Retention { max_rows, max_bytes },
            growth: crate::store::manifest::Growth {
                bytes_1x: 0,
                rows_1x: 0,
                with: "",
            },
            schema_version: 1,
            rollback: Rollback::R0,
            previous_exporter: None,
            access: Access::Keyed,
            fixture: None,
            slice: "S0",
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct Row {
        pub(crate) owner: String,
        pub(crate) body: String,
    }

    pub(crate) static INDEXES: &[IndexSpec<Row>] = &[IndexSpec {
        name: "owner",
        extract: |row| vec![row.owner.clone()],
    }];

    static SPEC: StoreSpec = spec_for_tests("kv-fixture", 10_000, 1 << 20);
    static OTHER: StoreSpec = spec_for_tests("kv-other", 10_000, 1 << 20);

    fn row(owner: &str, body: &str) -> Row {
        Row {
            owner: owner.into(),
            body: body.into(),
        }
    }

    fn open(db: &Arc<StoreDb>, spec: &'static StoreSpec) -> KvStore<String, Row> {
        KvStore::open(db.clone(), spec, INDEXES).unwrap()
    }

    #[test]
    fn get_put_delete_cas_and_revisions_never_repeat() {
        let (root, db) = temp_db("kv-cas");
        let store = open(&db, &SPEC);
        let scope = Scope::Session("s1".into());
        let commit = store
            .apply(ApplyBatch::new(scope.clone()).put("a".into(), row("x", "1"), Expect::Absent))
            .unwrap();
        let first = commit.revisions[0].1;
        assert!(first > 0);
        let got = store.get(&scope, &"a".into()).unwrap().unwrap();
        assert_eq!((got.revision, got.value.body.as_str()), (first, "1"));
        assert!(store.get(&Scope::Session("s2".into()), &"a".into()).unwrap().is_none());

        let conflict = store
            .apply(ApplyBatch::new(scope.clone()).put("a".into(), row("x", "2"), Expect::Absent))
            .unwrap_err();
        assert!(matches!(conflict, StoreError::Conflict { actual, .. } if actual == first));
        assert!(conflict.is_retryable());

        store
            .apply(ApplyBatch::new(scope.clone()).delete("a".into(), Expect::Revision(first)))
            .unwrap();
        assert!(store.get(&scope, &"a".into()).unwrap().is_none());
        let again = store
            .apply(ApplyBatch::new(scope.clone()).put("a".into(), row("x", "3"), Expect::Absent))
            .unwrap();
        assert!(again.revisions[0].1 > first, "a re-created row gets a fresh revision");
        // A stale CAS from before the delete cannot hit the new row (no ABA).
        assert!(store
            .apply(ApplyBatch::new(scope).put("a".into(), row("x", "4"), Expect::Revision(first)))
            .is_err());
        drop((store, db));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_batch_commits_nothing() {
        let (root, db) = temp_db("kv-atomic");
        let store = open(&db, &SPEC);
        let scope = Scope::Host;
        store
            .apply(ApplyBatch::new(scope.clone()).put("b".into(), row("x", "1"), Expect::Any))
            .unwrap();
        let head = store.change_head(&scope).unwrap();
        let error = store
            .apply(
                ApplyBatch::new(scope.clone())
                    .put("a".into(), row("x", "1"), Expect::Absent)
                    .put("b".into(), row("x", "2"), Expect::Absent)
                    .outbox(OutboxItem {
                        topic: "kv-test",
                        idem_key: "k".into(),
                        payload: b"p".to_vec(),
                    }),
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::Conflict { .. }));
        assert!(store.get(&scope, &"a".into()).unwrap().is_none());
        assert_eq!(store.change_head(&scope).unwrap(), head);
        assert_eq!(outbox::pending(&db, "kv-test").unwrap(), 0);
        drop((store, db));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pages_respect_rows_bytes_index_and_cursor_pins() {
        let (root, db) = temp_db("kv-page");
        let store = open(&db, &SPEC);
        let scope = Scope::Project("p".into());
        let mut batch = ApplyBatch::new(scope.clone());
        for n in 0..25 {
            batch = batch.put(format!("k{n:03}"), row(if n % 2 == 0 { "even" } else { "odd" }, &"x".repeat(10)), Expect::Any);
        }
        batch = batch.put("big".into(), row("odd", &"y".repeat(5000)), Expect::Any);
        store.apply(batch).unwrap();

        let budget = PageBudget { max_rows: 10, max_bytes: 1 << 20 };
        let mut keys = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.list(&scope, &KvFilter::All, cursor.as_ref(), budget).unwrap();
            keys.extend(page.rows.iter().map(|row| row.key.clone()));
            match page.next {
                Some(next) => cursor = Some(ListCursor::decode(&SPEC, &next.encode()).unwrap()),
                None => break,
            }
        }
        assert_eq!(keys.len(), 26);
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));

        // The oversized row comes alone, never truncated.
        let tiny = PageBudget { max_rows: 10, max_bytes: 100 };
        let page = store.list(&scope, &KvFilter::All, None, tiny).unwrap();
        assert_eq!(page.rows[0].key, "big");
        assert!(page.oversize_first && page.rows.len() == 1 && page.rows[0].value.body.len() == 5000);
        let page2 = store.list(&scope, &KvFilter::All, page.next.as_ref(), tiny).unwrap();
        assert!(!page2.oversize_first && page2.rows.len() > 1);

        let evens = store
            .list(&scope, &KvFilter::Index { name: "owner", value: "even".into() }, None, PageBudget::LIST_DEFAULT)
            .unwrap();
        assert_eq!(evens.rows.len(), 13);
        assert!(evens.rows.iter().all(|row| row.value.owner == "even"));

        // A cursor is bound to its scope and filter.
        let first = store.list(&scope, &KvFilter::All, None, budget).unwrap();
        let next = first.next.unwrap();
        assert!(matches!(
            store.list(&Scope::Host, &KvFilter::All, Some(&next), budget),
            Err(StoreError::Resync(ResyncReason::ScopeMismatch))
        ));
        assert!(matches!(
            store.list(&scope, &KvFilter::Index { name: "owner", value: "odd".into() }, Some(&next), budget),
            Err(StoreError::Resync(ResyncReason::FilterMismatch))
        ));
        assert!(matches!(
            ListCursor::decode(&OTHER, &next.encode()),
            Err(StoreError::Resync(ResyncReason::StoreMismatch))
        ));
        drop((store, db));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn get_many_reports_missing_and_defers_past_budget() {
        let (root, db) = temp_db("kv-many");
        let store = open(&db, &SPEC);
        let scope = Scope::Host;
        store
            .apply(
                ApplyBatch::new(scope.clone())
                    .put("a".into(), row("o", &"1".repeat(100)), Expect::Any)
                    .put("b".into(), row("o", &"2".repeat(100)), Expect::Any),
            )
            .unwrap();
        let many = store
            .get_many(&scope, &["a".into(), "zz".into(), "b".into()], 150)
            .unwrap();
        assert_eq!(many.found.len(), 1);
        assert_eq!(many.missing, vec!["zz".to_string()]);
        assert_eq!(many.deferred, vec!["b".to_string()]);
        drop((store, db));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changes_follow_commits_with_tombstones_and_resync_on_epoch() {
        let (root, db) = temp_db("kv-changes");
        let store = open(&db, &SPEC);
        let mine = Scope::Session("a".into());
        let other = Scope::Session("b".into());
        let start = store.change_head(&mine).unwrap();
        let host_start = store.change_head(&Scope::Host).unwrap();
        let mut subscription = store.subscribe(&mine);
        store.apply(ApplyBatch::new(mine.clone()).put("x".into(), row("o", "1"), Expect::Any)).unwrap();
        store.apply(ApplyBatch::new(other.clone()).put("y".into(), row("o", "1"), Expect::Any)).unwrap();
        let x = store.get(&mine, &"x".into()).unwrap().unwrap();
        store.apply(ApplyBatch::new(mine.clone()).delete("x".into(), Expect::Revision(x.revision))).unwrap();
        assert!(matches!(subscription.wait(std::time::Duration::ZERO), crate::store::Wake::Changed(_)));

        let ChangesOutcome::Page { changes, next, watermark, more } =
            store.changes(&mine, &start, PageBudget::DELTA_DEFAULT).unwrap()
        else {
            panic!("expected a page");
        };
        assert!(!more);
        assert_eq!(watermark, 3);
        assert_eq!(changes.iter().map(|c| (c.key.as_str(), c.kind)).collect::<Vec<_>>(),
                   [("x", ChangeKind::Put), ("x", ChangeKind::Delete)]);
        assert_eq!(changes[0].revision, x.revision);
        assert_eq!(next.after_seq(), 3);
        let ChangesOutcome::Page { changes, .. } = store.changes(&mine, &next, PageBudget::DELTA_DEFAULT).unwrap() else {
            panic!()
        };
        assert!(changes.is_empty());
        let ChangesOutcome::Page { changes, .. } =
            store.changes(&Scope::Host, &host_start, PageBudget::DELTA_DEFAULT).unwrap()
        else {
            panic!()
        };
        assert_eq!(changes.len(), 3, "the host feed sees every scope");

        // Paging the change feed.
        let ChangesOutcome::Page { changes, next, more, .. } =
            store.changes(&Scope::Host, &host_start, PageBudget { max_rows: 2, max_bytes: 1 << 20 }).unwrap()
        else {
            panic!()
        };
        assert!(more && changes.len() == 2 && next.after_seq() == 2);

        db.write(|tx| tx.reset_epoch(&SPEC).map(|_| ())).unwrap();
        assert!(matches!(
            store.changes(&mine, &next, PageBudget::DELTA_DEFAULT).unwrap(),
            ChangesOutcome::ResyncRequired { reason: ResyncReason::EpochChanged, .. }
        ));
        drop((store, db));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn outbox_is_idempotent_claimable_and_ackable() {
        let (root, db) = temp_db("kv-outbox");
        let store = open(&db, &SPEC);
        let item = OutboxItem {
            topic: "kv-outbox",
            idem_key: "audit-1".into(),
            payload: b"one".to_vec(),
        };
        for n in 0..2 {
            store
                .apply(ApplyBatch::new(Scope::Host).put(format!("k{n}"), row("o", "1"), Expect::Any).outbox(item.clone()))
                .unwrap();
        }
        assert_eq!(outbox::pending(&db, "kv-outbox").unwrap(), 1);
        let claimed = outbox::claim(&db, "kv-outbox", 10, std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].store, "kv-fixture");
        assert!(outbox::claim(&db, "kv-outbox", 10, std::time::Duration::from_secs(60)).unwrap().is_empty());
        outbox::release(&db, &[claimed[0].id]).unwrap();
        let again = outbox::claim(&db, "kv-outbox", 10, std::time::Duration::from_secs(60)).unwrap();
        outbox::ack(&db, &[again[0].id]).unwrap();
        assert_eq!(outbox::pending(&db, "kv-outbox").unwrap(), 0);
        drop((store, db));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_cas_writers_lose_cleanly() {
        let (root, db) = temp_db("kv-race");
        let store = Arc::new(open(&db, &SPEC));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.apply(ApplyBatch::new(Scope::Host).put("one".into(), row("o", &n.to_string()), Expect::Absent))
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results.iter().filter_map(|result| result.as_ref().err()).all(StoreError::is_retryable));
        drop((store, db));
        std::fs::remove_dir_all(root).unwrap();
    }
}
