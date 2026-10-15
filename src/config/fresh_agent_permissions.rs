//! One-time migration `fresh-agent-permissions` (owner decision 2026-10-15):
//! a FRESH install creates new agents in the composer's "auto" mode
//! (`workspace-write` + `on-request` + network on, the built-in default since
//! this release), while a home that existed before keeps full access.
//!
//! Rule (decided once per home, recorded in
//! `archive/fresh-agent-permissions/migration.json`):
//!
//! - Runs at the start of EVERY `cutex` process (before any command writes
//!   into the home), so the first process of a brand-new home sees it empty.
//!   A start that finds the record changes nothing: a home this binary
//!   created is never mistaken for an existing one on its second start.
//! - **Existing** home = any of `accounts.json` (profiles), the session
//!   store (`cutex-sessions.sqlite3` / `cutex-sessions.json`), the `runtime`
//!   dir or the legacy `~/.codez-cli` dir is present, or `config.json` holds
//!   a key outside the bootstrap set (see [`BOOTSTRAP_CONFIG_KEYS`]: the
//!   tokens and listener / instance settings the install guide writes before
//!   the first `cutex` command). A bootstrap-only `config.json` is no
//!   evidence: it shows the home was prepared, not used. Every agent
//!   permission field that is not stored explicitly in `config.json`
//!   `new_thread_defaults.agent` is written
//!   with the OLD built-in value (`danger-full-access`, `never`, network
//!   off), so new agents launch exactly as before. The record lists the
//!   previous stored values and what was written.
//! - **Fresh** home = none of those: nothing is written but the record; new
//!   agents follow the new built-in default.
//! - The config edit is done on the raw JSON (unknown fields kept) and before
//!   the record, so an interrupted run is simply repeated (idempotent).
//! - Errors never fail the command; the migration is retried on the next one.
//!   One exception: a FRESH home whose decision cannot be recorded fails the
//!   process ([`FreshDecisionUnrecorded`]) before any command writes into the
//!   home. Otherwise this start would go on to create accounts / sessions,
//!   and the next start would read them as evidence and pin the home to full
//!   access — the unsafe direction. An existing home's failure stays
//!   non-fatal: a retry reaches the same (safe) decision.
//!
//! Rollback: an older binary reads the explicit `new_thread_defaults.agent`
//! values and launches with them (binaries from before `new_thread_defaults`
//! ignore the block and always used full access): existing homes are
//! unaffected. A fresh home rolled back has no explicit values and gets the
//! older built-in (full access) for agents created after the rollback.

use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

pub const RECORD_SCHEMA: &str = "cutex/fresh-agent-permissions-migration/v1";
pub const ARCHIVE_ROOT: &str = "archive/fresh-agent-permissions";
const RECORD_FILE: &str = "migration.json";

/// Files / dirs whose presence marks a home used before this release.
const EVIDENCE: &[&str] = &[
    "accounts.json",
    "cutex-sessions.sqlite3",
    "cutex-sessions.json",
    "runtime",
];

/// `config.json` keys a not-yet-used home may hold: the tokens and listener
/// settings `docs/install/AGENT-INSTALL.md` §5.1 (desktop repo) writes before
/// the first `cutex` command, plus the ports / `instance` block of an
/// independent instance. Any other key (e.g. `new_thread_defaults`) makes
/// `config.json` evidence of an existing home.
pub const BOOTSTRAP_CONFIG_KEYS: &[&str] = &[
    "agent_bus_enabled",
    "agent_bus_token",
    "agent_bus_port",
    "management_api_token",
    "management_port",
    "management_listen",
    "instance",
];

/// Whether `config.json` shows prior use. Missing = no; unreadable,
/// unparsable or not an object = error (the migration is retried).
fn config_is_evidence(config_path: &Path) -> anyhow::Result<bool> {
    let raw = match std::fs::read(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", config_path.display())),
    };
    let config = serde_json::from_slice::<Value>(&raw)
        .with_context(|| format!("parse {}", config_path.display()))?;
    let root = config.as_object().context("config.json is not an object")?;
    Ok(root.keys().any(|key| !BOOTSTRAP_CONFIG_KEYS.contains(&key.as_str())))
}

