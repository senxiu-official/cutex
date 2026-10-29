//! Stable Responses prompt cache key for native agent launches (design 034).
//!
//! Native Responses requests default their `prompt_cache_key` to the thread's
//! session id, and the backend derives prompt-cache affinity from the
//! session-id header, so a fresh agent thread usually misses the prompt cache
//! on its first request. When the host setting is enabled we pin the launch
//! to one stable key: `cutex-<host>-<first 16 hex of sha256(workspace cwd)>`.
//! Agents in the same workspace then share cache routing; different
//! workspaces stay on different keys (one eviction domain each).
//!
//! The native side is opt-in too, mirroring thread retention: only pass the
//! key after probing this exact (possibly pinned) app-server. Older natives
//! ignore unknown `-c` keys without `--strict-config`; the probe keeps the
//! opt-in explicit instead of relying on that. A token-saving option never
//! keeps an agent from starting: without support, or when the probe itself
//! fails, the launch goes on without the key (as with the setting off); the
//! outcome is `promptCacheKeyStatus` in `/v2/settings`.
use super::{
    command::LaunchCommand,
    stock::{StockBundle, VerifiedFile},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

#[derive(Clone, PartialEq, Eq)]
struct Key {
    sha256: String,
    len: u64,
    modified: Option<SystemTime>,
}
/// What the last launch's probe found (`promptCacheKeyStatus` in
/// `/v2/settings`, besides `off` / `pending`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The native advertises the key: launches carry it.
    Active,
    /// The native does not know the key (older native, or not the direct
    /// app-server): launches go without it.
    Unsupported,
    /// The probe itself failed (timeout, crash, unreadable binary): launches
    /// go without the key; retried after [`PROBE_RETRY`].
    ProbeFailed,
}

/// A failed probe is tried again on a launch after this long.
const PROBE_RETRY: Duration = Duration::from_secs(600);

static CACHE: Mutex<Option<HashMap<PathBuf, (Key, Status, std::time::Instant)>>> = Mutex::new(None);
/// Native hashes whose failed probe was already logged (once per hash).
static WARNED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

fn key(file: &VerifiedFile) -> anyhow::Result<Key> {
    let metadata = std::fs::metadata(&file.path)?;
    anyhow::ensure!(metadata.is_file(), "native executable is not a file");
    Ok(Key {
        sha256: file.sha256.as_str().to_owned(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn advertised(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("promptCacheKey").and_then(serde_json::Value::as_u64))
        == Some(1)
}

/// Probe (or reuse the cached answer for) this exact native. Never an
/// error: a token-saving option must not keep an agent from starting, so a
/// failed probe is `ProbeFailed` and the launch goes on without the key.
fn probe(file: &VerifiedFile, home: &Path) -> Status {
    let before = match key(file) {
        Ok(before) => before,
        Err(error) => return failed(file, &format!("{error:#}")),
    };
    let cached = CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.as_ref().and_then(|cache| cache.get(&file.path).cloned()));
    if let Some((cached, status, at)) = cached {
        if cached == before && (status != Status::ProbeFailed || at.elapsed() < PROBE_RETRY) {
            return status;
        }
    }
    let status = probe_once(file, home, &before).unwrap_or_else(|error| failed(file, &format!("{error:#}")));
    if let Ok(mut cache) = CACHE.lock() {
        cache
            .get_or_insert_with(HashMap::new)
            .insert(file.path.clone(), (before, status, std::time::Instant::now()));
    }
    status
}

fn probe_once(file: &VerifiedFile, home: &Path, before: &Key) -> anyhow::Result<Status> {
    file.validate()?;
    let status = match crate::profiles::native_models::run_cli(
        file,
        &["--prompt-cache-key-capability"],
        home,
        Duration::from_secs(10),
    ) {
        Ok(bytes) if advertised(&bytes) => Status::Active,
        Ok(_) => Status::Unsupported,
        // Older clap-based natives reject the unknown probe with exit 2.
        Err(error)
            if error
                .downcast_ref::<crate::profiles::native_models::NativeCliExit>()
                .is_some_and(|exit| exit.status.code() == Some(2)) =>
        {
            Status::Unsupported
        }
        Err(error) => return Err(error.context("native prompt-cache-key capability probe failed")),
    };
    anyhow::ensure!(
        key(file).ok().as_ref() == Some(before),
        "native executable changed during prompt-cache-key probe"
    );
    Ok(status)
}

/// Log a failed probe once per native hash; the launch continues without the key.
fn failed(file: &VerifiedFile, detail: &str) -> Status {
    let hash = file.sha256.as_str().to_owned();
    let first = WARNED
        .lock()
        .map(|mut warned| warned.get_or_insert_with(Default::default).insert(hash.clone()))
        .unwrap_or(true);
    if first {
        eprintln!(
            "prompt cache key: probing native {} ({hash}) failed; launching without the key: {detail}",
            file.path.display()
        );
    }
    Status::ProbeFailed
}

/// The probe outcome of the last launch, shared across processes (launches
/// run in Management, the Agent Bus or the CLI): `runtime/prompt-cache-key.json`.
fn status_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("prompt-cache-key.json"))
}

fn record(status: Status, native_sha256: &str) {
    static LAST: Mutex<Option<(Status, String)>> = Mutex::new(None);
    let entry = (status, native_sha256.to_owned());
    let Ok(mut last) = LAST.lock() else { return };
    if last.as_ref() == Some(&entry) {
        return;
    }
    let value = serde_json::json!({
        "status": status,
        "nativeSha256": native_sha256,
        "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    match status_path().and_then(|path| crate::config::atomic::write_private_pretty_json_atomic(&path, &value, "prompt cache key status")) {
        Ok(()) => *last = Some(entry),
        Err(error) => eprintln!("prompt cache key: status not recorded: {error:#}"),
    }
}

/// `promptCacheKeyStatus` for `/v2/settings`: `off` (setting off),
/// `pending` (no launch probed since), else the last launch's [`Status`].
pub fn settings_status(enabled: bool) -> &'static str {
    if !enabled {
        return "off";
    }
    status_word(
        status_path()
            .ok()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| serde_json::from_value::<Status>(value["status"].clone()).ok()),
    )
}

