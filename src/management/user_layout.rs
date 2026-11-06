//! Per-person opaque layout sections (design 056, phase 1, r275).
//! Only small layout files are touched under the dedicated blocking file lock.
use super::users::{self, Principal, UserError};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs::OpenOptions, path::PathBuf};

pub const SCHEMA: &str = "cutex/user-layout/v1";
pub const MAX_SECTION_BYTES: usize = 64 * 1024;
pub const MAX_DOCUMENT_BYTES: usize = 512 * 1024;
pub const CHANGED: &str = "cutex/users/me/layoutChanged";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub revision: u64,
    pub data: Value,
    pub updated_at: String,
    pub updated_by: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pointer {
    pub memory_host: String,
    pub label: String,
    pub set_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub schema: String,
    pub doc_key: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_fingerprint: Option<String>,
    #[serde(default)]
    pub pointer: Option<Pointer>,
    #[serde(default)]
    pub sections: BTreeMap<String, Section>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub doc_key: String,
    pub user_id: String,
    pub fingerprint: Option<String>,
    pub display_name: String,
    pub owner_person_confirmed: bool,
}

/// Caller and person stores are resolved before acquiring the layout lock.
pub fn identity(store: &users::UserStore) -> Result<Identity, UserError> {
    let principal = users::current_principal();
    let user_id = match &principal {
        Some(Principal::Pending) => {
            return Err(UserError::new(
                403,
                "user_pending",
                "assign this device to a user first",
            ))
        }
        Some(Principal::User { id, .. }) => id.clone(),
        _ => users::OWNER.to_string(),
    };
    let persons = store.persons_snapshot().map_err(unavailable)?;
    // Explicitly unconfirmed hosts keep today's shared owner layout. Do not
    // infer ownership here: only the persisted confirmation splits documents.
    let owner_key = persons
        .owner_person
        .as_ref()
        .and_then(|p| p.public_key.as_deref())
        .filter(|key| {
            persons
                .keys_of(users::OWNER)
                .iter()
                .any(|k| k.public_key == *key)
        });
    let token = super::listeners::current_client_token();
    let presented = token
        .as_ref()
        .and_then(|t| persons.token_persons.get(&t.token_id))
        .map(String::as_str);
    let doc_key = derive_doc_key(&user_id, token.is_none(), presented, owner_key);
    let fingerprint = doc_key.strip_prefix("owner~").map(str::to_string);
    let state = store.snapshot().map_err(unavailable)?;
    let display_name = if fingerprint.is_some() {
        token
            .as_ref()
            .and_then(|t| persons.token_claims.get(&t.token_id))
            .and_then(|c| c.display_name.clone())
            .or_else(|| {
                persons
                    .keys_of(users::OWNER)
                    .iter()
                    .find(|k| Some(k.public_key.as_str()) == presented)
                    .and_then(|k| k.label.clone())
            })
            .unwrap_or_else(|| doc_key.clone())
    } else {
        super::user_profiles::ProfileStore::at(store.dir())
            .profile(&user_id)
            .display_name
            .or_else(|| state.user(&user_id).map(|u| u.name.clone()))
            .unwrap_or_else(|| user_id.clone())
    };
    Ok(Identity {
        doc_key,
        user_id,
        fingerprint,
        display_name,
        owner_person_confirmed: owner_key.is_some(),
    })
}

pub fn derive_doc_key(
    user: &str,
    root: bool,
    presented: Option<&str>,
    owner_key: Option<&str>,
) -> String {
    if user != users::OWNER
        || root
        || owner_key.is_none()
        || presented.is_none()
        || presented == owner_key
    {
        return user.to_string();
    }
    format!("owner~{}", super::persons::fingerprint(presented.unwrap()))
}

