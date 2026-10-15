//! Durable `requestId` idempotency for Management v2 requests.
//!
//! One SQLite database (WAL) per cutex home,
//! `runtime/management-v2/request-idempotency.sqlite3`, one row per
//! `(cutexSessionId | scope, requestId)`. Every operation is a short
//! single-row transaction, so requests of different sessions no longer queue
//! behind a whole-file rewrite (the former `request-idempotency.json` was
//! re-read, re-parsed and atomically rewritten twice per request under a
//! process-wide lock, and grew without bound).
//!
//! Policy:
//! - Read-only methods ([`READ_ONLY_METHODS`]) are not recorded at all: a
//!   retried read simply reads again. Their claims are ephemeral and the
//!   complete / release / outcome-unknown calls are no-ops.
//! - A completed response body is retained only up to
//!   [`RetentionPolicy::max_retained_body_bytes`] (default 64 KiB). A replay of
//!   a larger completed request returns `409 request_result_not_retained`
//!   (the request did complete; the client must resync instead of replaying).
//!   The response status and SHA-256 are always kept.
//! - Completed rows expire after 24 h; forwarded / outcome-unknown rows after
//!   7 days.
//! - At most `max_records` rows are kept. Capacity is made by evicting
//!   completed rows only, oldest first: an in-flight (`forwarded`) or
//!   `outcome_unknown` row is what stops a replayed `requestId` from being
//!   forwarded twice, so the cap never drops one. A new request is admitted in
//!   the same transaction that makes room for it. Only if the store is full of
//!   unresolved rows does admission fall back to evicting outcome-unknown rows
//!   (and rows a dead process left `forwarded`) older than
//!   [`RetentionPolicy::unresolved_min_retention`] (default 1 h; clients retry
//!   within seconds, not hours), oldest first, with a warning. Anything else
//!   (this process's own in-flight requests, younger unresolved rows) is kept
//!   and the new request is refused with a retryable
//!   `503 idempotency_capacity_exceeded`. So a burst of unresolved requests
//!   can refuse new ones for at most that grace period, never for the 7-day
//!   unresolved TTL.
//! - On first open, recent records of the legacy JSON file are imported and
//!   the file is renamed to `request-idempotency.json.bak` (never deleted).

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::TransactionBehavior;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use uuid::Uuid;

use crate::config::paths::runtime_dir;

use super::native_requests::canonical_request_sha256;

pub const IDEMPOTENCY_DB_FILE: &str = "request-idempotency.sqlite3";
const LEGACY_JSON_FILE: &str = "request-idempotency.json";
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_POOLED_CONNECTIONS: usize = 8;
/// Browser writes have a separate budget; they never reserve or evict a row
/// from the Management message/turn domains. Reads/claim polling are ephemeral.
/// At pressure only completed browser rows aged >=10 min can leave early;
/// unknown/in-flight rows retain their full seven-day browser TTL.
const MAX_BROWSER_RECORDS: usize = 2_000;
const MAX_BROWSER_SESSION_RECORDS: usize = 256;
// Reserve unknown capacity before dispatch as any in-flight write may become
// unknown. Eight disconnected sessions must not consume the global budget.
const MAX_BROWSER_SESSION_UNRESOLVED: usize = 16;
const BROWSER_COMPLETED_GRACE_MS: i64 = 10 * 60 * 1000;
const BROWSER_UNRESOLVED_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

static IDEMPOTENCY_REPOSITORY: super::process_repository::ProcessRepository<
    RequestIdempotencyRepository,
> = super::process_repository::ProcessRepository::new();

/// Methods that change nothing: their responses are not recorded.
pub const READ_ONLY_METHODS: &[&str] = &[
    // native (app-server) reads
    "thread/read",
    "thread/turns/list",
    "thread/items/list",
    "thread/backgroundTerminals/list",
    "thread/goal/get",
    "model/list",
    "claude/models/catalog",
    "claude/model/status",
    "permissionProfile/list",
    "collaborationMode/list",
    "account/rateLimits/read",
    // cutex reads
    "cutex/session/get",
    "cutex/session/groups/get",
    "cutex/focus/get",
    "cutex/userInput/queue/list",
];

/// Whether a request of `method` is recorded for `requestId` replay.
/// Unknown methods are recorded (safe default).
pub fn method_needs_idempotency(method: &str) -> bool {
    !READ_ONLY_METHODS.contains(&method)
}

#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    pub completed_ttl: Duration,
    pub pending_ttl: Duration,
    pub max_records: usize,
    /// Under capacity pressure only: how long an outcome-unknown row (or one
    /// a dead process left `forwarded`) is kept before admission may evict it
    /// for a new request. In-flight rows of this process are never evicted.
    pub unresolved_min_retention: Duration,
    pub max_retained_body_bytes: usize,
    pub prune_interval: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            completed_ttl: Duration::from_secs(24 * 60 * 60),
            pending_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            max_records: 20_000,
            unresolved_min_retention: Duration::from_secs(60 * 60),
            max_retained_body_bytes: 64 * 1024,
            prune_interval: Duration::from_secs(10 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BeginRequest {
    Forward(RequestClaim),
    InProgress,
    Completed(StoredHttpResponse),
    Conflict,
    OutcomeUnknown,
    /// The store is full of unresolved rows that must not be evicted; the
    /// request was not recorded or forwarded and can be retried.
    CapacityExceeded(CapacityExceeded),
}

/// Why a new request was refused: `503 idempotency_capacity_exceeded`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityExceeded {
    pub max_records: usize,
    /// Forwarded + outcome-unknown rows (all non-completed rows).
    pub unresolved_records: u64,
    /// In-flight rows of this process.
    pub in_flight_records: u64,
    /// Ordinary domains: when the oldest non-in-flight unresolved row becomes
    /// evictable (or a short in-flight retry). Browser: earliest eligible
    /// completed grace/unknown TTL, never an assumed near-term completion.
    pub retry_after_ms: u64,
}

impl CapacityExceeded {
    pub const CODE: &'static str = "idempotency_capacity_exceeded";
    pub const MESSAGE: &'static str = "the request store is full of requests whose outcome is still pending or unknown; nothing was forwarded, retry later";

    pub fn details(&self) -> Value {
        json!({
            "maxRecords": self.max_records,
            "unresolvedRecords": self.unresolved_records,
            "inFlightRecords": self.in_flight_records,
            "retryAfterMs": self.retry_after_ms,
        })
    }
}

fn settle_browser_claim(claim: &RequestClaim) -> bool {
    !claim.cutex_session_id.starts_with("browser:") || claim.browser_settled.set(()).is_ok()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StoredHttpResponse {
    pub status: u16,
    pub reason: String,
    pub body: Value,
}

/// Browser-only metadata lookup. No response body from another domain can be
/// reached through this API; the prefix is a credential/session digest.
#[derive(Clone, Debug)]
pub struct BrowserRecord {
    pub scope: String,
    pub body_sha256: String,
    pub state: String,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestClaim {
    cutex_session_id: String,
    request_id: String,
    body_sha256: String,
    process_instance_id: String,
    /// `false` for read-only methods: nothing was recorded.
    durable: bool,
    // Browser duplicates share settlement ownership. A late waiter must not
    // delete/complete a newly admitted row after a deterministic refusal was
    // released. A failed settlement conservatively leaves the old tombstone.
    browser_settled: Arc<OnceLock<()>>,
}

const STATE_FORWARDED: &str = "forwarded";
const STATE_COMPLETED: &str = "completed";
const STATE_OUTCOME_UNKNOWN: &str = "outcome_unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserClearCounts {
    pub outcome_unknown: u64,
    pub abandoned_forwarded: u64,
}

impl BrowserClearCounts {
    pub fn total(self) -> u64 { self.outcome_unknown + self.abandoned_forwarded }
}

pub struct RequestIdempotencyRepository {
    db_path: PathBuf,
    process_instance_id: String,
    policy: RetentionPolicy,
    pool: Mutex<Vec<Connection>>,
    last_prune_ms: AtomicI64,
}

pub fn request_idempotency_repository() -> anyhow::Result<&'static RequestIdempotencyRepository> {
    IDEMPOTENCY_REPOSITORY.get_or_open(
        || Ok(runtime_dir()?.join("management-v2")),
        |dir| RequestIdempotencyRepository::open(dir, Uuid::new_v4().to_string()),
    )
}

