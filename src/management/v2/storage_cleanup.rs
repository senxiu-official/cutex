//! Explicit, preview-bound owner cleanup. A worker resumes only durable,
//! already-consented runs; it never invents periodic cleanup or statistics work.
use super::{storage_cleanup_fs as files, storage_cleanup_state as state};
use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreviewRequest {
    action: String,
    items: Option<Vec<String>>,
    older_than_days: Option<u32>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecuteRequest {
    request_id: String,
    preview_id: String,
    items: Option<Vec<String>>,
    confirmed: bool,
    confirm_text: Option<String>,
}

pub(super) struct Candidate {
    pub relative: PathBuf,
    pub eligible: bool,
    pub code: String,
    pub reason: String,
}

pub(super) fn is_path(path: &str) -> bool {
    path == "/v2/storage/cleanups" || path.starts_with("/v2/storage/cleanups/")
}

pub(super) fn handle(
    stream: &mut std::net::TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    use crate::http::server::write_json_response;
    let path = request.path.split('?').next().unwrap_or(&request.path);
    if request.method != "GET" && !super::storage_cleanup_access::cleanup_allowed() {
        return super::server::write_v2_error(
            stream,
            403,
            "Forbidden",
            "storage_cleanup_desktop_only",
            "storage cleanup requires the owner's local or SSH desktop credential",
            false,
            json!({}),
        );
    }
    let home = crate::config::paths::config_dir()?;
    let _storage_guard = files::pin_storage(&home)?;
    let result = (|| -> anyhow::Result<(u16, Value)> {
        match (request.method.as_str(), path) {
            ("GET", "/v2/storage/cleanups") => Ok((
                200,
                json!({"items":state::history(&home,false)?.into_iter().map(public_run).collect::<Vec<_>>()}),
            )),
            ("GET", path) if path.starts_with("/v2/storage/cleanups/") => {
                let id = path.trim_start_matches("/v2/storage/cleanups/");
                uuid(id)?;
                Ok((
                    200,
                    public_run(state::run(&home, id)?.context("cleanup run not found")?),
                ))
            }
            ("POST", "/v2/storage/cleanups/preview") => Ok((
                200,
                preview(
                    &home,
                    serde_json::from_slice(&request.body)?,
                    serde_json::to_value(
                        super::storage_cleanup_access::cleanup_actor()
                            .context("desktop credential required")?,
                    )?,
                )?,
            )),
            ("POST", "/v2/storage/cleanups") => Ok((
                202,
                execute(
                    &home,
                    serde_json::from_slice(&request.body)?,
                    serde_json::to_value(
                        super::storage_cleanup_access::cleanup_actor()
                            .context("desktop credential required")?,
                    )?,
                )?,
            )),
            _ => anyhow::bail!("cleanup route not found"),
        }
    })();
    match result {
        Ok((status, value)) => write_json_response(
            stream,
            status,
            if status == 202 { "Accepted" } else { "OK" },
            &value,
        ),
        Err(error) => {
            let message = error.to_string();
            let (status, code, reason) = if message.contains("not found") {
                (404, "not_found", "Not Found")
            } else if message.contains("expired")
                || message.contains("already used")
                || message.contains("different desktop")
                || message.contains("preview missing")
            {
                (409, "cleanup_preview_changed", "Conflict")
            } else if error.downcast_ref::<rusqlite::Error>().is_some()
                || error.downcast_ref::<std::io::Error>().is_some()
            {
                (503, "storage_cleanup_unavailable", "Service Unavailable")
            } else {
                (400, "invalid_request", "Bad Request")
            };
            super::server::write_v2_error(stream, status, reason, code, &message, false, json!({}))
        }
    }
}

fn uuid(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        uuid::Uuid::parse_str(value)?.to_string() == value,
        "canonical UUID required"
    );
    Ok(())
}
fn days(request: &PreviewRequest) -> anyhow::Result<u32> {
    let value = request
        .older_than_days
        .unwrap_or(match request.action.as_str() {
            "orphan-homes" => 7,
            "old-backups" => 14,
            _ => 30,
        });
    anyhow::ensure!(
        match request.action.as_str() {
            "orphan-homes" => value == 7,
            "old-backups" => value == 14,
            "old-logs" => [7, 30, 90].contains(&value),
            "retired-homes" | "vacuum" => request.older_than_days.is_none(),
            _ => false,
        },
        "invalid action or retention window"
    );
    Ok(value)
}

fn candidates(
    home: &Path,
    request: &PreviewRequest,
    now: SystemTime,
    deadline: Option<Instant>,
) -> anyhow::Result<(Vec<Candidate>, Option<&'static str>)> {
    let days = days(request)?;
    match request.action.as_str() {
        "orphan-homes" | "retired-homes" => Ok((
            super::storage_cleanup_homes::candidates(home, &request.action, now)?,
            None,
        )),
        "old-backups" | "old-logs" => {
            let store = crate::session::store::load_cutex_session_store_from_path(
                &home.join("cutex-sessions.json"),
            )?;
            let mut protected = Vec::new();
            for record in store.sessions.values() {
                if let Some(binding) = &record.app_server_runtime {
                    protected.push(PathBuf::from(&binding.diagnostic_journal_path));
                }
            }
            if store.sessions.values().any(|record| {
                crate::session::archive::record_has_runtime_claim(record)
                    || record.pending_launch_id.is_some()
            }) {
                protected.push(home.join("runtime/diagnostics"));
            }
            let discovered = super::storage_cleanup_catalog::candidates_with_deadline(
                home,
                &request.action,
                days,
                now,
                crate::build_info::RELEASE,
                &protected,
                deadline,
            )?;
            Ok((
                discovered
                    .items
                    .into_iter()
                    .map(|c| Candidate {
                        relative: c.relative,
                        eligible: c.eligible,
                        code: c.code,
                        reason: c.reason,
                    })
                    .collect(),
                discovered.partial_reason,
            ))
        }
        "vacuum" => {
            let store = crate::session::store::load_cutex_session_store_from_path(
                &home.join("cutex-sessions.json"),
            )?;
            let idle = store.sessions.values().all(|record| {
                !crate::session::archive::record_has_runtime_claim(record)
                    && record.pending_launch_id.is_none()
            });
            Ok((
                super::storage_cleanup_vacuum::DATABASES
                    .iter()
                    .filter(|(_, path)| home.join(path).is_file())
                    .map(|(_, path)| Candidate {
                        relative: PathBuf::from(path),
                        eligible: idle,
                        code: if idle { "compactable" } else { "database_busy" }.into(),
                        reason: if idle {
                            "Manual SQLite compaction; database must be idle"
                        } else {
                            "Managed runtime or launch is present; stop runtimes before compacting"
                        }
                        .into(),
                    })
                    .collect(),
                None,
            ))
        }
        _ => anyhow::bail!("unknown cleanup action"),
    }
}

fn sqlite_snapshot(home: &Path, relative: &Path) -> anyhow::Result<Value> {
    let mut snapshot = serde_json::Map::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = PathBuf::from(format!("{}{suffix}", relative.display()));
        match files::fingerprint(home, &path) {
            Ok(value) => {
                anyhow::ensure!(value.is_regular_file(), "SQLite file or sidecar is linked");
                snapshot.insert(suffix.into(), serde_json::to_value(value)?);
            }
            Err(error)
                if !suffix.is_empty()
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                snapshot.insert(suffix.into(), Value::Null);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Value::Object(snapshot))
}

