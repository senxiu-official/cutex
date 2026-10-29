//! Read-only preflight for every rollback target predating avatar styles/modes.
//! Call before ANY export or compatibility write, with all writers stopped and
//! a trusted existing home. Never load normal stores (their loaders migrate).
//! No credentials or referenced files are opened; diagnostics contain no data.

use std::{fmt, fs, io::Read, path::Path, time::Duration};

use anyhow::{anyhow, bail, ensure, Result};
use rusqlite::{config::DbConfig, limits::Limit, types::ValueRef, Connection, OpenFlags};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde_json::{value::RawValue, Value};

const ACCOUNTS_MAX: u64 = 16 * 1024 * 1024;
const RECORD_MAX: usize = 8 * 1024 * 1024;
// SQLite must reject oversized strings/rows before a SELECT materializes them.
// Leave 1 KiB for SQLite's row encoding and the projected length column; the
// business payload still has the stricter 8 MiB check below.
const SQLITE_ROW_LIMIT: i32 = (RECORD_MAX + 1024) as i32;
const LEGACY_MAX: u64 = 128 * 1024 * 1024;
const DATABASE: &str = "cutex-sessions.sqlite3";
const LEGACY: &str = "cutex-sessions.json";

/// All supported r256/r252/public-0.1.5 targets predate this contract; do not
/// hide this behind the older profile-link gate (which only runs below r256).
pub fn preflight(home: &Path) -> Result<()> {
    ensure!(
        home.is_absolute(),
        "avatar rollback preflight requires an absolute existing home"
    );
    for directory in home.ancestors() {
        let metadata = fs::symlink_metadata(directory)
            .map_err(|_| anyhow!("avatar rollback home boundary is unreadable"))?;
        ensure!(
            !linked(&metadata) && metadata.is_dir(),
            "avatar rollback home boundary is not a plain directory"
        );
    }
    profiles(home)?;
    creator_actions(home)?;
    let database = file(home.join(DATABASE).as_path())?;
    // Check the logical JSON boundary even when SQLite is authoritative, but
    // never parse stale JSON/backups if the database exists.
    let legacy = file(home.join(LEGACY).as_path())?;
    let wal = file(home.join(format!("{DATABASE}-wal")).as_path())?;
    let shm = file(home.join(format!("{DATABASE}-shm")).as_path())?;
    let journal = file(home.join(format!("{DATABASE}-journal")).as_path())?;
    if database.is_some() {
        ensure!(
            journal.is_none(),
            "avatar rollback session journal requires offline recovery first"
        );
        let immutable = match (wal.as_ref(), shm.as_ref()) {
            (None, None) => true,
            (Some(wal), Some(shm)) => {
                validate_wal(home, wal.len(), shm.len())?;
                false
            }
            _ => bail!("avatar rollback requires complete existing WAL/shared-memory boundaries"),
        };
        sessions_sqlite(home, immutable)?;
    } else {
        ensure!(
            wal.is_none() && shm.is_none() && journal.is_none(),
            "avatar rollback found session sidecars without a database"
        );
        if legacy.is_some() {
            let bytes = read(home.join(LEGACY).as_path(), LEGACY_MAX)?;
            let mut parser = serde_json::Deserializer::from_slice(&bytes);
            LegacyStore.deserialize(&mut parser).map_err(|_| anyhow!("avatar rollback legacy sessions are incompatible, malformed, or require their SQLite database"))?;
            parser
                .end()
                .map_err(|_| anyhow!("avatar rollback legacy sessions are malformed"))?;
        }
    }
    Ok(())
}

fn linked(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn file(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !linked(&metadata) && metadata.is_file(),
                "avatar rollback store boundary is not a plain file"
            );
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("avatar rollback store metadata is unreadable"),
    }
}

