//! Design 043 A1/A2 discovery. No deletion or session/configuration mutation.
//!
//! Discovery and preparation run outside lifecycle locks. The prepared ticket
//! is checked using bounded metadata only while the engine holds its locks.
//! Unknown state is never evidence of an unreferenced or regenerable home.
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use super::storage_cleanup::Candidate;
use crate::session::model::{CutexSessionRecord, CutexSessionStore};

const PREFIX: &str = "runtime/light/agents";
const MAX_ENTRIES: usize = 200_000;
const MAX_JSON: u64 = 8 * 1024 * 1024;
// The deployed fork's upstream source base is 0.160.0. Unknown native pins
// require a separate writer audit before their reserved paths are eligible.
const AUDITED_NATIVE: &str = "f2469b10fa073f3041251014d52dceb0ce10465c";

// Capture is scoped to synchronous preparation on this thread. Ordinary preview
// discovery never accumulates a ticket or fingerprints the global home trees.
const MAX_RECHECK_STAMPS: usize = 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    directory: bool,
    linked: bool,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(windows)]
    attributes: u32,
}
fn stamp(path: &Path) -> Result<Option<Stamp>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(Stamp {
        len: meta.len(),
        modified: meta.modified()?,
        created: meta.created().ok(),
        directory: meta.is_dir(),
        linked: linked(&meta),
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
        },
        #[cfg(windows)]
        attributes: {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes()
        },
    }))
}
#[derive(Default)]
struct Capture {
    files: BTreeMap<PathBuf, Option<Stamp>>,
    unstable: bool,
}
thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
    static ALIASES: RefCell<Option<Vec<(PathBuf, PathBuf)>>> = const { RefCell::new(None) };
}
fn observe(path: &Path) -> Result<()> {
    CAPTURE.with(|slot| -> Result<()> {
        let mut slot = slot.borrow_mut();
        let Some(capture) = slot.as_mut() else {
            return Ok(());
        };
        ensure!(
            capture.files.len() < MAX_RECHECK_STAMPS || capture.files.contains_key(path),
            "reference stamp budget exceeded"
        );
        let current = stamp(path)?;
        if let Some(previous) = capture.files.get(path) {
            if previous != &current {
                capture.unstable = true;
            }
        } else {
            capture.files.insert(path.to_owned(), current);
        }
        Ok(())
    })
}

pub(super) struct PreparedHomeRecheck {
    pub(super) candidate: Candidate,
    root: PathBuf,
    action: String,
    files: BTreeMap<PathBuf, Option<Stamp>>,
    aliases: Vec<(PathBuf, PathBuf)>,
}

/// Broad reference parsing happens here, before acquiring any lifecycle locks.
/// Only the selected A1 home or A2 leaf is classified; other home trees are not
/// inventoried. Their provenance and membership still fence cross-home refs.
pub(super) fn prepare_recheck(
    home: &Path,
    action: &str,
    relative: &Path,
    now: SystemTime,
) -> Result<PreparedHomeRecheck> {
    ensure!(
        matches!(action, "orphan-homes" | "retired-homes"),
        "unknown home cleanup action"
    );
    let root = home.canonicalize()?;
    let suffix = relative.strip_prefix(PREFIX)?;
    ensure!(
        suffix
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "invalid selected home path"
    );
    let id = suffix
        .components()
        .next()
        .context("home identity missing")?
        .as_os_str()
        .to_str()
        .context("home identity invalid")?;
    ensure!(
        uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id),
        "noncanonical selected home"
    );
    let own = Path::new(PREFIX).join(id);
    ensure!(
        (action == "orphan-homes" && relative == own)
            || (action == "retired-homes"
                && relative != own
                && regenerable(relative.strip_prefix(&own)?)),
        "selected item is outside cleanup policy"
    );
    CAPTURE.with(|slot| *slot.borrow_mut() = Some(Capture::default()));
    ALIASES.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
    // Always drain capture state, including classification errors.
    let result = (|| -> Result<Candidate> {
        metadata(&root, Path::new(PREFIX))?.context("native home root missing")?;
        let selected_meta = metadata(&root, relative)?.context("selected item missing")?;
        ensure!(
            if action == "orphan-homes" {
                selected_meta.is_dir()
            } else {
                selected_meta.is_file()
            },
            "selected item has wrong type"
        );
        let mut homes = Vec::new();
        for entry in fs::read_dir(root.join(PREFIX))? {
            ensure!(
                homes.len() < MAX_RECHECK_STAMPS,
                "home provenance budget exceeded"
            );
            homes.push(Path::new(PREFIX).join(entry?.file_name()));
        }
        for file in [
            "cutex-sessions.json",
            "cutex-sessions.sqlite3",
            "cutex-sessions.sqlite3-wal",
            "cutex-sessions.sqlite3-shm",
        ] {
            metadata(&root, Path::new(file))?;
        }
        let sessions = crate::session::store::load_cutex_session_store_from_path(
            &root.join("cutex-sessions.json"),
        )?;
        let refs = references(&root, &homes, &sessions)?;
        if refs.pending_creates.contains(id) {
            return Ok(decision(
                relative,
                "unfinished_create",
                "An unfinished create owns this native identity",
            ));
        }
        let key = path_key(&root.join(&own))?;
        let mut owners = Vec::new();
        for record in sessions.sessions.values() {
            if owns(record, &key, id)? {
                owners.push(record);
            }
        }
        if action == "retired-homes" {
            return retired(&root, &own, &owners, &refs, Some(relative))?
                .into_iter()
                .find(|candidate| candidate.relative == relative)
                .context("selected leaf is not regenerable or its home is protected");
        }
        Ok(if !owners.is_empty() {
            decision(
                relative,
                "session_reference",
                "A session references this native home",
            )
        } else if registered_runtime(&root, &owners, id)? {
            decision(
                relative,
                "runtime_registered",
                "A runtime registration references this home",
            )
        } else if other_reference(&refs, &key) {
            decision(
                relative,
                "history_source",
                "A native home or deployment references this home",
            )
        } else if now
            .duration_since(newest(&root.join(&own))?)
            .unwrap_or_default()
            < Duration::from_secs(7 * 86_400)
        {
            decision(
                relative,
                "too_recent",
                "Home tree was modified within seven days",
            )
        } else {
            decision(
                relative,
                "eligible",
                "Unreferenced native home older than seven days",
            )
        })
    })();
    let capture = CAPTURE.with(|slot| slot.borrow_mut().take()).unwrap();
    let mut aliases = ALIASES.with(|slot| slot.borrow_mut().take()).unwrap();
    aliases.sort();
    aliases.dedup();
    let mut ticket = PreparedHomeRecheck {
        candidate: result.unwrap_or_else(|error| {
            decision(
                relative,
                "reference_state_unavailable",
                &format!("Reference evidence unavailable: {error}"),
            )
        }),
        root,
        action: action.into(),
        files: capture.files,
        aliases,
    };
    if ticket.candidate.eligible
        && (capture.unstable
            || ticket.aliases.len() > MAX_RECHECK_STAMPS
            || !recheck(
                home,
                action,
                relative,
                &ticket,
                Instant::now() + Duration::from_millis(50),
            )?)
    {
        ticket.candidate = decision(
            relative,
            "reference_evidence_changed",
            "Reference evidence changed or exceeded the bounded recheck budget",
        );
    }
    Ok(ticket)
}

