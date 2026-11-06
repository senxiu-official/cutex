//! Same-transaction outbox. Items are enqueued inside `StoreDb::write`
//! together with the rows that caused them; a consumer claims, delivers
//! idempotently by `idem_key`, then acks. Cross-database follow-ups use this
//! instead of claiming cross-database atomicity.

use std::time::Duration;

use rusqlite::params;

use super::db::StoreDb;
use super::Seq;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxItem {
    pub topic: &'static str,
    pub idem_key: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    pub id: i64,
    pub store: String,
    pub idem_key: String,
    pub payload: Vec<u8>,
    pub commit_seq: Seq,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Claim up to `limit` unclaimed (or lease-expired) items of `topic`, oldest first.
pub fn claim(db: &StoreDb, topic: &str, limit: usize, lease: Duration) -> anyhow::Result<Vec<Claimed>> {
    let now = now_ms();
    let until = now.saturating_add(i64::try_from(lease.as_millis()).unwrap_or(i64::MAX));
    db.write(|tx| {
        let mut statement = tx.sql().prepare_cached(
            "UPDATE store_outbox SET claimed_until = ?1
             WHERE id IN (SELECT id FROM store_outbox
                          WHERE topic = ?2 AND (claimed_until IS NULL OR claimed_until < ?3)
                          ORDER BY id LIMIT ?4)
             RETURNING id, store, idem_key, payload, commit_seq",
        )?;
        let mut claimed = statement
            .query_map(params![until, topic, now, i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(Claimed {
                    id: row.get(0)?,
                    store: row.get(1)?,
                    idem_key: row.get(2)?,
                    payload: row.get(3)?,
                    commit_seq: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        claimed.sort_by_key(|item| item.id);
        Ok(claimed)
    })
}

/// Delivered: remove the items.
pub fn ack(db: &StoreDb, ids: &[i64]) -> anyhow::Result<()> {
    db.write(|tx| {
        let mut statement = tx.sql().prepare_cached("DELETE FROM store_outbox WHERE id = ?1")?;
        for id in ids {
            statement.execute([id])?;
        }
        Ok(())
    })
}

/// Delivery failed: make the items claimable again now.
pub fn release(db: &StoreDb, ids: &[i64]) -> anyhow::Result<()> {
    db.write(|tx| {
        let mut statement = tx
            .sql()
            .prepare_cached("UPDATE store_outbox SET claimed_until = NULL WHERE id = ?1")?;
        for id in ids {
            statement.execute([id])?;
        }
        Ok(())
    })
}

/// Number of pending items of `topic` (diagnostics).
pub fn pending(db: &StoreDb, topic: &str) -> anyhow::Result<u64> {
    db.read(|tx| {
        let count: i64 = tx.sql().query_row(
            "SELECT COUNT(*) FROM store_outbox WHERE topic = ?1",
            [topic],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    })
}