pub(super) fn preview(home: &Path, body: Value, actor: Value) -> anyhow::Result<Value> {
    preview_with_limits(home, body, actor, Duration::from_secs(5), 10_000)
}
fn preview_with_limits(
    home: &Path,
    body: Value,
    actor: Value,
    budget: Duration,
    item_limit: usize,
) -> anyhow::Result<Value> {
    let started = Instant::now();
    let canonical = home.canonicalize()?;
    let home = canonical.as_path();
    let _storage_guard = files::pin_storage(home)?;
    let request: PreviewRequest = serde_json::from_value(body.clone())?;
    anyhow::ensure!(
        request
            .items
            .as_ref()
            .is_none_or(|items| !items.is_empty() && items.len() <= 10_000),
        "preview selection must contain 1 to 10000 paths"
    );
    days(&request)?;
    let requested_paths = request
        .items
        .as_ref()
        .map(|paths| paths.iter().map(String::as_str).collect::<HashSet<_>>());
    anyhow::ensure!(
        requested_paths
            .as_ref()
            .is_none_or(|paths| Some(paths.len()) == request.items.as_ref().map(Vec::len)),
        "duplicate paths"
    );
    let deadline = (request.action == "old-logs").then_some(started + budget);
    let now = chrono::Utc::now();
    let mut items = Vec::new();
    let mut freeable_bytes = 0u64;
    let (discovered, mut partial_reason) = candidates(home, &request, now.into(), deadline)?;
    let mut inspector = if deadline.is_some() {
        Some(files::PreviewInspector::new(home)?)
    } else {
        None
    };
    let mut selected = HashSet::new();
    for candidate in discovered {
        let path = candidate.relative.to_string_lossy().replace('\\', "/");
        if requested_paths
            .as_ref()
            .is_some_and(|paths| !paths.contains(path.as_str()))
        {
            continue;
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            partial_reason = Some("time_budget");
            break;
        }
        if deadline.is_some() && items.len() >= item_limit {
            partial_reason = Some("item_limit");
            break;
        }
        selected.insert(path.clone());
        let snapshot = if let (Some(inspector), Some(end)) = (&mut inspector, deadline) {
            inspector.fingerprint(&candidate.relative, end)
        } else {
            files::fingerprint(home, &candidate.relative)
        };
        if snapshot
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<files::PreviewDeadline>())
        {
            partial_reason = Some("time_budget");
            break;
        }
        let (mut snapshot, mut eligible, mut reason, mut code) = match snapshot {
            Ok(snapshot) => (
                Some(snapshot),
                candidate.eligible,
                candidate.reason,
                candidate.code,
            ),
            Err(error) => (
                None,
                false,
                format!("Cannot safely inspect this path: {error}"),
                "unreadable".into(),
            ),
        };
        if let Some(snapshot) = &snapshot {
            if code != "manual_required"
                && matches!(
                    request.action.as_str(),
                    "orphan-homes" | "old-backups" | "old-logs"
                )
            {
                let now_ns = SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos();
                let age_days = if request.action == "old-logs"
                    && super::storage_cleanup_catalog::is_release_log(&candidate.relative)
                {
                    1
                } else {
                    days(&request)?
                };
                let required = u128::from(age_days) * 86_400 * 1_000_000_000;
                if now_ns.saturating_sub(snapshot.mtime_ns) <= required {
                    eligible = false;
                    code = "recent".into();
                    reason =
                        "A file or directory in this item was modified inside the retention window"
                            .into();
                }
            }
        }
        let bytes = snapshot.as_ref().map_or(0, |s| s.bytes);
        let reclaimable = if eligible && request.action == "vacuum" {
            match super::storage_cleanup_vacuum::reclaimable(home, &candidate.relative) {
                Ok(bytes) => bytes,
                Err(error) => {
                    eligible = false;
                    code = "database_busy".into();
                    reason = format!("Cannot inspect idle database: {error}");
                    0
                }
            }
        } else if eligible {
            bytes
        } else {
            0
        };
        // The short SQLite availability probe can update SHM bookkeeping;
        // capture all preview preconditions after it, not across it.
        if request.action == "vacuum" {
            match files::fingerprint(home, &candidate.relative) {
                Ok(value) => snapshot = Some(value),
                Err(error) => {
                    eligible = false;
                    code = "database_busy".into();
                    reason = format!("Database changed during probe: {error}");
                    snapshot = None;
                }
            }
        }
        let bytes = snapshot.as_ref().map_or(0, |s| s.bytes);
        let count = snapshot.as_ref().map_or(0, |s| s.files);
        let mtime = snapshot
            .as_ref()
            .and_then(|s| i64::try_from(s.mtime_ns / 1_000_000).ok())
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|time| time.to_rfc3339());
        let sidecars = if request.action == "vacuum" {
            match sqlite_snapshot(home, &candidate.relative) {
                Ok(value) => value,
                Err(error) => {
                    eligible = false;
                    code = "database_busy".into();
                    reason = format!("Cannot inspect SQLite files: {error}");
                    Value::Null
                }
            }
        } else {
            Value::Null
        };
        if eligible {
            freeable_bytes = freeable_bytes.saturating_add(reclaimable);
        }
        items.push(json!({"id":uuid::Uuid::new_v4().to_string(),"path":path,"bytes":bytes,"files":count,"mtime":mtime,"eligible":eligible,"reason":reason,"code":code,"fingerprint":snapshot,"sidecars":sidecars}));
    }
    if let Some(requested) = &request.items {
        anyhow::ensure!(
            partial_reason.is_some() || requested.iter().all(|path| selected.contains(path)),
            "unknown cleanup path"
        );
    }
    let mut value = json!({"previewId":uuid::Uuid::new_v4().to_string(),"expiresAt":(now+chrono::Duration::minutes(10)).to_rfc3339(),"action":request.action,"risk":if matches!(request.action.as_str(),"orphan-homes"|"retired-homes"|"old-backups"){"high"}else{"medium"},"freeableBytes":freeable_bytes,"inspectedItems":items.len(),"elapsedMs":started.elapsed().as_millis() as u64,"complete":partial_reason.is_none(),"items":items,"actor":actor,"request":body});
    if let Some(reason) = partial_reason {
        value["partialReason"] = json!(reason);
    }
    state::save_preview(
        home,
        &value,
        (now + chrono::Duration::minutes(10)).timestamp(),
    )?;
    Ok(public_preview(value))
}

fn public_preview(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("actor");
        object.remove("request");
    }
    if let Some(items) = value["items"].as_array_mut() {
        for item in items {
            item.as_object_mut().unwrap().remove("fingerprint");
            item.as_object_mut().unwrap().remove("sidecars");
        }
    }
    value
}
pub(super) fn public_run(mut value: Value) -> Value {
    for key in ["request", "previewRequest", "attempt"] {
        value.as_object_mut().unwrap().remove(key);
    }
    if let Some(items) = value["items"].as_array_mut() {
        for item in items {
            for key in [
                "fingerprint",
                "sidecars",
                "eligible",
                "bytes",
                "files",
                "mtime",
                "retryPhase",
            ] {
                item.as_object_mut().unwrap().remove(key);
            }
        }
    }
    value
}

pub(super) fn execute(home: &Path, body: Value, actor: Value) -> anyhow::Result<Value> {
    let canonical = home.canonicalize()?;
    let home = canonical.as_path();
    let _storage_guard = files::pin_storage(home)?;
    let request: ExecuteRequest = serde_json::from_value(body.clone())?;
    uuid(&request.request_id)?;
    uuid(&request.preview_id)?;
    anyhow::ensure!(request.confirmed, "confirmation required");
    // Serialize consent consumption across processes independently of the worker.
    let lock = operation_lock(home, "consent.lock")?;
    fs2::FileExt::lock_exclusive(&lock)?;
    if let Some(previous) = state::by_request(home, &request.request_id)? {
        anyhow::ensure!(
            previous["request"] == body && previous["actor"] == actor,
            "requestId already used for a different cleanup"
        );
        if previous["state"] == "running"
            || previous["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(retryable_item))
        {
            spawn_resume(home)?;
        }
        return Ok(public_run(previous));
    }
    anyhow::ensure!(
        state::by_preview(home, &request.preview_id)?.is_none(),
        "preview already used for a cleanup"
    );
    let preview = state::preview(home, &request.preview_id)?.context("cleanup preview missing")?;
    anyhow::ensure!(
        preview["actor"] == actor,
        "preview belongs to a different desktop credential"
    );
    let expires = chrono::DateTime::parse_from_rfc3339(
        preview["expiresAt"].as_str().context("preview expiry")?,
    )?;
    anyhow::ensure!(chrono::Utc::now() < expires, "cleanup preview expired");
    anyhow::ensure!(
        preview["risk"] != "high" || request.confirm_text.as_deref() == Some("删除"),
        "type 删除 to confirm this cleanup"
    );
    let mut items = preview["items"]
        .as_array()
        .context("preview items")?
        .clone();
    if let Some(selected) = &request.items {
        anyhow::ensure!(
            !selected.is_empty() && selected.iter().collect::<HashSet<_>>().len() == selected.len(),
            "empty or duplicate item selection"
        );
        anyhow::ensure!(
            selected
                .iter()
                .all(|id| items.iter().any(|item| item["id"] == *id)),
            "unknown preview item"
        );
        items.retain(|item| selected.iter().any(|id| item["id"] == *id));
    }
    anyhow::ensure!(!items.is_empty(), "cleanup preview has no items");
    for item in &mut items {
        item["state"] = json!(if item["eligible"] == true {
            "queued"
        } else {
            "skipped"
        });
        item["outcome"] = if item["eligible"] == true {
            Value::Null
        } else {
            json!("skipped")
        };
        item["freedBytes"] = json!(0);
        item["operation"] = preview["action"].clone();
    }
    let run = json!({"runId":uuid::Uuid::new_v4().to_string(),"requestId":request.request_id,"previewId":request.preview_id,"action":preview["action"],"state":"running","createdAt":chrono::Utc::now().to_rfc3339(),"finishedAt":null,"freedBytes":0,"actor":actor,"items":items,"request":body,"previewRequest":preview["request"]});
    state::begin(home, &run)?;
    publish(&run);
    spawn_resume(home)?;
    Ok(public_run(run))
}

fn operation_lock(home: &Path, name: &str) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(home.join("runtime/storage"))?;
    Ok(OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(home.join("runtime/storage").join(name))?)
}

