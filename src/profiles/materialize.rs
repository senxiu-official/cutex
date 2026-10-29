//! Materialized profile file paths and pure profile-file helpers.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use chrono::Utc;

use crate::config::atomic::write_private_bytes_atomic;
use crate::config::paths::config_dir;
use crate::config::paths::host_codex_home_dir;
use crate::config::proxy::effective_proxy_config;
use crate::config::store::load_codez_config;
use crate::config::text::read_optional_text;
use crate::config::text::write_optional_text_if_changed;
use crate::profiles::model::AuthData;
use crate::profiles::model::CliKind;
use crate::profiles::model::CodezConfig;
use crate::profiles::model::CustomStatusItemCatalogEntry;
use crate::profiles::model::CustomStatusItemRender;
use crate::profiles::model::CustomStatusItemSource;
use crate::profiles::model::CustomStatusItemStyle;
use crate::profiles::model::CustomStatusItemsCatalogFile;
use crate::profiles::model::ImportedSnapshot;
use crate::profiles::model::MaterializedAccountFiles;
use crate::profiles::model::StoredAccount;
use crate::profiles::model_discovery::apply_known_model_metadata_overlays_to_catalog;
use crate::profiles::profile_config::extract_profile_config_toml;
use crate::profiles::profile_config::merge_and_write_config_toml;
use crate::profiles::profile_config::normalize_profile_config_for_account;
use crate::profiles::profile_config::parse_toml_table;
use crate::profiles::profile_config::profile_model_catalog_setting;
use crate::profiles::profile_config::profile_uses_local_model_catalog;

pub fn materialized_profiles_dir() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("profiles"))
}

pub fn materialized_account_files(
    account: &StoredAccount,
) -> anyhow::Result<MaterializedAccountFiles> {
    let dir = materialized_profiles_dir()?.join(&account.id);
    Ok(MaterializedAccountFiles {
        auth_path: dir.join("auth.json"),
        config_path: dir.join("config.toml"),
        model_catalog_path: dir.join("models.json"),
        // Keep legacy catalogs intact for already-running owners and their receipts.
        custom_status_items_path: dir.join("cutex-status-items.json"),
    })
}

fn default_custom_status_items() -> Vec<CustomStatusItemCatalogEntry> {
    vec![
        CustomStatusItemCatalogEntry {
            animation: None,
            id: "cutex_welcome".to_string(),
            title: "Cutex Welcome".to_string(),
            description: None,
            source: CustomStatusItemSource::Static {
                value: "Bon voyage !".to_string(),
            },
            render: CustomStatusItemRender::Value,
            style: CustomStatusItemStyle {
                fg: Some("#F6A3C8".to_string()),
                bg: None,
                bold: true,
                dim: false,
                italic: false,
                underlined: false,
            },
        },
        CustomStatusItemCatalogEntry {
            animation: None,
            id: "cutex_profile".to_string(),
            title: "Cutex Profile".to_string(),
            description: None,
            source: CustomStatusItemSource::LaunchProfile,
            render: CustomStatusItemRender::Value,
            style: CustomStatusItemStyle {
                fg: Some("#FFFFFF".to_string()),
                bg: None,
                bold: true,
                dim: false,
                italic: false,
                underlined: false,
            },
        },
    ]
}

pub fn normalize_custom_status_items(
    items: &[CustomStatusItemCatalogEntry],
) -> Vec<CustomStatusItemCatalogEntry> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    let defaults = default_custom_status_items();

    for item in items.iter().chain(defaults.iter()) {
        let id = crate::launch::selected_status::canonical_id(item.id.trim());
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }

        let title = match (id, item.title.trim()) {
            ("cutex_welcome", "Bon voyage") => "Cutex Welcome",
            ("cutex_profile", "Profile") => "Cutex Profile",
            (_, title) => title,
        };
        normalized.push(CustomStatusItemCatalogEntry {
            animation: item.animation.clone(),
            id: id.to_string(),
            title: if title.is_empty() {
                id.to_string()
            } else {
                title.to_string()
            },
            description: item
                .description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            source: item.source.clone(),
            render: item.render.clone(),
            style: item.style.clone(),
        });
    }

    normalized
}

