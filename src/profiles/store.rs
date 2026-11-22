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

// r290 P13: a preflight needs one profile, not another materialized AccountsStore.
// Accounts are config JSON; scan the array through serde while retaining only
// the first matching row. The existing writer still loads its authoritative
// store under the lifecycle lock before committing any changes.
pub fn account_by_key(profile: &str) -> anyhow::Result<Option<StoredAccount>> {
    use serde::de::DeserializeSeed;
    let path = accounts_path()?;
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("profile store could not be opened"),
    };
    let mut input = serde_json::Deserializer::from_reader(std::io::BufReader::new(file));
    let (version, account) = ProfileMatch(profile)
        .deserialize(&mut input)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))?;
    input.end().context("trailing data in accounts file")?;
    anyhow::ensure!(
        version >= STORE_VERSION,
        "accounts file version {} must be migrated by the cutex CLI first",
        version
    );
    Ok(account)
}

struct ProfileMatch<'a>(&'a str);
struct AccountMatch<'a>(&'a str);
impl<'de> serde::de::DeserializeSeed<'de> for ProfileMatch<'_> {
    type Value = (u32, Option<StoredAccount>);
    fn deserialize<D: serde::Deserializer<'de>>(self, input: D) -> Result<Self::Value, D::Error> {
        input.deserialize_map(self)
    }
}
impl<'de> serde::de::Visitor<'de> for ProfileMatch<'_> {
    type Value = (u32, Option<StoredAccount>);
    fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("a profile store object")
    }
    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        use serde::de::{Error, IgnoredAny};
        let (mut version, mut account, mut active_seen) = (None, None, false);
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "version" => {
                    if version.is_some() {
                        return Err(M::Error::duplicate_field("version"));
                    }
                    version = Some(map.next_value::<u32>()?);
                }
                "accounts" => {
                    if account.is_some() {
                        return Err(M::Error::duplicate_field("accounts"));
                    }
                    account = Some(map.next_value_seed(AccountMatch(self.0))?);
                }
                "active_account_id" => {
                    if active_seen {
                        return Err(M::Error::duplicate_field("active_account_id"));
                    }
                    active_seen = true;
                    let _ = map.next_value::<Option<String>>()?;
                }
                _ => {
                    let _ = map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok((
            version.ok_or_else(|| M::Error::missing_field("version"))?,
            account.ok_or_else(|| M::Error::missing_field("accounts"))?,
        ))
    }
}
impl<'de> serde::de::DeserializeSeed<'de> for AccountMatch<'_> {
    type Value = Option<StoredAccount>;
    fn deserialize<D: serde::Deserializer<'de>>(self, input: D) -> Result<Self::Value, D::Error> {
        input.deserialize_seq(self)
    }
}
impl<'de> serde::de::Visitor<'de> for AccountMatch<'_> {
    type Value = Option<StoredAccount>;
    fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("a profile array")
    }
    fn visit_seq<S: serde::de::SeqAccess<'de>>(self, mut rows: S) -> Result<Self::Value, S::Error> {
        let mut found = None;
        while let Some(account) = rows.next_element::<StoredAccount>()? {
            if found.is_none() && (account.id == self.0 || account.name == self.0) {
                found = Some(account);
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod keyed_tests {
    use super::*;
    #[test]
    fn context_preflight_keyed_lookup_is_read_only_strict_and_matches_id_or_name() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        assert!(account_by_key("missing").unwrap().is_none());
        let rows: Vec<StoredAccount> = (0..128)
            .map(|n| {
                serde_json::from_value(serde_json::json!({
                    "id":format!("id-{n}"),"name":format!("profile-{n}")
                }))
                .unwrap()
            })
            .collect();
        let mut store = AccountsStore {
            accounts: rows,
            ..Default::default()
        };
        save_store(&store).unwrap();
        let path = accounts_path().unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(
            account_by_key("id-127").unwrap().unwrap().name,
            "profile-127"
        );
        assert_eq!(account_by_key("profile-0").unwrap().unwrap().id, "id-0");
        assert!(account_by_key("missing").unwrap().is_none());
        assert_eq!(fs::read(&path).unwrap(), before);
        store.version = STORE_VERSION - 1;
        save_store(&store).unwrap();
        assert!(account_by_key("id-0").is_err());
        fs::write(
            &path,
            format!("{} trailing", String::from_utf8(before.clone()).unwrap()),
        )
        .unwrap();
        assert!(account_by_key("id-0").is_err());
        // A malformed later row must not be hidden by an earlier match.
        fs::write(
            &path,
            format!(
                r#"{{"version":3,"accounts":[{},{{"id":4}}]}}"#,
                serde_json::to_string(&store.accounts[0]).unwrap()
            ),
        )
        .unwrap();
        assert!(account_by_key("id-0").is_err());
    }
}
