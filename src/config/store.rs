//! Persistence for cutex global config and quick-run state.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::config::atomic::write_pretty_json_atomic;
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::config_path;
use crate::config::paths::quick_state_path;
use crate::profiles::model::CodezConfig;
use crate::profiles::model::QuickRunState;
use anyhow::Context;

pub fn load_quick_state() -> QuickRunState {
    let path = match quick_state_path() {
        Ok(p) => p,
        Err(_) => return QuickRunState::default(),
    };
    let data = match fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return QuickRunState::default(),
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_quick_state(state: &QuickRunState) -> anyhow::Result<()> {
    let path = quick_state_path()?;
    write_pretty_json_atomic(&path, state, "state file")
}

pub fn load_codez_config() -> CodezConfig {
    let path = match config_path() {
        Ok(p) => p,
        Err(_) => return CodezConfig::default(),
    };
    let data = match fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return CodezConfig::default(),
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn load_codez_config_checked() -> anyhow::Result<CodezConfig> {
    load_codez_config_from_path(&config_path()?)
}

fn load_codez_config_from_path(path: &Path) -> anyhow::Result<CodezConfig> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(CodezConfig::default()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read config file: {}", path.display()))
        }
    };
    serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))
}

pub fn save_codez_config(config: &CodezConfig) -> anyhow::Result<()> {
    let path = config_path()?;
    save_codez_config_to_path(&path, config)
}

fn save_codez_config_to_path(path: &Path, config: &CodezConfig) -> anyhow::Result<()> {
    write_private_pretty_json_atomic(path, config, "config file")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::ManagementApiToken;
    use uuid::Uuid;

    fn temp_config_path() -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("cutex-config-store-{}", Uuid::new_v4()))
            .join("config.json")
    }

    #[test]
    fn checked_load_defaults_only_when_config_is_missing() {
        let path = temp_config_path();

        let loaded = load_codez_config_from_path(&path).expect("missing config should default");
        assert_eq!(
            serde_json::to_value(loaded).expect("serialize loaded config"),
            serde_json::to_value(CodezConfig::default()).expect("serialize default config")
        );
    }

    #[test]
    fn checked_load_reports_invalid_json_without_replacing_it() {
        let path = temp_config_path();
        fs::create_dir_all(path.parent().expect("config parent")).expect("create config parent");
        fs::write(&path, "{ invalid config").expect("write invalid config");

        let error = load_codez_config_from_path(&path).expect_err("invalid config should fail");

        assert!(error.to_string().contains("Failed to parse config file"));
        assert_eq!(
            fs::read_to_string(&path).expect("read invalid config"),
            "{ invalid config"
        );
        fs::remove_dir_all(path.parent().expect("config parent")).expect("remove config parent");
    }

    #[test]
    fn legacy_config_without_management_token_parses() {
        let loaded: CodezConfig =
            serde_json::from_str(r#"{"agent_bus_enabled":true,"agent_bus_token":"legacy-bus"}"#)
                .expect("legacy config should parse");

        assert!(loaded.management_api_token.is_none());
        assert_eq!(loaded.agent_bus_token.as_deref(), Some("legacy-bus"));
    }

    #[test]
    fn management_token_round_trips_in_a_private_file_and_stays_redacted() {
        let path = temp_config_path();
        let raw = "fixture-private-management-root";
        let config = CodezConfig {
            management_api_token: Some(ManagementApiToken::new(raw)),
            agent_bus_token: Some("fixture-agent-bus-root".to_string()),
            ..CodezConfig::default()
        };

        save_codez_config_to_path(&path, &config).expect("save private config");
        let loaded = load_codez_config_from_path(&path).expect("reload private config");

        assert_eq!(
            loaded
                .management_api_token
                .as_ref()
                .map(ManagementApiToken::as_str),
            Some(raw)
        );
        assert_eq!(
            loaded.agent_bus_token.as_deref(),
            Some("fixture-agent-bus-root")
        );
        assert!(!format!("{loaded:?}").contains(raw));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&path)
                    .expect("config metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(path.parent().expect("config parent")).expect("remove config parent");
    }
}