pub fn custom_status_items_catalog_json(config: &CodezConfig) -> anyhow::Result<Option<String>> {
    let items = normalize_custom_status_items(&config.custom_status_items);
    if items.is_empty() {
        return Ok(None);
    }

    Ok(Some(serde_json::to_string_pretty(
        &CustomStatusItemsCatalogFile { items },
    )?))
}

pub fn set_materialized_file_permissions(_files: &MaterializedAccountFiles) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        // A freshly created (logged-out) profile has no auth.json yet.
        if _files.auth_path.exists() {
            fs::set_permissions(&_files.auth_path, perms.clone())?;
        }
        if _files.config_path.exists() {
            fs::set_permissions(&_files.config_path, perms.clone())?;
        }
        if _files.model_catalog_path.exists() {
            fs::set_permissions(&_files.model_catalog_path, perms.clone())?;
        }
        if _files.custom_status_items_path.exists() {
            fs::set_permissions(&_files.custom_status_items_path, perms)?;
        }
    }
    #[cfg(windows)]
    {
        let parent = _files.auth_path.parent().context("profile auth parent missing")?;
        let (_, identity) = crate::platform::private_fs::secure_directory(parent)?;
        for path in [&_files.auth_path, &_files.config_path, &_files.model_catalog_path, &_files.custom_status_items_path] {
            if path.exists() {
                let name = path.file_name().and_then(|s| s.to_str()).context("invalid profile file name")?;
                drop(crate::platform::private_fs::open_child(parent, identity, name, libc::O_RDONLY, true)?);
            }
        }
    }

    Ok(())
}

/// Limit every newly created component at mkdir time, including `profiles`.
/// Existing directories keep their permissions; health reports unsafe modes.
pub fn create_private_dir_all(path: &Path) -> anyhow::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
        .with_context(|| format!("Failed to create account dir: {}", path.display()))?;
    Ok(())
}

pub fn ensure_materialized_account_dir(files: &MaterializedAccountFiles) -> anyhow::Result<()> {
    if let Some(parent) = files.auth_path.parent() {
        create_private_dir_all(parent)?;
    }
    Ok(())
}

pub fn ensure_materialized_account_files(
    account: &StoredAccount,
) -> anyhow::Result<MaterializedAccountFiles> {
    ensure_materialized_account_files_inner(account, true)
}

/// Re-materialize the profile's config-derived files (config.toml, local
/// model catalog, status items, permissions) without requiring a usable
/// auth.json. Settings edits use this so a profile that is not logged in
/// yet (or has no API key yet) can still be configured; launches keep using
/// [`ensure_materialized_account_files`], which validates the credentials.
pub fn refresh_materialized_account_config(
    account: &StoredAccount,
) -> anyhow::Result<MaterializedAccountFiles> {
    ensure_materialized_account_files_inner(account, false)
}

fn ensure_materialized_account_files_inner(
    account: &StoredAccount,
    check_auth: bool,
) -> anyhow::Result<MaterializedAccountFiles> {
    let files = materialized_account_files(account)?;
    ensure_materialized_account_dir(&files)?;

    if check_auth {
        if !files.auth_path.exists() {
            anyhow::bail!(
                "Profile '{}' is missing auth.json at {}. Re-import or restore this profile file.",
                account.name,
                files.auth_path.display()
            );
        }
        read_and_validate_account_auth(account, &files.auth_path)?;
    }

    let profile_config = read_optional_text(&files.config_path)?
        .map(|existing| {
            extract_profile_config_toml(&existing).with_context(|| {
                format!(
                    "Failed to parse existing config.toml for profile '{}' at {}",
                    account.name,
                    files.config_path.display()
                )
            })
        })
        .transpose()?
        .flatten();
    let profile_config = normalize_profile_config_for_account(account, profile_config)?;
    let codez_config = load_codez_config();
    merge_and_write_config_toml(
        &files.config_path,
        profile_config.as_deref(),
        effective_proxy_config(account, &codez_config)
            .map(|proxy| proxy.enabled)
            .unwrap_or(false),
    )?;
    ensure_profile_local_model_catalog(account, &files, profile_config.as_deref())?;
    let custom_status_items_json = custom_status_items_catalog_json(&codez_config)?;
    write_optional_text_if_changed(
        &files.custom_status_items_path,
        custom_status_items_json.as_deref(),
    )?;
    set_materialized_file_permissions(&files)?;
    Ok(files)
}

