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
    use crate::asks::budget::AskLimits;
    use crate::profiles::model::ManagementApiToken;
    use serde_json::json;
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

    #[test]
    fn asks_limits_survive_typed_config_mutations_and_remain_effective() {
        let path = temp_config_path();
        let extension = json!({
            "limits": {
                "autoTurnsPerHour": 100000,
                "childrenPerAgent": 1000,
                "childrenPerDomain": 2048,
                "agentStartedOnlinePerDomain": 256,
                "creationDepth": 3,
                "stopOfflineGraceSecs": 17,
                "futureLimit": {"enabled": false, "values": [null, 0, "keep"]}
            },
            "futureSetting": {"nested": [true, "preserve"]}
        });
        write_private_pretty_json_atomic(
            &path,
            &json!({"auto_thread_titles": true, "asks": extension.clone()}),
            "fixture config",
        ).expect("write config with raw asks limits");
        let expected = AskLimits {
            children_per_agent: 1000,
            creation_depth: 3,
            children_per_domain: 2048,
            agent_started_online_per_domain: 256,
            auto_turns_per_hour: 100000,
            stop_offline_grace_secs: 17,
        };
        assert_eq!(AskLimits::load_from_path(&path), expected);

        for auto_thread_titles in [false, true] {
            let mut config = load_codez_config_from_path(&path).expect("read typed config");
            assert_eq!(config.asks.as_ref(), Some(&extension));
            // Real typed setters read config, mutate an unrelated field and save.
            config.auto_thread_titles = auto_thread_titles;
            config.management_port = Some(24870);
            save_codez_config_to_path(&path, &config).expect("save typed mutation");
            let saved: serde_json::Value = serde_json::from_slice(
                &fs::read(&path).expect("read saved config")
            ).expect("decode saved config");
            assert_eq!(saved["asks"], extension);
            assert_eq!(saved["auto_thread_titles"], auto_thread_titles);
            assert_eq!(saved["management_port"], 24870);
            let reloaded = load_codez_config_from_path(&path).expect("reload typed mutation");
            assert_eq!(reloaded.asks.as_ref(), Some(&extension));
            assert_eq!(reloaded.auto_thread_titles, auto_thread_titles);
            assert_eq!(AskLimits::load_from_path(&path), expected);
        }
        fs::remove_dir_all(path.parent().expect("config parent")).expect("remove config parent");
    }

    #[test]
    fn asks_limits_round_trip_preserves_defaults_unknowns_and_lenient_invalid_values() {
        let path = temp_config_path();
        let defaults = AskLimits::default();
        let cases = [
            (json!({}), defaults.clone()),
            (json!({"asks": {}}), defaults.clone()),
            (json!({"asks": null}), defaults.clone()),
            (json!({"asks": ["future", null]}), defaults.clone()),
            (json!({"asks": "not an object"}), defaults.clone()),
            (json!({"asks": {"limits": null}}), defaults.clone()),
            (json!({"asks": {"limits": []}}), defaults.clone()),
            (json!({"asks": {"limits": {"futureOnly": {"x": 1}}}}), defaults.clone()),
            (json!({"asks": {"limits": {"autoTurnsPerHour": 0, "childrenPerAgent": 0,
                                        "futureLimit": "retained"}}}), AskLimits {
                auto_turns_per_hour: 0,
                children_per_agent: 0,
                ..defaults.clone()
            }),
            // Keep r264's whole-limits fallback on any invalid known field;
            // valid siblings must not silently acquire different semantics.
            (json!({"asks": {"limits": {"autoTurnsPerHour": 1234, "childrenPerAgent": "bad"}}}), defaults.clone()),
            (json!({"asks": {"limits": {"autoTurnsPerHour": -1}}}), defaults.clone()),
            (json!({"asks": {"limits": {"autoTurnsPerHour": 1.5}}}), defaults.clone()),
        ];
        for (raw, expected) in cases {
            write_private_pretty_json_atomic(&path, &raw, "fixture config")
                .expect("write raw compatibility config");
            assert_eq!(AskLimits::load_from_path(&path), expected, "before save: {raw}");
            let mut typed = load_codez_config_from_path(&path)
                .expect("asks extension must not break unrelated typed config");
            typed.auto_thread_titles = false;
            save_codez_config_to_path(&path, &typed).expect("save compatibility config");
            let saved: serde_json::Value = serde_json::from_slice(
                &fs::read(&path).expect("read compatibility config")
            ).expect("decode compatibility config");
            // get() distinguishes a missing extension from explicit JSON null.
            assert_eq!(saved.get("asks"), raw.get("asks"), "raw extension changed: {raw}");
            let reloaded = load_codez_config_from_path(&path).expect("reload compatibility config");
            assert!(!reloaded.auto_thread_titles);
            assert_eq!(serde_json::to_value(reloaded).unwrap().get("asks"), raw.get("asks"));
            assert_eq!(AskLimits::load_from_path(&path), expected, "after save: {raw}");
        }
        fs::remove_dir_all(path.parent().expect("config parent")).expect("remove config parent");
    }

    #[test]
    fn absent_asks_does_not_materialize_default_limits_and_parser_fallback_is_unchanged() {
        let path = temp_config_path();
        assert_eq!(AskLimits::load_from_path(&path), AskLimits::default());
        save_codez_config_to_path(&path, &CodezConfig::default()).expect("save default config");
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.get("asks").is_none());
        assert_eq!(AskLimits::load_from_path(&path), AskLimits::default());
        // Invalid whole-file JSON still gets the existing defaults at the
        // budget reader and an error at the checked typed reader.
        fs::write(&path, b"{ invalid config").expect("write malformed fixture config");
        assert_eq!(AskLimits::load_from_path(&path), AskLimits::default());
        assert!(load_codez_config_from_path(&path).is_err());
        assert_eq!(AskLimits::default().auto_turns_per_hour, 60);
        fs::remove_dir_all(path.parent().expect("config parent")).expect("remove config parent");
    }
}
