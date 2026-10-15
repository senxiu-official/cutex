//! `cutex human claude account …`: Claude accounts on this host. The rules
//! live in `cutex::runtime::claude_accounts`; Management v2 serves the same
//! operations at `/v2/claude/accounts` (without login).
use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{bail, ensure, Context};
use serde_json::{json, Value};

use cutex::cli::args::ClaudeAccountCommand;
use cutex::config::global_settings::ConfigValueUpdate;
use cutex::runtime::claude_accounts::{
    self, AccountPatch, ApiKeyEnv, Change, ClaudeAccountKind, ClaudeApiSettings, NewClaudeAccount, RemoveError,
    DEFAULT_ACCOUNT_ID,
};

/// `default`, an id or a label (exact, then case-insensitive).
fn resolve(account: &str) -> anyhow::Result<String> {
    let account = account.trim();
    if account == DEFAULT_ACCOUNT_ID {
        return Ok(DEFAULT_ACCOUNT_ID.to_string());
    }
    let registry = claude_accounts::load()?;
    if registry.find(account).is_some() {
        return Ok(account.to_string());
    }
    if let Some(found) = registry.accounts.iter().find(|entry| entry.label == account) {
        return Ok(found.id.clone());
    }
    let matches: Vec<_> = registry
        .accounts
        .iter()
        .filter(|entry| entry.label.eq_ignore_ascii_case(account))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => bail!("no Claude account {account} (cutex human claude account list)"),
        _ => bail!("several Claude accounts are labelled {account}; use the id"),
    }
}

fn read_key_from_stdin() -> anyhow::Result<String> {
    let mut key = String::new();
    std::io::stdin()
        .take(claude_accounts::MAX_API_KEY_BYTES as u64 + 2)
        .read_to_string(&mut key)
        .context("read the API key from stdin")?;
    claude_accounts::validate_api_key(&key)
}

fn parse_key_env(text: &str) -> anyhow::Result<ApiKeyEnv> {
    ApiKeyEnv::parse(text).context("--key-env must be ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN")
}

fn parse_env(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    pairs
        .iter()
        .map(|pair| {
            let (key, value) = pair.split_once('=').with_context(|| format!("--env {pair}: expected KEY=VALUE"))?;
            Ok((key.trim().to_string(), value.to_string()))
        })
        .collect()
}

fn item(id: &str) -> anyhow::Result<Value> {
    let registry = claude_accounts::load()?;
    let usage = claude_accounts::usage_by_account(&registry);
    claude_accounts::item_json(&registry, id, usage.get(id)).with_context(|| format!("no Claude account {id}"))
}