/// Called once at server startup and after each accepted run. Waiting workers
/// drain durable running receipts, so neither restart nor a wakeup race loses work.
pub fn spawn_resume(home: &Path) -> anyhow::Result<()> {
    let home = home.canonicalize()?;
    std::thread::Builder::new()
        .name("cutex-storage-cleanup".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resume(&home)))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "cleanup worker panicked; durable receipts retained"
                    ))
                });
            if let Err(error) = result {
                eprintln!("storage cleanup paused; durable receipts retained: {error:#}");
                if let Ok(runs) = state::record_pause(&home, &format!("{error:#}")) {
                    for run in runs {
                        publish(&run);
                    }
                }
            }
        })?;
    Ok(())
}

pub fn resume_default() -> anyhow::Result<()> {
    let home = crate::config::paths::config_dir()?;
    if home.join("runtime/storage/storage.sqlite3").is_file() {
        spawn_resume(&home)?;
    }
    Ok(())
}

fn resume(home: &Path) -> anyhow::Result<()> {
    let canonical = home.canonicalize()?;
    let home = canonical.as_path();
    let _storage_guard = files::pin_storage(home)?;
    let lock = operation_lock(home, "worker.lock")?;
    fs2::FileExt::lock_exclusive(&lock)?;
    state::flush_audit(home)?;
    for mut run in state::history(home, true)? {
        run.as_object_mut()
            .context("cleanup receipt is not an object")?
            .remove("error");
        if run["state"] == "failed" {
            let next = run["attempt"]
                .as_u64()
                .unwrap_or(1)
                .checked_add(1)
                .context("cleanup attempt overflow")?;
            run["attempt"] = json!(next);
            run["state"] = json!("running");
            run["finishedAt"] = Value::Null;
            if let Some(items) = run["items"].as_array_mut() {
                for item in items {
                    if retryable_item(item) {
                        item["state"] = json!("deleting");
                        item["outcome"] = Value::Null;
                        item["reason"] = Value::Null;
                    }
                }
            }
            state::start_attempt(home, &run)?;
            publish(&run);
        }
        for index in 0..run["items"].as_array().context("run items")?.len() {
            if retryable_item(&run["items"][index]) {
                run["items"][index]["state"] = json!("deleting");
                run["items"][index]["outcome"] = Value::Null;
            }
            if matches!(
                run["items"][index]["state"].as_str(),
                Some("deleted" | "vacuumed" | "skipped" | "failed")
            ) {
                continue;
            }
            let outcome = process_item(home, &mut run, index);
            if let Err(error) = outcome {
                run["items"][index]["state"] = json!("failed");
                run["items"][index]["outcome"] = json!("failed");
                run["items"][index]["reason"] = json!(format!("{error:#}"));
                state::save_run(home, &run, false)?;
            }
            publish(&run);
        }
        run["freedBytes"] = json!(run["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["freedBytes"].as_u64())
            .sum::<u64>());
        run["state"] = json!(if run["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["state"] == "failed")
        {
            "failed"
        } else {
            "completed"
        });
        run["finishedAt"] = json!(chrono::Utc::now().to_rfc3339());
        state::save_run(home, &run, true)?;
        publish(&run);
        refresh_affected(home, &run)?;
    }
    Ok(())
}

fn changed(run: &mut Value, index: usize, reason: &str) {
    run["items"][index]["state"] = json!("skipped");
    run["items"][index]["outcome"] = json!("skipped");
    run["items"][index]["code"] = json!("changed");
    run["items"][index]["reason"] = json!(format!("changed: {reason}"));
    run["items"][index]["freedBytes"] = json!(0);
    remove_internal(&mut run["items"][index], "retryPhase");
}
fn retryable_item(item: &Value) -> bool {
    item["state"] == "failed" && item["retryPhase"] == "deleting"
}

#[cfg(test)]
thread_local! {static BEFORE_DELETE:std::cell::RefCell<Option<Box<dyn FnMut(&Path,&Path)->anyhow::Result<()>>>> = std::cell::RefCell::new(None);}
#[cfg(test)]
fn before_delete_hook(home: &Path, trash: &Path) -> anyhow::Result<()> {
    BEFORE_DELETE.with(|hook| {
        if let Some(callback) = hook.borrow_mut().as_mut() {
            callback(home, trash)
        } else {
            Ok(())
        }
    })
}

const LOCK_CHECK_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

struct SessionEvidence {
    stamps: Vec<(PathBuf, Option<crate::platform::file_stamp::FileStamp>)>,
    idle: bool,
    protected: Vec<PathBuf>,
}
impl SessionEvidence {
    fn stamps(
        home: &Path,
    ) -> anyhow::Result<Vec<(PathBuf, Option<crate::platform::file_stamp::FileStamp>)>> {
        let mut result = Vec::new();
        for name in [
            "cutex-sessions.lock",
            "cutex-sessions.json",
            "cutex-sessions.sqlite3",
            "cutex-sessions.sqlite3-wal",
            "cutex-sessions.sqlite3-shm",
            "cutex-sessions.sqlite3-journal",
        ] {
            let path = home.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) => {
                    anyhow::ensure!(
                        meta.is_file() && !meta.file_type().is_symlink(),
                        "invalid session evidence path"
                    );
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::MetadataExt;
                        anyhow::ensure!(
                            meta.file_attributes() & 0x400 == 0,
                            "linked session evidence"
                        );
                    }
                    let stamp = crate::platform::file_stamp::FileStamp::of(&path)
                        .context("session evidence changed")?;
                    result.push((path, Some(stamp)));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    result.push((path, None))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(result)
    }
    fn prepare(home: &Path) -> anyhow::Result<Self> {
        let before = Self::stamps(home)?;
        let store = crate::session::store::load_cutex_session_store_from_path(
            &home.join("cutex-sessions.json"),
        )?;
        let after = Self::stamps(home)?;
        anyhow::ensure!(
            before == after,
            "session evidence changed during preparation"
        );
        let idle = store.sessions.values().all(|record| {
            !crate::session::archive::record_has_runtime_claim(record)
                && record.pending_launch_id.is_none()
        });
        let mut protected = Vec::new();
        for record in store.sessions.values() {
            if let Some(binding) = &record.app_server_runtime {
                protected.push(PathBuf::from(&binding.diagnostic_journal_path));
            }
        }
        if !idle {
            protected.push(home.join("runtime/diagnostics"));
        }
        Ok(Self {
            stamps: after,
            idle,
            protected,
        })
    }
    fn unchanged(&self, home: &Path, deadline: std::time::Instant) -> anyhow::Result<bool> {
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        let current = Self::stamps(home)?;
        Ok(std::time::Instant::now() < deadline && current == self.stamps)
    }
}
enum PreparedEligibility {
    Home(super::storage_cleanup_homes::PreparedHomeRecheck),
    Catalog(super::storage_cleanup_catalog::PreparedCatalogRecheck),
}
impl PreparedEligibility {
    fn prepare(
        home: &Path,
        request: &PreviewRequest,
        relative: &Path,
        protected: &[PathBuf],
    ) -> anyhow::Result<Self> {
        match request.action.as_str() {
            "orphan-homes" | "retired-homes" => {
                Ok(Self::Home(super::storage_cleanup_homes::prepare_recheck(
                    home,
                    &request.action,
                    relative,
                    SystemTime::now(),
                )?))
            }
            "old-backups" | "old-logs" => Ok(Self::Catalog(
                super::storage_cleanup_catalog::prepare_recheck(
                    home,
                    &request.action,
                    relative,
                    days(request)?,
                    SystemTime::now(),
                    crate::build_info::RELEASE,
                    protected,
                )?,
            )),
            _ => anyhow::bail!("invalid rename action"),
        }
    }
    fn eligible(&self) -> bool {
        match self {
            Self::Home(ticket) => ticket.candidate.eligible,
            Self::Catalog(ticket) => ticket.candidate.eligible,
        }
    }
    fn unchanged(
        &self,
        home: &Path,
        request: &PreviewRequest,
        relative: &Path,
        deadline: std::time::Instant,
    ) -> anyhow::Result<bool> {
        match self {
            Self::Home(ticket) => super::storage_cleanup_homes::recheck(
                home,
                &request.action,
                relative,
                ticket,
                deadline,
            ),
            Self::Catalog(ticket) => {
                super::storage_cleanup_catalog::recheck_prepared(home, ticket, deadline)
            }
        }
    }
}
fn remove_internal(item: &mut Value, key: &str) {
    if let Some(object) = item.as_object_mut() {
        object.remove(key);
    }
}
fn move_changed(run: &mut Value, index: usize, reason: &str, retained: Option<PathBuf>) {
    changed(run, index, reason);
    if let Some(path) = retained {
        run["items"][index]["retainedPath"] = json!(path.to_string_lossy().replace('\\', "/"));
    } else {
        remove_internal(&mut run["items"][index], "retainedPath");
    }
}

fn with_cleanup_gate<T>(operation: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let _gate = crate::session::lifecycle_gate::lock()?;
    // Catch before the guard is dropped so a cleanup bug cannot poison the
    // shared lifecycle gate used by launches, retirement and restoration.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .map_err(|_| anyhow::anyhow!("cleanup metadata recheck panicked"))?
}
fn process_item(home: &Path, run: &mut Value, index: usize) -> anyhow::Result<()> {
    let relative = PathBuf::from(
        run["items"][index]["path"]
            .as_str()
            .context("cleanup path")?,
    );
    let expected: files::Fingerprint =
        serde_json::from_value(run["items"][index]["fingerprint"].clone())?;
    let request: PreviewRequest = serde_json::from_value(run["previewRequest"].clone())?;
    let item_state = run["items"][index]["state"]
        .as_str()
        .context("item state")?
        .to_owned();
    let run_id = run["runId"].as_str().context("run ID")?.to_owned();
    let item_id = run["items"][index]["id"]
        .as_str()
        .context("item ID")?
        .to_owned();
    let trash = files::trash_relative(&run_id, &item_id)?;

    if request.action == "vacuum" {
        let store_lock =
            crate::session::store::cleanup_lock_file(&home.join("cutex-sessions.json"))?;
        let evidence = SessionEvidence::prepare(home)?;
        if !evidence.idle
            || files::fingerprint(home, &relative).ok().as_ref() != Some(&expected)
            || sqlite_snapshot(home, &relative).ok().as_ref()
                != Some(&run["items"][index]["sidecars"])
        {
            changed(
                run,
                index,
                "database metadata or runtime activity changed since preview",
            );
            return state::save_run(home, run, false);
        }
        let idle = with_cleanup_gate(|| -> anyhow::Result<bool> {
            fs2::FileExt::try_lock_exclusive(&store_lock)?;
            evidence.unchanged(home, std::time::Instant::now() + LOCK_CHECK_BUDGET)
        })
        .unwrap_or(false);
        drop(store_lock);
        if !idle {
            changed(
                run,
                index,
                "session state busy or changed; compaction deferred",
            );
            return state::save_run(home, run, false);
        }
        // The database owns its exclusive maintenance lock; unrelated lifecycle
        // work is free to proceed. Only bounded ephemeral stores are allowed.
        run["items"][index]["state"] = json!("moving");
        state::save_run(home, run, false)?;
        let bytes = super::storage_cleanup_vacuum::compact(home, &relative)?;
        run["items"][index]["state"] = json!("vacuumed");
        run["items"][index]["outcome"] = json!("vacuumed");
        run["items"][index]["freedBytes"] = json!(bytes);
        run["items"][index]["reason"] = Value::Null;
        return state::save_run(home, run, false);
    }

    let trash_exists = std::fs::symlink_metadata(home.join(&trash)).is_ok();
    if item_state == "deleting" && !trash_exists {
        move_changed(
            run,
            index,
            "trash missing; deletion cannot be confirmed and source is preserved",
            None,
        );
        return state::save_run(home, run, false);
    }
    if item_state != "deleting" {
        let outcome = if trash_exists {
            if item_state != "moving" {
                move_changed(run, index, "unexpected existing trash path", Some(trash));
                return state::save_run(home, run, false);
            }
            files::verify_renamed(home, &relative, &trash, &expected)?
        } else {
            // All discovery, database reads, fingerprints and durable receipt
            // writes happen before the lifecycle/session lock acquisition.
            let store_lock =
                crate::session::store::cleanup_lock_file(&home.join("cutex-sessions.json"))?;
            let evidence = SessionEvidence::prepare(home)?;
            let eligibility =
                PreparedEligibility::prepare(home, &request, &relative, &evidence.protected)?;
            if !eligibility.eligible() {
                move_changed(run, index, "eligibility or references changed", None);
                return state::save_run(home, run, false);
            }
            let Some(prepared) =
                files::prepare_rename(home, &relative, &run_id, &item_id, &expected)?
            else {
                move_changed(run, index, "contents metadata or identity changed", None);
                return state::save_run(home, run, false);
            };
            run["items"][index]["state"] = json!("moving");
            run["items"][index]["retainedPath"] = json!(trash.to_string_lossy().replace('\\', "/"));
            state::save_run(home, run, false)?;
            let renamed = with_cleanup_gate(|| -> anyhow::Result<Option<files::MoveOutcome>> {
                fs2::FileExt::try_lock_exclusive(&store_lock)?;
                let deadline = std::time::Instant::now() + LOCK_CHECK_BUDGET;
                if !evidence.unchanged(home, deadline)?
                    || !eligibility.unchanged(home, &request, &relative, deadline)?
                {
                    return Ok(None);
                }
                Ok(Some(files::rename_prepared(prepared, deadline)?))
            });
            // In particular, no source-directory pin or lifecycle/store lock
            // survives into post-rename tree validation or recursive unlink.
            drop(store_lock);
            match renamed {
                Ok(Some(files::MoveOutcome::Moved { .. })) => {
                    files::verify_renamed(home, &relative, &trash, &expected)?
                }
                Ok(Some(changed)) => changed,
                Ok(None) => {
                    move_changed(
                        run,
                        index,
                        "reference evidence changed or lock-check budget exceeded",
                        None,
                    );
                    return state::save_run(home, run, false);
                }
                Err(error) => {
                    move_changed(
                        run,
                        index,
                        &format!("lifecycle state busy or changed: {error}"),
                        None,
                    );
                    return state::save_run(home, run, false);
                }
            }
        };
        if let files::MoveOutcome::Changed { retained } = outcome {
            move_changed(run, index, "path changed during trash move", retained);
            return state::save_run(home, run, false);
        }
        run["items"][index]["retainedPath"] = json!(trash.to_string_lossy().replace('\\', "/"));
        run["items"][index]["state"] = json!("deleting");
        run["items"][index]["retryPhase"] = json!("deleting");
        state::save_run(home, run, false)?;
    }
    run["items"][index]["retryPhase"] = json!("deleting");
    #[cfg(test)]
    before_delete_hook(home, &trash)?;
    let deletion = files::delete_trash(home, &trash, &expected);
    if matches!(deletion, Ok(false)) {
        changed(
            run,
            index,
            "trash disappeared before deletion; no bytes claimed",
        );
        return state::save_run(home, run, false);
    }
    if let Err(error) = deletion {
        if error.downcast_ref::<files::ChangedTrash>().is_some() {
            changed(
                run,
                index,
                "trash contents changed; retained for inspection",
            );
            return state::save_run(home, run, false);
        }
        return Err(error);
    }
    remove_internal(&mut run["items"][index], "retainedPath");
    remove_internal(&mut run["items"][index], "retryPhase");
    run["items"][index]["state"] = json!("deleted");
    run["items"][index]["outcome"] = json!("deleted");
    run["items"][index]["reason"] = Value::Null;
    run["items"][index]["freedBytes"] = json!(expected.bytes);
    state::save_run(home, run, false)
}
fn publish(run: &Value) {
    let result = (|| -> anyhow::Result<()> {
        let repository = super::repository::management_v2_repository()?;
        super::server::append_cutex_event(
            repository,
            super::services::HOST_EVENT_SESSION_ID,
            Default::default(),
            "cutex/storage/cleanupChanged",
            json!({"runId":run["runId"],"state":run["state"],"freedBytes":run["freedBytes"]}),
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("storage cleanup event unavailable: {error:#}");
    }
}
fn refresh_affected(home: &Path, run: &Value) -> anyhow::Result<()> {
    if !run["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| matches!(item["outcome"].as_str(), Some("deleted" | "vacuumed")))
    {
        return Ok(());
    }
    let ids: &[&str] = match run["action"].as_str() {
        Some("orphan-homes" | "retired-homes") => &["light-homes"],
        Some("old-backups") => &["codex-home-backups", "migration-leftovers"],
        Some("old-logs") => &[
            "daemon-logs",
            "release-logs",
            "diagnostics",
            "app-server-journal",
            "events-orphaned",
        ],
        Some("vacuum") => &[],
        _ => &[],
    };
    if !ids.is_empty() {
        super::storage::refresh_selected(
            home,
            Some(ids.iter().map(|id| (*id).to_owned()).collect()),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "synthetic 6870-file preview timing; run explicitly in a private HOME"]
    fn synthetic_old_logs_preview_timing() {
        let home = home();
        let old = SystemTime::now() - std::time::Duration::from_secs(90 * 86400);
        for i in 0..6870 {
            let path = home.join(format!(
                "runtime/app-server-journal/group-{}/capture-{i}.jsonl",
                i / 687
            ));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let file = std::fs::File::create(path).unwrap();
            file.set_len(1_500_000_000 / 6870).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let started = std::time::Instant::now();
        let plan = preview(&home, json!({"action":"old-logs"}), actor()).unwrap();
        eprintln!(
            "synthetic_old_logs preview_ms={} items={} freeable_bytes={}",
            started.elapsed().as_millis(),
            plan["items"].as_array().unwrap().len(),
            plan["freeableBytes"]
        );
        assert_eq!(plan["items"].as_array().unwrap().len(), 6870);
        assert_eq!(plan["freeableBytes"], (1_500_000_000u64 / 6870) * 6870);
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn log_preview_reports_expired_budget_without_authorizing_uninspected_files() {
        let home = home();
        old_file(&home, "runtime/app-server-journal/old.jsonl");
        let plan = preview_with_limits(
            &home,
            json!({"action":"old-logs"}),
            actor(),
            Duration::ZERO,
            10_000,
        )
        .unwrap();
        assert_eq!(plan["complete"], false);
        assert_eq!(plan["partialReason"], "time_budget");
        assert_eq!(plan["inspectedItems"], 0);
        assert_eq!(plan["freeableBytes"], 0);
        assert!(plan["items"].as_array().unwrap().is_empty());
        assert!(execute(&home, request(&plan), actor()).is_err());
        assert!(home.join("runtime/app-server-journal/old.jsonl").exists());
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn log_preview_item_limit_keeps_only_complete_fingerprints_and_exact_totals() {
        let home = home();
        for name in ["a", "b", "c"] {
            old_file(&home, &format!("runtime/app-server-journal/{name}.jsonl"));
        }
        let plan = preview_with_limits(
            &home,
            json!({"action":"old-logs"}),
            actor(),
            Duration::from_secs(30),
            2,
        )
        .unwrap();
        assert_eq!(plan["complete"], false);
        assert_eq!(plan["partialReason"], "item_limit");
        assert_eq!(plan["inspectedItems"], 2);
        assert_eq!(plan["items"].as_array().unwrap().len(), 2);
        assert_eq!(plan["freeableBytes"], 2 * b"synthetic old log".len());
        let stored = state::preview(&home, plan["previewId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        for item in stored["items"].as_array().unwrap() {
            assert!(item["fingerprint"].is_object());
        }
        let accepted = execute(&home, request(&plan), actor()).unwrap();
        resume(&home).unwrap();
        let receipt = state::run(&home, accepted["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt["items"].as_array().unwrap().len(), 2);
        assert!(home.join("runtime/app-server-journal/c.jsonl").exists());
    }
    #[test]
    fn complete_log_preview_marks_exact_totals_and_rejects_unknown_or_duplicate_selection() {
        let home = home();
        let path = "runtime/app-server-journal/old.jsonl";
        old_file(&home, path);
        let plan = preview(&home, json!({"action":"old-logs"}), actor()).unwrap();
        assert_eq!(plan["complete"], true);
        assert!(plan.get("partialReason").is_none());
        assert_eq!(plan["inspectedItems"], 1);
        assert!(preview(
            &home,
            json!({"action":"old-logs","items":["unknown"]}),
            actor()
        )
        .is_err());
        assert!(preview(
            &home,
            json!({"action":"old-logs","items":[path,path]}),
            actor()
        )
        .is_err());
        std::fs::remove_dir_all(home).unwrap();
    }
    fn actor() -> Value {
        json!({"principal":"owner","clientName":"desktop","deviceId":null})
    }
    fn home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("cleanup-engine-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        home.canonicalize().unwrap()
    }
    fn old_file(home: &Path, relative: &str) {
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"synthetic old log").unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - std::time::Duration::from_secs(90 * 86400)),
            )
            .unwrap();
    }
    fn request(preview: &Value) -> Value {
        json!({"requestId":uuid::Uuid::new_v4().to_string(),"previewId":preview["previewId"],"confirmed":true})
    }
    #[test]
    fn paired_owner_phone_is_refused_before_json_or_storage_access() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut token = crate::management::client_tokens::VerifiedClientToken::test_token(
                "synthetic-phone",
            );
            token.issued_via = crate::management::client_tokens::IssuedVia::Pairing;
            crate::management::listeners::set_current_client_token(Some(token));
            let (mut stream, _) = listener.accept().unwrap();
            handle(
                &mut stream,
                &crate::http::server::SimpleHttpRequest {
                    method: "POST".into(),
                    path: "/v2/storage/cleanups".into(),
                    headers: Default::default(),
                    body: b"invalid JSON".to_vec(),
                },
            )
            .unwrap();
        });
        let mut client = std::net::TcpStream::connect(address).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        worker.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains("storage_cleanup_desktop_only"));
    }
    #[test]
    fn old_backup_cleanup_is_high_risk() {
        let home = home();
        let plan = preview(&home, json!({"action":"old-backups"}), actor()).unwrap();
        assert_eq!(plan["risk"], "high");
    }
    #[test]
    fn fresh_fork_copy_receipt_keeps_manual_required_reason_and_zero_freeable_bytes() {
        let home = home();
        let path = "runtime/claude/fork-copy-leftovers/550e8400-e29b-41d4-a716-446655440000.json";
        std::fs::create_dir_all(home.join(path).parent().unwrap()).unwrap();
        std::fs::write(home.join(path), b"synthetic inspection receipt").unwrap();
        let plan = preview(
            &home,
            json!({"action":"old-backups","items":[path]}),
            actor(),
        )
        .unwrap();
        assert_eq!(plan["items"][0]["code"], "manual_required");
        assert_eq!(plan["items"][0]["eligible"], false);
        assert_eq!(plan["freeableBytes"], 0);
        assert!(plan["items"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("outside the cleanup root"));
    }
    fn vacuum_fixture() -> (PathBuf, &'static str) {
        let home = home();
        let path = "runtime/documents/documents-v1.sqlite3";
        std::fs::create_dir_all(home.join("runtime/documents")).unwrap();
        let db = rusqlite::Connection::open(home.join(path)).unwrap();
        db.execute_batch("CREATE TABLE records(value BLOB); INSERT INTO records VALUES(zeroblob(1000000)); INSERT INTO records VALUES('preserved'); DELETE FROM records WHERE rowid=1;").unwrap();
        drop(db);
        (home, path)
    }
    #[test]
    fn manual_vacuum_reclaims_only_free_pages_and_preserves_records() {
        let (home, path) = vacuum_fixture();
        let plan = preview(&home, json!({"action":"vacuum","items":[path]}), actor()).unwrap();
        assert_eq!(plan["risk"], "medium");
        assert_eq!(plan["items"][0]["eligible"], true);
        assert!(plan["freeableBytes"].as_u64().unwrap() > 0);
        let accepted = execute(&home, request(&plan), actor()).unwrap();
        resume(&home).unwrap();
        let receipt = state::run(&home, accepted["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt["items"][0]["outcome"], "vacuumed", "{receipt}");
        assert!(receipt["freedBytes"].as_u64().unwrap() > 0);
        let db = rusqlite::Connection::open(home.join(path)).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM records", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }
    #[test]
    fn vacuum_rechecks_wal_existence_without_opening_changed_database() {
        let (home, path) = vacuum_fixture();
        let plan = preview(&home, json!({"action":"vacuum","items":[path]}), actor()).unwrap();
        let original = std::fs::read(home.join(path)).unwrap();
        let wal = home.join(format!("{path}-wal"));
        std::fs::write(&wal, b"new opaque WAL bytes after preview").unwrap();
        let accepted = execute(&home, request(&plan), actor()).unwrap();
        resume(&home).unwrap();
        let receipt = state::run(&home, accepted["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt["items"][0]["code"], "changed");
        assert_eq!(receipt["items"][0]["outcome"], "skipped");
        assert_eq!(std::fs::read(home.join(path)).unwrap(), original);
        assert_eq!(
            std::fs::read(wal).unwrap(),
            b"new opaque WAL bytes after preview"
        );
    }
    #[test]
    fn cleanup_restart_child() {
        let Some(root) = std::env::var_os("CUTEX_STORAGE_CRASH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let run_id = std::env::var("CUTEX_STORAGE_CRASH_RUN").unwrap();
        let mode = std::env::var("CUTEX_STORAGE_CRASH_PHASE").unwrap();
        if mode == "resume" {
            resume(&root).unwrap();
            return;
        }
        let mut run = state::run(&root, &run_id).unwrap().unwrap();
        let item = &run["items"][0];
        let relative = PathBuf::from(item["path"].as_str().unwrap());
        let expected: files::Fingerprint =
            serde_json::from_value(item["fingerprint"].clone()).unwrap();
        let trash = files::trash_relative(&run_id, item["id"].as_str().unwrap()).unwrap();
        assert!(matches!(
            files::move_to_trash(
                &root,
                &relative,
                &run_id,
                item["id"].as_str().unwrap(),
                &expected
            )
            .unwrap(),
            files::MoveOutcome::Moved { .. }
        ));
        run["items"][0]["state"] = json!("deleting");
        run["items"][0]["retainedPath"] = json!(trash);
        state::save_run(&root, &run, false).unwrap();
        if mode == "partial" {
            std::fs::remove_file(root.join(&trash).join("a")).unwrap();
        }
        // Deliberately bypass Rust destructors: the next process must rely on
        // the committed manifest and filesystem state, never surviving handles.
        std::process::exit(23);
    }
    #[test]
    fn actual_process_restart_finishes_rename_and_partial_delete_receipts() {
        fn child(root: &Path, id: &str, mode: &str) -> std::process::Output {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "management::v2::storage_cleanup::tests::cleanup_restart_child",
                    "--nocapture",
                ])
                .env("CUTEX_STORAGE_CRASH_ROOT", root)
                .env("CUTEX_STORAGE_CRASH_RUN", id)
                .env("CUTEX_STORAGE_CRASH_PHASE", mode);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            command.output().unwrap()
        }
        for mode in ["renamed", "partial"] {
            let home = home();
            let path = format!("runtime/light/agents/{}", uuid::Uuid::new_v4());
            old_file(&home, &format!("{path}/a"));
            old_file(&home, &format!("{path}/b"));
            std::fs::File::open(home.join(&path))
                .and_then(|file| {
                    file.set_times(std::fs::FileTimes::new().set_modified(
                        SystemTime::now() - std::time::Duration::from_secs(90 * 86400),
                    ))
                })
                .or_else(|_| {
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::OpenOptionsExt;
                        std::fs::File::options()
                            .write(true)
                            .custom_flags(0x02000000)
                            .open(home.join(&path))?
                            .set_times(std::fs::FileTimes::new().set_modified(
                                SystemTime::now() - std::time::Duration::from_secs(90 * 86400),
                            ))
                    }
                    #[cfg(not(windows))]
                    {
                        Err(std::io::Error::other("set directory fixture time"))
                    }
                })
                .unwrap();
            let plan = preview(
                &home,
                json!({"action":"orphan-homes","items":[path]}),
                actor(),
            )
            .unwrap();
            assert_eq!(plan["items"][0]["eligible"], true, "{plan}");
            let internal = state::preview(&home, plan["previewId"].as_str().unwrap())
                .unwrap()
                .unwrap();
            let mut item = internal["items"][0].clone();
            item["state"] = json!("moving");
            item["outcome"] = Value::Null;
            item["freedBytes"] = json!(0);
            let id = uuid::Uuid::new_v4().to_string();
            let run = json!({"runId":id,"requestId":uuid::Uuid::new_v4().to_string(),"previewId":plan["previewId"],"action":"orphan-homes","state":"running","createdAt":chrono::Utc::now().to_rfc3339(),"freedBytes":0,"actor":actor(),"items":[item],"previewRequest":internal["request"]});
            state::begin(&home, &run).unwrap();
            let crashed = child(&home, &id, mode);
            assert_eq!(
                crashed.status.code(),
                Some(23),
                "{}",
                String::from_utf8_lossy(&crashed.stderr)
            );
            assert!(!home.join(&path).exists());
            let resumed = child(&home, &id, "resume");
            assert!(
                resumed.status.success(),
                "{}",
                String::from_utf8_lossy(&resumed.stderr)
            );
            let receipt = state::run(&home, &id).unwrap().unwrap();
            assert_eq!(receipt["state"], "completed", "{receipt}");
            assert_eq!(receipt["items"][0]["outcome"], "deleted");
            assert_eq!(receipt["freedBytes"], 34);
            let audit =
                std::fs::read_to_string(home.join("runtime/storage/cleanups.jsonl")).unwrap();
            assert_eq!(audit.lines().filter(|line| !line.is_empty()).count(), 2);
        }
    }
    #[test]
    fn preview_bound_cleanup_is_idempotent_and_changed_files_survive() {
        let home = home();
        let relative = "runtime/diagnostics/old.jsonl";
        old_file(&home, relative);
        let plan = preview(&home, json!({"action":"old-logs"}), actor()).unwrap();
        super::super::schema_export::assert_valid("storage-cleanup", "preview", &plan);
        assert_eq!(plan["items"][0]["eligible"], true);
        assert!(plan["items"][0].get("fingerprint").is_none());
        std::fs::write(home.join(relative), b"new contents after preview").unwrap();
        let body = request(&plan);
        let accepted = execute(&home, body.clone(), actor()).unwrap();
        resume(&home).unwrap();
        let receipt = state::run(&home, accepted["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt["items"][0]["outcome"], "skipped");
        assert_eq!(receipt["items"][0]["code"], "changed");
        assert_eq!(
            std::fs::read(home.join(relative)).unwrap(),
            b"new contents after preview"
        );
        assert_eq!(
            execute(&home, body.clone(), actor()).unwrap()["runId"],
            accepted["runId"]
        );
        let mut other = body;
        other["items"] = json!([]);
        assert!(execute(&home, other, actor()).is_err());
        let public = public_run(receipt);
        super::super::schema_export::assert_valid("storage-cleanup", "run", &public);
    }
    #[test]
    fn deletion_has_durable_start_end_audit_and_survives_another_resume() {
        let home = home();
        let relative = "runtime/diagnostics/old.jsonl";
        old_file(&home, relative);
        let plan = preview(&home, json!({"action":"old-logs"}), actor()).unwrap();
        let accepted = execute(&home, request(&plan), actor()).unwrap();
        resume(&home).unwrap();
        resume(&home).unwrap();
        assert!(!home.join(relative).exists());
        let receipt = state::run(&home, accepted["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt["items"][0]["outcome"], "deleted");
        assert_eq!(receipt["freedBytes"], 17);
        let lines = std::fs::read_to_string(home.join("runtime/storage/cleanups.jsonl")).unwrap();
        let audit: Vec<Value> = lines
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0]["phase"], "start");
        assert_eq!(audit[1]["phase"], "end");
        super::super::schema_export::assert_valid("storage-cleanup", "run", &public_run(receipt));
    }
    #[test]
    fn expired_cross_credential_and_high_risk_without_typed_text_cannot_execute() {
        let home = home();
        old_file(&home, "runtime/diagnostics/old.jsonl");
        let plan = preview(&home, json!({"action":"old-logs"}), actor()).unwrap();
        let body = request(&plan);
        assert!(execute(
            &home,
            body.clone(),
            json!({"principal":"owner","clientName":"hostCli","deviceId":"other"})
        )
        .is_err());
        let mut persisted = state::preview(&home, plan["previewId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        persisted["risk"] = json!("high");
        state::database(&home)
            .unwrap()
            .execute(
                "UPDATE cleanup_previews SET value=?2 WHERE id=?1",
                rusqlite::params![persisted["previewId"].as_str(), persisted.to_string()],
            )
            .unwrap();
        assert!(execute(&home, body.clone(), actor()).is_err());
        let mut confirmed = body.clone();
        confirmed["confirmText"] = json!("删除");
        persisted["expiresAt"] = json!("2020-01-01T00:00:00Z");
        state::database(&home)
            .unwrap()
            .execute(
                "UPDATE cleanup_previews SET value=?2 WHERE id=?1",
                rusqlite::params![persisted["previewId"].as_str(), persisted.to_string()],
            )
            .unwrap();
        assert!(execute(&home, confirmed, actor()).is_err());
        assert!(state::history(&home, false).unwrap().is_empty());
    }
    #[test]
    fn restart_from_moving_before_rename_rechecks_new_protection() {
        let home = home();
        let relative = "runtime/diagnostics/old.jsonl";
        old_file(&home, relative);
        let public = preview(&home, json!({"action":"old-logs"}), actor()).unwrap();
        let plan = state::preview(&home, public["previewId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        let mut item = plan["items"][0].clone();
        item["state"] = json!("moving");
        item["outcome"] = Value::Null;
        item["freedBytes"] = json!(0);
        let run = json!({"runId":uuid::Uuid::new_v4().to_string(),"requestId":uuid::Uuid::new_v4().to_string(),"previewId":plan["previewId"],"state":"running","action":"old-logs","createdAt":chrono::Utc::now().to_rfc3339(),"items":[item],"freedBytes":0,"actor":actor(),"previewRequest":plan["request"]});
        state::begin(&home, &run).unwrap();
        std::fs::write(home.join(relative), b"changed while daemon stopped").unwrap();
        resume(&home).unwrap();
        let receipt = state::run(&home, run["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt["items"][0]["code"], "changed");
        assert!(home.join(relative).exists());
    }
    #[test]
    fn request_replay_wakes_a_committed_run_without_an_original_worker() {
        let home = home();
        old_file(&home, "runtime/diagnostics/old.jsonl");
        let plan = preview(&home, json!({"action":"old-logs"}), actor()).unwrap();
        let internal = state::preview(&home, plan["previewId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        let body = request(&plan);
        let mut item = internal["items"][0].clone();
        item["state"] = json!("queued");
        item["outcome"] = Value::Null;
        item["freedBytes"] = json!(0);
        let id = uuid::Uuid::new_v4().to_string();
        let run = json!({"runId":id,"requestId":body["requestId"],"previewId":plan["previewId"],"request":body,"action":"old-logs","actor":actor(),"state":"running","createdAt":chrono::Utc::now().to_rfc3339(),"freedBytes":0,"items":[item],"previewRequest":internal["request"]});
        state::begin(&home, &run).unwrap();
        assert_eq!(execute(&home, body.clone(), actor()).unwrap()["runId"], id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let receipt = state::run(&home, &id).unwrap().unwrap();
            if receipt["state"] != "running" {
                assert_eq!(receipt["state"], "completed", "{receipt}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "replayed committed run did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let reused = execute(&home, request(&plan), actor()).unwrap_err();
        assert!(reused.to_string().contains("preview already used"));
    }
    struct DeleteHookGuard;
    impl Drop for DeleteHookGuard {
        fn drop(&mut self) {
            BEFORE_DELETE.with(|hook| *hook.borrow_mut() = None);
        }
    }
    fn seeded_trash(home: &Path) -> Value {
        let path = format!("runtime/light/agents/{}", uuid::Uuid::new_v4());
        old_file(home, &format!("{path}/a"));
        old_file(home, &format!("{path}/b"));
        let fp = files::fingerprint(home, Path::new(&path)).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let item = uuid::Uuid::new_v4().to_string();
        let mut run = json!({"runId":id,"requestId":uuid::Uuid::new_v4().to_string(),"previewId":uuid::Uuid::new_v4().to_string(),"action":"orphan-homes","state":"running","createdAt":chrono::Utc::now().to_rfc3339(),"freedBytes":0,"actor":actor(),"previewRequest":{"action":"orphan-homes"},"items":[{"id":item,"path":path,"fingerprint":fp,"state":"moving","outcome":null,"freedBytes":0,"reason":null}]});
        state::begin(home, &run).unwrap();
        assert!(matches!(
            files::move_to_trash(home, Path::new(&path), &id, &item, &fp).unwrap(),
            files::MoveOutcome::Moved { .. }
        ));
        run["items"][0]["state"] = json!("deleting");
        run["items"][0]["retryPhase"] = json!("deleting");
        run["items"][0]["retainedPath"] = json!(files::trash_relative(&id, &item).unwrap());
        state::save_run(home, &run, false).unwrap();
        run
    }
    #[test]
    fn preview_does_not_wait_for_global_or_session_writer_locks() {
        let home = home();
        old_file(&home, "runtime/diagnostics/old.jsonl");
        crate::session::store::with_locked_session_rows(&home.join("cutex-sessions.json"), |_| {
            Ok(())
        })
        .unwrap();
        let gate = crate::session::lifecycle_gate::lock().unwrap();
        let file =
            crate::session::store::cleanup_lock_file(&home.join("cutex-sessions.json")).unwrap();
        fs2::FileExt::lock_exclusive(&file).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let copied = home.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send(preview(&copied, json!({"action":"old-logs"}), actor()))
                .unwrap();
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(2));
        drop(file);
        drop(gate);
        worker.join().unwrap();
        assert_eq!(
            result
                .expect("preview blocked behind lifecycle writers")
                .unwrap()["items"][0]["eligible"],
            true
        );
    }
    #[test]
    fn lifecycle_and_store_locks_are_released_before_recursive_trash_delete() {
        let home = home();
        let run = seeded_trash(&home);
        let _hook = DeleteHookGuard;
        BEFORE_DELETE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|root, _| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let gate = loop {
                    match crate::session::lifecycle_gate::try_lock_for_cleanup() {
                        Ok(gate) => break gate,
                        Err(_) => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "trash GC holds lifecycle gate"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                };
                let file =
                    crate::session::store::cleanup_lock_file(&root.join("cutex-sessions.json"))?;
                fs2::FileExt::try_lock_exclusive(&file)
                    .expect("trash GC holds session writer lock");
                drop(file);
                drop(gate);
                Ok(())
            }))
        });
        resume(&home).unwrap();
        assert_eq!(
            state::run(&home, run["runId"].as_str().unwrap())
                .unwrap()
                .unwrap()["state"],
            "completed"
        );
    }
    #[test]
    fn cleanup_recheck_panic_does_not_poison_the_lifecycle_gate() {
        assert!(with_cleanup_gate::<()>(|| panic!("simulated recheck panic")).is_err());
        assert!(crate::session::lifecycle_gate::lock().is_ok());
    }
    #[test]
    fn deleting_failure_retries_only_manifest_trash_and_preserves_new_source() {
        let home = home();
        let run = seeded_trash(&home);
        let _hook = DeleteHookGuard;
        BEFORE_DELETE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|root, trash| {
                std::fs::remove_file(root.join(trash).join("a"))?;
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "simulated transient sharing violation",
                )
                .into())
            }))
        });
        resume(&home).unwrap();
        let failed = state::run(&home, run["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["items"][0]["retryPhase"], "deleting");
        BEFORE_DELETE.with(|slot| *slot.borrow_mut() = None);
        let new_source = format!("{}/new", run["items"][0]["path"].as_str().unwrap());
        old_file(&home, &new_source);
        // A separate process performs the retry; no source path is re-approved
        // or moved, even though a new source now exists at the old location.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "management::v2::storage_cleanup::tests::cleanup_restart_child",
                "--nocapture",
            ])
            .env("CUTEX_STORAGE_CRASH_ROOT", &home)
            .env("CUTEX_STORAGE_CRASH_RUN", run["runId"].as_str().unwrap())
            .env("CUTEX_STORAGE_CRASH_PHASE", "resume");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            child.creation_flags(0x08000000);
        }
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let done = state::run(&home, run["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(done["state"], "completed", "{done}");
        assert_eq!(done["freedBytes"], 34);
        assert!(home.join(new_source).is_file());
        assert_eq!(done["attempt"], 2);
        let audit = std::fs::read_to_string(home.join("runtime/storage/cleanups.jsonl")).unwrap();
        assert_eq!(audit.lines().filter(|line| !line.is_empty()).count(), 4);
        super::super::schema_export::assert_valid("storage-cleanup", "run", &public_run(done));
    }
    #[test]
    fn trash_disappearing_after_execution_check_claims_no_bytes() {
        let home = home();
        let mut run = seeded_trash(&home);
        let expected: files::Fingerprint =
            serde_json::from_value(run["items"][0]["fingerprint"].clone()).unwrap();
        let source = run["items"][0]["path"].as_str().unwrap().to_owned();
        let _hook = DeleteHookGuard;
        BEFORE_DELETE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |root, trash| {
                assert!(files::delete_trash(root, trash, &expected)?);
                old_file(root, &format!("{source}/new"));
                Ok(())
            }))
        });
        process_item(&home, &mut run, 0).unwrap();
        assert_eq!(run["items"][0]["outcome"], "skipped");
        assert_eq!(run["items"][0]["freedBytes"], 0);
        assert!(home
            .join(run["items"][0]["path"].as_str().unwrap())
            .join("new")
            .is_file());
    }
    #[test]
    fn missing_trash_never_claims_deleted_bytes_for_a_changed_source() {
        let home = home();
        let mut run = seeded_trash(&home);
        let trash = files::trash_relative(
            run["runId"].as_str().unwrap(),
            run["items"][0]["id"].as_str().unwrap(),
        )
        .unwrap();
        let fp: files::Fingerprint =
            serde_json::from_value(run["items"][0]["fingerprint"].clone()).unwrap();
        files::delete_trash(&home, &trash, &fp).unwrap();
        old_file(
            &home,
            &format!("{}/new", run["items"][0]["path"].as_str().unwrap()),
        );
        process_item(&home, &mut run, 0).unwrap();
        assert_eq!(run["items"][0]["outcome"], "skipped");
        assert_eq!(run["items"][0]["freedBytes"], 0);
        assert!(home
            .join(format!("{}/new", run["items"][0]["path"].as_str().unwrap()))
            .is_file());
    }
    #[test]
    #[ignore = "Produces an isolated cleaned home for the real older-binary rollback harness"]
    fn emit_cleaned_home_for_older_binary_rollback() {
        let base = PathBuf::from(
            std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test home required"),
        );
        assert!(base.join(".cutex-test-private-home").is_file());
        let root = base.join(format!("rollback-fixture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let native_id = "11111111-1111-4111-8111-111111111111";
        let relative = Path::new("runtime/light/agents").join(native_id);
        let native = root.join(&relative);
        std::fs::create_dir_all(&native).unwrap();
        let mut retained = Vec::new();
        let mut write = |name: &str, data: &[u8]| {
            let path = native.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, data).unwrap();
            retained.push(json!({"relative":relative.join(name).to_string_lossy().replace('\\',"/"),"sha256":crate::asks::sha256_hex(data)}));
        };
        write("config.toml", b"model = 'synthetic-model'\n");
        let bundle=serde_json::to_vec(&json!({"version":4,"native_patch_commit":"f2469b10fa073f3041251014d52dceb0ce10465c","synthetic":true})).unwrap();
        write("bundle.json", &bundle);
        let rollout = format!("sessions/2026/10/24/rollout-2026-10-24T00-00-00-{native_id}.jsonl");
        write(&rollout,format!("{}\n{}\n",json!({"type":"session_meta","payload":{"id":native_id,"cwd":root}}),json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"synthetic preserved conversation"}]}})).as_bytes());
        write("adoption.json",serde_json::to_vec(&json!({"native_id":native_id,"source_history":native.join(&rollout),"sha256":"0".repeat(64),"dependencies":[]})).unwrap().as_slice());
        write("auth.json", br#"{"synthetic":true}"#);
        write("checkpoints/keep.json", br#"{"synthetic":true}"#);
        write("unknown-cache/user-file.bin", b"unknown user data retained");
        drop(write);
        for (name, table) in [
            (
                "state_5.sqlite",
                "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT);",
            ),
            (
                "thread_history_1.sqlite",
                "CREATE TABLE thread_history(id TEXT, value TEXT);",
            ),
            ("logs_2.sqlite", "CREATE TABLE logs(value TEXT);"),
        ] {
            let db = rusqlite::Connection::open(native.join(name)).unwrap();
            db.execute_batch(table).unwrap();
            drop(db);
            retained.push(json!({"relative":relative.join(name).to_string_lossy().replace('\\',"/"),"sha256":crate::asks::sha256_hex(&std::fs::read(native.join(name)).unwrap())}));
        }
        let generated = [
            "models_cache.json".to_owned(),
            format!("shell_snapshots/{native_id}.12345.sh"),
            "log/codex-tui.log".into(),
        ];
        for name in &generated {
            let path = native.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"synthetic regenerable bytes").unwrap();
        }
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut record = crate::session::model::CutexSessionRecord::new(
            id.clone(),
            Some(native_id.into()),
            crate::platform::host::current_host_name(),
            root.to_string_lossy().into(),
            None,
        )
        .unwrap();
        record.agent_enabled = true;
        record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
        record.formal_agent_name = Some("Synthetic rollback agent".into());
        record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some(chrono::Utc::now().to_rfc3339());
        record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: native_id.into(),
            native_home: native.clone(),
            bundle_manifest: native.join("bundle.json"),
            bundle_sha256: crate::role_revision::sha256_bytes(&bundle),
        });
        crate::session::store::with_locked_session_rows(
            &root.join("cutex-sessions.json"),
            |rows| {
                rows.insert_session(id.clone(), record);
                rows.commit()?;
                Ok(())
            },
        )
        .unwrap();
        let plan = preview(&root, json!({"action":"retired-homes"}), actor()).unwrap();
        assert_eq!(
            plan["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["eligible"] == true)
                .count(),
            3,
            "{plan}"
        );
        let mut body = request(&plan);
        body["confirmText"] = json!("删除");
        let run = execute(&root, body, actor()).unwrap();
        resume(&root).unwrap();
        let receipt = state::run(&root, run["runId"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(receipt["state"], "completed");
        for entry in &retained {
            assert_eq!(
                crate::asks::sha256_hex(
                    &std::fs::read(root.join(entry["relative"].as_str().unwrap())).unwrap()
                ),
                entry["sha256"].as_str().unwrap()
            );
        }
        let deleted: Vec<String> = generated
            .iter()
            .map(|name| relative.join(name).to_string_lossy().replace('\\', "/"))
            .collect();
        for path in &deleted {
            assert!(!root.join(path).exists());
        }
        let manifest = json!({"synthetic":true,"cleanedByNewEngine":true,"cutexHome":root,"cutexSessionId":id,"retainedFiles":retained,"deletedPaths":deleted,"receipt":public_run(receipt)});
        let path = root.join("fixture.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        eprintln!("CLEANED_ROLLBACK_FIXTURE {}", path.display());
    }
}
