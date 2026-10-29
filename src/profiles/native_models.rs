//! Model choices for profiles without an editable `model_catalog_json`
//! (built-in OpenAI provider with ChatGPT auth / official profiles, and
//! Claude profiles), for `GET /v2/profiles/{profile}/models`.
//!
//! Codex: cute-codex resolves its model list in the models manager from, in
//! order, the remote `/models` catalog cached in `<CODEX_HOME>/models_cache.json`
//! (written by every native that refreshed models, 5 minute TTL for the native
//! itself) and the catalog compiled into the binary (`codex debug models
//! --bundled`). Cutex reads the same two sources without a running session:
//! the shared native home's cache file first (what a runtime would show,
//! including server-side additions), else the bundled catalog of the selected
//! deployment's CLI, run once and memoized per CLI digest. Neither path
//! contacts the network or starts an app-server. Rows keep the native
//! `ModelInfo` (snake_case) shape so `catalog_api::model_view` renders them.
//! `modelsSource.native {version, cliSha256}` names the deployed native CLI
//! (`--version`, memoized per digest); for a `nativeCache` list,
//! `modelsSource.notInBundled` lists cached slugs the deployed CLI's bundled
//! catalog lacks (server-side additions a newer native wrote into the cache).
//!
//! Claude: the selected adapter's shared reference catalog, filtered by its
//! CLI version; older adapters retain the legacy alias fallback.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use serde_json::{json, Value};

use crate::launch::stock::VerifiedFile;
use crate::profiles::model::{CliKind, StoredAccount};

/// Native models cache file inside a codex home (models-manager `MODEL_CACHE_FILE`).
pub const NATIVE_MODELS_CACHE_FILE: &str = "models_cache.json";

/// Claude Code aliases offered for Claude profiles: (alias, display name, effort supported).
pub const CLAUDE_MODEL_ALIASES: &[(&str, &str, bool)] = &[
    ("opus", "Opus", true),
    ("sonnet", "Sonnet", true),
    ("haiku", "Haiku", false),
    ("opus[1m]", "Opus (1M context)", true),
    ("sonnet[1m]", "Sonnet (1M context)", true),
];
/// `ClaudeEffort` values, in order.
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Rows plus a `modelsSource` description.
pub struct NativeModels {
    pub rows: Vec<Value>,
    pub source: Value,
}

/// Models for a profile whose catalog is not editable. `selected` is kept even
/// when the native marks it hidden. Never fails: an unavailable source yields
/// `None` and the caller reports no models.
pub fn for_profile(account: &StoredAccount, selected: Option<&str>) -> Result<NativeModels, String> {
    match account.cli_kind {
        CliKind::Claude => Ok(super::claude_models::for_profile(account, selected)),
        CliKind::Codex => codex_models(Rows::Picker(selected)),
        CliKind::Dsh => Ok(super::dsh::models(account)),
        CliKind::Kimi => Ok(super::kimi::native_models(account)),
    }
}

/// Which native rows a caller needs.
#[derive(Clone, Copy)]
pub enum Rows<'a> {
    /// What a picker shows: rows the native hides are dropped, except `selected`.
    Picker(Option<&'a str>),
    /// Every row, hidden ones included (visibility is a display property).
    All,
}

/// The complete upstream list of an official Codex profile, hidden rows
/// included (`profile-catalog-overlay` resolves against this and applies
/// visibility only for display). Claude profiles: their picker list.
pub fn upstream_for_profile(account: &StoredAccount) -> Result<NativeModels, String> {
    match account.cli_kind {
        CliKind::Claude => Ok(super::claude_models::for_profile(account, None)),
        CliKind::Codex => codex_models(Rows::All),
        CliKind::Dsh => Ok(super::dsh::models(account)),
        CliKind::Kimi => Ok(super::kimi::native_models(account)),
    }
}

