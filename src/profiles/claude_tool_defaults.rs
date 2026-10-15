//! One-time migration `claude-tool-defaults` (r225, owner decision
//! 2026-10-11): a Claude profile's write / exec rules default to on, and the
//! values stored before (`options.claude.allow_write` / `allow_exec`, which
//! were always written, default false) are cleared so every existing profile
//! follows the new default.
//!
//! - Runs at the start of `cutex management serve`, before any request. The
//!   record `archive/claude-tool-defaults/migration.json` marks it done; a
//!   start that finds it changes nothing, so a rule the owner sets afterwards
//!   is never cleared again.
//! - A run that changes the accounts file first copies it to
//!   `archive/claude-tool-defaults/accounts.pre-claude-tool-defaults-<UTC>.json`
//!   and names every profile with its old values in the record.
//! - Errors never fail startup; the migration is retried on the next start.
//! - Existing Claude sessions are not rewritten: records written before r225
//!   follow their profile's rule at their next start (`ClaudeSession::
//!   write_explicit` / `exec_explicit` unset).

use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

pub const RECORD_SCHEMA: &str = "cutex/claude-tool-defaults-migration/v1";
const ARCHIVE_ROOT: &str = "archive/claude-tool-defaults";
const RECORD_FILE: &str = "migration.json";

/// One cleared profile: name and the old stored values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileChange {
    pub profile: String,
    pub allow_write: Option<Value>,
    pub allow_exec: Option<Value>,
}

/// `cutex management serve` calls this. Never fails or panics.
pub fn run_on_startup(component: &str) {
    let result = std::panic::catch_unwind(|| {
        let cutex_dir = crate::config::paths::config_dir()?;
        let accounts = crate::config::paths::accounts_path()?;
        run_at(&cutex_dir, &accounts, Utc::now())
    });
    match result {
        Ok(Ok(changes)) => {
            for change in &changes {
                eprintln!(
                    "cutex claude-tool-defaults: profile {}: allow_write {:?}, allow_exec {:?} -> default (on)",
                    change.profile, change.allow_write, change.allow_exec
                );
            }
        }
        Ok(Err(error)) => eprintln!(
            "cutex claude-tool-defaults migration: {component}: not run on this start: {error:#}"
        ),
        Err(_) => eprintln!(
            "cutex claude-tool-defaults migration: {component}: panicked, not run on this start"
        ),
    }
}

/// Clears the stored Claude rules in `store` (the accounts JSON).
pub fn plan(store: &mut Value) -> Vec<ProfileChange> {
    let mut changes = Vec::new();
    let Some(accounts) = store.get_mut("accounts").and_then(Value::as_array_mut) else {
        return changes;
    };
    for account in accounts {
        let profile = account
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(claude) = account
            .get_mut("options")
            .and_then(|options| options.get_mut("claude"))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let allow_write = claude.remove("allow_write");
        let allow_exec = claude.remove("allow_exec");
        if allow_write.is_some() || allow_exec.is_some() {
            changes.push(ProfileChange { profile, allow_write, allow_exec });
        }
    }
    changes
}

/// Migrates the accounts file `accounts` of the cutex dir `cutex_dir` once.
/// Returns the cleared profiles (empty when done before or nothing stored).
pub fn run_at(
    cutex_dir: &Path,
    accounts: &Path,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<ProfileChange>> {
    let archive = cutex_dir.join(ARCHIVE_ROOT);
    let record_path = archive.join(RECORD_FILE);
    if record_path.exists() {
        return Ok(Vec::new());
    }
    crate::management::client_tokens::ensure_private_dir(&cutex_dir.join("archive"))?;
    crate::management::client_tokens::ensure_private_dir(&archive)?;
    let at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut changes = Vec::new();
    let mut backup = Value::Null;
    if accounts.exists() {
        let raw = std::fs::read(accounts)
            .with_context(|| format!("read {}", accounts.display()))?;
        let mut store: Value = serde_json::from_slice(&raw)
            .with_context(|| format!("parse {}", accounts.display()))?;
        changes = plan(&mut store);
        if !changes.is_empty() {
            let copy = archive.join(format!(
                "accounts.pre-claude-tool-defaults-{}.json",
                now.format("%Y%m%dT%H%M%SZ")
            ));
            crate::config::atomic::write_private_bytes_atomic(&copy, &raw)?;
            backup = json!(copy.to_string_lossy());
            crate::config::atomic::write_pretty_json_atomic(accounts, &store, "accounts file")?;
        }
    }
    let record = json!({
        "schema": RECORD_SCHEMA,
        "at": at,
        "backup": backup,
        "profiles": changes.iter().map(|change| json!({
            "profile": change.profile,
            "allowWrite": change.allow_write,
            "allowExec": change.allow_exec,
        })).collect::<Vec<_>>(),
    });
    crate::config::atomic::write_private_pretty_json_atomic(&record_path, &record, "claude-tool-defaults record")?;
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clears_stored_rules_once_with_a_backup() {
        let root = std::env::temp_dir().join(format!("claude-tool-defaults-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let accounts = root.join("accounts.json");
        let before = json!({"version": 3, "accounts": [
            {"name": "Rowan", "options": {"claude": {"model": "opus", "allow_write": false, "allow_exec": true}}},
            {"name": "codex", "options": {}},
            {"name": "Claude new", "options": {"claude": {"model": "sonnet"}}},
        ], "active_account_id": null});
        std::fs::write(&accounts, serde_json::to_vec(&before).unwrap()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-11T12:00:00Z").unwrap().with_timezone(&Utc);
        let changes = run_at(&root, &accounts, now).unwrap();
        assert_eq!(changes, vec![ProfileChange {
            profile: "Rowan".into(),
            allow_write: Some(json!(false)),
            allow_exec: Some(json!(true)),
        }]);
        let after: Value = serde_json::from_slice(&std::fs::read(&accounts).unwrap()).unwrap();
        assert_eq!(after["accounts"][0]["options"]["claude"], json!({"model": "opus"}));
        assert_eq!(after["accounts"][2], before["accounts"][2]);
        let options: crate::profiles::appearance::ProfileOptions =
            serde_json::from_value(after["accounts"][0]["options"].clone()).unwrap();
        assert!(options.claude.write() && options.claude.exec(), "unset = on");
        let backup = root.join(ARCHIVE_ROOT).join("accounts.pre-claude-tool-defaults-20261011T120000Z.json");
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&backup).unwrap()).unwrap(), before);
        let record: Value =
            serde_json::from_slice(&std::fs::read(root.join(ARCHIVE_ROOT).join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!(record["profiles"][0]["profile"], "Rowan");
        // A rule set after the migration stays.
        let mut store = after.clone();
        store["accounts"][0]["options"]["claude"]["allow_write"] = json!(false);
        std::fs::write(&accounts, serde_json::to_vec(&store).unwrap()).unwrap();
        assert!(run_at(&root, &accounts, now).unwrap().is_empty());
        let kept: Value = serde_json::from_slice(&std::fs::read(&accounts).unwrap()).unwrap();
        assert_eq!(kept["accounts"][0]["options"]["claude"]["allow_write"], false);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_home_without_accounts_is_marked_done() {
        let root = std::env::temp_dir().join(format!("claude-tool-defaults-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(run_at(&root, &root.join("accounts.json"), Utc::now()).unwrap().is_empty());
        assert!(root.join(ARCHIVE_ROOT).join(RECORD_FILE).exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
