//! Occurrence-bound Native scratch. Only the generated ingress binding is
//! daemon scratch. Shell TMPDIR contents and pre-upgrade entries are distinct.
use super::{
    storage_cleanup::Candidate, storage_cleanup_fs as files, storage_cleanup_state as state,
};
use crate::agent_management::{StockPublication, StockRuntimeReceipt};
use crate::launch::stock::ExternalInputBinding;
use crate::session::model::{CutexAppServerRuntimeBinding, CutexSessionRecord, NativeStopReceipt};
use anyhow::{ensure, Context};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const ROOT: &str = "runtime/stock-tmp";
const MANAGED: &str = "occurrences";
const INGRESS: &str = "external-input.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Owner {
    claim: String,
    session: String,
    generation: u64,
    directory: String,
    shell: Option<(u64, u64)>,
    scratch: Option<(u64, u64)>,
    binding: Option<CutexAppServerRuntimeBinding>,
    publication: Option<StockPublication>,
    #[serde(default)]
    stopped: bool,
    owned_scratch: Option<files::Fingerprint>,
    moving: bool,
    done: bool,
}

fn id(runtime: &Path) -> anyhow::Result<&str> {
    let id = runtime
        .file_name()
        .and_then(OsStr::to_str)
        .context("stock runtime directory identity")?;
    ensure!(
        id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid stock runtime directory identity"
    );
    Ok(id)
}

fn base(directory: &str) -> PathBuf {
    Path::new(ROOT).join(MANAGED).join(directory)
}