fn codex_models(which: Rows<'_>) -> Result<NativeModels, String> {
    let deployment = crate::launch::local_deployment::LocalDeployment::selected()
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(|| "no local runtime deployment is selected".to_string())?;
    let bundle_cli = || -> Result<VerifiedFile, String> {
        let bundle: crate::launch::stock::StockBundle = std::fs::read(&deployment.bundle_manifest)
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))?;
        bundle
            .cli
            .ok_or_else(|| "the selected native bundle has no CLI to list bundled models".to_string())
    };
    if let Some(mut models) = cached_rows(&deployment.native_home, which) {
        // Which deployed native the cache is compared against, and
        // which cached models that native does not bundle.
        if let Ok(cli) = bundle_cli() {
            models.source["native"] = native_view(&cli, &deployment.native_home);
            models.source["notInBundled"] = match bundled_models(&cli, &deployment.native_home) {
                Ok(bundled) => json!(not_in_bundled(&models.rows, &bundled)),
                Err(_) => Value::Null,
            };
        }
        return Ok(models);
    }
    let cli = bundle_cli()?;
    let rows = bundled_models(&cli, &deployment.native_home).map_err(|error| format!("{error:#}"))?;
    Ok(NativeModels {
        rows: select_rows(rows, which),
        source: json!({
            "kind": "nativeBundled",
            "cliSha256": cli.sha256.as_str(),
            "native": native_view(&cli, &deployment.native_home),
        }),
    })
}

/// `<native_home>/models_cache.json`, if present and non-empty.
pub fn cached_models(native_home: &Path, selected: Option<&str>) -> Option<NativeModels> {
    cached_rows(native_home, Rows::Picker(selected))
}

fn cached_rows(native_home: &Path, which: Rows<'_>) -> Option<NativeModels> {
    let bytes = std::fs::read(native_home.join(NATIVE_MODELS_CACHE_FILE)).ok()?;
    parse_cache_rows(&bytes, which)
}

#[cfg(test)]
fn parse_cache(bytes: &[u8], selected: Option<&str>) -> Option<NativeModels> {
    parse_cache_rows(bytes, Rows::Picker(selected))
}

fn parse_cache_rows(bytes: &[u8], which: Rows<'_>) -> Option<NativeModels> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let rows = value.get("models")?.as_array()?.clone();
    let rows = select_rows(rows, which);
    (!rows.is_empty()).then(|| NativeModels {
        rows,
        source: json!({
            "kind": "nativeCache",
            "fetchedAt": value.get("fetched_at"),
            "clientVersion": value.get("client_version"),
        }),
    })
}

/// Drop rows without a slug and rows the native hides from pickers (except
/// the selected model), ordered like the native picker (`priority`).
pub fn visible_rows(rows: Vec<Value>, selected: Option<&str>) -> Vec<Value> {
    select_rows(rows, Rows::Picker(selected))
}

/// Whether a picker shows `row`: the native hides `visibility: hide` rows,
/// except the selected model.
pub fn shown_in_picker(row: &Value, selected: Option<&str>) -> bool {
    row.get("visibility").and_then(Value::as_str) != Some("hide")
        || row.get("slug").and_then(Value::as_str).is_some_and(|slug| Some(slug) == selected)
}

fn select_rows(rows: Vec<Value>, which: Rows<'_>) -> Vec<Value> {
    let mut rows: Vec<Value> = rows
        .into_iter()
        .filter(|row| {
            row.get("slug").and_then(Value::as_str).is_some()
                && match which {
                    Rows::Picker(selected) => shown_in_picker(row, selected),
                    Rows::All => true,
                }
        })
        .collect();
    rows.sort_by_key(|row| row.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));
    rows
}

/// Slugs of `rows` that the bundled catalog does not contain. Rows the native
/// hides (`visibility: hide`, server-side entries such as `gpt-reserve`) are
/// not offered to anyone, so a missing one is no reason to update the native.
pub fn not_in_bundled(rows: &[Value], bundled: &[Value]) -> Vec<String> {
    let bundled: std::collections::HashSet<&str> =
        bundled.iter().filter_map(|row| row.get("slug").and_then(Value::as_str)).collect();
    rows.iter()
        .filter(|row| row.get("visibility").and_then(Value::as_str) != Some("hide"))
        .filter_map(|row| row.get("slug").and_then(Value::as_str))
        .filter(|slug| !bundled.contains(slug))
        .map(str::to_string)
        .collect()
}

