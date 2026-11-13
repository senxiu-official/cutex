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
use fs2::FileExt;

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
    let guard = config_write_lock(&path)?;
    save_codez_config_to_path(&path, config)?;
    drop(guard);
    crate::management::v2::host_files::refresh_secret_path_cache();
    Ok(())
}

/// Apply a mutation to the latest checked config while holding its stable
/// cross-process lock. Callbacks must only edit/validate config: do not call
/// another config writer or acquire a profile/store lock here. Acquire those
/// locks before entering this API, and do follow-up IO after it returns.
/// Complete snapshot replacement (`save_codez_config`) is for initialization
/// and fixtures; read/modify/write callers must use this entry point.
pub fn update_codez_config<T>(change: impl FnOnce(&mut CodezConfig) -> anyhow::Result<T>) -> anyhow::Result<(CodezConfig, T)> {
    begin_codez_config_update()?.commit(change)
}

/// Typed contention is distinct from invalid configuration or invalid PATCH
/// input, and remains recognizable through anyhow context wrappers.
#[derive(Debug)]
pub struct ConfigBusy { timeout_ms: u128 }
impl std::fmt::Display for ConfigBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "config_busy: global config writer lock timed out after {} ms; retry when the other writer finishes", self.timeout_ms)
    }
}
impl std::error::Error for ConfigBusy {}

pub fn is_config_busy(error: &anyhow::Error) -> bool { error.downcast_ref::<ConfigBusy>().is_some() }

/// Cross-store profile operations acquire this after their lifecycle lock,
/// before changing any profile/session state. Commit does not reacquire it.
pub(crate) struct ConfigUpdate {
    path: std::path::PathBuf,
    guard: fs::File,
    config: CodezConfig,
    before: Vec<u8>,
}

pub(crate) fn begin_codez_config_update() -> anyhow::Result<ConfigUpdate> {
    ConfigUpdate::open(&config_path()?)
}

impl ConfigUpdate {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let guard = config_write_lock(path)?;
        let config = load_codez_config_from_path(path)?;
        let before = serde_json::to_vec(&config)?;
        Ok(Self { path: path.to_path_buf(), guard, config, before })
    }

    pub(crate) fn current(&self) -> &CodezConfig { &self.config }

    pub(crate) fn commit<T>(self, change: impl FnOnce(&mut CodezConfig) -> anyhow::Result<T>) -> anyhow::Result<(CodezConfig, T)> {
        let result = self.commit_inner(change)?;
        crate::management::v2::host_files::refresh_secret_path_cache();
        Ok(result)
    }

    fn commit_inner<T>(mut self, change: impl FnOnce(&mut CodezConfig) -> anyhow::Result<T>) -> anyhow::Result<(CodezConfig, T)> {
        let result = change(&mut self.config)?;
        if serde_json::to_vec(&self.config)? != self.before {
            save_codez_config_to_path(&self.path, &self.config)?;
        }
        drop(self.guard);
        Ok((self.config, result))
    }
}

pub(crate) fn config_write_lock(path: &Path) -> anyhow::Result<fs::File> {
    let parent = path.parent().context("config path has no parent")?;
    crate::management::client_tokens::ensure_private_dir(parent)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // A separate, never-replaced inode locks the complete read/modify/write,
    // not the config inode which atomic publication replaces underneath it.
    let guard = options.open(path.with_extension("json.lock"))?;
    acquire_config_lock(&guard, std::time::Duration::from_secs(2))?;
    Ok(guard)
}

fn acquire_config_lock(file: &fs::File, timeout: std::time::Duration) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::WouldBlock || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                if std::time::Instant::now() >= deadline {
                    return Err(ConfigBusy { timeout_ms: timeout.as_millis() }.into());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("lock global config"),
        }
    }
}

#[cfg(test)]
fn update_codez_config_at<T>(path: &Path, change: impl FnOnce(&mut CodezConfig) -> anyhow::Result<T>) -> anyhow::Result<(CodezConfig, T)> {
    ConfigUpdate::open(path)?.commit_inner(change)
}