fn load(home: &Path, directory: &str) -> anyhow::Result<Option<Owner>> {
    if !home.join("runtime/storage/storage.sqlite3").is_file() {
        return Ok(None);
    }
    let db = rusqlite::Connection::open_with_flags(
        home.join("runtime/storage/storage.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='stock_temp_owners' AND type='table')",
        [], |r| r.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let row: Option<String> = db
        .query_row(
            "SELECT value FROM stock_temp_owners WHERE id=?1",
            [directory],
            |r| r.get(0),
        )
        .optional()?;
    row.map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

fn owner_page(home: &Path, after: &str, limit: usize) -> anyhow::Result<Vec<Owner>> {
    if !home.join("runtime/storage/storage.sqlite3").is_file() {
        return Ok(Vec::new());
    }
    let db = state::database(home)?;
    let mut query =
        db.prepare("SELECT value FROM stock_temp_owners WHERE id>?1 ORDER BY id LIMIT ?2")?;
    let rows = query.query_map(params![after, i64::try_from(limit)?], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn save(home: &Path, owner: &Owner, create: bool) -> anyhow::Result<()> {
    let sql = if create {
        "INSERT INTO stock_temp_owners(id,value) VALUES(?1,?2)"
    } else {
        "UPDATE stock_temp_owners SET value=?2 WHERE id=?1"
    };
    ensure!(
        state::database(home)?
            .execute(sql, params![owner.directory, serde_json::to_string(owner)?])?
            == 1,
        "stock temp receipt changed"
    );
    Ok(())
}

pub struct Directories {
    pub shell: Option<PathBuf>,
    pub scratch: Option<PathBuf>,
}

pub fn register(
    runtime: &Path,
    receipt: &StockRuntimeReceipt,
    shell: bool,
    scratch: bool,
) -> anyhow::Result<Directories> {
    let home = crate::config::paths::config_dir()?;
    let _state_guard = files::pin_storage_state(&home)?;
    let directory = id(runtime)?.to_owned();
    let parent = base(&directory);
    let shell_path = shell.then(|| parent.join("shell"));
    let scratch_path = scratch.then(|| parent.join("scratch"));
    files::new_stock_directory(&home, &parent)?;
    let identity = |path: &PathBuf| files::new_stock_directory(&home, path);
    let owner = Owner {
        claim: receipt.claim_id.clone(),
        session: receipt.review.subject.cutex_session_id.as_str().to_owned(),
        generation: receipt.expected_generation,
        directory,
        shell: shell_path.as_ref().map(identity).transpose()?,
        scratch: scratch_path.as_ref().map(identity).transpose()?,
        binding: None,
        publication: None,
        stopped: false,
        owned_scratch: None,
        moving: false,
        done: false,
    };
    save(&home, &owner, true)?;
    Ok(Directories {
        shell: shell_path.map(|path| home.join(path)),
        scratch: scratch_path.map(|path| home.join(path)),
    })
}

pub fn bind(
    receipt: &StockRuntimeReceipt,
    binding: &CutexAppServerRuntimeBinding,
) -> anyhow::Result<()> {
    let home = crate::config::paths::config_dir()?;
    let _state_guard = files::pin_storage_state(&home)?;
    let mut owner =
        load(&home, id(Path::new(&binding.runtime_dir))?)?.context("stock temp receipt missing")?;
    ensure!(
        owner.claim == receipt.claim_id && owner.binding.is_none(),
        "stock temp receipt does not own this launch"
    );
    if let Some(identity) = owner.scratch {
        let path = base(&owner.directory).join("scratch");
        let expected = files::fingerprint(&home, &path)?;
        ensure!(
            expected.only_regular_child(INGRESS) && expected.root_identity() == Some(identity),
            "stock scratch changed before launch publication"
        );
        owner.owned_scratch = Some(expected);
    }
    owner.binding = Some(binding.clone());
    owner.publication = Some(
        receipt
            .publication
            .clone()
            .context("stock temp publication unbound")?,
    );
    save(&home, &owner, false)
}

/// Verify ingress for the exact published launch, including after the
/// cleanup database has been rebuilt. Resolving ingress never creates
/// ownership evidence for a cleanup.
pub fn ingress_directory(runtime: &Path, receipt: &StockRuntimeReceipt) -> anyhow::Result<PathBuf> {
    ingress_directory_at(&crate::config::paths::config_dir()?, runtime, receipt)
}

fn ingress_directory_at(
    home: &Path,
    runtime: &Path,
    receipt: &StockRuntimeReceipt,
) -> anyhow::Result<PathBuf> {
    let binding = ExternalInputBinding::from_review(receipt);
    let Ok(directory) = id(runtime) else {
        binding.verify(runtime)?;
        return Ok(runtime.into());
    };
    let path = base(directory).join("scratch");
    let selected = if let Some(owner) = load(home, directory)? {
        ensure!(
            owner.directory == directory
                && owner.claim == receipt.claim_id
                && owner.generation == receipt.expected_generation,
            "ingress scratch belongs to another launch"
        );
        ensure!(
            Some(files::stock_directory_identity(home, &path)?) == owner.scratch,
            "ingress scratch directory replaced"
        );
        home.join(path)
    } else {
        match files::stock_directory_identity(home, &path) {
            Ok(_) => {
                // The published binding supplies the exact runtime layout;
                // the owner-private ingress file supplies session/thread/generation.
                ensure!(
                    runtime == home.join("runtime/app-server").join(directory)
                        && receipt.binding.as_ref().is_some_and(|published| {
                            Path::new(&published.runtime_dir) == runtime
                        }),
                    "ingress scratch has no matching published runtime"
                );
                home.join(path)
            }
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                runtime.into()
            }
            Err(error) => return Err(error),
        }
    };
    binding.verify(&selected)?;
    Ok(selected)
}

fn stopped_occurrence(
    owner: &Owner,
    current: &CutexSessionRecord,
    latest_stop: Option<&NativeStopReceipt>,
) -> bool {
    let Some(binding) = &owner.binding else {
        return false;
    };
    let clear = current.runtime_generation > owner.generation
        || (current.runtime_generation == owner.generation
            && current.pending_launch_id.is_none()
            && !crate::session::archive::record_has_runtime_claim(current));
    clear
        && (owner.stopped
            || latest_stop.is_some_and(|stop| {
                stop.generation == owner.generation && stop.binding.as_ref() == Some(binding)
            }))
}

fn committed_stop(home: &Path, owner: &Owner) -> anyhow::Result<bool> {
    let proof = crate::session::store::stock_temp_stop_at(
        &home.join("cutex-sessions.json"),
        &owner.session,
    )?;
    Ok(proof.is_some_and(|(current, stop)| stopped_occurrence(owner, &current, stop.as_ref())))
}

#[cfg(any(windows, target_os = "linux"))]
fn original_pid_is_gone(binding: &CutexAppServerRuntimeBinding) -> anyhow::Result<bool> {
    let expected = chrono::DateTime::parse_from_rfc3339(&binding.started_at)?;
    match crate::platform::process::process_started_at(binding.pid) {
        Ok(actual) => Ok(actual != expected),
        Err(error) => {
            #[cfg(windows)]
            let absent = error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.raw_os_error() == Some(87))
            });
            #[cfg(target_os = "linux")]
            let absent = fs::symlink_metadata(format!("/proc/{}/stat", binding.pid))
                .is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
            if absent {
                Ok(true)
            } else {
                Err(error)
            }
        }
    }
}

fn lock_released(home: &Path, owner: &Owner) -> anyhow::Result<File> {
    let binding = owner.binding.as_ref().context("launch was not published")?;
    let publication = owner
        .publication
        .as_ref()
        .context("publication identity missing")?;
    ensure!(
        uuid::Uuid::parse_str(&owner.claim)?.to_string() == owner.claim,
        "invalid claim identity"
    );
    let path = Path::new("runtime/stock-claims").join(format!("{}.lock", owner.claim));
    let lease = files::stock_claim_file(home, &path, (publication.device, publication.inode))?;
    fs2::FileExt::try_lock_exclusive(&lease).context("launch lease is held")?;
    #[cfg(any(windows, target_os = "linux"))]
    ensure!(
        original_pid_is_gone(binding)?,
        "runtime process still holds the occurrence"
    );
    #[cfg(windows)]
    {
        let job_name = format!(
            "Global\\CutexRuntime-{:016x}{:016x}",
            publication.device, publication.inode
        );
        if let Some(job) = crate::platform::windows_job::open_agent_job(&job_name)? {
            ensure!(
                crate::platform::windows_job::active_processes(&job)? == 0,
                "stock Job Object is populated"
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        ensure!(
            binding.linux_scope,
            "stock runtime has no verified Linux scope"
        );
        let scope = crate::runtime::process_scope::LinuxRuntimeScope::new(
            &owner.session,
            "stock",
            Path::new(&binding.runtime_dir),
        );
        ensure!(scope.is_empty()?, "stock Linux scope is populated");
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (binding, lease);
        anyhow::bail!("runtime descendant proof is unavailable on this platform")
    }
    #[cfg(any(windows, target_os = "linux"))]
    Ok(lease)
}

fn blocked(relative: PathBuf, code: &str, reason: &str) -> Candidate {
    Candidate {
        stock_owner: None,
        relative,
        eligible: false,
        code: code.into(),
        reason: reason.into(),
    }
}

fn shell_owner(home: &Path, path: &Path) -> anyhow::Result<Option<Owner>> {
    let Some(rest) = path.strip_prefix(Path::new(ROOT).join(MANAGED)).ok() else {
        return Ok(None);
    };
    let mut parts = rest.iter();
    let Some(directory) = parts.next().and_then(OsStr::to_str) else {
        return Ok(None);
    };
    if parts.next() != Some(OsStr::new("shell")) || parts.next().is_some() {
        return Ok(None);
    }
    load(home, directory)
}

/// The lease survives the final rename. No other launch can reuse this
/// occurrence directory. The existing session-store fence detects mutations.
pub(super) struct ShellTicket {
    directory: (u64, u64),
    _lease: File,
}
impl ShellTicket {
    pub(super) fn unchanged(&self, home: &Path, path: &Path) -> anyhow::Result<bool> {
        Ok(files::stock_directory_identity(home, path)? == self.directory)
    }
}

fn shell_ticket(home: &Path, path: &Path, owner: &Owner) -> anyhow::Result<ShellTicket> {
    let directory = owner.shell.context("stock shell directory not recorded")?;
    ensure!(
        files::stock_directory_identity(home, path)? == directory,
        "stock shell directory replaced"
    );
    ensure!(
        committed_stop(home, owner)?,
        "stock runtime stop is unproven"
    );
    Ok(ShellTicket {
        directory,
        _lease: lock_released(home, owner)?,
    })
}

pub(super) fn prepare(home: &Path, path: &Path) -> anyhow::Result<ShellTicket> {
    let owner = shell_owner(home, path)?.context("stock temp ownership unknown")?;
    shell_ticket(home, path, &owner)
}

pub(super) fn candidates(
    home: &Path,
    deadline: Instant,
) -> anyhow::Result<(Vec<Candidate>, Option<&'static str>)> {
    let mut result = Vec::new();
    let mut partial = None;
    let limit = 800;
    let entries = match files::stock_children(home, Path::new(ROOT)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((result, None)),
        Err(error) => return Err(error.into()),
    };
    // Show launch-owned entries first so a large legacy directory cannot
    // crowd them out. Bare legacy names only ever produce blocked entries.
    let owners = owner_page(home, "", limit + 1)?;
    let recorded: std::collections::HashSet<_> =
        owners.iter().map(|owner| owner.directory.clone()).collect();
    for owner in owners {
        if Instant::now() >= deadline || result.len() == limit {
            partial = Some("item_limit");
            break;
        }
        let path = base(&owner.directory).join("shell");
        if owner.shell.is_none() || fs::symlink_metadata(home.join(&path)).is_err() {
            continue;
        }
        let expected = owner.shell.unwrap();
        let mut item = if files::stock_directory_identity(home, &path).ok() != Some(expected) {
            blocked(
                path,
                "stock_temp_replaced",
                "Recorded launch directory identity has changed",
            )
        } else {
            match shell_ticket(home, &path, &owner) {
                Ok(_) => Candidate {
                    stock_owner: None,
                    relative: path,
                    eligible: true,
                    code: "stock_temp_stopped".into(),
                    reason: format!(
                        "{} · generation {} · stopped; publication lease released",
                        owner.session, owner.generation
                    ),
                },
                Err(_) => blocked(
                    path,
                    "stock_temp_held",
                    &format!(
                        "{} · generation {} · no complete stop or holder evidence",
                        owner.session, owner.generation
                    ),
                ),
            }
        };
        item.stock_owner = Some((owner.session, owner.generation));
        result.push(item);
    }
    if partial.is_none() && entries.iter().any(|entry| entry == OsStr::new(MANAGED)) {
        let managed = Path::new(ROOT).join(MANAGED);
        if let Ok(children) = files::stock_children(home, &managed) {
            for entry in children {
                if Instant::now() >= deadline || result.len() == limit {
                    partial = Some("item_limit");
                    break;
                }
                if entry.to_str().is_some_and(|id| recorded.contains(id)) {
                    continue;
                }
                result.push(blocked(
                    managed.join(entry),
                    "stock_temp_unknown",
                    "No launch ownership receipt was published",
                ));
            }
        }
    }
    for entry in entries {
        if Instant::now() >= deadline || result.len() == limit {
            partial = Some("item_limit");
            break;
        }
        if entry == OsStr::new(MANAGED) {
            continue;
        }
        result.push(blocked(
            Path::new(ROOT).join(entry),
            "stock_temp_unknown",
            "Ownership unknown; older Native or tool temporary data",
        ));
    }
    Ok((result, partial))
}

fn reclaim(home: &Path, owner: &mut Owner) -> anyhow::Result<Option<u64>> {
    if owner.done || owner.scratch.is_none() || !committed_stop(home, owner)? {
        return Ok(None);
    }
    let _lease = lock_released(home, owner)?;
    owner.stopped = true;
    let path = base(&owner.directory).join("scratch");
    let trash = files::trash_relative(&owner.claim, &owner.claim)?;
    let expected = owner
        .owned_scratch
        .as_ref()
        .context("scratch fingerprint missing")?
        .clone();
    let trash_exists = match fs::symlink_metadata(home.join(&trash)) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if owner.moving
                && fs::symlink_metadata(home.join(&path))
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            {
                owner.done = true;
                owner.moving = false;
                save(home, owner, false)?;
                eprintln!(
                    "stock temp cleanup recovered absent paths: run={} generation={}",
                    owner.claim, owner.generation
                );
                return Ok(None);
            }
            false
        }
        Err(error) => return Err(error.into()),
    };
    let file_identity = expected
        .entry_identity(INGRESS)
        .context("owned stock scratch leaf missing from receipt")?;
    let scratch_file = if trash_exists {
        ensure!(owner.moving, "unrecorded scratch in quarantine");
        files::lock_stock_scratch(home, &trash.join(INGRESS), file_identity)?
    } else {
        let file = files::lock_stock_scratch(home, &path.join(INGRESS), file_identity)?;
        ensure!(
            file.is_some(),
            "recorded stock scratch file absent before staging"
        );
        file
    };
    if !trash_exists {
        let Some(prepared) =
            files::prepare_rename(home, &path, &owner.claim, &owner.claim, &expected)?
        else {
            anyhow::bail!("scratch moved or replaced before reclamation");
        };
        if !owner.moving {
            owner.moving = true;
            save(home, owner, false)?;
        }
        ensure!(
            super::storage_cleanup::move_stock_scratch(home, prepared, || committed_stop(
                home, owner
            ))?,
            "stock stop evidence or scratch identity changed during staging"
        );
    }
    // The staged path is private and detached from the runtime. In particular,
    // close the Windows handle before deleting its parent directory.
    drop(scratch_file);
    if files::delete_trash(home, &trash, &expected)? {
        owner.done = true;
        owner.moving = false;
        save(home, owner, false)?;
        return Ok(Some(expected.bytes));
    }
    Ok(None)
}

pub(super) fn drain(home: &Path) -> anyhow::Result<()> {
    let mut cursor = String::new();
    loop {
        let mut pending = owner_page(home, &cursor, 128)?;
        let Some(last) = pending.last() else {
            return Ok(());
        };
        cursor.clone_from(&last.directory);
        for owner in &mut pending {
            match reclaim(home, owner) {
                Ok(Some(bytes)) => eprintln!(
                    "stock temp reclaimed: run={} generation={} bytes={bytes}",
                    owner.claim, owner.generation,
                ),
                Ok(None) => {}
                Err(error) => eprintln!(
                    "stock temp retained: run={} generation={} reason={error}",
                    owner.claim, owner.generation,
                ),
            }
        }
    }
}

pub fn wake_after_stop(record: &crate::session::model::CutexSessionRecord) {
    let result = (|| {
        let home = crate::config::paths::config_dir()?;
        let Some(binding) = record.app_server_runtime.as_ref() else {
            return Ok(());
        };
        let Ok(directory) = id(Path::new(&binding.runtime_dir)) else {
            return Ok(());
        };
        if let Some(owner) = load(&home, directory)? {
            ensure!(
                owner.session == record.cutex_session_id
                    && owner.binding.as_ref() == Some(binding)
                    && owner.generation == record.runtime_generation,
                "stock temporary stop occurrence changed"
            );
            let _state_guard = files::pin_storage_state(&home)?;
            if !owner.stopped {
                // Merge just the stop fact. A recovery worker may have staged a move.
                state::database(&home)?.execute(
                    "UPDATE stock_temp_owners SET value=json_set(value,'$.stopped',json('true')) WHERE id=?1",
                    [directory],
                )?;
            }
            super::storage_cleanup::spawn_resume(&home)?;
        }
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = result {
        eprintln!("stock temp worker unavailable; evidence retained: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::model::CutexSessionStore;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("stock-temp-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }

        fn owner(&self) -> Owner {
            let home = &self.0;
            let directory = "2f47a8ba9631".to_owned();
            let claim = uuid::Uuid::new_v4().to_string();
            let path = Path::new("runtime/stock-claims").join(format!("{claim}.lock"));
            files::test_stock_claim(home, &path, b"private fixture").unwrap();
            let lock = files::test_stock_claim_identity(home, &path).unwrap();
            let scratch_path = base(&directory).join("scratch");
            let scratch = files::new_stock_directory(home, &scratch_path).unwrap();
            files::test_stock_claim(home, &scratch_path.join(INGRESS), b"rebuildable").unwrap();
            let fingerprint = files::fingerprint(home, &scratch_path).unwrap();
            let shell_path = base(&directory).join("shell");
            let shell = files::new_stock_directory(home, &shell_path).unwrap();
            fs::write(
                home.join(&shell_path).join("browser-data"),
                b"owned by shell",
            )
            .unwrap();
            let binding = serde_json::from_value(serde_json::json!({
                "transport":"unix_socket",
                "endpoint":"unix:///tmp/private-native.sock",
                "pid":std::process::id(),
                "runtime_dir":home.join("runtime/app-server").join(&directory),
                "diagnostic_journal_path":home.join("synthetic-journal"),
                "schema_version":"stock","schema_sha256":"a".repeat(64),
                "started_at":"2001-01-01T00:00:00Z",
                "linux_scope":true
            }))
            .unwrap();
            Owner {
                claim,
                session: "cutex.stock-temp-fixture".into(),
                generation: 7,
                directory,
                shell: Some(shell),
                scratch: Some(scratch),
                binding: Some(binding),
                publication: Some(StockPublication {
                    path: home.join(path),
                    device: lock.0,
                    inode: lock.1,
                }),
                stopped: false,
                owned_scratch: Some(fingerprint),
                moving: false,
                done: false,
            }
        }

        fn publish(&self, owner: &Owner, runtime_active: bool) -> CutexSessionStore {
            let _pin = files::pin_storage_state(&self.0).unwrap();
            save(&self.0, owner, true).unwrap();
            let mut store = CutexSessionStore::default();
            let mut record = CutexSessionRecord::new(
                owner.session.clone(),
                None,
                "stock temp fixture".into(),
                self.0.to_string_lossy().into(),
                None,
            )
            .unwrap();
            record.runtime_generation = owner.generation;
            if runtime_active {
                record.app_server_runtime = owner.binding.clone();
            }
            store.sessions.insert(owner.session.clone(), record);
            store.native_stop_receipts.insert(
                owner.session.clone(),
                NativeStopReceipt {
                    native_id: None,
                    generation: owner.generation,
                    binding: owner.binding.clone(),
                },
            );
            crate::session::store::save_cutex_session_store_to_path(
                &self.0.join("cutex-sessions.json"),
                &store,
            )
            .unwrap();
            store
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn published_ingress_reconnects_after_ownership_record_is_lost() {
        use serde_json::json;

        let fixture = Fixture::new();
        let home = &fixture.0;
        let owner = fixture.owner();
        let runtime = Path::new(&owner.binding.as_ref().unwrap().runtime_dir);
        let receipt: StockRuntimeReceipt = serde_json::from_value(json!({
            "action_id":"stock-temp-reconnect",
            "review":{
                "subject":{
                    "cutex_session_id":owner.session,"formal_name":"fixture",
                    "durable_sha256":"a".repeat(64),"authority_sha256":"a".repeat(64),
                    "current_project_id":null,"revision":1,"runtime_generation":6
                },
                "contract":{
                    "version":2,"native_id":"2de32ab1-6e84-42bd-86d1-15a62ece9c31",
                    "native_home":home,"bundle_manifest":home.join("manifest"),
                    "bundle_sha256":"b".repeat(64)
                },
                "configuration":{
                    "profile_name":"alpha","profile_id":"private-profile","inherited":false,
                    "profile_sha256":"c".repeat(64),"account_sha256":"d".repeat(64),
                    "model":"private-model","model_provider":"private",
                    "provider":{"name":"private","base_url":"http://127.0.0.1:1/v1",
                        "wire_api":"responses","requires_openai_auth":false,"supports_websockets":false},
                    "sandbox":"read-only","approval":"on-request"
                },
                "restart":true
            },
            "stage":"ready","claim_id":owner.claim,"runtime_agent_id":"stock.fixture",
            "expected_generation":owner.generation,"binding":owner.binding,
            "publication":owner.publication,"error":null,"updated_at":"2026-11-24T00:00:00Z"
        })).unwrap();
        let scratch = home.join(base(&owner.directory).join("scratch"));
        fs::remove_file(scratch.join(INGRESS)).unwrap();
        let expected = ExternalInputBinding::from_review(&receipt);
        expected.stage(&scratch).unwrap();
        assert_eq!(
            ingress_directory_at(home, runtime, &receipt).unwrap(),
            scratch
        );
        assert!(!home.join("runtime/storage/storage.sqlite3").exists());

        fixture.publish(&owner, true);
        assert_eq!(
            ingress_directory_at(home, runtime, &receipt).unwrap(),
            scratch
        );

        // Rebuilding the database leaves an empty owner table.
        state::database(home)
            .unwrap()
            .execute(
                "DELETE FROM stock_temp_owners WHERE id=?1",
                [&owner.directory],
            )
            .unwrap();
        assert_eq!(
            ingress_directory_at(home, runtime, &receipt).unwrap(),
            scratch
        );
        assert!(load(home, &owner.directory).unwrap().is_none());
        assert!(prepare(home, &base(&owner.directory).join("shell")).is_err());

        // A private but stale binding cannot authenticate a different launch.
        let mut wrong_receipt = receipt.clone();
        wrong_receipt.expected_generation += 1;
        assert!(ingress_directory_at(home, runtime, &wrong_receipt).is_err());
        wrong_receipt = receipt.clone();
        wrong_receipt
            .binding
            .as_mut()
            .unwrap()
            .runtime_dir
            .push_str("-unrelated");
        assert!(ingress_directory_at(home, runtime, &wrong_receipt).is_err());

        // A pre-upgrade launch has only the old runtime-dir ingress file.
        fs::remove_file(scratch.join(INGRESS)).unwrap();
        fs::remove_dir(&scratch).unwrap();
        files::new_stock_directory(
            home,
            &Path::new("runtime/app-server").join(&owner.directory),
        )
        .unwrap();
        expected.stage(runtime).unwrap();
        assert_eq!(
            ingress_directory_at(home, runtime, &receipt).unwrap(),
            runtime
        );
    }

    #[test]
    fn owner_preview_reports_legacy_scratch_as_unknown() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let old = Path::new(ROOT).join("old-browser");
        files::new_stock_directory(home, &old).unwrap();
        fs::write(
            home.join(&old).join("profile"),
            b"legacy potentially valuable",
        )
        .unwrap();
        let result = super::super::storage_cleanup::preview(
            home,
            serde_json::json!({"action":"stock-tmp"}),
            serde_json::json!({"principal":"owner","clientName":"fixture","deviceId":null}),
        )
        .unwrap();
        let item = &result["items"][0];
        assert_eq!(item["code"], "stock_temp_unknown");
        assert_eq!(item["blockingReason"], "stock_temp_unknown");
        assert!(item.get("sessionId").is_none());
        assert!(item.get("generation").is_none());
        assert_eq!(item["eligible"], false);
        assert_eq!(item["bytes"], b"legacy potentially valuable".len());
        assert_eq!(result["freeableBytes"], 0);
        assert!(home.join(old).exists());
    }

    #[test]
    fn stock_temp_preview_exposes_recorded_owner_and_final_blocking_code() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        fixture.publish(&owner, true);
        let preview = || {
            super::super::storage_cleanup::preview(
                &fixture.0,
                serde_json::json!({"action":"stock-tmp"}),
                serde_json::json!({"principal":"owner","clientName":"fixture","deviceId":null}),
            )
            .unwrap()
        };
        let held = preview();
        let item = &held["items"][0];
        assert_eq!(item["eligible"], false);
        assert_eq!(item["sessionId"], owner.session);
        assert_eq!(item["generation"], owner.generation);
        assert_eq!(item["blockingReason"], "stock_temp_held");
        assert_eq!(item["blockingReason"], item["code"]);
        assert!(item["reason"]
            .as_str()
            .unwrap()
            .contains("no complete stop or holder evidence"));

        let shell = base(&owner.directory).join("shell");
        let source = fixture.0.join(&shell);
        fs::rename(&source, source.with_extension("prior")).unwrap();
        files::new_stock_directory(&fixture.0, &shell).unwrap();
        let replaced = preview();
        let item = &replaced["items"][0];
        assert_eq!(item["sessionId"], owner.session);
        assert_eq!(item["generation"], owner.generation);
        assert_eq!(item["blockingReason"], "stock_temp_replaced");
        assert_eq!(item["code"], "stock_temp_replaced");
    }

    #[test]
    fn pending_and_same_generation_claims_are_protected() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let mut store = fixture.publish(&owner, true);
        assert!(!stopped_occurrence(
            &owner,
            store.sessions.get(&owner.session).unwrap(),
            store.native_stop_receipts.get(&owner.session)
        ));
        let record = store.sessions.get_mut(&owner.session).unwrap();
        record.app_server_runtime = None;
        record.pending_launch_id = Some("another launch".into());
        assert!(!stopped_occurrence(
            &owner,
            store.sessions.get(&owner.session).unwrap(),
            store.native_stop_receipts.get(&owner.session)
        ));
        let record = store.sessions.get_mut(&owner.session).unwrap();
        record.pending_launch_id = None;
        assert!(stopped_occurrence(
            &owner,
            store.sessions.get(&owner.session).unwrap(),
            store.native_stop_receipts.get(&owner.session)
        ));
        let record = store.sessions.get_mut(&owner.session).unwrap();
        record.runtime_generation += 1;
        record.app_server_launch_claim_id = Some("new claim".into());
        assert_ne!(
            record.app_server_launch_claim_id.as_deref(),
            Some(owner.claim.as_str())
        );
        assert!(stopped_occurrence(
            &owner,
            store.sessions.get(&owner.session).unwrap(),
            store.native_stop_receipts.get(&owner.session)
        ));
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn exact_pid_is_live_but_reused_pid_is_not_the_old_owner() {
        let fixture = Fixture::new();
        let mut owner = fixture.owner();
        let binding = owner.binding.as_mut().unwrap();
        assert!(original_pid_is_gone(binding).unwrap());
        binding.started_at = crate::platform::process::process_started_at(binding.pid)
            .unwrap()
            .to_rfc3339();
        assert!(!original_pid_is_gone(binding).unwrap());
    }

    #[test]
    fn replaced_claim_and_directory_are_rejected() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let publication = owner.publication.as_ref().unwrap();
        let relative = publication.path.strip_prefix(&fixture.0).unwrap();
        let old = publication.path.with_extension("previous");
        fs::rename(&publication.path, &old).unwrap();
        files::test_stock_claim(&fixture.0, relative, b"new inode").unwrap();
        assert!(files::stock_claim_file(
            &fixture.0,
            relative,
            (publication.device, publication.inode)
        )
        .is_err());
        let shell = base(&owner.directory).join("shell");
        fs::rename(
            fixture.0.join(&shell),
            fixture.0.join(&shell).with_extension("previous"),
        )
        .unwrap();
        assert_ne!(
            Some(files::new_stock_directory(&fixture.0, &shell).unwrap()),
            owner.shell
        );
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn an_open_scratch_handle_or_lease_is_protected() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let file = base(&owner.directory).join("scratch").join(INGRESS);
        let id = owner
            .owned_scratch
            .as_ref()
            .unwrap()
            .entry_identity(INGRESS)
            .unwrap();
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1 | 4);
        }
        let reader = options.open(fixture.0.join(&file)).unwrap();
        #[cfg(unix)]
        fs2::FileExt::lock_shared(&reader).unwrap();
        assert!(files::lock_stock_scratch(&fixture.0, &file, id).is_err());
        drop(reader);
        assert!(files::lock_stock_scratch(&fixture.0, &file, id)
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn directory_link_cannot_claim_an_occurrence() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let path = base(&owner.directory).join("shell");
        let shell = fixture.0.join(&path);
        let target = shell.with_extension("target");
        fs::rename(&shell, &target).unwrap();
        std::os::unix::fs::symlink(&target, &shell).unwrap();
        assert!(files::stock_directory_identity(&fixture.0, &path).is_err());
        assert!(target.join("browser-data").is_file());
    }

    #[cfg(windows)]
    #[test]
    fn junction_cannot_claim_an_occurrence() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let relative = base(&owner.directory).join("shell");
        let shell = fixture.0.join(&relative);
        let target = shell.with_extension("target");
        fs::rename(&shell, &target).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&shell)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(files::stock_directory_identity(&fixture.0, &relative).is_err());
        assert!(target.join("browser-data").is_file());
    }

    #[cfg(windows)]
    #[test]
    fn active_job_keeps_scratch_and_resume_after_rename_keeps_shell() {
        use std::{os::windows::process::CommandExt, process::Command};
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        let fixture = Fixture::new();
        let mut owner = fixture.owner();
        fixture.publish(&owner, false);
        let publication = owner.publication.as_ref().unwrap();
        let job_name = format!(
            "Global\\CutexRuntime-{:016x}{:016x}",
            publication.device, publication.inode
        );
        let job = crate::platform::windows_job::create_agent_job(&job_name).unwrap();
        let exe = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut child = Command::new(exe)
            .args(["-NoProfile", "-Command", "Start-Sleep 60"])
            .creation_flags(CREATE_SUSPENDED)
            .spawn()
            .unwrap();
        let adopted = crate::platform::windows_job::adopt_suspended_child(&job, &child);
        if let Err(error) = adopted {
            let _ = child.kill();
            let _ = child.wait();
            panic!("private Job Object fixture failed: {error}");
        }
        let refused = reclaim(&fixture.0, &mut owner);
        crate::platform::windows_job::terminate_job(&job).unwrap();
        child.wait().unwrap();
        assert!(
            crate::platform::windows_job::wait_for_job_empty(&job, Duration::from_secs(5)).unwrap()
        );
        assert!(refused
            .unwrap_err()
            .to_string()
            .contains("Job Object is populated"));

        let scratch_path = base(&owner.directory).join("scratch");
        let expected = owner.owned_scratch.as_ref().unwrap();
        owner.moving = true;
        save(&fixture.0, &owner, false).unwrap();
        let prepared = files::prepare_rename(
            &fixture.0,
            &scratch_path,
            &owner.claim,
            &owner.claim,
            expected,
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            files::rename_prepared(prepared, Instant::now() + Duration::from_secs(5)).unwrap(),
            files::MoveOutcome::Moved { .. }
        ));
        drop(owner);
        drain(&fixture.0).unwrap();
        assert!(!fixture.0.join(scratch_path).exists());
        let owner = owner_page(&fixture.0, "", 4).unwrap().pop().unwrap();
        assert!(owner.done);
        assert!(fixture
            .0
            .join(base(&owner.directory).join("shell/browser-data"))
            .is_file());
        assert!(prepare(&fixture.0, &base(&owner.directory).join("shell")).is_ok());
        let preview = super::super::storage_cleanup::preview(
            &fixture.0,
            serde_json::json!({"action":"stock-tmp"}),
            serde_json::json!({"principal":"owner","clientName":"fixture","deviceId":null}),
        )
        .unwrap();
        let item = &preview["items"][0];
        assert_eq!(item["eligible"], true);
        assert_eq!(item["sessionId"], owner.session);
        assert_eq!(item["generation"], owner.generation);
        assert_eq!(item["code"], "stock_temp_stopped");
        assert!(item.get("blockingReason").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_systemd_scope_keeps_scratch() {
        if std::env::var_os("CUTEX_STOCK_TEMP_SCOPE_TEST").is_none() {
            eprintln!("real scope test requires CUTEX_STOCK_TEMP_SCOPE_TEST");
            return;
        }
        let fixture = Fixture::new();
        let mut owner = fixture.owner();
        fixture.publish(&owner, false);
        let binding = owner.binding.as_ref().unwrap();
        let scope = crate::runtime::process_scope::LinuxRuntimeScope::new(
            &owner.session,
            "stock",
            Path::new(&binding.runtime_dir),
        );
        let mut child = std::process::Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--unit",
                scope.unit(),
                "/bin/sleep",
                "20",
            ])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while scope.is_empty().unwrap() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let refused = reclaim(&fixture.0, &mut owner);
        scope.terminate(true).unwrap();
        child.wait().unwrap();
        assert!(refused
            .unwrap_err()
            .to_string()
            .contains("Linux scope is populated"));
        assert_eq!(reclaim(&fixture.0, &mut owner).unwrap(), Some(11));
        assert!(fixture
            .0
            .join(base(&owner.directory).join("shell/browser-data"))
            .is_file());
    }
}
