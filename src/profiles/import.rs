//! Import and source-detection helpers for existing Codex auth/config files.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use base64::Engine as _;
use serde_json::Value;

use crate::config::paths::home_dir;
use crate::config::text::read_optional_text;
use crate::profiles::model::ImportedSnapshot;
use crate::profiles::profile_config::extract_profile_config_toml;
use crate::profiles::profile_config::parse_toml_table;
use crate::profiles::profile_config::PROFILE_LOCAL_MODEL_CATALOG_FILE;

pub fn import_snapshot(
    auth_path: &str,
    config_path: Option<&str>,
) -> anyhow::Result<ImportedSnapshot> {
    let raw_auth_json = fs::read_to_string(auth_path)
        .with_context(|| format!("Failed to read auth.json: {auth_path}"))?;

    let config_source = match config_path {
        Some(path) => Some((
            PathBuf::from(path),
            fs::read_to_string(path)
                .with_context(|| format!("Failed to read config.toml: {path}"))?,
        )),
        None => infer_config_toml(auth_path)?,
    };
    let (raw_config_toml, raw_model_catalog_json) = match config_source {
        Some((path, text)) => import_profile_config(&path, &text)?,
        None => (None, None),
    };

    let (email, plan_type) = parse_auth_metadata(&raw_auth_json);
    let source = detect_source_label(Some(&raw_auth_json), raw_config_toml.as_deref());

    Ok(ImportedSnapshot {
        raw_auth_json,
        raw_config_toml,
        raw_model_catalog_json,
        email,
        plan_type,
        source,
    })
}

fn infer_config_toml(auth_path: &str) -> anyhow::Result<Option<(PathBuf, String)>> {
    let auth_path = Path::new(auth_path);
    let config_path = auth_path
        .parent()
        .map(|parent| parent.join("config.toml"))
        .ok_or_else(|| anyhow::anyhow!("Failed to determine auth.json parent directory"))?;

    Ok(read_optional_text(&config_path)?.map(|contents| (config_path, contents)))
}

fn import_profile_config(
    config_path: &Path,
    config_toml: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let mut root = parse_toml_table(config_toml)?;
    let configured_catalog = match root.get("model_catalog_json") {
        None => None,
        Some(value) => Some(toml_string_value(value).ok_or_else(|| {
            anyhow::anyhow!("config.toml key `model_catalog_json` must be a non-empty string path")
        })?),
    };
    let raw_model_catalog_json = match configured_catalog {
        Some(configured_path) => {
            let source_path = resolve_model_catalog_path(config_path, configured_path)?;
            let contents = fs::read_to_string(&source_path).with_context(|| {
                format!(
                    "Failed to read model catalog referenced by {}: {}",
                    config_path.display(),
                    source_path.display()
                )
            })?;
            serde_json::from_str::<serde_json::Value>(&contents).with_context(|| {
                format!("Failed to parse model catalog: {}", source_path.display())
            })?;
            root.insert(
                "model_catalog_json".to_string(),
                toml::Value::String(PROFILE_LOCAL_MODEL_CATALOG_FILE.to_string()),
            );
            Some(contents)
        }
        None => None,
    };
    let portable_config = toml::to_string_pretty(&root)?;
    Ok((
        extract_profile_config_toml(&portable_config)?,
        raw_model_catalog_json,
    ))
}

fn toml_string_value(value: &toml::Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn resolve_model_catalog_path(config_path: &Path, configured: &str) -> anyhow::Result<PathBuf> {
    let configured = configured.trim();
    if configured == "~" {
        return home_dir().context("Could not determine home directory for model_catalog_json");
    }
    if let Some(relative) = configured
        .strip_prefix("~/")
        .or_else(|| configured.strip_prefix("~\\"))
    {
        return Ok(home_dir()
            .context("Could not determine home directory for model_catalog_json")?
            .join(relative));
    }

    let configured_path = PathBuf::from(configured);
    if configured_path.is_absolute() {
        return Ok(configured_path);
    }
    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to determine config.toml parent directory"))?;
    Ok(parent.join(configured_path))
}

pub fn parse_auth_metadata(raw_auth_json: &str) -> (Option<String>, Option<String>) {
    let json: Value = match serde_json::from_str(raw_auth_json) {
        Ok(value) => value,
        Err(_) => return (None, None),
    };

    if let Some(id_token) = json
        .get("tokens")
        .and_then(|tokens| tokens.get("id_token"))
        .and_then(|value| value.as_str())
    {
        return parse_id_token_claims(id_token);
    }

    (None, None)
}

/// The decoded (unverified) claims of a JWT id token; `None` when malformed.
pub fn id_token_claims(id_token: &str) -> Option<Value> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice(&payload).ok()
}

