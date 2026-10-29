//! Shared direct-export guards with separate exact r263/r264 archive readers.
//! No store opens, recovery, locks or writes to the original home.
//! SQLite checks receive only the caller's disposable WAL-preserving snapshot.
use anyhow::{ensure, Context};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::path::{Path, PathBuf};
use super::r265_rollback::Reader;

const AUTHORITY: &str = "runtime/agent-management/v1/agent-management-v1.json";
const RETRY: &str = "runtime/model-overload-retry";
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Streaming fingerprint: no unbounded DB buffer and no source lock/ACL write.
pub(super) fn fingerprint(path: &Path) -> anyhow::Result<Option<(u64, (u64, u64), Vec<u8>)>> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    for component in path.ancestors() {
        match std::fs::symlink_metadata(component) {
            Ok(meta) => {
                ensure!(!meta.file_type().is_symlink(), "linked rollback input");
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse rollback input"
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "nonregular rollback input");
    if path.extension().is_some_and(|e| e == "json") {
        ensure!(metadata.len() <= MAX_BYTES, "oversize rollback JSON");
    }
    #[cfg(windows)]
    let identity = crate::platform::private_fs::identity(&file)?.publication_key();
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(any(windows, unix)))]
    let identity = (0, 0);
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    ensure!(
        metadata.len() == file.metadata()?.len()
            && metadata.modified()? == file.metadata()?.modified()?,
        "rollback input changed during read"
    );
    Ok(Some((metadata.len(), identity, hash.finalize().to_vec())))
}

// Value's normal Deserialize accepts duplicate keys. Reject them at every depth.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("unique-key finite JSON")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut m: M) -> Result<Unique, M::Error> {
                let mut out = serde_json::Map::new();
                while let Some((key, value)) = m.next_entry::<String, Unique>()? {
                    if out.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(Unique(Value::Object(out)))
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(self, mut s: S) -> Result<Unique, S::Error> {
                let mut out = Vec::new();
                while let Some(v) = s.next_element::<Unique>()? {
                    out.push(v.0);
                }
                Ok(Unique(Value::Array(out)))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Unique, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Unique(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite JSON"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
        }
        d.deserialize_any(Visitor)
    }
}
pub(super) fn decode(bytes: &[u8]) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice::<Unique>(bytes)?.0)
}
pub(super) fn object(v: &Value) -> anyhow::Result<&serde_json::Map<String, Value>> {
    v.as_object().context("expected JSON object")
}
fn exact(v: &Value, keys: &[&str]) -> anyhow::Result<()> {
    let obj = object(v)?;
    ensure!(
        obj.len() == keys.len() && keys.iter().all(|k| obj.contains_key(*k)),
        "unknown/missing rollback contract fields"
    );
    Ok(())
}

#[cfg(test)]
mod reader_dispatch_tests {
    use super::*;
    use serde_json::json;