/// `{version, cliSha256}` of the deployed native CLI (`version` null when
/// `--version` cannot be read).
pub fn native_view(cli: &VerifiedFile, native_home: &Path) -> Value {
    json!({
        "version": native_version(cli, native_home).ok(),
        "cliSha256": cli.sha256.as_str(),
    })
}

/// `codex-cli 0.157.1-r46` → `0.157.1-r46`.
pub fn parse_cli_version(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|line| !line.is_empty())?;
    let version = line.rsplit(' ').next()?.trim();
    (!version.is_empty() && version.len() <= 64 && !version.chars().any(char::is_control))
        .then(|| version.to_string())
}

static VERSION: Mutex<Option<(String, String)>> = Mutex::new(None);

/// `<cli> --version`, memoized per CLI digest.
pub fn native_version(cli: &VerifiedFile, native_home: &Path) -> anyhow::Result<String> {
    let key = cli.sha256.as_str().to_string();
    if let Some((cached, version)) = VERSION.lock().map_err(|_| anyhow::anyhow!("native version cache poisoned"))?.as_ref() {
        if *cached == key {
            return Ok(version.clone());
        }
    }
    cli.validate()?;
    let out = run_cli(cli, &["--version"], native_home, std::time::Duration::from_secs(10))?;
    let version = parse_cli_version(&String::from_utf8_lossy(&out)).context("native CLI printed no version")?;
    *VERSION.lock().map_err(|_| anyhow::anyhow!("native version cache poisoned"))? = Some((key, version.clone()));
    Ok(version)
}

#[derive(Debug)]
pub(crate) struct NativeCliExit {
    pub status: std::process::ExitStatus,
    args: String,
}
impl std::fmt::Display for NativeCliExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native CLI `{}` failed ({})", self.args, self.status)
    }
}
impl std::error::Error for NativeCliExit {}