/// Caller holds the shared lifecycle gate and the session-store file lock.
/// No contents, JSON, database reads, directory inventories or fingerprints.
pub(super) fn recheck(
    home: &Path,
    action: &str,
    relative: &Path,
    ticket: &PreparedHomeRecheck,
    deadline: Instant,
) -> Result<bool> {
    if !ticket.candidate.eligible
        || action != ticket.action
        || relative != ticket.candidate.relative
        || Instant::now() >= deadline
    {
        return Ok(false);
    }
    if home.canonicalize()? != ticket.root {
        return Ok(false);
    }
    for (path, previous) in &ticket.files {
        if Instant::now() >= deadline || stamp(path).ok().as_ref() != Some(previous) {
            return Ok(false);
        }
    }
    for (path, target) in &ticket.aliases {
        if Instant::now() >= deadline || path.canonicalize().ok().as_ref() != Some(target) {
            return Ok(false);
        }
    }
    Ok(Instant::now() < deadline)
}

fn decision(relative: &Path, code: &str, reason: &str) -> Candidate {
    Candidate {
        relative: relative.into(),
        eligible: code == "eligible",
        code: code.into(),
        reason: reason.into(),
    }
}

fn linked(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// Trusted instance root may be a junction; no component below it may be.
fn metadata(root: &Path, relative: &Path) -> Result<Option<fs::Metadata>> {
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "invalid relative path"
    );
    let mut path = root.to_owned();
    let count = relative.components().count();
    for (index, part) in relative.components().enumerate() {
        path.push(part.as_os_str());
        observe(&path)?;
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(!linked(&meta), "linked reference path");
        if index + 1 == count {
            return Ok(Some(meta));
        }
        ensure!(meta.is_dir(), "reference parent is not a directory");
    }
    Ok(None)
}

fn json(root: &Path, relative: &Path) -> Result<Option<Value>> {
    let Some(meta) = metadata(root, relative)? else {
        return Ok(None);
    };
    ensure!(
        meta.is_file() && meta.len() <= MAX_JSON,
        "reference document is not a bounded file"
    );
    let value: Value = serde_json::from_slice(&fs::read(root.join(relative))?)?;
    ensure!(value.is_object(), "reference document must be an object");
    Ok(Some(value))
}

/// Resolve the longest existing prefix before appending a missing historical
/// tail. Text-only fallback loses junction, drive and 8.3 aliases when the
/// original rollout was removed but its home still exists. Other lookup errors
/// are unknown references, never permission to clean.
fn path_key(path: &Path) -> Result<String> {
    ensure!(path.is_absolute(), "absolute reference path required");
    let mut prefix = path.to_owned();
    let mut tail = Vec::new();
    let mut path = loop {
        observe(&prefix)?;
        match prefix.canonicalize() {
            Ok(path) => {
                ALIASES.with(|capture| {
                    if let Some(aliases) = capture.borrow_mut().as_mut() {
                        aliases.push((prefix.clone(), path.clone()));
                    }
                });
                break path;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = prefix
                    .components()
                    .next_back()
                    .context("reference has no existing prefix")?;
                ensure!(
                    !matches!(component, Component::RootDir | Component::Prefix(_)),
                    "reference root unavailable"
                );
                tail.push(component.as_os_str().to_owned());
                ensure!(prefix.pop(), "reference has no existing prefix");
            }
            Err(error) => return Err(error.into()),
        }
    };
    for component in tail.into_iter().rev() {
        path.push(component);
    }
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(clean.pop(), "invalid parent reference");
            }
            _ => clean.push(part.as_os_str()),
        }
    }
    let key = clean.to_string_lossy().replace('\\', "/");
    let key = key
        .strip_prefix("//?/")
        .unwrap_or(&key)
        .trim_end_matches('/')
        .to_owned();
    #[cfg(windows)]
    let key = key.to_lowercase();
    Ok(key)
}

fn inside(child: &str, parent: &str) -> bool {
    child == parent
        || child
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with('/'))
}

struct References {
    // (home containing provenance, protected source). Empty origin = selection.
    paths: Vec<(String, String)>,
    selected: Option<PathBuf>,
    pending_creates: BTreeSet<String>,
}

fn pending_creates(root: &Path) -> Result<BTreeSet<String>> {
    use crate::agent_management::{
        AgentActionPhase, AgentManagementSnapshot, AgentManagementStoreSchema, AgentOperationKind,
    };
    let relative = Path::new("runtime/agent-management/v1/agent-management-v1.json");
    if metadata(root, relative)?.is_none() {
        return Ok(BTreeSet::new());
    }
    // snapshot() intentionally treats a missing lock as an empty store. Cleanup
    // cannot use that fallback when the data file exists. Read the atomically
    // replaced document without open(), which also changes directory permissions.
    ensure!(
        metadata(
            root,
            Path::new("runtime/agent-management/v1/agent-management-v1.lock")
        )?
        .is_some_and(|meta| meta.is_file()),
        "action store lock missing or invalid"
    );
    let snapshot: AgentManagementSnapshot =
        serde_json::from_value(json(root, relative)?.context("action store disappeared")?)?;
    // Match the store reader's bootstrap evidence checks. Project migrations
    // only affect roster state, which is not used for pending create identity.
    ensure!(
        snapshot.bootstrap_intents.is_empty() || snapshot.schema == AgentManagementStoreSchema::V2,
        "invalid bootstrap store schema"
    );
    for (action, intent) in &snapshot.bootstrap_intents {
        ensure!(
            &intent.request.action_id == action && intent.spec().is_ok(),
            "invalid bootstrap intent"
        );
    }
    Ok(snapshot
        .actions
        .values()
        .filter(|action| {
            action.operation == AgentOperationKind::Create
                && !matches!(
                    action.phase,
                    AgentActionPhase::Complete | AgentActionPhase::NoWrite
                )
        })
        .filter_map(|action| action.known_native_session_id.clone())
        .collect())
}