fn parse_id_token_claims(id_token: &str) -> (Option<String>, Option<String>) {
    let Some(json) = id_token_claims(id_token) else {
        return (None, None);
    };

    let email = json
        .get("email")
        .and_then(|value| value.as_str())
        .map(String::from);
    let plan_type = json
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_plan_type"))
        .and_then(|value| value.as_str())
        .map(String::from);

    (email, plan_type)
}

pub fn detect_source_label(raw_auth_json: Option<&str>, raw_config_toml: Option<&str>) -> String {
    if let Some(config) = raw_config_toml {
        let lower = config.to_ascii_lowercase();
        if lower.contains("base_url")
            || lower.contains("model_provider")
            || lower.contains("[model_providers.")
        {
            return "third-party".to_string();
        }
        if lower.contains("cli_auth_credentials_store") {
            return "official".to_string();
        }
    }

    if let Some(auth) = raw_auth_json {
        if let Ok(json) = serde_json::from_str::<Value>(auth) {
            if json
                .get("tokens")
                .and_then(|value| value.as_object())
                .is_some()
            {
                return "official".to_string();
            }
            if json.get("OPENAI_API_KEY").is_some() || json.get("openai_api_key").is_some() {
                return "api-key".to_string();
            }
        }
    }

    "custom".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_profile_dir() -> PathBuf {
        std::env::temp_dir().join(format!("cutex-profile-import-{}", Uuid::new_v4()))
    }

    #[test]
    fn import_copies_and_portabilizes_a_relative_model_catalog() {
        let dir = temp_profile_dir();
        fs::create_dir_all(&dir).expect("profile directory");
        let auth_path = dir.join("auth.json");
        let config_path = dir.join("config.toml");
        let catalog_path = dir.join("custom-models.json");
        fs::write(&auth_path, r#"{"OPENAI_API_KEY":"test"}"#).expect("auth");
        fs::write(
            &config_path,
            r#"
model = "custom-model"
model_provider = "custom"
model_catalog_json = "custom-models.json"

[model_providers.custom]
name = "Custom"
base_url = "https://example.test/"
"#,
        )
        .expect("config");
        fs::write(&catalog_path, r#"{"models":[]}"#).expect("catalog");

        let snapshot = import_snapshot(
            auth_path.to_str().expect("auth path"),
            Some(config_path.to_str().expect("config path")),
        )
        .expect("profile import");

        let imported_config = snapshot
            .raw_config_toml
            .as_deref()
            .expect("profile config should be imported");
        assert_eq!(
            parse_toml_table(imported_config)
                .expect("imported config should parse")
                .get("model_catalog_json")
                .and_then(toml::Value::as_str),
            Some(PROFILE_LOCAL_MODEL_CATALOG_FILE)
        );
        assert_eq!(
            snapshot.raw_model_catalog_json.as_deref(),
            Some(r#"{"models":[]}"#)
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn import_rejects_a_missing_or_invalid_referenced_model_catalog() {
        let dir = temp_profile_dir();
        fs::create_dir_all(&dir).expect("profile directory");
        let auth_path = dir.join("auth.json");
        let config_path = dir.join("config.toml");
        fs::write(&auth_path, "{}").expect("auth");
        fs::write(&config_path, "model_catalog_json = \"missing.json\"\n").expect("config");

        let error = import_snapshot(
            auth_path.to_str().expect("auth path"),
            Some(config_path.to_str().expect("config path")),
        )
        .expect_err("missing catalog should fail");
        assert!(error.to_string().contains("Failed to read model catalog"));

        fs::write(dir.join("missing.json"), "not-json").expect("invalid catalog");
        let error = import_snapshot(
            auth_path.to_str().expect("auth path"),
            Some(config_path.to_str().expect("config path")),
        )
        .expect_err("invalid catalog should fail");
        assert!(error.to_string().contains("Failed to parse model catalog"));
        let _ = fs::remove_dir_all(dir);
    }
}
