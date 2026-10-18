//! `check` and `stage` (RFC §4.2: check → download → verify → stage →
//! preflight). Neither changes what runs: staging only adds an immutable
//! `releases/<release>/` (and `natives/<version>/`) and records `staged`.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};

use super::layout::{Layout, CUTEX_EXE, FACADE_EXE};
use super::manifest::{self, ChannelPointer, ReleaseManifest};
use super::minisign::PublicKey;
use super::source::Source;
use super::state::{Available, Check, Installed, Staged, State};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const SIGNATURE_SUFFIX: &str = ".minisig";
pub const CHANNEL_FILE: &str = "channel.json";
/// Files a native archive must contain (`.exe` on Windows for binaries).
pub const NATIVE_APP_SERVER: &str = if cfg!(windows) { "codex-app-server.exe" } else { "codex-app-server" };
pub const NATIVE_CLI: &str = if cfg!(windows) { "codex.exe" } else { "codex" };
pub const NATIVE_CODE_MODE_HOST: &str = if cfg!(windows) { "codex-code-mode-host.exe" } else { "codex-code-mode-host" };
pub const NATIVE_SCHEMA: &str = "codex_app_server_protocol.schemas.json";
/// Free space wanted beyond the artifacts (backups, unpacking).
const SPARE_BYTES: u64 = 512 << 20;

/// Typed failure for the API (`code` = JSON error code).
#[derive(Debug)]
pub struct UpgradeError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: serde_json::Value,
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for UpgradeError {}

impl UpgradeError {
    pub fn boxed(status: u16, code: &'static str, message: impl Into<String>, details: serde_json::Value) -> anyhow::Error {
        anyhow::Error::new(Self { status, code, message: message.into(), details })
    }
}

pub fn upgrade_error(status: u16, code: &'static str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UpgradeError { status, code, message: message.into(), details: serde_json::json!({}) })
}

pub fn blocked(reason: &str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UpgradeError {
        status: 409,
        code: "upgrade_blocked",
        message: message.into(),
        details: serde_json::json!({ "reason": reason }),
    })
}

/// The runtime selection for new agents here (to tell whether a release
/// changes it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalRuntimes {
    pub native: Option<String>,
    pub claude: Option<String>,
}

impl LocalRuntimes {
    pub fn read() -> Self {
        let summary = crate::management::v2::runtimes_api::summary().unwrap_or_default();
        Self {
            native: summary.pointer("/native/selected").and_then(|v| v.as_str()).map(str::to_string),
            claude: summary.pointer("/claude/selected").and_then(|v| v.as_str()).map(str::to_string),
        }
    }
}

/// Fetch and verify a signed document.
fn fetch_signed(source: &Source, keys: &[PublicKey], tag: &str, name: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let bytes = source.fetch(tag, name, manifest::MAX_DOCUMENT_BYTES)?;
    let signature = source.fetch(tag, &format!("{name}{SIGNATURE_SUFFIX}"), 4096)?;
    let signature = String::from_utf8(signature).context("signature is not text")?;
    super::minisign::verify_text(keys, &bytes, &signature)
        .map_err(|error| upgrade_error(502, "signature_invalid", format!("{tag}/{name}: {error:#}")))?;
    Ok((bytes, signature))
}

fn require_keys(keys: &[PublicKey]) -> anyhow::Result<()> {
    if keys.is_empty() {
        return Err(upgrade_error(503, "no_trusted_keys", "this build trusts no release signing key (upgrade/keys.rs)"));
    }
    Ok(())
}

/// The channel head, verified.
pub fn fetch_pointer(source: &Source, keys: &[PublicKey], channel: &str) -> anyhow::Result<ChannelPointer> {
    require_keys(keys)?;
    let (bytes, _) = fetch_signed(source, keys, &format!("channel-{channel}"), CHANNEL_FILE)?;
    let pointer = manifest::parse_pointer(&bytes)?;
    pointer.validate(channel)?;
    Ok(pointer)
}

/// A release manifest, verified; `expected_sha256` from a channel pointer.
pub fn fetch_manifest(
    source: &Source,
    keys: &[PublicKey],
    channel: &str,
    tag: &str,
    expected_sha256: Option<&str>,
) -> anyhow::Result<(ReleaseManifest, Vec<u8>, String)> {
    require_keys(keys)?;
    let (bytes, signature) = fetch_signed(source, keys, tag, MANIFEST_FILE)?;
    if let Some(expected) = expected_sha256 {
        ensure!(manifest::sha256_hex(&bytes) == expected, "manifest does not match the channel pointer");
    }
    let parsed = ReleaseManifest::parse(&bytes)?;
    parsed.validate(channel)?;
    ensure!(parsed.tag() == tag, "manifest release {} does not belong to tag {tag}", parsed.cutex.release);
    Ok((parsed, bytes, signature))
}