fn open(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let handle = options
        .open(path)
        .map_err(|_| anyhow!("avatar rollback store is unreadable"))?;
    let metadata = handle
        .metadata()
        .map_err(|_| anyhow!("avatar rollback opened store metadata is unreadable"))?;
    ensure!(
        !linked(&metadata) && metadata.is_file(),
        "avatar rollback opened store is not a plain file"
    );
    Ok(handle)
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let handle = open(path)?;
    ensure!(
        handle
            .metadata()
            .map_err(|_| anyhow!("avatar rollback store metadata is unreadable"))?
            .len()
            <= limit,
        "avatar rollback store exceeds its inspection bound"
    );
    let mut bytes = Vec::new();
    handle
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("avatar rollback store read failed"))?;
    ensure!(
        bytes.len() as u64 <= limit,
        "avatar rollback store exceeds its inspection bound"
    );
    Ok(bytes)
}

fn profiles(home: &Path) -> Result<()> {
    let path = home.join("accounts.json");
    if file(&path)?.is_none() {
        return Ok(());
    }
    let bytes = read(&path, ACCOUNTS_MAX)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow!("avatar rollback accounts JSON is malformed"))?;
    ensure!(
        value.get("version").and_then(Value::as_u64) == Some(3),
        "avatar rollback accounts version is unsupported"
    );
    let accounts = value
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("avatar rollback accounts are malformed"))?;
    for account in accounts {
        ensure!(account.is_object(), "avatar rollback account is malformed");
        if let Some(options) = account.get("options") {
            let options = options
                .as_object()
                .ok_or_else(|| anyhow!("avatar rollback Profile options are malformed"))?;
            ensure!(
                options.get("avatar_style").is_none_or(Value::is_null),
                "avatar rollback blocked: Profile avatar style requires the newer backend"
            );
            ensure!(
                !options.contains_key("avatarStyle"),
                "avatar rollback Profile avatar metadata has an unknown shape"
            );
            if let Some(clocks) = options.get("appearance_updated_at") {
                let clocks = clocks.as_object().ok_or_else(|| {
                    anyhow!("avatar rollback Profile appearance clocks are malformed")
                })?;
                ensure!(!clocks.contains_key("avatarStyle") && !clocks.contains_key("avatar_style"),
                    "avatar rollback blocked: Profile avatar-style history requires the newer backend");
            }
        }
    }
    // Validate the full current schema, without invoking migration/load logic
    // or following source/auth paths. Errors deliberately discard private data.
    serde_json::from_slice::<crate::profiles::model::AccountsStore>(&bytes)
        .map_err(|_| anyhow!("avatar rollback accounts have malformed current-schema data"))?;
    Ok(())
}

/// The old action decoder is deny_unknown_fields. A pending Create/Replace
/// may contain frozen provenance before its session exists, so refuse rather
/// than dropping that evidence to make the previous build start.
fn creator_actions(home: &Path) -> Result<()> {
    for relative in ["runtime", "runtime/agent-management"] {
        match fs::symlink_metadata(home.join(relative)) {
            Ok(m) => ensure!(!linked(&m) && m.is_dir(), "creation provenance storage boundary is invalid"),
            Err(e) if e.kind()==std::io::ErrorKind::NotFound => return Ok(()),
            Err(_)=>bail!("creation provenance storage boundary is unreadable"),
        }
    }
    let path=home.join("runtime/agent-management/agent-management-v1.json");
    if file(&path)?.is_none() { return Ok(()); }
    let bytes=read(&path,64*1024*1024)?;
    let value:Value=serde_json::from_slice(&bytes).map_err(|_|anyhow!("creation provenance store is invalid"))?;
    let actions=value.get("actions").and_then(Value::as_object).ok_or_else(||anyhow!("creation provenance actions are invalid"))?;
    for action in actions.values() {
        let action=action.as_object().ok_or_else(||anyhow!("creation provenance action is invalid"))?;
        ensure!(action.get("created_by").is_none_or(Value::is_null), "rollback blocked: captured creation provenance requires the newer backend");
    }
    serde_json::from_slice::<crate::agent_management::AgentManagementSnapshot>(&bytes)
        .map_err(|_|anyhow!("creation provenance store has an unsupported shape"))?;
    Ok(())
}