    fn old_receipt() -> Value {
        json!({"request":{"action_id":"fixture","review":{"cutex_session_id":"cutex.fixture",
            "formal_name":"Fixture","operation":"archive"}},
            "request_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "stage":"committed","result":null,"error":null,"committed_at":"2026-10-26T00:00:00Z"})
    }

    #[test]
    fn r263_exact_receipt_boundary_survives_r264_dispatch() {
        let old = old_receipt();
        receipt(&old, Reader::R263).unwrap();
        receipt(&old, Reader::R264).unwrap();
        for (pointer, value) in [
            ("/stage", json!("permanent_retirement_committed")),
            ("/request/review/operation", json!("permanent_retire")),
        ] {
            let mut newer = old.clone();
            *newer.pointer_mut(pointer).unwrap() = value;
            assert!(receipt(&newer, Reader::R263).is_err());
            receipt(&newer, Reader::R264).unwrap();
        }
        for (field, value) in [("archiveKind", json!("soft")), ("restorable", json!(null))] {
            let mut newer = old.clone();
            newer[field] = value;
            assert!(receipt(&newer, Reader::R263).is_err());
            receipt(&newer, Reader::R264).unwrap();
        }
        let mut unknown = old;
        unknown["future"] = json!(true);
        assert!(receipt(&unknown, Reader::R263).is_err());
        assert!(receipt(&unknown, Reader::R264).is_err());
    }

    #[test]
    fn marker_dispatch_never_relaxes_r263() {
        for reader in [Reader::R263, Reader::R264] {
            marker(&json!({}), reader).unwrap();
        }
        let known = json!({"archive_storage_contract_v1":{
            "minimum_reader_release":"r264","binary_only_rollback_blocked_to":["r263"],
            "reason":"archive_receipt_fields_and_permanent_enum_variants"}});
        assert!(marker(&known, Reader::R263).is_err());
        marker(&known, Reader::R264).unwrap();
        for value in [json!(null), json!({}), json!({"minimum_reader_release":"r265"})] {
            let unknown = json!({"archive_storage_contract_v1":value});
            assert!(marker(&unknown, Reader::R263).is_err());
            assert!(marker(&unknown, Reader::R264).is_err());
        }
    }

    #[test]
    fn shared_json_decoder_rejects_duplicate_keys_at_receipt_depth() {
        assert!(decode(br#"{"agent_archive_receipts":{"fixture":{"stage":"committed","stage":"uncertain"}}}"#).is_err());
        assert!(decode(br#"{"archive_storage_contract_v1":{"minimum_reader_release":"r264","minimum_reader_release":"r265"}}"#).is_err());
        let old = old_receipt();
        assert_eq!(decode(&serde_json::to_vec(&old).unwrap()).unwrap(), old);
    }
}
fn optional_json(path: &Path) -> anyhow::Result<Option<Value>> {
    match std::fs::File::open(path) {
        Ok(file) => {
            use std::io::Read;
            ensure!(
                file.metadata()?.len() <= MAX_BYTES,
                "oversize rollback JSON"
            );
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() as u64 <= MAX_BYTES, "oversize rollback JSON");
            Ok(Some(decode(&bytes)?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read rollback guard JSON"),
    }
}

pub(super) fn paths(home: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = vec![
        home.join(AUTHORITY),
        home.join("cutex-sessions.json"),
        home.join("cutex-sessions.sqlite3"),
        home.join("runtime/management-v2/pending-stops.sqlite"),
        home.join("runtime/management-v2/pending-stops.worker.lock"),
    ];
    for db in [
        home.join("cutex-sessions.sqlite3"),
        home.join("runtime/management-v2/pending-stops.sqlite"),
    ] {
        for suffix in ["-wal", "-shm", "-journal"] {
            out.push(PathBuf::from(format!("{}{suffix}", db.display())));
        }
    }
    // Validate directory type/link before iterating (the caller validates every
    // returned file and ancestor too); unknown partial atomic writes block.
    let dir = home.join(RETRY);
    for path in dir.ancestors() {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "linked/non-directory retry ancestor"
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "retry reparse ancestor"
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("inspect retry directory"),
        }
    }
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                ensure!(
                    matches!(
                        entry.file_name().to_str(),
                        Some("state-v1.json" | "state-v1.lock" | "state-v1.disabled")
                    ),
                    "unknown retry directory member"
                );
                out.push(entry.path());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("read retry directory"),
    }
    Ok(out)
}

fn retry(home: &Path) -> anyhow::Result<()> {
    let dir = home.join(RETRY);
    let Some(state) = optional_json(&dir.join("state-v1.json"))? else {
        ensure!(
            !dir.join("state-v1.disabled").try_exists()?,
            "disabled retry without journal"
        );
        return Ok(());
    };
    exact(&state, &["version", "sessions"])?;
    ensure!(
        state["version"].as_u64() == Some(1),
        "unknown retry version"
    );
    for (id, entry) in object(&state["sessions"])? {
        ensure!(!id.is_empty(), "empty retry session");
        exact(
            entry,
            &[
                "revision",
                "plan",
                "last",
                "seen",
                "blockedGeneration",
                "settlement",
            ],
        )?;
        ensure!(entry["revision"].is_u64(), "invalid retry revision");
        ensure!(
            entry["seen"]
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
            "invalid retry seen history"
        );
        ensure!(
            entry["blockedGeneration"].is_null() || entry["blockedGeneration"].is_u64(),
            "invalid blocked generation"
        );
        // last is intentionally opaque historical Value in the real writer.
        // No loose Plan decoder/defaults: every non-null obligation is blocked.
        ensure!(
            entry["plan"].is_null() && entry["settlement"].is_null(),
            "unfinished retry plan/settlement; disabled is not an exemption"
        );
    }
    Ok(())
}
fn receipt(value: &Value, reader: Reader) -> anyhow::Result<()> {
    if reader == Reader::R264 {
        return super::r265_rollback::archive_receipt(value);
    }
    exact(
        value,
        &[
            "request",
            "request_sha256",
            "stage",
            "result",
            "error",
            "committed_at",
        ],
    )?;
    ensure!(
        matches!(
            value["stage"].as_str(),
            Some("prepared" | "stopped" | "committed" | "stopped_not_archived" | "uncertain")
        ),
        "new/unknown archive stage"
    );
    ensure!(
        matches!(
            value
                .pointer("/request/review/operation")
                .and_then(Value::as_str),
            Some("archive" | "restore")
        ),
        "new/unknown archive operation"
    );
    // Current decoder validates IDs, digests, timestamps and result records;
    // the closed field/stage/operation checks above enforce the r263 boundary.
    serde_json::from_value::<crate::agent_management::AgentArchiveReceipt>(value.clone())?;
    Ok(())
}
fn marker(value: &Value, reader: Reader) -> anyhow::Result<()> {
    if reader == Reader::R264 {
        return super::r265_rollback::archive_marker(value);
    }
    ensure!(
        !object(value)?
            .keys()
            .any(|key| key.starts_with("archive_storage_contract_")),
        "minimum archive reader marker requires r264"
    );
    Ok(())
}
fn receipts(value: Option<&Value>, reader: Reader) -> anyhow::Result<()> {
    if let Some(value) = value {
        for receipt_value in object(value)?.values() {
            receipt(receipt_value, reader)?;
        }
    }
    Ok(())
}
fn archive_json(home: &Path, reader: Reader) -> anyhow::Result<()> {
    if let Some(state) = optional_json(&home.join(AUTHORITY))? {
        marker(&state, reader)?;
        ensure!(
            matches!(
                state["schema"].as_str(),
                Some("cutex/agent-management-store/v1" | "cutex/agent-management-store/v2")
            ),
            "unknown authority schema"
        );
        serde_json::from_value::<crate::agent_management::AgentManagementSnapshot>(state.clone())?;
        receipts(state.get("agent_archive_actions"), reader)?;
        receipts(state.get("agent_archive_audit"), reader)?;
        if let Some(actions) = state.get("actions") {
            for action in object(actions)?.values() {
                if let Some(embedded) = action.pointer("/response/outcome/receipt") {
                    if reader == Reader::R264 {
                        super::r265_rollback::management_receipt(embedded)?;
                        continue;
                    }
                    exact(
                        embedded,
                        &[
                            "schema",
                            "action_id",
                            "request_sha256",
                            "operation",
                            "project_id",
                            "completed_at",
                            "result",
                        ],
                    )?;
                    ensure!(
                        embedded["schema"] == "cutex/agent-management-receipt/v1",
                        "unknown management receipt schema"
                    );
                    ensure!(
                        !matches!(
                            embedded["operation"].as_str(),
                            Some("archive" | "restore" | "permanent_retire")
                        ),
                        "new management archive operation"
                    );
                }
            }
        }
    }
    if let Some(state) = optional_json(&home.join("cutex-sessions.json"))? {
        marker(&state, reader)?;
        if state.get("cutexSessionStore").is_some() {
            ensure!(
                object(&state)?.keys().all(|k| matches!(
                    k.as_str(),
                    "cutexSessionStore"
                        | "database"
                        | "preSqliteReference"
                        | "migratedAt"
                        | "storeRevision"
                        | "sessions"
                )),
                "unknown session sentinel fields"
            );
            ensure!(
                state["cutexSessionStore"] == "migrated-to-sqlite"
                    && state["database"] == "cutex-sessions.sqlite3"
                    && home.join("cutex-sessions.sqlite3").is_file(),
                "invalid session SQLite sentinel"
            );
        } else {
            object(
                state
                    .get("sessions")
                    .context("missing legacy session map")?,
            )?;
            serde_json::from_value::<crate::session::model::CutexSessionStore>(state.clone())?;
            receipts(state.get("agent_archive_receipts"), reader)?;
        }
    }
    Ok(())
}

fn open(db: &Path) -> anyhow::Result<rusqlite::Connection> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(check == "ok", "rollback database integrity failure");
    Ok(conn)
}
fn schema(
    db: &rusqlite::Connection,
) -> anyhow::Result<Vec<(String, String, String, Option<String>)>> {
    Ok(db
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name,tbl_name")?
        .query_map([], |r| {
            let sql: Option<String> = r.get(3)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                sql.map(|s| {
                    s.chars()
                        .filter(|c| !c.is_whitespace())
                        .collect::<String>()
                        .to_lowercase()
                }),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
const PENDING_DDL:&str="CREATE TABLE pending_stop (id INTEGER PRIMARY KEY, session TEXT NOT NULL, generation TEXT NOT NULL, receipt_path TEXT NOT NULL, host TEXT NOT NULL, message TEXT NOT NULL, fallback INTEGER NOT NULL, due INTEGER NOT NULL DEFAULT 0, UNIQUE(session,generation)); CREATE INDEX pending_stop_due ON pending_stop(due,id);";
pub(super) fn validate_snapshot(home: &Path, reader: Reader) -> anyhow::Result<()> {
    retry(home)?;
    archive_json(home, reader)?;
    let path = home.join("runtime/management-v2/pending-stops.sqlite");
    if path.try_exists()? {
        let db = open(&path)?;
        let expected = rusqlite::Connection::open_in_memory()?;
        expected.execute_batch(PENDING_DDL)?;
        ensure!(
            schema(&db)? == schema(&expected)?,
            "unknown pending-stop schema/index set"
        );
        for pragma in ["PRAGMA user_version", "PRAGMA application_id"] {
            ensure!(
                db.query_row(pragma, [], |r| r.get::<_, i64>(0))? == 0,
                "unknown pending-stop version"
            );
        }
        ensure!(
            db.query_row("SELECT count(*) FROM pending_stop", [], |r| r
                .get::<_, i64>(0))?
                == 0,
            "pending terminal-event obligations remain"
        );
    } else {
        ensure!(
            !home
                .join("runtime/management-v2/pending-stops.worker.lock")
                .try_exists()?,
            "orphan pending-stop worker lock"
        );
    }
    let path = home.join("cutex-sessions.sqlite3");
    if path.try_exists()? {
        let db = open(&path)?;
        ensure!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? == 1,
            "unknown session database version"
        );
        // Match the predecessor's load prerequisites without opening its
        // repository (which could import, lock or write the original home).
        let revision: String = db
            .query_row(
                "SELECT value FROM meta WHERE key = 'store_revision'",
                [],
                |r| r.get(0),
            )
            .context("cutex session database has no store revision")?;
        revision
            .parse::<u64>()
            .context("invalid cutex session store revision")?;
        let mut sessions = db.prepare("SELECT key,rev,record_json FROM sessions")?;
        for row in sessions.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
        })? {
            let (_, revision, raw) = row?;
            u64::try_from(revision).context("negative cutex session row revision")?;
            serde_json::from_value::<crate::session::model::CutexSessionRecord>(decode(
                raw.as_bytes(),
            )?)?;
        }
        let mut query = db.prepare("SELECT map,key,rev,json FROM receipts")?;
        for row in query.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?, r.get::<_, String>(3)?,
            ))
        })? {
            let (map, _, revision, raw) = row?;
            u64::try_from(revision).context("negative cutex receipt row revision")?;
            ensure!(
                matches!(
                    map.as_str(),
                    "native_stop"
                        | "explicit_launch"
                        | "human_adoption"
                        | "agent_archive"
                        | "formal_name"
                        | "explicit_launch_tombstone"
                ),
                "unknown session receipt map"
            );
            let value = decode(raw.as_bytes())?;
            object(&value)?;
            if map == "agent_archive" {
                receipt(&value, reader)?;
            }
            // Reuse the store's typed receipt maps without opening its store,
            // importing JSON or constructing a permissive raw-JSON projection.
            let field = match map.as_str() {
                "native_stop" => "native_stop_receipts",
                "explicit_launch" => "explicit_launch_receipts",
                "human_adoption" => "human_adoption_receipts",
                "agent_archive" => "agent_archive_receipts",
                "formal_name" => "formal_name_receipts",
                _ => "explicitLaunchTombstones",
            };
            serde_json::from_value::<crate::session::model::CutexSessionStore>(
                serde_json::json!({field:{"rollback-validation":value},"sessions":{}}),
            )?;
        }
    }
    Ok(())
}