/// What is installed: the active release and its manifest (if it came from a
/// signed release; a hand-installed dev build has sequence 0).
pub fn installed(layout: &Layout) -> Installed {
    let release = layout.active_release().unwrap_or_else(|_| "unknown".into());
    let path = layout.release_dir(&release).join(MANIFEST_FILE);
    match std::fs::read(&path).ok().and_then(|bytes| ReleaseManifest::parse(&bytes).ok().map(|m| (m, bytes))) {
        Some((m, bytes)) => Installed {
            release,
            sequence: m.sequence,
            commit: Some(m.cutex.commit),
            manifest_sha256: Some(manifest::sha256_hex(&bytes)),
            notes: Some(m.notes),
            published_at: m.published_at,
        },
        None => Installed {
            release,
            sequence: 0,
            commit: crate::build_info::COMMIT.map(str::to_string),
            manifest_sha256: None,
            notes: None,
            published_at: None,
        },
    }
}

/// `installed` with `notes` / `publishedAt` read from the release's
/// committed manifest (`releases/<release>/manifest.json`) when the record
/// lacks them (a `state.json` written before these fields). Only a manifest
/// whose sha256 matches the record's `manifestSha256` is used.
pub fn with_manifest_details(layout: &Layout, mut installed: Installed) -> Installed {
    if installed.notes.is_some() || installed.manifest_sha256.is_none() || !super::layout::valid_label(&installed.release) {
        return installed;
    }
    let path = layout.release_dir(&installed.release).join(MANIFEST_FILE);
    let Ok(bytes) = std::fs::read(&path) else { return installed };
    if installed.manifest_sha256.as_deref() != Some(manifest::sha256_hex(&bytes).as_str()) {
        return installed;
    }
    if let Ok(m) = ReleaseManifest::parse(&bytes) {
        installed.notes = Some(m.notes);
        installed.published_at = m.published_at;
    }
    installed
}

pub fn runtime_changes(manifest: &ReleaseManifest, local: &LocalRuntimes) -> bool {
    manifest.native.as_ref().is_some_and(|native| local.native.as_deref() != Some(native.version.as_str()))
}

pub fn available_from(manifest: &ReleaseManifest, manifest_sha256: &str, installed: &Installed, local: &LocalRuntimes) -> Available {
    let classification = manifest::classify(manifest, Some(&installed.release), runtime_changes(manifest, local));
    Available {
        release: manifest.cutex.release.clone(),
        sequence: manifest.sequence,
        tag: manifest.tag(),
        manifest_sha256: manifest_sha256.into(),
        published_at: manifest.published_at.clone(),
        notes: manifest.notes.clone(),
        class: classification.class,
        mode: classification.mode,
        migrations: classification.migrations,
        below_floor: classification.below_floor,
        native_version: manifest.native.as_ref().map(|n| n.version.clone()),
        // `available.claudeRuntime` (desktop Backend updates): the claude-runtime component
        // version, also for an older manifest's `claudeRuntime` part (mapped at parse).
        claude_runtime: manifest.claude_runtime_version().map(str::to_string),
        capabilities_removed: manifest.cutex.capabilities.removed.clone(),
        desktop_min_commit: manifest.desktop.min_commit.clone(),
    }
}

fn cache_dir(layout: &Layout, release: &str) -> PathBuf {
    layout.upgrade_dir().join("cache").join(release)
}

/// `check`: read the channel head; record `available` (None when up to
/// date) and `lastCheckAt` / `lastCheckError`.
pub fn check(layout: &Layout, source: &Source, keys: &[PublicKey], channel: &str, local: &LocalRuntimes) -> anyhow::Result<Option<Available>> {
    let installed = installed(layout);
    let result = (|| -> anyhow::Result<Option<Available>> {
        let pointer = fetch_pointer(source, keys, channel)?;
        if pointer.sequence <= installed.sequence || pointer.release == installed.release {
            return Ok(None);
        }
        let (parsed, bytes, signature) = fetch_manifest(source, keys, channel, &pointer.tag(), Some(&pointer.manifest_sha256))?;
        ensure!(parsed.sequence == pointer.sequence && parsed.cutex.release == pointer.release, "manifest and channel pointer disagree");
        let cache = cache_dir(layout, &parsed.cutex.release);
        std::fs::create_dir_all(&cache)?;
        std::fs::write(cache.join(MANIFEST_FILE), &bytes)?;
        std::fs::write(cache.join(format!("{MANIFEST_FILE}{SIGNATURE_SUFFIX}")), &signature)?;
        Ok(Some(available_from(&parsed, &pointer.manifest_sha256, &installed, local)))
    })();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    State::update(layout, |state| {
        state.installed = Some(installed.clone());
        state.last_check_at = Some(now);
        match &result {
            Ok(available) => {
                state.available = available.clone();
                state.last_check_error = None;
                if state.staged.as_ref().is_some_and(|s| s.sequence <= installed.sequence) {
                    state.staged = None;
                }
            }
            Err(error) => {
                let code = error.downcast_ref::<UpgradeError>().map_or("check_failed", |e| e.code);
                state.last_check_error = Some(super::state::RunError { code: code.into(), message: format!("{error:#}") });
            }
        }
    })?;
    result
}

