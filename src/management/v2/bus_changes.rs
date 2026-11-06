//! S0 transport over the existing authoritative bus rows. No second copy of
//! canonical history: row changes and tombstones commit with hot-store CAS.
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{RowChange, RowKey, Rows};
use crate::store::{ChangeCursor, ChangeKind, PageBudget, ReadTx, Scope, StoreDb, StoreSpec};

pub(crate) fn spec(logical: &str) -> Option<&'static StoreSpec> {
    Some(crate::store::spec(match logical {
        "agent_bus" => "bus-messages",
        "agent_bus_settlement_evidence" => "bus-settlement-evidence",
        _ => return None,
    }))
}

pub(crate) fn open(path: &Path) -> anyhow::Result<Arc<StoreDb>> {
    StoreDb::open_at(&crate::store::domain::MANAGEMENT_STATE, path)
}

#[derive(Serialize, Deserialize)]
struct Summary {
    generation: u64,
}

fn logged_generation(sql: &Connection, spec: &StoreSpec) -> anyhow::Result<Option<u64>> {
    let summary: Option<Option<Vec<u8>>> = sql
        .query_row(
            "SELECT summary FROM store_changes WHERE store=?1 ORDER BY seq DESC LIMIT 1",
            [spec.id],
            |row| row.get(0),
        )
        .optional()?;
    let bytes = summary.flatten();
    crate::store::metrics::note(
        spec.id,
        crate::store::OpCounts {
            rows_visited: u64::from(bytes.is_some()),
            bytes_read: bytes.as_ref().map_or(0, Vec::len) as u64,
            bytes_decoded: bytes.as_ref().map_or(0, Vec::len) as u64,
            sql_statements: 1,
            ..Default::default()
        },
    );
    bytes
        .map(|bytes| {
            serde_json::from_slice::<Summary>(&bytes)
                .map(|row| row.generation)
                .map_err(Into::into)
        })
        .transpose()
}

pub(crate) fn commit(
    path: &Path,
    logical: &str,
    expected: u64,
    changes: Vec<RowChange>,
) -> anyhow::Result<u64> {
    let spec = spec(logical).context("bus change store")?;
    let db = open(path)?;
    db.write(|tx| {
        // A predecessor can still write canonical rows without the S0 log.
        // Never silently skip such writes: invalidate every older cursor.
        if logged_generation(tx.sql(), spec)? != Some(expected) {
            tx.reset_epoch(spec)?;
        }
        let next = super::commit_rows(tx.sql(), logical, expected, &changes)?;
        let summary = serde_json::to_vec(&Summary { generation: next })?;
        for change in &changes {
            let (collection, key, kind) = match change {
                RowChange::Put {
                    collection, key, ..
                } => (collection, key, ChangeKind::Put),
                RowChange::Delete { collection, key } => (collection, key, ChangeKind::Delete),
            };
            let key = serde_json::to_string(&(collection, key))?;
            let revision = tx
                .head(spec)?
                .seq
                .checked_add(1)
                .context("bus change revision overflow")?;
            tx.record_change(spec, &Scope::Host, &key, revision, kind, Some(&summary))?;
        }
        Ok(next)
    })
}

/// Legacy import is already protected by the bus OS lock. Reset only the
/// bounded transport history; business rows and receipt identities stay put.
pub(crate) fn invalidate(path: &Path, logical: &str) -> anyhow::Result<()> {
    if let Some(spec) = spec(logical) {
        open(path)?.write(|tx| tx.reset_epoch(spec).map(|_| ()))?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub cursor: ChangeCursor,
    pub generation: Option<u64>,
    logged_generation: Option<u64>,
}

pub(crate) fn position(tx: &ReadTx<'_>, db: &StoreDb, logical: &str) -> anyhow::Result<Position> {
    let spec = spec(logical).context("bus change store")?;
    let head = tx.head(spec)?;
    Ok(Position {
        cursor: ChangeCursor::new(db.instance_id(), spec, head.epoch, &Scope::Host, head.seq),
        generation: super::meta(tx.sql(), logical)?
            .filter(|(_, verified)| *verified)
            .map(|(generation, _)| generation),
        logged_generation: logged_generation(tx.sql(), spec)?,
    })
}

pub(crate) enum Delta {
    Rows {
        position: Position,
        keys: BTreeSet<RowKey>,
    },
    Resync(Position),
}

pub(crate) fn delta(
    tx: &ReadTx<'_>,
    db: &StoreDb,
    logical: &str,
    previous: Option<&Position>,
) -> anyhow::Result<Delta> {
    let current = position(tx, db, logical)?;
    let Some(previous) = previous else {
        return Ok(Delta::Resync(current));
    };
    if previous == &current {
        return Ok(Delta::Rows {
            position: current,
            keys: BTreeSet::new(),
        });
    }
    let spec = spec(logical).unwrap();
    let head = tx.head(spec)?;
    if current.logged_generation != current.generation
        || previous
            .cursor
            .resync_reason(db.instance_id(), spec, &head, &Scope::Host)
            .is_some()
    {
        return Ok(Delta::Resync(current));
    }
    let mut after = previous.cursor.after_seq();
    let mut keys = BTreeSet::new();
    while after < head.seq {
        let page = tx.changes(spec, &Scope::Host, after, PageBudget::DELTA_DEFAULT)?;
        if page.rows.is_empty() {
            return Ok(Delta::Resync(current));
        }
        for change in page.rows {
            // Every sequence is a row in this Host feed, including deletes.
            if change.seq != after + 1 {
                return Ok(Delta::Resync(current));
            }
            after = change.seq;
            crate::store::metrics::note(
                spec.id,
                crate::store::OpCounts {
                    bytes_decoded: change.key.len() as u64,
                    ..Default::default()
                },
            );
            keys.insert(serde_json::from_str::<RowKey>(&change.key)?);
        }
    }
    Ok(Delta::Rows {
        position: current,
        keys,
    })
}

pub(crate) fn keyed_rows(
    sql: &Connection,
    logical: &str,
    keys: impl IntoIterator<Item = RowKey>,
) -> anyhow::Result<Rows> {
    let mut rows = Rows::new();
    for (collection, key) in keys {
        if let Some(bytes) = super::verified_row(sql, logical, &collection, &key)? {
            rows.insert((collection, key), bytes);
        }
    }
    Ok(rows)
}
