//! Canonical offline component kits: preserve signed source bytes, never repack
//! or select a runtime. Host policy/identity-ledger acceptance belongs to callers.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component as PathComponent, Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::index_model::{Index, IndexAsset};
use crate::upgrade::minisign::{self, PublicKey};
use crate::upgrade::source::Source;

pub const KIT_LOCK_FILE: &str = "kit-lock.json";
const INDEX_TAG: &str = "channel-components";
const INDEX_FILE: &str = "index.json";
const INDEX_SIGNATURE: &str = "index.json.minisig";
const KIT_SCHEMA: &str = "cutex/component-kit/v1";
const MAX_DOCUMENT_BYTES: usize = crate::upgrade::manifest::MAX_DOCUMENT_BYTES;
const MAX_SIGNATURE_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KitLock {
    pub schema: String,
    pub channel: String,
    pub sequence: u64,
    pub index_sha256: String,
    pub assets: Vec<KitAsset>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KitAsset {
    pub component: String,
    pub version: String,
    pub platform: String,
    pub tag: String,
    pub asset: IndexAsset,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn signed_index(bytes: &[u8], signature: &[u8], keys: &[PublicKey]) -> anyhow::Result<Index> {
    minisign::verify_text(
        keys,
        bytes,
        std::str::from_utf8(signature).context("index signature is not UTF-8")?,
    )?;
    Index::parse(bytes)
}

// Return identities from the validated index, never paths supplied by a kit lock.
fn selected(
    index: &Index,
    component: &str,
    version: &str,
    platform: &str,
) -> anyhow::Result<KitAsset> {
    let component = index
        .components
        .iter()
        .find(|row| row.name == component)
        .context("selected component is absent from signed index")?;
    let release = component
        .releases
        .iter()
        .find(|row| row.version == version)
        .context("selected version is absent from signed index")?;
    ensure!(!release.yanked, "selected component version is withdrawn");
    let asset = release
        .platforms
        .get(platform)
        .context("selected platform is absent from signed index")?;
    Ok(KitAsset {
        component: component.name.clone(),
        version: release.version.clone(),
        platform: platform.into(),
        tag: release.asset_tag.clone(),
        asset: asset.clone(),
    })
}

fn unique_assets(assets: &[KitAsset]) -> anyhow::Result<()> {
    ensure!(
        !assets.is_empty(),
        "component kit selections must not be empty"
    );
    let mut identities = BTreeSet::new();
    let mut paths = BTreeMap::new();
    for item in assets {
        ensure!(
            identities.insert((&item.component, &item.version, &item.platform)),
            "duplicate kit selection"
        );
        if let Some(prior) = paths.insert((&item.tag, &item.asset.name), &item.asset) {
            ensure!(
                prior == &item.asset,
                "conflicting signed assets share a kit path"
            );
        }
    }
    Ok(())
}

fn absent(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => anyhow::bail!("destination already exists: {}", path.display()),
        Err(error) => Err(error).context("inspect kit destination"),
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn regular_file(path: &Path) -> anyhow::Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "kit input must be a regular non-symlink file: {}",
        path.display()
    );
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> anyhow::Result<Vec<u8>> {
    regular_file(path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "kit document exceeds size limit: {}",
        path.display()
    );
    Ok(bytes)
}

fn tag_directory(root: &Path, tag: &str) -> anyhow::Result<PathBuf> {
    let dir = root.join(tag);
    ensure!(
        fs::symlink_metadata(&dir)?.file_type().is_dir(),
        "kit tag must be a non-symlink directory"
    );
    Ok(dir)
}

fn verify_asset(root: &Path, item: &KitAsset, keys: &[PublicKey]) -> anyhow::Result<()> {
    let dir = tag_directory(root, &item.tag)?;
    let path = dir.join(&item.asset.name);
    regular_file(&path)?;
    let mut file = fs::File::open(&path)?;
    ensure!(
        file.metadata()?.len() == item.asset.size,
        "kit archive size mismatch"
    );
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        ensure!(
            size <= item.asset.size,
            "kit archive grew beyond signed size"
        );
        hash.update(&buffer[..count]);
    }
    ensure!(
        size == item.asset.size && format!("{:x}", hash.finalize()) == item.asset.sha256,
        "kit archive SHA256 or size mismatch"
    );
    let signature = read_bounded(&dir.join(&item.asset.signature_name), MAX_SIGNATURE_BYTES)?;
    minisign::verify_file(
        keys,
        &path,
        std::str::from_utf8(&signature).context("archive signature is not UTF-8")?,
    )?;
    Ok(())
}

/// Freeze only explicitly chosen signed identities. Failures leave the unique
/// owned sibling staging directory in place and identify it in the error.
pub fn freeze(
    source: &Source,
    keys: &[PublicKey],
    channel: &str,
    destination: &Path,
    selections: &[(String, String, String)],
) -> anyhow::Result<Value> {
    ensure!(
        destination.is_absolute(),
        "kit destination must be absolute"
    );
    ensure!(
        !destination
            .components()
            .any(|part| matches!(part, PathComponent::ParentDir | PathComponent::CurDir)),
        "kit destination must not contain dot components"
    );
    let name = destination
        .file_name()
        .context("kit destination must name a new directory")?;
    let parent = fs::canonicalize(
        destination
            .parent()
            .context("kit destination needs a parent")?,
    )?;
    ensure!(
        parent.is_dir(),
        "kit destination parent must be an existing directory"
    );
    let destination = parent.join(name);
    absent(&destination)?;

    let bytes = source.fetch(INDEX_TAG, INDEX_FILE, MAX_DOCUMENT_BYTES)?;
    let signature = source.fetch(INDEX_TAG, INDEX_SIGNATURE, MAX_SIGNATURE_BYTES)?;
    let index = signed_index(&bytes, &signature, keys)?;
    ensure!(index.channel == channel, "component index channel mismatch");
    let assets = selections
        .iter()
        .map(|(component, version, platform)| selected(&index, component, version, platform))
        .collect::<anyhow::Result<Vec<_>>>()?;
    unique_assets(&assets)?;
    let lock = KitLock {
        schema: KIT_SCHEMA.into(),
        channel: index.channel,
        sequence: index.sequence,
        index_sha256: digest(&bytes),
        assets,
    };
    let lock_bytes = serde_json::to_vec_pretty(&lock)?;
    ensure!(
        lock_bytes.len() <= MAX_DOCUMENT_BYTES,
        "component kit lock exceeds size limit"
    );
    let staging = parent.join(format!(".component-kit-staging-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging)?;
    (|| -> anyhow::Result<Value> {
        let index_dir = staging.join(INDEX_TAG);
        fs::create_dir(&index_dir)?;
        write_new(&index_dir.join(INDEX_FILE), &bytes)?;
        write_new(&index_dir.join(INDEX_SIGNATURE), &signature)?;
        let mut downloaded = BTreeSet::new();
        for item in &lock.assets {
            if !downloaded.insert((&item.tag, &item.asset.name)) {
                continue;
            }
            let dir = staging.join(&item.tag);
            if item.tag != INDEX_TAG {
                fs::create_dir_all(&dir)?;
            }
            let path = dir.join(&item.asset.name);
            source.download(
                &item.tag,
                &item.asset.name,
                &path,
                item.asset.size,
                &item.asset.sha256,
            )?;
            let signature =
                source.fetch(&item.tag, &item.asset.signature_name, MAX_SIGNATURE_BYTES)?;
            minisign::verify_file(
                keys,
                &path,
                std::str::from_utf8(&signature).context("archive signature is not UTF-8")?,
            )?;
            write_new(&dir.join(&item.asset.signature_name), &signature)?;
        }
        write_new(&staging.join(KIT_LOCK_FILE), &lock_bytes)?;
        verify(&staging, keys)?;
        publish_new(&staging, &destination)?;
        Ok(json!({"root": destination, "lock": lock}))
    })()
    .with_context(|| {
        format!(
            "component kit freeze failed; staging retained at {}",
            staging.display()
        )
    })
}

// Do not replace even an empty destination another process creates during the
// downloads. Atomic exclusive rename also avoids a check-then-rename gap.
#[cfg(target_os = "linux")]
fn publish_new(staging: &Path, destination: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(staging.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("publish new component kit without replacement");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn publish_new(staging: &Path, destination: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(staging.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("publish new component kit without replacement");
    }
    Ok(())
}

#[cfg(windows)]
fn publish_new(staging: &Path, destination: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let from: Vec<u16> = staging.as_os_str().encode_wide().collect();
    let to: Vec<u16> = destination.as_os_str().encode_wide().collect();
    ensure!(
        !from.contains(&0) && !to.contains(&0),
        "component kit path contains NUL"
    );
    let from: Vec<u16> = from.into_iter().chain(Some(0)).collect();
    let to: Vec<u16> = to.into_iter().chain(Some(0)).collect();
    // std::fs::rename can replace an empty directory on Windows. Flags zero
    // explicitly excludes MOVEFILE_REPLACE_EXISTING and preserves a racing target.
    let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("publish new component kit without replacement");
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn publish_new(_: &Path, _: &Path) -> anyhow::Result<()> {
    anyhow::bail!("exclusive component kit directory publication is unsupported on this platform")
}

/// Verify a self-contained offline kit without fetching, extracting, signing,
/// running a dependency, updating a trust ledger or selecting a component.
pub fn verify(root: &Path, keys: &[PublicKey]) -> anyhow::Result<KitLock> {
    let root = fs::canonicalize(root)?;
    let index_dir = tag_directory(&root, INDEX_TAG)?;
    let bytes = read_bounded(&index_dir.join(INDEX_FILE), MAX_DOCUMENT_BYTES)?;
    let signature = read_bounded(&index_dir.join(INDEX_SIGNATURE), MAX_SIGNATURE_BYTES)?;
    let index = signed_index(&bytes, &signature, keys)?;
    let lock: KitLock = serde_json::from_slice(&read_bounded(
        &root.join(KIT_LOCK_FILE),
        MAX_DOCUMENT_BYTES,
    )?)?;
    ensure!(
        lock.schema == KIT_SCHEMA
            && lock.channel == index.channel
            && lock.sequence == index.sequence
            && lock.index_sha256 == digest(&bytes),
        "kit lock does not match its signed index"
    );
    let mut trusted = Vec::new();
    for item in &lock.assets {
        let expected = selected(&index, &item.component, &item.version, &item.platform)?;
        ensure!(
            *item == expected,
            "kit asset does not match signed index identity"
        );
        trusted.push(expected);
    }
    unique_assets(&trusted)?;
    for item in &trusted {
        verify_asset(&root, item, keys)?;
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::minisign::test_key::TestKey;
    use crate::upgrade::stage::fixtures::temp;

    struct Fixture {
        root: PathBuf,
        source: Source,
        key: TestKey,
        bytes: Vec<u8>,
        signature: Vec<u8>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = temp("component-kit");
            let source = Source::Dir {
                root: root.join("source"),
            };
            let key = TestKey::new(71);
            let payload = b"canonical opaque component archive bytes";
            let asset = IndexAsset {
                name: "dsh-r3-linux-x64.tar.gz".into(),
                sha256: digest(payload),
                size: payload.len() as u64,
                signature_name: "dsh-r3-linux-x64.tar.gz.minisig".into(),
            };
            let index = json!({"schema":"cutex/components-index/v1", "channel":"private-r269", "sequence":7,
                "publishedAt":"2026-10-29T00:00:00Z", "components":[{"name":"dsh-runtime", "recommended":"dsh-r3",
                    "releases":[{"version":"dsh-r3", "payload":"bundled-v1", "compatible":{"allCapabilities":[]},
                    "assetTag":"component-dsh-r3", "platforms":{"linux-x64":asset}, "yanked":false,
                    "yankReason":null,"externalRuntime":null,"externalRuntimeSha256":null}]}]});
            // Pretty bytes and the original signature's comments must survive unchanged.
            let bytes = serde_json::to_vec_pretty(&index).unwrap();
            let signature = key
                .sign(&bytes, true, "original index fixture")
                .into_bytes();
            let source_root = root.join("source");
            fs::create_dir_all(source_root.join(INDEX_TAG)).unwrap();
            fs::create_dir_all(source_root.join("component-dsh-r3")).unwrap();
            fs::write(source_root.join(INDEX_TAG).join(INDEX_FILE), &bytes).unwrap();
            fs::write(
                source_root.join(INDEX_TAG).join(INDEX_SIGNATURE),
                &signature,
            )
            .unwrap();
            fs::write(
                source_root.join("component-dsh-r3").join(&asset.name),
                payload,
            )
            .unwrap();
            fs::write(
                source_root
                    .join("component-dsh-r3")
                    .join(&asset.signature_name),
                key.sign(payload, true, "original archive fixture"),
            )
            .unwrap();
            Self {
                root,
                source,
                key,
                bytes,
                signature,
            }
        }
        fn selections(&self) -> Vec<(String, String, String)> {
            vec![("dsh-runtime".into(), "dsh-r3".into(), "linux-x64".into())]
        }
        fn freeze(&self, destination: &Path) -> anyhow::Result<Value> {
            freeze(
                &self.source,
                &[self.key.public()],
                "private-r269",
                destination,
                &self.selections(),
            )
        }
        fn rewrite_lock(&self, root: &Path, change: impl FnOnce(&mut Value)) {
            let path = root.join(KIT_LOCK_FILE);
            let mut value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            change(&mut value);
            fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
    }

    #[test]
    fn preserves_canonical_index_archives_and_signatures_exactly() {
        let f = Fixture::new();
        let dest = f.root.join("kit");
        let result = f.freeze(&dest).unwrap();
        let lock = verify(&dest, &[f.key.public()]).unwrap();
        assert_eq!(result["lock"], serde_json::to_value(&lock).unwrap());
        assert_eq!(
            fs::read(dest.join(INDEX_TAG).join(INDEX_FILE)).unwrap(),
            f.bytes
        );
        assert_eq!(
            fs::read(dest.join(INDEX_TAG).join(INDEX_SIGNATURE)).unwrap(),
            f.signature
        );
        for item in &lock.assets {
            for name in [&item.asset.name, &item.asset.signature_name] {
                assert_eq!(
                    fs::read(dest.join(&item.tag).join(name)).unwrap(),
                    fs::read(f.root.join("source").join(&item.tag).join(name)).unwrap()
                );
            }
        }
    }

    #[test]
    fn rejects_archive_signature_and_index_tampering() {
        for part in ["archive", "signature", "index"] {
            let f = Fixture::new();
            let dest = f.root.join("kit");
            f.freeze(&dest).unwrap();
            let path = match part {
                "archive" => dest.join("component-dsh-r3/dsh-r3-linux-x64.tar.gz"),
                "signature" => dest.join("component-dsh-r3/dsh-r3-linux-x64.tar.gz.minisig"),
                _ => dest.join(INDEX_TAG).join(INDEX_FILE),
            };
            fs::write(path, b"tampered").unwrap();
            assert!(verify(&dest, &[f.key.public()]).is_err(), "{part}");
        }
    }

    #[test]
    fn refuses_unknown_duplicate_empty_wrong_channel_and_untrusted_key() {
        let f = Fixture::new();
        let dest = f.root.join("kit");
        let mut unknown = f.selections();
        unknown[0].1 = "missing-r9".into();
        assert!(freeze(
            &f.source,
            &[f.key.public()],
            "private-r269",
            &dest,
            &unknown
        )
        .is_err());
        let mut duplicates = f.selections();
        duplicates.extend(f.selections());
        assert!(freeze(
            &f.source,
            &[f.key.public()],
            "private-r269",
            &dest,
            &duplicates
        )
        .is_err());
        assert!(freeze(&f.source, &[f.key.public()], "private-r269", &dest, &[]).is_err());
        assert!(freeze(
            &f.source,
            &[f.key.public()],
            "public-stable",
            &dest,
            &f.selections()
        )
        .is_err());
        assert!(freeze(
            &f.source,
            &[TestKey::new(72).public()],
            "private-r269",
            &dest,
            &f.selections()
        )
        .is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn lock_cannot_retarget_paths_or_change_signed_identity() {
        for field in ["tag", "component", "version", "platform", "asset"] {
            let f = Fixture::new();
            let dest = f.root.join("kit");
            f.freeze(&dest).unwrap();
            f.rewrite_lock(&dest, |v| {
                if field == "asset" {
                    v["assets"][0]["asset"]["name"] = "../../outside.tar.gz".into();
                } else {
                    v["assets"][0][field] = "../../outside".into();
                }
            });
            assert!(verify(&dest, &[f.key.public()]).is_err(), "{field}");
        }
        for field in ["channel", "sequence", "indexSha256", "schema"] {
            let f = Fixture::new();
            let dest = f.root.join("kit");
            f.freeze(&dest).unwrap();
            f.rewrite_lock(&dest, |v| {
                v[field] = if field == "sequence" {
                    json!(8)
                } else {
                    json!("wrong")
                }
            });
            assert!(verify(&dest, &[f.key.public()]).is_err(), "{field}");
        }
    }

    #[test]
    fn refuses_overwrite_and_exclusive_publish_preserves_racing_destination() {
        let f = Fixture::new();
        let dest = f.root.join("kit");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("owner-file"), b"keep").unwrap();
        assert!(f.freeze(&dest).is_err());
        assert_eq!(fs::read(dest.join("owner-file")).unwrap(), b"keep");
        let staging = f.root.join("owned-staging");
        fs::create_dir(&staging).unwrap();
        let empty = f.root.join("racing-empty-destination");
        fs::create_dir(&empty).unwrap();
        assert!(publish_new(&staging, &empty).is_err());
        assert!(staging.is_dir());
        assert!(empty.is_dir());
        assert!(f.freeze(Path::new("relative-kit")).is_err());
    }

    #[test]
    fn failed_download_retains_owned_staging_without_publishing_destination() {
        let f = Fixture::new();
        let dest = f.root.join("kit");
        fs::write(
            f.root
                .join("source/component-dsh-r3/dsh-r3-linux-x64.tar.gz"),
            b"bad bytes",
        )
        .unwrap();
        let error = f.freeze(&dest).unwrap_err();
        assert!(format!("{error:#}").contains("staging retained"));
        assert!(!dest.exists());
        let staging: Vec<_> = fs::read_dir(&f.root)
            .unwrap()
            .map(|row| row.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".component-kit-staging-")
            })
            .collect();
        assert_eq!(staging.len(), 1);
        assert_eq!(
            fs::read(staging[0].join(INDEX_TAG).join(INDEX_FILE)).unwrap(),
            f.bytes
        );
    }

    #[test]
    fn correct_archive_hash_does_not_substitute_for_a_trusted_archive_signature() {
        let f = Fixture::new();
        let dest = f.root.join("kit");
        let archive = f
            .root
            .join("source/component-dsh-r3/dsh-r3-linux-x64.tar.gz");
        let original = fs::read(&archive).unwrap();
        fs::write(
            archive.with_file_name("dsh-r3-linux-x64.tar.gz.minisig"),
            TestKey::new(72).sign(&original, true, "untrusted archive signer"),
        )
        .unwrap();
        assert!(f.freeze(&dest).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn withdrawn_release_is_rejected_by_freeze_and_verify() {
        let f = Fixture::new();
        let dest = f.root.join("kit");
        f.freeze(&dest).unwrap();
        let mut index: Value = serde_json::from_slice(&f.bytes).unwrap();
        index["components"][0]["recommended"] = Value::Null;
        index["components"][0]["releases"][0]["yanked"] = true.into();
        let bytes = serde_json::to_vec(&index).unwrap();
        let signature = f.key.sign(&bytes, true, "withdrawn fixture");
        for root in [f.root.join("source"), dest.clone()] {
            fs::write(root.join(INDEX_TAG).join(INDEX_FILE), &bytes).unwrap();
            fs::write(root.join(INDEX_TAG).join(INDEX_SIGNATURE), &signature).unwrap();
        }
        f.rewrite_lock(&dest, |v| v["indexSha256"] = digest(&bytes).into());
        assert!(verify(&dest, &[f.key.public()]).is_err());
        assert!(f.freeze(&f.root.join("withdrawn-kit")).is_err());
    }
}
