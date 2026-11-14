//! Store manifest (design 058 §2 "新存储的强制入口").
//!
//! Every persistent store cutex writes under its home is registered here:
//! owner files, canonical location, class, transaction domain, key/scope,
//! budgets, growth, rollback class and its access status. Stores already on
//! the layer are `Access::Keyed`; legacy whole-state stores name the 058
//! slice that retires them. Paths are relative to the cutex home.
//!
//! Growth figures are the 2026-11-03 EVA evidence (design §7); `0` means not
//! measured yet. Fixture ids are store ids of the scale gate
//! (`tools/scale_gate/manifest.json`, branch test/scale-gate).

use super::domain::{self, TxDomain};
use super::keyed::{PageBudget, ScopeKind};

#[derive(Clone, Copy, Debug)]
pub enum Canonical {
    Sqlite {
        file: &'static str,
        tables: &'static [&'static str],
    },
    /// Logical store in `management-state.sqlite3` (`hot_store_rows`).
    HotStore { logical: &'static str },
    Json { path: &'static str },
    /// Opaque bytes (for example an encrypted credential envelope or key).
    File { path: &'static str },
    JsonDir { path: &'static str },
    Jsonl { path: &'static str },
    Markdown { path: &'static str },
    Dir { path: &'static str },
    External { owner: &'static str, path: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreClass {
    /// Queried and changed in place: SQLite rows with indexes.
    MutableState,
    /// Append-only evidence/audit/samples: sealed JSONL segments (or paged SQLite history).
    AppendLog,
    /// Small capped document, replaced as a whole on explicit edits.
    SmallConfig,
    /// Rebuildable cache or derived read model.
    Cache,
    /// Blobs addressed by key (media, attachments, artifacts' files).
    Blob,
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rollback {
    R0,
    R1,
    R2,
    R3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// On the layer (or already keyed rows); whole-state ratchet count must be 0.
    Keyed,
    /// Legacy whole-state access, retired by the named slice.
    WholeState { retire_in: &'static str },
    /// Whole-document read/replace allowed under this cap.
    SmallConfig { max_bytes: usize },
    /// Append plus bounded tail/offset reads.
    AppendOnly,
    /// Not cutex code (native runtime / component).
    External,
}

#[derive(Clone, Copy, Debug)]
pub struct Retention {
    pub max_rows: u64,
    pub max_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Growth {
    /// Serialized bytes at 1x (design §7), 0 = not measured.
    pub bytes_1x: u64,
    pub rows_1x: u64,
    /// What it grows with.
    pub with: &'static str,
}

#[derive(Debug)]
pub struct StoreSpec {
    pub id: &'static str,
    /// Repository files allowed to open/write it (repo-relative, `src/...`).
    pub owner: &'static [&'static str],
    pub canonical: Canonical,
    pub class: StoreClass,
    pub domain: &'static TxDomain,
    pub scopes: &'static [ScopeKind],
    pub key: &'static str,
    pub indexes: &'static [&'static str],
    pub page: PageBudget,
    pub max_row_bytes: usize,
    pub change_retention: Retention,
    pub growth: Growth,
    /// Layer-managed schema version; 0 = not on the layer yet.
    pub schema_version: u32,
    pub rollback: Rollback,
    /// Kit exporter to the exact previous build (R1/R3).
    pub previous_exporter: Option<&'static str>,
    pub access: Access,
    /// Scale-fixture area (design §7), built by the scale gate.
    pub fixture: Option<&'static str>,
    /// 058 slice that owns its migration, or "keep".
    pub slice: &'static str,
}

/// Default change-log transport budget (design §9: bounded, separate from history).
pub const DEFAULT_RETENTION: Retention = Retention {
    max_rows: 50_000,
    max_bytes: 8 * 1024 * 1024,
};
pub const DEFAULT_MAX_ROW_BYTES: usize = 1024 * 1024;
/// Stores at or above this 1x size are "hot": they need a fixture area or a slice adding one.
pub const HOT_BYTES: u64 = 64 * 1024;
/// Store list of the end-to-end scale gate, relative to the repo root.
pub const SCALE_GATE_MANIFEST: &str = "tools/scale_gate/manifest.json";

const NO_GROWTH: Growth = Growth {
    bytes_1x: 0,
    rows_1x: 0,
    with: "",
};

const fn base(
    id: &'static str,
    owner: &'static [&'static str],
    canonical: Canonical,
    class: StoreClass,
    domain: &'static TxDomain,
) -> StoreSpec {
    StoreSpec {
        id,
        owner,
        canonical,
        class,
        domain,
        scopes: &[ScopeKind::Host],
        key: "",
        indexes: &[],
        page: PageBudget::LIST_DEFAULT,
        max_row_bytes: DEFAULT_MAX_ROW_BYTES,
        change_retention: DEFAULT_RETENTION,
        growth: NO_GROWTH,
        schema_version: 0,
        rollback: Rollback::R0,
        previous_exporter: None,
        access: Access::Keyed,
        fixture: None,
        slice: "keep",
    }
}

impl StoreSpec {
    const fn key(self, key: &'static str, scopes: &'static [ScopeKind]) -> Self {
        Self { key, scopes, ..self }
    }
    const fn indexes(self, indexes: &'static [&'static str]) -> Self {
        Self { indexes, ..self }
    }
    const fn grows(self, bytes_1x: u64, rows_1x: u64, with: &'static str) -> Self {
        Self {
            growth: Growth {
                bytes_1x,
                rows_1x,
                with,
            },
            ..self
        }
    }
    const fn whole(self, retire_in: &'static str) -> Self {
        Self {
            access: Access::WholeState { retire_in },
            slice: retire_in,
            ..self
        }
    }
    const fn config(self, max_bytes: usize) -> Self {
        Self {
            access: Access::SmallConfig { max_bytes },
            ..self
        }
    }
    const fn append(self) -> Self {
        Self {
            access: Access::AppendOnly,
            ..self
        }
    }
    const fn external(self) -> Self {
        Self {
            access: Access::External,
            ..self
        }
    }
    const fn slice(self, slice: &'static str) -> Self {
        Self { slice, ..self }
    }
    const fn rollback(self, rollback: Rollback, previous_exporter: Option<&'static str>) -> Self {
        Self {
            rollback,
            previous_exporter,
            ..self
        }
    }
    const fn fixture(self, fixture: &'static str) -> Self {
        Self {
            fixture: Some(fixture),
            ..self
        }
    }
    pub(crate) const fn layer(self, schema_version: u32) -> Self {
        Self {
            schema_version,
            ..self
        }
    }

    pub fn is_hot(&self) -> bool {
        matches!(self.class, StoreClass::MutableState | StoreClass::AppendLog)
            && self.growth.bytes_1x >= HOT_BYTES
    }
}

use Canonical::*;
use ScopeKind as K;
use StoreClass::{AppendLog, Blob, Cache, MutableState, SmallConfig};

const SMALL: usize = 256 * 1024;

pub static MANIFEST: &[StoreSpec] = &[
    // --- management-state.sqlite3 (hot_store) ---------------------------------
    base("asks", &["src/asks/store.rs", "src/asks/store_diagnostics.rs", "src/asks/receipt_uncertainty/read_context/observer.rs"],
        HotStore { logical: "asks" }, MutableState, &domain::MANAGEMENT_STATE)
        .key("ask_id", &[K::Host, K::Recipient, K::Session])
        .indexes(&["requester", "target", "deadline"])
        .grows(13_650_000, 0, "asks incl. closed history")
        .fixture("asks").whole("S6"),
    base("asks-archive", &["src/asks/archive.rs"],
        Sqlite { file: "runtime/management-v2/management-state.sqlite3", tables: &["asks_archive_meta", "asks_archive_rows"] },
        AppendLog, &domain::MANAGEMENT_STATE)
        .key("ask_id", &[K::Host]).grows(0, 0, "closed asks moved to cold archive").slice("S6"),
    base("bus-messages", &["src/management/v2/agent_bus_state.rs", "src/management/v2/agent_bus_stuck.rs", "src/cli_app/agent_bus_runtime.rs", "src/management/v2/bus_changes.rs"],
        HotStore { logical: "agent_bus" }, MutableState, &domain::MANAGEMENT_STATE)
        .key("message_id", &[K::Host, K::Recipient])
        .indexes(&["snapshot.state (B6)", "recipient,next_attempt (C1)"])
        .grows(14_739_440, 1_846, "canonical bus messages")
        .fixture("agent_bus").slice("C1").layer(1),
    base("bus-settlement-evidence", &["src/management/v2/agent_bus_settlement_state.rs", "src/management/v2/agent_bus_settlement.rs", "src/management/v2/bus_changes.rs"],
        HotStore { logical: "agent_bus_settlement_evidence" }, MutableState, &domain::MANAGEMENT_STATE)
        .key("evidence_id", &[K::Host]).layer(1),
    base("peers-strip", &["src/peers/strip.rs"],
        HotStore { logical: "asks" }, MutableState, &domain::MANAGEMENT_STATE)
        .key("ask_id", &[K::Host]).whole("S6"),
    // --- own SQLite files -------------------------------------------------------
    base("session-store", &["src/session/store.rs", "src/session/store/sqlite.rs",
            "src/session/store/sqlite/indexed.rs",
            "src/session/store/sqlite/indexed_repository.rs",
            "src/session/store/sqlite/name_ledger.rs",
            "src/session/store/bench.rs",
        ],
        Sqlite { file: "cutex-sessions.sqlite3", tables: &["meta", "sessions", "receipts",
                "session_query_head",
                "session_changes",
                "session_catalog_index",
                "agent_name_events", "agent_name_claims", "agent_name_outbox",
                "store_instance", "store_head", "store_changes", "store_outbox", "store_migrations",
                "store_kv", "store_kv_index",
            ],
        },
        MutableState, &domain::SESSIONS,
    )
        .key("cutex_session_id", &[K::Host, K::Session, K::Project])
        .indexes(&["rev",
        "sessions_id",
        "agent_name_events_session", "agent_name_claims_name", "agent_name_outbox_pending",
        "session_catalog_active",
        "sessions_active_key",
        "sessions_active_id",
        "sessions_lifecycle_key",
        "sessions_lifecycle_page",
        "receipts_map_rev",
        "receipts_prepared_session",
        "session_changes_map",
        "session_changes_revision",
        "session_changes_subject",
        "session_changes_bytes",
    ])
        .grows(
        7_816_490,
        1918,
        "369 sessions + 1549 receipts; 2026-11-04 consistent aggregate metadata",
    )
        .fixture("sessions").slice("S3")
    .layer(1),
    base("input-ledger", &["src/management/v2/input_ledger.rs"],
        Sqlite { file: "runtime/management-v2/input-ledger.sqlite3", tables: &["input_ledger", "input_ledger_meta", "input_ledger_sessions", "input_ledger_payloads", "input_ledger_outbox", "input_ledger_losses"] },
        MutableState, &domain::INPUT_LEDGER)
        .key("input_id", &[K::Session]).grows(0, 0, "owner inputs (054)"),
    // Source authority, not cache: never expire/prune business decisions. The
    // generic change_retention is only the layer change journal, not row TTL.
    // Like input-ledger this is standalone SQLite (schema_version=0 means not
    // StoreDb-managed); input_disposal::configure owns transactional user_version=1.
    // R0 is additive: the preceding r284 writer already uses this exact format.
    // Backup/data-copy roots include runtime/management-v2; keep DB + WAL snapshot.
    base("input-dispositions", &["src/management/v2/input_disposal.rs"],
        Sqlite { file: "runtime/management-v2/input-dispositions.sqlite3", tables: &["owner_dispositions"] },
        MutableState, &domain::INPUT_DISPOSITIONS)
        .key("session,input_id", &[K::Host, K::Session])
        .indexes(&["PRIMARY KEY(session,input_id)", "owner_dispositions_pending(next_check_ms,requested_at) WHERE applied_at IS NULL"])
        .grows(0, 0, "permanent owner decisions; no row TTL or automatic pruning; standalone schema user_version=1"),
    base("request-idempotency", &["src/management/v2/request_idempotency.rs"],
        Sqlite { file: "runtime/management-v2/request-idempotency.sqlite3", tables: &["requests"] },
        MutableState, &domain::REQUEST_IDEMPOTENCY)
        .key("request_id", &[K::Host]).grows(0, 0, "idempotent requests"),
    // Preserve all pan/ metadata across upgrades, including SQLite WAL/SHM
    // and the encrypted credential envelope together with its local key.
    base("pan-downloads", &["src/pan/repository.rs"],
        Sqlite { file: "pan/pan.sqlite", tables: &["pan_downloads"] }, MutableState, &domain::PAN)
        .key("id; unique caller,request_id", &[K::Host, K::Session])
        .indexes(&["id", "caller,request_id"])
        .grows(0, 0, "download tasks; bounded recent lists and keyed control"),
    base("pan-download-log", &["src/pan/repository.rs"],
        Sqlite { file: "pan/pan.sqlite", tables: &["pan_log"] }, AppendLog, &domain::PAN)
        .key("seq", &[K::Host, K::Session]).append()
        .grows(0, 0, "initial task and every state transition; retained audit evidence"),
    base("pan-credentials", &["src/pan/store.rs"],
        File { path: "pan/credentials" }, SmallConfig, &domain::OWN_FILE)
        .config(SMALL),
    base("pan-key", &["src/pan/store.rs"],
        File { path: "pan/key" }, SmallConfig, &domain::OWN_FILE).config(32),
    base("launch-credential-custody", &["src/management/v2/launch_credentials.rs"],
        Json { path: "runtime/credential-custody.json" }, SmallConfig, &domain::OWN_FILE)
        .key("launch source", &[K::Host]).config(SMALL),
    base("name-target-pins", &["src/name_target_pins.rs"],
        Sqlite {file:"runtime/name-target-pins.sqlite3",tables:&["name_target_pins"]},
        MutableState,&domain::NAME_TARGET_PINS)
        .key("caller,tool,action",&[K::Host,K::Session])
        .grows(0,0,"immutable recipient IDs and resolution metadata only; no brief, auth, eviction or expiry"),
    base("integration-dedupe", &["src/management/v2/integration_event_ledger.rs", "src/management/v2/integration_events.rs"],
        Sqlite { file: "runtime/management-v2/integration-event-dedupe-v2.sqlite", tables: &["identities", "metadata", "integration_locations", "integration_lookup_checkpoint"] },
        AppendLog, &domain::INTEGRATION_DEDUPE)
        .key("identity", &[K::Host]).grows(0, 0, "integration event identities"),
    base("pending-stops", &["src/management/v2/native_events.rs"],
        Sqlite { file: "runtime/management-v2/pending-stops.sqlite", tables: &["pending_stop"] },
        MutableState, &domain::PENDING_STOPS)
        .key("session,generation", &[K::Session]),
    base("project-events", &["src/management/v2/project_events.rs"],
        Sqlite { file: "runtime/project-events/journal.sqlite3", tables: &["events", "floors", "meta"] },
        AppendLog, &domain::PROJECT_EVENTS)
        .key("project,seq", &[K::Project]).grows(0, 0, "project events"),
    base("workflow-runs", &["src/workflows/store.rs"],
        Sqlite { file: "runtime/workflows/runs.sqlite3", tables: &["workflow_run", "workflow_step", "workflow_log"] },
        MutableState, &domain::WORKFLOWS)
        .key("run_id", &[K::Project]).grows(0, 0, "workflow runs and logs"),
    base("artifacts", &["src/artifacts/store.rs"],
        Sqlite { file: "runtime/artifacts/artifacts-v1.sqlite3", tables: &["artifacts", "artifact_versions", "actions", "capture_files", "turn_windows", "meta"] },
        AppendLog, &domain::ARTIFACTS)
        .key("artifact_id,version", &[K::Project]).grows(0, 0, "artifact versions"),
    base("documents", &["src/documents/mod.rs", "src/documents/sweep.rs"],
        Sqlite { file: "runtime/documents/documents-v1.sqlite3", tables: &["documents"] },
        MutableState, &domain::DOCUMENTS)
        .key("document_id", &[K::Host]),
    base("archive", &["src/documents/archive.rs"],
        Sqlite { file: "archive/index-v1.sqlite3", tables: &["records"] },
        AppendLog, &domain::ARCHIVE)
        .key("record_id", &[K::Host]).grows(0, 0, "archived records"),
    base("summary", &["src/management/v2/summary.rs"],
        Sqlite { file: "summary/summary.sqlite3", tables: &["agents", "events", "meta"] },
        Cache, &domain::SUMMARY)
        .key("session", &[K::Host, K::Session]).slice("S8"),
    base("notes", &["src/notes/store.rs", "src/notes/mod.rs"],
        Sqlite { file: "notes/notes.sqlite3", tables: &["notes", "note_pins", "note_nonces"] },
        MutableState, &domain::NOTES)
        .key("note_id", &[K::Host]),
    base("owner-pins", &["src/owner_pins/store.rs", "src/owner_pins/mod.rs"],
        Sqlite { file: "runtime/owner-pins/owner-pins.sqlite3", tables: &["pins", "pin_actions"] },
        MutableState, &domain::OWNER_PINS)
        .key("pin_id", &[K::Host]),
    base("notify-state", &["src/notify/state.rs"],
        Sqlite { file: "notifications/states.sqlite3", tables: &["state", "meta"] },
        MutableState, &domain::NOTIFY_STATES)
        .key("thread", &[K::Host]).grows(75_671, 0, "notifications").slice("S10"),
    base("notify-outbound", &["src/notify/outbound.rs"],
        Sqlite { file: "notifications/outbound.sqlite3", tables: &["delivery", "meta"] },
        AppendLog, &domain::NOTIFY_OUTBOUND)
        .key("delivery_id", &[K::Host]),
    base("storage-cache", &["src/management/v2/storage.rs", "src/management/v2/storage_cleanup_state.rs", "src/management/v2/storage_cleanup_catalog.rs", "src/management/v2/storage_cleanup_homes.rs", "src/management/v2/storage_cleanup_vacuum.rs", "src/management/v2/storage_cleanup.rs"],
        Sqlite { file: "runtime/storage/storage.sqlite3", tables: &["walk", "walk_items", "cleanup_previews", "cleanups", "cleanup_items", "cleanup_audit"] },
        Cache, &domain::STORAGE)
        .key("walk,item", &[K::Host]),
    // --- management JSON / JSONL ---------------------------------------------
    base("events", &["src/management/v2/repository.rs"],
        Jsonl { path: "runtime/management-v2/events.jsonl" }, AppendLog, &domain::OWN_FILE)
        .key("event seq", &[K::Host, K::Session]).grows(0, 0, "management events").append(),
    base("user-input", &["src/management/v2/user_input.rs", "src/management/v2/user_input/sql.rs"],
        Sqlite { file: "runtime/management-v2/owner-input.sqlite3", tables: &["owner_sessions", "owner_inputs", "owner_identities"] }, MutableState, &domain::OWNER_INPUT)
        .key("session,input_id", &[K::Session]).indexes(&["queue_order", "delivery", "admission revision"])
        .grows(1_458_863, 0, "owner input queue and identities")
        .fixture("user_input").layer(1).slice("S5").rollback(Rollback::R1, Some("scripts/owner-input-rollback.py (r275 9cf4cbab)")),
    base("session-activity", &["src/management/v2/activity.rs", "src/management/v2/session_row_store.rs"],
        Sqlite { file: "runtime/management-v2/session-activity.sqlite3", tables: &["session_rows", "session_rows_ready", "turn_error_notices"] }, MutableState, &domain::SESSION_ACTIVITY)
        .key("cutex_session_id", &[K::Session]).grows(442_230, 0, "sessions")
        .fixture("sessions").layer(1).slice("S7").rollback(Rollback::R1, Some("tools/session_rows_r1.py (9cf4cbab v1)")),
    base("memory-idle-note", &["src/memory/idle_note.rs", "src/memory/idle_note/transport.rs"],
        Sqlite { file: "runtime/management-v2/session-activity.sqlite3", tables: &["store_kv"] }, MutableState, &domain::SESSION_ACTIVITY)
        .key("session,seen|activity", &[K::Session]).layer(1)
        .grows(0, 0, "one overwritten seen/launch/pending index snapshot per session; bounded index reads; no history"),
    base("session-projection", &["src/management/v2/session.rs", "src/management/v2/session_row_store.rs"],
        Sqlite { file: "runtime/management-v2/session-projection.sqlite3", tables: &["session_rows", "session_rows_ready"] }, MutableState, &domain::SESSION_PROJECTION)
        .key("cutex_session_id", &[K::Session]).grows(256_327, 0, "sessions")
        .fixture("sessions").layer(1).slice("S7").rollback(Rollback::R1, Some("tools/session_rows_r1.py (9cf4cbab v1)")),
    base("session-usage", &["src/management/v2/usage/store.rs"],
        Json { path: "runtime/management-v2/session-usage-state.json" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Session]).grows(0, 0, "usage counters and pending outbox").whole("S9"),
    base("usage-ledger", &["src/management/v2/usage/store.rs"],
        Jsonl { path: "runtime/management-v2/usage-ledger/YYYY-MM.jsonl" }, AppendLog, &domain::OWN_FILE)
        .key("entry_id", &[K::Host, K::Session]).indexes(&["scope,source,profile,time (S1 usage domain)"])
        .grows(82_992_850, 0, "native usage samples per month").fixture("usage").append().slice("S1"),
    base("usage-ledger-external", &["src/management/v2/usage/store.rs", "src/management/v2/usage/external.rs", "src/management/v2/usage/external_zcode.rs"],
        Jsonl { path: "runtime/management-v2/usage-ledger-external" }, AppendLog, &domain::OWN_FILE)
        .key("entry_id", &[K::Host]).grows(99_861_683, 0, "external usage samples").fixture("usage").append().slice("S1"),
    // S1: derived read model of the two ledgers above (JSONL stays canonical; R0: the previous build ignores it).
    base("usage-index", &["src/management/v2/usage/index.rs"],
        Sqlite { file: "runtime/management-v2/usage-index.sqlite3", tables: &["usage_index_meta", "usage_index_files", "usage_index_entries", "usage_index_duplicates", "usage_index_warnings", "usage_index_native_threads"] },
        Cache, &domain::USAGE)
        .key("entry_id", &[K::Host]).indexes(&["observed_ms", "profile_id,observed_ms", "file_id"])
        .grows(0, 0, "one row per ledger entry; rebuilt from the ledger when missing").slice("S1").layer(1),
    base("usage-source-checkpoints", &["src/management/v2/usage/external_checkpoints.rs", "src/management/v2/usage/external_discovery.rs"],
        Sqlite { file:"runtime/management-v2/usage-collector-checkpoints.sqlite3", tables:&["collector_header","collector_files"] },
        Cache, &domain::USAGE_COLLECTORS).key("source_id,relative_file", &[K::Host]).indexes(&["pending"])
        .grows(0,0,"rebuildable collector offsets; canonical entry IDs remain JSONL").slice("S9").layer(1),
    base("quota-history", &["src/management/v2/usage/quota_history.rs"],
        Jsonl { path: "runtime/management-v2/usage-quota-history" }, AppendLog, &domain::OWN_FILE)
        .key("profile,time", &[K::Host]).grows(0, 0, "quota gauge samples").whole("S9"),
    base("usage-prices", &["src/management/v2/usage/prices.rs"],
        Json { path: "usage-prices-v2.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("usage-pricing-catalog", &["src/management/v2/usage/catalog.rs"],
        JsonDir { path: "runtime/usage-pricing" }, Cache, &domain::OWN_FILE).config(4 * 1024 * 1024),
    base("claude-token-usage", &["src/management/v2/claude_token_usage.rs", "src/management/v2/claude_plan.rs"],
        JsonDir { path: "runtime/management-v2" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Session]).config(SMALL),
    base("activity-projector", &["src/app_server/activity_bridge.rs"],
        Json { path: "runtime/management-v2/activity-projector-state.json" }, MutableState, &domain::OWN_FILE)
        .key("session cursor", &[K::Session]).whole("B2 (mgmt-incident)"),
    base("busy-wake-capabilities", &["src/agent_bus/busy_delivery.rs"],
        JsonDir { path: "runtime/busy-wake-capabilities" }, Cache, &domain::OWN_FILE)
        .key("session,generation", &[K::Session]).config(SMALL),
    base("event-owners", &["src/app_server/single_writer.rs"],
        Json { path: "runtime/management-v2/event-owners.json" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Session]).config(SMALL),
    base("server-requests", &["src/management/v2/server_requests.rs"],
        Json { path: "runtime/management-v2/pending-server-requests.json" }, MutableState, &domain::OWN_FILE)
        .key("session,generation,request_id", &[K::Session]).whole("S14").rollback(Rollback::R1, Some("S14 exporter")),
    base("owner-held", &["src/management/v2/owner_held.rs"],
        Json { path: "runtime/management-v2/owner-held-inputs.json" }, MutableState, &domain::OWN_FILE)
        .key("input_id", &[K::Session]).whole("S14").rollback(Rollback::R1, Some("S14 exporter")),
    base("history-media", &["src/management/v2/media_refs.rs"],
        Dir { path: "runtime/management-v2/history-media" }, Blob, &domain::OWN_FILE).key("sha256", &[K::Host]),
    base("management-errors", &["src/management/v2/error_log.rs"],
        Jsonl { path: "runtime/service-logs/management-errors.jsonl" }, AppendLog, &domain::OWN_FILE).append(),
    base("event-blobs", &["src/management/v2/event_blobs.rs"],
        JsonDir { path: "runtime/management-v2/event-blobs" }, Blob, &domain::OWN_FILE).key("grant", &[K::Host]),
    base("attachments", &["src/management/v2/attachments.rs"],
        Dir { path: "runtime/attachments" }, Blob, &domain::OWN_FILE).key("session,file", &[K::Session]),
    base("skills-archive", &["src/management/v2/skills_archive.rs"],
        JsonDir { path: "archive/skills" }, AppendLog, &domain::OWN_FILE).key("skill_id", &[K::Host]).append(),
    base("runtime-packages", &["src/management/v2/runtime_packages.rs", "src/agent_management/runtime_package.rs"],
        JsonDir { path: "runtime/runtime-packages" }, AppendLog, &domain::OWN_FILE).key("run_id", &[K::Host]).append(),
    base("quota-continue", &["src/management/v2/quota_continue.rs"],
        Json { path: "runtime/quota-continue/state-v1.json" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Session]).whole("S13").rollback(Rollback::R1, Some("S13 exporter")),
    base("model-overload-retry", &["src/management/v2/model_overload_retry.rs"],
        Json { path: "runtime/model-overload-retry/state-v1.json" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Session]).whole("S13").rollback(Rollback::R1, Some("S13 exporter")),
    base("owner-rotation", &["src/management/v2/owner_rotation.rs"],
        Json { path: "runtime/owner-rotation/rotations.json" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Session]).whole("S13").rollback(Rollback::R1, Some("S13 exporter")),
    base("guides-prefs", &["src/management/v2/guides_preferences.rs"],
        Json { path: "runtime/users/guides-preferences-v1.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("profile-auto-pick", &["src/management/v2/profile_auto_pick.rs"],
        Json { path: "runtime/profile-auto-pick" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("profile-quota", &["src/profiles/quota.rs", "src/management/v2/profile_quota_native.rs", "src/management/v2/profile_quota_claude.rs"],
        JsonDir { path: "runtime/profile-quota" }, Cache, &domain::OWN_FILE).key("profile_id", &[K::Host]).config(SMALL),
    base("user-instructions", &["src/management/v2/user_instructions.rs"],
        JsonDir { path: "runtime/user-instructions" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("fork-preview", &["src/management/v2/fork_preview.rs", "src/session/fork_preview_size.rs"],
        Dir { path: "runtime/fork-preview" }, Cache, &domain::OWN_FILE),
    base("downgrade-drill", &["src/management/v2/downgrade_drill.rs"],
        Dir { path: "runtime/downgrade-drill" }, Cache, &domain::OWN_FILE),
    // --- agent bus / roster / director ----------------------------------------
    // S4: SQLite rows; the legacy JSON is only the import source and the R1
    // export target. Writes still go through the whole-state `with_state`
    // callback (typed diff -> changed rows) until the call sites move to row
    // mutations, hence WholeState until then.
    base("agent-management", &["src/agent_management/store.rs", "src/agent_management/roster_sql.rs", "src/agent_management/create_abandon.rs"],
        Sqlite { file: "runtime/agent-management/v1/roster.sqlite3", tables: &["roster_meta", "roster_agents", "roster_projects", "roster_project_states", "roster_project_tombstones", "roster_presentations", "roster_memberships", "roster_grants", "roster_grant_revisions", "roster_actions", "roster_receipts", "roster_events", "roster_extra", "roster_changes", "roster_audit_outbox"] },
        MutableState, &domain::ROSTER)
        .key("agent_id / project_id / action_id", &[K::Host, K::Project])
        .indexes(&["agents.project_id", "agents.name", "memberships.project_id", "actions.project_id", "actions.phase", "events.action_id"])
        .grows(4_173_656, 3_324, "agents, projects, memberships, actions, receipts, history events")
        .fixture("roster").whole("S4").rollback(Rollback::R1, Some("cutex storage roster-export (roster_sql::export_previous)")),
    base("agent-management-audit", &["src/agent_management/roster_sql.rs"],
        Jsonl { path: "runtime/agent-management/v1/roster-audit.jsonl" }, AppendLog, &domain::OWN_FILE)
        .key("outbox seq", &[K::Host]).grows(1_112_994, 2_048, "roster history events").append().slice("S4"),
    base("pending-forget", &["src/agent_management/pending_forget.rs"],
        Json { path: "runtime/agent-management/deleted-sessions-pending-forget.json" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Session]).whole("S4"),
    base("agent-bus-registry", &["src/agent_bus/store.rs"],
        Json { path: "runtime/agent-bus-registry.json" }, MutableState, &domain::OWN_FILE)
        .key("session", &[K::Host]).whole("S4"),
    base("agent-bus-audit", &["src/agent_bus/audit.rs"],
        Jsonl { path: "runtime/agent-bus-audit.jsonl" }, AppendLog, &domain::OWN_FILE).append(),
    base("alarms", &["src/agent_bus/alarms.rs"],
        Json { path: "runtime/alarms/alarms.json" }, MutableState, &domain::OWN_FILE)
        .key("alarm_id", &[K::Host, K::Recipient]).indexes(&["next_due", "state"])
        .grows(216_915, 0, "alarms incl. fired history")
        .whole("S12").rollback(Rollback::R1, Some("S12 exporter")),
    base("director-address", &["src/director_address/store.rs"],
        Json { path: "runtime/director-address/ownership-v1.json" }, MutableState, &domain::OWN_FILE)
        .key("project", &[K::Project]).whole("S12").rollback(Rollback::R1, Some("S12 exporter")),
    base("director-management", &["src/director_address/management.rs"],
        Json { path: "runtime/director-address/management-v1.json" }, MutableState, &domain::OWN_FILE)
        .key("project", &[K::Project]).whole("S12").rollback(Rollback::R1, Some("S12 exporter")),
    base("owner-notify", &["src/owner_notify/store.rs", "src/owner_notify/sql.rs"],
        Sqlite { file: "runtime/owner-notify/owner-notify.sqlite3", tables: &["notifications", "notification_actions", "notification_meta", "notification_count", "notification_outbox_count", "store_outbox"] }, MutableState, &domain::OWNER_NOTIFY)
        .key("notification_id", &[K::Host]).grows(75_671, 0, "owner notifications")
        .fixture("owner_notify").layer(1).slice("S10").indexes(&["pending/time", "agent/time", "state/agent", "safety", "action_id"]).rollback(Rollback::R1, Some("storage notification-store --rollback --daemon-stopped")),
    base("approvals", &["src/approvals.rs", "src/approvals_sql.rs", "src/approval_rules.rs"],
        Sqlite { file: "runtime/approvals/approvals.sqlite3", tables: &["approval_rows", "approval_rules", "approval_audit", "approval_nonces", "approval_overrides", "approval_meta", "approval_count", "approval_rule_count", "approval_audit_count", "approval_orphan_seen", "store_outbox"] }, MutableState, &domain::APPROVALS)
        .key("approval_id / rule_id / nonce", &[K::Host]).layer(1).slice("S10").indexes(&["state/time", "requester/digest", "rule subject", "nonce", "wait deadline"]).rollback(Rollback::R1, Some("storage approval-store --rollback --daemon-stopped")),
    base("notify-config", &["src/notify/session.rs"],
        JsonDir { path: "notifications" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    // --- peers / services ------------------------------------------------------
    base("peers", &["src/peers/store.rs", "src/peers/routes.rs"],
        Json { path: "runtime/peers/peers.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("peer-tells", &["src/peers/tells.rs"],
        Json { path: "runtime/peers/tell-receipts.json" }, MutableState, &domain::OWN_FILE)
        .key("receipt", &[K::Host]).whole("S13"),
    base("peer-audit", &["src/peers/audit.rs"],
        Jsonl { path: "runtime/peers/peer-audit.jsonl" }, AppendLog, &domain::OWN_FILE)
        .key("peer,seq", &[K::Host]).grows(4_000_000, 0, "peer audit records")
        .append().slice("S13").rollback(Rollback::R3, Some("S13 exporter")),
    base("services", &["src/services/store.rs", "src/services/supervisor.rs"],
        Json { path: "services/registry.json" }, MutableState, &domain::OWN_FILE)
        .key("slug", &[K::Host, K::Project]).grows(47_400, 0, "registered services")
        .whole("S11").rollback(Rollback::R1, Some("S11 exporter")),
    // --- users / profiles / small config ----------------------------------------
    base("users", &["src/management/users.rs", "src/management/persons.rs", "src/management/user_home.rs"],
        Json { path: "runtime/users/users-v1.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    // 056 user layout: one JSON document per user/device key under the users dir (r275 merge).
    base("user-layout", &["src/management/user_layout.rs"],
        JsonDir { path: "runtime/users/layout" }, SmallConfig, &domain::OWN_FILE).config(512 * 1024),
    base("user-profiles", &["src/management/user_profiles.rs"],
        Json { path: "profiles-v1.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("client-tokens", &["src/management/client_tokens.rs"],
        Json { path: "management-clients/tokens.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    // Relay trust is separate from ordinary upgrade backups. These R0 entries
    // retain current trust, never restore stale auth snapshots. Pre-QR readers
    // reject bootstrap metadata in identity.json: the kit refuses an r279
    // rollback before stopping services when those fields are present.
    base("relay-identity", &["src/management/relay.rs", "src/management/relay/bootstrap.rs"],
        Json { path: "management-relay/identity.json" }, SmallConfig, &domain::OWN_FILE).config(64 * 1024),
    base("relay-certificates", &["src/management/relay.rs"],
        Json { path: "management-relay/certificates.json" }, SmallConfig, &domain::OWN_FILE).config(16 * 1024 * 1024),
    // Bounded authentication coordination, not an unbounded cache. Each file
    // is an OwnFile domain; the encrypted journal is the durable cross-file
    // operation/outbox and token activation is its final security commit.
    base("relay-pairing-key", &["src/management/relay/pairing.rs"],
        Json { path: "management-relay/pairing-recovery-key.json" }, SmallConfig, &domain::OWN_FILE).config(4 * 1024),
    base("relay-pairing-journal", &["src/management/relay/pairing.rs"],
        Json { path: "management-relay/pairing-journal.json" }, SmallConfig, &domain::OWN_FILE).config(2 * 1024 * 1024),
    base("relay-audit", &["src/management/relay.rs"],
        Jsonl { path: "management-relay/audit.jsonl" }, AppendLog, &domain::OWN_FILE).append(),
    base("hosts", &["src/management/connections.rs"],
        Json { path: "hosts.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("im-sessions", &["src/im/registry.rs", "src/session/im_exposure_migration.rs"],
        Json { path: "im-sessions.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("profile-avatars", &["src/profiles/avatar.rs"],
        Json { path: "runtime/profile-avatars/avatars-v1.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("profiles", &["src/profiles/store.rs", "src/profiles/identity.rs", "src/profiles/lifecycle.rs", "src/profiles/settings_api.rs", "src/profiles/catalog_api.rs", "src/profiles/catalog_overlay.rs", "src/profiles/generic_switches.rs", "src/profiles/claude_compaction.rs", "src/profiles/claude_tool_defaults.rs", "src/profiles/reviewer_catalog.rs", "src/profiles/account_link.rs", "src/profiles/transfer.rs", "src/profiles/stored_keys.rs", "src/profiles/cc_switch.rs", "src/profiles/materialize.rs", "src/profiles/kimi_launch.rs", "src/profiles/kimi_accounts.rs"],
        JsonDir { path: "profiles" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("config", &["src/config/store.rs", "src/config/paths.rs", "src/config/text.rs", "src/config/fresh_agent_permissions.rs", "src/config/notification_levels.rs", "src/cli_app/profile.rs"],
        Json { path: "config.json, accounts.json, state.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("claude-accounts", &["src/runtime/claude_accounts.rs"],
        Json { path: "runtime/claude/accounts.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("runtime-intent", &["src/session/runtime_intent.rs"],
        Json { path: "runtime/runtime-intent/online.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("prompt-cache-key", &["src/launch/prompt_cache_key.rs"],
        Json { path: "runtime/prompt-cache-key.json" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("memory", &["src/memory/store.rs"],
        Markdown { path: "projects/<key>/memory" }, MutableState, &domain::OWN_FILE).key("project,file", &[K::Project]),
    base("files-revert-journal", &["src/files_revert/journal.rs"],
        Dir { path: "<journal dir>" }, MutableState, &domain::OWN_FILE).key("action", &[K::Session]),
    base("history-reverts", &["src/cli_app/dsh_structure.rs", "src/cli_app/session_history.rs"],
        Dir { path: "runtime/history-reverts" }, AppendLog, &domain::OWN_FILE).key("action", &[K::Session]).append(),
    base("inbound-mail", &["src/inbound_mail/mod.rs"],
        JsonDir { path: "runtime/inbound-mail" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("nci-verification", &["src/nci_verification/mod.rs"],
        JsonDir { path: "runtime/nci-verification" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("app-server-journal", &["src/app_server/journal.rs", "src/app_server/live_settings.rs"],
        Dir { path: "runtime/app-server-journal" }, AppendLog, &domain::OWN_FILE).append(),
    base("action-recovery", &["src/session/fork.rs", "src/session/runtime_stop.rs", "src/cli_app/session_delete.rs", "src/agent_management/human_config.rs", "src/agent_management/human_runtime.rs"],
        JsonDir { path: "runtime/<per-action journals>" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("session-display", &["src/launch/session_display.rs"],
        JsonDir { path: "runtime/session-display" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    base("components-index", &["src/components/index.rs", "src/components/mod.rs"],
        JsonDir { path: "components" }, SmallConfig, &domain::OWN_FILE).config(SMALL),
    // --- native runtimes / components (registered, measured; not cutex stores) ---
    base("codex-thread-history", &["src/launch/native_history.rs"],
        External { owner: "cute-codex", path: "codex-home/thread_history_1.sqlite" }, StoreClass::External, &domain::EXTERNAL)
        .external().slice("native-history (conditional)"),
    base("claude-adapter-state", &["src/runtime/claude/structure.rs", "src/runtime/claude/fork_history.rs", "src/runtime/claude_pin.rs"],
        External { owner: "claude runtime component", path: "runtime/claude/<session>/state.sqlite3" }, StoreClass::External, &domain::EXTERNAL)
        .external().slice("S2").rollback(Rollback::R2, None),
    base("dsh-timeline", &["src/runtime/dsh.rs", "src/runtime/dsh/product.rs"],
        External { owner: "dsh runtime component", path: "runtime/dsh/<session>/timeline.sqlite" }, StoreClass::External, &domain::EXTERNAL).external(),
];

pub fn find(id: &str) -> Option<&'static StoreSpec> {
    MANIFEST.iter().find(|spec| spec.id == id)
}

/// Manifest entry by id. An unknown id is a programming error.
pub fn spec(id: &str) -> &'static StoreSpec {
    find(id).unwrap_or_else(|| panic!("store {id:?} is not registered in store::manifest"))
}

/// Store ids owned by a repo-relative source file.
pub fn owned_by(file: &str) -> impl Iterator<Item = &'static StoreSpec> + '_ {
    MANIFEST.iter().filter(move |spec| spec.owner.contains(&file))
}

/// Problems with the manifest; empty means valid.
pub fn validate(repo_root: Option<&std::path::Path>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for spec in MANIFEST {
        let id = spec.id;
        if !seen.insert(id) {
            problems.push(format!("{id}: duplicate store id"));
        }
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            problems.push(format!("{id}: id must be kebab-case"));
        }
        if spec.owner.is_empty() {
            problems.push(format!("{id}: no owner file"));
        }
        if domain::domain(spec.domain.id).is_none() {
            problems.push(format!("{id}: domain {} is not in DOMAINS", spec.domain.id));
        }
        if let Some(root) = repo_root {
            for owner in spec.owner {
                if !root.join(owner).is_file() {
                    problems.push(format!("{id}: owner file {owner} does not exist"));
                }
            }
        }
        match (spec.domain.kind, &spec.canonical) {
            (domain::DomainKind::Sqlite, Sqlite { file, .. }) if *file != spec.domain.path => {
                problems.push(format!("{id}: SQLite file {file} is not its domain file {}", spec.domain.path))
            }
            (domain::DomainKind::Sqlite, Sqlite { .. } | HotStore { .. }) => {}
            (domain::DomainKind::Sqlite, _) => problems.push(format!("{id}: SQLite domain but non-SQLite canonical")),
            (domain::DomainKind::External, External { .. }) => {}
            (domain::DomainKind::External, _) | (_, External { .. }) => {
                problems.push(format!("{id}: external canonical and domain must match"))
            }
            (domain::DomainKind::OwnFile, Sqlite { .. } | HotStore { .. }) => {
                problems.push(format!("{id}: SQLite store needs its SQLite domain"))
            }
            _ => {}
        }
        if matches!(spec.class, MutableState | AppendLog) && spec.access != Access::External {
            if spec.key.is_empty() && !matches!(spec.access, Access::AppendOnly) {
                problems.push(format!("{id}: mutable/append store without a key description"));
            }
            if spec.is_hot() {
                if spec.growth.with.is_empty() {
                    problems.push(format!("{id}: hot store must say what it grows with"));
                }
                if spec.fixture.is_none() && spec.slice == "keep" {
                    problems.push(format!("{id}: hot store needs a fixture area or a slice that adds one"));
                }
                if spec.page.max_rows == 0 || spec.page.max_bytes == 0 {
                    problems.push(format!("{id}: hot store needs a page budget"));
                }
            }
        }
        if let Access::WholeState { retire_in } = spec.access {
            if retire_in.is_empty() || retire_in == "keep" {
                problems.push(format!("{id}: whole-state access must name the slice retiring it"));
            }
        }
        if matches!(spec.rollback, Rollback::R1 | Rollback::R3) && spec.previous_exporter.is_none() {
            problems.push(format!("{id}: {:?} rollback needs a previous-kit exporter", spec.rollback));
        }
        if spec.schema_version > 0 && spec.access != Access::Keyed {
            problems.push(format!("{id}: a store on the layer must be Access::Keyed"));
        }
        if let Access::SmallConfig { max_bytes } = spec.access {
            if spec.growth.bytes_1x > max_bytes as u64 {
                problems.push(format!("{id}: grew past its small-config cap; reclassify"));
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_dispositions_are_registered_as_permanent_source_authority() {
        let entry=spec("input-dispositions");
        assert_eq!(entry.owner,&["src/management/v2/input_disposal.rs"]);
        assert!(matches!(entry.canonical,Canonical::Sqlite{file:"runtime/management-v2/input-dispositions.sqlite3",tables} if tables==["owner_dispositions"]));
        assert_eq!(entry.class,StoreClass::MutableState);
        assert_eq!(entry.domain.path,"runtime/management-v2/input-dispositions.sqlite3");
        assert_eq!(entry.access,Access::Keyed);assert_eq!(entry.key,"session,input_id");
        assert_eq!(entry.rollback,Rollback::R0);assert!(entry.previous_exporter.is_none());
        assert_eq!(entry.schema_version,0,"schema is standalone PRAGMA user_version, not StoreDb layer");
        assert!(entry.growth.with.contains("no row TTL"));
        assert!(crate::upgrade::backup::STATE_DIRS.iter().any(|root|
            entry.domain.path.starts_with(&format!("{root}/"))));
    }

    #[test]
    fn manifest_is_valid() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let problems = validate(Some(root));
        assert!(problems.is_empty(), "store manifest problems:\n{}", problems.join("\n"));
    }

    #[test]
    fn spec_lookup_finds_registered_and_rejects_unknown() {
        assert_eq!(spec("asks").domain.id, "management-state");
        assert!(find("no-such-store").is_none());
        assert!(std::panic::catch_unwind(|| spec("no-such-store")).is_err());
    }

    #[test]
    fn relay_pairing_auth_files_have_real_owners_and_caps() {
        for (id,path,cap) in [("relay-pairing-key","management-relay/pairing-recovery-key.json",4*1024),("relay-pairing-journal","management-relay/pairing-journal.json",2*1024*1024)] {
            let entry=spec(id);assert_eq!(entry.owner,&["src/management/relay/pairing.rs"]);
            assert!(matches!(entry.canonical,Canonical::Json{path:p} if p==path));
            assert_eq!(entry.access,Access::SmallConfig{max_bytes:cap});assert_eq!(entry.rollback,Rollback::R0);
        }
        assert!(spec("relay-identity").owner.contains(&"src/management/relay/bootstrap.rs"));
    }

    /// The scale gate (design §7) and the manifest name the same stores.
    /// Active once `tools/scale_gate/manifest.json` is in the tree.
    #[test]
    fn scale_gate_stores_match_manifest_fixtures() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SCALE_GATE_MANIFEST);
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("scale gate manifest not in this tree yet: {}", path.display());
            return;
        };
        let gate: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let gate_ids: std::collections::BTreeSet<&str> = gate["stores"]
            .as_array()
            .unwrap()
            .iter()
            .map(|store| store["id"].as_str().unwrap())
            .collect();
        let fixtures: std::collections::BTreeSet<&str> =
            MANIFEST.iter().filter_map(|spec| spec.fixture).collect();
        assert_eq!(gate_ids, fixtures, "scale gate stores and manifest fixture ids differ");
    }
}
