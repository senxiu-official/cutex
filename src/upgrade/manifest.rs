//! Signed channel pointer and release manifest (RFC §3.2, §6).
//!
//! Both documents are JSON, each with a detached minisign signature
//! (`<file>.minisig`) by a trusted release key ([`super::keys`]). Readers
//! ignore unknown fields (they are signed; a newer publisher may add some).
//!
//! ```text
//! <source>/channel-<channel>/channel.json   ChannelPointer
//! <source>/<tag>/manifest.json              ReleaseManifest (tag = cutex-<release>)
//! <source>/<tag>/<asset>                    artifacts (sha256 + size in the manifest)
//! ```

use std::cmp::Ordering;
use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::layout::{ensure_label, valid_label};

pub const CHANNEL_SCHEMA: &str = "cutex/channel/v1";
pub const MANIFEST_SCHEMA: &str = "cutex/release-manifest/v1";
pub const MAX_DOCUMENT_BYTES: usize = 1 << 20;
/// `components.<name>` of the Claude runtime (was the `claudeRuntime` part).
pub const CLAUDE_RUNTIME_COMPONENT: &str = "claude-runtime";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPointer {
    pub schema: String,
    pub channel: String,
    pub sequence: u64,
    pub release: String,
    /// Release tag holding the manifest; default `cutex-<release>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub manifest_sha256: String,
    #[serde(default)]
    pub published_at: Option<String>,
}

impl ChannelPointer {
    pub fn tag(&self) -> String {
        self.tag.clone().unwrap_or_else(|| release_tag(&self.release))
    }

    pub fn validate(&self, channel: &str) -> anyhow::Result<()> {
        ensure!(self.schema == CHANNEL_SCHEMA, "unsupported channel pointer schema `{}`", self.schema);
        ensure!(
            crate::build_info::normalize_channel(&self.channel) == Some(channel),
            "channel pointer is for channel `{}`, expected `{channel}`",
            self.channel
        );
        ensure_label(&self.release, "release")?;
        ensure_label(&self.tag(), "release tag")?;
        ensure!(is_sha256(&self.manifest_sha256), "channel pointer manifestSha256 malformed");
        Ok(())
    }
}

pub fn release_tag(release: &str) -> String {
    format!("cutex-{release}")
}

/// One downloadable file: `<source>/<tag>/<name>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// Release tag; default: the tag of the manifest that names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

