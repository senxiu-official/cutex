//! Keyed repository contract shared by every store on the layer.

use std::fmt;

use super::cursor::{ChangeCursor, ListCursor};
use super::manifest::StoreSpec;
use super::notify::{self, Subscription};
use super::outbox::OutboxItem;
use super::{Revision, Seq};

/// Largest `Change::summary` the layer accepts. Bodies are fetched with `get`.
pub const MAX_CHANGE_SUMMARY_BYTES: usize = 512;

/// Authorization range of a read or write. Callers authorize before calling a
/// repository; repositories never widen the scope they are given.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Host,
    Session(String),
    Recipient(String),
    Project(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Host,
    Session,
    Recipient,
    Project,
}

impl Scope {
    pub fn kind(&self) -> ScopeKind {
        match self {
            Self::Host => ScopeKind::Host,
            Self::Session(_) => ScopeKind::Session,
            Self::Recipient(_) => ScopeKind::Recipient,
            Self::Project(_) => ScopeKind::Project,
        }
    }

    /// Text form stored in SQLite.
    pub fn encode(&self) -> String {
        match self {
            Self::Host => "host".into(),
            Self::Session(id) => format!("session:{id}"),
            Self::Recipient(id) => format!("recipient:{id}"),
            Self::Project(id) => format!("project:{id}"),
        }
    }

    pub fn decode(text: &str) -> Result<Self, StoreError> {
        if text == "host" {
            return Ok(Self::Host);
        }
        let (kind, id) = text
            .split_once(':')
            .ok_or_else(|| StoreError::Invalid(format!("malformed scope {text:?}")))?;
        if id.is_empty() {
            return Err(StoreError::Invalid(format!("empty scope id in {text:?}")));
        }
        match kind {
            "session" => Ok(Self::Session(id.into())),
            "recipient" => Ok(Self::Recipient(id.into())),
            "project" => Ok(Self::Project(id.into())),
            _ => Err(StoreError::Invalid(format!("unknown scope kind {kind:?}"))),
        }
    }
}

/// Row key. `encode` must sort like `Ord`: keyset paging relies on it.
pub trait StoreKey:
    Clone + Ord + Eq + std::hash::Hash + fmt::Debug + Send + Sync + 'static
{
    fn encode(&self) -> String;
    fn decode(text: &str) -> anyhow::Result<Self>;
}

impl StoreKey for String {
    fn encode(&self) -> String {
        self.clone()
    }
    fn decode(text: &str) -> anyhow::Result<Self> {
        Ok(text.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versioned<K, R> {
    pub key: K,
    pub revision: Revision,
    /// Commit sequence that wrote this revision.
    pub seq: Seq,
    pub value: R,
}

/// Row and byte limit of one page or change batch. A non-empty range always
/// yields at least one row, even when that row alone exceeds `max_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageBudget {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl PageBudget {
    pub const LIST_DEFAULT: Self = Self {
        max_rows: 200,
        max_bytes: 256 * 1024,
    };
    pub const DELTA_DEFAULT: Self = Self {
        max_rows: 1000,
        max_bytes: 64 * 1024,
    };

    /// Whether a row of `bytes` still fits after `rows`/`used` already taken.
    pub(crate) fn admits(&self, rows: usize, used: usize, bytes: usize) -> bool {
        rows == 0 || (rows < self.max_rows && used.saturating_add(bytes) <= self.max_bytes)
    }
}

#[derive(Clone, Debug)]
pub struct Page<K, R> {
    pub rows: Vec<Versioned<K, R>>,
    /// `None` at the end of the range.
    pub next: Option<ListCursor>,
    /// Store head of the WAL snapshot this page was read from.
    pub snapshot_seq: Seq,
    /// The single row of this page exceeded the byte budget.
    pub oversize_first: bool,
}

#[derive(Clone, Debug)]
pub struct Many<K, R> {
    pub found: Vec<Versioned<K, R>>,
    /// No row (never written or deleted).
    pub missing: Vec<K>,
    /// Not read because the byte budget ran out; ask again.
    pub deferred: Vec<K>,
    pub snapshot_seq: Seq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Any,
    Absent,
    Revision(Revision),
}

#[derive(Clone, Debug)]
pub enum Mutation<K, R> {
    Put { key: K, value: R, expect: Expect },
    /// Removes the row and records a tombstone change.
    Delete { key: K, expect: Expect },
}

#[derive(Clone, Debug)]
pub struct ApplyBatch<K, R> {
    pub scope: Scope,
    pub mutations: Vec<Mutation<K, R>>,
    /// Committed in the same SQLite transaction as the mutations.
    pub outbox: Vec<OutboxItem>,
}

impl<K, R> ApplyBatch<K, R> {
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            mutations: Vec::new(),
            outbox: Vec::new(),
        }
    }
    pub fn put(mut self, key: K, value: R, expect: Expect) -> Self {
        self.mutations.push(Mutation::Put { key, value, expect });
        self
    }
    pub fn delete(mut self, key: K, expect: Expect) -> Self {
        self.mutations.push(Mutation::Delete { key, expect });
        self
    }
    pub fn outbox(mut self, item: OutboxItem) -> Self {
        self.outbox.push(item);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit<K> {
    /// Store head after the commit; unchanged when the batch changed nothing.
    pub seq: Seq,
    /// New revision per mutated key (0 for a delete).
    pub revisions: Vec<(K, Revision)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Put,
    Delete,
}

impl ChangeKind {
    pub(crate) fn code(self) -> i64 {
        match self {
            Self::Put => 1,
            Self::Delete => 2,
        }
    }
    pub(crate) fn from_code(code: i64) -> anyhow::Result<Self> {
        match code {
            1 => Ok(Self::Put),
            2 => Ok(Self::Delete),
            _ => anyhow::bail!("unknown change kind {code}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change<K> {
    pub seq: Seq,
    pub scope: Scope,
    pub key: K,
    pub revision: Revision,
    pub kind: ChangeKind,
    pub summary: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub enum ChangesOutcome<K> {
    Page {
        changes: Vec<Change<K>>,
        next: ChangeCursor,
        /// Store head seen by this read.
        watermark: Seq,
        more: bool,
    },
    /// Do one budgeted `list` and continue from `watermark`. Never a reason
    /// to replay messages or wake agents again.
    ResyncRequired { reason: ResyncReason, watermark: Seq },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncReason {
    EpochChanged,
    BelowRetention,
    WrongInstance,
    ScopeMismatch,
    FilterMismatch,
    StoreMismatch,
}

#[derive(Debug)]
pub enum StoreError {
    /// CAS failure; reload the key and re-apply.
    Conflict {
        key: String,
        expected: Expect,
        actual: Revision,
    },
    Resync(ResyncReason),
    /// SQLite stayed busy past the busy timeout. Retryable; not a business "busy".
    Busy,
    Invalid(String),
    Other(anyhow::Error),
}

impl StoreError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Conflict { .. } | Self::Busy)
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict {
                key,
                expected,
                actual,
            } => write!(
                f,
                "store revision changed for {key:?}: expected {expected:?}, actual {actual}"
            ),
            Self::Resync(reason) => write!(f, "store cursor needs resync: {reason:?}"),
            Self::Busy => f.write_str("store database is busy"),
            Self::Invalid(message) => write!(f, "invalid store request: {message}"),
            Self::Other(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<anyhow::Error> for StoreError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<StoreError>() {
            Ok(store) => store,
            Err(error) => {
                if is_busy(&error) {
                    Self::Busy
                } else {
                    Self::Other(error)
                }
            }
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::from(anyhow::Error::from(error))
    }
}

fn is_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(code, _))
                if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        )
    })
}

/// Indexed filter for `list`. The fingerprint is pinned in the cursor.
pub trait IndexedFilter: Clone + fmt::Debug {
    fn fingerprint(&self) -> u64;
}

/// All rows of the scope in key order.
impl IndexedFilter for () {
    fn fingerprint(&self) -> u64 {
        0
    }
}

pub trait KeyedStore: Send + Sync {
    type Key: StoreKey;
    type Record: Clone + Send + Sync;
    type Filter: IndexedFilter;

