//! Manifest-selected Linux runtime artifacts with checked file integrity and protocol compatibility.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::agent_management::{ExplicitLaunchContract, file_sha256};
use crate::role_revision::Sha256;
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore};

pub const STOCK_COMMIT: &str = "3d2ee51ca2d5db578f328aa75e20aa22c0197c9a";
pub const STOCK_EXECUTABLE_SHA256: &str =
    "56ef98ab4032d317ab26e9b5e5a175650717351edb16ed9cde0cb6d1734d62da";
pub const STOCK_HOST_SHA256: &str =
    "3e85d67471825f73d02ff5f7e047ca1f6ca8caa3f59e4c6e8d9ca6ca7302cb45";
pub const STOCK_SCHEMA_SHA256: &str =
    "b06f77062369d481a59cc70720c12b89cb9dd49c385863923262102d3ad6c978";
pub const S6_COMMIT: &str = "c2aaceb411b7851806c62435b97895a63a7d34cd";
pub const S6_EXECUTABLE_SHA256: &str =
    "b70d48151c9deb76a9c0ab14a820c582f2bc12a73bbb1512fee9b2f1bec9fa60";
pub const S6_SCHEMA_SHA256: &str =
    "00e035e34ac1034ee34473f8f68b7704d6058c5b180ff4f4b6cad9fadab3a86d";
// Exact accepted durable-presentation CLI/server family. Older
// bundle receipts remain historical facts, not permission to launch old bytes.
pub const S6E_COMMIT: &str = "8cde795620e8b2fa6ba3bfa1fd15a5732a1e12f6";
// Coherent accepted auth/legacy-reader family, not a descendant allowlist.
pub const S6E_SERVER_COMMIT: &str = "2eab060b191a0fe59e22b28785d02d76eafb7fc4";
pub const S6E_EXECUTABLE_SHA256: &str =
    "15c72a6bd476a60af9ba99cfbc0672a8376e8bb447cda2f1745e72aef2070c49";
pub const S6E_CLI_SHA256: &str = "f66dd9126fb73b8f6bc04cd275f619630c96c4255e6281dcbeb1fe41f8b691cf";
pub const S6E_SCHEMA_SHA256: &str =
    "3fc006075408b0a001f9cb3f706625c0161e083ab0691d6be6116622b434f1d3";

// Generated from the reviewed Cutex protocol on Codex 0.156.1; exact bytes only.
pub const NATIVE_156_SCHEMA_SHA256: &str =
    "297a6d7f53d92a59c65758e985ee8d84bbec6a0fdf9ccbf3f0c0312e7bef9b7d";
// Generated from the reviewed Cutex protocol on Codex 0.157.1; exact bytes only.
pub const NATIVE_157_SCHEMA_SHA256: &str =
    "11e43638050b443665f981806f6752b0e7519825d62886668d091599f49a74fe";
// Reviewed Cutex protocol on Codex 0.157.1-r44; exact bytes only.
pub const NATIVE_157_R44_SCHEMA_SHA256: &str =
    "4bbfc46228f9fb344125bf4deab474b98f564dbf9fb5f30effb9eb8c16ef5ebd";
// Reviewed Cutex protocol on Codex 0.160.0-r47; exact bytes only.
pub const NATIVE_160_R47_SCHEMA_SHA256: &str =
    "36f226cbe2f320b35951f665d7e8a50aabe77a9e8b5427d16b54c8212e868cd1";

pub fn is_common_ingress_schema(hash: &str) -> bool {
    matches!(
        hash,
        S6_SCHEMA_SHA256
            | S6E_SCHEMA_SHA256
            | NATIVE_156_SCHEMA_SHA256
            | NATIVE_157_SCHEMA_SHA256
            | NATIVE_157_R44_SCHEMA_SHA256
            | NATIVE_160_R47_SCHEMA_SHA256
    )
}

pub fn is_private_native_schema(hash: &str) -> bool {
    matches!(
        hash,
        STOCK_SCHEMA_SHA256
            | S6_SCHEMA_SHA256
            | S6E_SCHEMA_SHA256
            | NATIVE_156_SCHEMA_SHA256
            | NATIVE_157_SCHEMA_SHA256
            | NATIVE_157_R44_SCHEMA_SHA256
            | NATIVE_160_R47_SCHEMA_SHA256
    )
}

/// Oldest native app-server release whose protocol Cutex speaks (ExternalInput
/// v1, auth file, project/list). Newer releases are accepted; the runtime's
/// initialize response, not its schema bytes, says what it supports.
pub const MIN_NATIVE_VERSION: (u64, u64) = (0, 153);

/// Version from an initialize `userAgent` (`product/0.160.0-r47 (...)`).
pub fn native_version_from_user_agent(user_agent: &str) -> Option<(u64, u64, u64)> {
    let version = user_agent
        .split_whitespace()
        .next()
        .and_then(|product| product.rsplit_once('/'))
        .map(|(_, version)| version)?;
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next().flatten().unwrap_or(0)))
}

/// The native-version floor is about the Codex app-server; the Claude adapter
/// (`cute-claude-app-server/0.3.0`) speaks the same protocol subset under its
/// own version scheme and must not be measured against it.
pub fn user_agent_is_claude_adapter(user_agent: &str) -> bool {
    user_agent
        .split_whitespace()
        .next()
        .and_then(|product| product.rsplit_once('/'))
        .is_some_and(|(name, _)| name.to_ascii_lowercase().contains("claude"))
}

pub fn native_version_supported(version: (u64, u64, u64)) -> bool {
    (version.0, version.1) >= MIN_NATIVE_VERSION
}

/// Stock launches record their protocol family in the runtime binding's
/// `schema_version`; the schema file's hash is not an allowlist.
pub fn stock_schema_version_has_external_input(version: &str) -> bool {
    version.ends_with("-external-input-v1") || version.ends_with("-soon-v1")
}

/// A runtime launched from a stock bundle (any ingress family) rather than a
/// legacy Cutex-patched app-server.
pub fn is_stock_schema_version(version: &str) -> bool {
    version.starts_with("U-") || version.starts_with("stock-")
}

/// Whether a runtime with this schema identity takes ExternalInput.
pub fn schema_has_external_input(version: &str, hash: &str) -> bool {
    stock_schema_version_has_external_input(version) || is_common_ingress_schema(hash)
}