impl RequestIdempotencyRepository {
    /// Owner recovery operation for browser writes. Removing an
    /// `outcome_unknown` row allows that request key to execute again.
    pub fn clear_browser_records(&self, cutex_session_id: Option<&str>) -> anyhow::Result<BrowserClearCounts> {
        let session_group = cutex_session_id.map(|session| {
            let scope = format!("browser:{}:session:{:x}:owner", "0".repeat(64), Sha256::digest(session.as_bytes()));
            browser_session_group(&scope).expect("constructed browser session group").to_string()
        });
        self.with_connection(|connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (outcome_unknown, abandoned_forwarded): (i64, i64) = transaction.query_row(
                "SELECT COALESCE(SUM(state='outcome_unknown'),0), COALESCE(SUM(state='forwarded' AND process_instance_id<>?2),0)
                 FROM requests WHERE substr(cutex_session_id,1,8)='browser:'
                 AND (?1 IS NULL OR substr(cutex_session_id,74,73)=?1)
                 AND state<>'completed' AND NOT (state='forwarded' AND process_instance_id=?2)",
                params![session_group, self.process_instance_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            transaction.execute(
                "DELETE FROM requests WHERE substr(cutex_session_id,1,8)='browser:'
                 AND (?1 IS NULL OR substr(cutex_session_id,74,73)=?1)
                 AND state<>'completed' AND NOT (state='forwarded' AND process_instance_id=?2)",
                params![session_group, self.process_instance_id],
            )?;
            transaction.commit()?;
            Ok(BrowserClearCounts { outcome_unknown: outcome_unknown as u64, abandoned_forwarded: abandoned_forwarded as u64 })
        })
    }

    pub fn browser_lookup(
        &self,
        prefix: &str,
        request_id: &str,
    ) -> anyhow::Result<Vec<BrowserRecord>> {
        anyhow::ensure!(
            prefix.len() == 73
                && prefix.starts_with("browser:")
                && prefix.ends_with(':')
                && prefix.as_bytes()[8..72]
                    .iter()
                    .all(|b| b.is_ascii_hexdigit()),
            "invalid browser scope prefix"
        );
        self.maybe_prune();
        self.with_connection(|connection| {
            let mut statement=connection.prepare("SELECT cutex_session_id,body_sha256,state,response_body FROM requests WHERE cutex_session_id LIKE ?1 AND request_id=?2 ORDER BY created_at ASC LIMIT 2")?;
            let rows=statement.query_map(params![format!("{prefix}%"),request_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?)))?;
            let mut records=Vec::new();
            for row in rows {let (scope,body_sha256,state,body)=row?;records.push(BrowserRecord{scope,body_sha256,state,metadata:body.map(|body|serde_json::from_str(&body)).transpose()?});}
            Ok(records)
        })
    }
    pub fn open(root: impl Into<PathBuf>, process_instance_id: String) -> anyhow::Result<Self> {
        Self::open_with_policy(root, process_instance_id, RetentionPolicy::default())
    }

    pub fn open_with_policy(
        root: impl Into<PathBuf>,
        process_instance_id: String,
        policy: RetentionPolicy,
    ) -> anyhow::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        secure_directory(&root)?;
        let db_path = root.join(IDEMPOTENCY_DB_FILE);
        // Create the database owner-only before SQLite opens it: SQLite gives
        // the -wal / -shm files the database file's mode, and they hold
        // response bodies too.
        create_private_file(&db_path)?;
        let mut connection = open_connection(&db_path)?;
        secure_sqlite_files(&db_path)?;
        let now = now_ms();
        if let Err(error) = migrate_legacy_json(&mut connection, &root, &policy, now) {
            eprintln!("management v2 idempotency: legacy import failed: {error:#}");
        }
        prune(&connection, &policy, now)?;
        Ok(Self {
            db_path,
            process_instance_id,
            policy,
            pool: Mutex::new(vec![connection]),
            last_prune_ms: AtomicI64::new(now),
        })
    }

    /// `begin` for a request of `method`; read-only methods get an ephemeral
    /// claim without touching the store.
    pub fn begin_for_method(
        &self,
        method: &str,
        cutex_session_id: &str,
        request_id: &str,
        canonical_body: &Value,
    ) -> anyhow::Result<BeginRequest> {
        if method_needs_idempotency(method) {
            return self.begin(cutex_session_id, request_id, canonical_body);
        }
        Ok(BeginRequest::Forward(RequestClaim {
            cutex_session_id: cutex_session_id.to_string(),
            request_id: request_id.to_string(),
            body_sha256: String::new(),
            process_instance_id: self.process_instance_id.clone(),
            durable: false,
            browser_settled: Arc::new(OnceLock::new()),
        }))
    }

    pub fn begin(
        &self,
        cutex_session_id: &str,
        request_id: &str,
        canonical_body: &Value,
    ) -> anyhow::Result<BeginRequest> {
        let body_sha256 = canonical_request_sha256(canonical_body)?;
        self.maybe_prune();
        self.with_connection(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing = transaction
                .query_row(
                    "SELECT body_sha256, state, process_instance_id, response_status,
                            response_reason, response_body, response_sha256
                     FROM requests WHERE cutex_session_id = ?1 AND request_id = ?2",
                    params![cutex_session_id, request_id],
                    |row| {
                        Ok(ExistingRow {
                            body_sha256: row.get(0)?,
                            state: row.get(1)?,
                            process_instance_id: row.get(2)?,
                            response_status: row.get(3)?,
                            response_reason: row.get(4)?,
                            response_body: row.get(5)?,
                            response_sha256: row.get(6)?,
                        })
                    },
                )
                .optional()?;
            let now = now_ms();
            let outcome = match existing {
                Some(row) if row.body_sha256 != body_sha256 => BeginRequest::Conflict,
                Some(row) if row.state == STATE_COMPLETED => {
                    BeginRequest::Completed(replayed_response(request_id, row)?)
                }
                Some(row)
                    if row.state == STATE_FORWARDED
                        && row.process_instance_id == self.process_instance_id =>
                {
                    BeginRequest::InProgress
                }
                Some(row) if row.state == STATE_FORWARDED => {
                    transaction.execute(
                        "UPDATE requests SET state = ?3, updated_at = ?4,
                                response_status = NULL, response_reason = NULL,
                                response_body = NULL, response_sha256 = NULL
                         WHERE cutex_session_id = ?1 AND request_id = ?2",
                        params![cutex_session_id, request_id, STATE_OUTCOME_UNKNOWN, now],
                    )?;
                    BeginRequest::OutcomeUnknown
                }
                Some(_) => BeginRequest::OutcomeUnknown,
                // Room is made (or refused) in this same write transaction,
                // so concurrent admissions cannot overshoot the cap. A
                // refusal still commits the completed rows evicted on the way.
                None => match reserve_capacity(
                    &transaction,
                    &self.policy,
                    &self.process_instance_id,
                    now,
                    cutex_session_id,
                )? {
                    Some(full) => BeginRequest::CapacityExceeded(full),
                    None => {
                        transaction.execute(
                            "INSERT INTO requests (cutex_session_id, request_id, body_sha256,
                                    state, process_instance_id, created_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                            params![
                                cutex_session_id,
                                request_id,
                                body_sha256,
                                STATE_FORWARDED,
                                self.process_instance_id,
                                now
                            ],
                        )?;
                        BeginRequest::Forward(RequestClaim {
                            cutex_session_id: cutex_session_id.to_string(),
                            request_id: request_id.to_string(),
                            body_sha256: body_sha256.clone(),
                            process_instance_id: self.process_instance_id.clone(),
                            durable: true,
                            browser_settled: Arc::new(OnceLock::new()),
                        })
                    }
                },
            };
            transaction.commit()?;
            Ok(outcome)
        })
    }

    pub fn complete(
        &self,
        claim: &RequestClaim,
        status: u16,
        reason: &str,
        body: Value,
    ) -> anyhow::Result<()> {
        if !claim.durable {
            return Ok(());
        }
        if claim.cutex_session_id.starts_with("browser:")
            && body.get("status").and_then(Value::as_str) == Some("outcome_unknown")
        {
            return self.mark_outcome_unknown(claim);
        }
        // Browser receipts never persist page text or images, including tiny
        // images. Other domains retain their original response-body policy.
        let body = if claim.cutex_session_id.starts_with("browser:") {
            let mut metadata = json!({"requestId":claim.request_id,"resultRetained":false,
                "status":body.get("status").and_then(Value::as_str).filter(|v|matches!(*v,"succeeded"|"failed"|"cancelled"|"outcome_unknown")).unwrap_or("failed")});
            if let Some(code) = body
                .pointer("/error/code")
                .and_then(Value::as_str)
                .filter(|v| {
                    !v.is_empty()
                        && v.len() <= 64
                        && v.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
            {
                metadata["error"] = json!({"code":code});
            }
            if let Some(id) = body.get("approvalId").and_then(Value::as_str).filter(|v| {
                v.starts_with("apr-")
                    && v.len() <= 64
                    && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }) {
                metadata["approvalId"] = json!(id);
            }
            metadata
        } else {
            body
        };
        let text = serde_json::to_string(&body)?;
        let sha = sha256_hex(text.as_bytes());
        let retained = (text.len() <= self.policy.max_retained_body_bytes).then_some(text);
        if !settle_browser_claim(claim) {
            return Ok(());
        }
        self.update_claim(claim, |connection| {
            Ok(connection.execute(
                "UPDATE requests SET state = ?5, updated_at = ?6, response_status = ?7,
                        response_reason = ?8, response_body = ?9, response_sha256 = ?10
                 WHERE cutex_session_id = ?1 AND request_id = ?2 AND state = 'forwarded'
                   AND body_sha256 = ?3 AND process_instance_id = ?4",
                params![
                    claim.cutex_session_id,
                    claim.request_id,
                    claim.body_sha256,
                    claim.process_instance_id,
                    STATE_COMPLETED,
                    now_ms(),
                    status,
                    reason,
                    retained,
                    sha
                ],
            )?)
        })
    }

    pub fn mark_outcome_unknown(&self, claim: &RequestClaim) -> anyhow::Result<()> {
        if !claim.durable {
            return Ok(());
        }
        if !settle_browser_claim(claim) {
            return Ok(());
        }
        self.update_claim(claim, |connection| {
            Ok(connection.execute(
                "UPDATE requests SET state = ?5, updated_at = ?6, response_status = NULL,
                        response_reason = NULL, response_body = NULL, response_sha256 = NULL
                 WHERE cutex_session_id = ?1 AND request_id = ?2 AND state = 'forwarded'
                   AND body_sha256 = ?3 AND process_instance_id = ?4",
                params![
                    claim.cutex_session_id,
                    claim.request_id,
                    claim.body_sha256,
                    claim.process_instance_id,
                    STATE_OUTCOME_UNKNOWN,
                    now_ms()
                ],
            )?)
        })
    }

    pub fn release_before_forward(&self, claim: &RequestClaim) -> anyhow::Result<()> {
        if !claim.durable {
            return Ok(());
        }
        if !settle_browser_claim(claim) {
            return Ok(());
        }
        self.update_claim(claim, |connection| {
            Ok(connection.execute(
                "DELETE FROM requests
                 WHERE cutex_session_id = ?1 AND request_id = ?2 AND state = 'forwarded'
                   AND body_sha256 = ?3 AND process_instance_id = ?4",
                params![
                    claim.cutex_session_id,
                    claim.request_id,
                    claim.body_sha256,
                    claim.process_instance_id
                ],
            )?)
        })
    }

    /// Browser-only: the caller proved rejection before any target action.
    /// Never use for a timeout, disconnect or otherwise uncertain dispatch.
    pub fn release_browser_refusal(&self, claim: &RequestClaim) -> anyhow::Result<()> {
        anyhow::ensure!(claim.cutex_session_id.starts_with("browser:"), "browser claim required");
        self.release_before_forward(claim)
    }

    /// Runs expiry + cap now (normally done at open and every
    /// `prune_interval`).
    pub fn prune_now(&self) -> anyhow::Result<()> {
        let now = now_ms();
        self.last_prune_ms.store(now, Ordering::Relaxed);
        self.with_connection(|connection| prune(connection, &self.policy, now))
    }

    #[cfg(test)]
    fn record_count(&self) -> i64 {
        self.with_connection(|connection| {
            Ok(connection.query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))?)
        })
        .expect("count records")
    }

    fn update_claim(
        &self,
        claim: &RequestClaim,
        update: impl FnOnce(&Connection) -> anyhow::Result<usize>,
    ) -> anyhow::Result<()> {
        let _ = claim;
        self.with_connection(|connection| {
            if update(connection)? == 0 {
                anyhow::bail!("idempotency claim no longer exists or its ownership changed");
            }
            Ok(())
        })
    }

    fn maybe_prune(&self) {
        let now = now_ms();
        let last = self.last_prune_ms.load(Ordering::Relaxed);
        if now - last < self.policy.prune_interval.as_millis() as i64 {
            return;
        }
        if self
            .last_prune_ms
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        if let Err(error) = self.with_connection(|connection| prune(connection, &self.policy, now))
        {
            eprintln!("management v2 idempotency: prune failed: {error:#}");
        }
    }

    fn with_connection<T>(
        &self,
        action: impl FnOnce(&mut Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let pooled = self
            .pool
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 idempotency pool was poisoned"))?
            .pop();
        let mut connection = match pooled {
            Some(connection) => connection,
            None => open_connection(&self.db_path)?,
        };
        let result = action(&mut connection);
        if result.is_ok() {
            if let Ok(mut pool) = self.pool.lock() {
                if pool.len() < MAX_POOLED_CONNECTIONS {
                    pool.push(connection);
                }
            }
        }
        result
    }
}

