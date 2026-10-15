//! Account lookup and profile-name validation helpers.

use anyhow::anyhow;

use crate::profiles::model::AccountsStore;
use crate::profiles::model::StoredAccount;

pub fn find_account<'a>(
    store: &'a AccountsStore,
    target: &str,
) -> anyhow::Result<Option<&'a StoredAccount>> {
    Ok(store
        .accounts
        .iter()
        .find(|account| account.name == target || account.id == target))
}

pub fn resolve_configured_default_profile_name(
    store: &AccountsStore,
    target: Option<String>,
) -> anyhow::Result<Option<String>> {
    let Some(target) = target else {
        return Ok(None);
    };
    let target = target.trim();
    if target.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        find_account(store, target)?
            .map(|account| account.name.clone())
            .ok_or_else(|| anyhow!("Account not found: {target}"))?,
    ))
}

pub fn ensure_unique_name(
    store: &AccountsStore,
    name: &str,
    ignore_account_id: Option<&str>,
) -> anyhow::Result<()> {
    if store.accounts.iter().any(|account| {
        account.name == name && ignore_account_id.map(|id| id != account.id).unwrap_or(true)
    }) {
        anyhow::bail!("An account with name '{}' already exists", name);
    }
    Ok(())
}
