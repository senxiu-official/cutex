//! Machine-local known versions for installed cute-codex artifacts.
//!
//! The registry is release metadata, not an authority over launch hashes. A
//! missing or malformed registry must therefore degrade to “unknown version”
//! rather than changing launch or owner-action behavior.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const MAX_REGISTRY_BYTES: usize = 64 * 1024;
pub const REGISTRY_SCHEMA: &str = "cutex/native-version-registry/v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeVersionRegistry {
    pub schema: String,
    pub latest_cli: NativeVersionRecord,
    pub latest_runtime: NativeVersionRecord,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_artifacts: BTreeMap<String, NativeVersionRecord>,
    /// Fast display-only mapping from immutable release artifact paths to the
    /// version recorded for their exact bytes. Unknown paths still fall back to
    /// hashing `/proc/<pid>/exe`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_paths: BTreeMap<String, NativeVersionRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeVersionRecord {
    pub version: String,
    /// RFC 3339 timestamp, recorded with minute precision by release tooling.
    pub recorded_at: String,
}

impl NativeVersionRegistry {
    pub fn read(path: &Path) -> anyhow::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            bytes.len() <= MAX_REGISTRY_BYTES,
            "native version registry is too large"
        );
        let registry: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid native version registry at {}", path.display()))?;
        anyhow::ensure!(
            registry.schema == REGISTRY_SCHEMA,
            "unsupported native version registry schema"
        );
        for record in std::iter::once(&registry.latest_cli)
            .chain(std::iter::once(&registry.latest_runtime))
            .chain(registry.runtime_artifacts.values())
            .chain(registry.runtime_paths.values())
        {
            validate_record(record)?;
        }
        anyhow::ensure!(
            registry
                .runtime_paths
                .keys()
                .all(|path| Path::new(path).is_absolute()),
            "runtime version paths must be absolute"
        );
        Ok(Some(registry))
    }

    pub fn runtime_version(&self, artifact_sha256: &str) -> Option<&str> {
        self.runtime_artifacts
            .get(artifact_sha256)
            .map(|record| record.version.as_str())
    }

    pub fn runtime_version_for_launch_contract(
        &self,
        contract: &crate::agent_management::ExplicitLaunchContract,
    ) -> Option<String> {
        let bundle: crate::launch::stock::StockBundle =
            serde_json::from_slice(&std::fs::read(&contract.bundle_manifest).ok()?).ok()?;
        self.runtime_version(bundle.executable.sha256.as_str())
            .map(str::to_string)
    }

    /// Resolve the version of a live backend by its exact executable bytes.
    ///
    /// A durable contract can be advanced while its old process keeps running,
    /// so process bytes take precedence whenever a PID is available. Unknown
    /// hashes safely degrade to `None`.
    pub fn runtime_version_for_executable(&self, executable: &Path) -> Option<String> {
        let digest = crate::agent_management::file_sha256(executable).ok()?;
        self.runtime_version(digest.as_str()).map(str::to_string)
    }

    #[cfg(target_os = "linux")]
    pub fn runtime_version_for_pid(&self, pid: u32) -> Option<String> {
        let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        if let Some(record) = executable
            .to_str()
            .and_then(|path| self.runtime_paths.get(path))
        {
            return Some(record.version.clone());
        }
        self.runtime_version_for_executable(&executable)
    }

    #[cfg(windows)]
    pub fn runtime_version_for_pid(&self, pid: u32) -> Option<String> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut image = [0u16; 32768];
        let mut len = image.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, image.as_mut_ptr(), &mut len) };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        let executable = PathBuf::from(String::from_utf16_lossy(&image[..len as usize]));
        self.runtime_version_for_executable(&executable)
    }
}

pub fn registry_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("native-version-registry-v1.json"))
}

pub fn load(path: &Path) -> anyhow::Result<Option<NativeVersionRegistry>> {
    NativeVersionRegistry::read(path)
}

pub fn load_relaxed() -> Option<NativeVersionRegistry> {
    let path = registry_path().ok()?;
    load(&path).ok().flatten()
}