/// The verified manifest of `release`: the check cache (verified again),
/// else fetched from its tag.
pub fn load_manifest(layout: &Layout, source: &Source, keys: &[PublicKey], channel: &str, release: &str) -> anyhow::Result<(ReleaseManifest, Vec<u8>, String)> {
    manifest::check_release_label(release)?;
    require_keys(keys)?;
    let cache = cache_dir(layout, release);
    if let (Ok(bytes), Ok(signature)) =
        (std::fs::read(cache.join(MANIFEST_FILE)), std::fs::read_to_string(cache.join(format!("{MANIFEST_FILE}{SIGNATURE_SUFFIX}"))))
    {
        if super::minisign::verify_text(keys, &bytes, &signature).is_ok() {
            let parsed = ReleaseManifest::parse(&bytes)?;
            parsed.validate(channel)?;
            ensure!(parsed.cutex.release == release, "cached manifest is for another release");
            return Ok((parsed, bytes, signature));
        }
    }
    fetch_manifest(source, keys, channel, &manifest::release_tag(release), None)
}

fn require_files(dir: &Path, files: &[&str], what: &str) -> anyhow::Result<()> {
    for file in files {
        ensure!(dir.join(file).is_file(), "{what} archive lacks {file}");
    }
    Ok(())
}

fn rename_into_place(from: &Path, to: &Path) -> anyhow::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(from, to).with_context(|| format!("move {} to {}", from.display(), to.display()))
}

/// Run `exe args…` with a timeout; stdout on success.
pub fn run_probe(exe: &Path, args: &[&str], timeout: std::time::Duration) -> anyhow::Result<String> {
    let spawn = || {
        std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
    };
    // A just-unpacked executable can be briefly "busy" (ETXTBSY) while a
    // child forked by another thread still holds the writer's descriptor.
    let mut attempt = 0;
    let mut child = loop {
        match spawn() {
            Err(error) if cfg!(unix) && error.raw_os_error() == Some(26) && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => break other.with_context(|| format!("run {}", exe.display()))?,
        }
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            ensure!(
                status.success(),
                "{} {} failed: {}",
                exe.display(),
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).chars().take(500).collect::<String>()
            );
            return Ok(stdout);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} {} timed out", exe.display(), args.join(" "));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Preflight of a staged release (nothing changes): the new binary's