pub(crate) fn run_cli(cli: &VerifiedFile, args: &[&str], native_home: &Path, timeout: std::time::Duration) -> anyhow::Result<Vec<u8>> {
    let mut command = std::process::Command::new(&cli.path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command.args(args)
        .env("CODEX_HOME", native_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("native CLI could not be started")?;
    let mut stdout = child.stdout.take().context("native CLI stdout missing")?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut out).map(|_| out)
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("native CLI did not finish `{}` within {}s", args.join(" "), timeout.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let out = reader.join().map_err(|_| anyhow::anyhow!("native CLI reader panicked"))??;
    if !status.success() {
        return Err(NativeCliExit { status, args: args.join(" ") }.into());
    }
    Ok(out)
}

static BUNDLED: Mutex<Option<(String, Vec<Value>)>> = Mutex::new(None);

/// `<cli> debug models --bundled`, memoized per CLI digest (the catalog is
/// compiled into the binary). The CLI is verified before its first run.
pub fn bundled_models(cli: &VerifiedFile, native_home: &Path) -> anyhow::Result<Vec<Value>> {
    let key = cli.sha256.as_str().to_string();
    if let Some((cached, rows)) = BUNDLED.lock().map_err(|_| anyhow::anyhow!("bundled model cache poisoned"))?.as_ref() {
        if *cached == key {
            return Ok(rows.clone());
        }
    }
    cli.validate()?;
    let out = run_cli(cli, &["debug", "models", "--bundled"], native_home, std::time::Duration::from_secs(15))?;
    let value: Value = serde_json::from_slice(&out).context("native bundled model list is not JSON")?;
    let rows = value
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .context("native bundled model list has no models")?;
    *BUNDLED.lock().map_err(|_| anyhow::anyhow!("bundled model cache poisoned"))? = Some((key, rows.clone()));
    Ok(rows)
}

/// Claude aliases (plus the selected model when it is not an alias), as
/// native-shaped rows.
pub fn claude_models(selected: Option<&str>) -> NativeModels {
    let row = |slug: &str, display: &str, effort: bool| {
        let levels: Vec<Value> = if effort {
            CLAUDE_EFFORTS.iter().map(|effort| json!({ "effort": effort })).collect()
        } else {
            Vec::new()
        };
        json!({
            "slug": slug,
            "display_name": display,
            "supported_reasoning_levels": levels,
            "default_reasoning_level": Value::Null,
            "input_modalities": ["text", "image"],
            "kind": "alias",
            "source": "legacyAlias",
            "accountAvailability": "unknown",
        })
    };
    let mut rows: Vec<Value> = CLAUDE_MODEL_ALIASES
        .iter()
        .map(|(slug, display, effort)| row(slug, &format!("{display} (alias)"), *effort))
        .collect();
    if let Some(selected) = selected.filter(|s| !s.trim().is_empty()) {
        if !CLAUDE_MODEL_ALIASES.iter().any(|(slug, _, _)| *slug == selected) {
            let mut custom = row(selected, selected, true);
            custom["kind"] = json!("custom");
            custom["source"] = json!("selected");
            rows.push(custom);
        }
    }
    NativeModels { rows, source: json!({ "kind": "claudeAliases" }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_cache_rows_hide_hidden_models_except_the_selected_one() {
        let cache = json!({
            "fetched_at": "2026-10-06T11:15:27Z", "client_version": "0.160.0", "etag": "x", "identity": "secret-free-hash",
            "models": [
                { "slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "visibility": "list", "priority": 5,
                  "default_reasoning_level": "low", "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "high" }] },
                { "slug": "gpt-reserve", "visibility": "hide", "priority": 4 },
                { "slug": "gpt-6.1-sol", "visibility": "list", "priority": 1 },
                { "display_name": "no slug" }
            ]
        });
        let bytes = serde_json::to_vec(&cache).unwrap();
        let models = parse_cache(&bytes, None).unwrap();
        let slugs: Vec<_> = models.rows.iter().map(|r| r["slug"].as_str().unwrap()).collect();
        assert_eq!(slugs, ["gpt-6.1-sol", "gpt-5.6-sol"]);
        assert_eq!(models.source, json!({ "kind": "nativeCache", "fetchedAt": "2026-10-06T11:15:27Z", "clientVersion": "0.160.0" }));
        assert!(!models.source.to_string().contains("identity"));
        let models = parse_cache(&bytes, Some("gpt-reserve")).unwrap();
        assert_eq!(models.rows.len(), 3);
        assert!(parse_cache(br#"{"models":[]}"#, None).is_none());
        assert!(parse_cache(b"not json", None).is_none());
    }

    #[test]
    fn native_cache_reports_models_missing_from_the_bundled_catalog() {
        let rows = vec![
            json!({ "slug": "gpt-6.1-sol" }),
            json!({ "slug": "gpt-5.6-sol" }),
            json!({ "slug": "server-only" }),
            json!({ "slug": "gpt-reserve", "visibility": "hide" }),
            json!({ "display_name": "no slug" }),
        ];
        let bundled = vec![json!({ "slug": "gpt-5.6-sol" }), json!({ "slug": "gpt-6.1-sol" }), json!({})];
        assert_eq!(not_in_bundled(&rows, &bundled), ["server-only"]);
        assert!(not_in_bundled(&rows[..2], &bundled).is_empty());
        assert_eq!(parse_cli_version("codex-cli 0.157.1-r46\n").as_deref(), Some("0.157.1-r46"));
        assert_eq!(parse_cli_version("\n  codex-cli 0.160.0-r47  \r\n").as_deref(), Some("0.160.0-r47"));
        assert_eq!(parse_cli_version(""), None);
    }

    #[test]
    fn claude_aliases_include_a_custom_selected_model() {
        let models = claude_models(Some("sonnet"));
        assert_eq!(models.rows.len(), CLAUDE_MODEL_ALIASES.len());
        assert_eq!(models.source["kind"], "claudeAliases");
        assert_eq!(models.rows[0]["supported_reasoning_levels"][4]["effort"], "max");
        let haiku = models.rows.iter().find(|r| r["slug"] == "haiku").unwrap();
        assert_eq!(haiku["supported_reasoning_levels"], json!([]));
        let models = claude_models(Some("claude-fable-5-1[1m]"));
        assert_eq!(models.rows.last().unwrap()["slug"], "claude-fable-5-1[1m]");
    }
}