fn status_word(recorded: Option<Status>) -> &'static str {
    match recorded {
        None => "pending",
        Some(Status::Active) => "active",
        Some(Status::Unsupported) => "unsupported",
        Some(Status::ProbeFailed) => "probe_failed",
    }
}

/// Stable key for one workspace: `cutex-<host>-<16 hex of sha256(cwd)>`.
/// Moving or renaming the workspace intentionally changes the key.
pub fn workspace_key(host: &str, cwd: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest: [u8; 32] = Sha256::digest(cwd.as_bytes()).into();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("cutex-{}-{}", sanitize_host(host), &hex[..16])
}

/// The key becomes a native config value and the root agent's session-id
/// header; keep the host part to the native key charset.
fn sanitize_host(host: &str) -> String {
    let cleaned: String = host
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "host".to_string()
    } else {
        trimmed.to_string()
    }
}

fn override_for(launch: LaunchCommand, key: &str) -> LaunchCommand {
    launch
        .arg("-c")
        .arg(format!("prompt_cache_key=\"{key}\""))
}

/// Called after thread retention, only for the owned native server process.
/// Never refuses the launch: without support (or when the probe fails) the
/// native starts exactly as with the setting off.
pub fn apply(launch: LaunchCommand, bundle: &StockBundle, home: &Path, host: &str, cwd: &str) -> LaunchCommand {
    let status = if bundle.common_ingress() { probe(&bundle.executable, home) } else { Status::Unsupported };
    record(status, bundle.executable.sha256.as_str());
    if status == Status::Active {
        override_for(launch, &workspace_key(host, cwd))
    } else {
        launch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_workspace_scoped() {
        assert_eq!(
            workspace_key("host-a", "/home/example/work"),
            workspace_key("host-a", "/home/example/work")
        );
        assert_ne!(
            workspace_key("host-a", "/home/example/work"),
            workspace_key("host-a", "/home/example/other")
        );
        assert_ne!(
            workspace_key("host-a", "/home/example/work"),
            workspace_key("host-b", "/home/example/work")
        );
        let key = workspace_key("host-a", "/home/example/work");
        assert!(key.starts_with("cutex-host-a-") && key.len() == "cutex-host-a-".len() + 16);
    }

    #[test]
    fn host_part_is_sanitized() {
        assert_eq!(sanitize_host(" Host_Name.01 "), "Host_Name.01");
        assert_eq!(sanitize_host("bad host/name"), "bad-host-name");
        assert_eq!(sanitize_host("///"), "host");
    }

    #[test]
    fn probe_requires_exact_advertised_contract_and_old_natives_keep_args() {
        for bytes in [
            b"{}".as_slice(),
            br#"{"promptCacheKey":true}"#,
            br#"{"promptCacheKey":2}"#,
            b"not json",
        ] {
            assert!(!advertised(bytes));
        }
        assert!(advertised(br#"{"promptCacheKey":1}"#));
        let launch = LaunchCommand::new("native").args(["-c", "model=\"gpt-6-astra\""]);
        assert_eq!(
            override_for(launch.clone(), "cutex-host-0011223344556677").args,
            vec![
                "-c".to_string(),
                "model=\"gpt-6-astra\"".to_string(),
                "-c".to_string(),
                "prompt_cache_key=\"cutex-host-0011223344556677\"".to_string(),
            ]
        );
        assert_eq!(launch.args, vec!["-c", "model=\"gpt-6-astra\""]);
    }

    /// A failed probe is never an error (the launch goes on without the
    /// key), is cached for this binary and warned about once per hash.
    #[test]
    fn a_failed_probe_never_refuses_the_launch() {
        let dir = std::env::temp_dir().join(format!("pck-probe-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let sha = |c: char| crate::role_revision::Sha256::new(c.to_string().repeat(64)).unwrap();
        // Unreadable: the executable is missing.
        let missing = VerifiedFile { path: dir.join("missing-native"), sha256: sha('b') };
        assert_eq!(probe(&missing, &dir), Status::ProbeFailed);
        assert_eq!(probe(&missing, &dir), Status::ProbeFailed);
        // Present but not the verified binary (validation fails before running it).
        let path = dir.join("native");
        std::fs::write(&path, b"not a native").unwrap();
        let changed = VerifiedFile { path: path.clone(), sha256: sha('c') };
        assert_eq!(probe(&changed, &dir), Status::ProbeFailed);
        let cached = CACHE.lock().unwrap().as_ref().unwrap().get(&path).cloned().unwrap();
        assert_eq!(cached.1, Status::ProbeFailed);
        assert_eq!(probe(&changed, &dir), Status::ProbeFailed, "cached until PROBE_RETRY");
        let warned = WARNED.lock().unwrap().clone().unwrap();
        assert!(warned.contains(&"b".repeat(64)) && warned.contains(&"c".repeat(64)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_status_words() {
        assert_eq!(settings_status(false), "off");
        assert_eq!(status_word(None), "pending");
        assert_eq!(status_word(Some(Status::Active)), "active");
        assert_eq!(status_word(Some(Status::Unsupported)), "unsupported");
        assert_eq!(status_word(Some(Status::ProbeFailed)), "probe_failed");
        assert_eq!(serde_json::to_value(Status::ProbeFailed).unwrap(), "probe_failed");
    }
}