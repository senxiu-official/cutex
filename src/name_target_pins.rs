//! Immutable action-to-recipient bindings for local and peer name resolution.
//! A binding selects identity; it grants no permission to send or execute.
//! Indexed single-row IO; pins have no expiry, cap, or eviction policy.
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

pub struct Key<'a> {
    pub caller: &'a str,
    pub tool: &'a str,
    pub action: &'a str,
    /// Digest of the original request, before any names are normalized.
    pub digest: &'a str,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub cutex_session_id: String,
    /// Resolution metadata/notes only; never credentials or the work brief.
    pub detail: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinError {
    Invalid,
    Conflict,
    Corrupt,
    Unavailable,
}
impl PinError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_request",
            Self::Conflict => "idempotency_conflict",
            Self::Corrupt => "name_target_pins_corrupt",
            Self::Unavailable => "persistence_unavailable",
        }
    }
}
impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for PinError {}
type Result<T> = std::result::Result<T, PinError>;
fn sql<T>(value: rusqlite::Result<T>) -> Result<T> {
    value.map_err(|_| PinError::Unavailable)
}
fn key_valid(k: &Key<'_>) -> bool {
    !k.caller.is_empty()
        && k.caller.len() <= 128
        && !k.tool.is_empty()
        && k.tool.len() <= 128
        && !k.action.is_empty()
        && k.action.len() <= 4096
        && !k.digest.is_empty()
        && k.digest.len() <= 128
}
fn binding_valid(b: &Binding) -> bool {
    !b.cutex_session_id.is_empty()
        && b.cutex_session_id.len() <= 128
        && !b.cutex_session_id.chars().any(char::is_control)
        && b.peer_id
            .as_ref()
            .is_none_or(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
}
fn open(root: &Path, create: bool) -> Result<Option<Connection>> {
    let path = root.join("name-target-pins.sqlite3");
    if !path.exists() && !create {
        return Ok(None);
    }
    if path.is_symlink() {
        return Err(PinError::Corrupt);
    }
    crate::management::client_tokens::ensure_private_dir(root)
        .map_err(|_| PinError::Unavailable)?;
    // busy_timeout does not serialize concurrent journal-mode promotion on a
    // brand-new DB. Fence initialization across processes before either opener
    // touches SQLite. Readers must not observe a published but partial schema.
    // The descriptor closes on every return; the small lock file is retained.
    let lock_path = root.join("name-target-pins-init.lock");
    if lock_path.is_symlink() {
        return Err(PinError::Corrupt);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let initialization = options.open(lock_path).map_err(|_| PinError::Unavailable)?;
    if create {
        FileExt::lock_exclusive(&initialization)
    } else {
        FileExt::lock_shared(&initialization)
    }
    .map_err(|_| PinError::Unavailable)?;
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | if create {
            rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        } else {
            rusqlite::OpenFlags::empty()
        };
    let db = sql(Connection::open_with_flags(&path, flags))?;
    sql(db.busy_timeout(std::time::Duration::from_secs(5)))?;
    if create {
        sql(db.pragma_update(None, "journal_mode", "WAL"))?;
        sql(db.execute_batch(
            "CREATE TABLE IF NOT EXISTS name_target_pins (
            caller TEXT NOT NULL, tool TEXT NOT NULL, action TEXT NOT NULL,
            digest TEXT NOT NULL, binding_json TEXT NOT NULL,
            PRIMARY KEY(caller,tool,action)) WITHOUT ROWID;",
        ))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| PinError::Unavailable)?;
        }
    }
    Ok(Some(db))
}
fn row(db: &Connection, key: &Key<'_>) -> Result<Option<Binding>> {
    let stored:Option<(String,String)>=sql(db.query_row(
        "SELECT digest,binding_json FROM name_target_pins WHERE caller=?1 AND tool=?2 AND action=?3",
        params![key.caller,key.tool,key.action],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
    let Some((digest, value)) = stored else {
        return Ok(None);
    };
    if digest != key.digest {
        return Err(PinError::Conflict);
    }
    let binding: Binding = serde_json::from_str(&value).map_err(|_| PinError::Corrupt)?;
    if !binding_valid(&binding) {
        return Err(PinError::Corrupt);
    }
    Ok(Some(binding))
}

pub fn lookup(key: &Key<'_>) -> Result<Option<Binding>> {
    let root = crate::config::paths::runtime_dir().map_err(|_| PinError::Unavailable)?;
    lookup_at(&root, key)
}
pub fn bind(key: &Key<'_>, binding: &Binding) -> Result<Binding> {
    let root = crate::config::paths::runtime_dir().map_err(|_| PinError::Unavailable)?;
    bind_at(&root, key, binding)
}
pub fn lookup_at(root: &Path, key: &Key<'_>) -> Result<Option<Binding>> {
    if !key_valid(key) {
        return Err(PinError::Invalid);
    }
    let Some(db) = open(root, false)? else {
        return Ok(None);
    };
    row(&db, key)
}
pub fn bind_at(root: &Path, key: &Key<'_>, binding: &Binding) -> Result<Binding> {
    if !key_valid(key) || !binding_valid(binding) {
        return Err(PinError::Invalid);
    }
    let encoded = serde_json::to_string(binding).map_err(|_| PinError::Invalid)?;
    if encoded.len() > 65536 {
        return Err(PinError::Invalid);
    }
    let mut db = open(root, true)?.ok_or(PinError::Unavailable)?;
    let tx = sql(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
    sql(tx.execute("INSERT OR IGNORE INTO name_target_pins(caller,tool,action,digest,binding_json) VALUES(?1,?2,?3,?4,?5)",
        params![key.caller,key.tool,key.action,key.digest,encoded]))?;
    let selected = row(&tx, key)?.ok_or(PinError::Corrupt)?;
    sql(tx.commit())?;
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an explicitly marked, consistent release business copy"]
    fn release_data_copy_target_pins_validate() {
        let root=std::path::PathBuf::from(std::env::var_os("CUTEX_RELEASE_DATA_COPY").expect("explicit copy root"));
        assert!(root.is_absolute() && root.join(".cutex-release-data-copy").is_file());
        assert_eq!(root.canonicalize().unwrap(),crate::config::paths::config_dir().unwrap().canonicalize().unwrap());
        let path=root.join("runtime/name-target-pins.sqlite3");
        if !path.exists() {eprintln!("RELEASE DATA COPY target pins absent in predecessor");return}
        let db=Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut statement=db.prepare("SELECT caller,tool,action,digest,binding_json FROM name_target_pins").unwrap();
        let rows=statement.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?))).unwrap();
        let mut count=0;
        for row in rows {
            let (caller,tool,action,digest,value)=row.unwrap();
            assert!(key_valid(&Key {caller:&caller,tool:&tool,action:&action,digest:&digest}));
            assert!(value.len()<=65536);
            let binding:Binding=serde_json::from_str(&value).expect("invalid copied pin binding");
            assert!(binding_valid(&binding));count+=1;
        }
        eprintln!("RELEASE DATA COPY target pins validated={count}");
    }
    #[test]
    fn concurrent_first_local_and_peer_bind_initializes_a_fresh_database() {
        let _guard = crate::config::paths::test_home_env_lock();
        let home = crate::config::paths::home_dir().unwrap();
        for trial in 0..64 {
            let root = home.join(format!("name-pin-first-init-{}", uuid::Uuid::new_v4()));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let workers: Vec<_> = [false, true]
                .into_iter()
                .map(|remote| {
                    let root = root.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        let key = Key {
                            caller: "cutex.fixture",
                            tool: "agent_tell",
                            action: if remote { "peer-first" } else { "local-first" },
                            digest: "original-raw-request",
                        };
                        let binding = Binding {
                            peer_id: remote.then(|| "ci-0123456789abcdef0123456789abcdef".into()),
                            cutex_session_id: if remote {
                                "cutex.remote".into()
                            } else {
                                "cutex.local".into()
                            },
                            detail: serde_json::json!({"kind":"fixture"}),
                        };
                        barrier.wait();
                        bind_at(&root, &key, &binding).map(|actual| (binding, actual))
                    })
                })
                .collect();
            let outcomes: Vec<_> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            let good = outcomes
                .iter()
                .all(|outcome| matches!(outcome,Ok((expected,actual)) if expected==actual));
            if root.exists() {
                std::fs::remove_dir_all(&root).unwrap();
            }
            assert!(good, "fresh local/peer init trial {trial}: {outcomes:?}");
        }
    }
}