fn validate_record(record: &NativeVersionRecord) -> anyhow::Result<()> {
    anyhow::ensure!(
        !record.version.trim().is_empty() && record.version.len() <= 64,
        "invalid native version"
    );
    anyhow::ensure!(
        chrono::DateTime::parse_from_rfc3339(&record.recorded_at).is_ok()
            || chrono::DateTime::parse_from_str(&record.recorded_at, "%Y-%m-%dT%H:%M%z",).is_ok(),
        "invalid native version timestamp"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_maps_runtime_hash_to_version() {
        let registry: NativeVersionRegistry = serde_json::from_str(
            r#"{
              "schema":"cutex/native-version-registry/v1",
              "latest_cli":{"version":"0.156.1-r36","recorded_at":"2026-09-23T22:15:00Z"},
              "latest_runtime":{"version":"0.156.1-r35","recorded_at":"2026-09-23T21:58:00Z"},
              "runtime_artifacts":{
                "c5a78524eca1eadc3309935a4ee7673b5755185722fdb2451eec3079951bb9f1":{
                  "version":"0.156.1-r35","recorded_at":"2026-09-23T21:58:00Z"
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(
            registry.runtime_version(
                "c5a78524eca1eadc3309935a4ee7673b5755185722fdb2451eec3079951bb9f1"
            ),
            Some("0.156.1-r35")
        );
    }

    #[test]
    fn registry_accepts_documented_minute_precision_timestamps() {
        let root =
            std::env::temp_dir().join(format!("cutex-version-minute-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("registry.json");
        std::fs::write(
            &path,
            r#"{
              "schema":"cutex/native-version-registry/v1",
              "latest_cli":{"version":"0.157.1-r46","recorded_at":"2026-09-28T11:27+10:00"},
              "latest_runtime":{"version":"0.157.1-r46","recorded_at":"2026-09-28T11:27+10:00"}
            }"#,
        )
        .unwrap();
        let registry = load(&path).unwrap().unwrap();
        assert_eq!(registry.latest_runtime.version, "0.157.1-r46");
        std::fs::write(
            &path,
            r#"{"schema":"cutex/native-version-registry/v1","latest_cli":{"version":"x","recorded_at":"2026-09-28 11:27+10:00"},"latest_runtime":{"version":"x","recorded_at":"2026-09-28T11:27+10:00"}}"#,
        )
        .unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launch_contract_version_uses_backend_artifact_hash() {
        let root =
            std::env::temp_dir().join(format!("cutex-version-contract-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        const BACKEND_SHA: &str =
            "c5a78524eca1eadc3309935a4ee7673b5755185722fdb2451eec3079951bb9f1";
        let manifest = root.join("bundle.json");
        std::fs::write(
            &manifest,
            serde_json::json!({
                "version": 4,
                "upstream_commit": "test-upstream",
                "executable": {"path": root.join("backend"), "sha256": BACKEND_SHA},
                "code_mode_host": {"path": root.join("host"), "sha256": "1".repeat(64)},
                "facade": {"path": root.join("facade"), "sha256": "2".repeat(64)},
                "schema": {"path": root.join("schema"), "sha256": "3".repeat(64)},
                "shared_config": {"path": root.join("config.toml"), "sha256": "4".repeat(64)}
            })
            .to_string(),
        )
        .unwrap();
        let registry: NativeVersionRegistry = serde_json::from_str(
            r#"{
              "schema":"cutex/native-version-registry/v1",
              "latest_cli":{"version":"0.156.1-r36","recorded_at":"2026-09-24T00:01:00Z"},
              "latest_runtime":{"version":"0.156.1-r36","recorded_at":"2026-09-24T00:01:00Z"},
              "runtime_artifacts":{
                "c5a78524eca1eadc3309935a4ee7673b5755185722fdb2451eec3079951bb9f1":{
                  "version":"0.156.1-r35","recorded_at":"2026-09-23T23:58:00Z"
                }
              }
            }"#,
        )
        .unwrap();
        let contract = crate::agent_management::ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id: "native".into(),
            native_home: root.clone(),
            bundle_manifest: manifest,
            bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        };
        assert_eq!(
            registry.runtime_version_for_launch_contract(&contract),
            Some("0.156.1-r35".into())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absent_registry_is_none_and_schema_is_strict() {
        let root =
            std::env::temp_dir().join(format!("cutex-version-registry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(load(&root.join("missing.json")).unwrap().is_none());
        let path = root.join("registry.json");
        std::fs::write(
            &path,
            r#"{"schema":"cutex/native-version-registry/v2","latest_cli":{"version":"x","recorded_at":"2026-09-23T22:15:00Z"},"latest_runtime":{"version":"x","recorded_at":"2026-09-23T22:15:00Z"}}"#,
        )
        .unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_version_uses_exact_executable_bytes() {
        let root =
            std::env::temp_dir().join(format!("cutex-version-executable-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("backend");
        std::fs::write(&executable, b"exact-backend-bytes").unwrap();
        let digest = crate::agent_management::file_sha256(&executable).unwrap();
        let mut registry = NativeVersionRegistry {
            schema: REGISTRY_SCHEMA.into(),
            latest_cli: NativeVersionRecord {
                version: "0.156.1-r36".into(),
                recorded_at: "2026-09-24T00:01:00Z".into(),
            },
            latest_runtime: NativeVersionRecord {
                version: "0.156.1-r36".into(),
                recorded_at: "2026-09-24T00:01:00Z".into(),
            },
            runtime_artifacts: BTreeMap::from([(
                digest.as_str().to_owned(),
                NativeVersionRecord {
                    version: "0.156.1-r35".into(),
                    recorded_at: "2026-09-23T23:58:00Z".into(),
                },
            )]),
            runtime_paths: BTreeMap::new(),
        };
        assert_eq!(
            registry.runtime_version_for_executable(&executable),
            Some("0.156.1-r35".into())
        );
        let test_executable = std::fs::read_link("/proc/self/exe").unwrap();
        registry.runtime_paths.insert(
            test_executable.to_string_lossy().into_owned(),
            NativeVersionRecord {
                version: "0.156.1-r36".into(),
                recorded_at: "2026-09-24T00:01:00Z".into(),
            },
        );
        assert_eq!(
            registry.runtime_version_for_pid(std::process::id()),
            Some("0.156.1-r36".into())
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