pub(super) fn run(command: ClaudeAccountCommand) -> anyhow::Result<Value> {
    match command {
        ClaudeAccountCommand::List => claude_accounts::list_json(),
        ClaudeAccountCommand::Add { label, api, base_url, key_env, env, api_key_stdin } => {
            let kind = if api { ClaudeAccountKind::Api } else { ClaudeAccountKind::Subscription };
            let api_settings = api
                .then(|| -> anyhow::Result<ClaudeApiSettings> {
                    Ok(ClaudeApiSettings {
                        base_url,
                        key_env: key_env.as_deref().map(parse_key_env).transpose()?.unwrap_or_default(),
                        env: parse_env(&env)?,
                    })
                })
                .transpose()?;
            let api_key = api_key_stdin.then(read_key_from_stdin).transpose()?;
            let account = claude_accounts::add(NewClaudeAccount { label, kind, api: api_settings, api_key })?;
            let mut out = item(&account.id)?;
            out["next"] = match kind {
                ClaudeAccountKind::Subscription => {
                    json!(format!("log it in: cutex human claude account login {}", account.id))
                }
                ClaudeAccountKind::Api if !claude_accounts::api_key_present(&account.id) => json!(format!(
                    "set its key: <key source> | cutex human claude account set {} --api-key-stdin",
                    account.id
                )),
                ClaudeAccountKind::Api => {
                    json!(format!("bind a Claude profile: cutex human claude account bind <profile> {}", account.id))
                }
            };
            Ok(out)
        }
        ClaudeAccountCommand::Login { account, email, print } => {
            let id = resolve(&account)?;
            if print {
                return claude_accounts::login_command(&id);
            }
            let status = claude_accounts::run_login(&id, email.as_deref())?;
            Ok(json!({ "id": id, "status": status }))
        }
        ClaudeAccountCommand::Logout { account } => {
            let id = resolve(&account)?;
            let status = claude_accounts::run_logout(&id)?;
            Ok(json!({ "id": id, "status": status }))
        }
        ClaudeAccountCommand::Status { account } => {
            let id = resolve(&account)?;
            claude_accounts::check_status(&id)?;
            item(&id)
        }
        ClaudeAccountCommand::Set {
            account,
            label,
            base_url,
            clear_base_url,
            key_env,
            env,
            clear_env,
            api_key_stdin,
        } => {
            let id = resolve(&account)?;
            let patch = AccountPatch {
                label,
                base_url: match (base_url, clear_base_url) {
                    (Some(url), _) => Change::Set(url),
                    (None, true) => Change::Clear,
                    (None, false) => Change::Unchanged,
                },
                key_env: key_env.as_deref().map(parse_key_env).transpose()?,
                env: if clear_env {
                    Some(BTreeMap::new())
                } else if env.is_empty() {
                    None
                } else {
                    Some(parse_env(&env)?)
                },
                api_key: api_key_stdin.then(read_key_from_stdin).transpose()?,
            };
            ensure!(patch != AccountPatch::default(), "nothing to change");
            claude_accounts::update(&id, patch)?;
            item(&id)
        }
        ClaudeAccountCommand::Bind { profile, account } => {
            let id = resolve(&account)?;
            let patch = cutex::profiles::settings_api::ProfileApiPatch {
                claude_account: if id == DEFAULT_ACCOUNT_ID {
                    ConfigValueUpdate::Clear
                } else {
                    ConfigValueUpdate::Set(id.clone())
                },
                ..Default::default()
            };
            let (stored, changed) = cutex::profiles::settings_api::update_profile(&profile, &patch)?;
            Ok(json!({
                "profile": { "id": stored.id, "name": stored.name },
                "claudeAccount": id,
                "changed": changed,
                "note": "new agents on this profile use the account; existing agents keep theirs",
            }))
        }
        ClaudeAccountCommand::Remove { account, delete_home } => {
            let id = resolve(&account)?;
            match claude_accounts::remove(&id, delete_home) {
                Ok(outcome) => Ok(json!({
                    "id": outcome.removed.id,
                    "removed": true,
                    "homeDeleted": outcome.home_deleted,
                    "keptHome": outcome.kept_home,
                })),
                Err(RemoveError::InUse(usage)) => bail!(
                    "{}\nprofiles: {}\nactive agents: {}",
                    RemoveError::InUse(usage.clone()),
                    usage.profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "),
                    usage
                        .sessions
                        .iter()
                        .filter(|s| s.lifecycle == "active")
                        .map(|s| s.display_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Err(error) => bail!("{error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::runtime::claude::ClaudeDeployment;
    use std::path::{Path, PathBuf};

    fn install_runtime(root: &Path, name: &str, home: &Path, api_accounts: bool) -> ClaudeDeployment {
        let rt = root.join(name);
        let adapter = rt.join("adapter");
        std::fs::create_dir_all(adapter.join("src/cute_claude")).unwrap();
        std::fs::create_dir_all(adapter.join("scripts")).unwrap();
        for file in ["src/cute_claude/__main__.py", "scripts/connect_tui.py"] {
            std::fs::write(adapter.join(file), "").unwrap();
        }
        std::fs::write(
            adapter.join("src/cute_claude/features.json"),
            serde_json::to_vec(&json!({ "features": { "apiAccounts": api_accounts } })).unwrap(),
        )
        .unwrap();
        let binary = rt.join("python");
        std::fs::write(&binary, "").unwrap();
        ClaudeDeployment {
            python: binary,
            adapter_source: adapter,
            claude_cli: root.join("claude"),
            claude_home: home.to_path_buf(),
            tui_binary: root.join("tui"),
        }
    }

    fn pin(id: &str) -> ClaudeDeployment {
        cutex::session::store::load_cutex_session_store().unwrap().sessions[id]
            .claude_session
            .clone()
            .unwrap()
            .deployment
    }

    fn new_claude_profile(name: &str) {
        let mut store = super::super::account_store::load_store().unwrap();
        store.accounts.push(cutex::profiles::lifecycle::new_claude_account(name));
        cutex::profiles::store::save_store(&store).unwrap();
    }

    fn add_account(label: &str, api: bool) -> String {
        let out = run(ClaudeAccountCommand::Add {
            label: label.into(),
            api,
            base_url: api.then(|| "https://api.deepseek.com/anthropic".to_string()),
            key_env: None,
            env: if api { vec!["ANTHROPIC_DEFAULT_SONNET_MODEL=deepseek-v4-pro".into()] } else { Vec::new() },
            api_key_stdin: false,
        })
        .unwrap();
        out["id"].as_str().unwrap().to_string()
    }

    fn create(name: &str, root: &Path, profile: &str) -> anyhow::Result<cutex::session::model::CutexSessionRecord> {
        cutex::runtime::claude::create_profile(name, root, "sonnet", Some(false), Some(false), Some(profile.into()), true, None)
    }

    #[test]
    fn agents_keep_the_account_of_their_profile_for_life() {
        let home = super::super::test_home::IsolatedTestHome::new("claude-accounts").unwrap();
        let root = home.root().to_path_buf();
        for file in ["claude", "tui"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let shared: PathBuf = root.join("shared-login");
        std::fs::create_dir(&shared).unwrap();
        let r6 = install_runtime(&root, "claude-r6", &shared, false);
        r6.install().unwrap();

        new_claude_profile("Claude main");
        new_claude_profile("Claude work");
        new_claude_profile("Claude deepseek");
        let work = add_account("Work", false);
        let deepseek = add_account("DeepSeek", true);
        let bound = run(ClaudeAccountCommand::Bind { profile: "Claude work".into(), account: "Work".into() }).unwrap();
        assert_eq!(bound["claudeAccount"], json!(work));
        run(ClaudeAccountCommand::Bind { profile: "Claude deepseek".into(), account: deepseek.clone() }).unwrap();
        assert!(run(ClaudeAccountCommand::Bind { profile: "Claude main".into(), account: "ca-nothere".into() }).is_err());

        // Without a profile, or on a default profile: the shared home, as before.
        let plain = cutex::runtime::claude::create("plain", &root, "sonnet", false, false).unwrap();
        assert_eq!(pin(&plain.cutex_session_id).claude_home, shared);
        let main = create("main", &root, "Claude main").unwrap();
        assert_eq!(pin(&main.cutex_session_id).claude_home, shared);
        // A bound profile freezes the account home into the session.
        let worker = create("worker", &root, "Claude work").unwrap();
        let work_home = cutex::runtime::claude_accounts::account_home(&work).unwrap();
        assert_eq!(pin(&worker.cutex_session_id).claude_home, work_home);
        // An API account needs a key and an adapter that declares apiAccounts.
        let error = create("ds", &root, "Claude deepseek").unwrap_err();
        assert!(format!("{error:#}").contains("no API key"), "{error:#}");
        cutex::runtime::claude_accounts::update(
            &deepseek,
            cutex::runtime::claude_accounts::AccountPatch { api_key: Some("sk-test-only".into()), ..Default::default() },
        )
        .unwrap();
        let error = create("ds", &root, "Claude deepseek").unwrap_err();
        assert!(format!("{error:#}").contains("claude-r7"), "{error:#}");
        let r7 = install_runtime(&root, "claude-r7", &shared, true);
        r7.install().unwrap();
        let ds = create("ds", &root, "Claude deepseek").unwrap();
        assert_eq!(
            pin(&ds.cutex_session_id).claude_home,
            cutex::runtime::claude_accounts::account_home(&deepseek).unwrap()
        );

        // Profile changes never move a session to another account.
        let mut sessions = cutex::session::store::load_cutex_session_store().unwrap();
        let set = |sessions: &mut cutex::session::model::CutexSessionStore, id: &str, profile: Option<&str>| {
            cutex::session::service::set_cutex_session_profile_by_key(sessions, id, profile.map(str::to_string))
        };
        let error = set(&mut sessions, &worker.cutex_session_id, Some("Claude main")).unwrap_err();
        assert!(error.downcast_ref::<cutex::runtime::claude_accounts::ClaudeAccountMismatch>().is_some(), "{error:#}");
        assert!(set(&mut sessions, &main.cutex_session_id, Some("Claude work")).is_err());
        assert!(set(&mut sessions, &ds.cutex_session_id, None).is_err());

        // Repinning to the new runtime keeps each session's own account home.
        cutex::runtime::claude_pin::repin(&worker.cutex_session_id, &r7, false).unwrap();
        cutex::runtime::claude_pin::repin(&main.cutex_session_id, &r7, false).unwrap();
        assert_eq!(pin(&worker.cutex_session_id), ClaudeDeployment { claude_home: work_home.clone(), ..r7.clone() });
        assert_eq!(pin(&main.cutex_session_id), r7);

        // The list shows who uses what; an account in use cannot be removed.
        let list = run(ClaudeAccountCommand::List).unwrap();
        let validator = cutex::management::v2::schema_export::validator("claude-accounts", "accountList").unwrap();
        assert!(validator.is_valid(&list), "{list:#}");
        let accounts = list["accounts"].as_array().unwrap();
        let work_item = accounts.iter().find(|a| a["id"] == json!(work)).unwrap();
        assert_eq!(work_item["profiles"][0]["name"], "Claude work");
        assert_eq!(work_item["sessions"]["active"], 1);
        assert_eq!(work_item["removable"], false);
        assert_eq!(accounts[0]["sessions"]["active"], 2, "plain + main on default");
        assert!(!list.to_string().contains("sk-test-only"));
        assert!(run(ClaudeAccountCommand::Remove { account: work.clone(), delete_home: false }).is_err());
        let printed = run(ClaudeAccountCommand::Login { account: "Work".into(), email: None, print: true }).unwrap();
        assert!(printed["powershell"].as_str().unwrap().contains("CLAUDE_CONFIG_DIR"));
        assert_eq!(printed["claudeConfigDir"], json!(work_home));
    }
}