fn references(root: &Path, homes: &[PathBuf], sessions: &CutexSessionStore) -> Result<References> {
    let mut refs = References {
        paths: Vec::new(),
        selected: None,
        pending_creates: pending_creates(root)?,
    };
    if let Some(deployment) = json(root, Path::new("runtime/light/deployment.json"))? {
        let selected = PathBuf::from(
            deployment["native_home"]
                .as_str()
                .context("unknown selected native home")?,
        );
        refs.paths.push((String::new(), path_key(&selected)?));
        refs.selected = Some(selected);
    }
    // Include non-UUID / in-progress homes too: their adoption may protect a
    // UUID source. Linked or malformed provenance fails closed for all homes.
    for relative in homes {
        let origin = path_key(&root.join(relative))?;
        if let Some(adoption) = json(root, &relative.join("adoption.json"))? {
            ensure!(
                adoption.as_object().unwrap().keys().all(|key| matches!(
                    key.as_str(),
                    "native_id" | "source_history" | "sha256" | "history_home" | "dependencies"
                )),
                "unknown adoption schema"
            );
            ensure!(
                adoption["native_id"].as_str().is_some(),
                "unknown adoption identity"
            );
            for field in ["history_home", "source_history"] {
                if let Some(value) = adoption.get(field) {
                    let path = Path::new(value.as_str().context("unknown adoption source")?);
                    refs.paths.push((origin.clone(), path_key(path)?));
                }
            }
            if let Some(dependencies) = adoption.get("dependencies") {
                for dependency in dependencies
                    .as_array()
                    .context("unknown adoption dependencies")?
                {
                    let path = Path::new(
                        dependency["path"]
                            .as_str()
                            .context("unknown adoption dependency")?,
                    );
                    refs.paths.push((origin.clone(), path_key(path)?));
                }
            }
        }
        if let Some(migration) = json(root, &relative.join("v3-migration.json"))? {
            ensure!(
                migration["schema"] == "cutex.v3-to-v4-runtime-home.v1",
                "unknown migration provenance"
            );
            ensure!(
                migration.as_object().unwrap().keys().all(|key| matches!(
                    key.as_str(),
                    "schema"
                        | "native_id"
                        | "source_home"
                        | "source_contract"
                        | "source_history"
                        | "sources"
                )),
                "unknown migration fields"
            );
            for value in [
                &migration["source_home"],
                &migration["source_history"],
                &migration["source_contract"]["native_home"],
            ] {
                refs.paths.push((
                    origin.clone(),
                    path_key(Path::new(
                        value.as_str().context("unknown migration source")?,
                    ))?,
                ));
            }
            for source in migration["sources"]
                .as_array()
                .context("unknown migration sources")?
            {
                refs.paths.push((
                    origin.clone(),
                    path_key(Path::new(
                        source["path"]
                            .as_str()
                            .context("unknown migration source")?,
                    ))?,
                ));
            }
        }
    }
    for record in sessions
        .sessions
        .values()
        .filter(|record| !record.is_retired() && record.claude_session.is_none())
    {
        let Some(_) = record.codex_session_id.as_deref() else {
            continue;
        };
        let native = record
            .explicit_launch
            .as_ref()
            .map(|c| c.native_home.clone())
            .or_else(|| refs.selected.clone())
            .unwrap_or_else(|| root.join("codex-home"));
        // Recovery ancestry is constrained beneath this native home. Protecting
        // its entire scope is a conservative superset and needs no rollout walk.
        refs.paths.push((String::new(), path_key(&native)?));
    }
    Ok(refs)
}

fn other_reference(refs: &References, candidate: &str) -> bool {
    refs.paths.iter().any(|(origin, target)| {
        origin != candidate && (inside(target, candidate) || inside(candidate, target))
    })
}

fn owns(record: &CutexSessionRecord, directory: &str, native_id: &str) -> Result<bool> {
    Ok(record.codex_session_id.as_deref() == Some(native_id)
        || record
            .explicit_launch
            .as_ref()
            .is_some_and(|c| c.native_id == native_id)
        || match &record.explicit_launch {
            Some(contract) => {
                let home = path_key(&contract.native_home)?;
                inside(&home, directory) || inside(directory, &home)
            }
            None => false,
        })
}

/// Metadata only; links are not traversed and their targets do not affect age.
fn newest(root: &Path) -> Result<SystemTime> {
    let mut pending = vec![root.to_owned()];
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut count = 0;
    while let Some(path) = pending.pop() {
        count += 1;
        ensure!(count <= MAX_ENTRIES, "home inventory limit exceeded");
        let meta = fs::symlink_metadata(&path)?;
        newest = newest.max(meta.modified()?);
        if meta.is_dir() && !linked(&meta) {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(newest)
}

fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(value) => out.push(value.clone()),
        Value::Array(values) => {
            for value in values {
                strings(value, out);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                out.push(key.clone());
                strings(value, out);
            }
        }
        _ => {}
    }
}

fn mentions(value: &Value, identities: &BTreeSet<String>) -> bool {
    let mut values = Vec::new();
    strings(value, &mut values);
    values
        .iter()
        .any(|value| identities.iter().any(|id| value.contains(id)))
}

