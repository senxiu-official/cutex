//! Resolve the current native rollout rather than an obsolete pre-revert file.
use anyhow::{ensure, Context};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub fn metadata(path: &Path) -> anyhow::Result<serde_json::Value> {
    let first = std::io::BufReader::new(std::fs::File::open(path)?)
        .lines()
        .next()
        .context("empty native history")??;
    let value: serde_json::Value = serde_json::from_str(&first)?;
    ensure!(
        value["type"] == "session_meta",
        "native history metadata missing"
    );
    Ok(value["payload"].clone())
}

pub fn current(home: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let db = home.join("state_5.sqlite");
    if db.is_file() {
        let connection = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let row: Option<String> = connection
            .query_row(
                "SELECT rollout_path FROM threads WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(path) = row {
            return validate(home, Path::new(&path), id);
        }
    }
    let files = files(home)?;
    let candidates: Vec<_> = files
        .into_iter()
        .filter(|p| p.starts_with(home.join("sessions")))
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.ends_with(&format!("-{id}.jsonl")) || name.contains(&format!("-{id}_"))
        })
        .collect();
    ensure!(
        candidates.len() == 1,
        "native history missing or ambiguous; current native index required"
    );
    validate(home, &candidates[0], id)
}

fn validate(home: &Path, path: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let path = path.canonicalize()?;
    ensure!(
        path.starts_with(home.join("sessions").canonicalize()?),
        "history outside active native sessions"
    );
    ensure!(
        metadata(&path)?["id"] == id,
        "native history identity mismatch"
    );
    Ok(path)
}
fn files(home: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut pending = vec![home.join("sessions")];
    if home.join("archived_sessions").is_dir() {
        pending.push(home.join("archived_sessions"));
    }
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else if entry.file_type()?.is_file() {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

/// The immutable prefix a rollout inherits (`session_meta.history_base`, set
/// by native revert and fork): the prefix file and the byte length of it that
/// belongs to this history. `None` for a self-contained rollout.
pub fn history_base(home: &Path, source: &Path) -> anyhow::Result<Option<(PathBuf, u64)>> {
    let meta = metadata(source)?;
    let Some(id) = meta["history_base"]["thread_id"].as_str() else {
        return Ok(None);
    };
    uuid::Uuid::parse_str(id)?;
    let end = meta["history_base"]["end_byte_offset"]
        .as_u64()
        .context("history base end offset missing")?;
    let home = home.canonicalize()?;
    let matches: Vec<_> = files(&home)?
        .into_iter()
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.ends_with(&format!("-{id}.jsonl")) || name.ends_with(&format!("_{id}.jsonl"))
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "history dependency missing or ambiguous: {id}"
    );
    let prefix = matches[0].canonicalize()?;
    ensure!(
        prefix.starts_with(&home),
        "history dependency outside native home"
    );
    Ok(Some((prefix, end)))
}

/// Copy every immutable source referenced by this rollout, for manual recovery.
/// The native API remains responsible for the actual history mutation.
pub fn recovery_sources(home: &Path, source: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let home = home.canonicalize()?;
    let mut source = source.canonicalize()?;
    let mut result = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut candidates: Option<Vec<PathBuf>> = None;
    loop {
        ensure!(
            source.starts_with(&home),
            "history dependency outside native home"
        );
        ensure!(
            seen.insert(source.clone()),
            "cyclic native history reference"
        );
        let meta = metadata(&source)?;
        result.push(source);
        let Some(id) = meta["history_base"]["thread_id"].as_str() else {
            break;
        };
        uuid::Uuid::parse_str(id)?;
        let paths = match &candidates {
            Some(paths) => paths,
            None => candidates.insert(files(&home)?),
        };
        let matches: Vec<_> = paths
            .iter()
            .filter(|p| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                name.ends_with(&format!("-{id}.jsonl")) || name.ends_with(&format!("_{id}.jsonl"))
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "history dependency missing or ambiguous: {id}"
        );
        source = matches[0].canonicalize()?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_selects_reverted_history_and_backup_follows_immutable_ancestors() {
        let root =
            std::env::temp_dir().join(format!("cutex-history-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        // current()/recovery_sources() return canonical paths on every platform.
        let root = root.canonicalize().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let rollout = uuid::Uuid::new_v4().to_string();
        let old = root
            .join("sessions")
            .join(format!("rollout-date-{id}.jsonl"));
        let current_path = root
            .join("sessions")
            .join(format!("rollout-date-{id}_{rollout}.jsonl"));
        for (path, base) in [
            (&old, serde_json::Value::Null),
            (&current_path, serde_json::json!({"thread_id":id})),
        ] {
            std::fs::write(
                path,
                serde_json::json!({"type":"session_meta","payload":{"id":id,"history_base":base}})
                    .to_string(),
            )
            .unwrap();
        }
        assert!(current(&root, &id).is_err()); // no index: never guess the obsolete file
        let connection = Connection::open(root.join("state_5.sqlite")).unwrap();
        connection
            .execute_batch("CREATE TABLE threads (id TEXT, rollout_path TEXT)")
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1,?2)",
                [&id, current_path.to_str().unwrap()],
            )
            .unwrap();
        assert_eq!(current(&root, &id).unwrap(), current_path);
        assert_eq!(
            recovery_sources(&root, &current_path).unwrap(),
            vec![current_path.clone(), old.clone()]
        );
        std::fs::remove_file(&current_path).unwrap();
        assert!(current(&root, &id).is_err()); // never fall back to the deleted turns
        drop(connection);
        std::fs::remove_dir_all(root).unwrap();
    }
}
