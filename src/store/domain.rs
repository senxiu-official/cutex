//! Transaction domains (design 058 §9, first row).
//!
//! A domain is one SQLite file, or one JSON/JSONL file or directory together
//! with its lock. Atomicity exists only inside a domain; follow-up work in
//! another domain goes through the outbox. There is no host-wide database.
//! A new SQL store joins an existing domain only when its writes must be
//! atomic with rows already there.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DomainKind {
    Sqlite,
    /// The store's canonical JSON file (or directory) and its lock.
    OwnFile,
    /// Written by a native runtime or a runtime component, not by cutex.
    External,
}

#[derive(Debug)]
pub struct TxDomain {
    pub id: &'static str,
    pub kind: DomainKind,
    /// Relative to the cutex home. Empty for `OwnFile`.
    pub path: &'static str,
    pub note: &'static str,
}

macro_rules! sqlite_domain {
    ($name:ident, $id:literal, $path:literal, $note:literal) => {
        pub static $name: TxDomain = TxDomain {
            id: $id,
            kind: DomainKind::Sqlite,
            path: $path,
            note: $note,
        };
    };
}

sqlite_domain!(
    MANAGEMENT_STATE,
    "management-state",
    "runtime/management-v2/management-state.sqlite3",
    "hot_store rows (asks, agent_bus, settlement evidence) and the asks cold archive; per-store generation CAS"
);
sqlite_domain!(SESSIONS, "sessions", "cutex-sessions.sqlite3", "session rows and receipts");
sqlite_domain!(
    INPUT_LEDGER,
    "input-ledger",
    "runtime/management-v2/input-ledger.sqlite3",
    "054 input ledger with its own outbox"
);
sqlite_domain!(
    INPUT_DISPOSITIONS,
    "input-dispositions",
    "runtime/management-v2/input-dispositions.sqlite3",
    "durable owner disposal decisions; independent of the rebuildable input ledger"
);
sqlite_domain!(
    REQUEST_IDEMPOTENCY,
    "request-idempotency",
    "runtime/management-v2/request-idempotency.sqlite3",
    "request idempotency receipts"
);
sqlite_domain!(PAN, "pan", "pan/pan.sqlite", "Pan download tasks and audit entries commit together");
sqlite_domain!(NAME_TARGET_PINS, "name-target-pins", "runtime/name-target-pins.sqlite3", "immutable action recipient bindings");
sqlite_domain!(
    INTEGRATION_DEDUPE,
    "integration-dedupe",
    "runtime/management-v2/integration-event-dedupe-v2.sqlite",
    "integration event identities (kept forever)"
);
sqlite_domain!(
    PENDING_STOPS,
    "pending-stops",
    "runtime/management-v2/pending-stops.sqlite",
    "native stop observer queue"
);
sqlite_domain!(
    PROJECT_EVENTS,
    "project-events",
    "runtime/project-events/journal.sqlite3",
    "paged project event journal"
);
sqlite_domain!(
    WORKFLOWS,
    "workflows",
    "runtime/workflows/runs.sqlite3",
    "workflow runs, steps and logs"
);
sqlite_domain!(
    ARTIFACTS,
    "artifacts",
    "runtime/artifacts/artifacts-v1.sqlite3",
    "artifact versions and actions"
);
sqlite_domain!(
    DOCUMENTS,
    "documents",
    "runtime/documents/documents-v1.sqlite3",
    "documents"
);
sqlite_domain!(ARCHIVE, "archive", "archive/index-v1.sqlite3", "archive index");
sqlite_domain!(SUMMARY, "summary", "summary/summary.sqlite3", "summary journal and projections");
sqlite_domain!(NOTES, "notes", "notes/notes.sqlite3", "notes, pins and nonces");
sqlite_domain!(OWNER_PINS, "owner-pins", "runtime/owner-pins/owner-pins.sqlite3", "备忘: pins agents put up for the owner, and their action receipts");
sqlite_domain!(
    NOTIFY_STATES,
    "notify-states",
    "notifications/states.sqlite3",
    "notification read state"
);
sqlite_domain!(
    NOTIFY_OUTBOUND,
    "notify-outbound",
    "notifications/outbound.sqlite3",
    "outbound notification deliveries"
);
sqlite_domain!(
    STORAGE,
    "storage",
    "runtime/storage/storage.sqlite3",
    "storage walk cache and cleanup records"
);
sqlite_domain!(
    USAGE,
    "usage",
    "runtime/management-v2/usage-index.sqlite3",
    "reserved for S1: usage index, day aggregates, quota reads; raw samples stay JSONL"
);
sqlite_domain!(
    USAGE_COLLECTORS,
    "usage-collectors",
    "runtime/management-v2/usage-collector-checkpoints.sqlite3",
    "collector source/file checkpoints and small scan status; separate from billing JSONL and the report index"
);
sqlite_domain!(
    ROSTER,
    "roster",
    "runtime/agent-management/v1/roster.sqlite3",
    "S4 (r276): agents, projects, memberships, grants, actions, receipts, history; next to the roster lock files"
);
sqlite_domain!(
    OWNER_INPUT,
    "owner-input",
    "runtime/management-v2/owner-input.sqlite3",
    "reserved for S5 (r276); separate from the 054 input ledger"
);
sqlite_domain!(OWNER_NOTIFY, "owner-notify", "runtime/owner-notify/owner-notify.sqlite3", "S10 notifications, action receipts and notice outbox");
sqlite_domain!(APPROVALS, "approvals", "runtime/approvals/approvals.sqlite3", "S10 approvals, rules and confirmation nonces");

sqlite_domain!(SESSION_ACTIVITY, "session-activity", "runtime/management-v2/session-activity.sqlite3", "S7 activity rows and deletion fences");
sqlite_domain!(SESSION_PROJECTION, "session-projection", "runtime/management-v2/session-projection.sqlite3", "S7 presentation revisions and focus; independent of delivery");

pub static OWN_FILE: TxDomain = TxDomain {
    id: "own-file",
    kind: DomainKind::OwnFile,
    path: "",
    note: "the store's canonical file or directory and its lock",
};

pub static EXTERNAL: TxDomain = TxDomain {
    id: "external",
    kind: DomainKind::External,
    path: "",
    note: "native runtime or runtime component owned; cutex registers and measures only",
};

pub static DOMAINS: &[&TxDomain] = &[
    &OWNER_NOTIFY,
    &APPROVALS,
    &MANAGEMENT_STATE,
    &SESSIONS,
    &INPUT_LEDGER,
    &INPUT_DISPOSITIONS,
    &REQUEST_IDEMPOTENCY,
    &NAME_TARGET_PINS,
    &PAN,
    &INTEGRATION_DEDUPE,
    &PENDING_STOPS,
    &PROJECT_EVENTS,
    &WORKFLOWS,
    &ARTIFACTS,
    &DOCUMENTS,
    &ARCHIVE,
    &SUMMARY,
    &NOTES,
    &OWNER_PINS,
    &NOTIFY_STATES,
    &NOTIFY_OUTBOUND,
    &STORAGE,
    &USAGE,
    &USAGE_COLLECTORS,
    &ROSTER,
    &OWNER_INPUT,
    &SESSION_ACTIVITY,
    &SESSION_PROJECTION,
    &OWN_FILE,
    &EXTERNAL,
];

pub fn domain(id: &str) -> Option<&'static TxDomain> {
    DOMAINS.iter().copied().find(|domain| domain.id == id)
}