/// identity, its read-only self test over the live stores, native files,
/// disk space, rollback target present.
pub fn preflight(layout: &Layout, keys: &[PublicKey], manifest: &ReleaseManifest, native_dir: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();
    let release = &manifest.cutex.release;
    // Never start a binary that is not the one unpacked from the signed archive.
    if let Err(error) = super::integrity::verify_executable(layout, keys, release) {
        checks.push(Check { name: "integrity".into(), ok: false, detail: Some(format!("{error:#}")) });
        return checks;
    }
    checks.push(Check { name: "integrity".into(), ok: true, detail: None });
    let exe = layout.release_executable(release);
    let probe = |args: &[&str]| run_probe(&exe, args, std::time::Duration::from_secs(60));
    checks.push(match probe(&["--version"]) {
        Ok(out) if out.contains(release.as_str()) && out.contains(&manifest.cutex.commit[..7]) => {
            Check { name: "version".into(), ok: true, detail: Some(out.trim().to_string()) }
        }
        Ok(out) => Check { name: "version".into(), ok: false, detail: Some(format!("expected {release} {}, got `{}`", &manifest.cutex.commit[..7], out.trim())) },
        Err(error) => Check { name: "version".into(), ok: false, detail: Some(format!("{error:#}")) },
    });
    checks.push(match probe(&["upgrade", "selftest", "--json"]) {
        Ok(out) => match serde_json::from_str::<serde_json::Value>(&out) {
            Ok(value) if value["ok"] == true => Check { name: "selftest".into(), ok: true, detail: None },
            Ok(value) => Check { name: "selftest".into(), ok: false, detail: Some(value["failures"].to_string()) },
            Err(_) => Check { name: "selftest".into(), ok: false, detail: Some(format!("unexpected output `{}`", out.trim())) },
        },
        Err(error) => Check { name: "selftest".into(), ok: false, detail: Some(format!("{error:#}")) },
    });
    if let Some(dir) = native_dir {
        let missing: Vec<&str> =
            [NATIVE_APP_SERVER, NATIVE_CODE_MODE_HOST, NATIVE_SCHEMA].into_iter().filter(|f| !dir.join(f).is_file()).collect();
        checks.push(Check {
            name: "native".into(),
            ok: missing.is_empty(),
            detail: (!missing.is_empty()).then(|| format!("missing {}", missing.join(", "))),
        });
    }
    let free = fs2::available_space(&layout.base).unwrap_or(u64::MAX);
    checks.push(Check { name: "disk".into(), ok: free >= SPARE_BYTES, detail: Some(format!("{} MiB free", free >> 20)) });
    let active = layout.active_release().ok();
    checks.push(Check {
        name: "rollbackTarget".into(),
        ok: active.as_ref().is_some_and(|a| layout.release_executable(a).is_file()),
        detail: active,
    });
    checks
}

