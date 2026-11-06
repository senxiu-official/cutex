//! Per-operation store counters (design 058 §7 item 2).
//!
//! Every count goes to the current thread (for `measure`, used by scale
//! gates) and to a process-wide per-store total (for diagnostics).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpCounts {
    pub bytes_read: u64,
    pub bytes_decoded: u64,
    pub bytes_encoded: u64,
    pub bytes_written: u64,
    pub rows_visited: u64,
    pub rows_materialized: u64,
    pub whole_state_copies: u64,
    pub whole_state_serializations: u64,
    pub sql_statements: u64,
}

impl OpCounts {
    fn add(&mut self, other: &OpCounts) {
        self.bytes_read += other.bytes_read;
        self.bytes_decoded += other.bytes_decoded;
        self.bytes_encoded += other.bytes_encoded;
        self.bytes_written += other.bytes_written;
        self.rows_visited += other.rows_visited;
        self.rows_materialized += other.rows_materialized;
        self.whole_state_copies += other.whole_state_copies;
        self.whole_state_serializations += other.whole_state_serializations;
        self.sql_statements += other.sql_statements;
    }

    fn minus(&self, before: &OpCounts) -> OpCounts {
        OpCounts {
            bytes_read: self.bytes_read - before.bytes_read,
            bytes_decoded: self.bytes_decoded - before.bytes_decoded,
            bytes_encoded: self.bytes_encoded - before.bytes_encoded,
            bytes_written: self.bytes_written - before.bytes_written,
            rows_visited: self.rows_visited - before.rows_visited,
            rows_materialized: self.rows_materialized - before.rows_materialized,
            whole_state_copies: self.whole_state_copies - before.whole_state_copies,
            whole_state_serializations: self.whole_state_serializations
                - before.whole_state_serializations,
            sql_statements: self.sql_statements - before.sql_statements,
        }
    }
}

thread_local! {
    static THREAD: Cell<OpCounts> = const { Cell::new(OpCounts {
        bytes_read: 0, bytes_decoded: 0, bytes_encoded: 0, bytes_written: 0,
        rows_visited: 0, rows_materialized: 0, whole_state_copies: 0,
        whole_state_serializations: 0, sql_statements: 0,
    }) };
}

static TOTALS: OnceLock<Mutex<BTreeMap<&'static str, OpCounts>>> = OnceLock::new();

fn totals() -> std::sync::MutexGuard<'static, BTreeMap<&'static str, OpCounts>> {
    TOTALS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record counts for `store` on this thread and in the process totals.
pub fn note(store: &'static str, counts: OpCounts) {
    THREAD.with(|cell| {
        let mut current = cell.get();
        current.add(&counts);
        cell.set(current);
    });
    totals().entry(store).or_default().add(&counts);
}

/// Legacy whole-state paths call this when they clone a whole store state.
pub fn note_whole_state_copy(store: &'static str) {
    note(
        store,
        OpCounts {
            whole_state_copies: 1,
            ..OpCounts::default()
        },
    );
}

/// Legacy whole-state paths call this when they serialize a whole store state.
pub fn note_whole_state_serialization(store: &'static str) {
    note(
        store,
        OpCounts {
            whole_state_serializations: 1,
            ..OpCounts::default()
        },
    );
}

/// Counts recorded on this thread while `f` ran.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, OpCounts) {
    let before = THREAD.with(Cell::get);
    let value = f();
    let after = THREAD.with(Cell::get);
    (value, after.minus(&before))
}

/// Process-wide cumulative counts of one store.
pub fn store_counters(store: &str) -> OpCounts {
    totals().get(store).copied().unwrap_or_default()
}

pub fn snapshot_all() -> Vec<(&'static str, OpCounts)> {
    totals().iter().map(|(store, counts)| (*store, *counts)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_sees_only_this_threads_work() {
        let ((), counts) = measure(|| {
            note(
                "metrics-fixture",
                OpCounts {
                    rows_visited: 2,
                    ..OpCounts::default()
                },
            );
            std::thread::spawn(|| note_whole_state_copy("metrics-fixture"))
                .join()
                .unwrap();
        });
        assert_eq!(counts.rows_visited, 2);
        assert_eq!(counts.whole_state_copies, 0);
        let total = store_counters("metrics-fixture");
        assert!(total.rows_visited >= 2 && total.whole_state_copies >= 1);
    }
}