struct ExistingRow {
    body_sha256: String,
    state: String,
    process_instance_id: String,
    response_status: Option<u16>,
    response_reason: Option<String>,
    response_body: Option<String>,
    response_sha256: Option<String>,
}

fn replayed_response(request_id: &str, row: ExistingRow) -> anyhow::Result<StoredHttpResponse> {
    let status = row
        .response_status
        .context("completed idempotency entry omitted response status")?;
    if let Some(body) = row.response_body {
        return Ok(StoredHttpResponse {
            status,
            reason: row.response_reason.unwrap_or_default(),
            body: serde_json::from_str(&body)
                .context("stored idempotency response body is not JSON")?,
        });
    }
    Ok(StoredHttpResponse {
        status: 409,
        reason: "Conflict".to_string(),
        body: json!({
            "contractVersion": 2,
            "requestId": request_id,
            "error": {
                "source": "cutex",
                "code": "request_result_not_retained",
                "message": "the request already completed, but its response was too large to retain for replay",
                "retryable": false,
                "details": {
                    "resyncRequired": true,
                    "originalStatus": status,
                    "responseSha256": row.response_sha256,
                },
            }
        }),
    })
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let connection = Connection::open(path).with_context(|| {
        format!(
            "failed to open management v2 idempotency database: {}",
            path.display()
        )
    })?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    // WAL + NORMAL: a committed row survives a process crash; only an OS
    // crash / power loss can drop the last commits (never corrupts).
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
            cutex_session_id TEXT NOT NULL,
            request_id TEXT NOT NULL,
            body_sha256 TEXT NOT NULL,
            state TEXT NOT NULL,
            process_instance_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            response_status INTEGER,
            response_reason TEXT,
            response_body TEXT,
            response_sha256 TEXT,
            PRIMARY KEY (cutex_session_id, request_id)
        );
        CREATE INDEX IF NOT EXISTS requests_updated_at ON requests (updated_at);",
    )?;
    Ok(connection)
}

fn prune(connection: &Connection, policy: &RetentionPolicy, now: i64) -> anyhow::Result<()> {
    connection.execute(
        "DELETE FROM requests WHERE state = 'completed' AND
         ((substr(cutex_session_id,1,8) <> 'browser:' AND updated_at < ?1)
          OR (substr(cutex_session_id,1,8) = 'browser:' AND updated_at < ?2))",
        params![
            now - policy.completed_ttl.as_millis() as i64,
            now - 24 * 60 * 60 * 1000
        ],
    )?;
    connection.execute(
        "DELETE FROM requests WHERE state <> 'completed' AND
         ((substr(cutex_session_id,1,8) <> 'browser:' AND updated_at < ?1)
          OR (substr(cutex_session_id,1,8) = 'browser:' AND updated_at < ?2))",
        params![
            now - policy.pending_ttl.as_millis() as i64,
            now - 7 * 24 * 60 * 60 * 1000
        ],
    )?;
    // The cap only ever drops completed rows; unresolved rows leave by their
    // TTL (or, under admission pressure, see `reserve_capacity`).
    let excess = ordinary_record_count(connection)? - policy.max_records as i64;
    if excess > 0 {
        evict_oldest_completed(connection, excess, now)?;
    }
    Ok(())
}

fn ordinary_record_count(connection: &Connection) -> anyhow::Result<i64> {
    Ok(connection.query_row(
        "SELECT COUNT(*) FROM requests WHERE substr(cutex_session_id,1,8) <> 'browser:'",
        [],
        |row| row.get(0),
    )?)
}

fn evict_oldest_completed(connection: &Connection, limit: i64, _now: i64) -> anyhow::Result<i64> {
    Ok(connection.execute(
        "DELETE FROM requests WHERE rowid IN
            (SELECT rowid FROM requests WHERE state = 'completed'
             AND substr(cutex_session_id,1,8) <> 'browser:'
             ORDER BY updated_at ASC LIMIT ?1)",
        params![limit],
    )? as i64)
}

/// Rows a capacity-pressed admission may evict after the completed ones:
/// outcome-unknown rows and rows another (by now dead) process left
/// `forwarded`, once older than `unresolved_min_retention`.
const STALE_UNRESOLVED: &str =
    "(state = 'outcome_unknown' OR (state = 'forwarded' AND process_instance_id <> ?1))";

// New scopes retain the original credential/session lookup prefix (73 bytes)
// and append session:<SHA256(session)>:<device>. This adds cross-credential,
// cross-device fairness without altering the SQLite schema or old replay keys.
// Legacy rows remain protected and count against the global browser budget.
fn browser_session_group(scope: &str) -> Option<&str> {
    let suffix = scope.get(73..)?;
    let digest = suffix.strip_prefix("session:")?.get(..64)?;
    (digest.bytes().all(|b| b.is_ascii_hexdigit()) && suffix.as_bytes().get(72) == Some(&b':'))
        .then(|| &suffix[..73])
}

