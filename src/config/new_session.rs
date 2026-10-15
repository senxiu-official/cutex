//! Read-only defaults used only when creating a new human session or agent.
use anyhow::Context;
use crate::profiles::model::{AccountsStore, NewSessionDefaults};

/// The profile with this name or id. Clients may select a profile by its
/// stable id (the desktop pickers do) or by name; both must resolve.
fn find_profile<'a>(
    accounts: &'a AccountsStore,
    profile: &str,
) -> anyhow::Result<&'a crate::profiles::model::StoredAccount> {
    let profile = profile.trim();
    accounts
        .accounts
        .iter()
        .find(|a| a.name == profile)
        .or_else(|| accounts.accounts.iter().find(|a| a.id == profile))
        .with_context(|| {
            crate::profiles::identity::unknown_profile_message(
                profile,
                &crate::profiles::identity::available_profile_names(accounts),
            )
        })
}

/// Canonical profile name for a profile selected by name or id.
pub fn resolve_profile_name(profile: &str) -> anyhow::Result<String> {
    let accounts: AccountsStore =
        serde_json::from_slice(&std::fs::read(super::paths::accounts_path()?)?)?;
    Ok(find_profile(&accounts, profile)?.name.clone())
}

pub fn inherited(profile: &str) -> anyhow::Result<NewSessionDefaults> {
    let accounts: AccountsStore = serde_json::from_slice(&std::fs::read(super::paths::accounts_path()?)?)?;
    let account = find_profile(&accounts, profile)?;
    if account.cli_kind==crate::profiles::model::CliKind::Claude {return Ok(NewSessionDefaults{model:Some(account.options.claude.model.clone()),reasoning:None});}
    let files = crate::profiles::materialize::materialized_account_files(account)?;
    let profile: toml::Value = toml::from_str(&std::fs::read_to_string(files.config_path)?)?;
    let deployment = crate::launch::local_deployment::LocalDeployment::selected()?.context("No local runtime installed")?;
    let shared: toml::Value = toml::from_str(&std::fs::read_to_string(deployment.native_home.join("config.toml"))?)?;
    Ok(from_tables(&profile, &shared))
}

fn from_tables(profile: &toml::Value, shared: &toml::Value) -> NewSessionDefaults {
    let value = |key| profile.get(key).or_else(|| shared.get(key)).and_then(toml::Value::as_str).map(str::to_owned);
    NewSessionDefaults { model: value("model"), reasoning: value("model_reasoning_effort") }
}

#[cfg(test)]
mod tests {
    #[test]
    fn profiles_resolve_by_name_or_stable_id() {
        let accounts: crate::profiles::model::AccountsStore = serde_json::from_value(serde_json::json!({
            "version": crate::profiles::model::STORE_VERSION,
            "active_account_id": null,
            "accounts": [
                {"id": "a684acb3-79ed-4c3c-9066-4a35ababe49e", "name": "deepseek"},
                {"id": "97ea4b0b-ca34-4f2c-84f2-7bc3e868c086", "name": "rowan"},
            ],
        }))
        .unwrap();
        assert_eq!(super::find_profile(&accounts, "deepseek").unwrap().name, "deepseek");
        assert_eq!(
            super::find_profile(&accounts, "97ea4b0b-ca34-4f2c-84f2-7bc3e868c086").unwrap().name,
            "rowan"
        );
        let missing = super::find_profile(&accounts, "nope").unwrap_err().to_string();
        assert!(missing.contains("Profile not found (by name or id): nope"), "{missing}");
    }

    #[test]
    fn profile_values_win_and_missing_effort_inherits_shared() {
        let profile = toml::from_str("model = 'glm'").unwrap();
        let shared = toml::from_str("model = 'gpt'\nmodel_reasoning_effort = 'high'").unwrap();
        let actual = super::from_tables(&profile, &shared);
        assert_eq!(actual.model.as_deref(), Some("glm"));
        assert_eq!(actual.reasoning.as_deref(), Some("high"));
    }
}