fn save_codez_config_to_path(path: &Path, config: &CodezConfig) -> anyhow::Result<()> {
    write_private_pretty_json_atomic(path, config, "config file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::budget::AskLimits;
    use crate::profiles::model::ManagementApiToken;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn held_config_lock_times_out_without_changing_bytes_and_then_recovers() {
        let path = temp_config_path();
        save_codez_config_to_path(&path, &CodezConfig::default()).unwrap();
        let before = fs::read(&path).unwrap();
        let held = config_write_lock(&path).unwrap();
        let other = fs::OpenOptions::new().read(true).write(true).open(path.with_extension("json.lock")).unwrap();
        let error = acquire_config_lock(&other, std::time::Duration::from_millis(25)).unwrap_err();
        assert!(error.to_string().contains("config_busy"));
        assert!(is_config_busy(&error.context("outer operation")));
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(held);
        acquire_config_lock(&other, std::time::Duration::from_millis(25)).unwrap();
        drop(other);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    #[ignore = "owned subprocess fixture for config writer contention"]
    fn private_config_mutator() {
        let path = std::path::PathBuf::from(std::env::var_os("CUTEX_CONFIG_MUTATOR_FIXTURE").unwrap());
        assert!(path.parent().unwrap().canonicalize().unwrap().starts_with(std::env::temp_dir().canonicalize().unwrap()));
        let slot = std::env::var("CUTEX_CONFIG_MUTATOR_SLOT").unwrap();
        fs::write(path.with_extension(format!("ready-{slot}")), []).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !path.with_extension("go").exists() {
            assert!(std::time::Instant::now() < deadline, "config writer start timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for _ in 0..40 {
            update_codez_config_at(&path, |config| {
                let field = if slot == "0" { &mut config.agent_bus_port } else { &mut config.management_port };
                *field = Some(field.unwrap_or(0) + 1);
                Ok(())
            }).unwrap();
        }
    }

    #[test]
    fn separate_process_config_writers_preserve_both_fields() {
        let _env = crate::config::paths::test_home_env_lock();
        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut children = Vec::new();
        for slot in 0..2 {
            children.push(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "config::store::tests::private_config_mutator", "--ignored", "--test-threads=1"])
                .env("CUTEX_CONFIG_MUTATOR_FIXTURE", &path)
                .env("CUTEX_CONFIG_MUTATOR_SLOT", slot.to_string())
                .stdout(std::process::Stdio::null())
                .spawn().unwrap());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while (0..2).any(|slot| !path.with_extension(format!("ready-{slot}")).exists()) {
            if std::time::Instant::now() >= deadline {
                for child in &mut children { let _ = child.kill(); let _ = child.wait(); }
                panic!("owned config writer did not start");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        fs::write(path.with_extension("go"), []).unwrap();
        for child in &mut children {
            loop {
                if let Some(status) = child.try_wait().unwrap() { assert!(status.success()); break; }
                if std::time::Instant::now() >= deadline {
                    // Exact Child handles from the two spawns above; no tree traversal.
                    let _ = child.kill(); let _ = child.wait();
                    panic!("owned config writer timed out");
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let config = load_codez_config_from_path(&path).unwrap();
        assert_eq!(config.agent_bus_port, Some(40));
        assert_eq!(config.management_port, Some(40));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_config_mutations_serialize_and_preserve_both_changes() {
        use std::sync::mpsc;
        use std::time::Duration;
        let path = temp_config_path();
        let first_path = path.clone();
        let (entered, first_entered) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let first = std::thread::spawn(move || update_codez_config_at(&first_path, |config| {
            config.agent_bus_port = Some(24910);
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        }).unwrap());
        first_entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let second_path = path.clone();
        let (entered, second_entered) = mpsc::channel();
        let second = std::thread::spawn(move || update_codez_config_at(&second_path, |config| {
            entered.send(()).unwrap();
            config.agent_bus_enabled = true;
            Ok(())
        }).unwrap());
        let early = second_entered.recv_timeout(Duration::from_millis(150));
        release.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert!(early.is_err(), "second callback must not read a stale config while the first owns the writer lock");
        let config = load_codez_config_from_path(&path).unwrap();
        assert_eq!(config.agent_bus_port, Some(24910));
        assert!(config.agent_bus_enabled);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_config_mutation_preserves_original_and_releases_lock() {
        let path = temp_config_path();
        save_codez_config_to_path(&path, &CodezConfig::default()).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(update_codez_config_at::<()>(&path, |config| {
            config.agent_bus_port = Some(24910);
            anyhow::bail!("injected validation failure")
        }).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        update_codez_config_at(&path, |config| { config.agent_bus_enabled = true; Ok(()) }).unwrap();
        assert!(load_codez_config_from_path(&path).unwrap().agent_bus_enabled);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

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