fn reserve_browser_capacity(
    connection: &Connection,
    policy: &RetentionPolicy,
    process_instance_id: &str,
    now: i64,
    scope: &str,
) -> anyhow::Result<Option<CapacityExceeded>> {
    // Admission applies browser TTL immediately, not up to one prune interval
    // after the retry time returned below. No unresolved row leaves early.
    connection.execute(
        "DELETE FROM requests WHERE substr(cutex_session_id,1,8) = 'browser:'
         AND state <> 'completed' AND updated_at <= ?1",
        params![now - BROWSER_UNRESOLVED_TTL_MS],
    )?;
    let global_cap = MAX_BROWSER_RECORDS.min(policy.max_records.max(1));
    if let Some(group) = browser_session_group(scope) {
        let cap = MAX_BROWSER_SESSION_UNRESOLVED.min(global_cap);
        let (unresolved, in_flight, eligible_at): (i64, i64, Option<i64>) = connection.query_row(
            "SELECT COUNT(*),
             COALESCE(SUM(state = 'forwarded' AND process_instance_id = ?2),0),
             MIN(updated_at + ?3)
             FROM requests WHERE substr(cutex_session_id,1,8) = 'browser:'
             AND substr(cutex_session_id,74,73) = ?1 AND state <> 'completed'",
            params![group, process_instance_id, BROWSER_UNRESOLVED_TTL_MS],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if unresolved >= cap as i64 {
            return Ok(Some(CapacityExceeded {
                max_records: cap,
                unresolved_records: unresolved as u64,
                in_flight_records: in_flight as u64,
                retry_after_ms: eligible_at.unwrap_or(now).saturating_sub(now).max(1) as u64,
            }));
        }
    }
    let groups = browser_session_group(scope)
        .map(|group| (Some(group), MAX_BROWSER_SESSION_RECORDS.min(global_cap)))
        .into_iter()
        .chain(std::iter::once((None, global_cap)));
    for (group, cap) in groups {
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM requests WHERE substr(cutex_session_id,1,8) = 'browser:'
             AND (?1 IS NULL OR substr(cutex_session_id,74,73) = ?1)",
            params![group], |row| row.get(0),
        )?;
        let excess = count - cap as i64 + 1;
        if excess <= 0 { continue; }
        let evicted = connection.execute(
            "DELETE FROM requests WHERE rowid IN (
             SELECT rowid FROM requests WHERE substr(cutex_session_id,1,8) = 'browser:'
             AND (?1 IS NULL OR substr(cutex_session_id,74,73) = ?1)
             AND state = 'completed' AND updated_at <= ?2
             ORDER BY updated_at ASC LIMIT ?3)",
            params![group, now - BROWSER_COMPLETED_GRACE_MS, excess],
        )?;
        if evicted as i64 >= excess { continue; }
        let (unresolved, in_flight, eligible_at): (i64, i64, i64) = connection.query_row(
            "SELECT COALESCE(SUM(state <> 'completed'),0),
             COALESCE(SUM(state = 'forwarded' AND process_instance_id = ?2),0),
             MIN(updated_at + CASE WHEN state = 'completed' THEN ?3 ELSE ?4 END)
             FROM requests WHERE substr(cutex_session_id,1,8) = 'browser:'
             AND (?1 IS NULL OR substr(cutex_session_id,74,73) = ?1)",
            params![group, process_instance_id, BROWSER_COMPLETED_GRACE_MS, BROWSER_UNRESOLVED_TTL_MS],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        return Ok(Some(CapacityExceeded {
            max_records: cap,
            unresolved_records: unresolved as u64,
            in_flight_records: in_flight as u64,
            // Based on the current store, not a promise an in-flight request
            // will finish soon. A completion may permit an earlier retry.
            retry_after_ms: eligible_at.saturating_sub(now).max(1) as u64,
        }));
    }
    Ok(None)
}

