use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{config::atomic::write_private_pretty_json_atomic, session::model::CutexSessionRecord};

use super::{
    archive_transfer::{self as transfer, Manifest},
    storage_cleanup_fs::{self as files, Entry},
};

pub(super) mod uuid_text {
    use serde::{de, Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        Uuid::parse_str(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}
pub(super) const STATE: &str = "runtime/archive-migration";
const MAX_RECORD: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Location {
    pub path: PathBuf,
    pub(super) identity: (u64, u64),
}

impl Location {
    pub(super) fn bind(home: &Path, path: &Path) -> anyhow::Result<Self> {
        let path = super::archive_location::validate(home, path, false)?;
        let identity = files::archive_directory_identity(&path)?;
        Ok(Self { path, identity })
    }

    pub(super) fn pin(&self) -> anyhow::Result<files::ArchiveDirectoryPin> {
        let pin =
            files::pin_archive_directory(&self.path).context("archive location unavailable")?;
        ensure!(
            pin.identity == self.identity,
            "archive location unavailable: physical directory changed"
        );
        Ok(pin)
    }

    pub(super) fn verify(&self) -> anyhow::Result<()> {
        self.pin()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Side {
    Archive,
    NativePendingCleanup,
    Native,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Record {
    #[serde(with = "uuid_text")]
    pub job: uuid::Uuid,
    pub session_id: String,
    pub revision: u64,
    pub source_home: PathBuf,
    pub location: Location,
    pub manifest: Manifest,
    pub manifest_hash: String,
    pub source_entry: Entry,
    pub archive_entry: Entry,
    pub side: Side,
}

impl Record {
    pub(super) fn destination(&self) -> PathBuf {
        archive_path(self.job, &self.session_id, "rollout.jsonl")
    }

    pub(super) fn verify(&self, session: &CutexSessionRecord) -> anyhow::Result<()> {
        self.verify_session(session)?;
        self.location.verify()
    }

    fn verify_session(&self, session: &CutexSessionRecord) -> anyhow::Result<()> {
        ensure!(
            self.session_id == session.cutex_session_id
                && session.is_retired()
                && session
                    .explicit_launch
                    .as_ref()
                    .is_some_and(|contract| contract.native_home == self.source_home),
            "archive session binding changed"
        );
        ensure!(
            self.manifest.hash()? == self.manifest_hash,
            "archive manifest hash changed"
        );
        Ok(())
    }
}

pub(super) fn archive_path(job: uuid::Uuid, id: &str, name: &str) -> PathBuf {
    Path::new(".cutex")
        .join("archive-history")
        .join(job.to_string())
        .join(key(id))
        .join(name)
}

fn key(id: &str) -> String {
    crate::role_revision::sha256_bytes(id.as_bytes())
        .as_str()
        .to_owned()
}

pub(super) fn read<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD,
        "archive record exceeds budget"
    );
    Ok(Some(
        serde_json::from_slice(&bytes).context("invalid archive record")?,
    ))
}

pub(super) fn write<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let parent = path.parent().context("archive state has no parent")?;
    crate::management::client_tokens::ensure_private_dir(parent)?;
    write_private_pretty_json_atomic(path, value, "archive migration record")
}

fn record_path(home: &Path, id: &str) -> PathBuf {
    home.join(STATE)
        .join("locations")
        .join(format!("{}.json", key(id)))
}

pub(super) fn record(home: &Path, id: &str) -> anyhow::Result<Option<Record>> {
    read(&record_path(home, id))
}

pub(super) fn save(home: &Path, value: &Record) -> anyhow::Result<()> {
    write(&record_path(home, &value.session_id), value)
}

fn auto_revision_path(home: &Path, id: &str) -> PathBuf {
    home.join(STATE)
        .join("auto-revisions")
        .join(format!("{}.json", key(id)))
}

pub(super) fn auto_revision(home: &Path, id: &str) -> anyhow::Result<Option<u64>> {
    read(&auto_revision_path(home, id))
}

pub(super) fn remember_auto_revision(home: &Path, id: &str, revision: u64) -> anyhow::Result<()> {
    if auto_revision(home, id)?.is_none_or(|saved| saved < revision) {
        write(&auto_revision_path(home, id), &revision)?;
    }
    Ok(())
}

fn open_lock(path: &Path) -> anyhow::Result<File> {
    let parent = path.parent().context("archive lock has no parent")?;
    crate::management::client_tokens::ensure_private_dir(parent)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    Ok(file)
}

pub(super) fn lock(path: &Path) -> anyhow::Result<File> {
    let file = open_lock(path)?;
    fs2::FileExt::try_lock_exclusive(&file).context("archive migration is busy")?;
    Ok(file)
}

pub(super) fn lock_wait(path: &Path) -> anyhow::Result<File> {
    let file = open_lock(path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

pub(super) fn session_lock(home: &Path, id: &str) -> anyhow::Result<File> {
    lock(
        &home
            .join(STATE)
            .join("locks")
            .join(format!("{}.lock", key(id))),
    )
}

/// Keep the lease alive until the isolated Native app-server has stopped.
pub struct ArchiveReaderHome {
    path: PathBuf,
    lease: Option<File>,
}

impl ArchiveReaderHome {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ArchiveReaderHome {
    fn drop(&mut self) {
        // Release the lease immediately before cleanup. A concurrent sweep
        // can win the removal race after this point; both own the same path.
        drop(self.lease.take());
        if let Err(error) = remove_reader_home(&self.path) {
            eprintln!(
                "archive reader cleanup for {}: {error:#}",
                self.path.display()
            );
        }
    }
}

fn remove_reader_home(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_dir() && !super::archive_location::linked(&meta),
                "archive reader home is not a private directory"
            );
            match fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reader_root(home: &Path) -> PathBuf {
    home.join("runtime").join("archive-readers")
}

fn validate_reader_root(root: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(root) {
        Ok(meta) => {
            ensure!(
                meta.is_dir() && !super::archive_location::linked(&meta),
                "archive readers root is not a private directory"
            );
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn create_reader_home(root: &Path) -> anyhow::Result<ArchiveReaderHome> {
    validate_reader_root(root)?;
    let _admission = lock_wait(&root.join(".admission.lock"))?;
    let path = root.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&path)?;
    let mut scratch = ArchiveReaderHome { path, lease: None };
    scratch.lease = Some(lock(&scratch.path.join(".lease"))?);
    Ok(scratch)
}

pub(crate) fn sweep_reader_homes() -> anyhow::Result<()> {
    sweep_reader_homes_at(&reader_root(&crate::config::paths::config_dir()?))
}

fn sweep_reader_homes_at(root: &Path) -> anyhow::Result<()> {
    if !validate_reader_root(root)? {
        return Ok(());
    }
    let _admission = lock_wait(&root.join(".admission.lock"))?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| uuid::Uuid::parse_str(name).ok())
        else {
            continue;
        };
        let path = entry.path();
        if name.to_str() != Some(&id.to_string()) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                eprintln!(
                    "archive reader startup inspection for {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        if !metadata.is_dir() || super::archive_location::linked(&metadata) {
            continue;
        }
        let lease = path.join(".lease");
        let removable = match fs::symlink_metadata(&lease) {
            Ok(meta) if meta.is_file() && !super::archive_location::linked(&meta) => {
                match OpenOptions::new().read(true).write(true).open(&lease) {
                    Ok(file) => match fs2::FileExt::try_lock_exclusive(&file) {
                        Ok(()) => true,
                        Err(error) if error.kind() == fs2::lock_contended_error().kind() => false,
                        Err(error) => {
                            eprintln!("archive reader lease for {}: {error}", path.display());
                            false
                        }
                    },
                    // A reader can drop its home while this sweep is inspecting it.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                    Err(error) => {
                        eprintln!("archive reader lease for {}: {error}", path.display());
                        false
                    }
                }
            }
            // Legacy scratch and a crash before lease creation are both stale.
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Ok(_) => false,
            Err(error) => {
                eprintln!("archive reader lease for {}: {error}", path.display());
                false
            }
        };
        if removable {
            if let Err(error) = remove_reader_home(&path) {
                eprintln!(
                    "archive reader startup cleanup for {}: {error:#}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

pub fn open_reader(session: &CutexSessionRecord) -> anyhow::Result<Option<ArchiveReaderHome>> {
    if !session.is_retired() {
        return Ok(None);
    }
    let home = crate::config::paths::config_dir()?;
    if record(&home, &session.cutex_session_id)?.is_none_or(|value| value.side != Side::Archive) {
        return Ok(None);
    }
    let scratch = create_reader_home(&reader_root(&home))?;
    Ok(stage_reader_at(&home, session, scratch.path())?.then_some(scratch))
}

pub(super) fn stage_reader_at(
    home: &Path,
    session: &CutexSessionRecord,
    reader_home: &Path,
) -> anyhow::Result<bool> {
    let Some(value) = record(home, &session.cutex_session_id)? else {
        return Ok(false);
    };
    if value.side != Side::Archive {
        return Ok(false);
    }
    let _session = session_lock(home, &session.cutex_session_id)?;
    let value =
        record(home, &session.cutex_session_id)?.context("archive location record changed")?;
    if value.side != Side::Archive {
        return Ok(false);
    }
    value.verify(session)?;
    let _location = value.location.pin()?;
    crate::management::client_tokens::ensure_private_dir(reader_home)?;
    let input = files::open_archive_rollout(&value.location.path, &value.destination())
        .context("archived history unavailable")?;
    let output = files::create_archive_file(reader_home, &value.manifest.relative_path)?;
    transfer::copy_for_reader(input, output, &value.manifest)?;
    Ok(true)
}

/// Pin an archive's placement through permanent deletion. For cold history,
/// the recovery directory receives the verified rollout and its location
/// record before the normal delete path can remove Native or session state.
pub struct ArchiveDeleteRecovery {
    _session_lock: File,
    saved: bool,
}

impl ArchiveDeleteRecovery {
    pub fn saved(&self) -> bool {
        self.saved
    }
}

pub fn prepare_delete_recovery(
    session: &CutexSessionRecord,
    directory: &Path,
) -> anyhow::Result<Option<ArchiveDeleteRecovery>> {
    if !session.is_retired() || session.explicit_launch.is_none() {
        return Ok(None);
    }
    let home = crate::config::paths::config_dir()?;
    prepare_delete_recovery_at(&home, session, directory).map(Some)
}

pub(super) fn prepare_delete_recovery_at(
    home: &Path,
    session: &CutexSessionRecord,
    directory: &Path,
) -> anyhow::Result<ArchiveDeleteRecovery> {
    let id = &session.cutex_session_id;
    let session_lock = session_lock(home, id)?;
    let mut recovery = ArchiveDeleteRecovery {
        _session_lock: session_lock,
        saved: false,
    };
    let Some(value) = record(home, id)?.filter(|value| value.side != Side::Native) else {
        return Ok(recovery);
    };
    value.verify_session(session)?;
    let destination = directory.join("archive-history");
    crate::management::client_tokens::ensure_private_dir(&destination)?;
    let relative = &value.manifest.relative_path;
    let stage = relative.with_file_name(".archive-recovery-staging");
    let (_location, from, from_path) = match value.side {
        Side::Archive => (
            Some(value.location.pin()?),
            &value.location.path,
            value.destination(),
        ),
        Side::NativePendingCleanup => (None, &value.source_home, relative.to_owned()),
        Side::Native => unreachable!("filtered Native records above"),
    };
    let entry = transfer::stage(from, &from_path, &destination, &stage, &value.manifest)?;
    transfer::publish(&destination, &stage, relative, &value.manifest, &entry)?;
    write(&destination.join("location.json"), &value)?;
    recovery.saved = true;
    Ok(recovery)
}

pub fn restore(session: &CutexSessionRecord) -> anyhow::Result<()> {
    let home = crate::config::paths::config_dir()?;
    restore_at(&home, session)
}

pub(super) fn restore_at(home: &Path, session: &CutexSessionRecord) -> anyhow::Result<()> {
    let id = &session.cutex_session_id;
    if record(home, id)?.is_none_or(|record| record.side == Side::Native) {
        return Ok(());
    }
    let session_lock = session_lock(home, id)?;
    let Some(mut value) = record(home, id)? else {
        return Ok(());
    };
    let _reader_fence = super::history::fence_offline_reader(id);
    if value.side == Side::Native {
        return Ok(());
    }
    value.verify(session)?;
    let _location = value.location.pin()?;
    if value.side == Side::Archive {
        let source = &value.manifest.relative_path;
        let temporary = source.with_file_name(format!(".cutex-restore-{}", value.job));
        if transfer::digest(&value.source_home, source)?
            .is_none_or(|(digest, _)| digest != value.manifest)
        {
            let entry = transfer::stage(
                &value.location.path,
                &value.destination(),
                &value.source_home,
                &temporary,
                &value.manifest,
            )?;
            transfer::publish(
                &value.source_home,
                &temporary,
                source,
                &value.manifest,
                &entry,
            )?;
        }
        value.side = Side::NativePendingCleanup;
        save(home, &value)?;
    }
    ensure!(
        transfer::digest(&value.source_home, &value.manifest.relative_path)?
            .is_some_and(|(hash, _)| hash == value.manifest),
        "restored Native history no longer matches the archive manifest"
    );
    value.location.verify()?;
    transfer::remove_verified(
        &value.location.path,
        &value.destination(),
        &value.manifest,
        &value.archive_entry,
    )?;
    value.side = Side::Native;
    save(home, &value)?;
    // Keep a durable local-side receipt until the next migration overwrites it;
    // a failed lifecycle CAS can retry without having to see the old location.
    drop(session_lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_reader_sweep_keeps_active_leases_and_removes_abandoned_copies() {
        let root = std::env::temp_dir().join(format!("cutex-reader-test-{}", uuid::Uuid::new_v4()));
        let active = create_reader_home(&root).unwrap();
        let stale_path = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&stale_path).unwrap();
        drop(lock(&stale_path.join(".lease")).unwrap());
        let legacy = root.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&legacy).unwrap();
        let foreign = root.join("foreign");
        fs::create_dir(&foreign).unwrap();

        sweep_reader_homes_at(&root).unwrap();
        assert!(active.path().is_dir());
        assert!(!stale_path.exists());
        assert!(!legacy.exists());
        assert!(foreign.is_dir());
        let active_path = active.path.clone();
        drop(active);
        assert!(!active_path.exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
