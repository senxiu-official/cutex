//! Release file records: which `cutex` / `cutex-mcp` bytes a release
//! directory held when it was placed into the layout, checked before any
//! release executable is started (preflight probes, the apply / rollback
//! runner, the release a rollback switches to).
//!
//! Why a record and not the manifest directly: a signed release manifest
//! lists only the platform ARCHIVE (`cutex.artifacts.<platform>.sha256`),
//! never the unpacked files, and staging deletes the archive after
//! unpacking. So the stager, right after it unpacked an archive whose
//! sha256 matched the signed manifest, records the sha256 of every unpacked
//! executable together with the manifest digest and the archive digest in
//! `<base>/upgrade/release-files/<release>.json` (private file; only
//! [`record`] writes it: `cutex upgrade stage` and `cutex service install`).
//! Verification then chains the executable to the signature:
//!
//! 1. executable sha256 == record (detects a binary replaced or damaged after
//!    staging);
//! 2. record manifest sha256 == the release dir's `manifest.json` (the record
//!    belongs to these files), and for a staged release the manifest's
//!    minisign signature verifies with a trusted key and its archive sha256
//!    equals the record's (the bytes came out of the signed archive).
//!
//! A release without a record (staged by an older binary, or hand-copied) is
//! refused: `cutex upgrade stage <release>` recreates the record by
//! re-downloading the signed archive and comparing; `cutex service install`
//! records the release it installs. What this cannot catch: someone who can
//! write the whole install base (the owner's own account) can also rewrite
//! the record; that is the install's trust boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::layout::{Layout, CUTEX_EXE, FACADE_EXE};
use super::manifest::{self, ReleaseManifest};
use super::minisign::PublicKey;
use super::stage::{upgrade_error, MANIFEST_FILE, SIGNATURE_SUFFIX};

pub const RECORD_SCHEMA: &str = "cutex/release-files/v1";
pub const SOURCE_STAGE: &str = "stage";
pub const SOURCE_SERVICE_INSTALL: &str = "service-install";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseFiles {
    pub schema: String,
    pub release: String,
    /// `stage` (unpacked from a signed archive) or `service-install`.
    pub source: String,
    pub recorded_at: String,
    /// sha256 of `releases/<r>/manifest.json` when it has one.
    #[serde(default)]
    pub manifest_sha256: Option<String>,
    /// The signed archive's sha256 the files were unpacked from (stage).
    #[serde(default)]
    pub artifact_sha256: Option<String>,
    /// File name -> sha256 (`cutex[.exe]`, `cutex-mcp[.exe]`).
    pub files: BTreeMap<String, String>,
}

pub fn record_path(layout: &Layout, release: &str) -> PathBuf {
    layout.upgrade_dir().join("release-files").join(format!("{release}.json"))
}

/// Hash `cutex` and `cutex-mcp` of `releases/<release>/` as they are now.
pub fn file_hashes(layout: &Layout, release: &str) -> anyhow::Result<BTreeMap<String, String>> {
    file_hashes_in(&layout.release_dir(release))
}

/// Hash `cutex` and `cutex-mcp` in `dir` (a release dir or an unpacked archive).
pub fn file_hashes_in(dir: &std::path::Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for name in [CUTEX_EXE, FACADE_EXE] {
        files.insert(name.to_string(), manifest::file_sha256_hex(&dir.join(name))?);
    }
    Ok(files)
}