/// `stage`: download, verify, unpack into `releases/<release>/` (and
/// `natives/<version>/`), preflight; records `staged`.
pub fn stage(
    layout: &Layout,
    source: &Source,
    keys: &[PublicKey],
    channel: &str,
    release: &str,
    local: &LocalRuntimes,
) -> anyhow::Result<Staged> {
    ensure!(layout.is_managed(), "not a managed install");
    let installed = installed(layout);
    let (parsed, bytes, signature) = load_manifest(layout, source, keys, channel, release)?;
    if parsed.sequence <= installed.sequence {
        return Err(upgrade_error(409, "downgrade_refused", format!(
            "release {release} (sequence {}) is not newer than the installed {} (sequence {})",
            parsed.sequence, installed.release, installed.sequence
        )));
    }
    let manifest_sha = manifest::sha256_hex(&bytes);
    let artifact = parsed.cutex_artifact()?.clone();
    let staging = layout.staging_dir().join(format!("{release}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::create_dir_all(&staging)?;
    let result = (|| -> anyhow::Result<Option<String>> {
        let free = fs2::available_space(&layout.base).unwrap_or(u64::MAX);
        let native_size = parsed.native_artifact().map_or(0, |a| a.size);
        let component_size = crate::components::stage_bytes(layout, &parsed);
        ensure!(
            free >= artifact.size.saturating_mul(4) + native_size.saturating_mul(4) + component_size.saturating_mul(4) + SPARE_BYTES,
            "not enough disk space under {} ({} MiB free)",
            layout.base.display(),
            free >> 20
        );
        // cutex release
        let target = layout.release_dir(release);
        let existing = target.join(MANIFEST_FILE).is_file();
        if existing {
            let existing = std::fs::read(target.join(MANIFEST_FILE))?;
            ensure!(manifest::sha256_hex(&existing) == manifest_sha, "releases/{release} exists with another manifest");
        } else {
            ensure!(!target.exists(), "releases/{release} exists but is incomplete; remove it first");
        }
        // The release file record (integrity.rs) binds the unpacked
        // executables to this signed manifest; a release dir without one
        // (staged by an older binary) is compared with the signed archive
        // again before it is recorded.
        let recorded = existing
            && super::integrity::read(layout, release).is_some_and(|r| {
                r.source == super::integrity::SOURCE_STAGE
                    && r.manifest_sha256.as_deref() == Some(manifest_sha.as_str())
                    && r.artifact_sha256.as_deref() == Some(artifact.sha256.as_str())
            });
        if !recorded {
            let archive = staging.join(&artifact.name);
            source.download(&artifact.tag.clone().unwrap_or_else(|| parsed.tag()), &artifact.name, &archive, artifact.size, &artifact.sha256)?;
            let unpacked = staging.join("release");
            super::archive::unpack_tar_gz_file(&archive, &unpacked)?;
            require_files(&unpacked, &[CUTEX_EXE, FACADE_EXE], "cutex")?;
            let hashes = super::integrity::file_hashes_in(&unpacked)?;
            if existing {
                ensure!(
                    super::integrity::file_hashes(layout, release)? == hashes,
                    "releases/{release} differs from the signed archive; move it away and stage again"
                );
            } else {
                std::fs::write(unpacked.join(MANIFEST_FILE), &bytes)?;
                std::fs::write(unpacked.join(format!("{MANIFEST_FILE}{SIGNATURE_SUFFIX}")), &signature)?;
                rename_into_place(&unpacked, &target)?;
            }
            super::integrity::record(layout, release, super::integrity::SOURCE_STAGE, hashes, Some(&bytes), Some(&artifact.sha256))?;
        }
        // native (only when this host does not have that version yet)
        let mut native_version = None;
        if let (Some(native), Some(artifact)) = (&parsed.native, parsed.native_artifact()) {
            let dir = layout.native_dir(&native.version);
            if !dir.join(NATIVE_APP_SERVER).is_file() {
                let archive = staging.join(&artifact.name);
                source.download(&artifact.tag.clone().unwrap_or_else(|| parsed.tag()), &artifact.name, &archive, artifact.size, &artifact.sha256)?;
                let unpacked = staging.join("native");
                super::archive::unpack_tar_gz_file(&archive, &unpacked)?;
                require_files(&unpacked, &[NATIVE_APP_SERVER, NATIVE_CODE_MODE_HOST, NATIVE_SCHEMA], "native")?;
                ensure!(!dir.exists(), "natives/{} exists but is incomplete; remove it first", native.version);
                rename_into_place(&unpacked, &dir)?;
            }
            native_version = Some(native.version.clone());
        }
        // New versions of installed components (selected at the swap; a
        // failure here is retried there and never fails the stage).
        for note in crate::components::stage_installed(layout, &parsed, Some(source)) {
            eprintln!("cutex upgrade stage: component {note}");
        }
        Ok(native_version)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let native_version = result?;
    let checks = preflight(layout, keys, &parsed, native_version.as_ref().map(|v| layout.native_dir(v)).as_deref());
    let staged = Staged {
        release: release.to_string(),
        sequence: parsed.sequence,
        staged_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        manifest_sha256: manifest_sha.clone(),
        preflight_ok: checks.iter().all(|c| c.ok),
        preflight: checks,
        native_version,
    };
    let available = available_from(&parsed, &manifest_sha, &installed, local);
    State::update(layout, |state| {
        state.installed = Some(installed.clone());
        state.staged = Some(staged.clone());
        if state.available.as_ref().is_none_or(|a| a.sequence <= available.sequence) {
            state.available = Some(available);
        }
    })?;
    Ok(staged)
}

/// The staged release's verified manifest (re-read from its release dir).
pub fn staged_manifest(layout: &Layout, keys: &[PublicKey], release: &str) -> anyhow::Result<ReleaseManifest> {
    manifest::check_release_label(release)?;
    let dir = layout.release_dir(release);
    let bytes = std::fs::read(dir.join(MANIFEST_FILE)).with_context(|| format!("release {release} is not staged"))?;
    let signature = std::fs::read_to_string(dir.join(format!("{MANIFEST_FILE}{SIGNATURE_SUFFIX}")))?;
    super::minisign::verify_text(keys, &bytes, &signature)?;
    ReleaseManifest::parse(&bytes)
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! A signed release published into a local source directory.
    use super::*;
    use crate::upgrade::archive::writer::{tar_gz, Item};
    use crate::upgrade::layout::InstallRecord;
    use crate::upgrade::minisign::test_key::TestKey;

    pub fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-upgrade-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake `cutex` executable (Unix shell script) answering `--version`
    /// and `upgrade selftest --json`.
    pub fn fake_cutex(release: &str, commit: &str, selftest_ok: bool) -> Vec<u8> {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"cutex 0.1.0 ({release}, {})\" ;;\n  upgrade) echo '{{\"ok\": {selftest_ok}, \"failures\": []}}' ;;\n  *) exit 0 ;;\nesac\n",
            &commit[..7]
        )
        .into_bytes()
    }

    pub struct Published {
        pub manifest: ReleaseManifest,
    }

    /// Publish `release` (cutex archive, optional native) under
    /// `<root>/cutex-<release>/` and point `channel-release` at it.
    pub fn publish(root: &Path, key: &TestKey, release: &str, sequence: u64, native: Option<&str>, tweak: impl FnOnce(&mut ReleaseManifest)) -> Published {
        let tag = manifest::release_tag(release);
        let dir = root.join(&tag);
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = manifest::fixtures::manifest(release, sequence);
        let exe = fake_cutex(release, &m.cutex.commit, true);
        // The license files the release builds put next to the executables (build-cutex.sh,
        // build-natives.sh): extra regular files at the archive root, unpacked and otherwise ignored.
        let archive = tar_gz(&[
            Item::File { path: CUTEX_EXE, data: &exe, mode: 0o755 },
            Item::File { path: FACADE_EXE, data: b"facade", mode: 0o755 },
            Item::File { path: "LICENSE", data: b"license", mode: 0o644 },
            Item::File { path: "NOTICE", data: b"notice", mode: 0o644 },
            Item::File { path: "THIRD_PARTY_NOTICES.md", data: b"# Third-party notices\n", mode: 0o644 },
        ]);
        std::fs::write(dir.join(&m.cutex_artifact().unwrap().name.clone()), &archive).unwrap();
        let artifact = m.cutex.artifacts.get_mut(&manifest::platform_key()).unwrap();
        artifact.sha256 = manifest::sha256_hex(&archive);
        artifact.size = archive.len() as u64;
        if let Some(version) = native {
            let name = format!("native-{version}-{}.tar.gz", manifest::platform_key());
            let archive = tar_gz(&[
                Item::File { path: NATIVE_APP_SERVER, data: b"app-server", mode: 0o755 },
                Item::File { path: NATIVE_CLI, data: b"cli", mode: 0o755 },
                Item::File { path: NATIVE_CODE_MODE_HOST, data: b"host", mode: 0o755 },
                Item::File { path: NATIVE_SCHEMA, data: b"{}", mode: 0o644 },
                Item::File { path: "LICENSE", data: b"license", mode: 0o644 },
                Item::File { path: "THIRD_PARTY_NOTICES.md", data: b"# Third-party notices\n", mode: 0o644 },
            ]);
            let native_tag = format!("native-{version}");
            std::fs::create_dir_all(root.join(&native_tag)).unwrap();
            std::fs::write(root.join(&native_tag).join(&name), &archive).unwrap();
            m.native = Some(manifest::NativePart {
                version: version.into(),
                commit: "a956835d020762cb2b570053af06f643a11c0ecc".into(),
                native_patch_commit: None,
                min_for_cutex: None,
                probes: Default::default(),
                artifacts: [(manifest::platform_key(), manifest::Artifact {
                    tag: Some(native_tag),
                    name,
                    sha256: manifest::sha256_hex(&archive),
                    size: archive.len() as u64,
                })]
                .into(),
            });
        }
        tweak(&mut m);
        let bytes = serde_json::to_vec_pretty(&m).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), &bytes).unwrap();
        std::fs::write(dir.join("manifest.json.minisig"), key.sign(&bytes, true, "file:manifest.json")).unwrap();
        let pointer = ChannelPointer {
            schema: manifest::CHANNEL_SCHEMA.into(),
            channel: "release".into(),
            sequence,
            release: release.into(),
            tag: None,
            manifest_sha256: manifest::sha256_hex(&bytes),
            published_at: None,
        };
        let channel = root.join("channel-release");
        std::fs::create_dir_all(&channel).unwrap();
        let bytes = serde_json::to_vec(&pointer).unwrap();
        std::fs::write(channel.join(CHANNEL_FILE), &bytes).unwrap();
        std::fs::write(channel.join("channel.json.minisig"), key.sign(&bytes, false, "file:channel.json")).unwrap();
        Published { manifest: m }
    }

    /// A managed install with `release` active (fake executables).
    pub fn installed_layout(release: &str, sequence: u64) -> Layout {
        let layout = Layout::at(temp("base"));
        let dir = layout.release_dir(release);
        std::fs::create_dir_all(&dir).unwrap();
        let m = manifest::fixtures::manifest(release, sequence);
        std::fs::write(dir.join(CUTEX_EXE), fake_cutex(release, &m.cutex.commit, true)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join(CUTEX_EXE), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join(FACADE_EXE), b"facade").unwrap();
        if sequence > 0 {
            std::fs::write(dir.join(MANIFEST_FILE), serde_json::to_vec(&m).unwrap()).unwrap();
        }
        #[cfg(unix)]
        crate::upgrade::layout::point_current(&layout, release).unwrap();
        let manifest_bytes = std::fs::read(dir.join(MANIFEST_FILE)).ok();
        crate::upgrade::integrity::record(
            &layout,
            release,
            crate::upgrade::integrity::SOURCE_SERVICE_INSTALL,
            crate::upgrade::integrity::file_hashes(&layout, release).unwrap(),
            manifest_bytes.as_deref(),
            None,
        )
        .unwrap();
        layout
            .write_install(&InstallRecord {
                schema: crate::upgrade::layout::INSTALL_SCHEMA.into(),
                os: std::env::consts::OS.into(),
                service_manager: "systemd".into(),
                channel: "release".into(),
                created_at: "2026-10-15T00:00:00Z".into(),
                active_release: release.into(),
                previous_release: None,
                unit_dir: None,
                task_name: None,
                extra: Default::default(),
            })
            .unwrap();
        layout
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::upgrade::manifest::{MigrationClass, Mode};
    use crate::upgrade::minisign::test_key::TestKey;

    #[test]
    fn installed_carries_the_manifest_notes() {
        let layout = installed_layout("r239", 40);
        let fresh = installed(&layout);
        assert_eq!(fresh.notes, Some(std::collections::BTreeMap::from([("en".to_string(), "r239 notes".to_string())])));
        assert_eq!(fresh.published_at.as_deref(), Some("2026-10-20T03:00:00Z"));
        // A record from before these fields gets them from the matching manifest.
        let old = Installed { notes: None, published_at: None, ..fresh.clone() };
        assert_eq!(with_manifest_details(&layout, old.clone()), fresh);
        // A manifest that does not match the record is not used.
        let other = Installed { manifest_sha256: Some("0".repeat(64)), ..old.clone() };
        assert_eq!(with_manifest_details(&layout, other.clone()), other);
        // The serialized record: null when unknown.
        let value = serde_json::to_value(&old).unwrap();
        assert!(value["notes"].is_null() && value["publishedAt"].is_null());
        std::fs::remove_dir_all(&layout.base).unwrap();
        // A release without a committed manifest (older layouts): unknown.
        let bare = installed_layout("r100", 0);
        let none = installed(&bare);
        assert_eq!((none.notes, none.published_at), (None, None));
        std::fs::remove_dir_all(&bare.base).unwrap();
    }

    #[test]
    fn check_finds_a_newer_signed_release_and_records_it() {
        let key = TestKey::new(1);
        let root = temp("source");
        let layout = installed_layout("r239", 40);
        publish(&root, &key, "r240", 41, None, |m| {
            m.cutex.migrations = vec![manifest::fixtures::migration("x-v1", "r240", MigrationClass::RollbackSafe)];
        });
        let source = Source::Dir { root: root.clone() };
        let local = LocalRuntimes::default();
        let available = check(&layout, &source, &[key.public()], "release", &local).unwrap().unwrap();
        assert_eq!((available.release.as_str(), available.sequence), ("r240", 41));
        assert_eq!(available.class, MigrationClass::RollbackSafe);
        assert_eq!(available.mode, Mode::Full);
        let state = State::load(&layout);
        assert_eq!(state.available.unwrap().release, "r240");
        assert_eq!(state.installed.unwrap().release, "r239");
        assert!(state.last_check_error.is_none());
        // Untrusted key: recorded as an error, available unchanged.
        let error = check(&layout, &source, &[TestKey::new(2).public()], "release", &local).unwrap_err();
        assert_eq!(error.downcast_ref::<UpgradeError>().unwrap().code, "signature_invalid");
        assert_eq!(State::load(&layout).last_check_error.unwrap().code, "signature_invalid");
        // No keys at all.
        let error = check(&layout, &source, &[], "release", &local).unwrap_err();
        assert_eq!(error.downcast_ref::<UpgradeError>().unwrap().code, "no_trusted_keys");
        // Up to date.
        let current = installed_layout("r240", 41);
        assert!(check(&current, &source, &[key.public()], "release", &local).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tampered_documents_are_refused() {
        let key = TestKey::new(1);
        let root = temp("source");
        let layout = installed_layout("r239", 40);
        publish(&root, &key, "r240", 41, None, |_| {});
        // The manifest changes after signing.
        let path = root.join("cutex-r240/manifest.json");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text = text.replace("\"sequence\": 41", "\"sequence\": 42");
        std::fs::write(&path, text).unwrap();
        let source = Source::Dir { root: root.clone() };
        let error = check(&layout, &source, &[key.public()], "release", &LocalRuntimes::default()).unwrap_err();
        assert!(format!("{error:#}").contains("signature"), "{error:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stage_unpacks_verifies_and_preflights_over_http() {
        let key = TestKey::new(1);
        let root = temp("source");
        let layout = installed_layout("r239", 40);
        publish(&root, &key, "r240", 41, Some("0.162.1-r53"), |_| {});
        let server = crate::upgrade::source::test_server::serve(root.clone());
        let source = Source::parse(&server.base).unwrap();
        let local = LocalRuntimes { native: Some("0.160.0-r51".into()), claude: None };
        check(&layout, &source, &[key.public()], "release", &local).unwrap().unwrap();
        let staged = stage(&layout, &source, &[key.public()], "release", "r240", &local).unwrap();
        assert!(staged.preflight_ok, "{:?}", staged.preflight);
        assert_eq!(staged.native_version.as_deref(), Some("0.162.1-r53"));
        assert!(layout.release_executable("r240").is_file());
        assert!(layout.release_dir("r240").join("manifest.json.minisig").is_file());
        assert!(layout.native_dir("0.162.1-r53").join(NATIVE_APP_SERVER).is_file());
        // License files of the archives land next to the executables.
        assert!(layout.release_dir("r240").join("THIRD_PARTY_NOTICES.md").is_file());
        assert!(layout.native_dir("0.162.1-r53").join("LICENSE").is_file());
        assert_eq!(layout.active_release().unwrap(), "r239", "staging never switches");
        assert_eq!(State::load(&layout).staged.unwrap().release, "r240");
        assert_eq!(staged_manifest(&layout, &[key.public()], "r240").unwrap().sequence, 41);
        // Staging again reuses the release directory.
        stage(&layout, &source, &[key.public()], "release", "r240", &local).unwrap();
        // Older or equal releases are refused.
        let current = installed_layout("r240", 41);
        let error = stage(&current, &source, &[key.public()], "release", "r240", &local).unwrap_err();
        assert_eq!(error.downcast_ref::<UpgradeError>().unwrap().code, "downgrade_refused");
        assert!(std::fs::read_dir(layout.staging_dir()).unwrap().next().is_none(), "staging cleaned");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_executables_are_recorded_and_checked_before_any_probe() {
        let key = TestKey::new(1);
        let root = temp("source");
        let layout = installed_layout("r239", 40);
        publish(&root, &key, "r240", 41, None, |_| {});
        let source = Source::Dir { root: root.clone() };
        let keys = [key.public()];
        let local = LocalRuntimes::default();
        let staged = stage(&layout, &source, &keys, "release", "r240", &local).unwrap();
        assert!(staged.preflight_ok, "{:?}", staged.preflight);
        let record = crate::upgrade::integrity::read(&layout, "r240").unwrap();
        assert_eq!(record.source, crate::upgrade::integrity::SOURCE_STAGE);
        crate::upgrade::integrity::verify_executable(&layout, &keys, "r240").unwrap();
        // Untrusted key: the signature binding fails.
        assert!(crate::upgrade::integrity::verify_executable(&layout, &[TestKey::new(9).public()], "r240").is_err());
        // A record lost (older stager): staging again re-checks the dir
        // against the signed archive and records it.
        std::fs::remove_file(crate::upgrade::integrity::record_path(&layout, "r240")).unwrap();
        assert!(stage(&layout, &source, &keys, "release", "r240", &local).unwrap().preflight_ok);
        assert!(crate::upgrade::integrity::read(&layout, "r240").is_some());
        // The executable replaced after staging: preflight refuses before probing.
        let marker = layout.base.join("probed");
        let exe = layout.release_executable("r240");
        std::fs::write(&exe, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        let parsed = staged_manifest(&layout, &keys, "r240").unwrap();
        let checks = preflight(&layout, &keys, &parsed, None);
        assert_eq!(checks.len(), 1);
        assert_eq!((checks[0].name.as_str(), checks[0].ok), ("integrity", false));
        assert!(!marker.exists(), "the replaced binary never ran");
        // Without a record, a dir that differs from the archive is refused.
        std::fs::remove_file(crate::upgrade::integrity::record_path(&layout, "r240")).unwrap();
        let error = stage(&layout, &source, &keys, "release", "r240", &local).unwrap_err();
        assert!(format!("{error:#}").contains("differs from the signed archive"), "{error:#}");
        assert!(crate::upgrade::integrity::read(&layout, "r240").is_none());
        std::fs::remove_dir_all(&layout.base).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stage_refuses_a_corrupt_artifact() {
        let key = TestKey::new(1);
        let root = temp("source");
        let layout = installed_layout("r239", 40);
        let published = publish(&root, &key, "r240", 41, None, |_| {});
        let name = published.manifest.cutex_artifact().unwrap().name.clone();
        let path = root.join("cutex-r240").join(&name);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
        let source = Source::Dir { root: root.clone() };
        let error = stage(&layout, &source, &[key.public()], "release", "r240", &LocalRuntimes::default()).unwrap_err();
        assert!(format!("{error:#}").contains("sha256"), "{error:#}");
        assert!(!layout.release_dir("r240").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
