use std::{
    fs, io,
    net::TcpStream,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    http::server::{write_json_response, SimpleHttpRequest},
    session::{model::CutexSessionRecord, store::load_session_row},
};

use super::{
    archive_inventory::{self as inventory, PreviewRequest},
    archive_records::{self as records, Location, Record, Side, STATE},
    archive_transfer as transfer, storage_cleanup_fs as files,
};

const RUN: &str = "/v2/storage/archive-migrations";
const JOB_BUDGET: usize = 100;
const PREVIEW_LIFETIME_SECONDS: i64 = 600;
const FINISHED_HISTORY: usize = 100;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum State {
    Preview,
    Queued,
    Finished,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    session_id: String,
    revision: u64,
    relative_path: PathBuf,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Job {
    #[serde(with = "records::uuid_text")]
    id: Uuid,
    state: State,
    location: Location,
    created_at: i64,
    #[serde(default)]
    finished_at: Option<i64>,
    items: Vec<Item>,
    #[serde(default)]
    active: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartRequest {
    #[serde(with = "records::uuid_text")]
    preview_id: Uuid,
    confirmed: bool,
}

fn job_path(home: &Path, id: Uuid) -> PathBuf {
    home.join(STATE).join("jobs").join(format!("{id}.json"))
}

fn queue_path(home: &Path, id: Uuid) -> PathBuf {
    home.join(STATE).join("queue").join(format!("{id}.json"))
}

fn load_job(home: &Path, id: Uuid) -> anyhow::Result<Job> {
    let job: Job = records::read(&job_path(home, id))?.context("archive migration not found")?;
    ensure!(
        job.id == id && job.items.len() <= JOB_BUDGET,
        "invalid archive job"
    );
    Ok(job)
}

fn save_job(home: &Path, job: &Job) -> anyhow::Result<()> {
    records::write(&job_path(home, job.id), job)
}

pub(super) fn request_lock(home: &Path) -> anyhow::Result<fs::File> {
    records::lock_wait(&home.join(STATE).join("request.lock"))
}

fn job_lock_path(home: &Path, id: Uuid) -> PathBuf {
    // Locks outlive their jobs. Use stable shards to bound their files.
    let shard = id.as_bytes()[0] % 64;
    home.join(STATE)
        .join("worker-locks")
        .join(format!("{shard}.lock"))
}

fn auto_id(session_id: &str, revision: u64) -> Uuid {
    let hash = crate::role_revision::sha256_bytes(
        format!("archive-auto:{session_id}:{revision}").as_bytes(),
    );
    Uuid::parse_str(&hash.as_str()[..32]).expect("SHA-256 prefix is a UUID")
}

fn automatic_item(job: &Job) -> Option<&Item> {
    match job.items.as_slice() {
        [item] if job.id == auto_id(&item.session_id, item.revision) => Some(item),
        _ => None,
    }
}

fn visit_jobs(home: &Path, mut visit: impl FnMut(Job) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let entries = match fs::read_dir(home.join(STATE).join("jobs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let name = entry?.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|name| Uuid::parse_str(name).ok())
            .filter(|id| name.to_str() == Some(&format!("{id}.json")))
        else {
            continue;
        };
        match load_job(home, id) {
            Ok(job) => visit(job)?,
            Err(error) => eprintln!("archive migration job {id}: {error:#}"),
        }
    }
    Ok(())
}

fn remove_job_files(home: &Path, id: Uuid) -> anyhow::Result<()> {
    // The queue key can survive a failed transition or a crash at finish.
    match fs::remove_file(queue_path(home, id)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::remove_file(job_path(home, id))?;
    Ok(())
}

fn prune_jobs(home: &Path) -> anyhow::Result<()> {
    let _requests = request_lock(home)?;
    let now = chrono::Utc::now().timestamp();
    let mut retained = std::collections::BTreeSet::new();
    visit_jobs(home, |job| {
        if job.state == State::Preview
            && now.saturating_sub(job.created_at) > PREVIEW_LIFETIME_SECONDS
        {
            remove_job_files(home, job.id)?;
        } else if job.state == State::Finished {
            retained.insert((job.finished_at.unwrap_or(job.created_at), job.id));
            if retained.len() > FINISHED_HISTORY {
                let (_, id) = retained.pop_first().expect("history exceeded cap");
                let expired = load_job(home, id)?;
                if let Some(item) = automatic_item(&expired) {
                    // A crashed retirement hook is replayed on startup.
                    // Remember the revision before dropping its job receipt.
                    records::remember_auto_revision(home, &item.session_id, item.revision)?;
                }
                remove_job_files(home, id)?;
            }
        }
        Ok(())
    })
}

fn prune_best_effort(home: &Path) {
    if let Err(error) = prune_jobs(home) {
        eprintln!("archive migration history cleanup: {error:#}");
    }
}

fn list(home: &Path) -> anyhow::Result<Value> {
    prune_best_effort(home);
    let _requests = request_lock(home)?;
    let mut jobs = std::collections::BTreeMap::new();
    visit_jobs(home, |job| {
        if job.state != State::Preview {
            let rank = (
                job.state != State::Finished,
                job.finished_at.unwrap_or(job.created_at),
                job.id,
            );
            jobs.insert(
                rank,
                json!({
                    "id": job.id.to_string(), "state": job.state, "location": job.location.path,
                    "automatic": automatic_item(&job).is_some(), "createdAt": job.created_at,
                    "finishedAt": job.finished_at, "totalItems": job.items.len(),
                    "failedItems": job.items.iter().filter(|item| item.error.is_some()).count()
                }),
            );
        }
        Ok(())
    })?;
    Ok(json!({"jobs": jobs.into_values().rev().collect::<Vec<_>>() }))
}

fn configured_location(home: &Path) -> anyhow::Result<Option<Location>> {
    let persisted = super::archive_location::read(home)?;
    match (persisted.path, persisted.binding) {
        (None, _) => Ok(None),
        (Some(path), Some(identity)) => Ok(Some(Location { path, identity })),
        (Some(path), None) => Location::bind(home, &path).map(Some),
    }
}
fn load_agent(home: &Path, id: &str) -> anyhow::Result<CutexSessionRecord> {
    let (_, record) = load_session_row(&home.join("cutex-sessions.json"), id)?
        .with_context(|| format!("archive session not found: {id}"))?;
    ensure!(
        record.cutex_session_id == id,
        "archive session identity changed"
    );
    Ok(record)
}

fn prepare_items(home: &Path, preview: &Value) -> anyhow::Result<Vec<Item>> {
    let mut items = Vec::new();
    for candidate in preview["items"]
        .as_array()
        .context("archive preview has no items")?
    {
        if candidate["status"] != "candidate" {
            continue;
        }
        let session_id = candidate["sessionId"]
            .as_str()
            .context("preview session missing")?;
        let row = load_agent(home, session_id)?;
        let entry = &candidate["files"][0];
        items.push(Item {
            session_id: session_id.into(),
            revision: row.durable_revision(),
            relative_path: serde_json::from_value(entry["relativePath"].clone())?,
            bytes: entry["bytes"].as_u64().context("preview size missing")?,
            result: None,
            error: None,
        });
    }
    Ok(items)
}

pub(super) fn preview(home: &Path, request: PreviewRequest) -> anyhow::Result<Value> {
    prune_best_effort(home);
    let mut view = inventory::inspect(home, request)?;
    if let Some(location) = configured_location(home)? {
        location.verify()?;
        let items = prepare_items(home, &view)?;
        if !items.is_empty() {
            let job = Job {
                id: Uuid::new_v4(),
                state: State::Preview,
                location,
                created_at: chrono::Utc::now().timestamp(),
                finished_at: None,
                items,
                active: None,
            };
            save_job(home, &job)?;
            view["previewId"] = json!(job.id.to_string());
            view["expiresAt"] = json!(job.created_at + PREVIEW_LIFETIME_SECONDS);
            view["location"] = json!(job.location.path);
        }
    }
    Ok(view)
}

fn queue(home: &Path, job: &mut Job) -> anyhow::Result<()> {
    job.state = State::Queued;
    job.finished_at = None;
    // Write the ready key first. A worker that sees the previous preview
    // cannot act until the durable job itself has transitioned to queued.
    records::write(&queue_path(home, job.id), &job.id.to_string())?;
    save_job(home, job)?;
    spawn(home.to_owned())
}

fn start(home: &Path, request: StartRequest) -> anyhow::Result<Job> {
    ensure!(request.confirmed, "archive migration confirmation required");
    let _requests = request_lock(home)?;
    let mut job = load_job(home, request.preview_id)?;
    match job.state {
        State::Preview => {
            ensure!(
                chrono::Utc::now()
                    .timestamp()
                    .saturating_sub(job.created_at)
                    <= PREVIEW_LIFETIME_SECONDS,
                "archive preview expired"
            );
            ensure!(
                configured_location(home)?.as_ref() == Some(&job.location),
                "archive location changed since preview"
            );
            queue(home, &mut job)?;
        }
        State::Queued => spawn(home.to_owned())?,
        State::Finished if job.items.iter().any(|item| item.error.is_some()) => {
            let _job = records::lock(&job_lock_path(home, job.id))?;
            job = load_job(home, job.id)?;
            ensure!(
                configured_location(home)?.as_ref() == Some(&job.location),
                "archive location changed since this job ran"
            );
            job.location.verify()?;
            for item in &mut job.items {
                if item.error.take().is_some() {
                    item.result = None;
                }
            }
            queue(home, &mut job)?;
        }
        State::Finished => {}
    }
    Ok(job)
}

pub(super) fn handles(path: &str) -> bool {
    path == RUN || path.starts_with("/v2/storage/archive-migrations/")
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    home: &Path,
    path: &str,
) -> anyhow::Result<()> {
    let result = match (request.method.as_str(), path) {
        ("POST", RUN) => serde_json::from_slice(&request.body)
            .map_err(anyhow::Error::from)
            .and_then(|body| start(home, body))
            .map(|job| (202, "Accepted", progress(&job))),
        ("GET", RUN) => list(home).map(|value| (200, "OK", value)),
        ("GET", path) => path
            .strip_prefix("/v2/storage/archive-migrations/")
            .context("archive migration not found")
            .and_then(|id| Ok(Uuid::parse_str(id)?))
            .and_then(|id| {
                let job = load_job(home, id)?;
                ensure!(
                    job.state != State::Preview
                        || chrono::Utc::now()
                            .timestamp()
                            .saturating_sub(job.created_at)
                            <= PREVIEW_LIFETIME_SECONDS,
                    "archive migration not found"
                );
                Ok(job)
            })
            .map(|job| (200, "OK", progress(&job))),
        _ => anyhow::bail!("archive migration route not found"),
    };
    match result {
        Ok((status, reason, value)) => write_json_response(stream, status, reason, &value),
        Err(error) => {
            let missing = error.to_string().contains("not found");
            super::server::write_v2_error(
                stream,
                if missing { 404 } else { 409 },
                if missing { "Not Found" } else { "Conflict" },
                if missing {
                    "archive_migration_not_found"
                } else {
                    "archive_migration_conflict"
                },
                &format!("{error:#}"),
                !missing,
                json!({}),
            )
        }
    }
}

fn progress(job: &Job) -> Value {
    json!({
        "id": job.id.to_string(), "state": job.state, "location": job.location.path,
        "totalBytes": job.items.iter().map(|item| item.bytes).sum::<u64>(),
        "completedBytes": job.items.iter().filter(|item| item.result.as_deref() == Some("moved"))
            .map(|item| item.bytes).sum::<u64>(),
        "items": job.items.iter().enumerate().map(|(index, item)| json!({
            "sessionId": item.session_id, "relativePath": item.relative_path, "bytes": item.bytes,
            "status": item.result.as_deref().unwrap_or(if job.active == Some(index) {"running"}else{"pending"}),
            "error": item.error,
        })).collect::<Vec<_>>()
    })
}
/// Coalesce triggers from overlapping HTTP requests and retirements. A worker
/// scans fresh queue keys after every request that came in during its pass.
static WORKER: Mutex<Option<bool>> = Mutex::new(None);

fn spawn(home: PathBuf) -> anyhow::Result<()> {
    let mut state = WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(requested) = state.as_mut() {
        *requested = true;
        return Ok(());
    }
    let worker = std::thread::Builder::new()
        .name("archive-migration".into())
        .spawn(move || loop {
            if let Err(error) = drain(&home) {
                eprintln!("archive migration worker: {error:#}");
            }
            let mut state = WORKER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *state == Some(true) {
                *state = Some(false);
            } else {
                *state = None;
                break;
            }
        });
    worker?;
    *state = Some(false);
    Ok(())
}
pub(crate) fn resume_default() -> anyhow::Result<()> {
    let home = crate::config::paths::config_dir()?;
    prune_best_effort(&home);
    if home.join(STATE).join("queue").is_dir() {
        spawn(home.clone())?;
    }
    let location = super::archive_location::read(&home)?;
    if let Some(since) = location.selected_at.filter(|_| location.path.is_some()) {
        std::thread::Builder::new()
            .name("archive-migration-reconcile".into())
            .spawn(move || {
                if let Err(error) = reconcile(&home, since) {
                    eprintln!("automatic archive recovery: {error:#}");
                }
            })?;
    }
    Ok(())
}

fn reconcile(home: &Path, since: chrono::DateTime<chrono::Utc>) -> anyhow::Result<()> {
    let path = home.join("cutex-sessions.json");
    let mut after = None;
    loop {
        let Some(page) = crate::session::store::session_page_at(
            &path,
            true,
            after.as_deref(),
            crate::store::keyed::PageBudget {
                max_rows: 64,
                max_bytes: 1024 * 1024,
            },
        )?
        else {
            break;
        };
        ensure!(
            !page.oversize_first,
            "archive recovery row exceeds page budget"
        );
        for row in page.rows {
            let retired_at = row
                .retired_at
                .as_deref()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
            if retired_at.is_some_and(|at| at >= since) {
                after_archive(&row.cutex_session_id, row.durable_revision());
            }
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    Ok(())
}
pub fn after_archive(session_id: &str, revision: u64) {
    let result = (|| -> anyhow::Result<()> {
        let home = crate::config::paths::config_dir()?;
        let session = load_agent(&home, session_id)?;
        if !session.is_retired()
            || session.durable_revision() != revision
            || !crate::runtime::lifecycle::cutex_session_host_is_local(
                &session.host_id,
                &crate::platform::host::current_host_name(),
            )
            || session
                .explicit_launch
                .as_ref()
                .is_none_or(|contract| contract.version != 4)
            || records::record(&home, session_id)?.is_some_and(|record| record.side != Side::Native)
        {
            return Ok(());
        }
        // A deterministic request key makes retries of the same retirement inert.
        let id = auto_id(session_id, revision);
        let _requests = request_lock(&home)?;
        let Some(location) = configured_location(&home)? else {
            return Ok(());
        };
        if job_path(&home, id).exists()
            || records::auto_revision(&home, session_id)?.is_some_and(|saved| saved >= revision)
        {
            return Ok(());
        }
        let item = Item {
            session_id: session_id.into(),
            revision,
            relative_path: PathBuf::new(),
            bytes: 0,
            result: None,
            error: None,
        };
        let mut job = Job {
            id,
            state: State::Queued,
            location,
            created_at: chrono::Utc::now().timestamp(),
            finished_at: None,
            items: vec![item],
            active: None,
        };
        queue(&home, &mut job)?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("automatic archive migration: {error:#}");
    }
}

fn drain(home: &Path) -> anyhow::Result<()> {
    // Serialize each job with retries and other worker processes. Wait for
    // the shard so contention on an unrelated job cannot strand this key.
    let directory = home.join(STATE).join("queue");
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            continue;
        };
        let _guard = records::lock_wait(&job_lock_path(home, id))?;
        let Ok(mut job) = load_job(home, id) else {
            continue;
        };
        if job.state == State::Queued {
            for index in 0..job.items.len() {
                if job.items[index].result.is_some() {
                    continue;
                }
                job.active = Some(index);
                save_job(home, &job)?;
                let item = &mut job.items[index];
                match process_item(home, job.id, &job.location, item) {
                    Ok(()) if item.result.is_none() => item.result = Some("moved".into()),
                    Ok(()) => {}
                    Err(error) => {
                        item.result = Some("failed".into());
                        item.error = Some(format!("{error:#}"));
                    }
                }
                job.active = None;
                save_job(home, &job)?;
            }
            job.state = State::Finished;
            job.finished_at = Some(chrono::Utc::now().timestamp());
            save_job(home, &job)?;
        }
        if job.state == State::Finished {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    prune_best_effort(home);
    Ok(())
}

fn process_item(
    home: &Path,
    job: Uuid,
    location: &Location,
    item: &mut Item,
) -> anyhow::Result<()> {
    if item.relative_path.as_os_str().is_empty() {
        let Some(existing) = records::record(home, &item.session_id)?
            .filter(|record| record.side == Side::Archive && record.job == job)
        else {
            let view = inventory::inspect(
                home,
                serde_json::from_value(json!({"sessionIds":[item.session_id]}))?,
            )?;
            let Some(candidate) = prepare_items(home, &view)?.pop() else {
                item.result = Some("skipped".into());
                return Ok(());
            };
            ensure!(
                candidate.revision == item.revision,
                "automatic archive session changed"
            );
            *item = candidate;
            migrate(home, job, location, item)?;
            return Ok(());
        };
        item.relative_path = existing.manifest.relative_path;
        item.bytes = existing.manifest.bytes;
    }
    migrate(home, job, location, item)
}
fn migrate(home: &Path, job: Uuid, location: &Location, item: &Item) -> anyhow::Result<()> {
    let _session = records::session_lock(home, &item.session_id)?;
    let record = load_agent(home, &item.session_id)?;
    ensure!(
        record.is_retired() && record.durable_revision() == item.revision,
        "archive session changed since preview"
    );
    if let Some(existing) = records::record(home, &item.session_id)? {
        match existing.side {
            Side::Archive => {
                ensure!(
                    existing.job == job,
                    "session already uses its recorded archive location"
                );
                return clean_source(home, &record, &existing);
            }
            Side::NativePendingCleanup => {
                anyhow::bail!("previous archive restore cleanup is pending");
            }
            Side::Native => {}
        }
    }
    let _location = location.pin()?;
    let source_home = record
        .explicit_launch
        .as_ref()
        .context("native home missing")?
        .native_home
        .clone();
    let (manifest, source_entry) = transfer::digest(&source_home, &item.relative_path)?
        .context("native rollout is missing")?;
    ensure!(
        manifest.bytes == item.bytes,
        "native rollout size changed since preview"
    );
    let temporary = records::archive_path(job, &item.session_id, ".staging");
    let destination = records::archive_path(job, &item.session_id, "rollout.jsonl");
    let archive_entry =
        match transfer::verify_bytes(&location.path, &destination, &manifest.relative_path)? {
            Some((hash, entry)) => {
                ensure!(hash == manifest, "existing destination has different bytes");
                if let Some((hash, staged)) =
                    transfer::verify_bytes(&location.path, &temporary, &manifest.relative_path)?
                {
                    ensure!(hash == manifest, "archive staging has different bytes");
                    transfer::remove_verified(&location.path, &temporary, &manifest, &staged)?;
                }
                entry
            }
            None => {
                let staged = transfer::stage(
                    &source_home,
                    &item.relative_path,
                    &location.path,
                    &temporary,
                    &manifest,
                )?;
                #[cfg(test)]
                tests::checkpoint("stage");
                location.verify()?;
                transfer::publish(&location.path, &temporary, &destination, &manifest, &staged)?
            }
        };
    #[cfg(test)]
    tests::checkpoint("published");

    let value = Record {
        job,
        session_id: item.session_id.clone(),
        revision: item.revision,
        source_home,
        location: location.clone(),
        manifest_hash: manifest.hash()?,
        manifest,
        source_entry,
        archive_entry,
        side: Side::Archive,
    };
    if let Err(error) = cutover(home, &record, &value, true) {
        if let Err(cleanup) = remove_unreferenced_copy(home, &value) {
            eprintln!(
                "archive migration orphan cleanup for {}: {cleanup:#}",
                value.session_id
            );
        }
        return Err(error);
    }
    Ok(())
}

fn remove_unreferenced_copy(home: &Path, value: &Record) -> anyhow::Result<()> {
    // The caller still holds the session lock. A failed save can leave the
    // new record visible, so reread the authority before removing anything.
    if records::record(home, &value.session_id)?.is_some_and(|current| {
        current.side != Side::Native
            && current.job == value.job
            && current.location.path == value.location.path
    }) {
        return Ok(());
    }
    value.location.verify()?;
    transfer::remove_verified(
        &value.location.path,
        &value.destination(),
        &value.manifest,
        &value.archive_entry,
    )
}

fn cutover(
    home: &Path,
    record: &CutexSessionRecord,
    value: &Record,
    publish_record: bool,
) -> anyhow::Result<()> {
    // Open the reference lock before capturing directory metadata. Its first
    // creation would otherwise invalidate the runtime directory observation.
    drop(crate::runtime::reference_guard::acquire_shared()?);
    let (evidence, rollout) = super::storage_cleanup_homes::prepare_archive_cutover(home, record)?;
    ensure!(
        rollout.relative_path == value.manifest.relative_path
            && rollout.bytes == value.manifest.bytes,
        "native rollout inventory changed during copy"
    );
    // Close warm history before acquiring the host reference fence. The reader
    // opener must use a try lock, so it cannot deadlock with this session lock.
    let _reader = super::history::fence_offline_reader(&value.session_id);
    let _references = crate::runtime::reference_guard::acquire_exclusive()?;
    let _lifecycle = crate::session::lifecycle_gate::try_lock_for_cleanup()?;
    evidence.verify(home)?;
    verify_cutover(home, record, value)?;
    if publish_record {
        records::save(home, value)?;
        #[cfg(test)]
        tests::checkpoint("record");
    }
    delete_source(value)
}

fn clean_source(home: &Path, record: &CutexSessionRecord, value: &Record) -> anyhow::Result<()> {
    value.verify(record)?;
    let _location = value.location.pin()?;
    let archived_path = value.destination();
    let (hash, archived) = transfer::verify_bytes(
        &value.location.path,
        &archived_path,
        &value.manifest.relative_path,
    )?
    .context("archive rollout unavailable")?;
    ensure!(
        hash == value.manifest && archived == value.archive_entry,
        "archive rollout changed since verification"
    );
    let source = &value.manifest.relative_path;
    let Some((hash, entry)) = transfer::digest(&value.source_home, source)? else {
        return Ok(());
    };
    ensure!(
        hash == value.manifest && entry == value.source_entry,
        "original rollout changed"
    );
    cutover(home, record, value, false)
}

fn verify_cutover(
    home: &Path,
    expected: &CutexSessionRecord,
    value: &Record,
) -> anyhow::Result<()> {
    value.verify(expected)?;
    let record = load_agent(home, &value.session_id)?;
    ensure!(
        record == *expected && record.durable_revision() == value.revision,
        "archive session changed during copy"
    );
    ensure!(
        files::archive_entry(&value.source_home, &value.manifest.relative_path)?.as_ref()
            == Some(&value.source_entry),
        "original rollout changed during copy"
    );
    ensure!(
        files::archive_entry(&value.location.path, &value.destination())?.as_ref()
            == Some(&value.archive_entry),
        "archive rollout changed during copy"
    );
    let original = value.source_home.join(&value.manifest.relative_path);
    ensure!(
        !crate::launch::local_deployment::has_history_writer(&original)?,
        "native rollout still has a writer"
    );
    Ok(())
}

fn delete_source(value: &Record) -> anyhow::Result<()> {
    files::unlink_archive_file(
        &value.source_home,
        &value.manifest.relative_path,
        &value.source_entry,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, process::Command};

    const SESSION: &str = "cutex.archive-fixture";
    const NATIVE: &str = "11111111-1111-4111-8111-111111111111";
    const ENV: &str = "CUTEX_ARCHIVE_CRASH_CHILD_ROOT";

    thread_local! {
        static CHECKPOINT: Cell<Option<&'static str>> = const { Cell::new(None) };
    }

    pub(super) fn checkpoint(stage: &str) {
        if CHECKPOINT.with(|slot| slot.get() == Some(stage)) {
            // Crash evidence: the child leaves its file locks and durable
            // state at exactly this instruction, without unwinding.
            std::process::exit(86);
        }
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("cutex-arc-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }

        fn prepare(&self) -> (Job, PathBuf) {
            let home = self.0.join("home");
            let native = home.join("runtime/light/agents").join(NATIVE);
            let history =
                Path::new("sessions").join(format!("rollout-2026-11-25T11-22-33-{NATIVE}.jsonl"));
            let bytes = serde_json::to_vec(&json!({"type":"session_meta","payload":{"id":NATIVE}}))
                .unwrap();
            fs::create_dir_all(native.join("sessions")).unwrap();
            let original = native.join(&history);
            fs::write(
                &original,
                bytes.into_iter().chain([b'\n']).collect::<Vec<_>>(),
            )
            .unwrap();
            let mut session = CutexSessionRecord::new(
                SESSION.into(),
                Some(NATIVE.into()),
                crate::platform::host::current_host_name(),
                self.0.to_string_lossy().into(),
                None,
            )
            .unwrap();
            session.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
                version: 4,
                migration_action_id: None,
                native_id: NATIVE.into(),
                native_home: native.canonicalize().unwrap(),
                bundle_manifest: self.0.join("unused.json"),
                bundle_sha256: crate::role_revision::sha256_bytes(b"synthetic"),
            });
            session.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
            let session_db = home.join("cutex-sessions.json");
            crate::session::store::with_locked_session_rows(&session_db, |rows| {
                rows.insert_session(SESSION, session);
                rows.commit()?;
                Ok(())
            })
            .unwrap();
            let location =
                super::super::archive_location::validate(&home, &self.0.join("archive"), true)
                    .unwrap();
            let location = Location::bind(&home, &location).unwrap();
            let row = load_agent(&home, SESSION).unwrap();
            let job = Job {
                id: Uuid::new_v4(),
                state: State::Queued,
                location,
                created_at: chrono::Utc::now().timestamp(),
                finished_at: None,
                active: None,
                items: vec![Item {
                    session_id: SESSION.into(),
                    revision: row.durable_revision(),
                    relative_path: history,
                    bytes: original.metadata().unwrap().len(),
                    result: None,
                    error: None,
                }],
            };
            records::write(&queue_path(&home, job.id), &job.id.to_string()).unwrap();
            save_job(&home, &job).unwrap();
            fs::write(self.0.join(".archive-fixture"), job.id.to_string()).unwrap();
            (job, original)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            crate::session::store::close_session_store(&self.0.join("home/cutex-sessions.json"));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn failed_cutover_removes_published_copy_without_touching_source() {
        let fixture = Fixture::new();
        let (job, original) = fixture.prepare();
        let home = fixture.0.join("home");
        let unexpected = original.with_file_name("other-rollout.jsonl");
        fs::write(&unexpected, b"unexpected history").unwrap();
        let error = migrate(&home, job.id, &job.location, &job.items[0]).unwrap_err();
        assert!(error.to_string().contains("unexpected file"), "{error:#}");
        assert!(original.exists());
        assert!(unexpected.exists());
        assert!(records::record(&home, SESSION).unwrap().is_none());
        assert!(!job
            .location
            .path
            .join(records::archive_path(job.id, SESSION, "rollout.jsonl"))
            .exists());
    }

    #[test]
    fn archive_reference_evidence_rejects_new_native_history_and_new_session_row() {
        let fixture = Fixture::new();
        let (_, original) = fixture.prepare();
        let home = fixture.0.join("home");
        let row = load_agent(&home, SESSION).unwrap();
        let prepare = || {
            super::super::storage_cleanup_homes::prepare_archive_cutover(&home, &row)
                .unwrap()
                .0
        };
        let ticket = prepare();
        ticket.verify(&home).unwrap();
        let nested = original.parent().unwrap().join("new-history");
        fs::create_dir(&nested).unwrap();
        assert!(ticket.verify(&home).is_err());
        fs::remove_dir(nested).unwrap();

        let ticket = prepare();
        crate::session::store::with_locked_session_rows(
            &home.join("cutex-sessions.json"),
            |rows| {
                let new = CutexSessionRecord::new(
                    "cutex.reference-fixture".into(),
                    Some("22222222-2222-4222-8222-222222222222".into()),
                    crate::platform::host::current_host_name(),
                    fixture.0.to_string_lossy().into(),
                    None,
                )?;
                rows.insert_session("cutex.reference-fixture", new);
                rows.commit()?;
                Ok(())
            },
        )
        .unwrap();
        assert!(ticket.verify(&home).is_err());
    }

    #[test]
    fn lists_failed_automatic_job_and_rejects_retry_after_location_is_cleared() {
        let fixture = Fixture::new();
        let (mut job, _) = fixture.prepare();
        let home = fixture.0.join("home");
        job.id = auto_id(SESSION, job.items[0].revision);
        job.state = State::Finished;
        job.items[0].result = Some("failed".into());
        job.items[0].error = Some("fixture error".into());
        save_job(&home, &job).unwrap();
        let view = list(&home).unwrap();
        assert_eq!(view["jobs"].as_array().unwrap().len(), 2);
        let failed = view["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["automatic"] == true)
            .unwrap();
        assert_eq!(failed["id"], job.id.to_string());
        assert_eq!(failed["failedItems"], 1);
        let error = start(
            &home,
            StartRequest {
                preview_id: job.id,
                confirmed: true,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("archive location changed"));
        assert_eq!(
            load_job(&home, job.id).unwrap().items[0].error.as_deref(),
            Some("fixture error")
        );
    }

    #[test]
    fn prunes_expired_previews_and_old_finishes_without_replaying_automatic_job() {
        let fixture = Fixture::new();
        let (queued, _) = fixture.prepare();
        let home = fixture.0.join("home");
        let mut example = queued.clone();
        example.id = Uuid::new_v4();
        example.state = State::Preview;
        example.created_at -= PREVIEW_LIFETIME_SECONDS + 1;
        save_job(&home, &example).unwrap();
        records::write(&queue_path(&home, example.id), &example.id.to_string()).unwrap();
        let expired = example.id;
        example.state = State::Finished;
        example.id = auto_id(SESSION, example.items[0].revision);
        save_job(&home, &example).unwrap();
        let old_auto = example.id;
        for offset in 0..FINISHED_HISTORY {
            example.id = Uuid::new_v4();
            example.finished_at = Some(queued.created_at + offset as i64);
            save_job(&home, &example).unwrap();
        }
        prune_jobs(&home).unwrap();
        assert!(!job_path(&home, expired).exists());
        assert!(!queue_path(&home, expired).exists());
        assert!(!job_path(&home, old_auto).exists());
        assert_eq!(
            records::auto_revision(&home, SESSION).unwrap(),
            Some(queued.items[0].revision)
        );
        assert_eq!(load_job(&home, queued.id).unwrap().state, State::Queued);
        assert_eq!(
            list(&home).unwrap()["jobs"].as_array().unwrap().len(),
            FINISHED_HISTORY + 1
        );
    }

    #[test]
    fn archive_crash_child() {
        let Some(root) = std::env::var_os(ENV).map(PathBuf::from) else {
            return;
        };
        let root = root.canonicalize().unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert!(root.starts_with(&temp) && root != temp);
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("cutex-arc-"));
        assert!(root.join(".archive-fixture").is_file());
        let stage = match std::env::var("CUTEX_ARCHIVE_CRASH_STAGE").unwrap().as_str() {
            "stage" => "stage",
            "published" => "published",
            "record" => "record",
            _ => panic!("invalid checkpoint"),
        };
        CHECKPOINT.with(|slot| slot.set(Some(stage)));
        drain(&root.join("home")).unwrap();
        panic!("worker did not reach checkpoint: {stage}");
    }

    #[test]
    fn archive_crash_can_resume_before_or_after_record_switch() {
        for stage in ["stage", "published", "record"] {
            let fixture = Fixture::new();
            let (job, original) = fixture.prepare();
            let home = fixture.0.join("home");
            let initial = fs::read(&original).unwrap();
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management::v2::archive_jobs::tests::archive_crash_child",
                    "--nocapture",
                ])
                .env(ENV, &fixture.0)
                .env("CUTEX_ARCHIVE_CRASH_STAGE", stage)
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(86),
                "{}",
                String::from_utf8_lossy(&child.stdout)
            );
            assert_eq!(fs::read(&original).unwrap(), initial);
            assert_eq!(
                records::record(&home, SESSION).unwrap().is_some(),
                stage == "record"
            );
            drain(&home).unwrap();
            let after = load_job(&home, job.id).unwrap();
            assert_eq!(
                after.items[0].result.as_deref(),
                Some("moved"),
                "{:?}",
                after.items[0].error
            );
            assert!(!original.exists());
            assert!(!job
                .location
                .path
                .join(records::archive_path(job.id, SESSION, ".staging"))
                .exists());
            assert!(!queue_path(&home, job.id).exists());

            let row = load_agent(&home, SESSION).unwrap();
            let reader = fixture.0.join("reader");
            assert!(records::stage_reader_at(&home, &row, &reader).unwrap());
            assert_eq!(
                fs::read(reader.join(&job.items[0].relative_path)).unwrap(),
                initial
            );
            let stored = records::record(&home, SESSION).unwrap().unwrap();
            let archive = stored.location.path.join(stored.destination());
            let recovery_dir = fixture.0.join("delete-recovery");
            fs::create_dir(&recovery_dir).unwrap();
            let recovery = records::prepare_delete_recovery_at(&home, &row, &recovery_dir).unwrap();
            assert!(recovery.saved());
            assert_eq!(
                fs::read(
                    recovery_dir
                        .join("archive-history")
                        .join(&job.items[0].relative_path)
                )
                .unwrap(),
                initial
            );
            let saved: Record = records::read(&recovery_dir.join("archive-history/location.json"))
                .unwrap()
                .unwrap();
            assert_eq!(saved.manifest_hash, stored.manifest_hash);
            assert_eq!(saved.location, stored.location);
            drop(recovery);

            let missing = fixture.0.join("unmounted-fixture");
            fs::rename(&job.location.path, &missing).unwrap();
            assert!(records::restore_at(&home, &row)
                .unwrap_err()
                .to_string()
                .contains("archive location unavailable"));
            assert!(!original.exists());
            fs::rename(&missing, &job.location.path).unwrap();
            records::restore_at(&home, &row).unwrap();
            assert_eq!(fs::read(&original).unwrap(), initial);
            assert!(!archive.exists());
            records::restore_at(&home, &row).unwrap();
        }
    }
}
