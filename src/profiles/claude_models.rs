//! Offline profile choices from the selected adapter's shared reference data.
//! No SDK runtime, account credentials, session catalogs or network are read.
use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::model::StoredAccount;
use super::native_models::{claude_models, NativeModels};
use crate::runtime::claude::ClaudeDeployment;

const CATALOG_PATH: &str = "src/cute_claude/model_catalog.json";
const MAX_CATALOG_BYTES: u64 = 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(3);
const VERSION_TTL: Duration = Duration::from_secs(300);
const MAX_VERSION_CACHE: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogError(&'static str);

impl CatalogError {
    fn view(&self) -> Value {
        json!({"code": self.0, "accountAvailability": "unknown"})
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    version: u64,
    updated_at: String,
    source_urls: Vec<String>,
    models: Vec<Model>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Model {
    id: String,
    display_name: String,
    family: Option<String>,
    kind: String,
    min_cli_version: Option<String>,
    supported_effort_levels: Option<Vec<String>>,
    /// Nominal context window from Claude Code's model registry (absent in
    /// older adapters, null for aliases resolved per account).
    #[serde(default)]
    context_window: Option<u64>,
    description: String,
}

fn version(value: &str) -> Option<[u64; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some([parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?])
}

fn parse_cli_version(output: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(output).ok()?.trim();
    let raw = text.strip_suffix(" (Claude Code)").unwrap_or(text);
    let raw = raw.strip_prefix('v').unwrap_or(raw);
    version(raw).map(|_| raw.to_string())
}

fn parse_catalog(bytes: &[u8]) -> Result<Catalog, CatalogError> {
    let catalog: Catalog = serde_json::from_slice(bytes).map_err(|_| CatalogError("claude_catalog_invalid"))?;
    let mut ids = HashSet::new();
    let label = |value: &str, max: usize| !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control);
    if catalog.version != 1 || catalog.models.is_empty() || catalog.models.len() > 512
        || !label(&catalog.updated_at, 64) || catalog.source_urls.len() > 32
        || catalog.source_urls.iter().any(|url| !label(url, 2048) || !url.starts_with("https://"))
        || catalog.models.iter().any(|model| {
            !label(&model.id, 200) || model.id.chars().any(char::is_whitespace)
                || !label(&model.display_name, 200) || model.description.len() > 4000
                || model.description.chars().any(char::is_control)
                || model.family.as_ref().is_some_and(|family| !label(family, 100))
                || !ids.insert(&model.id) || !matches!(model.kind.as_str(), "alias" | "pinned" | "default")
                || model.min_cli_version.as_deref().is_some_and(|v| version(v).is_none())
                || model.supported_effort_levels.as_ref().is_some_and(|levels| levels.iter()
                    .any(|level| !super::native_models::CLAUDE_EFFORTS.contains(&level.as_str())))
                || model.context_window.is_some_and(|window| window == 0 || window > 100_000_000)
        }) {
        return Err(CatalogError("claude_catalog_invalid"));
    }
    Ok(catalog)
}

/// The only account information exposed is the profile's explicit binding.
/// Reference models have no account entitlement semantics and never merge
/// another account's SDK observations or native session state.
pub fn for_profile(account: &StoredAccount, selected: Option<&str>) -> NativeModels {
    let account_id = account.claude_account.as_deref().unwrap_or("default");
    let deployment = match ClaudeDeployment::selected() {
        Ok(deployment) => deployment,
        Err(_) => return fallback(selected, account_id, None, CatalogError("claude_deployment_unavailable")),
    };
    from_deployment(&deployment, account_id, selected)
}

fn fallback(selected: Option<&str>, account_id: &str, path: Option<&Path>, error: CatalogError) -> NativeModels {
    let mut models = claude_models(selected);
    models.source = json!({"kind": "claudeAliases", "source": "legacyAliasFallback",
        "accountId": account_id, "accountAvailability": "unknown", "catalogPath": path,
        "cliVersion": null, "versionConstraints": [], "fallbackReason": error.view()});
    models
}

fn from_deployment(deployment: &ClaudeDeployment, account_id: &str, selected: Option<&str>) -> NativeModels {
    let path = deployment.adapter_source.join(CATALOG_PATH);
    let read = || -> Result<Vec<u8>, CatalogError> {
        let file = std::fs::File::open(&path).map_err(|e| CatalogError(if e.kind() == std::io::ErrorKind::NotFound {
            "claude_catalog_missing"
        } else { "claude_catalog_unreadable" }))?;
        let mut bytes = Vec::new();
        file.take(MAX_CATALOG_BYTES + 1).read_to_end(&mut bytes).map_err(|_| CatalogError("claude_catalog_unreadable"))?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES { return Err(CatalogError("claude_catalog_too_large")); }
        Ok(bytes)
    };
    let bytes = match read() {
        Ok(bytes) => bytes,
        Err(error) => return fallback(selected, account_id, Some(&path), error),
    };
    let catalog = match parse_catalog(&bytes) {
        Ok(catalog) => catalog,
        Err(error) => return fallback(selected, account_id, Some(&path), error),
    };
    let cli = cli_version(&deployment.claude_cli);
    render(catalog, &bytes, &path, account_id, selected, cli)
}

fn render(catalog: Catalog, bytes: &[u8], path: &Path, account_id: &str,
    selected: Option<&str>, cli: Result<String, CatalogError>) -> NativeModels {
    let mut rows = Vec::new();
    let mut constraints = Vec::new();
    for model in catalog.models {
        let compatible = model.min_cli_version.as_deref().and_then(|minimum|
            cli.as_ref().ok().and_then(|actual| Some(version(actual)? >= version(minimum)?)));
        let state = match compatible { Some(true) => "compatible", Some(false) => "incompatible", None => "unknown" };
        let reason = match compatible { Some(true) => None, Some(false) => Some("cliVersionTooOld"),
            None if cli.is_err() => Some("cliVersionUnknown"), None => Some("minimumCliVersionUnknown") };
        let compatibility = json!({"state": state, "reason": reason, "minCliVersion": model.min_cli_version});
        constraints.push(json!({"id": model.id, "minCliVersion": model.min_cli_version, "cliCompatibility": compatibility}));
        if compatible == Some(false) && selected != Some(model.id.as_str()) { continue; }
        let levels = model.supported_effort_levels.as_ref().map(|levels| levels.iter()
            .map(|effort| json!({"effort": effort})).collect::<Vec<_>>());
        let display = if model.kind == "alias" && !model.display_name.ends_with(" (alias)") {
            format!("{} (alias)", model.display_name)
        } else { model.display_name };
        rows.push(json!({"slug": model.id, "display_name": display,
            "description": model.description, "family": model.family, "kind": model.kind,
            "source": "reference", "accountAvailability": "unknown",
            "minCliVersion": model.min_cli_version, "cliCompatibility": compatibility, "effortSupportKnown": levels.is_some(),
            "supported_reasoning_levels": levels.unwrap_or_default(), "default_reasoning_level": null,
            "context_window": model.context_window, "input_modalities": ["text", "image"]}));
    }
    if let Some(selected) = selected.filter(|id| !id.trim().is_empty()) {
        if !rows.iter().any(|row| row["slug"] == selected) {
            rows.push(json!({"slug": selected, "display_name": selected, "kind": "custom", "source": "selected",
                "accountAvailability": "unknown", "effortSupportKnown": false,
                "supported_reasoning_levels": [], "default_reasoning_level": null, "input_modalities": ["text", "image"]}));
        }
    }
    NativeModels { rows, source: json!({"kind": "claudeReference", "source": "adapterReference",
        "catalogPath": path, "digest": format!("{:x}", Sha256::digest(bytes)),
        "updatedAt": catalog.updated_at, "sourceUrls": catalog.source_urls,
        "cliVersion": cli.as_ref().ok(), "cliVersionError": cli.err().map(|error| error.view()),
        "versionConstraints": constraints, "accountId": account_id, "accountAvailability": "unknown"}) }
}

#[derive(Clone, PartialEq, Eq)]
struct VersionKey { path: PathBuf, modified: Option<SystemTime>, len: u64 }
struct VersionEntry { key: VersionKey, checked: Instant, result: Result<String, CatalogError> }
static VERSIONS: Mutex<VecDeque<VersionEntry>> = Mutex::new(VecDeque::new());

fn cli_version(path: &Path) -> Result<String, CatalogError> {
    let metadata = path.metadata().map_err(|_| CatalogError("claude_cli_unavailable"))?;
    let key = VersionKey { path: path.to_path_buf(), modified: metadata.modified().ok(), len: metadata.len() };
    // Serialize the bounded probe too: simultaneous picker requests never
    // create a herd of CLI processes. Cache failures for the same short TTL.
    let mut cache = VERSIONS.lock().map_err(|_| CatalogError("claude_cli_cache_unavailable"))?;
    cached_version(&mut cache, key, Instant::now(), || probe_version(path))
}

fn cached_version(cache: &mut VecDeque<VersionEntry>, key: VersionKey, now: Instant,
    probe: impl FnOnce() -> Result<String, CatalogError>) -> Result<String, CatalogError> {
    if let Some(entry) = cache.iter().find(|entry| entry.key == key && now.duration_since(entry.checked) < VERSION_TTL) {
        return entry.result.clone();
    }
    cache.retain(|entry| entry.key.path != key.path && now.duration_since(entry.checked) < VERSION_TTL);
    let result = probe();
    while cache.len() >= MAX_VERSION_CACHE { cache.pop_front(); }
    cache.push_back(VersionEntry { key, checked: now, result: result.clone() });
    result
}

fn probe_version(path: &Path) -> Result<String, CatalogError> {
    let scratch = std::env::temp_dir().join(format!("cutex-claude-version-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).map_err(|_| CatalogError("claude_cli_probe_failed"))?;
    struct Cleanup(PathBuf);
    impl Drop for Cleanup { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    let _scratch = Cleanup(scratch.clone());
    let mut command = Command::new(path);
    command.arg("--version").env_clear().current_dir(&scratch)
        .env("HOME", &scratch).env("USERPROFILE", &scratch).env("CLAUDE_CONFIG_DIR", &scratch)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    for name in ["SYSTEMROOT", "WINDIR", "PATH", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) { command.env(name, value); }
    }
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|_| CatalogError("claude_cli_probe_failed"))?;
    let stdout = child.stdout.take().ok_or(CatalogError("claude_cli_probe_failed"))?;
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(4097).read_to_end(&mut bytes).map(|_| bytes);
        let _ = send.send(result);
    });
    let deadline = Instant::now() + VERSION_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(_) => { let _ = child.kill(); let _ = child.wait(); return Err(CatalogError("claude_cli_probe_failed")); }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill(); let _ = child.wait(); return Err(CatalogError("claude_cli_probe_timeout"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    if !status.success() { return Err(CatalogError("claude_cli_probe_failed")); }
    let output = receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| CatalogError("claude_cli_probe_timeout"))?
        .map_err(|_| CatalogError("claude_cli_probe_failed"))?;
    if output.len() > 4096 { return Err(CatalogError("claude_cli_version_invalid")); }
    parse_cli_version(&output).ok_or(CatalogError("claude_cli_version_invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> Vec<u8> {
        serde_json::to_vec(&json!({"version": 1, "updatedAt": "2026-10-11", "sourceUrls": ["https://example.invalid/reference"],
            "models": [
                {"id": "alias", "displayName": "Family (alias)", "family": "family", "kind": "alias", "description": "Alias",
                    "minCliVersion": null, "supportedEffortLevels": null},
                {"id": "pinned-old", "displayName": "Family old", "family": "family", "kind": "pinned", "description": "Reference",
                    "minCliVersion": "2.1.9", "supportedEffortLevels": ["low", "high"], "contextWindow": 200000},
                {"id": "pinned-new", "displayName": "Family new", "family": "family", "kind": "pinned", "description": "Reference",
                    "minCliVersion": "2.1.100", "supportedEffortLevels": []}
            ]})).unwrap()
    }

    fn models(cli: Result<&str, CatalogError>, selected: Option<&str>, account: &str) -> NativeModels {
        let bytes = reference();
        render(parse_catalog(&bytes).unwrap(), &bytes, Path::new("private/reference.json"), account, selected, cli.map(str::to_string))
    }

    #[test]
    fn reference_filters_old_cli_but_preserves_selected_and_account_scope() {
        let result = models(Ok("2.1.10"), None, "account-a");
        assert_eq!(result.rows.iter().map(|r| r["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["alias", "pinned-old"]);
        assert_eq!(result.source["versionConstraints"][2]["cliCompatibility"]["state"], "incompatible");
        assert_eq!(result.source["versionConstraints"][2]["cliCompatibility"]["reason"], "cliVersionTooOld");
        assert_eq!(result.source["accountAvailability"], "unknown");
        assert_eq!(result.source["accountId"], "account-a");
        assert_eq!(result.source["digest"], format!("{:x}", Sha256::digest(reference())));
        // Nominal window passes through; a catalog without the field (older adapters) gives null.
        assert_eq!(result.rows[1]["context_window"], 200_000);
        assert_eq!(result.rows[0]["context_window"], Value::Null);
        let other = models(Ok("2.1.10"), Some("pinned-new"), "account-b");
        assert_eq!(other.rows.len(), 3);
        assert_eq!(other.rows[2]["cliCompatibility"]["state"], "incompatible");
        assert_eq!(other.source["accountId"], "account-b");
        assert!(!other.source.to_string().contains("account-a"));
    }

    #[test]
    fn unknown_cli_and_custom_selection_never_claim_availability_or_effort() {
        let result = models(Err(CatalogError("claude_cli_probe_timeout")), Some("custom-route-id"), "default");
        assert_eq!(result.rows.len(), 4);
        assert_eq!(result.source["cliVersion"], Value::Null);
        assert_eq!(result.source["cliVersionError"]["code"], "claude_cli_probe_timeout");
        for row in &result.rows { assert_eq!(row["accountAvailability"], "unknown"); }
        assert_eq!(result.rows[0]["effortSupportKnown"], false);
        assert_eq!(result.rows[3]["source"], "selected");
        assert_eq!(result.rows[3]["kind"], "custom");
        assert_eq!(result.rows[3]["supported_reasoning_levels"], json!([]));
        assert_eq!(result.source["versionConstraints"][1]["cliCompatibility"]["state"], "unknown");
    }

    #[test]
    fn malformed_catalog_is_typed_and_legacy_fallback_retains_selection() {
        assert_eq!(parse_catalog(b"not json").err(), Some(CatalogError("claude_catalog_invalid")));
        let mut value: Value = serde_json::from_slice(&reference()).unwrap();
        value["models"][2]["id"] = json!("alias");
        assert_eq!(parse_catalog(&serde_json::to_vec(&value).unwrap()).err(), Some(CatalogError("claude_catalog_invalid")));
        let result = fallback(Some("saved-custom"), "private-account", None, CatalogError("claude_catalog_missing"));
        assert_eq!(result.rows.last().unwrap()["slug"], "saved-custom");
        assert_eq!(result.source["kind"], "claudeAliases");
        assert_eq!(result.source["fallbackReason"]["code"], "claude_catalog_missing");
        assert_eq!(result.source["accountAvailability"], "unknown");
    }

    #[test]
    fn shared_kinds_nullable_family_and_alias_labels_are_preserved() {
        let mut value: Value = serde_json::from_slice(&reference()).unwrap();
        value["models"][0]["displayName"] = json!("Family");
        value["models"][0]["family"] = Value::Null;
        value["models"][1]["kind"] = json!("default");
        let bytes = serde_json::to_vec(&value).unwrap();
        let result = render(parse_catalog(&bytes).unwrap(), &bytes, Path::new("reference.json"), "default", None, Ok("2.1.100".into()));
        assert_eq!(result.rows[0]["display_name"], "Family (alias)");
        assert_eq!(result.rows[0]["family"], Value::Null);
        assert_eq!(result.rows[1]["kind"], "default");
        value["models"][1]["contextWindow"] = json!(0);
        assert_eq!(parse_catalog(&serde_json::to_vec(&value).unwrap()).err(), Some(CatalogError("claude_catalog_invalid")));
        value["models"][1]["contextWindow"] = json!(1_000_000);
        assert!(parse_catalog(&serde_json::to_vec(&value).unwrap()).is_ok());
        value["models"][0]["displayName"] = json!("invalid\nlabel");
        assert_eq!(parse_catalog(&serde_json::to_vec(&value).unwrap()).err(), Some(CatalogError("claude_catalog_invalid")));
    }

    #[test]
    #[ignore = "requires CUTEX_CLAUDE_REFERENCE_FIXTURE from the independently shipped adapter"]
    fn deployed_shared_catalog_contract() {
        let path = PathBuf::from(std::env::var_os("CUTEX_CLAUDE_REFERENCE_FIXTURE").expect("explicit private adapter catalog fixture"));
        let bytes = std::fs::read(&path).unwrap();
        let catalog = parse_catalog(&bytes).unwrap();
        assert!(catalog.models.iter().any(|model| model.kind == "pinned"));
        assert!(catalog.models.iter().any(|model| model.kind == "alias"));
        let result = render(catalog, &bytes, &path, "fixture-account", Some("fixture-custom-id"), Ok("0.0.0".into()));
        assert_eq!(result.rows.last().unwrap()["slug"], "fixture-custom-id");
        assert_eq!(result.source["accountAvailability"], "unknown");
        assert_eq!(result.source["digest"], format!("{:x}", Sha256::digest(&bytes)));
    }

    #[test]
    #[ignore = "requires CUTEX_CLAUDE_VERSION_FIXTURE pointing to the private Claude CLI fixture"]
    fn private_cli_version_process_probe() {
        let path = PathBuf::from(std::env::var_os("CUTEX_CLAUDE_VERSION_FIXTURE").expect("explicit private CLI fixture"));
        assert!(path.is_absolute() && path.is_file());
        assert_eq!(cli_version(&path).unwrap(), "2.1.292");
        assert_eq!(cli_version(&path).unwrap(), "2.1.292");
        let cache = VERSIONS.lock().unwrap();
        assert_eq!(cache.iter().filter(|entry| entry.key.path == path).count(), 1);
    }

    #[test]
    fn missing_and_invalid_deployment_catalogs_do_not_launch_a_cli() {
        let root = std::env::temp_dir().join(format!("cutex-claude-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("adapter/src/cute_claude")).unwrap();
        let deployment = ClaudeDeployment { python: root.join("missing-python"), adapter_source: root.join("adapter"),
            claude_cli: root.join("must-not-run"), claude_home: root.join("must-not-read"), tui_binary: root.join("missing-tui") };
        let result = from_deployment(&deployment, "bound-account", Some("custom"));
        assert_eq!(result.source["fallbackReason"]["code"], "claude_catalog_missing");
        std::fs::write(deployment.adapter_source.join(CATALOG_PATH), "invalid").unwrap();
        let result = from_deployment(&deployment, "bound-account", None);
        assert_eq!(result.source["fallbackReason"]["code"], "claude_catalog_invalid");
        assert!(!deployment.claude_home.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cli_version_parser_is_numeric_and_rejects_ambiguous_output() {
        assert_eq!(parse_cli_version(b"2.1.98 (Claude Code)\r\n"), Some("2.1.98".into()));
        assert_eq!(parse_cli_version(b"v2.1.98\n"), Some("2.1.98".into()));
        assert!(version("2.1.100") > version("2.1.9"));
        assert_eq!(parse_cli_version(b"2.1.98-beta (Claude Code)"), None);
        assert_eq!(parse_cli_version(b"2.1.98 2.1.99"), None);
        assert_eq!(parse_cli_version(b"not a version"), None);
        assert_eq!(parse_cli_version(b"Python 3.11.9"), None);
        assert_eq!(parse_cli_version(b"3.11.9"), Some("3.11.9".into()));
    }

    #[test]
    fn version_cache_is_bounded_invalidates_metadata_and_expires_failures() {
        let mut cache = VecDeque::new();
        let now = Instant::now();
        let key = VersionKey { path: "private-cli".into(), modified: None, len: 1 };
        assert_eq!(cached_version(&mut cache, key.clone(), now, || Err(CatalogError("claude_cli_probe_failed"))), Err(CatalogError("claude_cli_probe_failed")));
        assert!(cached_version(&mut cache, key.clone(), now, || panic!("cached failure must not respawn")).is_err());
        let changed = VersionKey { len: 2, ..key.clone() };
        assert_eq!(cached_version(&mut cache, changed.clone(), now, || Ok("2.1.9".into())), Ok("2.1.9".into()));
        assert_eq!(cached_version(&mut cache, changed, now + VERSION_TTL, || Ok("2.1.10".into())), Ok("2.1.10".into()));
        for index in 0..(MAX_VERSION_CACHE + 3) {
            let key = VersionKey { path: format!("cli-{index}").into(), ..key.clone() };
            cached_version(&mut cache, key, now + VERSION_TTL, || Ok("2.1.10".into())).unwrap();
        }
        assert_eq!(cache.len(), MAX_VERSION_CACHE);
    }
}