/// Makes room for one new row inside the caller's write transaction:
/// completed rows first (oldest first), then stale unresolved rows past
/// their minimum retention. `Some` (nothing to admit) when only rows that
/// still protect a retry remain.
fn reserve_capacity(
    connection: &Connection,
    policy: &RetentionPolicy,
    process_instance_id: &str,
    now: i64,
    scope: &str,
) -> anyhow::Result<Option<CapacityExceeded>> {
    if scope.starts_with("browser:") {
        return reserve_browser_capacity(connection, policy, process_instance_id, now, scope);
    }
    let max_records = policy.max_records.max(1) as i64;
    let mut excess = ordinary_record_count(connection)? - max_records + 1;
    if excess <= 0 {
        return Ok(None);
    }
    excess -= evict_oldest_completed(connection, excess, now)?;
    if excess <= 0 {
        return Ok(None);
    }
    let cutoff = now - policy.unresolved_min_retention.as_millis() as i64;
    let evicted = connection.execute(
        &format!(
            "DELETE FROM requests WHERE rowid IN
                (SELECT rowid FROM requests WHERE {STALE_UNRESOLVED} AND updated_at < ?2
                 AND substr(cutex_session_id,1,8) <> 'browser:'
                 ORDER BY updated_at ASC LIMIT ?3)"
        ),
        params![process_instance_id, cutoff, excess],
    )? as i64;
    if evicted > 0 {
        eprintln!(
            "management v2 idempotency: store full of unresolved requests; evicted {evicted} outcome-unknown record(s) older than {}s (a replay of those requestIds is no longer deduplicated)",
            policy.unresolved_min_retention.as_secs()
        );
    }
    excess -= evicted;
    if excess <= 0 {
        return Ok(None);
    }
    let (unresolved, in_flight): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(state = 'forwarded' AND process_instance_id = ?1), 0)
         FROM requests WHERE state <> 'completed' AND substr(cutex_session_id,1,8) <> 'browser:'",
        params![process_instance_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let oldest_stale: Option<i64> = connection.query_row(
        &format!("SELECT MIN(updated_at) FROM requests WHERE {STALE_UNRESOLVED} AND substr(cutex_session_id,1,8) <> 'browser:'"),
        params![process_instance_id],
        |row| row.get(0),
    )?;
    let retention = policy.unresolved_min_retention.as_millis() as i64;
    let retry_after_ms = oldest_stale
        .map(|oldest| (oldest + retention - now).clamp(1_000, retention.max(1_000)))
        .unwrap_or(1_000);
    eprintln!(
        "management v2 idempotency: refusing a new request, {unresolved} unresolved record(s) ({in_flight} in flight) fill the {max_records}-record store"
    );
    Ok(Some(CapacityExceeded {
        max_records: max_records as usize,
        unresolved_records: unresolved as u64,
        in_flight_records: in_flight as u64,
        retry_after_ms: retry_after_ms as u64,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStore<'a> {
    #[serde(borrow)]
    entries: std::collections::BTreeMap<String, LegacyEntry<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyEntry<'a> {
    cutex_session_id: String,
    request_id: String,
    body_sha256: String,
    state: String,
    process_instance_id: String,
    created_at: String,
    updated_at: String,
    #[serde(borrow, default)]
    response: Option<LegacyResponse<'a>>,
}

#[derive(Deserialize)]
struct LegacyResponse<'a> {
    status: u16,
    reason: String,
    #[serde(borrow)]
    body: &'a RawValue,
}

/// Imports still-relevant rows of the legacy JSON store, then renames it to
/// `.bak`. Bodies stay borrowed from the file bytes (no `Value` tree), so a
/// 200 MB file costs ~its size in memory once.
fn migrate_legacy_json(
    connection: &mut Connection,
    root: &Path,
    policy: &RetentionPolicy,
    now: i64,
) -> anyhow::Result<()> {
    let legacy = root.join(LEGACY_JSON_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another process may have imported and renamed it meanwhile.
    let bytes = match fs::read(&legacy) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut imported = 0usize;
    let mut skipped = 0usize;
    match serde_json::from_slice::<LegacyStore<'_>>(&bytes) {
        Ok(store) => {
            let completed_cutoff = now - policy.completed_ttl.as_millis() as i64;
            let pending_cutoff = now - policy.pending_ttl.as_millis() as i64;
            let mut statement = transaction.prepare(
                "INSERT OR IGNORE INTO requests (cutex_session_id, request_id, body_sha256,
                        state, process_instance_id, created_at, updated_at, response_status,
                        response_reason, response_body, response_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for entry in store.entries.into_values() {
                let (Some(created_at), Some(updated_at)) =
                    (parse_ms(&entry.created_at), parse_ms(&entry.updated_at))
                else {
                    skipped += 1;
                    continue;
                };
                let state = match entry.state.as_str() {
                    STATE_COMPLETED if updated_at >= completed_cutoff => STATE_COMPLETED,
                    STATE_FORWARDED if updated_at >= pending_cutoff => STATE_FORWARDED,
                    STATE_OUTCOME_UNKNOWN if updated_at >= pending_cutoff => STATE_OUTCOME_UNKNOWN,
                    _ => {
                        skipped += 1;
                        continue;
                    }
                };
                let response = entry.response.filter(|_| state == STATE_COMPLETED);
                if state == STATE_COMPLETED && response.is_none() {
                    skipped += 1;
                    continue;
                }
                let (status, reason, body, sha) = match &response {
                    Some(response) => {
                        let raw = response.body.get();
                        (
                            Some(response.status),
                            Some(response.reason.as_str()),
                            (raw.len() <= policy.max_retained_body_bytes).then_some(raw),
                            Some(sha256_hex(raw.as_bytes())),
                        )
                    }
                    None => (None, None, None, None),
                };
                statement.execute(params![
                    entry.cutex_session_id,
                    entry.request_id,
                    entry.body_sha256,
                    state,
                    entry.process_instance_id,
                    created_at,
                    updated_at,
                    status,
                    reason,
                    body,
                    sha
                ])?;
                imported += 1;
            }
        }
        Err(error) => {
            eprintln!(
                "management v2 idempotency: legacy store {} is unreadable ({error}); keeping it as .bak",
                legacy.display()
            );
        }
    }
    transaction.commit()?;
    drop(bytes);
    let mut backup = root.join(format!("{LEGACY_JSON_FILE}.bak"));
    if backup.exists() {
        backup = root.join(format!("{LEGACY_JSON_FILE}.{now}.bak"));
    }
    fs::rename(&legacy, &backup).with_context(|| {
        format!(
            "failed to rename legacy idempotency store to {}",
            backup.display()
        )
    })?;
    eprintln!(
        "management v2 idempotency: imported {imported} recent records from {} ({skipped} expired/skipped); kept as {}",
        legacy.display(),
        backup.display()
    );
    Ok(())
}

fn parse_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Creates `path` with mode 0600 when missing (an existing file is left as is;
/// [`secure_sqlite_files`] tightens it).
fn create_private_file(path: &Path) -> anyhow::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

/// Owner-only database plus any -wal / -shm sidecars (stores created by
/// earlier builds left them 0644).
fn secure_sqlite_files(db_path: &Path) -> anyhow::Result<()> {
    secure_file(db_path)?;
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        if sidecar.exists() {
            secure_file(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_clear_is_namespace_and_session_scoped_and_allows_reexecution() {
        let root = temp_root("browser-clear");
        let repo = RequestIdempotencyRepository::open(&root, "p".into()).unwrap();
        let session_a = "cutex.a";
        let session_b = "cutex.b";
        let scope = |credential: char, session: &str, device: &str| format!(
            "browser:{}:session:{:x}:{device}",
            credential.to_string().repeat(64),
            Sha256::digest(session.as_bytes())
        );
        let a1 = scope('a', session_a, "device-a");
        let a2 = scope('b', session_a, "device-b");
        let b = scope('c', session_b, "device-c");
        for (scope, id) in [(a1.as_str(), "a1"), (a2.as_str(), "a2"), (b.as_str(), "b"), ("ordinary", "ordinary")] {
            let BeginRequest::Forward(claim) = repo.begin(scope, id, &json!({"id":id})).unwrap() else { panic!("fresh") };
            repo.mark_outcome_unknown(&claim).unwrap();
        }
        let current = repo.begin(&a1, "current", &json!({"id":"current"})).unwrap();
        let abandoned = repo.begin(&a1, "abandoned", &json!({"id":"abandoned"})).unwrap();
        repo.with_connection(|connection| {
            connection.execute("UPDATE requests SET process_instance_id='previous-process' WHERE request_id='abandoned'", [])?;
            Ok(())
        }).unwrap();
        let BeginRequest::Forward(completed) = repo.begin(&a1, "completed", &json!({"id":"completed"})).unwrap() else { panic!("fresh") };
        repo.complete(&completed, 200, "OK", json!({"status":"succeeded"})).unwrap();
        assert_eq!(repo.clear_browser_records(Some(session_a)).unwrap(), BrowserClearCounts { outcome_unknown: 2, abandoned_forwarded: 1 });
        assert!(matches!(repo.begin(&a1, "a1", &json!({"id":"a1"})).unwrap(), BeginRequest::Forward(_)));
        assert!(matches!(repo.begin(&a1, "current", &json!({"id":"current"})).unwrap(), BeginRequest::InProgress));
        assert!(matches!(repo.begin(&a1, "completed", &json!({"id":"completed"})).unwrap(), BeginRequest::Completed(_)));
        assert!(matches!(current, BeginRequest::Forward(_)));
        assert!(matches!(abandoned, BeginRequest::Forward(_)));
        assert_eq!(repo.clear_browser_records(None).unwrap().total(), 1);
        assert!(matches!(repo.begin("ordinary", "ordinary", &json!({"id":"ordinary"})).unwrap(), BeginRequest::OutcomeUnknown));
        drop(repo);
        let _ = fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cutex-v2-idem-{label}-{}", Uuid::new_v4()))
    }

    #[cfg(unix)]
    #[test]
    fn database_and_sidecars_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        fs::create_dir_all(&root).unwrap();
        let old_umask = unsafe { libc::umask(0o022) };
        let repository = RequestIdempotencyRepository::open(&root, "p".into()).unwrap();
        unsafe { libc::umask(old_umask) };
        let claim = forward(&repository, "r-perm");
        repository
            .complete(&claim, 200, "OK", json!({ "ok": true }))
            .unwrap();
        for name in ["", "-wal", "-shm"] {
            let path = root.join(format!("{IDEMPOTENCY_DB_FILE}{name}"));
            if path.exists() {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{}", path.display());
            }
        }
        assert!(root.join(format!("{IDEMPOTENCY_DB_FILE}-wal")).exists());
        drop(repository);
        // Reopening tightens sidecars an older build left readable.
        let wal = root.join(format!("{IDEMPOTENCY_DB_FILE}-wal"));
        if wal.exists() {
            fs::set_permissions(&wal, fs::Permissions::from_mode(0o644)).unwrap();
            let _again = RequestIdempotencyRepository::open(&root, "p".into()).unwrap();
            assert_eq!(
                fs::metadata(&wal).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    fn forward(repository: &RequestIdempotencyRepository, request_id: &str) -> RequestClaim {
        match repository
            .begin("session-1", request_id, &json!({ "requestId": request_id }))
            .expect("begin")
        {
            BeginRequest::Forward(claim) => claim,
            other => panic!("expected claim, got {other:?}"),
        }
    }

    #[test]
    fn read_only_methods_are_not_recorded() {
        let root = temp_root("readonly");
        let repository = RequestIdempotencyRepository::open(&root, "p".into()).expect("open");
        let body = json!({ "requestId": "r-1" });
        for _ in 0..2 {
            let BeginRequest::Forward(claim) = repository
                .begin_for_method("thread/read", "session-1", "r-1", &body)
                .expect("begin read")
            else {
                panic!("read must always forward");
            };
            repository
                .complete(&claim, 200, "OK", json!({ "big": "x".repeat(1000) }))
                .expect("complete is a no-op");
        }
        assert_eq!(repository.record_count(), 0);
        for method in ["claude/models/catalog", "claude/model/status"] {
            let BeginRequest::Forward(claim) = repository
                .begin_for_method(method, "session-1", "model-read", &body)
                .unwrap()
            else {
                panic!("Claude metadata reads must always forward");
            };
            repository
                .complete(&claim, 200, "OK", json!({"models": []}))
                .unwrap();
        }
        assert_eq!(repository.record_count(), 0);
        assert!(matches!(
            repository
                .begin_for_method("turn/start", "session-1", "w-1", &body)
                .expect("begin write"),
            BeginRequest::Forward(RequestClaim { durable: true, .. })
        ));
        assert_eq!(repository.record_count(), 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn large_bodies_are_hashed_not_retained() {
        let root = temp_root("large");
        let policy = RetentionPolicy {
            max_retained_body_bytes: 100,
            ..RetentionPolicy::default()
        };
        let repository = RequestIdempotencyRepository::open_with_policy(&root, "p".into(), policy)
            .expect("open");
        let claim = forward(&repository, "small");
        repository
            .complete(&claim, 200, "OK", json!({ "ok": true }))
            .expect("complete");
        assert!(matches!(
            repository.begin("session-1", "small", &json!({ "requestId": "small" })).unwrap(),
            BeginRequest::Completed(response) if response.status == 200 && response.body["ok"] == true
        ));
        let claim = forward(&repository, "large");
        repository
            .complete(&claim, 200, "OK", json!({ "data": "x".repeat(500) }))
            .expect("complete");
        let BeginRequest::Completed(replay) = repository
            .begin("session-1", "large", &json!({ "requestId": "large" }))
            .unwrap()
        else {
            panic!("completed");
        };
        assert_eq!(replay.status, 409);
        assert_eq!(replay.body["error"]["code"], "request_result_not_retained");
        assert_eq!(replay.body["error"]["details"]["originalStatus"], 200);
        assert_eq!(replay.body["requestId"], "large");
        fs::remove_dir_all(root).ok();
    }

    fn backdate(repository: &RequestIdempotencyRepository, request_id: &str, ago_secs: i64) {
        repository
            .with_connection(|connection| {
                Ok(connection.execute(
                    "UPDATE requests SET updated_at = ?2 WHERE request_id = ?1",
                    params![request_id, now_ms() - ago_secs * 1000],
                )?)
            })
            .unwrap();
    }

    #[test]
    fn browser_rows_have_separate_capacity_and_strip_tiny_images() {
        let root = temp_root("browser-retention");
        let repo = RequestIdempotencyRepository::open_with_policy(
            &root,
            "p".into(),
            RetentionPolicy {
                max_records: 2,
                completed_ttl: Duration::from_secs(1),
                pending_ttl: Duration::from_secs(1),
                unresolved_min_retention: Duration::from_secs(1),
                ..Default::default()
            },
        )
        .unwrap();
        let BeginRequest::Forward(done) = repo
            .begin("browser:scope", "done", &json!({"op":"screenshot"}))
            .unwrap()
        else {
            panic!()
        };
        repo.complete(
            &done,
            200,
            "OK",
            json!({"status":"succeeded","image":"tiny-secret","snapshot":"page-secret"}),
        )
        .unwrap();
        let BeginRequest::Forward(lost) = repo
            .begin("browser:scope", "lost", &json!({"op":"click"}))
            .unwrap()
        else {
            panic!()
        };
        repo.mark_outcome_unknown(&lost).unwrap();
        backdate(&repo, "done", 9 * 60);
        backdate(&repo, "lost", 6 * 24 * 60 * 60);
        repo.prune_now().unwrap();
        assert!(matches!(
            repo.begin("other-domain", "new", &json!({})).unwrap(),
            BeginRequest::Forward(_)
        ));
        assert!(matches!(
            repo.begin("browser:other", "new", &json!({})).unwrap(),
            BeginRequest::CapacityExceeded(_)
        ));
        let BeginRequest::Completed(response) = repo
            .begin("browser:scope", "done", &json!({"op":"screenshot"}))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            response.body,
            json!({"requestId":"done","status":"succeeded","resultRetained":false})
        );
        assert_eq!(
            repo.begin("browser:scope", "lost", &json!({"op":"click"}))
                .unwrap(),
            BeginRequest::OutcomeUnknown
        );
        backdate(&repo, "done", 11 * 60);
        assert!(matches!(
            repo.begin("browser:other", "new", &json!({})).unwrap(),
            BeginRequest::Forward(_)
        ));
        assert!(!rows(&repo).iter().any(|(id, _)| id == "done"));
        assert!(rows(&repo).contains(&row("lost", "outcome_unknown")));
        assert!(matches!(
            repo.begin("other-domain", "another", &json!({})).unwrap(),
            BeginRequest::Forward(_)
        ));
        for n in 0..100 {
            let BeginRequest::Forward(read) = repo
                .begin_for_method(
                    "thread/read",
                    "browser:reads",
                    &format!("read-{n}"),
                    &json!({}),
                )
                .unwrap()
            else {
                panic!()
            };
            repo.complete(&read, 200, "OK", json!({})).unwrap();
        }
        assert!(matches!(
            repo.begin("other-domain", "overflow", &json!({})).unwrap(),
            BeginRequest::CapacityExceeded(_)
        ));
        backdate(&repo, "lost", 8 * 24 * 60 * 60);
        repo.prune_now().unwrap();
        assert!(matches!(
            repo.begin("browser:scope", "lost", &json!({"op":"click"}))
                .unwrap(),
            BeginRequest::Forward(_)
        ));
        drop(repo);
        std::fs::remove_dir_all(root).ok();
    }
    #[test]
    fn browser_quota_is_per_session_across_devices_and_credentials_with_honest_retry() {
        let root = temp_root("browser-session-cap");
        let repo = RequestIdempotencyRepository::open(&root, "p".into()).unwrap();
        let session = "c".repeat(64);
        let first = format!("browser:{}:session:{session}:device-a", "a".repeat(64));
        let rotated = format!("browser:{}:session:{session}:device-b", "b".repeat(64));
        let other = format!("browser:{}:session:{}:device-a", "a".repeat(64), "d".repeat(64));
        repo.with_connection(|connection| {
            let transaction = connection.transaction()?;
            for n in 0..MAX_BROWSER_SESSION_RECORDS {
                transaction.execute(
                    "INSERT INTO requests(cutex_session_id,request_id,body_sha256,state,process_instance_id,created_at,updated_at)
                     VALUES(?1,?2,'fixture',?3,'p',?4,?4)",
                    params![first, format!("fixture-{n}"), if n == 0 { STATE_OUTCOME_UNKNOWN } else { STATE_COMPLETED }, now_ms()],
                )?;
            }
            transaction.commit()?;
            Ok(())
        }).unwrap();
        backdate(&repo, "fixture-0", 6 * 24 * 60 * 60);
        let BeginRequest::CapacityExceeded(full) = repo.begin(&rotated, "new", &json!({})).unwrap() else { panic!("session quota"); };
        assert_eq!(full.max_records, 256);
        assert_eq!(full.unresolved_records, 1);
        assert_eq!(full.in_flight_records, 0);
        assert!((9 * 60 * 1000..=10 * 60 * 1000).contains(&full.retry_after_ms));
        assert!(matches!(repo.begin(&other, "new", &json!({})).unwrap(), BeginRequest::Forward(_)));
        assert!(matches!(repo.begin("ordinary-domain", "new", &json!({})).unwrap(), BeginRequest::Forward(_)));
        // A young completed result is protected for ten minutes. Only this
        // session's old completed row can clear its own cap, not another's.
        repo.with_connection(|connection| {
            connection.execute("UPDATE requests SET state='completed' WHERE request_id='fixture-1'", [])?;
            Ok(())
        }).unwrap();
        backdate(&repo, "fixture-1", 9 * 60);
        let BeginRequest::CapacityExceeded(full) = repo.begin(&rotated, "new", &json!({})).unwrap() else { panic!("grace period"); };
        assert_eq!(full.unresolved_records, 1);
        assert_eq!(full.in_flight_records, 0);
        assert!((55_000..=60_000).contains(&full.retry_after_ms));
        backdate(&repo, "fixture-1", 11 * 60);
        assert!(matches!(repo.begin(&rotated, "new", &json!({})).unwrap(), BeginRequest::Forward(_)));
        assert!(rows(&repo).contains(&row("fixture-0", "outcome_unknown")));
        assert!(rows(&repo).contains(&row("fixture-2", "completed")));
        assert!(!rows(&repo).iter().any(|(id, _)| id == "fixture-1"));
        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn browser_unresolved_budget_is_reserved_before_dispatch_and_recovers_on_completion() {
        let root = temp_root("browser-unresolved-cap");
        let repo = Arc::new(RequestIdempotencyRepository::open(&root, "p".into()).unwrap());
        let scope = format!("browser:{}:session:{}:device-a", "a".repeat(64), "c".repeat(64));
        let mut workers = Vec::new();
        for n in 0..32 {
            let repo = repo.clone();
            let scope = scope.clone();
            workers.push(std::thread::spawn(move || repo.begin(&scope, &format!("write-{n}"), &json!({})).unwrap()));
        }
        let mut claims = Vec::new();
        let mut refused = 0;
        for worker in workers {
            match worker.join().unwrap() {
                BeginRequest::Forward(claim) => claims.push(claim),
                BeginRequest::CapacityExceeded(full) => {
                    assert_eq!(full.max_records, MAX_BROWSER_SESSION_UNRESOLVED);
                    assert_eq!(full.unresolved_records, 16);
                    assert_eq!(full.in_flight_records, 16);
                    refused += 1;
                }
                other => panic!("unexpected admission: {other:?}"),
            }
        }
        assert_eq!(claims.len(), 16);
        assert_eq!(refused, 16);
        let completed = claims.pop().unwrap();
        for claim in &claims { repo.mark_outcome_unknown(claim).unwrap(); }
        repo.complete(&completed, 200, "OK", json!({"status":"succeeded"})).unwrap();
        let rotated = format!("browser:{}:session:{}:device-b", "b".repeat(64), "c".repeat(64));
        let BeginRequest::Forward(last) = repo.begin(&rotated, "replacement", &json!({})).unwrap() else { panic!("completed frees unresolved slot"); };
        repo.mark_outcome_unknown(&last).unwrap();
        let BeginRequest::CapacityExceeded(full) = repo.begin(&rotated, "blocked", &json!({})).unwrap() else { panic!("unknown cap"); };
        assert_eq!(full.unresolved_records, 16);
        assert_eq!(full.in_flight_records, 0);
        assert!((BROWSER_UNRESOLVED_TTL_MS as u64 - 60_000..=BROWSER_UNRESOLVED_TTL_MS as u64).contains(&full.retry_after_ms));
        assert!(matches!(repo.begin(&scope, &claims[0].request_id, &json!({})).unwrap(), BeginRequest::OutcomeUnknown));
        backdate(&repo, &last.request_id, 8 * 24 * 60 * 60);
        assert!(matches!(repo.begin(&rotated, "blocked", &json!({})).unwrap(), BeginRequest::Forward(_)));
        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn browser_existing_unknown_rows_above_new_limit_are_preserved() {
        let root = temp_root("browser-existing-unknown-cap");
        let repo = RequestIdempotencyRepository::open(&root, "p".into()).unwrap();
        let scope = format!("browser:{}:session:{}:device", "a".repeat(64), "c".repeat(64));
        repo.with_connection(|connection| {
            let transaction = connection.transaction()?;
            for n in 0..MAX_BROWSER_SESSION_RECORDS {
                transaction.execute(
                    "INSERT INTO requests(cutex_session_id,request_id,body_sha256,state,process_instance_id,created_at,updated_at)
                     VALUES(?1,?2,'fixture','outcome_unknown','old-process',?3,?3)",
                    params![scope, format!("old-{n}"), now_ms()],
                )?;
            }
            transaction.commit()?;
            Ok(())
        }).unwrap();
        let BeginRequest::CapacityExceeded(full) = repo.begin(&scope, "new", &json!({})).unwrap() else { panic!("existing unknown cap"); };
        assert_eq!(full.max_records, MAX_BROWSER_SESSION_UNRESOLVED);
        assert_eq!(full.unresolved_records, MAX_BROWSER_SESSION_RECORDS as u64);
        assert_eq!(rows(&repo).len(), MAX_BROWSER_SESSION_RECORDS);
        assert!(rows(&repo).iter().all(|(_, state)| state == STATE_OUTCOME_UNKNOWN));
        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn browser_eight_unknown_sessions_leave_capacity_for_other_sessions() {
        let root = temp_root("browser-eight-unknown-sessions");
        let repo = RequestIdempotencyRepository::open(&root, "p".into()).unwrap();
        for session in 0..8 {
            let scope = format!("browser:{}:session:{session:064x}:device", "a".repeat(64));
            for n in 0..MAX_BROWSER_SESSION_UNRESOLVED {
                let BeginRequest::Forward(claim) = repo.begin(&scope, &format!("write-{n}"), &json!({})).unwrap() else { panic!("session slot"); };
                repo.mark_outcome_unknown(&claim).unwrap();
            }
            assert!(matches!(repo.begin(&scope, "blocked", &json!({})).unwrap(), BeginRequest::CapacityExceeded(_)));
        }
        let fresh = format!("browser:{}:session:{:064x}:device", "a".repeat(64), 8);
        assert!(matches!(repo.begin(&fresh, "new", &json!({})).unwrap(), BeginRequest::Forward(_)));
        assert_eq!(rows(&repo).len(), 129);
        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn browser_late_settlement_cannot_delete_or_complete_replacement_reservation() {
        let root = temp_root("browser-settlement");
        let repo = RequestIdempotencyRepository::open(&root, "p".into()).unwrap();
        let BeginRequest::Forward(old) = repo.begin("browser:scope", "same", &json!({})).unwrap() else { panic!(); };
        let duplicate = old.clone();
        repo.release_browser_refusal(&old).unwrap();
        let BeginRequest::Forward(new) = repo.begin("browser:scope", "same", &json!({})).unwrap() else { panic!(); };
        repo.release_browser_refusal(&duplicate).unwrap();
        repo.complete(&duplicate, 400, "old", json!({"status":"failed"})).unwrap();
        repo.mark_outcome_unknown(&duplicate).unwrap();
        assert_eq!(repo.begin("browser:scope", "same", &json!({})).unwrap(), BeginRequest::InProgress);
        repo.mark_outcome_unknown(&new).unwrap();
        repo.release_browser_refusal(&new).unwrap();
        assert_eq!(repo.begin("browser:scope", "same", &json!({})).unwrap(), BeginRequest::OutcomeUnknown);
        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn browser_lookup_is_scoped_and_retains_only_safe_approval_metadata() {
        let root = temp_root("browser-lookup");
        let repo = RequestIdempotencyRepository::open_with_policy(
            &root,
            "p".into(),
            RetentionPolicy::default(),
        )
        .unwrap();
        let prefix = format!("browser:{}:", "a".repeat(64));
        let scope = format!("{prefix}device");
        let BeginRequest::Forward(claim) = repo
            .begin(&scope, "r", &json!({"tool":"browser_claim"}))
            .unwrap()
        else {
            panic!("new claim")
        };
        repo.complete(&claim,400,"refused",json!({"status":"failed","error":{"code":"approval_required","raw":"secret"},"approvalId":"apr-test","title":"secret","image":"secret"})).unwrap();
        let rows = repo.browser_lookup(&prefix, "r").unwrap();
        assert_eq!(rows.len(), 1);
        let meta = rows[0].metadata.as_ref().unwrap();
        assert_eq!(meta["error"]["code"], "approval_required");
        assert_eq!(meta["approvalId"], "apr-test");
        assert!(!meta.to_string().contains("secret"));
        assert!(repo
            .browser_lookup(&format!("browser:{}:", "b".repeat(64)), "r")
            .unwrap()
            .is_empty());
        assert!(repo.browser_lookup("browser:%:", "r").is_err());
        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `(requestId, state)` of every row, by requestId.
    fn rows(repository: &RequestIdempotencyRepository) -> Vec<(String, String)> {
        repository
            .with_connection(|connection| {
                let mut statement = connection
                    .prepare("SELECT request_id, state FROM requests ORDER BY request_id")?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<(String, String)>, _>>()?;
                Ok(rows)
            })
            .unwrap()
    }

    fn row(id: &str, state: &str) -> (String, String) {
        (id.to_string(), state.to_string())
    }

    fn begin(repository: &RequestIdempotencyRepository, request_id: &str) -> BeginRequest {
        repository
            .begin("session-1", request_id, &json!({ "requestId": request_id }))
            .expect("begin")
    }

    #[test]
    fn prune_expires_by_state_and_caps_only_completed_records() {
        let root = temp_root("prune");
        let policy = RetentionPolicy {
            completed_ttl: Duration::from_secs(60),
            pending_ttl: Duration::from_secs(600),
            max_records: 10,
            ..RetentionPolicy::default()
        };
        let repository = RequestIdempotencyRepository::open_with_policy(&root, "p".into(), policy)
            .expect("open");
        let done = forward(&repository, "done");
        repository.complete(&done, 200, "OK", json!({})).unwrap();
        forward(&repository, "pending");
        backdate(&repository, "done", 120);
        backdate(&repository, "pending", 120);
        repository.prune_now().unwrap();
        assert_eq!(
            repository.record_count(),
            1,
            "completed expired, pending kept"
        );
        backdate(&repository, "pending", 700);
        repository.prune_now().unwrap();
        assert_eq!(repository.record_count(), 0);

        // Older in-flight rows than any completed one, then a lower cap (or an
        // import) leaves the store over it: only completed rows go, oldest first.
        for index in 0..3 {
            let claim = forward(&repository, &format!("done-{index}"));
            repository
                .complete(&claim, 200, "OK", json!({ "i": index }))
                .unwrap();
            backdate(&repository, &format!("done-{index}"), 30 - index);
        }
        let live = forward(&repository, "live");
        backdate(&repository, "live", 50);
        drop(repository);
        let repository = RequestIdempotencyRepository::open_with_policy(
            &root,
            "p".into(),
            RetentionPolicy {
                max_records: 2,
                ..policy
            },
        )
        .expect("reopen with a lower cap");
        assert_eq!(
            rows(&repository),
            vec![row("done-2", "completed"), row("live", "forwarded")],
            "the two oldest completed rows were evicted, the older in-flight row kept"
        );
        // A lone unresolved row over the cap is never dropped by the cap.
        drop(repository);
        let repository = RequestIdempotencyRepository::open_with_policy(
            &root,
            "p".into(),
            RetentionPolicy {
                max_records: 0,
                ..policy
            },
        )
        .expect("reopen with no room");
        assert_eq!(rows(&repository), vec![row("live", "forwarded")]);
        repository
            .complete(&live, 200, "OK", json!({ "late": true }))
            .unwrap();
        assert!(matches!(
            begin(&repository, "live"),
            BeginRequest::Completed(response) if response.body["late"] == true
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn admission_evicts_completed_records_and_refuses_when_only_in_flight_remain() {
        let root = temp_root("admission");
        let policy = RetentionPolicy {
            max_records: 3,
            ..RetentionPolicy::default()
        };
        let repository = RequestIdempotencyRepository::open_with_policy(&root, "p".into(), policy)
            .expect("open");
        for (id, ago) in [("done-a", 30), ("done-b", 20)] {
            let claim = forward(&repository, id);
            repository
                .complete(&claim, 200, "OK", json!({ "id": id }))
                .unwrap();
            backdate(&repository, id, ago);
        }
        let live = forward(&repository, "live");
        backdate(&repository, "live", 100);

        // Each new request evicts the oldest completed row, never the older
        // in-flight one.
        forward(&repository, "new-1");
        assert_eq!(
            rows(&repository),
            vec![
                row("done-b", "completed"),
                row("live", "forwarded"),
                row("new-1", "forwarded")
            ]
        );
        forward(&repository, "new-2");
        assert_eq!(
            rows(&repository),
            vec![
                row("live", "forwarded"),
                row("new-1", "forwarded"),
                row("new-2", "forwarded")
            ]
        );
        // Full of this process's in-flight requests: refused, nothing recorded.
        let BeginRequest::CapacityExceeded(full) = begin(&repository, "new-3") else {
            panic!("a full store of in-flight requests must refuse");
        };
        assert_eq!(
            full,
            CapacityExceeded {
                max_records: 3,
                unresolved_records: 3,
                in_flight_records: 3,
                retry_after_ms: 1_000,
            }
        );
        assert_eq!(full.details()["retryAfterMs"], 1_000);
        assert_eq!(repository.record_count(), 3);
        // Dedupe of the in-flight requests is intact.
        assert_eq!(begin(&repository, "live"), BeginRequest::InProgress);
        repository
            .complete(&live, 200, "OK", json!({ "done": true }))
            .unwrap();
        assert!(matches!(
            begin(&repository, "live"),
            BeginRequest::Completed(response) if response.body["done"] == true
        ));
        // A completed row is room again; the refused request was not recorded.
        assert!(matches!(
            begin(&repository, "new-3"),
            BeginRequest::Forward(_)
        ));
        assert_eq!(
            rows(&repository),
            vec![
                row("new-1", "forwarded"),
                row("new-2", "forwarded"),
                row("new-3", "forwarded")
            ]
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stale_unresolved_records_yield_only_after_their_minimum_retention() {
        let root = temp_root("stale");
        let policy = RetentionPolicy {
            max_records: 2,
            unresolved_min_retention: Duration::from_secs(600),
            ..RetentionPolicy::default()
        };
        // A process forwarded two requests and died before they finished.
        let dead = RequestIdempotencyRepository::open_with_policy(&root, "dead".into(), policy)
            .expect("open");
        forward(&dead, "lost-old");
        forward(&dead, "lost-young");
        backdate(&dead, "lost-old", 700);
        backdate(&dead, "lost-young", 100);
        drop(dead);

        let repository = RequestIdempotencyRepository::open_with_policy(&root, "p".into(), policy)
            .expect("reopen");
        // Past its minimum retention, the oldest unknown outcome makes room.
        assert!(matches!(
            begin(&repository, "fresh-1"),
            BeginRequest::Forward(_)
        ));
        assert_eq!(
            rows(&repository),
            vec![row("fresh-1", "forwarded"), row("lost-young", "forwarded")]
        );
        // The younger one still protects a retry: refused, with the wait until
        // it may yield.
        let BeginRequest::CapacityExceeded(full) = begin(&repository, "fresh-2") else {
            panic!("a young unknown outcome must not be evicted");
        };
        assert_eq!((full.unresolved_records, full.in_flight_records), (2, 1));
        assert!(
            (400_000..=500_000).contains(&full.retry_after_ms),
            "about 500 s left of 600 s: {}",
            full.retry_after_ms
        );
        assert_eq!(
            begin(&repository, "lost-young"),
            BeginRequest::OutcomeUnknown
        );
        assert_eq!(
            rows(&repository),
            vec![
                row("fresh-1", "forwarded"),
                row("lost-young", "outcome_unknown")
            ]
        );
        // Once it is old enough it yields too; this process's request never does.
        backdate(&repository, "lost-young", 700);
        backdate(&repository, "fresh-1", 7_000);
        assert!(matches!(
            begin(&repository, "fresh-2"),
            BeginRequest::Forward(_)
        ));
        assert_eq!(
            rows(&repository),
            vec![row("fresh-1", "forwarded"), row("fresh-2", "forwarded")]
        );
        assert_eq!(begin(&repository, "fresh-1"), BeginRequest::InProgress);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn legacy_json_is_imported_selectively_and_kept_as_backup() {
        let root = temp_root("legacy");
        fs::create_dir_all(&root).unwrap();
        let now = Utc::now();
        let recent = now.to_rfc3339();
        let old = (now - chrono::Duration::days(30)).to_rfc3339();
        let body_sha = |id: &str| canonical_request_sha256(&json!({ "requestId": id })).unwrap();
        let entry = |id: &str, state: &str, at: &str, response: Option<Value>| {
            let mut value = json!({
                "cutexSessionId": "session-1",
                "requestId": id,
                "bodySha256": body_sha(id),
                "state": state,
                "processInstanceId": "old-process",
                "createdAt": at,
                "updatedAt": at,
            });
            if let Some(response) = response {
                value["response"] = response;
            }
            (serde_json::to_string(&("session-1", id)).unwrap(), value)
        };
        let entries: serde_json::Map<String, Value> = [
            entry("recent-done", "completed", &recent, Some(json!({ "status": 200, "reason": "OK", "body": { "ok": 1 } }))),
            entry("recent-huge", "completed", &recent, Some(json!({ "status": 200, "reason": "OK", "body": { "data": "y".repeat(70_000) } }))),
            entry("old-done", "completed", &old, Some(json!({ "status": 200, "reason": "OK", "body": {} }))),
            entry("inflight", "forwarded", &recent, None),
            entry("old-inflight", "forwarded", &old, None),
        ]
        .into_iter()
        .collect();
        fs::write(
            root.join(LEGACY_JSON_FILE),
            serde_json::to_vec_pretty(&json!({ "version": 1, "entries": entries })).unwrap(),
        )
        .unwrap();

        let repository = RequestIdempotencyRepository::open(&root, "new".into()).expect("open");
        assert!(!root.join(LEGACY_JSON_FILE).exists());
        assert!(root.join(format!("{LEGACY_JSON_FILE}.bak")).exists());
        assert_eq!(repository.record_count(), 3);
        let begin = |id: &str| {
            repository
                .begin("session-1", id, &json!({ "requestId": id }))
                .unwrap()
        };
        assert!(matches!(begin("recent-done"), BeginRequest::Completed(r) if r.body["ok"] == 1));
        assert!(matches!(begin("recent-huge"), BeginRequest::Completed(r) if r.status == 409));
        assert_eq!(begin("inflight"), BeginRequest::OutcomeUnknown);
        assert!(matches!(begin("old-done"), BeginRequest::Forward(_)));
        drop(repository);
        // Reopening is a no-op (nothing left to import).
        let reopened = RequestIdempotencyRepository::open(&root, "new-2".into()).expect("reopen");
        assert_eq!(reopened.record_count(), 4);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn concurrent_sessions_do_not_lose_records() {
        let root = temp_root("concurrent");
        let repository = std::sync::Arc::new(
            RequestIdempotencyRepository::open(&root, "p".into()).expect("open"),
        );
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let repository = repository.clone();
                std::thread::spawn(move || {
                    for index in 0..25 {
                        let session = format!("session-{thread}");
                        let request_id = format!("r-{index}");
                        let body = json!({ "requestId": request_id });
                        let BeginRequest::Forward(claim) =
                            repository.begin(&session, &request_id, &body).unwrap()
                        else {
                            panic!("fresh claim");
                        };
                        repository
                            .complete(&claim, 200, "OK", json!({ "i": index }))
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(repository.record_count(), 200);
        // A second repository (another process) sees the same rows.
        let other = RequestIdempotencyRepository::open(&root, "q".into()).expect("open other");
        assert!(matches!(
            other.begin("session-3", "r-7", &json!({ "requestId": "r-7" })).unwrap(),
            BeginRequest::Completed(r) if r.body["i"] == 7
        ));
        drop(other);
        drop(repository);
        fs::remove_dir_all(root).ok();
    }

    /// The same requestId sent concurrently (from this process and from a
    /// second repository standing in for another process): the reservation
    /// is one IMMEDIATE transaction, so exactly one caller forwards; the
    /// others see it in progress / outcome unknown, never a second Forward.
    #[test]
    fn concurrent_same_request_id_is_forwarded_once() {
        let root = temp_root("same-id");
        let here = RequestIdempotencyRepository::open(&root, "p".into()).expect("open");
        let there = RequestIdempotencyRepository::open(&root, "q".into()).expect("open other");
        let barrier = std::sync::Barrier::new(16);
        let body = json!({ "requestId": "same", "method": "cutex/userInput/submit" });
        let outcomes: Vec<BeginRequest> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|index| {
                    let (repository, barrier, body) =
                        (if index % 2 == 0 { &here } else { &there }, &barrier, &body);
                    scope.spawn(move || {
                        barrier.wait();
                        repository.begin("session", "same", body).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let forwards = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, BeginRequest::Forward(_)))
            .count();
        assert_eq!(forwards, 1, "{outcomes:?}");
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            BeginRequest::Forward(_) | BeginRequest::InProgress | BeginRequest::OutcomeUnknown
        )));
        assert_eq!(here.record_count(), 1);
        drop((here, there));
        fs::remove_dir_all(root).ok();
    }
}