    fn spec(&self) -> &'static StoreSpec;

    fn get(
        &self,
        scope: &Scope,
        key: &Self::Key,
    ) -> Result<Option<Versioned<Self::Key, Self::Record>>, StoreError>;

    fn get_many(
        &self,
        scope: &Scope,
        keys: &[Self::Key],
        max_bytes: usize,
    ) -> Result<Many<Self::Key, Self::Record>, StoreError>;

    /// Keyset page strictly after `after`, read in one WAL snapshot.
    fn list(
        &self,
        scope: &Scope,
        filter: &Self::Filter,
        after: Option<&ListCursor>,
        budget: PageBudget,
    ) -> Result<Page<Self::Key, Self::Record>, StoreError>;

    fn apply(
        &self,
        batch: ApplyBatch<Self::Key, Self::Record>,
    ) -> Result<Commit<Self::Key>, StoreError>;

    fn changes(
        &self,
        scope: &Scope,
        after: &ChangeCursor,
        budget: PageBudget,
    ) -> Result<ChangesOutcome<Self::Key>, StoreError>;

    /// Cursor at the current head of `scope`, for a reader that just listed.
    fn change_head(&self, scope: &Scope) -> Result<ChangeCursor, StoreError>;

    /// In-process wake-up hint; cross-process truth is `changes`.
    fn subscribe(&self, scope: &Scope) -> Subscription {
        notify::subscribe(self.spec().id, scope.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_text_round_trips_and_rejects_garbage() {
        for scope in [
            Scope::Host,
            Scope::Session("s:1".into()),
            Scope::Recipient("a".into()),
            Scope::Project("p".into()),
        ] {
            assert_eq!(Scope::decode(&scope.encode()).unwrap(), scope);
        }
        for bad in ["", "session:", "nope:x", "hostx"] {
            assert!(Scope::decode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn budget_always_admits_the_first_row() {
        let budget = PageBudget {
            max_rows: 2,
            max_bytes: 10,
        };
        assert!(budget.admits(0, 0, 1000));
        assert!(budget.admits(1, 4, 6));
        assert!(!budget.admits(1, 4, 7));
        assert!(!budget.admits(2, 0, 1));
    }

    #[test]
    fn conflict_travels_through_anyhow() {
        let error = anyhow::Error::from(StoreError::Conflict {
            key: "k".into(),
            expected: Expect::Absent,
            actual: 3,
        });
        let back = StoreError::from(error);
        assert!(back.is_retryable());
        assert!(back.to_string().contains("revision changed"));
    }
}
