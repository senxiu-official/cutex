//! Accounts/profile store persistence helpers.

use std::fs;
use std::path::Path;

use anyhow::Context;

use crate::config::atomic::write_pretty_json_atomic;
use crate::config::paths::accounts_path;
use crate::profiles::model::AccountsStore;
use crate::profiles::model::StoredAccount;
use crate::profiles::model::STORE_VERSION;

pub fn save_store(store: &AccountsStore) -> anyhow::Result<()> {
    let path = accounts_path()?;
    write_pretty_json_atomic(&path, store, "accounts file")
}

pub fn canonicalize_store(
    mut store: AccountsStore,
    mut detect_source: impl FnMut(&StoredAccount) -> Option<String>,
) -> AccountsStore {
    if store.version != STORE_VERSION {
        store.version = STORE_VERSION;
    }
    for account in &mut store.accounts {
        if account.source.is_none() {
            account.source = detect_source(account);
        }
    }
    let active_exists = store.active_account_id.as_ref().is_some_and(|active_id| {
        store
            .accounts
            .iter()
            .any(|account| account.id.as_str() == active_id.as_str())
    });
    if !active_exists {
        store.active_account_id = store.accounts.first().map(|account| account.id.clone());
    }
    store
}

pub fn backup_legacy_accounts_file(accounts_file_path: &Path) -> anyhow::Result<()> {
    let backup_path = accounts_file_path.with_file_name("accounts.v2.backup.json");
    if backup_path.exists() {
        return Ok(());
    }
    fs::copy(accounts_file_path, &backup_path).with_context(|| {
        format!(
            "Failed to create legacy accounts backup {} -> {}",
            accounts_file_path.display(),
            backup_path.display()
        )
    })?;
    Ok(())
}