fn validate_wal(home: &Path, wal_bytes: u64, shm_bytes: u64) -> Result<()> {
    ensure!(
        shm_bytes >= 32 * 1024,
        "avatar rollback shared-memory header is incomplete"
    );
    let mut shared = [0u8; 96];
    open(&home.join(format!("{DATABASE}-shm")))?
        .read_exact(&mut shared)
        .map_err(|_| anyhow!("avatar rollback shared-memory header is unreadable"))?;
    ensure!(
        shared[0..48] == shared[48..96]
            && shared[12] == 1
            && u32::from_ne_bytes(shared[0..4].try_into().unwrap()) == 3_007_000,
        "avatar rollback shared-memory header is invalid or not quiescent"
    );
    if wal_bytes == 0 {
        return Ok(());
    }
    ensure!(wal_bytes >= 32, "avatar rollback WAL header is incomplete");
    let mut header = [0u8; 32];
    open(&home.join(format!("{DATABASE}-wal")))?
        .read_exact(&mut header)
        .map_err(|_| anyhow!("avatar rollback WAL header is unreadable"))?;
    let magic = u32::from_be_bytes(header[0..4].try_into().unwrap());
    let page = u32::from_be_bytes(header[8..12].try_into().unwrap());
    ensure!(
        (magic == 0x377f0682 || magic == 0x377f0683)
            && u32::from_be_bytes(header[4..8].try_into().unwrap()) == 3_007_000
            && (512..=65536).contains(&page)
            && page.is_power_of_two()
            && (wal_bytes - 32) % (24 + u64::from(page)) == 0,
        "avatar rollback WAL shape is invalid"
    );
    ensure!(
        shared[32..40] == header[16..24],
        "avatar rollback shared-memory header is invalid or not quiescent"
    );
    Ok(())
}

fn sessions_sqlite(home: &Path, immutable: bool) -> Result<()> {
    let mut url = url::Url::from_file_path(home.join(DATABASE))
        .map_err(|_| anyhow!("avatar rollback database path cannot be inspected"))?;
    url.set_query(Some(if immutable {
        "mode=ro&immutable=1"
    } else {
        "mode=ro"
    }));
    // Immutable is ONLY for proven absent wal/shm/journal, under stopped-writer
    // custody. A real WAL always uses a normal read-only consistent transaction.
    let mut db = Connection::open_with_flags(
        url.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| anyhow!("avatar rollback session database cannot be opened read-only"))?;
    db.set_limit(Limit::SQLITE_LIMIT_LENGTH, SQLITE_ROW_LIMIT)
        .map_err(|_| anyhow!("avatar rollback database length limit could not be installed"))?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("avatar rollback database setup failed"))?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(|_| anyhow!("avatar rollback database setup failed"))?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")
        .map_err(|_| anyhow!("avatar rollback database setup failed"))?;
    let transaction = db
        .transaction()
        .map_err(|_| anyhow!("avatar rollback database snapshot failed"))?;
    let version: i64 = transaction
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| anyhow!("avatar rollback session database header is unreadable"))?;
    ensure!(
        version == 1,
        "avatar rollback session database version is unsupported"
    );
    for (query, adoption) in [
        (
            "SELECT length(CAST(record_json AS BLOB)), record_json FROM sessions",
            false,
        ),
        (
            "SELECT length(CAST(json AS BLOB)), json FROM receipts WHERE map='human_adoption'",
            true,
        ),
    ] {
        let mut statement = transaction
            .prepare(query)
            .map_err(|_| anyhow!("avatar rollback session database schema is invalid"))?;
        let mut rows = statement
            .query([])
            .map_err(|_| anyhow!("avatar rollback session rows are unreadable"))?;
        while let Some(row) = rows
            .next()
            .map_err(|_| anyhow!("avatar rollback session row read failed"))?
        {
            let length: i64 = row
                .get(0)
                .map_err(|_| anyhow!("avatar rollback session record length is invalid"))?;
            ensure!(
                (0..=RECORD_MAX as i64).contains(&length),
                "avatar rollback session record exceeds its inspection bound"
            );
            let raw = row
                .get_ref(1)
                .map_err(|_| anyhow!("avatar rollback session record is unreadable"))?;
            let ValueRef::Text(bytes) = raw else {
                bail!("avatar rollback session record is not JSON text");
            };
            if adoption {
                validate_adoption(bytes)?;
            } else {
                validate_record(bytes)?;
            }
        }
    }
    Ok(())
}

