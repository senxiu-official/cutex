use super::storage_cleanup_fs::{self as files, Entry};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
};

/// The manifest is versioned and currently contains one self-contained rollout.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Manifest {
    pub version: u32,
    pub relative_path: std::path::PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

impl Manifest {
    pub(super) fn hash(&self) -> anyhow::Result<String> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

fn fingerprint(
    path: &Path,
    mut reader: impl Read,
    mut writer: impl Write,
) -> anyhow::Result<Manifest> {
    let mut sha = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let data = &buffer[..count];
        bytes = bytes
            .checked_add(count as u64)
            .context("rollout length overflow")?;
        sha.update(data);
        writer.write_all(data)?;
    }
    Ok(Manifest {
        version: 1,
        relative_path: path.to_owned(),
        bytes,
        sha256: format!("{:x}", sha.finalize()),
    })
}

pub(super) fn verify_bytes(
    root: &Path,
    path: &Path,
    manifest_path: &Path,
) -> anyhow::Result<Option<(Manifest, Entry)>> {
    let Some(file) = files::open_archive_rollout_if_exists(root, path)? else {
        return Ok(None);
    };
    let before = files::archive_entry(root, path)?.context("rollout disappeared")?;
    let hash = fingerprint(manifest_path, &file, io::sink())?;
    ensure!(
        hash.bytes == file.metadata()?.len(),
        "rollout changed while hashing"
    );
    drop(file);
    ensure!(
        files::archive_entry(root, path)?.as_ref() == Some(&before),
        "rollout changed while hashing"
    );
    Ok(Some((hash, before)))
}

pub(super) fn digest(root: &Path, path: &Path) -> anyhow::Result<Option<(Manifest, Entry)>> {
    verify_bytes(root, path, path)
}
/// Copy to a private staging file, flush it, then hash it independently.
/// A retry uses the deterministic staging name and accepts only verified bytes.
pub(super) fn stage(
    from_root: &Path,
    from: &Path,
    to_root: &Path,
    stage_path: &Path,
    manifest: &Manifest,
) -> anyhow::Result<Entry> {
    if let Some((hash, entry)) = verify_bytes(to_root, stage_path, &manifest.relative_path)? {
        if hash == *manifest {
            return Ok(entry);
        }
        // The caller supplies a job-owned staging name. Only a stopped,
        // identifiable staging file may be removed to retry a partial copy.
        files::unlink_archive_file(to_root, stage_path, &entry)?;
    }
    let mut output = files::create_archive_file(to_root, stage_path)?;
    let input = files::open_archive_rollout(from_root, from)?;
    ensure!(
        fingerprint(&manifest.relative_path, input, &mut output)? == *manifest,
        "source changed during archive copy"
    );
    output.sync_all()?;
    drop(output);
    let (actual, entry) = verify_bytes(to_root, stage_path, &manifest.relative_path)?
        .context("archive staging file missing")?;
    ensure!(actual == *manifest, "archive verification failed");
    Ok(entry)
}

/// The authoritative record can only switch after this returns. If a crash
/// interrupted acknowledgement of a rename, verify the destination in place.
pub(super) fn publish(
    root: &Path,
    stage_path: &Path,
    final_path: &Path,
    manifest: &Manifest,
    staged_entry: &Entry,
) -> anyhow::Result<Entry> {
    if let Some((hash, entry)) = verify_bytes(root, final_path, &manifest.relative_path)? {
        ensure!(
            hash == *manifest,
            "existing destination has different bytes"
        );
        return Ok(entry);
    }
    files::publish_archive_file(root, stage_path, final_path, staged_entry)?;
    let (hash, entry) = verify_bytes(root, final_path, &manifest.relative_path)?
        .context("archive publication missing")?;
    ensure!(hash == *manifest, "published archive failed verification");
    Ok(entry)
}

pub(super) fn remove_verified(
    root: &Path,
    path: &Path,
    manifest: &Manifest,
    expected: &Entry,
) -> anyhow::Result<()> {
    if let Some((hash, entry)) = verify_bytes(root, path, &manifest.relative_path)? {
        ensure!(
            hash == *manifest && &entry == expected,
            "rollout changed since verification"
        );
        files::unlink_archive_file(root, path, expected)?;
    }
    Ok(())
}

pub(super) fn copy_for_reader(
    input: File,
    mut output: File,
    manifest: &Manifest,
) -> anyhow::Result<()> {
    ensure!(
        fingerprint(&manifest.relative_path, input, &mut output)? == *manifest,
        "archived history failed verification"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    struct Root(PathBuf);

    impl Root {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("archive-transfer-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn archive_transfer_resumes_partial_copy_and_published_rename() {
        let source = Root::new();
        let archive = Root::new();
        let path = Path::new("sessions/fixture.jsonl");
        let tmp = Path::new("archive/.stage-fixture");
        let dest = Path::new("archive/fixture.jsonl");
        fs::create_dir(source.0.join("sessions")).unwrap();
        fs::write(source.0.join(path), b"complete synthetic history\n").unwrap();
        let (hash, original) = digest(&source.0, path).unwrap().unwrap();

        let mut interrupted = files::create_archive_file(&archive.0, tmp).unwrap();
        interrupted.write_all(b"partial history").unwrap();
        interrupted.sync_all().unwrap();
        drop(interrupted);
        let staged = stage(&source.0, path, &archive.0, tmp, &hash).unwrap();
        let committed = publish(&archive.0, tmp, dest, &hash, &staged).unwrap();
        let archive_reader = Root::new();
        let reader_path = Path::new("sessions/copy.jsonl");
        copy_for_reader(
            files::open_archive_rollout(&archive.0, dest).unwrap(),
            files::create_archive_file(&archive_reader.0, reader_path).unwrap(),
            &hash,
        )
        .unwrap();
        assert_eq!(
            fs::read(archive_reader.0.join(reader_path)).unwrap(),
            b"complete synthetic history\n"
        );
        assert_eq!(hash.hash().unwrap().len(), 64);
        assert!(source.0.join(path).exists());
        assert_eq!(
            publish(&archive.0, tmp, dest, &hash, &staged).unwrap(),
            committed
        );

        remove_verified(&source.0, path, &hash, &original).unwrap();
        remove_verified(&source.0, path, &hash, &original).unwrap();
        let restored = stage(&archive.0, dest, &source.0, tmp, &hash).unwrap();
        publish(&source.0, tmp, path, &hash, &restored).unwrap();
        remove_verified(&archive.0, dest, &hash, &committed).unwrap();
        assert_eq!(digest(&source.0, path).unwrap().unwrap().0, hash);
        assert!(!archive.0.join(dest).exists());
    }

    #[test]
    fn archive_transfer_never_replaces_or_deletes_changed_destination() {
        let root = Root::new();
        let original = Path::new("original");
        let dest = Path::new("dest");
        let tmp = Path::new("temp");
        fs::write(root.0.join(original), b"expected").unwrap();
        let (hash, expected) = digest(&root.0, original).unwrap().unwrap();
        let staged = stage(&root.0, original, &root.0, tmp, &hash).unwrap();
        fs::write(root.0.join(dest), b"existing").unwrap();

        assert!(publish(&root.0, tmp, dest, &hash, &staged).is_err());
        fs::write(root.0.join(original), b"changed").unwrap();
        assert!(remove_verified(&root.0, original, &hash, &expected).is_err());
        assert_eq!(fs::read(root.0.join(dest)).unwrap(), b"existing");
        assert_eq!(fs::read(root.0.join(original)).unwrap(), b"changed");
    }
}