impl Artifact {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure_label(&self.name, "artifact name")?;
        if let Some(tag) = &self.tag {
            ensure_label(tag, "artifact tag")?;
        }
        ensure!(is_sha256(&self.sha256), "artifact {} sha256 malformed", self.name);
        ensure!(self.size > 0 && self.size <= 4 << 30, "artifact {} size out of range", self.name);
        ensure!(self.name.ends_with(".tar.gz"), "artifact {} must be a .tar.gz", self.name);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationClass {
    /// No store format change, no one-time migration, no selection change.
    ReloadSafe,
    /// New store / fields; the new binary can prepare a lossless rollback.
    RollbackSafe,
    /// The old binary cannot read the migrated state, or rollback loses data.
    OneWay,
}

impl MigrationClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReloadSafe => "reload-safe",
            Self::RollbackSafe => "rollback-safe",
            Self::OneWay => "one-way",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Migration {
    pub id: String,
    pub class: MigrationClass,
    /// First release that runs it.
    pub since: String,
    #[serde(default)]
    pub notes: BTreeMap<String, String>,
    /// Stores this migration creates; a binary rollback moves them aside
    /// (`upgrade/runs/<id>/displaced/`, never deleted). Paths relative to the
    /// cutex home.
    #[serde(default)]
    pub new_stores: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityChanges {
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CutexPart {
    pub release: String,
    pub commit: String,
    #[serde(default)]
    pub capabilities: CapabilityChanges,
    /// Cumulative list; the client takes (installed, target].
    #[serde(default)]
    pub migrations: Vec<Migration>,
    /// Lowest release that may upgrade straight to this one.
    #[serde(default)]
    pub upgrade_floor: Option<String>,
    /// Platform key (`linux-x64`, `windows-x64`, `darwin-arm64`) -> archive with
    /// `cutex[.exe]` and `cutex-mcp[.exe]` at its root.
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativePart {
    /// `0.162.1-r53`.
    pub version: String,
    pub commit: String,
    #[serde(default)]
    pub native_patch_commit: Option<String>,
    #[serde(default)]
    pub min_for_cutex: Option<String>,
    #[serde(default)]
    pub probes: BTreeMap<String, Value>,
    /// Archive with `codex-app-server[.exe]`, `codex-code-mode-host[.exe]`,
    /// `codex_app_server_protocol.schemas.json` and optionally `codex[.exe]`.
    pub artifacts: BTreeMap<String, Artifact>,
}

/// The legacy `claudeRuntime` part (manifests before components). Since
/// 0.1.1 the generator writes `components.claude-runtime` instead and omits
/// this; [`ReleaseManifest::parse`] maps an old one onto that component, so
/// everything reads the Claude runtime from `components` only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRuntimePart {
    /// `claude-r12`.
    pub name: String,
    #[serde(default)]
    pub adapter_version: Option<String>,
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
}

/// An optional component shipped as its own archive in the same release
/// (`components.<name>`, e.g. `job-service`): installed only on hosts that
/// ask for it, refreshed by backend updates only where installed. Older
/// manifests have none; older readers ignore the section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentPart {
    pub version: String,
    /// Platform key -> archive.
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopPart {
    #[serde(default)]
    pub min_commit: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Only the daemons restart; native processes live on.
    Reload,
    /// Runtimes go offline, selection changes, package run.
    Full,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reload => "reload",
            Self::Full => "full",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseManifest {
    pub schema: String,
    pub channel: String,
    pub sequence: u64,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub notes: BTreeMap<String, String>,
    pub cutex: CutexPart,
    #[serde(default)]
    pub native: Option<NativePart>,
    #[serde(default)]
    pub claude_runtime: Option<ClaudeRuntimePart>,
    /// Optional components by name (`job-service`, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub components: BTreeMap<String, ComponentPart>,
    #[serde(default)]
    pub desktop: DesktopPart,
    pub mode: Mode,
    #[serde(default)]
    pub package_run_recommended: bool,
}

pub fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `linux-x64`, `windows-x64`, `darwin-arm64`, `linux-arm64`, … for this
/// binary. macOS is `darwin` (the release artifact names of cutex and
/// cute-codex), not Rust's `macos`.
pub fn platform_key() -> String {
    platform_key_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn platform_key_for(os: &str, arch: &str) -> String {
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let os = match os {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{arch}")
}

impl ReleaseManifest {
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(bytes.len() <= MAX_DOCUMENT_BYTES, "release manifest too large");
        let mut manifest: Self = serde_json::from_slice(bytes).context("invalid release manifest")?;
        manifest.map_legacy_claude_runtime();
        Ok(manifest)
    }

    /// An old `claudeRuntime` part is the `claude-runtime` component (one
    /// source of truth). `components.claude-runtime` wins when both exist.
    fn map_legacy_claude_runtime(&mut self) {
        if let Some(legacy) = &self.claude_runtime {
            self.components.entry(CLAUDE_RUNTIME_COMPONENT.to_string()).or_insert_with(|| ComponentPart {
                version: legacy.name.clone(),
                artifacts: legacy.artifacts.clone(),
            });
        }
    }

    /// The Claude runtime version this release names (`claude-rN`).
    pub fn claude_runtime_version(&self) -> Option<&str> {
        self.components.get(CLAUDE_RUNTIME_COMPONENT).map(|c| c.version.as_str())
    }

    pub fn tag(&self) -> String {
        release_tag(&self.cutex.release)
    }

    pub fn validate(&self, channel: &str) -> anyhow::Result<()> {
        ensure!(self.schema == MANIFEST_SCHEMA, "unsupported release manifest schema `{}`", self.schema);
        ensure!(
            crate::build_info::normalize_channel(&self.channel) == Some(channel),
            "release manifest is for channel `{}`, expected `{channel}`",
            self.channel
        );
        ensure_label(&self.cutex.release, "release")?;
        ensure!(
            self.cutex.commit.len() >= 7 && self.cutex.commit.bytes().all(|b| b.is_ascii_hexdigit()),
            "release commit malformed"
        );
        if let Some(floor) = &self.cutex.upgrade_floor {
            ensure_label(floor, "upgrade floor")?;
        }
        for migration in &self.cutex.migrations {
            ensure_label(&migration.id, "migration id")?;
            ensure_label(&migration.since, "migration release")?;
            for store in &migration.new_stores {
                ensure!(safe_relative(store), "migration {} new store `{store}` is not a safe relative path", migration.id);
            }
        }
        for artifact in self.cutex.artifacts.values() {
            artifact.validate()?;
        }
        if let Some(native) = &self.native {
            ensure_label(&native.version, "native version")?;
            for artifact in native.artifacts.values() {
                artifact.validate()?;
            }
        }
        if let Some(claude) = &self.claude_runtime {
            ensure_label(&claude.name, "Claude runtime")?;
            for artifact in claude.artifacts.values() {
                artifact.validate()?;
            }
        }
        for (name, component) in &self.components {
            ensure_label(name, "component name")?;
            ensure_label(&component.version, "component version")?;
            for artifact in component.artifacts.values() {
                artifact.validate()?;
            }
        }
        Ok(())
    }

    /// Component `name`'s archive for this platform.
    pub fn component_artifact(&self, name: &str) -> Option<(&ComponentPart, &Artifact)> {
        let component = self.components.get(name)?;
        Some((component, component.artifacts.get(&platform_key())?))
    }

    /// The cutex archive for this platform.
    pub fn cutex_artifact(&self) -> anyhow::Result<&Artifact> {
        let key = platform_key();
        self.cutex.artifacts.get(&key).with_context(|| format!("release {} has no {key} build", self.cutex.release))
    }

    pub fn native_artifact(&self) -> Option<&Artifact> {
        self.native.as_ref().and_then(|native| native.artifacts.get(&platform_key()))
    }
}

/// `runtime/users`: relative, no `..`, no root, no backslash.
pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Compare release labels by their numbers: `r99 < r233 < r233h1 < r240`.
pub fn compare_releases(a: &str, b: &str) -> Ordering {
    natural_key(a).cmp(&natural_key(b))
}

fn natural_key(value: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut digits = String::new();
    let mut text = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            if !text.is_empty() {
                out.push((0, std::mem::take(&mut text)));
            }
            digits.push(ch);
        } else {
            if !digits.is_empty() {
                out.push((digits.parse().unwrap_or(u64::MAX), String::new()));
                digits.clear();
            }
            text.push(ch);
        }
    }
    if !digits.is_empty() {
        out.push((digits.parse().unwrap_or(u64::MAX), String::new()));
    }
    if !text.is_empty() {
        out.push((0, text));
    }
    out
}

/// What an upgrade from `installed` to the manifest means (RFC §6.1).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    /// Strictest class over (installed, target]; `reload-safe` without migrations.
    pub class: MigrationClass,
    /// Migrations in (installed, target], oldest first.
    pub migrations: Vec<Migration>,
    /// The publisher's mode, raised to `full` when the native / Claude
    /// runtime for new agents changes here.
    pub mode: Mode,
    /// Set when `installed` is below the manifest's `upgradeFloor`.
    pub below_floor: Option<String>,
}

/// `installed` = release label running now (None: unknown / dev, no
/// migrations are skipped: every listed migration applies).
pub fn classify(manifest: &ReleaseManifest, installed: Option<&str>, runtime_changes: bool) -> Classification {
    let target = manifest.cutex.release.as_str();
    let mut migrations: Vec<Migration> = manifest
        .cutex
        .migrations
        .iter()
        .filter(|m| compare_releases(&m.since, target) != Ordering::Greater)
        .filter(|m| installed.is_none_or(|installed| compare_releases(&m.since, installed) == Ordering::Greater))
        .cloned()
        .collect();
    migrations.sort_by(|a, b| compare_releases(&a.since, &b.since).then_with(|| a.id.cmp(&b.id)));
    let class = migrations.iter().map(|m| m.class).max().unwrap_or(MigrationClass::ReloadSafe);
    let below_floor = match (&manifest.cutex.upgrade_floor, installed) {
        (Some(floor), Some(installed)) if compare_releases(installed, floor) == Ordering::Less => Some(floor.clone()),
        _ => None,
    };
    let mode = if runtime_changes || class != MigrationClass::ReloadSafe { Mode::Full } else { manifest.mode };
    Classification { class, migrations, mode: mode.max(manifest.mode), below_floor }
}

/// Parse a channel pointer document.
pub fn parse_pointer(bytes: &[u8]) -> anyhow::Result<ChannelPointer> {
    ensure!(bytes.len() <= MAX_DOCUMENT_BYTES, "channel pointer too large");
    serde_json::from_slice(bytes).context("invalid channel pointer")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn file_sha256_hex(path: &std::path::Path) -> anyhow::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Labels from untrusted places (API, settings) before they become paths.
pub fn check_release_label(release: &str) -> anyhow::Result<()> {
    if !valid_label(release) {
        bail!("invalid release `{release}`");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub fn manifest(release: &str, sequence: u64) -> ReleaseManifest {
        let artifact = Artifact {
            tag: None,
            name: format!("cutex-{release}-{}.tar.gz", platform_key()),
            sha256: "0".repeat(64),
            size: 1,
        };
        ReleaseManifest {
            schema: MANIFEST_SCHEMA.into(),
            channel: "release".into(),
            sequence,
            published_at: Some("2026-10-20T03:00:00Z".into()),
            notes: BTreeMap::from([("en".into(), format!("{release} notes"))]),
            cutex: CutexPart {
                release: release.into(),
                commit: "5f30a7cad4dcd8435e797592a380349df509e35d".into(),
                capabilities: CapabilityChanges::default(),
                migrations: Vec::new(),
                upgrade_floor: None,
                artifacts: BTreeMap::from([(platform_key(), artifact)]),
            },
            native: None,
            claude_runtime: None,
            components: BTreeMap::new(),
            desktop: DesktopPart::default(),
            mode: Mode::Reload,
            package_run_recommended: false,
        }
    }

    pub fn migration(id: &str, since: &str, class: MigrationClass) -> Migration {
        Migration { id: id.into(), class, since: since.into(), notes: BTreeMap::new(), new_stores: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn platform_keys_name_macos_darwin() {
        assert_eq!(platform_key_for("linux", "x86_64"), "linux-x64");
        assert_eq!(platform_key_for("windows", "x86_64"), "windows-x64");
        assert_eq!(platform_key_for("macos", "aarch64"), "darwin-arm64");
        assert_eq!(platform_key_for("macos", "x86_64"), "darwin-x64");
        assert_eq!(platform_key_for("linux", "aarch64"), "linux-arm64");
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(platform_key(), "darwin-arm64");
        }
    }

    #[test]
    fn release_ordering_is_numeric() {
        assert_eq!(compare_releases("r99", "r233"), Ordering::Less);
        assert_eq!(compare_releases("r233", "r233h1"), Ordering::Less);
        assert_eq!(compare_releases("r233h1", "r240"), Ordering::Less);
        assert_eq!(compare_releases("r240", "r240"), Ordering::Equal);
    }

    #[test]
    fn classification_takes_the_strictest_migration_in_the_interval() {
        let mut m = manifest("r240", 41);
        m.cutex.migrations = vec![
            migration("users-v1", "r233", MigrationClass::OneWay),
            migration("workflows-v1", "r235", MigrationClass::RollbackSafe),
            migration("future", "r241", MigrationClass::OneWay),
        ];
        let from_r233 = classify(&m, Some("r233"), false);
        assert_eq!(from_r233.class, MigrationClass::RollbackSafe);
        assert_eq!(from_r233.migrations.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["workflows-v1"]);
        assert_eq!(from_r233.mode, Mode::Full, "a migration needs a full upgrade");
        let from_r230 = classify(&m, Some("r230"), false);
        assert_eq!(from_r230.class, MigrationClass::OneWay);
        assert_eq!(from_r230.migrations.len(), 2);
        let from_r235 = classify(&m, Some("r235"), false);
        assert_eq!(from_r235.class, MigrationClass::ReloadSafe);
        assert_eq!(from_r235.mode, Mode::Reload);
        assert_eq!(classify(&m, Some("r235"), true).mode, Mode::Full, "a runtime change raises reload to full");
        assert_eq!(classify(&m, None, false).class, MigrationClass::OneWay);
    }

    #[test]
    fn floor_is_reported() {
        let mut m = manifest("r240", 41);
        m.cutex.upgrade_floor = Some("r233".into());
        assert_eq!(classify(&m, Some("r230"), false).below_floor.as_deref(), Some("r233"));
        assert_eq!(classify(&m, Some("r233"), false).below_floor, None);
        assert_eq!(classify(&m, None, false).below_floor, None);
    }

    #[test]
    fn validation() {
        let mut m = manifest("r240", 41);
        m.validate("release").unwrap();
        assert!(m.validate("main").is_err());
        m.channel = "stable".into();
        m.validate("release").unwrap();
        m.cutex.artifacts.values_mut().next().unwrap().name = "../x.tar.gz".into();
        assert!(m.validate("release").is_err());
        let mut m = manifest("r240", 41);
        m.cutex.migrations = vec![Migration { new_stores: vec!["../outside".into()], ..migration("x", "r240", MigrationClass::RollbackSafe) }];
        assert!(m.validate("release").is_err());
        let mut m = manifest("../r", 1);
        assert!(m.validate("release").is_err());
        m = manifest("r240", 1);
        m.schema = "cutex/release-manifest/v2".into();
        assert!(m.validate("release").is_err());
        for (path, ok) in [("runtime/users", true), ("a", true), ("/abs", false), ("a/../b", false), ("a\\b", false), ("c:x", false), ("", false)] {
            assert_eq!(safe_relative(path), ok, "{path}");
        }
    }

    #[test]
    fn components_are_optional_and_validated() {
        // A 0.1.0 manifest (no `components`) stays valid and round-trips without one.
        let plain = manifest("0.1.0", 1);
        let text = serde_json::to_string(&plain).unwrap();
        assert!(!text.contains("components"));
        assert!(ReleaseManifest::parse(text.as_bytes()).unwrap().components.is_empty());
        let mut m = manifest("0.1.1", 2);
        let raw = serde_json::json!({"job-service": {"version": "0.1.1", "artifacts": {platform_key(): {
            "name": format!("cutex-job-service-0.1.1-{}.tar.gz", platform_key()), "sha256": "a".repeat(64), "size": 10}}}});
        m.components = serde_json::from_value(raw).unwrap();
        m.validate("release").unwrap();
        let (component, artifact) = m.component_artifact("job-service").unwrap();
        assert_eq!(component.version, "0.1.1");
        assert!(artifact.name.starts_with("cutex-job-service-"));
        assert!(m.component_artifact("claude-runtime").is_none());
        m.components.get_mut("job-service").unwrap().version = "../x".into();
        assert!(m.validate("release").is_err());
    }

    #[test]
    fn claude_runtime_is_the_component_and_an_old_part_maps_onto_it() {
        let artifact = |name: &str| serde_json::json!({"name": name, "sha256": "b".repeat(64), "size": 7});
        // 0.1.1+: only `components.claude-runtime`, no `claudeRuntime` (a 0.1.0 reader sees the
        // Option as absent and does not validate anything of it).
        let mut value = serde_json::to_value(manifest("0.1.1", 2)).unwrap();
        assert!(value.get("claudeRuntime").is_none() || value["claudeRuntime"].is_null());
        value.as_object_mut().unwrap().remove("claudeRuntime");
        value["components"] = serde_json::json!({"claude-runtime": {"version": "claude-r13", "artifacts": {
            platform_key(): artifact(&format!("component-claude-runtime-claude-r13-{}.tar.gz", platform_key()))}}});
        let m = ReleaseManifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        m.validate("release").unwrap();
        assert!(m.claude_runtime.is_none());
        assert_eq!(m.claude_runtime_version(), Some("claude-r13"));
        assert!(m.component_artifact(CLAUDE_RUNTIME_COMPONENT).is_some());

        // An older manifest's `claudeRuntime` is read as the same component.
        let mut old = serde_json::to_value(manifest("r240", 41)).unwrap();
        old["claudeRuntime"] = serde_json::json!({"name": "claude-r12", "adapterVersion": "0.7.1",
            "artifacts": {platform_key(): artifact("claude-r12.tar.gz")}});
        let m = ReleaseManifest::parse(&serde_json::to_vec(&old).unwrap()).unwrap();
        m.validate("release").unwrap();
        assert_eq!(m.claude_runtime_version(), Some("claude-r12"));
        assert_eq!(m.component_artifact(CLAUDE_RUNTIME_COMPONENT).unwrap().1.name, "claude-r12.tar.gz");

        // Both: the component wins.
        old["components"] = value["components"].clone();
        let m = ReleaseManifest::parse(&serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(m.claude_runtime_version(), Some("claude-r13"));

        // A manifest with neither names no Claude runtime.
        let m = ReleaseManifest::parse(&serde_json::to_vec(&manifest("r240", 41)).unwrap()).unwrap();
        assert_eq!(m.claude_runtime_version(), None);
    }

    #[test]
    fn manifest_round_trip_ignores_unknown_fields() {
        let m = manifest("r240", 41);
        let mut value = serde_json::to_value(&m).unwrap();
        value["futureTopLevel"] = serde_json::json!(true);
        value["cutex"]["futureCutex"] = serde_json::json!(1);
        let parsed = ReleaseManifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed, m);
        let text = serde_json::to_string(&m).unwrap();
        assert!(text.contains("\"mode\":\"reload\"") && text.contains("\"sequence\":41"));
    }
}