/// Write the record for a release just placed into the layout. Callers:
/// the stager (after unpacking a verified archive) and `service install`.
pub fn record(
    layout: &Layout,
    release: &str,
    source: &str,
    files: BTreeMap<String, String>,
    manifest_bytes: Option<&[u8]>,
    artifact_sha256: Option<&str>,
) -> anyhow::Result<()> {
    super::layout::ensure_label(release, "release")?;
    let record = ReleaseFiles {
        schema: RECORD_SCHEMA.into(),
        release: release.into(),
        source: source.into(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        manifest_sha256: manifest_bytes.map(manifest::sha256_hex),
        artifact_sha256: artifact_sha256.map(str::to_string),
        files,
    };
    crate::config::atomic::write_private_pretty_json_atomic(&record_path(layout, release), &record, "release file record")
}

pub fn read(layout: &Layout, release: &str) -> Option<ReleaseFiles> {
    if !super::layout::valid_label(release) {
        return None;
    }
    let record: ReleaseFiles = serde_json::from_slice(&std::fs::read(record_path(layout, release)).ok()?).ok()?;
    (record.schema == RECORD_SCHEMA && record.release == release).then_some(record)
}

fn unverified(release: &str, why: impl std::fmt::Display) -> anyhow::Error {
    upgrade_error(409, "release_unverified", format!("release {release}: {why}; not started"))
}

/// Refuse to start `releases/<release>/cutex` unless it is the file the
/// stager (or `service install`) recorded and, for a staged release, the
/// record is bound to the signed manifest (module docs).
pub fn verify_executable(layout: &Layout, keys: &[PublicKey], release: &str) -> anyhow::Result<()> {
    let Some(record) = read(layout, release) else {
        return Err(unverified(release, "no release file record (stage it again with `cutex upgrade stage`)"));
    };
    let exe = layout.release_executable(release);
    let actual = manifest::file_sha256_hex(&exe).map_err(|e| unverified(release, format!("{e:#}")))?;
    if record.files.get(CUTEX_EXE) != Some(&actual) {
        return Err(unverified(release, format!("{} does not match the hash recorded when it was staged", exe.display())));
    }
    let dir = layout.release_dir(release);
    let manifest_bytes = match std::fs::read(dir.join(MANIFEST_FILE)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(unverified(release, format!("read manifest: {error}"))),
    };
    match (&manifest_bytes, &record.manifest_sha256) {
        (None, None) => {}
        (Some(bytes), Some(expected)) if manifest::sha256_hex(bytes) == *expected => {}
        _ => return Err(unverified(release, "manifest.json does not match the release file record")),
    }
    if record.source == SOURCE_STAGE {
        let Some(bytes) = &manifest_bytes else {
            return Err(unverified(release, "a staged release without manifest.json"));
        };
        let signature = std::fs::read_to_string(dir.join(format!("{MANIFEST_FILE}{SIGNATURE_SUFFIX}")))
            .map_err(|e| unverified(release, format!("read manifest signature: {e}")))?;
        super::minisign::verify_text(keys, bytes, &signature).map_err(|e| unverified(release, format!("manifest signature: {e:#}")))?;
        let parsed = ReleaseManifest::parse(bytes).map_err(|e| unverified(release, format!("{e:#}")))?;
        let artifact = parsed.cutex_artifact().map_err(|e| unverified(release, format!("{e:#}")))?;
        if record.artifact_sha256.as_deref() != Some(artifact.sha256.as_str()) {
            return Err(unverified(release, "the recorded archive is not the one the signed manifest names"));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::upgrade::stage::fixtures::installed_layout;

    fn code(error: anyhow::Error) -> &'static str {
        error.downcast_ref::<super::super::stage::UpgradeError>().unwrap().code
    }

    #[test]
    fn a_replaced_or_unrecorded_executable_is_refused() {
        let layout = installed_layout("r239", 40);
        verify_executable(&layout, &[], "r239").unwrap();
        // Replaced after it was recorded.
        std::fs::write(layout.release_executable("r239"), b"#!/bin/sh\necho evil\n").unwrap();
        assert_eq!(code(verify_executable(&layout, &[], "r239").unwrap_err()), "release_unverified");
        // A release copied in by hand has no record.
        std::fs::create_dir_all(layout.release_dir("r238")).unwrap();
        std::fs::copy(layout.release_dir("r239").join(FACADE_EXE), layout.release_executable("r238")).unwrap();
        assert_eq!(code(verify_executable(&layout, &[], "r238").unwrap_err()), "release_unverified");
        // A manifest swapped under a recorded release.
        let fresh = installed_layout("r240", 41);
        std::fs::write(fresh.release_dir("r240").join(MANIFEST_FILE), b"{}").unwrap();
        assert_eq!(code(verify_executable(&fresh, &[], "r240").unwrap_err()), "release_unverified");
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(&fresh.base).unwrap();
    }
}