pub fn validate_materialized_account_files(
    account: &StoredAccount,
) -> anyhow::Result<MaterializedAccountFiles> {
    let files = materialized_account_files(account)?;
    validate_materialized_account_files_at(account, &files)?;
    Ok(files)
}

fn validate_materialized_account_files_at(
    account: &StoredAccount,
    files: &MaterializedAccountFiles,
) -> anyhow::Result<()> {
    read_and_validate_account_auth(account, &files.auth_path)?;

    let config_toml = fs::read_to_string(&files.config_path).with_context(|| {
        format!(
            "Profile '{}' is missing or cannot read config.toml at {}. Open the profile editor to materialize it before using a one-launch override.",
            account.name,
            files.config_path.display()
        )
    })?;
    toml::from_str::<toml::Value>(&config_toml).with_context(|| {
        format!(
            "Failed to parse config.toml for profile '{}' at {}",
            account.name,
            files.config_path.display()
        )
    })?;

    if profile_references_profile_local_model_catalog(&config_toml, &files.model_catalog_path)? {
        read_and_validate_model_catalog(account, files)?;
    }

    if files.custom_status_items_path.exists() {
        let custom_status_items = fs::read_to_string(&files.custom_status_items_path)
            .with_context(|| {
                format!(
                    "Failed to read cutex-status-items.json for profile '{}' at {}",
                    account.name,
                    files.custom_status_items_path.display()
                )
            })?;
        serde_json::from_str::<CustomStatusItemsCatalogFile>(&custom_status_items).with_context(
            || {
                format!(
                    "Failed to parse cutex-status-items.json for profile '{}' at {}",
                    account.name,
                    files.custom_status_items_path.display()
                )
            },
        )?;
    }

    Ok(())
}

fn read_and_validate_account_auth(
    account: &StoredAccount,
    auth_path: &Path,
) -> anyhow::Result<serde_json::Value> {
    let auth_json = fs::read_to_string(auth_path).with_context(|| {
        format!(
            "Profile '{}' is missing or cannot read auth.json at {}. Re-import or restore this profile file.",
            account.name,
            auth_path.display()
        )
    })?;
    let auth = serde_json::from_str::<serde_json::Value>(&auth_json).with_context(|| {
        format!(
            "Failed to parse auth.json for profile '{}' at {}",
            account.name,
            auth_path.display()
        )
    })?;
    validate_account_auth_payload(account, auth_path, &auth)?;
    Ok(auth)
}