/// Capability negotiation at initialize. An ExternalInput runtime must
/// advertise ExternalInput v1 (all Cutex needs to deliver); structured-view v2,
/// `soon` delivery and presentation are optional and checked where used. A
/// reported version older than [`MIN_NATIVE_VERSION`] is refused.
pub fn validate_ingress_capability(
    expects_external_input: bool,
    init: &serde_json::Value,
) -> anyhow::Result<()> {
    if let Some(version) = init
        .get("userAgent")
        .and_then(serde_json::Value::as_str)
        .filter(|agent| !user_agent_is_claude_adapter(agent))
        .and_then(native_version_from_user_agent)
    {
        ensure!(
            native_version_supported(version),
            "native app-server {}.{}.{} is older than the supported {}.{}",
            version.0,
            version.1,
            version.2,
            MIN_NATIVE_VERSION.0,
            MIN_NATIVE_VERSION.1
        );
    }
    if expects_external_input {
        ensure!(
            init.get("externalInputVersion")
                .and_then(serde_json::Value::as_u64)
                == Some(1),
            "native app-server does not advertise ExternalInput v1"
        );
    }
    Ok(())
}
// Human receiver policy, never an ingress message/tool field. Null is invalid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CanonicalBytePolicy {
    Limit(std::num::NonZeroU32),
    Off(OffPolicy),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OffPolicy {
    #[serde(rename = "off")]
    Off,
}
impl Default for CanonicalBytePolicy {
    fn default() -> Self {
        Self::Limit(std::num::NonZeroU32::new(10000).unwrap())
    }
}
impl CanonicalBytePolicy {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalInputBinding {
    pub version: u32,
    pub owner_id: String,
    pub thread_id: String,
    pub runtime_generation: u64,
    #[serde(default, skip_serializing_if = "CanonicalBytePolicy::is_default")]
    pub canonical_byte_limit: CanonicalBytePolicy,
}
impl ExternalInputBinding {
    #[cfg(not(any(unix, windows)))]
    pub fn stage(&self, _directory: &Path) -> anyhow::Result<PathBuf> {
        anyhow::bail!("ExternalInput requires private Unix transport")
    }
    #[cfg(not(any(unix, windows)))]
    pub fn verify(&self, _directory: &Path) -> anyhow::Result<()> {
        anyhow::bail!("ExternalInput requires private Unix transport")
    }
    pub fn from_review(receipt: &crate::agent_management::StockRuntimeReceipt) -> Self {
        Self {
            version: 1,
            owner_id: receipt.review.subject.cutex_session_id.as_str().to_string(),
            thread_id: receipt.review.contract.native_id.clone(),
            runtime_generation: receipt.expected_generation,
            canonical_byte_limit: receipt.review.receiver_canonical_byte_limit.clone(),
        }
    }
    #[cfg(unix)]
    fn private_directory(path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::MetadataExt;
        canonical(path)?;
        let m = std::fs::symlink_metadata(path)?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "ingress binding requires owned private runtime directory"
        );
        Ok(())
    }
    #[cfg(unix)]
    pub fn stage(&self, directory: &Path) -> anyhow::Result<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        Self::private_directory(directory)?;
        let path = directory.join("external-input.json");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.flush()?;
        Ok(path)
    }
    #[cfg(unix)]
    pub fn verify(&self, directory: &Path) -> anyhow::Result<()> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        Self::private_directory(directory)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(directory.join("external-input.json"))?;
        let m = file.metadata()?;
        ensure!(
            m.is_file()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o7777 == 0o600
                && m.len() <= 4096,
            "invalid ingress binding file"
        );
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        ensure!(
            serde_json::from_slice::<Self>(&bytes)? == *self,
            "ingress binding changed"
        );
        Ok(())
    }
    #[cfg(windows)]
    pub fn stage(&self, directory: &Path) -> anyhow::Result<PathBuf> {
        use crate::platform::private_fs;
        use std::io::Write;
        let (_guard, identity) = private_fs::open_validated_directory(directory)?;
        let mut file = private_fs::open_child(
            directory,
            identity,
            "external-input.json",
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            false,
        )?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.flush()?;
        Ok(directory.join("external-input.json"))
    }
    #[cfg(windows)]
    pub fn verify(&self, directory: &Path) -> anyhow::Result<()> {
        use crate::platform::private_fs;
        use std::io::Read;
        use std::os::windows::fs::OpenOptionsExt;
        let (_guard, identity) = private_fs::open_validated_directory(directory)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x00200000)
            .open(directory.join("external-input.json"))?;
        private_fs::validate_private_file(&file)?;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 4096 && serde_json::from_slice::<Self>(&bytes)? == *self,
            "ingress binding changed"
        );
        private_fs::validate_binding(directory, identity)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedFile {
    pub path: PathBuf,
    pub sha256: Sha256,
}
impl VerifiedFile {
    pub fn validate(&self) -> anyhow::Result<()> {
        canonical(&self.path)?;
        ensure!(self.path.is_file(), "stock reference is not a file");
        ensure!(
            file_sha256(&self.path)? == self.sha256,
            "stock reference changed"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockBundle {
    pub version: u32,
    pub upstream_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_patch_commit: Option<String>,
    pub executable: VerifiedFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<VerifiedFile>,
    pub code_mode_host: VerifiedFile,
    pub facade: VerifiedFile,
    /// Pinned stock experimental schema, or accepted S6 generated schema.
    pub schema: VerifiedFile,
    /// Mutable shared config; its current content is validated on each load.
    pub shared_config: VerifiedFile,
    /// Historical migration digest, retained for receipts, not a launch restriction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_config_sha256: Option<Sha256>,
}

impl StockBundle {
    pub fn common_ingress(&self) -> bool {
        (self.version == 2
            && self.native_patch_commit.as_deref() == Some(S6_COMMIT)
            && self.executable.sha256.as_str() == S6_EXECUTABLE_SHA256
            && self.schema.sha256.as_str() == S6_SCHEMA_SHA256
            && self.cli.is_none())
            || self.soon_ingress()
    }
    pub fn soon_ingress(&self) -> bool {
        self.local_deployment()
            || (self.version == 3
                && self.native_patch_commit.as_deref() == Some(S6E_COMMIT)
                && self.executable.sha256.as_str() == S6E_EXECUTABLE_SHA256
                && self.schema.sha256.as_str() == S6E_SCHEMA_SHA256
                && self
                    .cli
                    .as_ref()
                    .is_some_and(|c| c.sha256.as_str() == S6E_CLI_SHA256))
    }

    /// Version 4 selects locally installed build bytes, rather than a compiled
    /// allowlist of source commits or schema hashes. What the runtime supports
    /// is negotiated from its initialize response (validate_ingress_capability);
    /// every selected file is still hashed by validate_components. The TUI
    /// frontend (`cli`) is optional: runtimes only need the app-server.
    fn local_deployment(&self) -> bool {
        self.version == 4
    }
    fn validate_identity(&self) -> anyhow::Result<()> {
        if self.version == 4 {
            return Ok(());
        }
        ensure!(
            self.upstream_commit == STOCK_COMMIT,
            "unsupported native upstream source"
        );
        let stock = self.version == 1
            && self.cli.is_none()
            && self.native_patch_commit.is_none()
            && self.executable.sha256.as_str() == STOCK_EXECUTABLE_SHA256
            && self.schema.sha256.as_str() == STOCK_SCHEMA_SHA256;
        ensure!(
            stock || self.common_ingress(),
            "unpinned native bundle/source/executable/schema"
        );
        ensure!(
            self.code_mode_host.sha256.as_str() == STOCK_HOST_SHA256,
            "unpinned native companion"
        );
        Ok(())
    }
    pub fn load(contract: &ExplicitLaunchContract) -> anyhow::Result<Self> {
        Self::load_contract(contract)
    }

    /// An existing process keeps its startup configuration. Reading messages,
    /// history, or stopping it must not depend on later edits to config.toml.
    pub fn load_running(contract: &ExplicitLaunchContract) -> anyhow::Result<Self> {
        contract.validate()?;
        canonical(&contract.native_home)?;
        canonical(&contract.bundle_manifest)?;
        ensure!(contract.native_home.is_dir(), "native home missing");
        ensure!(
            file_sha256(&contract.bundle_manifest)? == contract.bundle_sha256,
            "bundle evidence missing or changed"
        );
        let bundle: Self = serde_json::from_slice(&std::fs::read(&contract.bundle_manifest)?)
            .context("invalid stock bundle manifest")?;
        bundle.validate_components_with_config(false)?;
        ensure!(
            contract.version
                == if contract.version == 4 && bundle.version == 4 {
                    4
                } else if bundle.soon_ingress() {
                    if contract.migration_action_id.is_some() {
                        3
                    } else {
                        2
                    }
                } else {
                    1
                },
            "runtime contract version does not match its bundle"
        );
        Ok(bundle)
    }

    fn load_contract(contract: &ExplicitLaunchContract) -> anyhow::Result<Self> {
        contract.validate()?;
        let bundle = Self::load_verified_references(
            &contract.native_home,
            &contract.bundle_manifest,
            &contract.bundle_sha256,
        )?;
        ensure!(
            contract.version
                == if contract.version == 4 && bundle.version == 4 {
                    4
                } else if bundle.soon_ingress() {
                    if contract.migration_action_id.is_some() {
                        3
                    } else {
                        2
                    }
                } else {
                    1
                },
            "new coherent Soon bundle requires explicit version-2 activation; old markers cannot opt in"
        );
        Ok(bundle)
    }

    /// Validate execution evidence before a native identity exists. This does
    /// not authorize launch or manufacture an ExplicitLaunchContract.
    pub fn load_references(
        native_home: &Path,
        manifest: &Path,
        digest: &Sha256,
    ) -> anyhow::Result<Self> {
        Self::load_verified_references(native_home, manifest, digest)
    }

    fn load_verified_references(
        native_home: &Path,
        manifest: &Path,
        digest: &Sha256,
    ) -> anyhow::Result<Self> {
        canonical(native_home)?;
        canonical(manifest)?;
        ensure!(native_home.is_dir(), "native home missing");
        ensure!(
            &file_sha256(manifest)? == digest,
            "bundle evidence missing or changed"
        );
        let mut bundle: Self = serde_json::from_slice(&std::fs::read(manifest)?)
            .context("invalid stock bundle manifest")?;
        let raw = std::fs::read_to_string(&bundle.shared_config.path)?;
        bundle.validate_shared_configuration(&raw)?;
        // Config is user-editable state, not an immutable build artifact.
        // Old manifest hashes remain historical evidence, never launch gates.
        bundle.shared_config.sha256 = file_sha256(&bundle.shared_config.path)?;
        bundle.validate_components()?;
        ensure!(
            bundle.shared_config.path.canonicalize()?
                == native_home.join("config.toml").canonicalize()?,
            "wrong shared config/home"
        );
        bundle
            .validate_shared_configuration(&std::fs::read_to_string(&bundle.shared_config.path)?)?;
        Ok(bundle)
    }

    fn validate_shared_configuration(&self, raw: &str) -> anyhow::Result<()> {
        validate_shared_config(raw)
    }

    /// Identical artifact fences, before a migration's shared config exists.
    pub(crate) fn validate_components(&self) -> anyhow::Result<()> {
        self.validate_components_with_config(true)
    }

    /// Validate the frontend actually selected for a new attach/launch. An old
    /// frontend is not a dependency of a running app-server's identity or stop.
    pub fn validate_frontend(&self) -> anyhow::Result<()> {
        if let Some(cli) = &self.cli {
            cli.validate()
                .context("selected frontend executable unavailable")?;
            validate_frontend_neighbors(cli, &self.executable, &self.code_mode_host)?;
        }
        Ok(())
    }

    fn validate_components_with_config(&self, validate_config: bool) -> anyhow::Result<()> {
        let bundle = self;
        ensure!(
            cfg!(any(target_os = "linux", windows)),
            "stock runtime unsupported on this platform"
        );
        #[cfg(windows)]
        ensure!(
            bundle.local_deployment(),
            "Windows requires a local v4 runtime deployment"
        );
        bundle.validate_identity()?;
        if validate_config {
            bundle.validate_frontend()?;
        }
        for file in [
            &bundle.executable,
            &bundle.code_mode_host,
            &bundle.facade,
            &bundle.schema,
        ] {
            file.validate()?;
        }
        if validate_config {
            bundle.shared_config.validate()?;
        }
        ensure!(
            bundle.code_mode_host.path
                == bundle
                    .executable
                    .path
                    .parent()
                    .context("stock binary parent missing")?
                    .join(if cfg!(windows) {
                        "codex-code-mode-host.exe"
                    } else {
                        "codex-code-mode-host"
                    }),
            "stock companion must be beside executable"
        );
        let schema: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&bundle.schema.path)?)?;
        ensure!(schema.is_object(), "invalid stock protocol schema");
        Ok(())
    }
}

/// Hash launch-affecting values, preserving all non-presentation configuration.
pub fn launch_config_digest(raw: &str) -> anyhow::Result<Sha256> {
    use sha2::Digest;
    validate_shared_config(raw)?;
    let mut value: toml::Value = toml::from_str(raw)?;
    value
        .as_table_mut()
        .context("config must be a table")?
        .remove("tui");
    Sha256::new(format!(
        "{:x}",
        sha2::Sha256::digest(serde_json::to_vec(&value)?)
    ))
    .map_err(anyhow::Error::msg)
}

/// Cutex's legacy in-config projection marker. Native 0.156.1 reports this
/// root field as an unrecognized user setting, so current projections keep the
/// marker out of config.toml entirely.
pub const SHARED_PROJECTION_MARKER: &str = "cutex_projection_version";

/// Remove the legacy `cutex_projection_version` marker from a shared native
/// config. Returns `None` when the marker is already absent, preserving the
/// caller's original bytes (including formatting) in that case. Unknown
/// marker versions fail closed instead of being silently discarded.
pub fn remove_projection_marker(raw: &str) -> anyhow::Result<Option<String>> {
    let mut value: toml::Value = toml::from_str(raw)?;
    let table = value
        .as_table_mut()
        .context("shared config table required")?;
    let Some(marker) = table.remove(SHARED_PROJECTION_MARKER) else {
        return Ok(None);
    };
    ensure!(
        marker.as_integer() == Some(2),
        "unknown shared projection version"
    );
    let projected = toml::to_string(&value)?;
    validate_shared_config(&projected)?;
    Ok(Some(projected))
}

/// The model catalog belongs to the launch profile, not to an agent's home.
///
/// An agent home's `config.toml` is a copy of the shared home's, and the
/// shared home carries whatever profile was last activated into it (e.g. a
/// Kimi profile's `model_provider` / `model_catalog_json`). Launches override
/// the provider with `-c`, but a profile without a catalog (ChatGPT login,
/// API keys without one) emits nothing for `model_catalog_json`, and TOML
/// overrides cannot unset a key — so native kept serving the other profile's
/// catalog from `model/list` (agents on a ChatGPT profile listing Kimi models).
///
/// When `launch_args` set no `model_catalog_json`, remove a root-level
/// `model_catalog_json` from `<home>/config.toml`. Other bytes are kept as
/// they are. Returns whether the file changed.
pub fn drop_inherited_model_catalog(home: &Path, launch_args: &[String]) -> anyhow::Result<bool> {
    if launch_args
        .iter()
        .any(|arg| arg.trim_start().starts_with("model_catalog_json="))
    {
        return Ok(false);
    }
    let path = home.join("config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let value: toml::Value = toml::from_str(&raw)?;
    if value.get("model_catalog_json").is_none() {
        return Ok(false);
    }
    let mut root = true;
    let mut kept = String::with_capacity(raw.len());
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            root = false;
        }
        let is_catalog = root
            && trimmed
                .strip_prefix("model_catalog_json")
                .is_some_and(|rest| rest.trim_start().starts_with('='));
        if !is_catalog {
            kept.push_str(line);
        }
    }
    let edited = match toml::from_str::<toml::Value>(&kept) {
        Ok(parsed) if parsed.get("model_catalog_json").is_none() => kept,
        // Unusual layout (multi-line value, quoted key): re-serialize instead.
        _ => {
            let mut value = value;
            value
                .as_table_mut()
                .context("native config must be a table")?
                .remove("model_catalog_json");
            toml::to_string(&value)?
        }
    };
    validate_shared_config(&edited)?;
    crate::config::atomic::write_private_bytes_atomic(&path, edited.as_bytes())?;
    Ok(true)
}