/// Agent permission defaults before 2026-10-15 (stored key, value).
fn previous_builtin() -> [(&'static str, Value); 3] {
    [
        ("sandbox", json!("danger-full-access")),
        ("approval", json!("never")),
        ("network_access", json!(false)),
    ]
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The record existed: nothing done.
    AlreadyDone,
    Fresh,
    /// Existing home; the fields written (empty = all were explicit already).
    Existing { evidence: Vec<String>, written: Map<String, Value> },
}

/// A fresh home's decision could not be written (see the module docs):
/// the process must stop before it writes anything else into the home.
#[derive(Debug)]
pub struct FreshDecisionUnrecorded;

impl std::fmt::Display for FreshDecisionUnrecorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not record the fresh-agent-permissions decision for this new cutex home; nothing else was written, fix the home's permissions / disk and run cutex again")
    }
}

/// Every `cutex` process calls this first. Never panics; fails only with
/// [`FreshDecisionUnrecorded`] (callers stop then). `quiet` drops the
/// one-line notices on stderr, for commands whose stderr is a machine
/// channel (`cutex session retire|restore --json` writes exactly one JSON
/// object there); the decision is still recorded in the archive record.
pub fn run_on_startup(component: &str, quiet: bool) -> anyhow::Result<()> {
    let result = std::panic::catch_unwind(|| {
        let cutex_dir = crate::config::paths::config_dir()?;
        let legacy = crate::config::paths::legacy_config_dir().ok();
        let legacy_exists = !crate::config::instance::is_isolated()
            && legacy.as_deref().is_some_and(Path::exists);
        run_at(&cutex_dir, legacy_exists, Utc::now())
    });
    match result {
        Ok(Err(error)) if error.downcast_ref::<FreshDecisionUnrecorded>().is_some() => return Err(error),
        _ if quiet => {}
        Ok(Ok(Outcome::Existing { written, .. })) if !written.is_empty() => eprintln!(
            "cutex fresh-agent-permissions: existing home keeps full access for new agents ({})",
            Value::Object(written)
        ),
        Ok(Ok(_)) => {}
        Ok(Err(error)) => eprintln!(
            "cutex fresh-agent-permissions migration: {component}: not run on this start: {error:#}"
        ),
        Err(_) => eprintln!(
            "cutex fresh-agent-permissions migration: {component}: panicked, not run on this start"
        ),
    }
    Ok(())
}