fn validate_account_auth_payload(
    account: &StoredAccount,
    auth_path: &Path,
    auth: &serde_json::Value,
) -> anyhow::Result<()> {
    if account.source.as_deref() != Some("api-key") {
        return Ok(());
    }

    let has_api_key = auth
        .get("OPENAI_API_KEY")
        .or_else(|| auth.get("openai_api_key"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .is_some_and(|key| !key.is_empty());
    if !has_api_key {
        anyhow::bail!(
            "Profile '{}' is configured for API-key authentication, but {} has no non-empty OPENAI_API_KEY. Re-import the profile or restore its API key.",
            account.name,
            auth_path.display()
        );
    }
    Ok(())
}

pub fn materialize_imported_account_files(
    account: &StoredAccount,
    snapshot: &ImportedSnapshot,
) -> anyhow::Result<()> {
    materialize_new_account_files(
        account,
        Some(&snapshot.raw_auth_json),
        snapshot.raw_config_toml.clone(),
        snapshot.raw_model_catalog_json.as_deref(),
    )
}

/// Write a new profile's materialized files. `raw_auth_json: None` creates a
/// logged-out profile (no auth.json); it then needs a login or an API key
/// before it can launch.
pub fn materialize_new_account_files(
    account: &StoredAccount,
    raw_auth_json: Option<&str>,
    raw_config_toml: Option<String>,
    raw_model_catalog_json: Option<&str>,
) -> anyhow::Result<()> {
    let files = materialized_account_files(account)?;
    ensure_materialized_account_dir(&files)?;

    if let Some(raw_auth_json) = raw_auth_json {
        write_optional_text_if_changed(&files.auth_path, Some(raw_auth_json))?;
    }
    let codez_config = load_codez_config();
    let profile_config = normalize_profile_config_for_account(account, raw_config_toml)?;
    merge_and_write_config_toml(
        &files.config_path,
        profile_config.as_deref(),
        effective_proxy_config(account, &codez_config)
            .map(|proxy| proxy.enabled)
            .unwrap_or(false),
    )?;
    write_optional_text_if_changed(&files.model_catalog_path, raw_model_catalog_json)?;
    ensure_profile_local_model_catalog(account, &files, profile_config.as_deref())?;
    let custom_status_items_json = custom_status_items_catalog_json(&codez_config)?;
    write_optional_text_if_changed(
        &files.custom_status_items_path,
        custom_status_items_json.as_deref(),
    )?;
    set_materialized_file_permissions(&files)?;
    Ok(())
}

pub fn sync_active_codex_home_files(
    account: &StoredAccount,
    files: &MaterializedAccountFiles,
) -> anyhow::Result<()> {
    if account.cli_kind != CliKind::Codex {
        return Ok(());
    }

    let codex_home = host_codex_home_dir()?;
    fs::create_dir_all(&codex_home)
        .with_context(|| format!("Failed to create CODEX_HOME: {}", codex_home.display()))?;

    let active_auth_path = codex_home.join("auth.json");
    let auth_json = fs::read_to_string(&files.auth_path)
        .with_context(|| format!("Failed to read auth.json: {}", files.auth_path.display()))?;
    write_optional_text_if_changed(&active_auth_path, Some(&auth_json))?;

    let profile_config = read_optional_text(&files.config_path)?
        .map(|existing| {
            extract_profile_config_toml(&existing).with_context(|| {
                format!(
                    "Failed to parse existing config.toml for profile '{}' at {}",
                    account.name,
                    files.config_path.display()
                )
            })
        })
        .transpose()?
        .flatten();
    let global_config = load_codez_config();
    merge_and_write_config_toml(
        &codex_home.join("config.toml"),
        profile_config.as_deref(),
        effective_proxy_config(account, &global_config)
            .map(|proxy| proxy.enabled)
            .unwrap_or(false),
    )?;

    let active_model_catalog_path = codex_home.join("models.json");
    let model_catalog = if profile_config
        .as_deref()
        .map(|contents| {
            profile_references_profile_local_model_catalog(contents, &files.model_catalog_path)
        })
        .transpose()?
        .unwrap_or(false)
    {
        Some(read_and_validate_model_catalog(account, files)?)
    } else {
        None
    };
    write_optional_text_if_changed(&active_model_catalog_path, model_catalog.as_deref())?;

    let active_custom_status_items_path = codex_home.join("cutex-status-items.json");
    let custom_status_items = read_optional_text(&files.custom_status_items_path)?;
    write_optional_text_if_changed(
        &active_custom_status_items_path,
        custom_status_items.as_deref(),
    )?;

    set_active_codex_home_file_permissions(
        &active_auth_path,
        &codex_home.join("config.toml"),
        &active_model_catalog_path,
        &active_custom_status_items_path,
    )?;
    Ok(())
}

fn set_active_codex_home_file_permissions(
    _auth_path: &Path,
    _config_path: &Path,
    _model_catalog_path: &Path,
    _custom_status_items_path: &Path,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        for path in [
            _auth_path,
            _config_path,
            _model_catalog_path,
            _custom_status_items_path,
        ] {
            if path.exists() {
                fs::set_permissions(path, perms.clone())?;
            }
        }
    }
    Ok(())
}

fn ensure_profile_local_model_catalog(
    account: &StoredAccount,
    files: &MaterializedAccountFiles,
    profile_config_toml: Option<&str>,
) -> anyhow::Result<()> {
    let Some(profile_config_toml) = profile_config_toml else {
        return Ok(());
    };
    if !profile_references_profile_local_model_catalog(
        profile_config_toml,
        &files.model_catalog_path,
    )? {
        return Ok(());
    }

    if !files.model_catalog_path.exists() {
        let provider = parse_toml_table(profile_config_toml)?
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        if let Some(catalog) = provider
            .as_deref()
            .and_then(crate::profiles::preset_spec::catalog_for_provider)
        {
            write_optional_text_if_changed(&files.model_catalog_path, Some(&catalog))?;
        } else {
            anyhow::bail!(
                "Profile '{}' references a profile-local models.json but it is missing at {}",
                account.name,
                files.model_catalog_path.display()
            );
        }
    }

    repair_known_model_metadata_overlays(account, files)?;
    read_and_validate_model_catalog(account, files)?;
    Ok(())
}

fn profile_references_profile_local_model_catalog(
    profile_config_toml: &str,
    model_catalog_path: &Path,
) -> anyhow::Result<bool> {
    let Some(setting) = profile_model_catalog_setting(profile_config_toml)? else {
        return Ok(false);
    };
    Ok(profile_uses_local_model_catalog(profile_config_toml)?
        || lexical_catalog_paths_equal(&setting, &model_catalog_path.to_string_lossy()))
}

fn lexical_catalog_paths_equal(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        value
            .replace('\\', "/")
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let left_parts = normalize(left);
    let right_parts = normalize(right);
    let windows_like = left_parts
        .first()
        .or_else(|| right_parts.first())
        .is_some_and(|part| part.contains(':'));
    if windows_like {
        return left_parts.len() == right_parts.len()
            && left_parts
                .iter()
                .zip(right_parts.iter())
                .all(|(left, right)| left.eq_ignore_ascii_case(right));
    }
    left_parts == right_parts
}

fn repair_known_model_metadata_overlays(
    account: &StoredAccount,
    files: &MaterializedAccountFiles,
) -> anyhow::Result<()> {
    let contents = fs::read_to_string(&files.model_catalog_path).with_context(|| {
        format!(
            "Profile '{}' is missing or cannot read models.json at {}",
            account.name,
            files.model_catalog_path.display()
        )
    })?;
    let mut catalog = serde_json::from_str::<serde_json::Value>(&contents).with_context(|| {
        format!(
            "Failed to parse models.json for profile '{}' at {}",
            account.name,
            files.model_catalog_path.display()
        )
    })?;
    if !apply_known_model_metadata_overlays_to_catalog(&mut catalog) {
        return Ok(());
    }
    let encoded = serde_json::to_vec_pretty(&catalog).with_context(|| {
        format!(
            "Failed to serialize models.json for profile '{}' at {}",
            account.name,
            files.model_catalog_path.display()
        )
    })?;
    write_private_bytes_atomic(&files.model_catalog_path, &encoded).with_context(|| {
        format!(
            "Failed to repair models.json for profile '{}' at {}",
            account.name,
            files.model_catalog_path.display()
        )
    })
}

fn read_and_validate_model_catalog(
    account: &StoredAccount,
    files: &MaterializedAccountFiles,
) -> anyhow::Result<String> {
    let contents = fs::read_to_string(&files.model_catalog_path).with_context(|| {
        format!(
            "Profile '{}' is missing or cannot read models.json at {}",
            account.name,
            files.model_catalog_path.display()
        )
    })?;
    serde_json::from_str::<serde_json::Value>(&contents).with_context(|| {
        format!(
            "Failed to parse models.json for profile '{}' at {}",
            account.name,
            files.model_catalog_path.display()
        )
    })?;
    Ok(contents)
}

pub fn legacy_auth_json_from_auth_data(auth: &AuthData) -> anyhow::Result<String> {
    let value = match auth {
        AuthData::ApiKey { key } => serde_json::json!({
            "OPENAI_API_KEY": key,
            "tokens": null,
            "last_refresh": null,
        }),
        AuthData::ChatGPT {
            id_token,
            access_token,
            refresh_token,
            account_id,
        } => serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": Utc::now(),
        }),
    };

    Ok(serde_json::to_string_pretty(&value)?)
}