/// Locally installed native runtimes own their config schema. Do not keep a
/// second, progressively stale allowlist that rejects ordinary native settings.
pub fn validate_shared_config(raw: &str) -> anyhow::Result<()> {
    let value: toml::Value = toml::from_str(raw).map_err(|error: toml::de::Error| {
        anyhow::anyhow!("invalid native configuration: {}", error.message())
    })?;
    ensure!(value.is_table(), "native config must be a table");
    Ok(())
}

// A frontend-only release may retain the existing backend path and process.
// Its local fallback binaries must still be the selected backend/host bytes.
fn validate_frontend_neighbors(
    cli: &VerifiedFile,
    backend: &VerifiedFile,
    host: &VerifiedFile,
) -> anyhow::Result<()> {
    if cli.path.parent() == backend.path.parent() {
        return Ok(());
    }
    let parent = cli.path.parent().context("CLI parent missing")?;
    for selected in [backend, host] {
        let sibling = parent
            .join(
                selected
                    .path
                    .file_name()
                    .context("runtime filename missing")?,
            )
            .canonicalize()
            .context("frontend package is missing its selected app-server or code-mode host")?;
        if sibling != selected.path {
            VerifiedFile {
                path: sibling,
                sha256: selected.sha256.clone(),
            }
            .validate()?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn frontend_only_upgrade_keeps_backend_selection_and_rejects_mismatched_neighbors() {
    let dir = std::env::temp_dir().join(format!("cutex-frontend-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let old = dir.join("backend");
    let new = dir.join("frontend");
    std::fs::create_dir(&old).unwrap();
    std::fs::create_dir(&new).unwrap();
    let mut files = Vec::new();
    for name in ["codex-app-server", "codex-code-mode-host"] {
        let path = old.join(name);
        std::fs::write(&path, name).unwrap();
        std::fs::copy(&path, new.join(name)).unwrap();
        files.push(VerifiedFile {
            sha256: file_sha256(&path).unwrap(),
            path,
        });
    }
    let cli = VerifiedFile {
        path: new.join("codex"),
        sha256: files[0].sha256.clone(),
    };
    validate_frontend_neighbors(&cli, &files[0], &files[1]).unwrap();
    std::fs::write(new.join("codex-code-mode-host"), "wrong host").unwrap();
    assert!(validate_frontend_neighbors(&cli, &files[0], &files[1]).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

pub fn canonical(path: &Path) -> anyhow::Result<()> {
    ensure!(path.is_absolute(), "absolute stock path required");
    let resolved = path.canonicalize()?;
    #[cfg(not(windows))]
    ensure!(resolved == path, "canonical existing stock path required");
    #[cfg(windows)]
    {
        fn spelling(path: &Path) -> String {
            let text = path.to_string_lossy().replace('/', "\\");
            if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{unc}")
            } else {
                text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
            }
        }
        ensure!(
            spelling(&resolved).eq_ignore_ascii_case(&spelling(path)),
            "canonical existing stock path required"
        );
    }
    Ok(())
}

/// Current nonsecret configuration, bound separately from durable identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_projection: Option<super::selected_profile::Projection>,
    /// The reviewed private ChatGPT auth file of a profile with
    /// `privateChatgptAuthFile` on. The serialized name predates the generic
    /// switch and stays: launch receipts persist it, and older binaries
    /// (deny_unknown_fields) must still read them.
    #[serde(
        rename = "aemeath_auth",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub private_chatgpt_auth: Option<super::chatgpt_auth_file::ReviewedChatgptAuth>,
    pub profile_name: String,
    pub profile_id: String,
    pub inherited: bool,
    pub profile_sha256: Sha256,
    pub account_sha256: Sha256,
    pub model: String,
    pub reasoning: Option<String>,
    pub model_provider: String,
    pub provider: DummyProvider,
    pub sandbox: String,
    pub approval: String,
    /// Native `approvals_reviewer` for owner launches (`user` | `auto_review`);
    /// None leaves the profile config / native default. Launch receipts
    /// written before the field read as None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    /// workspace-write only: outbound network for sandboxed commands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub network_access: bool,
    /// workspace-write only: extra writable roots besides the cwd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
}

/// Named native permission profile cutex defines (via `-c permissions.<id>`)
/// when a workspace-write thread needs network access or extra writable
/// roots: explicitly selecting the built-in `:workspace` profile ignores the
/// legacy `[sandbox_workspace_write]` table, so the extras must live in a
/// profile that extends it.
pub const CUTEX_WORKSPACE_PERMISSION_PROFILE: &str = "cutex-workspace";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DummyProvider {
    pub name: String,
    pub base_url: String,
    pub wire_api: String,
    pub requires_openai_auth: bool,
    pub supports_websockets: bool,
}
impl StockConfiguration {
    /// Revalidate mutable Human New overrides against the source profile.
    ///
    /// Human New first resolves a valid profile default, then allows the user to
    /// change model and reasoning before an identity exists. Recompute the
    /// configuration from source after those edits so catalog restrictions fail
    /// before a native thread is persisted.
    /// Re-reviews a Human-created configuration (built by
    /// `local_configuration*`) after its model / effort was reselected. It
    /// must resolve the profile the same way that construction did, i.e.
    /// with the in-memory selected-profile-v2 projection: a profile whose
    /// `config.toml` predates `cutex_provider_mode` (ChatGPT / API-key
    /// profiles with an `auth.json`) is otherwise rejected by the private
    /// fake subset even though the unselected configuration was accepted.
    pub fn validate_runtime_selection(&self) -> anyhow::Result<()> {
        configuration_for_selection(
            Some(&self.profile_name),
            Some(self.sandbox.as_str()),
            Some(self.sandbox.clone()),
            Some(self.approval.clone()),
            Some(&self.model),
            self.reasoning.as_ref(),
            true,
        )?;
        Ok(())
    }

    /// Network / extra roots only matter for workspace-write.
    pub fn has_workspace_extras(&self) -> bool {
        self.sandbox == "workspace-write" && (self.network_access || !self.writable_roots.is_empty())
    }

    /// Native permission profile id selected by `default_permissions`.
    pub fn permission_profile_id(&self) -> anyhow::Result<&'static str> {
        if self.has_workspace_extras() {
            return Ok(CUTEX_WORKSPACE_PERMISSION_PROFILE);
        }
        match self.sandbox.as_str() {
            "read-only" => Ok(":read-only"),
            "workspace-write" => Ok(":workspace"),
            "danger-full-access" => Ok(":danger-full-access"),
            _ => anyhow::bail!("unknown receiver permission profile"),
        }
    }

    /// `[permissions.cutex-workspace]` definition when extras are present.
    pub fn permission_profile_definition(&self) -> Option<serde_json::Value> {
        self.has_workspace_extras().then(|| {
            let roots: serde_json::Map<String, serde_json::Value> = self
                .writable_roots
                .iter()
                .map(|root| (root.clone(), serde_json::Value::Bool(true)))
                .collect();
            let mut profile = serde_json::json!({
                "extends": ":workspace",
                "network": { "enabled": self.network_access },
            });
            if !roots.is_empty() {
                profile["workspace_roots"] = serde_json::Value::Object(roots);
            }
            profile
        })
    }

    /// Legacy `[sandbox_workspace_write]` table for bundles that still select
    /// permissions through `sandbox_mode` (no named profile).
    pub fn legacy_workspace_write_table(&self) -> Option<serde_json::Value> {
        self.has_workspace_extras().then(|| {
            serde_json::json!({
                "network_access": self.network_access,
                "writable_roots": self.writable_roots,
            })
        })
    }

    /// Apply resolved per-thread permissions (Human create paths).
    pub fn with_permissions(
        mut self,
        permissions: &crate::config::new_thread::LaunchPermissions,
    ) -> anyhow::Result<Self> {
        permissions.validate_shape()?;
        validate_selected_permissions(Some(&permissions.sandbox), &permissions.sandbox, &permissions.approval)?;
        self.sandbox = permissions.sandbox.clone();
        self.approval = permissions.approval.clone();
        self.approvals_reviewer = permissions.approvals_reviewer.clone();
        self.network_access = permissions.network_access;
        self.writable_roots = permissions.writable_roots.clone();
        Ok(self)
    }

    pub fn validate_job_requirement(&self, present: bool) -> anyhow::Result<()> {
        ensure!(
            self.selected_projection
                .as_ref()
                .is_none_or(|p| !p.requires_job || present),
            "selected profile requires an explicit reviewed coherent Job descriptor"
        );
        Ok(())
    }
    pub fn validate_auth_home(&self, home: &Path) -> anyhow::Result<()> {
        if let Some(projection) = &self.selected_projection {
            ensure!(
                self.private_chatgpt_auth.is_none(),
                "conflicting reviewed auth modes"
            );
            projection.validate()?;
            if let Some(status) = &projection.status {
                status.validate_projection(
                    &projection
                        .settings
                        .tui
                        .as_ref()
                        .context("status selection missing")?
                        .status_line
                        .as_deref()
                        .unwrap_or_default(),
                    &self.profile_name,
                )?;
            }
            return Ok(());
        }
        match &self.private_chatgpt_auth {
            Some(auth) => {
                ensure!(
                    auth.version == 1 && auth.path == home.join("auth.json"),
                    "reviewed native auth home/version mismatch"
                );
                ensure!(
                    super::chatgpt_auth_file::review(auth.path.clone())? == *auth,
                    "reviewed native auth custody/account changed"
                );
            }
            None => ensure!(
                !home.join("auth.json").try_exists()?,
                "stock fake subset does not consume native auth files"
            ),
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileConfig {
    #[serde(default)]
    cutex_provider_mode: Option<String>,
    model: String,
    model_provider: String,
    model_reasoning_effort: Option<String>,
    #[serde(default)]
    model_providers: BTreeMap<String, DummyProvider>,
}

#[derive(Debug, PartialEq)]
enum ProviderMode {
    /// Private loopback fake provider (tests / rehearsals).
    Fake,
    /// The legacy marker of the private ChatGPT mode
    /// (`generic_switches::legacy::PRIVATE_CHATGPT_MODE`): accepted only
    /// together with the profile switch `privateChatgptAuthFile`.
    LegacyPrivateChatgpt,
}

impl ProviderMode {
    fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value {
            None | Some("fake") => Ok(Self::Fake),
            Some(mode) if mode == crate::profiles::generic_switches::legacy::PRIVATE_CHATGPT_MODE => {
                Ok(Self::LegacyPrivateChatgpt)
            }
            Some(_) => anyhow::bail!("unsupported stock profile configuration"),
        }
    }
}

pub fn current_configuration(record: &CutexSessionRecord) -> anyhow::Result<StockConfiguration> {
    configuration_for_record(
        record,
        record
            .explicit_launch
            .as_ref()
            .is_some_and(|c| c.version == 4 || (c.version == 3 && c.migration_action_id.is_some())),
    )
}

/// Only maintenance review calls this before installing the explicit v3 marker.
/// The original profile and independent auth path remain authoritative.
pub(crate) fn migration_configuration(
    record: &CutexSessionRecord,
) -> anyhow::Result<StockConfiguration> {
    configuration_for_record(record, true)
}

fn configuration_for_record(
    record: &CutexSessionRecord,
    migration: bool,
) -> anyhow::Result<StockConfiguration> {
    ensure!(
        record.default_cli_args.is_empty(),
        "stock does not accept arbitrary durable CLI overrides"
    );
    let (sandbox, approval) = crate::runtime::args::effective_runtime_permission_defaults(record);
    // A stale preset alias next to an explicit sandbox (written by older
    // daemons on a running-thread permission change) follows the explicit
    // sandbox instead of bricking the session; online persists the repair.
    let permission = crate::runtime::args::effective_permission_alias(record);
    let mut configuration = configuration_for_selection(
        record.profile.as_ref(),
        permission.as_deref(),
        sandbox,
        approval,
        record.model_defaults.as_ref(),
        record.reasoning_defaults.as_ref(),
        migration,
    )?;
    configuration.approvals_reviewer = record.approvals_reviewer.clone();
    configuration.network_access = record.sandbox_network_access.unwrap_or(false);
    configuration.writable_roots = record.sandbox_writable_roots.clone();
    if !record.agent_enabled
        && record.registration_class == crate::agent_bus::model::AgentRegistrationClass::LocalOnly
    {
        if let Some(projection) = configuration.selected_projection.as_mut() {
            projection.requires_job = false;
        }
    }
    Ok(configuration)
}

/// Configuration is independent of identity. Bootstrap has no durable/native
/// ID yet, so it must not construct a placeholder session to resolve a profile.
pub fn bootstrap_configuration(
    spec: &crate::agent_management::ManagedAgentSpec,
) -> anyhow::Result<StockConfiguration> {
    ensure!(
        spec.runtime_backend == "host",
        "stock bootstrap requires host backend"
    );
    configuration_for_selection(
        spec.profile.as_ref(),
        Some(&spec.permissions),
        Some(spec.sandbox_mode.clone()),
        Some(spec.approval_policy.clone()),
        Some(&spec.model),
        Some(&spec.reasoning),
        false,
    )
}

/// Installed native defaults accept the same selected-profile projection as
/// Human New, while keeping the typed request's explicit model and permissions.
pub fn local_bootstrap_configuration(
    spec: &crate::agent_management::ManagedAgentSpec,
) -> anyhow::Result<StockConfiguration> {
    configuration_for_selection(
        spec.profile.as_ref(),
        Some(&spec.permissions),
        Some(spec.sandbox_mode.clone()),
        Some(spec.approval_policy.clone()),
        Some(&spec.model),
        Some(&spec.reasoning),
        true,
    )
}

/// Human new-agent defaults resolve the installed profile without inventing an identity.
pub fn local_configuration() -> anyhow::Result<StockConfiguration> {
    local_configuration_with_profile(None)
}

pub fn local_configuration_with_profile(
    profile: Option<&String>,
) -> anyhow::Result<StockConfiguration> {
    local_configuration_with_selection(profile, None, None)
}

/// As [`local_configuration_with_profile`], with an explicit model / effort
/// taking precedence over the new-session defaults and the profile's own
/// model. Lets a creator pick a model for a profile that configures none
/// (e.g. a ChatGPT profile without `model`).
pub fn local_configuration_with_selection(
    profile: Option<&String>,
    model: Option<&String>,
    reasoning: Option<&String>,
) -> anyhow::Result<StockConfiguration> {
    let config = crate::config::store::load_codez_config_checked()?;
    let name = profile
        .or(config.default_profile.as_ref())
        .context("Select a default profile in Settings / Defaults")?;
    // Clients may select the profile by id; defaults are keyed by name.
    let name = &crate::config::new_session::resolve_profile_name(name)?;
    let inherited = crate::config::new_session::inherited(name)?;
    let defaults = config.new_session_defaults.get(name);
    configuration_for_selection(
        Some(name),
        Some("danger-full-access"),
        Some("danger-full-access".into()),
        Some("never".into()),
        model.or(defaults.and_then(|d| d.model.as_ref())).or(inherited.model.as_ref()),
        reasoning
            .or(defaults.and_then(|d| d.reasoning.as_ref()))
            .or(inherited.reasoning.as_ref()),
        true,
    )
}

fn configuration_for_selection(
    selected_profile: Option<&String>,
    permission: Option<&str>,
    sandbox: Option<String>,
    approval: Option<String>,
    selected_model: Option<&String>,
    selected_reasoning: Option<&String>,
    migration: bool,
) -> anyhow::Result<StockConfiguration> {
    use crate::profiles::model::{AccountsStore, CliKind, RuntimeConfig};
    // `default` / empty select the profile default (agent_create, §8.2).
    let selected_reasoning =
        selected_reasoning.filter(|r| !crate::profiles::reasoning::is_default(r));
    let config = crate::config::store::load_codez_config_checked()?;
    let inherited = selected_profile.is_none();
    let name = selected_profile
        .or(config.default_profile.as_ref())
        .context("stock inherited profile unavailable; explicit current default required")?;
    ensure!(!name.trim().is_empty(), "stock profile is empty");
    // An official profile with model edits selects the effective catalog of
    // the live upstream list plus its overlay (content-addressed file, so the
    // review below digests exactly what the runtime reads). Best effort.
    crate::profiles::catalog_overlay::refresh_for_launch(name);
    // Read current store without canonicalizing, selecting another account or writing it.
    let accounts: AccountsStore =
        serde_json::from_slice(&std::fs::read(crate::config::paths::accounts_path()?)?)?;
    ensure!(
        accounts.version == crate::profiles::model::STORE_VERSION,
        "unsupported stock account store version"
    );
    let matches: Vec<_> = accounts
        .accounts
        .iter()
        .filter(|a| &a.name == name)
        .collect();
    // A profile selected by its stable id resolves to the same account.
    let matches = if matches.is_empty() {
        accounts.accounts.iter().filter(|a| &a.id == name).collect()
    } else {
        matches
    };
    ensure!(
        !matches.is_empty(),
        "stock profile missing ({})",
        crate::profiles::identity::unknown_profile_message(
            name,
            &crate::profiles::identity::available_profile_names(&accounts),
        )
    );
    ensure!(matches.len() == 1, "stock profile ambiguous: {name}");
    let account = matches[0];
    let name = &account.name;
    ensure!(
        uuid::Uuid::parse_str(&account.id)?.to_string() == account.id,
        "stock profile id must be an exact UUID"
    );
    ensure!(
        matches!(account.runtime, RuntimeConfig::Host)
            && account.cli_kind == CliKind::Codex
            && account.session.is_none()
            && account.default_cli_args.is_empty(),
        "unsupported stock account/runtime/options"
    );
    let files = crate::profiles::materialize::materialized_account_files(account)?;
    canonical(&files.config_path)?;
    let raw = std::fs::read_to_string(&files.config_path)?;
    let mode: toml::Value =
        toml::from_str(&raw).map_err(|_| anyhow::anyhow!("invalid profile configuration"))?;
    let selected_v2 = mode.get("cutex_provider_mode").and_then(toml::Value::as_str)
        == Some("selected_profile_v2");
    // profile-generic-switches: the switch selects the private ChatGPT auth
    // launch below; the selected-profile projection (Human New, installed
    // defaults) would silently use the profile's own credential instead.
    ensure!(
        !(account.private_chatgpt_auth_file && (migration || selected_v2)),
        "privateChatgptAuthFile profiles launch only with the private ChatGPT auth file (explicit agent launch); turn the switch off to use the profile's own login"
    );
    if migration
        || mode
            .get("cutex_provider_mode")
            .and_then(toml::Value::as_str)
            == Some("selected_profile_v2")
    {
        let projected_raw = if migration {
            let mut value = mode.clone();
            let table = value.as_table_mut().context("profile table required")?;
            ensure!(
                !table.contains_key("cutex_provider_mode")
                    || table["cutex_provider_mode"].as_str() == Some("selected_profile_v2"),
                "conflicting original provider mode"
            );
            table.insert(
                "cutex_provider_mode".into(),
                toml::Value::String("selected_profile_v2".into()),
            );
            toml::to_string(&value)?
        } else {
            raw.clone()
        };
        let mut selected = super::selected_profile::Config::parse(&projected_raw)?;
        if (selected_model.is_none() && selected.model.is_none())
            || (selected_reasoning.is_none() && selected.model_reasoning_effort.is_none())
        {
            let home = super::local_deployment::LocalDeployment::source_home()?;
            let shared: toml::Value =
                toml::from_str(&std::fs::read_to_string(home.join("config.toml"))?)?;
            selected.inherit_model_defaults(&shared);
        }
        let (mut projection, model, reasoning) = selected.review(
            &account.id,
            files.auth_path.clone(),
            selected_model,
            selected_reasoning,
        )?;
        if let Some(tui) = &projection.settings.tui {
            projection.status = super::selected_status::Status::review(
                &files.custom_status_items_path,
                tui.status_line.as_deref().unwrap_or_default(),
                &account.name,
            )?;
        }
        let sandbox = sandbox.context("explicit selected sandbox required")?;
        let approval = approval.context("explicit selected approval required")?;
        validate_selected_permissions(permission, &sandbox, &approval)?;
        let chatgpt = projection.route == super::selected_profile::Route::ChatgptFile;
        use sha2::Digest;
        let profile_sha256 = file_sha256(&files.config_path)?;
        ensure!(
            format!("{:x}", sha2::Sha256::digest(raw.as_bytes())) == profile_sha256.as_str(),
            "selected profile changed during review"
        );
        let provider_id = projection.provider_id().to_owned();
        let provider = if let Some(api) = &projection.api {
            DummyProvider {
                name: api.provider.name.clone(),
                base_url: api.provider.base_url.clone(),
                wire_api: api.provider.wire_api.clone(),
                requires_openai_auth: false,
                supports_websockets: false,
            }
        } else {
            // Only the ChatGPT route has no API connection (the API-key
            // route always carries its provider).
            ensure!(chatgpt, "selected API route without a provider connection");
            DummyProvider {
                name: "OpenAI".into(),
                base_url: super::chatgpt_auth_file::ENDPOINT.into(),
                wire_api: "responses".into(),
                requires_openai_auth: true,
                supports_websockets: true,
            }
        };
        return Ok(StockConfiguration {
            selected_projection: Some(projection),
            private_chatgpt_auth: None,
            profile_name: name.clone(),
            profile_id: account.id.clone(),
            inherited,
            profile_sha256,
            account_sha256: Sha256::new(format!(
                "{:x}",
                sha2::Sha256::digest(
                    crate::profiles::generic_switches::launch_account_digest_input(account)?
                )
            ))
            .map_err(|_| anyhow::anyhow!("invalid selected account digest"))?,
            model,
            reasoning,
            model_provider: provider_id,
            provider,
            sandbox,
            approval,
            approvals_reviewer: None,
            network_access: false,
            writable_roots: Vec::new(),
        });
    }
    ensure!(
        !files.auth_path.try_exists()?,
        "stock private subset does not consume profile auth files"
    );
    let profile: ProfileConfig = toml::from_str(&raw)
        .map_err(|_| anyhow::anyhow!("unsupported stock profile configuration"))?;
    let provider_mode = ProviderMode::parse(profile.cutex_provider_mode.as_deref())?;
    let private_chatgpt = account.private_chatgpt_auth_file;
    ensure!(
        private_chatgpt || provider_mode != ProviderMode::LegacyPrivateChatgpt,
        "the profile config carries the legacy private ChatGPT marker; turn on privateChatgptAuthFile for this profile"
    );
    ensure!(
        !private_chatgpt || profile.cutex_provider_mode.as_deref() != Some("fake"),
        "privateChatgptAuthFile conflicts with the fake provider mode"
    );
    let (provider, private_chatgpt_auth) = match private_chatgpt {
        false => {
            ensure!(
                account.email.is_none() && account.plan_type.is_none(),
                "real account authentication is unsupported by private fake subset"
            );
            ensure!(
                profile.model_providers.len() == 1,
                "exactly one dummy provider required"
            );
            let provider = profile
                .model_providers
                .get(&profile.model_provider)
                .context("configured provider missing")?
                .clone();
            let url = url::Url::parse(&provider.base_url)?;
            ensure!(
                url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1") | Some("[::1]"))
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && !provider.requires_openai_auth
                    && !provider.supports_websockets
                    && provider.wire_api == "responses",
                "only private loopback unauthenticated fake Responses provider is supported"
            );
            (provider, None)
        }
        true => {
            // The native subscription route: builtin provider, no overrides.
            ensure!(
                profile.model_provider == "openai" && profile.model_providers.is_empty(),
                "privateChatgptAuthFile requires the builtin openai provider without provider overrides"
            );
            let auth = super::chatgpt_auth_file::review(
                crate::config::paths::host_codex_home_dir()?.join("auth.json"),
            )?;
            (
                DummyProvider {
                    name: "OpenAI".into(),
                    base_url: super::chatgpt_auth_file::ENDPOINT.into(),
                    wire_api: "responses".into(),
                    requires_openai_auth: true,
                    supports_websockets: true,
                },
                Some(auth),
            )
        }
    };
    let sandbox = sandbox.context("explicit stock sandbox required")?;
    ensure!(
        matches!(
            sandbox.as_str(),
            "read-only" | "workspace-write" | "danger-full-access"
        ),
        "unsupported stock sandbox"
    );
    // Approval remains explicit: do not silently turn full access into never.
    let approval = approval.context("explicit stock approval policy required")?;
    ensure!(
        matches!(approval.as_str(), "untrusted" | "on-request" | "never"),
        "unsupported stock approval policy"
    );
    if let Some(permission) = permission {
        ensure!(
            crate::runtime::args::permission_alias_sandbox(permission) == Some(sandbox.as_str()),
            "unknown or inconsistent stock permission alias"
        );
    }
    if private_chatgpt_auth.is_some() {
        crate::profiles::generic_switches::validate_private_chatgpt_model(
            &profile.model,
            profile.model_reasoning_effort.as_deref(),
            selected_model.map(String::as_str),
            selected_reasoning.map(String::as_str),
        )?;
    }
    let model = selected_model.cloned().unwrap_or(profile.model);
    ensure!(
        !model.trim().is_empty() && !model.chars().any(char::is_control),
        "invalid stock model"
    );
    let reasoning = selected_reasoning
        .cloned()
        .or(profile.model_reasoning_effort);
    ensure!(
        reasoning
            .as_deref()
            .is_none_or(|r| crate::profiles::reasoning::NATIVE_EFFORTS.contains(&r)),
        "unsupported stock reasoning"
    );
    use sha2::Digest;
    Ok(StockConfiguration {
        selected_projection: None,
        private_chatgpt_auth,
        profile_name: name.clone(),
        profile_id: account.id.clone(),
        inherited,
        profile_sha256: file_sha256(&files.config_path)?,
        account_sha256: Sha256::new(format!(
            "{:x}",
            sha2::Sha256::digest(crate::profiles::generic_switches::launch_account_digest_input(account)?)
        ))
        .map_err(|_| anyhow::anyhow!("invalid account digest"))?,
        model,
        reasoning,
        model_provider: profile.model_provider,
        provider,
        sandbox,
        approval,
        approvals_reviewer: None,
        network_access: false,
        writable_roots: Vec::new(),
    })
}

fn validate_selected_permissions(
    permission: Option<&str>,
    sandbox: &str,
    approval: &str,
) -> anyhow::Result<()> {
    ensure!(
        matches!(approval, "never" | "on-request" | "untrusted"),
        "unsupported selected approval"
    );
    let expected = permission
        .and_then(crate::runtime::args::permission_alias_sandbox)
        .ok_or_else(|| anyhow::anyhow!("unknown selected permission alias"))?;
    ensure!(
        sandbox == expected,
        "inconsistent selected sandbox and permission alias"
    );
    Ok(())
}

pub fn validate_native(
    record: &CutexSessionRecord,
    sessions: &CutexSessionStore,
    contract: &ExplicitLaunchContract,
) -> anyhow::Result<PathBuf> {
    ensure!(
        crate::runtime::lifecycle::cutex_session_host_is_local(
            &record.host_id,
            &crate::platform::host::current_host_name()
        ),
        "stock host is not local"
    );
    ensure!(
        record.runtime_backend == CutexSessionRuntimeBackend::Host,
        "stock subset requires existing host backend"
    );
    ensure!(
        record.codex_session_id.as_deref() == Some(&contract.native_id),
        "stock native identity mismatch"
    );
    ensure!(
        sessions
            .sessions
            .values()
            .filter(|r| r.codex_session_id.as_deref() == Some(&contract.native_id))
            .count()
            == 1,
        "ambiguous durable/native mapping"
    );
    if contract.version == 4 {
        ensure!(
            contract.native_home
                == crate::config::paths::runtime_dir()?
                    .join("light/agents")
                    .join(&contract.native_id)
                    .canonicalize()?,
            "local runtime home does not match saved identity"
        );
    } else if contract.version == 3 {
        #[cfg(target_os = "linux")]
        crate::agent_management::validate_migration_home(record, sessions, contract)?;
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("maintenance home unsupported on this platform");
    } else {
        ensure!(
            crate::config::paths::host_codex_home_dir()?.canonicalize()? == contract.native_home,
            "stock home is not authoritative native home"
        );
    }
    current_configuration(record)?.validate_auth_home(&contract.native_home)?;
    super::native_history::current(&contract.native_home, &contract.native_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_permission_aliases_preserve_effective_policy() {
        for (alias, sandbox) in [
            (":danger-full-access", "danger-full-access"),
            ("danger-full-access", "danger-full-access"),
            (":read-only", "read-only"),
        ] {
            for approval in ["never", "on-request"] {
                assert!(validate_selected_permissions(Some(alias), sandbox, approval).is_ok());
            }
            assert!(validate_selected_permissions(Some(alias), "guessed", "never").is_err());
        }
        assert!(
            validate_selected_permissions(Some("unknown"), "danger-full-access", "never").is_err()
        );
        assert!(validate_selected_permissions(Some(":read-only"), "read-only", "always").is_err());
    }

    #[test]
    fn stale_alias_next_to_explicit_sandbox_does_not_brick_launch() {
        // Linux host, session cutex.01a1141b (r208): alias workspace-write, explicit full access.
        let mut record = CutexSessionRecord::new_at(
            "cutex.stale-alias".to_string(),
            Some("thread-stale-alias".to_string()),
            "host".to_string(),
            "/tmp/stale-alias".to_string(),
            None,
            "2026-10-08T00:00:00Z".to_string(),
        )
        .expect("record");
        record.permission_defaults = Some("workspace-write".to_string());
        record.sandbox_mode = Some("danger-full-access".to_string());
        record.approval_policy = Some("never".to_string());
        let (sandbox, approval) = crate::runtime::args::effective_runtime_permission_defaults(&record);
        let (sandbox, approval) = (sandbox.unwrap(), approval.unwrap());
        assert!(validate_selected_permissions(
            record.permission_defaults.as_deref(),
            &sandbox,
            &approval
        )
        .is_err());
        let alias = crate::runtime::args::effective_permission_alias(&record);
        validate_selected_permissions(alias.as_deref(), &sandbox, &approval).unwrap();
        assert_eq!(sandbox, "danger-full-access");
        // A custom alias is not guessed away.
        record.permission_defaults = Some("custom-profile".to_string());
        let alias = crate::runtime::args::effective_permission_alias(&record);
        assert!(validate_selected_permissions(alias.as_deref(), &sandbox, &approval).is_err());
    }

    #[test]
    fn runtime_revalidation_requires_the_canonical_sandbox_alias() {
        for sandbox in ["danger-full-access", "read-only", "workspace-write"] {
            assert!(validate_selected_permissions(Some(sandbox), sandbox, "never").is_ok());
        }
        assert!(
            validate_selected_permissions(None, "danger-full-access", "never")
                .unwrap_err()
                .to_string()
                .contains("unknown selected permission alias"),
            "runtime revalidation must pass the canonical sandbox alias, not None"
        );
    }
    #[test]
    #[ignore = "requires the task-owned accepted native manifest"]
    fn accepted_auth_manifest_matches_compiled_pins() {
        let path = Path::new(
            "/srv/projects/cutex-light-core-r1/artifacts/custom-status-r1/build-manifest.json",
        );
        let bytes = std::fs::read(path).expect("accepted immutable native manifest");
        use sha2::Digest;
        assert_eq!(
            format!("{:x}", sha2::Sha256::digest(&bytes)),
            "6727a35e9154ed53f1384e3c3bcc5362097dd4ef6e3c9de7dd93dec1d3556063"
        );
        let m: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(m["source_commit"], S6E_COMMIT);
        for (name, expected) in [
            ("bundle/codex", S6E_CLI_SHA256),
            ("bundle/codex-app-server", S6E_EXECUTABLE_SHA256),
            ("bundle/codex-code-mode-host", STOCK_HOST_SHA256),
            (
                "bundle/codex_app_server_protocol.schemas.json",
                S6E_SCHEMA_SHA256,
            ),
        ] {
            assert_eq!(m["files"][name]["sha256"], expected);
        }
    }
    #[test]
    fn local_deployment_accepts_rebuilt_bytes_and_any_schema() {
        let file = |hash: &str| VerifiedFile {
            path: "/private/file".into(),
            sha256: Sha256::new(hash.to_string()).unwrap(),
        };
        let mut bundle = StockBundle {
            version: 4,
            upstream_commit: "local-upstream".into(),
            native_patch_commit: Some("local-fix".into()),
            executable: file(&"a".repeat(64)),
            cli: Some(file(&"b".repeat(64))),
            code_mode_host: file(&"c".repeat(64)),
            facade: file(&"d".repeat(64)),
            schema: file(S6E_SCHEMA_SHA256),
            launch_config_sha256: None,
            shared_config: file(&"e".repeat(64)),
        };
        bundle.validate_identity().unwrap();
        assert!(bundle.common_ingress() && bundle.soon_ingress());
        // Artifact eligibility is not successful integrity validation.
        assert!(bundle.validate_components().is_err());
        // A rebuilt or upgraded native (new schema bytes, e.g. without the
        // legacy inter-agent item) stays launchable; capabilities are
        // negotiated at initialize.
        for hash in [
            NATIVE_160_R47_SCHEMA_SHA256,
            "4da7c17ec5397b62cc70e68515ab221946e8c9a82f4d66992cb3559de1dcdf7f",
            &"f".repeat(64),
        ] {
            bundle.schema = file(hash);
            bundle.validate_identity().unwrap();
            assert!(bundle.common_ingress() && bundle.soon_ingress());
        }
        // The TUI frontend is optional for v4 runtimes.
        bundle.cli = None;
        bundle.validate_identity().unwrap();
        assert!(bundle.common_ingress());
    }

    #[test]
    fn native_version_is_a_minimum_not_an_allowlist() {
        for (agent, version) in [
            ("codex_app_server/0.160.0-r47 (Windows)", Some((0, 160, 0))),
            ("cutex_catalog/0.153.2 (test)", Some((0, 153, 2))),
            ("x/1.2", Some((1, 2, 0))),
            ("no-version", None),
        ] {
            assert_eq!(native_version_from_user_agent(agent), version, "{agent}");
        }
        assert!(native_version_supported((0, 153, 0)));
        assert!(native_version_supported((0, 160, 3)));
        assert!(native_version_supported((1, 0, 0)));
        assert!(!native_version_supported((0, 152, 9)));
    }

    #[test]
    fn initialize_negotiation_requires_only_external_input_v1() {
        let minimal = serde_json::json!({"externalInputVersion":1,"userAgent":"codex/0.160.0"});
        assert!(validate_ingress_capability(true, &minimal).is_ok());
        // Presentation, structured-view v2 and soon are optional here.
        assert!(validate_ingress_capability(
            true,
            &serde_json::json!({"externalInputVersion":1,"externalInputVersions":[1,2],"presentationVersion":1,"externalInputDeliveries":["after_turn","passive","soon"]})
        )
        .is_ok());
        assert!(validate_ingress_capability(true, &serde_json::json!({"userAgent":"codex/0.160.0"})).is_err());
        assert!(validate_ingress_capability(true, &serde_json::json!({"externalInputVersion":2})).is_err());
        assert!(validate_ingress_capability(false, &serde_json::json!({})).is_ok());
        assert!(validate_ingress_capability(false, &serde_json::json!({"userAgent":"codex/0.150.1"})).is_err());
        // The Claude adapter has its own version scheme: no Codex floor (Linux r199 dry run).
        assert!(validate_ingress_capability(false, &serde_json::json!({"userAgent":"cute-claude-app-server/0.3.0"})).is_ok());
        assert!(user_agent_is_claude_adapter("cute-claude-app-server/0.3.0 (Linux)"));
        assert!(!user_agent_is_claude_adapter("codex_app_server/0.160.0-r47 (Windows)"));
        assert!(schema_has_external_input("U-0.153.4+provider-item-id-ca580a78-soon-v1", &"f".repeat(64)));
        assert!(schema_has_external_input("U-0.153.4+S6-c2aaceb4-external-input-v1", &"f".repeat(64)));
        assert!(!schema_has_external_input("stock-0.153.4-app-server-v2", &"f".repeat(64)));
        assert!(!schema_has_external_input("0.144.1+cutex-inter-agent-v2", &"f".repeat(64)));
        assert!(is_stock_schema_version("stock-0.153.4-app-server-v2"));
        assert!(!is_stock_schema_version("0.144.1+cutex-inter-agent-v2"));
        assert!(!is_common_ingress_schema(STOCK_SCHEMA_SHA256));
    }
    #[test]
    fn exact_s6_bundle_identity_not_boolean_capability() {
        let file = |hash: &str| VerifiedFile {
            path: "/private/file".into(),
            sha256: Sha256::new(hash.to_string()).unwrap(),
        };
        let mut b = StockBundle {
            version: 2,
            upstream_commit: STOCK_COMMIT.into(),
            native_patch_commit: Some(S6_COMMIT.into()),
            executable: file(S6_EXECUTABLE_SHA256),
            cli: None,
            code_mode_host: file(STOCK_HOST_SHA256),
            facade: file(STOCK_HOST_SHA256),
            schema: file(S6_SCHEMA_SHA256),
            launch_config_sha256: None,
            shared_config: file(STOCK_HOST_SHA256),
        };
        b.validate_identity().unwrap();
        assert!(b.common_ingress());
        b.native_patch_commit = Some(STOCK_COMMIT.into());
        assert!(b.validate_identity().is_err());
        b.native_patch_commit = Some(S6_COMMIT.into());
        b.executable = file(STOCK_EXECUTABLE_SHA256);
        assert!(b.validate_identity().is_err());
        b.version = 1;
        b.native_patch_commit = None;
        b.schema = file(STOCK_SCHEMA_SHA256);
        b.validate_identity().unwrap();
        assert!(!b.common_ingress());
        b.version = 3;
        b.native_patch_commit = Some(S6E_COMMIT.into());
        b.executable = file(S6E_EXECUTABLE_SHA256);
        b.schema = file(S6E_SCHEMA_SHA256);
        assert!(b.validate_identity().is_err());
        b.cli = Some(file(S6E_CLI_SHA256));
        b.validate_identity().unwrap();
        assert!(b.common_ingress() && b.soon_ingress());
        let accepted = b.clone();
        // Each mixed/spoofed component rejects independently, including the
        // previous coherent bundle. No descendant/version wildcard or fallback.
        for (field, value) in [
            (
                "native_patch_commit",
                "cc4a080df1df4433f6fd67fee9c1c4fa4a42baab",
            ),
            (
                "cli",
                "52b868441c65cf12370ffbd7dea30d972b801f875b7e3d69b41ce2a0501fd2a9",
            ),
            (
                "executable",
                "df95936f3f0d1ff62efb978fee32b85e45da0f7f3166de7606cce3b49fb12018",
            ),
            (
                "schema",
                "77e75b7fc47c9b8a9caacf5a7d31c040c6520679a9833f1c2b0e55937ed39c27",
            ),
            (
                "native_patch_commit",
                "3d8a73a747cf5b957a7ca0491c28d1517f6d7722",
            ),
            (
                "cli",
                "d97f8d4f32377b22066d25dd545b79b32bb9cacb8b501f9db9a21c290504ad60",
            ),
            (
                "native_patch_commit",
                "ca580a783fc1ab34613be4f81ceab96ef4d393a2",
            ),
            (
                "executable",
                "cac03d6d1b77e4d3681a2d71b25435a537bc257807c28ea0f5aff491c622b69d",
            ),
            (
                "cli",
                "9e887b3a3439154fc00f8bb2a44b0f534ba33c587a5aa0f2aed354b7842bef15",
            ),
            (
                "schema",
                "459861225d5bfb73bb4c3896edb489169637424be410be346f955a39596da7e9",
            ),
            (
                "native_patch_commit",
                "0c425b5f9fca90835fd2b4377a1bca212532f66c",
            ),
            (
                "executable",
                "9caa26abe4ec3094543b402e235654f8434d09e49e14017b856b4cdac0801bde",
            ),
            (
                "cli",
                "f93c92bfe528636eae87450d918700d90d24db7d8f2eee0f4dd0fe5cbee998f4",
            ),
            (
                "native_patch_commit",
                "a83dbb47ba6aa775f5d4b679fafc532c4db74c7f",
            ),
            (
                "executable",
                "4638b86221593dd4bab1f66b504641836ac1adb864e946cbe42cb5cbf9f05a74",
            ),
            (
                "cli",
                "f360100339560e6a57eb08dea38ed740450904ce7bd72ea7b8a37238fff97bd6",
            ),
            ("code_mode_host", STOCK_EXECUTABLE_SHA256),
            ("schema", S6_SCHEMA_SHA256),
            ("executable", STOCK_HOST_SHA256),
        ] {
            let mut raw = serde_json::to_value(&accepted).unwrap();
            if field == "native_patch_commit" {
                raw[field] = value.into();
            } else {
                raw[field]["sha256"] = value.into();
            }
            let rejected: StockBundle = serde_json::from_value(raw).unwrap();
            assert!(rejected.validate_identity().is_err(), "{field}");
        }
        let mut old = accepted.clone();
        old.native_patch_commit = Some("a83dbb47ba6aa775f5d4b679fafc532c4db74c7f".into());
        old.executable = file("4638b86221593dd4bab1f66b504641836ac1adb864e946cbe42cb5cbf9f05a74");
        old.cli = Some(file(
            "f360100339560e6a57eb08dea38ed740450904ce7bd72ea7b8a37238fff97bd6",
        ));
        assert!(old.validate_identity().is_err());
        assert!(!old.common_ingress() && !old.soon_ingress());
        old.native_patch_commit = Some("0c425b5f9fca90835fd2b4377a1bca212532f66c".into());
        old.executable = file("9caa26abe4ec3094543b402e235654f8434d09e49e14017b856b4cdac0801bde");
        old.cli = Some(file(
            "f93c92bfe528636eae87450d918700d90d24db7d8f2eee0f4dd0fe5cbee998f4",
        ));
        assert!(old.validate_identity().is_err());
        assert!(!old.common_ingress() && !old.soon_ingress());
        b.version = 2;
        assert!(b.validate_identity().is_err());
        b.version = 3;
        b.cli = Some(file(STOCK_EXECUTABLE_SHA256));
        assert!(b.validate_identity().is_err());
        let mut raw = serde_json::to_value(b).unwrap();
        raw["externalInput"] = true.into();
        assert!(serde_json::from_value::<StockBundle>(raw).is_err());
    }

    #[test]
    fn private_chatgpt_marker_is_explicit_and_unknown_mcp_stays_rejected() {
        let base = "model='m'\nmodel_provider='openai'\nmodel_reasoning_effort='low'\n";
        let plain: ProfileConfig = toml::from_str(base).unwrap();
        assert_eq!(ProviderMode::parse(plain.cutex_provider_mode.as_deref()).unwrap(), ProviderMode::Fake);
        let marker = crate::profiles::generic_switches::legacy::PRIVATE_CHATGPT_MODE;
        let explicit: ProfileConfig =
            toml::from_str(&format!("{base}cutex_provider_mode='{marker}'\n")).unwrap();
        assert_eq!(
            ProviderMode::parse(explicit.cutex_provider_mode.as_deref()).unwrap(),
            ProviderMode::LegacyPrivateChatgpt
        );
        let other: ProfileConfig =
            toml::from_str(&format!("{base}cutex_provider_mode='{marker}x'\n")).unwrap();
        assert!(ProviderMode::parse(other.cutex_provider_mode.as_deref()).is_err());
        assert!(
            toml::from_str::<ProfileConfig>(&format!(
                "{base}[mcp_servers.arbitrary]\ncommand='anything'\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn private_chatgpt_auth_keeps_its_persisted_field_name() {
        // Launch receipts written by older binaries (deny_unknown_fields)
        // name the reviewed auth by its pre-switch field; both directions
        // must keep reading.
        let value = serde_json::json!({"version": 1, "path": "/x/auth.json", "device": 1, "inode": 2,
            "owner": 3, "directory_device": 4, "directory_inode": 5, "account_sha256": "a".repeat(64)});
        let auth: super::super::chatgpt_auth_file::ReviewedChatgptAuth = serde_json::from_value(value).unwrap();
        let configuration = StockConfiguration {
            selected_projection: None,
            private_chatgpt_auth: Some(auth),
            profile_name: "alpha".into(),
            profile_id: "00000000-0000-4000-8000-000000000000".into(),
            inherited: false,
            profile_sha256: Sha256::new("b".repeat(64)).unwrap(),
            account_sha256: Sha256::new("c".repeat(64)).unwrap(),
            model: "m".into(),
            reasoning: Some("low".into()),
            model_provider: "openai".into(),
            provider: DummyProvider {
                name: "OpenAI".into(),
                base_url: super::super::chatgpt_auth_file::ENDPOINT.into(),
                wire_api: "responses".into(),
                requires_openai_auth: true,
                supports_websockets: true,
            },
            sandbox: "read-only".into(),
            approval: "never".into(),
            approvals_reviewer: None,
            network_access: false,
            writable_roots: Vec::new(),
        };
        let wire = serde_json::to_value(&configuration).unwrap();
        let object = wire.as_object().unwrap();
        assert!(!object.contains_key("private_chatgpt_auth"));
        let legacy_name = object.keys().find(|key| key.ends_with("_auth")).unwrap();
        assert_ne!(legacy_name, "private_chatgpt_auth");
        assert_eq!(serde_json::from_value::<StockConfiguration>(wire).unwrap(), configuration);
    }

    #[test]
    fn presentation_changes_do_not_change_launch_semantics() {
        let base = "model='gpt-5'\n";
        let expected = launch_config_digest(base).unwrap();
        for tui in [
            "[tui]\nstatus_line=['model-name']\n",
            "[tui]\nanimations=false\n",
        ] {
            assert_eq!(
                launch_config_digest(&format!("{base}{tui}")).unwrap(),
                expected
            );
        }
        assert_ne!(
            launch_config_digest(&base.replace("gpt-5", "other-model")).unwrap(),
            expected
        );
        assert!(launch_config_digest(&format!("{base}unknown=true\n")).is_ok());
    }

    #[test]
    fn legacy_projection_marker_is_removed_from_native_config() {
        let projected = remove_projection_marker("cutex_projection_version=2\nmodel='gpt-5'\n")
            .unwrap()
            .expect("marker removed");
        assert!(!projected.contains(SHARED_PROJECTION_MARKER));
        let value: toml::Value = toml::from_str(&projected).unwrap();
        assert_eq!(value["model"].as_str(), Some("gpt-5"));
        assert_eq!(
            remove_projection_marker("model='gpt-5'\n").unwrap(),
            None,
            "marker-less config bytes must be preserved"
        );
        assert!(remove_projection_marker("cutex_projection_version=999\nmodel='gpt-5'\n").is_err());
    }

    #[test]
    fn ordinary_settings_do_not_require_migration_marker() {
        for raw in [
            "model='gpt-6-astra'\nmodel_reasoning_effort='xhigh'",
            "sandbox_mode='danger-full-access'\napprovals_reviewer='user'",
            "service_tier='priority'\nplan_mode_reasoning_effort='high'",
            "[tui]\nstatus_line=['model-name']",
            "[[skills.config]]\npath='/skills/example/SKILL.md'\nenabled=false",
            "[plugins.example]\nenabled=false",
        ] {
            validate_shared_config(raw).unwrap();
        }
    }

    #[test]
    fn stock_unknown_bundle_fields_and_versions_reject_without_spawn() {
        let json = serde_json::json!({"version":1,"native_id":"not-a-native-id","native_home":"/","bundle_manifest":"/","bundle_sha256":"a".repeat(64)});
        let contract: ExplicitLaunchContract = serde_json::from_value(json.clone()).unwrap();
        assert!(contract.validate().is_err());
        let mut changed = json;
        changed["fallback"] = serde_json::json!("codex");
        assert!(serde_json::from_value::<ExplicitLaunchContract>(changed).is_err());
    }
    #[test]
    fn stock_requirement_survives_runtime_clear_and_serialization() {
        let mut record = CutexSessionRecord::new(
            "cutex.stock".into(),
            Some(uuid::Uuid::new_v4().to_string()),
            "private".into(),
            "/private".into(),
            None,
        )
        .unwrap();
        record.explicit_launch = Some(ExplicitLaunchContract {
            version: 999,
            migration_action_id: None,
            native_id: record.codex_session_id.clone().unwrap(),
            native_home: "/private".into(),
            bundle_manifest: "/private/bundle".into(),
            bundle_sha256: Sha256::new("a".repeat(64)).unwrap(),
        });
        let contract = record.explicit_launch.clone();
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        crate::session::service::clear_cutex_session_runtime_record(
            &mut store,
            "cutex.stock",
            false,
        )
        .unwrap();
        let record = &store.sessions["cutex.stock"];
        assert_eq!(record.explicit_launch, contract);
        let decoded: CutexSessionRecord =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        assert_eq!(decoded.explicit_launch, contract);
        crate::session::service::set_cutex_session_profile_by_key(
            &mut store,
            "cutex.stock",
            Some("changed-profile".into()),
        )
        .unwrap();
        let record = store.sessions.get_mut("cutex.stock").unwrap();
        let revision = record.durable_revision();
        crate::session::archive::commit_retire(
            record,
            revision,
            0,
            true,
            "2026-09-09T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(record.explicit_launch, contract);
        let revision = record.durable_revision();
        crate::session::archive::commit_restore(record, revision, "2026-09-09T00:00:01Z".into())
            .unwrap();
        assert_eq!(record.explicit_launch, contract);
        assert_eq!(record.profile.as_deref(), Some("changed-profile"));
    }
}

#[cfg(test)]
mod native_plugin_config_tests {
    use super::*;
    const CONFIG: &str = r#"
[plugins."visualize@openai-bundled"]
enabled=true
[marketplaces.openai-bundled]
source_type="local"
source="/native/.tmp/bundled-marketplaces/openai-bundled"
"#;
    #[test]
    fn native_plugins_and_marketplaces_survive_adoption_config_and_remain_bound() {
        validate_shared_config(CONFIG).unwrap();
        let hash = launch_config_digest(CONFIG).unwrap();
        assert_ne!(
            hash,
            launch_config_digest(&CONFIG.replace("enabled=true", "enabled=false")).unwrap()
        );
        assert_ne!(
            hash,
            launch_config_digest(&CONFIG.replace("/native/", "/other/")).unwrap()
        );
        let rich = format!(
            "{CONFIG}last_updated='today'\nlast_revision='revision'\nref='main'\nsparse_paths=['plugins']\n[plugins.\"visualize@openai-bundled\".mcp_servers.visualize]\ndefault_tools_approval_mode='prompt'\nenabled_tools=['view']\ndisabled_tools=['delete']\n[plugins.\"visualize@openai-bundled\".mcp_servers.visualize.tools.view]\napproval_mode='auto'\noutput_token_limit=1000\n"
        );
        validate_shared_config(&rich).unwrap();
        // Native owns semantic validation of plugin options.
        assert!(
            validate_shared_config(&CONFIG.replace("source_type=\"local\"", "source_type=\"git\""))
                .is_ok()
        );
    }
    #[test]
    #[ignore = "explicit read-only live shared config probe"]
    fn actual_shared_config_is_accepted_after_projection_marker() {
        let path = std::env::var("CUTEX_SHARED_CONFIG_TEST_FILE").unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        validate_shared_config(&raw).unwrap();
        launch_config_digest(&raw).unwrap();
    }
    #[test]
    fn rejected_config_reports_field_without_echoing_source_document() {
        let error = validate_shared_config("private_unknown_key='SENSITIVE_SENTINEL")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid native configuration"));
        assert!(!error.contains("SENSITIVE_SENTINEL"));
        assert!(validate_shared_config(&format!("{CONFIG}unknown_option=true\n")).is_ok());
    }
}

#[cfg(test)]
mod permission_profile_tests {
    use super::*;

    fn configuration(sandbox: &str, network: bool, roots: &[&str]) -> StockConfiguration {
        StockConfiguration {
            selected_projection: None,
            private_chatgpt_auth: None,
            profile_name: "p".into(),
            profile_id: uuid::Uuid::nil().to_string(),
            inherited: false,
            profile_sha256: Sha256::new("a".repeat(64)).unwrap(),
            account_sha256: Sha256::new("b".repeat(64)).unwrap(),
            model: "m".into(),
            reasoning: None,
            model_provider: "openai".into(),
            provider: DummyProvider {
                name: "n".into(),
                base_url: "http://127.0.0.1:1/v1".into(),
                wire_api: "responses".into(),
                requires_openai_auth: false,
                supports_websockets: false,
            },
            sandbox: sandbox.into(),
            approval: "on-request".into(),
            approvals_reviewer: None,
            network_access: network,
            writable_roots: roots.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn builtin_profiles_without_extras_and_cutex_profile_with_them() {
        for (sandbox, expected) in [
            ("read-only", ":read-only"),
            ("workspace-write", ":workspace"),
            ("danger-full-access", ":danger-full-access"),
        ] {
            let plain = configuration(sandbox, false, &[]);
            assert_eq!(plain.permission_profile_id().unwrap(), expected);
            assert!(plain.permission_profile_definition().is_none());
            assert!(plain.legacy_workspace_write_table().is_none());
        }
        // Extras are ignored outside workspace-write.
        assert_eq!(configuration("danger-full-access", true, &["/x"]).permission_profile_id().unwrap(), ":danger-full-access");
        let extras = configuration("workspace-write", true, &["/extra/root"]);
        assert_eq!(extras.permission_profile_id().unwrap(), CUTEX_WORKSPACE_PERMISSION_PROFILE);
        assert_eq!(
            extras.permission_profile_definition().unwrap(),
            serde_json::json!({"extends": ":workspace", "network": {"enabled": true},
                "workspace_roots": {"/extra/root": true}})
        );
        assert_eq!(
            extras.legacy_workspace_write_table().unwrap(),
            serde_json::json!({"network_access": true, "writable_roots": ["/extra/root"]})
        );
        // The inline TOML used for `-c permissions.cutex-workspace=...` parses back.
        let value = toml::Value::try_from(extras.permission_profile_definition().unwrap()).unwrap();
        let reparsed: toml::Value = toml::from_str(&format!("p = {value}")).unwrap();
        assert_eq!(reparsed["p"]["workspace_roots"]["/extra/root"].as_bool(), Some(true));
        assert_eq!(reparsed["p"]["network"]["enabled"].as_bool(), Some(true));
        assert!(configuration("bogus", false, &[]).permission_profile_id().is_err());
    }

    #[test]
    fn extras_round_trip_and_older_receipts_still_parse() {
        let plain = configuration("danger-full-access", false, &[]);
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value.get("network_access").is_none() && value.get("writable_roots").is_none());
        assert_eq!(serde_json::from_value::<StockConfiguration>(value).unwrap(), plain);
        let extras = configuration("workspace-write", true, &["/r"]);
        let back: StockConfiguration = serde_json::from_value(serde_json::to_value(&extras).unwrap()).unwrap();
        assert_eq!(back, extras);
        let permissions = crate::config::new_thread::LaunchPermissions {
            sandbox: "workspace-write".into(), approval: "never".into(), approvals_reviewer: None,
            network_access: true, writable_roots: vec![std::env::temp_dir().display().to_string()],
        };
        let applied = plain.with_permissions(&permissions).unwrap();
        assert_eq!((applied.sandbox.as_str(), applied.approval.as_str()), ("workspace-write", "never"));
        assert!(applied.has_workspace_extras());
    }
}

#[cfg(test)]
mod inherited_model_catalog_tests {
    use super::drop_inherited_model_catalog;

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-model-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // An agent home copied from a shared home a Kimi profile was activated into.
    const AGENT: &str = "approvals_reviewer = \"user\"\nmodel = \"gpt-5.6-sol\"\nmodel_catalog_json = \"/profiles/kimi/models-1.json\"\nmodel_provider = \"kimi\"\n\n# keep me\n[model_providers.kimi]\nbase_url = \"https://example.invalid/v1\"\nname = \"Kimi\"\n\n[projects.\"/w\"]\ntrust_level = \"trusted\"\n";

    #[test]
    fn profile_without_catalog_drops_the_inherited_one() {
        let home = tempdir();
        std::fs::write(home.as_path().join("config.toml"), AGENT).unwrap();
        // ChatGPT-login launch: provider overridden, no catalog emitted.
        let args = vec!["-c".to_string(), "model_provider=\"openai\"".to_string()];
        assert!(drop_inherited_model_catalog(home.as_path(), &args).unwrap());
        let edited = std::fs::read_to_string(home.as_path().join("config.toml")).unwrap();
        assert_eq!(
            edited,
            AGENT.replace("model_catalog_json = \"/profiles/kimi/models-1.json\"\n", "")
        );
        // Idempotent.
        assert!(!drop_inherited_model_catalog(home.as_path(), &args).unwrap());
    }

    #[test]
    fn profile_catalog_or_nested_key_is_left_alone() {
        let home = tempdir();
        std::fs::write(home.as_path().join("config.toml"), AGENT).unwrap();
        let args = vec![
            "-c".to_string(),
            "model_catalog_json=\"/profiles/kimi/models-1.json\"".to_string(),
        ];
        assert!(!drop_inherited_model_catalog(home.as_path(), &args).unwrap());
        assert_eq!(std::fs::read_to_string(home.as_path().join("config.toml")).unwrap(), AGENT);

        let nested = "model = \"m\"\n[profiles.x]\nmodel_catalog_json = \"/c.json\"\n";
        std::fs::write(home.as_path().join("config.toml"), nested).unwrap();
        assert!(!drop_inherited_model_catalog(home.as_path(), &[]).unwrap());
        assert_eq!(std::fs::read_to_string(home.as_path().join("config.toml")).unwrap(), nested);
    }

    #[test]
    fn missing_config_is_fine() {
        let home = tempdir();
        assert!(!drop_inherited_model_catalog(home.as_path(), &[]).unwrap());
    }
}