pub fn valid_doc_key(key: &str) -> bool {
    users::valid_user_id(key)
        || key.strip_prefix("owner~").is_some_and(|fp| {
            fp.len() == 16
                && fp
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}

pub fn valid_section(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn valid_instance(id: &str) -> bool {
    id.strip_prefix("ci-").is_some_and(|s| {
        s.len() == 32
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

pub fn invalid(message: impl Into<String>) -> UserError {
    UserError::new(400, "invalid_layout", message)
}
pub fn unavailable(error: impl std::fmt::Display) -> UserError {
    UserError::new(503, "layout_unavailable", error.to_string())
}

fn assert_no_lane() {
    debug_assert!(
        !super::v2::session_coordination::thread_holds_lane(),
        "layout store entered while holding OperationLane"
    );
}

#[derive(Clone)]
pub struct LayoutStore {
    users_dir: PathBuf,
}

impl LayoutStore {
    pub fn at(users_dir: impl Into<PathBuf>) -> Self {
        Self {
            users_dir: users_dir.into(),
        }
    }
    pub fn dir(&self) -> PathBuf {
        self.users_dir.join("layout")
    }
    fn path(&self, key: &str) -> PathBuf {
        self.dir().join(format!("{}.json", key.replace('~', "--")))
    }
    pub fn get(&self, who: &Identity) -> Result<Record, UserError> {
        assert_no_lane();
        if !valid_doc_key(&who.doc_key) {
            return Err(invalid("invalid docKey"));
        }
        match std::fs::read(self.path(&who.doc_key)) {
            Ok(bytes) => {
                let record: Record = serde_json::from_slice(&bytes).map_err(unavailable)?;
                if record.schema != SCHEMA
                    || record.doc_key != who.doc_key
                    || record.user_id != who.user_id
                {
                    return Err(unavailable("unsupported or mismatched layout document"));
                }
                Ok(record)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record {
                schema: SCHEMA.into(),
                doc_key: who.doc_key.clone(),
                user_id: who.user_id.clone(),
                person_fingerprint: who.fingerprint.clone(),
                pointer: None,
                sections: BTreeMap::new(),
                extra: Default::default(),
            }),
            Err(e) => Err(unavailable(e)),
        }
    }

    fn with(
        &self,
        who: &Identity,
        edit: impl FnOnce(&mut Record) -> Result<bool, UserError>,
    ) -> Result<(Record, bool), UserError> {
        assert_no_lane();
        if !valid_doc_key(&who.doc_key) {
            return Err(invalid("invalid docKey"));
        }
        super::client_tokens::ensure_private_dir(&self.dir()).map_err(unavailable)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir().join("layout.lock"))
            .map_err(unavailable)?;
        lock.lock_exclusive().map_err(unavailable)?;
        let mut record = self.get(who)?;
        let changed = edit(&mut record)?;
        if changed {
            let size = serde_json::to_vec(&record).map_err(unavailable)?.len();
            if size > MAX_DOCUMENT_BYTES {
                return Err(too_large(size, MAX_DOCUMENT_BYTES));
            }
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.path(&who.doc_key),
                &record,
                "user layout",
            )
            .map_err(unavailable)?;
        }
        Ok((record, changed)) // lock drops before audit or publication
    }

    pub fn put(
        &self,
        who: &Identity,
        name: &str,
        revision: u64,
        data: Value,
    ) -> Result<Section, UserError> {
        if !valid_section(name) || name == "pointer" {
            return Err(invalid("invalid or reserved section name"));
        }
        if !data.is_object() && !data.is_null() {
            return Err(invalid("data must be an object or null"));
        }
        let bytes = serde_json::to_vec(&data).map_err(unavailable)?.len();
        if bytes > MAX_SECTION_BYTES {
            return Err(too_large(bytes, MAX_SECTION_BYTES));
        }
        let (record, changed) = self.with(who, |record| {
            let current = record.sections.get(name);
            let actual = current.map_or(0, |s| s.revision);
            if revision != actual {
                let mut e = UserError::new(
                    409,
                    "layout_revision_conflict",
                    "reload this section and apply the edit again",
                );
                e.details = json!({"revision": actual});
                return Err(e);
            }
            if current.map_or(data.is_null(), |s| s.data == data) {
                return Ok(false);
            }
            let next = actual
                .checked_add(1)
                .ok_or_else(|| unavailable("layout revision exhausted"))?;
            record.sections.insert(
                name.into(),
                Section {
                    revision: next,
                    data,
                    updated_at: super::instance::now_utc(),
                    updated_by: who.doc_key.clone(),
                },
            );
            Ok(true)
        })?;
        let section = record.sections.get(name).cloned().unwrap_or(Section {
            revision: 0,
            data: Value::Null,
            updated_at: String::new(),
            updated_by: String::new(),
        });
        if changed {
            users::audit(
                &self.users_dir,
                "layoutUpdated",
                json!({"docKey": who.doc_key, "actor": who.doc_key,
                "section": name, "revision": section.revision, "viaSwitch": false}),
            );
            super::v2::documents::publish_host_event(
                CHANGED,
                json!({"docKey": who.doc_key, "section": name, "revision": section.revision}),
            );
        }
        Ok(section)
    }

    pub fn set_pointer(
        &self,
        who: &Identity,
        memory_host: &str,
        label: &str,
    ) -> Result<Pointer, UserError> {
        if !valid_instance(memory_host)
            || label.is_empty()
            || label.chars().count() > 200
            || label.chars().any(char::is_control)
        {
            return Err(invalid("memoryHost must be an instanceId; label must contain 1..=200 characters without controls"));
        }
        let (record, changed) = self.with(who, |record| {
            if record
                .pointer
                .as_ref()
                .is_some_and(|p| p.memory_host == memory_host && p.label == label)
            {
                return Ok(false);
            }
            record.pointer = Some(Pointer {
                memory_host: memory_host.into(),
                label: label.into(),
                set_at: super::instance::now_utc(),
            });
            Ok(true)
        })?;
        let pointer = record.pointer.unwrap();
        if changed {
            users::audit(
                &self.users_dir,
                "layoutPointerUpdated",
                json!({"docKey": who.doc_key, "actor": who.doc_key, "viaSwitch": false}),
            );
            super::v2::documents::publish_host_event(
                CHANGED,
                json!({"docKey": who.doc_key, "section": "pointer", "revision": 0, "pointer": pointer}),
            );
        }
        Ok(pointer)
    }

    pub fn remove(&self, user: &str) -> Result<(), UserError> {
        assert_no_lane();
        if !users::valid_user_id(user) || user == users::OWNER {
            return Err(invalid("invalid removable user"));
        }
        if !self.dir().exists() {
            return Ok(());
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir().join("layout.lock"))
            .map_err(unavailable)?;
        lock.lock_exclusive().map_err(unavailable)?;
        match std::fs::remove_file(self.path(user)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(unavailable(e)),
        }
    }
}

fn too_large(bytes: usize, limit: usize) -> UserError {
    let mut e = UserError::new(
        413,
        "layout_too_large",
        "layout exceeds its compact JSON size limit",
    );
    e.details = json!({"bytes": bytes, "limit": limit});
    e
}

#[cfg(test)]
#[path = "user_layout_tests.rs"]
mod tests;