#[cfg(test)]
mod tests {
    use super::ensure_materialized_account_dir;
    use super::validate_materialized_account_files_at;
    use crate::profiles::model::{CliKind, MaterializedAccountFiles, RuntimeConfig, StoredAccount};
    use std::fs;
    use uuid::Uuid;

    fn test_account() -> StoredAccount {
        StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            auto_pick: false,
            claude_code: Default::default(),
            variants: Default::default(),
            id: "profile-id".to_string(),
            name: "alpha".to_string(),
            email: None,
            plan_type: None,
            plan_label: None,
            quota_preset: Default::default(),
            source: None,
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
            interface_base_urls: Default::default(),
        }
    }

    fn api_key_account() -> StoredAccount {
        StoredAccount {
            source: Some("api-key".to_string()),
            ..test_account()
        }
    }

    fn test_files() -> (std::path::PathBuf, MaterializedAccountFiles) {
        let dir = std::env::temp_dir().join(format!("cutex-profile-validate-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("test profile directory");
        let files = MaterializedAccountFiles {
            auth_path: dir.join("auth.json"),
            config_path: dir.join("config.toml"),
            model_catalog_path: dir.join("models.json"),
            custom_status_items_path: dir.join("cutex-status-items.json"),
        };
        (dir, files)
    }

    #[cfg(unix)]
    #[test]
    fn materialization_preserves_existing_profile_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, files) = test_files();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).expect("widen test mode");
        ensure_materialized_account_dir(&files).expect("existing profile directory");
        super::set_materialized_file_permissions(&files).expect("file permissions");
        let mode = fs::metadata(&dir)
            .expect("profile directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o775);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn materialization_creates_private_ancestors_without_changing_existing_ones() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, mut files) = test_files();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
        let profiles = dir.join("profiles");
        let account = profiles.join("account");
        files.auth_path = account.join("auth.json");
        ensure_materialized_account_dir(&files).unwrap();
        for path in [&profiles, &account] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o775);
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o775)).unwrap();
        ensure_materialized_account_dir(&files).unwrap();
        assert_eq!(fs::metadata(&profiles).unwrap().permissions().mode() & 0o777, 0o775);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn validation_reads_complete_materialization_without_changing_files() {
        let (dir, files) = test_files();
        fs::write(&files.auth_path, r#"{"OPENAI_API_KEY":"test"}"#).expect("auth");
        fs::write(&files.config_path, "model = \"gpt-test\"\n").expect("config");
        fs::write(&files.custom_status_items_path, r#"{"items":[]}"#).expect("status");
        let before = [
            fs::read(&files.auth_path).expect("auth before"),
            fs::read(&files.config_path).expect("config before"),
            fs::read(&files.custom_status_items_path).expect("status before"),
        ];

        validate_materialized_account_files_at(&test_account(), &files)
            .expect("materialization should validate");

        assert_eq!(fs::read(&files.auth_path).expect("auth after"), before[0]);
        assert_eq!(
            fs::read(&files.config_path).expect("config after"),
            before[1]
        );
        assert_eq!(
            fs::read(&files.custom_status_items_path).expect("status after"),
            before[2]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validation_requires_existing_valid_auth_and_config() {
        let (dir, files) = test_files();
        fs::write(&files.auth_path, "not-json").expect("invalid auth");
        fs::write(&files.config_path, "model = \"gpt-test\"\n").expect("config");
        let error = validate_materialized_account_files_at(&test_account(), &files)
            .expect_err("invalid auth must fail");
        assert!(error.to_string().contains("Failed to parse auth.json"));

        fs::write(&files.auth_path, "{}").expect("auth");
        fs::remove_file(&files.config_path).expect("remove config");
        let error = validate_materialized_account_files_at(&test_account(), &files)
            .expect_err("missing config must fail");
        assert!(error
            .to_string()
            .contains("missing or cannot read config.toml"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validation_requires_a_nonempty_key_for_api_key_profiles() {
        let (dir, files) = test_files();
        fs::write(
            &files.auth_path,
            r#"{"OPENAI_API_KEY":null,"tokens":{"refresh_token":"stale"}}"#,
        )
        .expect("auth");
        fs::write(&files.config_path, "model = \"gpt-test\"\n").expect("config");

        let error = validate_materialized_account_files_at(&api_key_account(), &files)
            .expect_err("missing API key must fail before launch");

        assert!(error
            .to_string()
            .contains("configured for API-key authentication"));
        assert!(error.to_string().contains("OPENAI_API_KEY"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validation_accepts_the_legacy_lowercase_api_key_alias() {
        let (dir, files) = test_files();
        fs::write(&files.auth_path, r#"{"openai_api_key":" test-key "}"#).expect("auth");
        fs::write(&files.config_path, "model = \"gpt-test\"\n").expect("config");

        validate_materialized_account_files_at(&api_key_account(), &files)
            .expect("lowercase API key alias should validate");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validation_requires_a_valid_profile_local_model_catalog_when_configured() {
        let (dir, files) = test_files();
        fs::write(&files.auth_path, "{}").expect("auth");
        fs::write(&files.config_path, "model_catalog_json = \"models.json\"\n").expect("config");

        let error = validate_materialized_account_files_at(&test_account(), &files)
            .expect_err("missing catalog must fail");
        assert!(error
            .to_string()
            .contains("missing or cannot read models.json"));

        fs::write(&files.model_catalog_path, "not-json").expect("invalid catalog");
        let error = validate_materialized_account_files_at(&test_account(), &files)
            .expect_err("invalid catalog must fail");
        assert!(error.to_string().contains("Failed to parse models.json"));

        fs::write(&files.model_catalog_path, r#"{"models":[]}"#).expect("valid catalog");
        validate_materialized_account_files_at(&test_account(), &files)
            .expect("valid catalog should pass");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn profile_materialization_repairs_known_glm_flash_vision_metadata() {
        let (dir, files) = test_files();
        let absolute_catalog_path = files
            .model_catalog_path
            .to_string_lossy()
            .replace('\\', "/");
        fs::write(
            &files.config_path,
            format!("model_catalog_json = {absolute_catalog_path:?}\n"),
        )
        .expect("config");
        fs::write(
            &files.model_catalog_path,
            r#"{"models":[{"slug":"glm-5.3","input_modalities":["text"]},{"slug":"glm-5.3-flash","input_modalities":["text"]}]}"#,
        )
        .expect("models");
        let config = fs::read_to_string(&files.config_path).expect("config");

        super::ensure_profile_local_model_catalog(&test_account(), &files, Some(&config))
            .expect("repair profile catalog");
        let repaired = fs::read_to_string(&files.model_catalog_path).expect("repaired models");
        let value: serde_json::Value = serde_json::from_str(&repaired).expect("valid repaired");
        assert_eq!(value["models"][0]["input_modalities"], serde_json::json!(["text"]));
        assert_eq!(
            value["models"][1]["input_modalities"],
            serde_json::json!(["text", "image"])
        );
        let _ = fs::remove_dir_all(dir);
    }
}