fn validate_adoption(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= RECORD_MAX,
        "avatar rollback adoption receipt exceeds its inspection bound"
    );
    #[derive(serde::Deserialize)]
    struct Receipt<'a> {
        #[serde(borrow)]
        record: &'a RawValue,
    }
    let receipt: Receipt<'_> = serde_json::from_slice(bytes)
        .map_err(|_| anyhow!("avatar rollback adoption receipt is malformed"))?;
    validate_record(receipt.record.get().as_bytes())
}

fn validate_record(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= RECORD_MAX,
        "avatar rollback session record exceeds its inspection bound"
    );
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow!("avatar rollback session record JSON is malformed"))?;
    let record = value
        .as_object()
        .ok_or_else(|| anyhow!("avatar rollback session record is malformed"))?;
    ensure!(
        !record.contains_key("avatarMode"),
        "avatar rollback session mode has an unknown shape"
    );
    ensure!(record.get("created_by").is_none_or(Value::is_null) && !record.contains_key("createdBy"),
        "rollback blocked: session creation provenance requires the newer backend");
    match record.get("avatar_mode") {
        None | Some(Value::Null) => {}
        Some(Value::String(mode)) if mode == "fixed" => ensure!(
            record.get("avatar").is_some_and(|avatar| !avatar.is_null()),
            "avatar rollback blocked: an explicitly empty fixed avatar requires the newer backend"
        ),
        Some(Value::String(mode)) if mode == "inherit" => {
            bail!("avatar rollback blocked: inherited session avatars require the newer backend")
        }
        _ => bail!("avatar rollback session avatar mode is unknown or malformed"),
    }
    serde_json::from_slice::<crate::session::model::CutexSessionRecord>(bytes)
        .map_err(|_| anyhow!("avatar rollback session record has malformed current-schema data"))?;
    Ok(())
}