/// Read existing obligation stores only. Unknown schema/status cannot prove
/// absence. In particular, approved-but-unconsumed approvals still protect a
/// retired home. No expiry/pruning/repair writes occur during discovery.
fn obligations(root: &Path, records: &[&CutexSessionRecord], native_id: &str) -> Result<bool> {
    let ids: BTreeSet<_> = records
        .iter()
        .map(|record| record.cutex_session_id.clone())
        .chain([native_id.to_owned()])
        .collect();
    let runtime = root.join("runtime");
    let ask_database = Path::new("runtime/management-v2").join(super::hot_store::DATABASE_FILE);
    if metadata(root, Path::new("runtime/asks/asks-v1.json"))?.is_some()
        || super::hot_store::generation(&root.join(&ask_database), "asks")?.is_some()
    {
        ensure!(
            metadata(root, Path::new("runtime/asks/asks-v1.lock"))?.is_some(),
            "ask store lock missing"
        );
        let asks = crate::asks::store::AskStore::at_runtime(runtime).try_snapshot_existing()?;
        if ids.iter().any(|id| {
            asks.leases.contains_key(id)
                || asks.next_step_leases.contains_key(id)
                || asks.presence.get(id).is_some_and(|p| p.online)
        }) {
            return Ok(true);
        }
        for ask in asks.asks.values().filter(|ask| ask.closed.is_none()) {
            if ids.contains(&ask.target_cutex_session_id)
                || ask.requester.session().is_some_and(|id| ids.contains(id))
            {
                return Ok(true);
            }
        }
    }
    if let Some(approvals) = json(root, Path::new("runtime/approvals/approvals.json"))? {
        for approval in approvals["approvals"]
            .as_array()
            .context("unknown approvals document")?
        {
            match approval["state"].as_str() {
                Some("denied" | "consumed" | "expired") => {}
                Some("pending" | "approved") => {
                    if mentions(approval, &ids) {
                        return Ok(true);
                    }
                }
                _ => anyhow::bail!("unknown approval state"),
            }
        }
    }
    let relative = Path::new("runtime/workflows/runs.sqlite3");
    if metadata(root, relative)?.is_some() {
        for suffix in ["-wal", "-shm"] {
            metadata(
                root,
                &PathBuf::from(format!("{}{suffix}", relative.display())),
            )?;
        }
        let db = Connection::open_with_flags(
            root.join(relative),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(Duration::from_secs(2))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(version == 1, "unknown workflow store version");
        let mut statement = db.prepare("SELECT data,state FROM workflow_run WHERE state NOT IN ('completed','errored','stopped','rejected','expired') UNION ALL SELECT s.data,r.state FROM workflow_step s JOIN workflow_run r ON r.run_id=s.run_id WHERE r.state NOT IN ('completed','errored','stopped','rejected','expired')")?;
        let values = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for value in values {
            let (value, state) = value?;
            ensure!(
                matches!(state.as_str(), "pendingApproval" | "running" | "stopping"),
                "unknown workflow state"
            );
            let value: Value = serde_json::from_str(&value)?;
            ensure!(value.is_object(), "unknown workflow record");
            if mentions(&value, &ids) {
                return Ok(true);
            }
        }
    } else {
        ensure!(
            metadata(root, Path::new("runtime/workflows/runs.sqlite3-wal"))?.is_none(),
            "workflow WAL without database"
        );
    }
    Ok(false)
}

fn registered_runtime(
    root: &Path,
    records: &[&CutexSessionRecord],
    native_id: &str,
) -> Result<bool> {
    let ids: BTreeSet<_> = records
        .iter()
        .map(|r| r.cutex_session_id.clone())
        .chain([native_id.to_owned()])
        .collect();
    if let Some(registry) = json(root, Path::new("runtime/agent-bus-registry.json"))? {
        for (key, agent) in registry["agents"]
            .as_object()
            .context("unknown runtime registry")?
        {
            ensure!(agent.is_object(), "unknown runtime registration");
            // Do not guess whether an old registration is stale. Reconciliation
            // of stale PIDs belongs to its lifecycle owner, not cleanup.
            if ids.iter().any(|id| key.contains(id)) || mentions(agent, &ids) {
                return Ok(true);
            }
        }
    }
    if let Some(intent) = json(root, Path::new("runtime/runtime-intent/online.json"))? {
        ensure!(
            intent["version"]
                .as_u64()
                .is_none_or(|version| version <= 1),
            "unknown runtime intent version"
        );
        let intents = intent["intents"]
            .as_object()
            .context("unknown runtime intent state")?;
        if ids.iter().any(|id| intents.contains_key(id)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Upstream base 0.160.0 creators: models-manager/src/manager.rs MODEL_CACHE_FILE
/// (cache miss falls back to catalog); core/src/shell_snapshot.rs SNAPSHOT_DIR
/// (generated for each shell); tui/src/lib.rs log/codex-tui.log (diagnostic).
/// Do NOT extend this by directory names like cache/tmp, arbitrary *.log, or
/// any SQLite family. logs_2.sqlite needs a family-wide rename fence; the engine
/// currently locks per leaf, so all of its main/WAL/SHM files remain protected.
fn regenerable(path: &Path) -> bool {
    if path == Path::new("models_cache.json") || path == Path::new("log/codex-tui.log") {
        return true;
    }
    if path.parent() != Some(Path::new("shell_snapshots")) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let parts: Vec<_> = name.split('.').collect();
    parts.len() == 3
        && uuid::Uuid::parse_str(parts[0]).is_ok_and(|id| id.to_string() == parts[0])
        && !parts[1].is_empty()
        && parts[1].bytes().all(|b| b.is_ascii_digit())
        && matches!(parts[2], "sh" | "ps1")
}

/// Config/bundle values also include URLs, commands and prose. Keep their raw
/// references, but only ask the filesystem to resolve plausible path literals.
/// Explicit rooted/prefixed paths always retain strict error handling.
fn configuration_path_literal(value: &str) -> bool {
    let path = Path::new(value);
    let drive_prefix = value.as_bytes().get(1) == Some(&b':')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    if path.has_root()
        || matches!(path.components().next(), Some(Component::Prefix(_)))
        || drive_prefix
        || value.starts_with("\\\\")
    {
        return true;
    }
    if value.contains("://")
        || value.chars().any(|character| {
            character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
    {
        return false;
    }
    !value.split(['/', '\\']).any(|component| {
        #[cfg(windows)]
        let length = component.encode_utf16().count();
        #[cfg(not(windows))]
        let length = component.len();
        length > 255
    })
}

fn configuration_references(
    root: &Path,
    relative_home: &Path,
    records: &[&CutexSessionRecord],
) -> Result<Vec<String>> {
    let native = root.join(relative_home);
    let config_path = relative_home.join("config.toml");
    ensure!(
        metadata(root, &config_path)?.is_some(),
        "native config missing"
    );
    let meta = fs::metadata(root.join(&config_path))?;
    ensure!(meta.len() <= MAX_JSON, "native config too large");
    let config: toml::Value = toml::from_str(&fs::read_to_string(root.join(&config_path))?)?;
    let mut values = Vec::new();
    strings(&serde_json::to_value(config)?, &mut values);
    // Runtime-package upgrades can pin an external release bundle instead of
    // native/bundle.json. Inspect the actual immutable contract evidence.
    for record in records {
        let contract = record
            .explicit_launch
            .as_ref()
            .context("native contract missing")?;
        observe(&contract.bundle_manifest)?;
        let meta = fs::symlink_metadata(&contract.bundle_manifest)?;
        ensure!(
            meta.is_file() && !linked(&meta) && meta.len() <= MAX_JSON,
            "bundle evidence unavailable"
        );
        let bytes = fs::read(&contract.bundle_manifest)?;
        ensure!(
            crate::role_revision::sha256_bytes(&bytes).as_str() == contract.bundle_sha256.as_str(),
            "bundle evidence changed"
        );
        let bundle: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            bundle["native_patch_commit"].as_str() == Some(AUDITED_NATIVE),
            "native cache writer not audited"
        );
        ensure!(
            bundle["version"]
                .as_u64()
                .is_some_and(|version| (1..=4).contains(&version)),
            "unknown bundle version"
        );
        strings(&bundle, &mut values);
    }
    let mut refs = Vec::new();
    for value in values {
        if value.trim().is_empty() {
            continue;
        }
        // A configured shell command may mention a relative filename inside
        // a larger string. Treat such uncertain references conservatively too.
        refs.push(format!("raw:{}", value.replace('\\', "/")));
        if !configuration_path_literal(&value) {
            continue;
        }
        let path = Path::new(&value);
        refs.push(path_key(&if path.is_absolute() {
            path.to_owned()
        } else {
            native.join(path)
        })?);
    }
    Ok(refs)
}

fn retired(
    root: &Path,
    relative: &Path,
    records: &[&CutexSessionRecord],
    refs: &References,
    selected: Option<&Path>,
) -> Result<Vec<Candidate>> {
    let key = path_key(&root.join(relative))?;
    let id = relative
        .file_name()
        .and_then(|id| id.to_str())
        .context("home identity")?;
    let bound_home = records.iter().all(|record| {
        record
            .explicit_launch
            .as_ref()
            .filter(|contract| {
                contract.native_id == id && record.codex_session_id.as_deref() == Some(id)
            })
            .and_then(|contract| path_key(&contract.native_home).ok())
            .is_some_and(|home| home == key)
    });
    let block = if records.is_empty() {
        Some(("not_retired", "No retired session owns this home"))
    } else if records.iter().any(|record| {
        record.claude_session.is_some()
            || !crate::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &crate::platform::host::current_host_name(),
            )
            || !matches!(
                record.runtime_backend,
                crate::session::model::CutexSessionRuntimeBackend::Host
                    | crate::session::model::CutexSessionRuntimeBackend::HostForeground
            )
    }) {
        Some((
            "not_local_native",
            "Only retired local native host homes may be cleaned",
        ))
    } else if !bound_home {
        Some((
            "native_home_unknown",
            "Retired identity does not prove this exact explicit native home",
        ))
    } else if records.iter().any(|r| !r.is_retired()) {
        Some(("session_active", "An active session still owns this home"))
    } else if records.iter().any(|r| {
        r.pending_launch_id.is_some() || crate::session::archive::record_has_runtime_claim(r)
    }) {
        Some((
            "runtime_claim",
            "A runtime or launch claim still owns this home",
        ))
    } else if registered_runtime(root, records, id)? {
        Some((
            "runtime_registered",
            "Runtime registration or online intent still references this session",
        ))
    } else if other_reference(refs, &key) {
        Some((
            "history_source",
            "Another native home or selected deployment references this home",
        ))
    } else if obligations(root, records, id)? {
        Some((
            "pending_obligation",
            "An open delegation, workflow or approval references this session",
        ))
    } else {
        None
    };
    if let Some((code, reason)) = block {
        return Ok(vec![decision(relative, code, reason)]);
    }
    let config_refs = configuration_references(root, relative, records)?;
    let mut candidates = Vec::new();
    let mut pending = vec![selected.unwrap_or(relative).to_owned()];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        visited += 1;
        ensure!(visited <= MAX_ENTRIES, "home inventory limit exceeded");
        let meta = fs::symlink_metadata(root.join(&path))?;
        if linked(&meta) {
            continue;
        }
        if meta.is_dir() {
            for entry in fs::read_dir(root.join(&path))? {
                pending.push(path.join(entry?.file_name()));
            }
        } else if meta.is_file() && regenerable(path.strip_prefix(relative)?) {
            let key = path_key(&root.join(&path))?;
            let leaf = path
                .strip_prefix(relative)?
                .to_string_lossy()
                .replace('\\', "/");
            let name = path.file_name().unwrap().to_string_lossy();
            let referenced =
                config_refs
                    .iter()
                    .any(|reference| match reference.strip_prefix("raw:") {
                        Some(raw) => raw.contains(&leaf) || raw.contains(name.as_ref()),
                        None => inside(&key, reference),
                    });
            candidates.push(if referenced { decision(&path, "configured_input", "Native config or bundle explicitly references this file") }
                else { decision(&path, "eligible", "Regenerable cache/snapshot or exact disposable native diagnostic log; conversation and restore inputs remain") });
        }
    }
    if candidates.is_empty() {
        candidates.push(decision(relative, "no_regenerable_files", "No proven regenerable leaves; all conversation, database, asset and unknown paths are preserved"));
    }
    Ok(candidates)
}

pub(super) fn candidates(home: &Path, action: &str, now: SystemTime) -> Result<Vec<Candidate>> {
    ensure!(
        matches!(action, "orphan-homes" | "retired-homes"),
        "unknown home cleanup action"
    );
    let root = home.canonicalize()?;
    let Some(meta) = metadata(&root, Path::new(PREFIX))? else {
        return Ok(Vec::new());
    };
    ensure!(meta.is_dir(), "native agents root is not a directory");
    let mut homes = Vec::new();
    for entry in fs::read_dir(root.join(PREFIX))? {
        homes.push(Path::new(PREFIX).join(entry?.file_name()));
    }
    homes.sort();
    let reference_state = (|| -> Result<_> {
        metadata(&root, Path::new("cutex-sessions.json"))?;
        metadata(&root, Path::new("cutex-sessions.sqlite3"))?;
        let sessions = crate::session::store::load_cutex_session_store_from_path(
            &root.join("cutex-sessions.json"),
        )?;
        let refs = references(&root, &homes, &sessions)?;
        Ok((sessions, refs))
    })();
    let mut out = Vec::new();
    for relative in homes {
        let name = relative.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !uuid::Uuid::parse_str(name).is_ok_and(|id| id.to_string() == name) {
            out.push(decision(
                &relative,
                "not_uuid",
                "Templates and non-canonical native UUID directories are preserved",
            ));
            continue;
        }
        if metadata(&root, &relative)
            .ok()
            .flatten()
            .is_none_or(|meta| !meta.is_dir())
        {
            out.push(decision(
                &relative,
                "unreadable",
                "Linked, unreadable or non-directory native home is preserved",
            ));
            continue;
        }
        let classify = (|| -> Result<Vec<Candidate>> {
            let (sessions, refs) = reference_state
                .as_ref()
                .map_err(|_| anyhow::anyhow!("reference state unavailable"))?;
            let key = path_key(&root.join(&relative))?;
            if refs.pending_creates.contains(name) {
                return Ok(vec![decision(
                    &relative,
                    "unfinished_create",
                    "An unfinished Agent Management create still owns this native identity",
                )]);
            }
            let mut owners = Vec::new();
            for record in sessions.sessions.values() {
                if owns(record, &key, name)? {
                    owners.push(record);
                }
            }
            if action == "retired-homes" {
                return retired(&root, &relative, &owners, refs, None);
            }
            let result = if !owners.is_empty() {
                decision(
                    &relative,
                    "session_reference",
                    "An active or retired session identity still references this home",
                )
            } else if registered_runtime(&root, &owners, name)? {
                decision(
                    &relative,
                    "runtime_registered",
                    "A runtime registration or online intent still references this native identity",
                )
            } else if other_reference(refs, &key) {
                decision(
                    &relative,
                    "history_source",
                    "Another adoption, recovery chain or selected deployment references this home",
                )
            } else if now
                .duration_since(newest(&root.join(&relative))?)
                .unwrap_or_default()
                < Duration::from_secs(7 * 86_400)
            {
                decision(
                    &relative,
                    "too_recent",
                    "Home or a descendant was modified within seven days",
                )
            } else {
                decision(
                    &relative,
                    "eligible",
                    "Unreferenced canonical native UUID home; entire tree older than seven days",
                )
            };
            Ok(vec![result])
        })();
        match classify {
            Ok(candidates) => out.extend(candidates),
            Err(_) => out.push(decision(&relative, "reference_state_unavailable", "Unknown/unreadable session, provenance, config or obligation state; nothing in this home may be cleaned")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER: &str = "22222222-2222-4222-8222-222222222222";

    struct Fixture {
        root: PathBuf,
        relative: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("cleanup-homes-{}", uuid::Uuid::new_v4()));
            let fixture = Self {
                root,
                relative: Path::new(PREFIX).join(ID),
            };
            fixture.file("config.toml", b"model='synthetic'\n");
            fixture.file(
                "bundle.json",
                &serde_json::to_vec(&json!({"version":4,"native_patch_commit":AUDITED_NATIVE}))
                    .unwrap(),
            );
            fixture.file("models_cache.json", b"synthetic cache");
            fixture
        }
        fn native(&self) -> PathBuf {
            self.root.join(&self.relative)
        }
        fn file(&self, relative: &str, bytes: &[u8]) {
            let path = self.native().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fn state_file(&self, relative: &str, value: Value) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        fn record(&self, retired: bool) -> CutexSessionRecord {
            let mut record = CutexSessionRecord::new(
                "cutex.33333333-3333-4333-8333-333333333333".into(),
                Some(ID.into()),
                crate::platform::host::current_host_name(),
                self.root.to_string_lossy().into(),
                None,
            )
            .unwrap();
            record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
                version: 4,
                migration_action_id: None,
                native_id: ID.into(),
                native_home: self.native(),
                bundle_manifest: self.native().join("bundle.json"),
                bundle_sha256: crate::role_revision::sha256_bytes(
                    &fs::read(self.native().join("bundle.json")).unwrap(),
                ),
            });
            if retired {
                record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
            }
            record
        }
        fn save(&self, record: CutexSessionRecord) {
            let path = self.root.join("cutex-sessions.json");
            crate::session::store::with_locked_session_store(&path, |state| {
                state
                    .sessions
                    .insert(record.cutex_session_id.clone(), record);
                crate::session::store::save_locked_session_store(&path, state)
            })
            .unwrap();
        }
        fn classified(&self, action: &str, after_days: u64) -> Vec<Candidate> {
            candidates(
                &self.root,
                action,
                SystemTime::now() + Duration::from_secs(after_days * 86_400),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn prepared_leaf_recheck_rejects_changed_config_and_expired_deadline() {
        let f = Fixture::new();
        f.save(f.record(true));
        // Prime the session reader before recording its stable SQLite family.
        let _ = f.classified("retired-homes", 0);
        let relative = f.relative.join("models_cache.json");
        let ticket =
            prepare_recheck(&f.root, "retired-homes", &relative, SystemTime::now()).unwrap();
        assert!(
            ticket.candidate.eligible,
            "{}: {}",
            ticket.candidate.code, ticket.candidate.reason
        );
        assert!(recheck(
            &f.root,
            "retired-homes",
            &relative,
            &ticket,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
        assert!(!recheck(&f.root, "retired-homes", &relative, &ticket, Instant::now()).unwrap());
        f.file(
            "config.toml",
            b"model='synthetic'\ninput='models_cache.json'\n",
        );
        assert!(!recheck(
            &f.root,
            "retired-homes",
            &relative,
            &ticket,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
    }

    #[test]
    fn prepared_home_recheck_fences_new_cross_home_provenance() {
        let f = Fixture::new();
        // Session-store creation is permitted during the outside-lock read;
        // prepare again once its physical evidence has settled.
        let _ = f.classified("orphan-homes", 8);
        let ticket = prepare_recheck(
            &f.root,
            "orphan-homes",
            &f.relative,
            SystemTime::now() + Duration::from_secs(8 * 86_400),
        )
        .unwrap();
        assert!(
            ticket.candidate.eligible,
            "{}: {}",
            ticket.candidate.code, ticket.candidate.reason
        );
        f.state_file(
            &format!("{PREFIX}/{OTHER}/adoption.json"),
            json!({"native_id":OTHER,"history_home":f.native()}),
        );
        assert!(!recheck(
            &f.root,
            "orphan-homes",
            &f.relative,
            &ticket,
            Instant::now() + Duration::from_secs(1)
        )
        .unwrap());
    }

    #[test]
    fn a1_requires_old_uuid_home_and_no_session_native_identity_or_selection() {
        let f = Fixture::new();
        assert!(!f.classified("orphan-homes", 0)[0].eligible);
        assert!(f.classified("orphan-homes", 8)[0].eligible);
        f.state_file(
            "runtime/light/deployment.json",
            json!({"native_home":f.native()}),
        );
        assert!(!f.classified("orphan-homes", 8)[0].eligible);
        fs::remove_file(f.root.join("runtime/light/deployment.json")).unwrap();
        f.save(f.record(true));
        assert_eq!(f.classified("orphan-homes", 8)[0].code, "session_reference");
        fs::create_dir_all(f.root.join(PREFIX).join("template")).unwrap();
        assert!(f
            .classified("orphan-homes", 8)
            .iter()
            .any(|row| row.code == "not_uuid" && !row.eligible));
    }

    #[test]
    fn a1_adoption_dependency_and_unknown_provenance_preserve_source() {
        let f = Fixture::new();
        let other = Path::new(PREFIX).join(OTHER).join("adoption.json");
        f.state_file(
            other.to_str().unwrap(),
            json!({"native_id":OTHER,"history_home":f.native()}),
        );
        assert_eq!(f.classified("orphan-homes", 8)[0].code, "history_source");
        f.state_file(
            other.to_str().unwrap(),
            json!({"native_id":OTHER,"new_dependency_format":{"home":f.native()}}),
        );
        assert!(f
            .classified("orphan-homes", 8)
            .iter()
            .all(|row| !row.eligible));
    }

    #[test]
    fn a2_positive_leaves_only_preserve_all_conversation_database_and_unknown_files() {
        let f = Fixture::new();
        f.save(f.record(true));
        let snapshot = format!("shell_snapshots/{ID}.123456.sh");
        f.file(&snapshot, b"synthetic generated snapshot");
        f.file("log/codex-tui.log", b"synthetic native log");
        let preserved = [
            "sessions/rollout-a.jsonl",
            "archived_sessions/rollout-parent.jsonl",
            "state_5.sqlite",
            "state_5.sqlite-wal",
            "thread_history_1.sqlite",
            "thread_history_1.sqlite-shm",
            "logs_2.sqlite",
            "logs_2.sqlite-wal",
            "logs_2.sqlite-shm",
            "auth.json",
            "checkpoints/new.json",
            "skills/custom.md",
            "cache/new.bin",
            "tmp/new.bin",
            "shell_snapshots/custom.sh",
            "log/user.log",
        ];
        for path in preserved {
            f.file(path, b"synthetic bytes must stay");
        }
        let actual: BTreeSet<_> = f
            .classified("retired-homes", 0)
            .into_iter()
            .filter(|row| row.eligible)
            .map(|row| row.relative)
            .collect();
        assert_eq!(
            actual,
            BTreeSet::from([
                f.relative.join("models_cache.json"),
                f.relative.join(&snapshot),
                f.relative.join("log/codex-tui.log")
            ])
        );
        for path in preserved {
            assert_eq!(
                fs::read(f.native().join(path)).unwrap(),
                b"synthetic bytes must stay"
            );
        }
    }

    #[test]
    fn a2_runtime_claim_registration_intent_and_approved_obligation_block_cleanup() {
        let f = Fixture::new();
        let mut record = f.record(true);
        record.pending_launch_id = Some("synthetic-launch".into());
        f.save(record);
        assert_eq!(f.classified("retired-homes", 0)[0].code, "runtime_claim");
        f.save(f.record(true));
        f.state_file(
            "runtime/agent-bus-registry.json",
            json!({"agents":{"synthetic":{"session_id":ID}}}),
        );
        assert_eq!(
            f.classified("retired-homes", 0)[0].code,
            "runtime_registered"
        );
        f.state_file("runtime/agent-bus-registry.json", json!({"agents":{}}));
        let session_id = f.record(true).cutex_session_id;
        f.state_file(
            "runtime/runtime-intent/online.json",
            json!({"version":1,"intents":{session_id:{"lastOnlineAt":"synthetic"}}}),
        );
        assert_eq!(
            f.classified("retired-homes", 0)[0].code,
            "runtime_registered"
        );
        f.state_file(
            "runtime/runtime-intent/online.json",
            json!({"version":1,"intents":{}}),
        );
        f.state_file("runtime/approvals/approvals.json", json!({"approvals":[{"state":"approved","requesterCutexSessionId":f.record(true).cutex_session_id}]}));
        assert_eq!(
            f.classified("retired-homes", 0)[0].code,
            "pending_obligation"
        );
    }

    #[test]
    fn a2_realistic_provider_urls_and_instructions_keep_raw_file_references() {
        let f = Fixture::new();
        f.file(
            "bundle.json",
            &serde_json::to_vec(&json!({
                "version":4, "native_patch_commit":AUDITED_NATIVE,
                "provider":{"base_url":"https://provider.example.invalid/v1?route=tools"},
                "instructions":"Use <tools> carefully.\nPreserve user data.",
                "description":"x".repeat(300)
            }))
            .unwrap(),
        );
        f.save(f.record(true));
        for references_cache in [false, true] {
            let instruction = if references_cache {
                "Inspect <models_cache.json> before using tools."
            } else {
                "Use <tools> carefully and preserve user data."
            };
            f.file(
                "config.toml",
                format!(
                    r#"model_provider = "synthetic"
instructions = """
{instruction}
Use the configured provider, then report results.
"""
[model_providers.synthetic]
name = "Synthetic Provider"
base_url = "https://provider.example.invalid/v1?route=tools"
wire_api = "responses"
"#
                )
                .as_bytes(),
            );
            let rows = f.classified("retired-homes", 0);
            let cache = rows
                .iter()
                .find(|row| row.relative == f.relative.join("models_cache.json"))
                .unwrap();
            assert_eq!(
                cache.eligible, !references_cache,
                "{}: {}",
                cache.code, cache.reason
            );
            if references_cache {
                assert_eq!(cache.code, "configured_input");
            }
        }
    }

    #[test]
    fn a2_configured_cache_and_unreadable_reference_state_are_never_eligible() {
        let f = Fixture::new();
        f.save(f.record(true));
        f.file("config.toml", b"model_catalog_json='models_cache.json'\n");
        assert_eq!(f.classified("retired-homes", 0)[0].code, "configured_input");
        f.state_file(
            "runtime/approvals/approvals.json",
            json!({"approvals":[{"state":"future"}]}),
        );
        assert!(f
            .classified("retired-homes", 0)
            .iter()
            .all(|row| !row.eligible));
        f.state_file("runtime/approvals/approvals.json", json!({"approvals":[]}));
        f.file("config.toml", b"[invalid config");
        assert!(f
            .classified("retired-homes", 0)
            .iter()
            .all(|row| !row.eligible));
    }

    #[test]
    fn a1_recent_descendant_and_a2_open_ask_or_workflow_are_preserved() {
        let f = Fixture::new();
        let future = SystemTime::now() + Duration::from_secs(9 * 86_400);
        fs::File::options()
            .write(true)
            .open(f.native().join("models_cache.json"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(future))
            .unwrap();
        assert_eq!(f.classified("orphan-homes", 8)[0].code, "too_recent");
        for workflow in [false, true] {
            let f = Fixture::new();
            let record = f.record(true);
            let session_id = record.cutex_session_id.clone();
            f.save(record);
            if workflow {
                fs::create_dir_all(f.root.join("runtime/workflows")).unwrap();
                let db = Connection::open(f.root.join("runtime/workflows/runs.sqlite3")).unwrap();
                db.execute_batch("PRAGMA user_version=1; CREATE TABLE workflow_run(run_id TEXT,state TEXT,data TEXT); CREATE TABLE workflow_step(run_id TEXT,data TEXT);").unwrap();
                db.execute(
                    "INSERT INTO workflow_run VALUES('run','running',?1)",
                    [json!({"authorSession":session_id}).to_string()],
                )
                .unwrap();
            } else {
                let store = crate::asks::store::AskStore::at_runtime(f.root.join("runtime"));
                let mut ask =
                    crate::asks::store::tests::new_ask("protect-retired", "synthetic work");
                ask.target_cutex_session_id = session_id;
                store
                    .with_state(|state| state.create_ask(ask, chrono::Utc::now()))
                    .unwrap()
                    .unwrap();
            }
            assert_eq!(
                f.classified("retired-homes", 0)[0].code,
                "pending_obligation"
            );
        }
    }

    #[test]
    fn active_native_scope_is_preserved_and_explicit_bundle_refs_fail_closed() {
        let f = Fixture::new();
        let record = f.record(false);
        f.save(record);
        // Missing rollout/index evidence protects the entire active native
        // scope. Recovery ancestry cannot escape it, so an unrelated old home
        // remains eligible without parsing the broken active history.
        let other = f.root.join(PREFIX).join(OTHER);
        fs::create_dir_all(&other).unwrap();
        let rows = f.classified("orphan-homes", 8);
        assert!(
            !rows
                .iter()
                .find(|row| row.relative == f.relative)
                .unwrap()
                .eligible
        );
        assert!(
            rows.iter()
                .find(|row| row.relative == Path::new(PREFIX).join(OTHER))
                .unwrap()
                .eligible
        );
        f.save(f.record(true));
        // Corrupt pinned bundle evidence still blocks the retired home's leaves.
        f.file("bundle.json", b"invalid bundle evidence");
        assert!(f
            .classified("retired-homes", 0)
            .iter()
            .filter(|row| row.relative.starts_with(&f.relative))
            .all(|row| !row.eligible));
        // A configuration/bundle path reference protects a cache even when it
        // is structurally a known regenerable name.
        f.file(
            "bundle.json",
            &serde_json::to_vec(
                &json!({"version":4,"native_patch_commit":AUDITED_NATIVE,"schema":{"path":f.native().join("models_cache.json")}}),
            )
            .unwrap(),
        );
        f.save(f.record(true));
        assert_eq!(f.classified("retired-homes", 0)[0].code, "configured_input");
    }

    #[test]
    fn nested_snapshot_link_is_never_followed_or_offered() {
        let f = Fixture::new();
        f.save(f.record(true));
        let outside = Fixture::new();
        let name = format!("{ID}.123456.sh");
        outside.file(&name, b"synthetic outside snapshot");
        let link = f.native().join("shell_snapshots");
        #[cfg(windows)]
        {
            let result = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &link.to_string_lossy().replace('/', "\\"),
                    &outside.native().to_string_lossy().replace('/', "\\"),
                ])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.native(), &link).unwrap();
        let rows = f.classified("retired-homes", 0);
        assert!(rows
            .iter()
            .filter(|row| row.eligible)
            .all(|row| row.relative == f.relative.join("models_cache.json")));
        assert_eq!(
            fs::read(outside.native().join(name)).unwrap(),
            b"synthetic outside snapshot"
        );
        #[cfg(windows)]
        fs::remove_dir(link).unwrap();
        #[cfg(unix)]
        fs::remove_file(link).unwrap();
    }

    #[test]
    fn a2_remote_host_and_unaudited_native_pins_are_preserved() {
        let f = Fixture::new();
        let mut record = f.record(true);
        record.host_id = "remote-cleanup-fixture.invalid".into();
        f.save(record);
        assert_eq!(f.classified("retired-homes", 0)[0].code, "not_local_native");
        f.file(
            "bundle.json",
            &serde_json::to_vec(&json!({"version":4,"native_patch_commit":"future-runtime"}))
                .unwrap(),
        );
        f.save(f.record(true));
        assert!(f
            .classified("retired-homes", 0)
            .iter()
            .all(|row| !row.eligible));
    }
    #[test]
    fn unfinished_create_protects_native_home_without_a_session_record() {
        use crate::agent_management::{AgentActionRecord, AgentManagementSnapshot};
        let f = Fixture::new();
        fs::create_dir_all(f.root.join("runtime/agent-management/v1")).unwrap();
        fs::write(
            f.root
                .join("runtime/agent-management/v1/agent-management-v1.lock"),
            b"",
        )
        .unwrap();
        for phase in [
            "prepared",
            "private_cwd_ready",
            "native_bootstrap_pending",
            "native_session_captured",
            "adopted",
            "configured",
            "online",
            "ready",
            "message_pending",
            "message_queued",
            "predecessor_closing",
            "predecessor_closed",
            "authority_transfer_pending",
            "authority_transferred",
            "successor_ready",
            "owner_action_required",
            "failure",
            "complete",
            "no_write",
        ] {
            let mut state = AgentManagementSnapshot::empty();
            let action: AgentActionRecord = serde_json::from_value(json!({
                "action_id":"synthetic-create", "request_sha256":"0".repeat(64), "operation":"create",
                "project_id":"synthetic-project", "caller_cutex_session":"cutex.33333333-3333-4333-8333-333333333333",
                "phase":phase, "known_native_session_id":ID,
                "created_at":"2026-10-24T00:00:00Z", "updated_at":"2026-10-24T00:00:00Z"
            })).unwrap();
            state.actions.insert(action.action_id.clone(), action);
            f.state_file(
                "runtime/agent-management/v1/agent-management-v1.json",
                serde_json::to_value(state).unwrap(),
            );
            let rows = f.classified("orphan-homes", 8);
            assert_eq!(rows.len(), 1);
            if matches!(phase, "complete" | "no_write") {
                assert!(rows[0].eligible, "{phase}");
            } else {
                assert_eq!(rows[0].code, "unfinished_create", "{phase}");
            }
        }
    }

    #[test]
    fn corrupt_action_store_never_authorizes_any_home_cleanup() {
        let f = Fixture::new();
        f.save(f.record(true));
        fs::create_dir_all(f.root.join("runtime/agent-management/v1")).unwrap();
        fs::write(
            f.root
                .join("runtime/agent-management/v1/agent-management-v1.lock"),
            b"",
        )
        .unwrap();
        f.state_file(
            "runtime/agent-management/v1/agent-management-v1.json",
            json!({"actions":"unreadable schema"}),
        );
        for missing_lock in [false, true] {
            if missing_lock {
                // A valid document with a missing lock must not become the
                // store API's synthetic empty snapshot during cleanup.
                f.state_file(
                    "runtime/agent-management/v1/agent-management-v1.json",
                    serde_json::to_value(crate::agent_management::AgentManagementSnapshot::empty())
                        .unwrap(),
                );
                fs::remove_file(
                    f.root
                        .join("runtime/agent-management/v1/agent-management-v1.lock"),
                )
                .unwrap();
            }
            for action in ["orphan-homes", "retired-homes"] {
                let rows = f.classified(action, 8);
                assert!(rows
                    .iter()
                    .all(|row| !row.eligible && row.code == "reference_state_unavailable"));
            }
        }
    }

    #[test]
    fn missing_reference_tail_resolves_its_existing_junction_or_symlink_prefix() {
        let f = Fixture::new();
        let alias = f.root.join("native-alias");
        #[cfg(windows)]
        {
            let result = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &alias.to_string_lossy().replace('/', "\\"),
                    &f.native().to_string_lossy().replace('/', "\\"),
                ])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(f.native(), &alias).unwrap();
        let missing = alias.join("missing/rollout-original.jsonl");
        assert_eq!(
            path_key(&missing).unwrap(),
            path_key(&f.native().join("missing/rollout-original.jsonl")).unwrap()
        );
        f.state_file(
            "runtime/light/deployment.json",
            json!({"native_home":missing}),
        );
        assert_eq!(f.classified("orphan-homes", 8)[0].code, "history_source");
        #[cfg(windows)]
        fs::remove_dir(alias).unwrap();
        #[cfg(unix)]
        fs::remove_file(alias).unwrap();
    }

    #[test]
    fn busy_asks_writer_does_not_deadlock_lifecycle_cleanup() {
        let f = Fixture::new();
        f.save(f.record(true));
        crate::asks::store::AskStore::at_runtime(f.root.join("runtime"))
            .with_state(|state| state.next_seq = state.next_seq.saturating_add(1))
            .unwrap();
        assert!(f
            .classified("retired-homes", 0)
            .iter()
            .any(|row| row.eligible));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.root.join("runtime/asks/asks-v1.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let started = std::time::Instant::now();
        let rows = f.classified("retired-homes", 0);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(rows.iter().all(|row| !row.eligible));
    }
}
