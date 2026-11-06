//! Common store layer (design 058 §2, slice S0).
//!
//! Shared facilities for keyed persistent state: point reads, keyset pages,
//! CAS writes, change cursors with tombstones, a same-transaction outbox,
//! in-process wake-ups and per-operation counters. Each store keeps its own
//! transaction domain (one SQLite file or one JSON file); this is not a
//! host-wide database and there is no host-wide write lock.
//!
//! Every store, on the layer or not yet, is registered in [`manifest`]. The
//! ratchet tests in [`ratchet`] fail when a new persistent writer appears
//! without a manifest entry, or when a whole-state access site is added.
//! The contract is `.claude/reports/058-s0-contract.md` in cutex-desktop.

pub mod cursor;
pub mod db;
pub mod domain;
pub mod keyed;
pub mod kv;
pub mod manifest;
pub mod metrics;
pub mod notify;
pub mod outbox;

#[cfg(test)]
mod ratchet;
#[cfg(test)]
mod scale_tests;

/// Per-store commit sequence (change-log order). Strictly increasing, never reused.
pub type Seq = u64;
/// Per-record revision. Never 0 for a stored row.
pub type Revision = u64;

/// Identity of a store's change history. A new epoch invalidates every
/// cursor of that store (migration, restore, history reset).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Epoch(pub u64);

pub use cursor::{ChangeCursor, ListCursor};
pub use db::{ReadTx, StoreDb, StoreHead, WriteTx};
pub use domain::{DomainKind, TxDomain};
pub use keyed::{
    ApplyBatch, Change, ChangeKind, ChangesOutcome, Commit, Expect, IndexedFilter, KeyedStore,
    Many, Mutation, Page, PageBudget, ResyncReason, Scope, ScopeKind, StoreError, StoreKey,
    Versioned,
};
pub use kv::KvStore;
pub use manifest::{spec, Access, Canonical, Rollback, StoreClass, StoreSpec, MANIFEST};
pub use metrics::OpCounts;
pub use notify::{Subscription, Wake};
pub use outbox::OutboxItem;