/// Decides and records once for the cutex dir `cutex_dir`.
pub fn run_at(cutex_dir: &Path, legacy_exists: bool, now: DateTime<Utc>) -> anyhow::Result<Outcome> {
    let archive = cutex_dir.join(ARCHIVE_ROOT);
    let record_path = archive.join(RECORD_FILE);
    if record_path.exists() {
        return Ok(Outcome::AlreadyDone);
    }
    let mut evidence: Vec<String> = Vec::new();
    if config_is_evidence(&cutex_dir.join("config.json"))? {
        evidence.push("config.json".into());
    }
    evidence.extend(
        EVIDENCE
            .iter()
            .filter(|name| cutex_dir.join(name).exists())
            .map(|name| name.to_string()),
    );
    if legacy_exists {
        evidence.push(".codez-cli (legacy)".into());
    }
    let mut previous = Value::Null;
    let mut written = Map::new();
    if !evidence.is_empty() {
        let config_path = cutex_dir.join("config.json");
        let mut config = match std::fs::read(&config_path) {
            Ok(raw) => serde_json::from_slice::<Value>(&raw)
                .with_context(|| format!("parse {}", config_path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(error) => return Err(error).with_context(|| format!("read {}", config_path.display())),
        };
        let root = config
            .as_object_mut()
            .context("config.json is not an object")?;
        let defaults = root
            .entry("new_thread_defaults")
            .or_insert_with(|| json!({}));
        if defaults.is_null() {
            *defaults = json!({});
        }
        let agent = defaults
            .as_object_mut()
            .context("new_thread_defaults is not an object")?
            .entry("agent")
            .or_insert_with(|| json!({}));
        if agent.is_null() {
            *agent = json!({});
        }
        previous = agent.clone();
        let agent = agent.as_object_mut().context("new_thread_defaults.agent is not an object")?;
        for (key, value) in previous_builtin() {
            if agent.get(key).is_none_or(Value::is_null) {
                agent.insert(key.to_string(), value.clone());
                written.insert(key.to_string(), value);
            }
        }
        if !written.is_empty() {
            crate::config::atomic::write_private_pretty_json_atomic(&config_path, &config, "config file")?;
        }
    }
    let fresh = evidence.is_empty();
    let home = if fresh { "fresh" } else { "existing" };
    let record = json!({
        "schema": RECORD_SCHEMA,
        "at": now.to_rfc3339_opts(SecondsFormat::Secs, true),
        "home": home,
        "evidence": evidence,
        "previousAgentDefaults": previous,
        "written": Value::Object(written.clone()),
    });
    let written_record = (|| -> anyhow::Result<()> {
        if !cutex_dir.exists() {
            crate::management::client_tokens::ensure_private_dir(cutex_dir)?;
        }
        crate::management::client_tokens::ensure_private_dir(&cutex_dir.join("archive"))?;
        crate::management::client_tokens::ensure_private_dir(&archive)?;
        crate::config::atomic::write_private_pretty_json_atomic(&record_path, &record, "fresh-agent-permissions record")
    })();
    if let Err(error) = written_record {
        return Err(if fresh { error.context(FreshDecisionUnrecorded) } else { error });
    }
    Ok(if evidence.is_empty() {
        Outcome::Fresh
    } else {
        Outcome::Existing { evidence, written }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::new_thread::{NewThreadDefaults, ThreadKind};
    use crate::profiles::model::CodezConfig;

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("fresh-agent-permissions-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn agent_defaults(root: &Path) -> NewThreadDefaults {
        let raw = std::fs::read(root.join("config.json")).unwrap_or_else(|_| b"{}".to_vec());
        serde_json::from_slice::<CodezConfig>(&raw).unwrap().new_thread_defaults
    }

    fn record(root: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(root.join(ARCHIVE_ROOT).join(RECORD_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn a_fresh_home_gets_auto_and_stays_fresh_on_its_second_start() {
        let root = temp_home("fresh").join(".cutex");
        assert_eq!(run_at(&root, false, Utc::now()).unwrap(), Outcome::Fresh);
        assert!(!root.join("config.json").exists());
        assert_eq!(record(&root)["home"], "fresh");
        let agent = agent_defaults(&root).resolve(ThreadKind::Agent);
        assert_eq!((agent.sandbox.as_str(), agent.approval.as_str()), ("workspace-write", "on-request"));
        assert!(agent.network_access);
        // The home now has a config and sessions written by this binary.
        std::fs::write(root.join("config.json"), b"{\"agent_bus_port\": 25360}").unwrap();
        std::fs::write(root.join("cutex-sessions.sqlite3"), b"").unwrap();
        assert_eq!(run_at(&root, false, Utc::now()).unwrap(), Outcome::AlreadyDone);
        assert_eq!(agent_defaults(&root).resolve(ThreadKind::Agent), agent);
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_existing_home_keeps_full_access_once() {
        let root = temp_home("existing");
        let before = json!({"agent_bus_port": 24260, "future_field": {"x": 1},
            "new_thread_defaults": {"quick_chat": {"network_access": true}}});
        std::fs::write(root.join("config.json"), serde_json::to_vec(&before).unwrap()).unwrap();
        let Outcome::Existing { evidence, written } = run_at(&root, false, Utc::now()).unwrap() else {
            panic!("existing home expected");
        };
        assert_eq!(evidence, vec!["config.json"]);
        assert_eq!(written.len(), 3);
        let agent = agent_defaults(&root).resolve(ThreadKind::Agent);
        assert_eq!((agent.sandbox.as_str(), agent.approval.as_str()), ("danger-full-access", "never"));
        assert!(!agent.network_access);
        let raw: Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).unwrap()).unwrap();
        assert_eq!(raw["future_field"], json!({"x": 1}), "unknown fields kept");
        assert_eq!(raw["new_thread_defaults"]["quick_chat"]["network_access"], true);
        let rec = record(&root);
        assert_eq!(rec["home"], "existing");
        assert_eq!(rec["previousAgentDefaults"], json!({}));
        assert_eq!(rec["written"]["sandbox"], "danger-full-access");
        // The owner later picks auto: a later start keeps it.
        let mut changed = raw.clone();
        changed["new_thread_defaults"]["agent"] = json!({"sandbox": "workspace-write", "approval": "on-request"});
        std::fs::write(root.join("config.json"), serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_eq!(run_at(&root, false, Utc::now()).unwrap(), Outcome::AlreadyDone);
        assert_eq!(agent_defaults(&root).resolve(ThreadKind::Agent).sandbox, "workspace-write");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_agent_values_are_kept_and_only_gaps_filled() {
        let root = temp_home("explicit");
        std::fs::write(root.join("accounts.json"), b"{}").unwrap();
        std::fs::write(
            root.join("config.json"),
            serde_json::to_vec(&json!({"new_thread_defaults": {"agent": {"sandbox": "workspace-write"}}})).unwrap(),
        )
        .unwrap();
        let Outcome::Existing { written, .. } = run_at(&root, false, Utc::now()).unwrap() else { panic!() };
        assert_eq!(written.keys().cloned().collect::<Vec<_>>(), vec!["approval", "network_access"]);
        let agent = agent_defaults(&root).resolve(ThreadKind::Agent);
        assert_eq!((agent.sandbox.as_str(), agent.approval.as_str()), ("workspace-write", "never"));
        assert!(!agent.network_access);
        assert_eq!(record(&root)["previousAgentDefaults"], json!({"sandbox": "workspace-write"}));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sessions_or_legacy_dir_without_config_count_as_existing() {
        let root = temp_home("sessions");
        std::fs::write(root.join("cutex-sessions.sqlite3"), b"").unwrap();
        assert!(matches!(run_at(&root, false, Utc::now()).unwrap(), Outcome::Existing { .. }));
        assert_eq!(agent_defaults(&root).resolve(ThreadKind::Agent).sandbox, "danger-full-access");
        std::fs::remove_dir_all(root).unwrap();
        let root = temp_home("legacy");
        assert!(matches!(run_at(&root, true, Utc::now()).unwrap(), Outcome::Existing { .. }));
        assert_eq!(agent_defaults(&root).resolve(ThreadKind::Agent).sandbox, "danger-full-access");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_bootstrap_only_config_is_fresh() {
        // What the install guide writes before the first cutex command
        // (plus listener / instance settings), next to its codex-home.
        for config in [
            json!({"agent_bus_enabled": true, "agent_bus_token": "a", "management_api_token": "b"}),
            json!({"agent_bus_token": "a", "management_api_token": "b",
                "management_listen": ["127.0.0.1:24270", "tailscale"], "agent_bus_port": 25360,
                "management_port": 25370, "instance": {"name": "x"}}),
            json!({}),
        ] {
            let root = temp_home("bootstrap");
            std::fs::create_dir_all(root.join("codex-home")).unwrap();
            std::fs::write(root.join("codex-home").join("config.toml"), b"").unwrap();
            std::fs::write(root.join("config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
            assert_eq!(run_at(&root, false, Utc::now()).unwrap(), Outcome::Fresh, "{config}");
            let rec = record(&root);
            assert_eq!(rec["home"], "fresh");
            assert_eq!(rec["evidence"], json!([]));
            let raw: Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).unwrap()).unwrap();
            assert_eq!(raw, config, "bootstrap config left untouched");
            let agent = agent_defaults(&root).resolve(ThreadKind::Agent);
            assert_eq!((agent.sandbox.as_str(), agent.approval.as_str()), ("workspace-write", "on-request"));
            assert!(agent.network_access);
            // Second start: recorded, nothing changes even with a session store now.
            std::fs::write(root.join("cutex-sessions.sqlite3"), b"").unwrap();
            assert_eq!(run_at(&root, false, Utc::now()).unwrap(), Outcome::AlreadyDone);
            assert_eq!(record(&root)["home"], "fresh");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_config_with_real_settings_is_existing() {
        for config in [
            json!({"management_api_token": "b", "new_thread_defaults": {}}),
            json!({"agent_bus_token": "a", "management_api_token": "b", "default_profile": "p"}),
            json!({"management_api_token": "b", "auto_thread_titles": false}),
        ] {
            let root = temp_home("settings");
            std::fs::write(root.join("config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
            let Outcome::Existing { evidence, .. } = run_at(&root, false, Utc::now()).unwrap() else {
                panic!("existing home expected for {config}");
            };
            assert_eq!(evidence, vec!["config.json"]);
            assert_eq!(agent_defaults(&root).resolve(ThreadKind::Agent).sandbox, "danger-full-access");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn usage_files_next_to_a_bootstrap_config_count_as_existing() {
        let bootstrap = json!({"agent_bus_token": "a", "management_api_token": "b"});
        for (name, dir) in [
            ("accounts.json", false),
            ("cutex-sessions.sqlite3", false),
            ("cutex-sessions.json", false),
            ("runtime", true),
        ] {
            let root = temp_home("usage");
            std::fs::write(root.join("config.json"), serde_json::to_vec(&bootstrap).unwrap()).unwrap();
            if dir {
                std::fs::create_dir_all(root.join(name)).unwrap();
            } else {
                std::fs::write(root.join(name), b"{}").unwrap();
            }
            let Outcome::Existing { evidence, written } = run_at(&root, false, Utc::now()).unwrap() else {
                panic!("existing home expected for {name}");
            };
            assert_eq!(evidence, vec![name.to_string()]);
            assert_eq!(written.len(), 3);
            let raw: Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).unwrap()).unwrap();
            assert_eq!(raw["management_api_token"], "b", "tokens kept");
            assert_eq!(agent_defaults(&root).resolve(ThreadKind::Agent).approval, "never");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn an_unreadable_config_is_left_alone_and_retried() {
        let root = temp_home("broken");
        std::fs::write(root.join("config.json"), b"{ broken").unwrap();
        assert!(run_at(&root, false, Utc::now()).is_err());
        assert!(!root.join(ARCHIVE_ROOT).join(RECORD_FILE).exists());
        assert_eq!(std::fs::read(root.join("config.json")).unwrap(), b"{ broken");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fresh_decision_that_cannot_be_recorded_is_fatal() {
        // `archive` is a file: the record cannot be written.
        let root = temp_home("unrecordable");
        std::fs::write(root.join("archive"), b"").unwrap();
        let error = run_at(&root, false, Utc::now()).unwrap_err();
        assert!(error.downcast_ref::<FreshDecisionUnrecorded>().is_some(), "{error:#}");
        assert!(!root.join("config.json").exists(), "nothing else written");
        // An existing home in the same state: retried, not fatal (its
        // explicit full-access values are already in config.json).
        std::fs::write(root.join("accounts.json"), b"{}").unwrap();
        let error = run_at(&root, false, Utc::now()).unwrap_err();
        assert!(error.downcast_ref::<FreshDecisionUnrecorded>().is_none(), "{error:#}");
        assert_eq!(agent_defaults(&root).resolve(ThreadKind::Agent).sandbox, "danger-full-access");
        std::fs::remove_dir_all(root).unwrap();
    }
}