// The legacy file is bounded to 128 MiB once. Borrow each raw record from that
// buffer, enforce 8 MiB before parsing it, and never collect a second full store.
struct LegacyStore;
impl<'de> DeserializeSeed<'de> for LegacyStore {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for LegacyStore {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("legacy session store")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<(), M::Error> {
        let (mut sessions, mut adoptions) = (false, false);
        while let Some(key) = map.next_key::<String>()? {
            if key == "sessions" && !sessions {
                sessions = true;
                map.next_value_seed(LegacyRecords(false))?;
            } else if key == "human_adoption_receipts" && !adoptions {
                adoptions = true;
                map.next_value_seed(LegacyRecords(true))?;
            } else if key == "sessions"
                || key == "human_adoption_receipts"
                || key == "cutexSessionStore"
            {
                return Err(serde::de::Error::custom(
                    "incompatible session store marker or duplicate sessions",
                ));
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}
struct LegacyRecords(bool);
impl<'de> DeserializeSeed<'de> for LegacyRecords {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for LegacyRecords {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("legacy session records")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<(), M::Error> {
        while map.next_key::<IgnoredAny>()?.is_some() {
            let record: &RawValue = map.next_value()?;
            if self.0 {
                validate_adoption(record.get().as_bytes())
            } else {
                validate_record(record.get().as_bytes())
            }
            .map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    struct Home(std::path::PathBuf);
    impl Home {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("cutex-avatar-compat-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn account(home: &Path, options: Value) {
        fs::write(home.join("accounts.json"), serde_json::to_vec(&json!({
            "version":3, "accounts":[{"id":"synthetic", "name":"Profile", "options":options,
                "source":"/must-not-open-private-credential-reference"}], "active_account_id":null
        })).unwrap()).unwrap();
    }
    fn record(mode: Option<Value>, retired: bool) -> Value {
        let record = crate::session::model::CutexSessionRecord::new_at(
            "cutex.synthetic".into(),
            None,
            "synthetic-host".into(),
            "/synthetic".into(),
            None,
            "2026-10-24T00:00:00Z".into(),
        )
        .unwrap();
        let mut record = serde_json::to_value(record).unwrap();
        if let Some(mode) = mode {
            record["avatar_mode"] = mode;
        }
        if record.get("avatar_mode").and_then(Value::as_str) == Some("fixed") {
            record["avatar"] = json!({"kind":"emoji","value":"F"});
        }
        if retired {
            record["lifecycle"] = json!("retired");
        }
        record
    }
    fn legacy(home: &Path, record: Value) {
        fs::write(
            home.join(LEGACY),
            serde_json::to_vec(&json!({"sessions":{"cutex.synthetic":record}})).unwrap(),
        )
        .unwrap();
    }
    fn database(home: &Path, record: &Value, wal: bool) -> Connection {
        let db = Connection::open(home.join(DATABASE)).unwrap();
        db.execute_batch("PRAGMA user_version=1; CREATE TABLE sessions (archive_state TEXT,record_json TEXT NOT NULL); CREATE TABLE receipts (map TEXT,key TEXT,rev INTEGER,json TEXT);").unwrap();
        if wal {
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
                .unwrap();
        }
        db.execute(
            "INSERT INTO sessions VALUES('retired',?1)",
            [serde_json::to_string(record).unwrap()],
        )
        .unwrap();
        db
    }
    // Shared-memory lock/read marks may move during real WAL reads; all file
    // names, DB/WAL business bytes and file mtimes must otherwise stay intact.
    fn snapshot(home: &Path, skip_shm: bool) -> BTreeMap<String, (Vec<u8>, std::time::SystemTime)> {
        fs::read_dir(home)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                let bytes = if skip_shm && name.ends_with("-shm") {
                    Vec::new()
                } else {
                    fs::read(entry.path()).unwrap()
                };
                let modified = if skip_shm && name.ends_with("-shm") {
                    std::time::SystemTime::UNIX_EPOCH
                } else {
                    entry.metadata().unwrap().modified().unwrap()
                };
                (name, (bytes, modified))
            })
            .collect()
    }

    #[test]
    fn nonnull_profile_style_blocks_even_without_clock_and_never_writes() {
        for style in [
            json!({"kind":"letter"}),
            json!({"kind":"emoji","emojiThemes":null}),
            json!({"kind":"portrait","portraitThemes":[]}),
        ] {
            let home = Home::new();
            account(&home.0, json!({"avatar_style":style}));
            let before = snapshot(&home.0, false);
            assert!(preflight(&home.0)
                .unwrap_err()
                .to_string()
                .contains("Profile avatar style"));
            assert_eq!(snapshot(&home.0, false), before);
        }
    }
    #[test]
    fn cleared_style_clock_still_blocks_but_plain_null_and_absence_allow() {
        let home = Home::new();
        for clock in [json!("2026-10-24T00:00:00Z"), Value::Null] {
            account(
                &home.0,
                json!({"avatar_style":null,"appearance_updated_at":{"avatarStyle":clock}}),
            );
            let before = snapshot(&home.0, false);
            assert!(preflight(&home.0)
                .unwrap_err()
                .to_string()
                .contains("avatar-style history"));
            assert_eq!(snapshot(&home.0, false), before);
        }
        for options in [
            json!({}),
            json!({"avatar_style":null,"appearance_updated_at":{"color":"2026-10-24T00:00:00Z"}}),
        ] {
            account(&home.0, options);
            let before = snapshot(&home.0, false);
            preflight(&home.0).unwrap();
            assert_eq!(snapshot(&home.0, false), before);
        }
    }
    #[test]
    fn inherit_in_legacy_sessions_blocks_including_retired_records() {
        let home = Home::new();
        for retired in [false, true] {
            legacy(&home.0, record(Some(json!("inherit")), retired));
            let before = snapshot(&home.0, false);
            assert!(preflight(&home.0).is_err());
            assert_eq!(snapshot(&home.0, false), before);
        }
    }
    #[test]
    fn inherit_in_sqlite_blocks_even_when_legacy_json_is_empty() {
        let home = Home::new();
        fs::write(home.0.join(LEGACY), b"{\"sessions\":{}}").unwrap();
        let db = database(&home.0, &record(Some(json!("inherit")), true), false);
        drop(db);
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0)
            .unwrap_err()
            .to_string()
            .contains("inherited session avatars"));
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn sqlite_rejects_oversized_payload_before_materializing_the_result() {
        let home = Home::new();
        let db = database(&home.0, &record(Some(json!("fixed")), false), false);
        db.execute("UPDATE sessions SET record_json=?1", [" ".repeat(SQLITE_ROW_LIMIT as usize + 4096)]).unwrap();
        drop(db);
        let before = snapshot(&home.0, false);
        let error = preflight(&home.0).unwrap_err();
        assert!(error.to_string().contains("row read failed"), "SQLite must refuse at row stepping: {error}");
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn creation_provenance_is_kept_in_sessions_and_pending_actions_not_stripped() {
        for creator in [json!({"kind":"owner"}),json!({"kind":"system"}),json!({"kind":"agent","cutexSessionId":"cutex.director","role":"director"})] {
            let home=Home::new();let mut current=record(None,false);current["created_by"]=creator;
            legacy(&home.0,current.clone());let before=snapshot(&home.0,false);
            assert!(preflight(&home.0).is_err());assert_eq!(snapshot(&home.0,false),before);
            let db=database(&home.0,&current,false);drop(db);
            let before=snapshot(&home.0,false);assert!(preflight(&home.0).is_err());assert_eq!(snapshot(&home.0,false),before);
        }
        let home=Home::new();let path=home.0.join("runtime/agent-management/agent-management-v1.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut state=serde_json::to_value(crate::agent_management::AgentManagementSnapshot::empty()).unwrap();
        fs::write(&path,serde_json::to_vec(&state).unwrap()).unwrap();preflight(&home.0).unwrap();
        state["actions"]=json!({"new-action":{"created_by":{"kind":"agent","cutexSessionId":"cutex.director","role":"director"}}});
        let bytes=serde_json::to_vec(&state).unwrap();fs::write(&path,&bytes).unwrap();
        assert!(preflight(&home.0).unwrap_err().to_string().contains("captured creation provenance"));
        assert_eq!(fs::read(&path).unwrap(),bytes);assert!(!home.0.join(DATABASE).exists());
    }

    #[test]
    fn fixed_null_and_absent_session_modes_allow_both_formats() {
        for mode in [Some(json!("fixed")), Some(Value::Null), None] {
            let home = Home::new();
            legacy(&home.0, record(mode.clone(), false));
            let before = snapshot(&home.0, false);
            preflight(&home.0).unwrap();
            assert_eq!(snapshot(&home.0, false), before);
            let db = database(&home.0, &record(mode, true), false);
            drop(db);
            // Database is authoritative: do not scan an obsolete JSON backup.
            legacy(&home.0, record(Some(json!("inherit")), false));
            let before = snapshot(&home.0, false);
            preflight(&home.0).unwrap();
            assert_eq!(snapshot(&home.0, false), before);
        }
        // Unlike a legacy empty record, explicit fixed+empty pins letter
        // presentation. An older reader would drop that distinction and let
        // first-seen assignment replace it after a style change.
        let home = Home::new();
        let mut empty_fixed = record(Some(json!("fixed")), false);
        empty_fixed["avatar"] = Value::Null;
        assert!(validate_record(&serde_json::to_vec(&empty_fixed).unwrap()).unwrap_err().to_string().contains("empty fixed avatar"));
        legacy(&home.0, empty_fixed.clone());
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0).is_err(), "legacy errors intentionally redact the inner record diagnostic");
        assert_eq!(snapshot(&home.0, false), before);
        let db = database(&home.0, &empty_fixed, false);
        drop(db);
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0).unwrap_err().to_string().contains("empty fixed avatar"));
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn sentinel_without_database_unknown_modes_and_bad_json_fail_closed_without_leaking_values() {
        let home = Home::new();
        for bytes in [
            br#"{"cutexSessionStore":"migrated-to-sqlite","sessions":"do not load"}"#.as_slice(),
            b"{broken",
        ] {
            fs::write(home.0.join(LEGACY), bytes).unwrap();
            let before = snapshot(&home.0, false);
            assert!(preflight(&home.0).is_err());
            assert_eq!(snapshot(&home.0, false), before);
        }
        for mode in [json!("private-mode-canary"), json!(7), json!({})] {
            legacy(&home.0, record(Some(mode), false));
            let before = snapshot(&home.0, false);
            let error = preflight(&home.0).unwrap_err().to_string();
            assert!(!error.contains("private-mode-canary"));
            assert_eq!(snapshot(&home.0, false), before);
        }
    }
    #[test]
    fn malformed_accounts_unknown_versions_and_corrupt_database_never_seed_or_rewrite() {
        let home = Home::new();
        for bytes in [
            b"{bad".as_slice(),
            br#"{"version":99,"accounts":[]}"#,
            br#"{"version":3,"accounts":[{"options":null}]}"#,
        ] {
            fs::write(home.0.join("accounts.json"), bytes).unwrap();
            let before = snapshot(&home.0, false);
            assert!(preflight(&home.0).is_err());
            assert_eq!(snapshot(&home.0, false), before);
        }
        fs::remove_file(home.0.join("accounts.json")).unwrap();
        fs::write(home.0.join(DATABASE), b"not sqlite").unwrap();
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0).is_err());
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn empty_home_is_allowed_but_missing_home_and_nonfiles_are_not_created_or_followed() {
        let home = Home::new();
        preflight(&home.0).unwrap();
        assert_eq!(fs::read_dir(&home.0).unwrap().count(), 0);
        let missing = home.0.join("missing");
        assert!(preflight(&missing).is_err());
        assert!(!missing.exists());
        fs::create_dir(home.0.join("accounts.json")).unwrap();
        assert!(preflight(&home.0).is_err());
    }
    #[test]
    fn closed_wal_database_with_no_sidecars_is_read_without_creating_any() {
        let home = Home::new();
        let db = database(&home.0, &record(Some(json!("fixed")), false), true);
        drop(db);
        assert!(!home.0.join(format!("{DATABASE}-wal")).exists());
        assert!(!home.0.join(format!("{DATABASE}-shm")).exists());
        let before = snapshot(&home.0, false);
        preflight(&home.0).unwrap();
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn existing_wal_is_not_ignored_and_neither_main_database_nor_wal_is_changed() {
        let home = Home::new();
        let db = database(&home.0, &record(Some(json!("fixed")), false), true);
        db.execute(
            "UPDATE sessions SET record_json=?1",
            [serde_json::to_string(&record(Some(json!("inherit")), true)).unwrap()],
        )
        .unwrap();
        let before = snapshot(&home.0, true);
        assert!(preflight(&home.0)
            .unwrap_err()
            .to_string()
            .contains("inherited session avatars"));
        assert_eq!(snapshot(&home.0, true), before);
        drop(db);
    }
    #[test]
    fn incomplete_or_linked_wal_boundaries_fail_before_sqlite_opens() {
        let home = Home::new();
        let db = database(&home.0, &record(Some(json!("fixed")), false), false);
        drop(db);
        fs::write(home.0.join(format!("{DATABASE}-wal")), b"incomplete").unwrap();
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0).is_err());
        assert_eq!(snapshot(&home.0, false), before);
        fs::write(home.0.join(format!("{DATABASE}-shm")), b"invalid").unwrap();
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0).is_err());
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn bounded_accounts_legacy_and_record_reads_fail_closed() {
        let home = Home::new();
        let accounts = fs::File::create(home.0.join("accounts.json")).unwrap();
        accounts.set_len(ACCOUNTS_MAX + 1).unwrap();
        drop(accounts);
        assert!(preflight(&home.0).is_err());
        fs::remove_file(home.0.join("accounts.json")).unwrap();
        let legacy = fs::File::create(home.0.join(LEGACY)).unwrap();
        legacy.set_len(LEGACY_MAX + 1).unwrap();
        drop(legacy);
        assert!(preflight(&home.0).is_err());
        fs::remove_file(home.0.join(LEGACY)).unwrap();
        assert!(validate_record(&vec![b' '; RECORD_MAX + 1]).is_err());
        let db = database(&home.0, &record(Some(json!("fixed")), false), false);
        db.execute(
            "UPDATE sessions SET record_json=?1",
            [" ".repeat(RECORD_MAX + 1)],
        )
        .unwrap();
        drop(db);
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0).is_err());
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn historical_adoption_inherit_blocks_even_when_current_session_is_fixed() {
        let home = Home::new();
        let current = record(Some(json!("fixed")), false);
        let receipt = json!({"record":record(Some(json!("inherit")), true)});
        fs::write(
            home.0.join(LEGACY),
            serde_json::to_vec(&json!({
                "sessions":{"cutex.synthetic":current}, "human_adoption_receipts":{"old":receipt}
            }))
            .unwrap(),
        )
        .unwrap();
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0).is_err());
        assert_eq!(snapshot(&home.0, false), before);
        let db = database(&home.0, &record(Some(json!("fixed")), false), false);
        db.execute(
            "INSERT INTO receipts VALUES('human_adoption','old',1,?1)",
            [serde_json::to_string(&receipt).unwrap()],
        )
        .unwrap();
        drop(db);
        let before = snapshot(&home.0, false);
        assert!(preflight(&home.0)
            .unwrap_err()
            .to_string()
            .contains("inherited session avatars"));
        assert_eq!(snapshot(&home.0, false), before);
    }
    #[test]
    fn fixed_adoption_records_allow_and_unrelated_receipt_request_bodies_are_not_records() {
        let home = Home::new();
        let receipt = json!({"record":record(Some(json!("fixed")), true)});
        fs::write(
            home.0.join(LEGACY),
            serde_json::to_vec(&json!({"human_adoption_receipts":{"old":receipt}})).unwrap(),
        )
        .unwrap();
        preflight(&home.0).unwrap();
        let db = database(&home.0, &record(None, false), false);
        db.execute(
            "INSERT INTO receipts VALUES('human_adoption','old',1,?1)",
            [serde_json::to_string(&receipt).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO receipts VALUES('other_map','request',1,?1)",
            [r#"{"record":{"avatar_mode":"inherit"}}"#],
        )
        .unwrap();
        drop(db);
        let before = snapshot(&home.0, false);
        preflight(&home.0).unwrap();
        assert_eq!(snapshot(&home.0, false), before);
        assert!(validate_adoption(br#"{"record":{}}"#).is_err());
        assert!(validate_adoption(br#"{"request":{}}"#).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_home_accounts_database_and_sidecars_are_rejected() {
        use std::os::unix::fs::symlink;
        let home = Home::new();
        let target = home.0.join("plain");
        fs::write(&target, b"unchanged").unwrap();
        for name in [
            "accounts.json",
            DATABASE,
            "cutex-sessions.sqlite3-wal",
            "cutex-sessions.sqlite3-shm",
        ] {
            let path = home.0.join(name);
            symlink(&target, &path).unwrap();
            assert!(preflight(&home.0).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"unchanged");
            fs::remove_file(path).unwrap();
        }
        let alias = home.0.with_extension("alias");
        symlink(&home.0, &alias).unwrap();
        assert!(preflight(&alias).is_err());
        fs::remove_file(alias).unwrap();
    }
}
