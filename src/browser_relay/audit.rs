//! Metadata-only browser audit group, reusing the existing rotating AuditLog.
//! Queue/rotation loss is counted; this log is never an idempotency authority.
use super::private_store::Directory;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, OnceLock,
    },
};
const MAX_BYTES: u64 = 64 * 1024 * 1024;
static DROPPED: AtomicU64 = AtomicU64::new(0);
static WRITER: OnceLock<Option<mpsc::SyncSender<Record>>> = OnceLock::new();
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub time: DateTime<Utc>,
    pub request_id: String,
    pub device_id: String,
    pub generation: String,
    pub lease_id: Option<String>,
    pub operation_kind: String,
    pub status: super::commands::Status,
    pub error: Option<&'static str>,
    pub duration_ms: u64,
    pub bytes: usize,
    pub late_result: u64,
    pub dropped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<Cleanup>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cleanup {
    pub session_sha256: Option<String>,
    pub outcome_unknown: u64,
    pub abandoned_forwarded: u64,
}
fn allowed_name(name: &str) -> bool {
    name == "agent-bus-audit.jsonl"
        || name == "agent-bus-audit.lock"
        || name.starts_with("agent-bus-audit-")
            && name.ends_with(".jsonl")
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
}
fn day_name(name: &str) -> Option<chrono::NaiveDate> {
    if name.len() != 8 || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    chrono::NaiveDate::parse_from_str(name, "%Y%m%d").ok()
}
fn prune(path: &Path, now: DateTime<Utc>, reserve: u64) -> anyhow::Result<()> {
    let root = Directory::open(path)?;
    let mut files = Vec::new();
    let cutoff = now.date_naive() - chrono::Duration::days(6);
    for day in root.names(16)? {
        let Some(date) = day_name(&day) else {
            continue;
        };
        let dir = Directory::open(&path.join(&day))?;
        for name in dir.names(128)? {
            if !allowed_name(&name) || name.ends_with(".lock") {
                continue;
            }
            let meta = std::fs::symlink_metadata(dir.path.join(&name))?;
            anyhow::ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "invalid browser audit file"
            );
            if date < cutoff || date > now.date_naive() {
                dir.remove(&name)?;
                DROPPED.fetch_add(1, Ordering::Relaxed);
            } else {
                files.push((date, meta.modified()?, dir.path.clone(), name, meta.len()));
            }
        }
        if date < cutoff || date > now.date_naive() {
            for name in dir.names(128)? {
                if name == "agent-bus-audit.lock" {
                    dir.remove(&name)?;
                }
            }
            drop(dir);
            let _ = std::fs::remove_dir(path.join(&day));
        }
    }
    files.sort_by_key(|(date, modified, _, _, _)| (*date, *modified));
    let mut total: u64 = files.iter().map(|f| f.4).sum();
    for (_, _, path, name, bytes) in files {
        if total.saturating_add(reserve) <= MAX_BYTES {
            break;
        }
        Directory::open(&path)?.remove(&name)?;
        total = total.saturating_sub(bytes);
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}
fn append(path: &Path, record: &Record) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(record)?;
    anyhow::ensure!(bytes.len() <= 4096, "browser audit record budget");
    prune(path, record.time, 4096)?;
    let dir = Directory::open(&path.join(record.time.format("%Y%m%d").to_string()))?;
    let settings = crate::config::agent_bus_audit::AgentBusAuditSettings {
        max_file_mb: 8,
        keep_files: 7,
        gzip: false,
        coalesce_polls_secs: 0,
    };
    let mut log = crate::agent_bus::audit::AuditLog::new(dir.path.clone(), settings);
    log.append(&serde_json::from_slice::<Value>(&bytes)?)?;
    dir.validate()?;
    prune(path, record.time, 0)
}
#[cfg(not(test))]
pub fn record(mut record: Record) {
    let writer = WRITER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Record>(256);
        std::thread::Builder::new()
            .name("browser-audit".into())
            .spawn(move || {
                while let Ok(record) = rx.recv() {
                    let result = crate::config::paths::runtime_dir()
                        .and_then(|root| append(&root.join("browser/audit"), &record));
                    if result.is_err() {
                        DROPPED.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
            .ok()?;
        Some(tx)
    });
    record.dropped = DROPPED.swap(0, Ordering::Relaxed);
    if writer
        .as_ref()
        .is_none_or(|tx| tx.try_send(record).is_err())
    {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}
// Unit tests explicitly exercise an isolated directory, never start a global
// background writer whose home might change under another test fixture.
#[cfg(test)]
pub fn record(_record: Record) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_audit_whitelist_and_date_retention() {
        let temp = std::env::temp_dir().join(format!("browser-audit-{}", uuid::Uuid::new_v4()));
        let now = Utc::now();
        let record = Record {
            time: now - chrono::Duration::days(8),
            request_id: "r".into(),
            device_id: "d".into(),
            generation: "g".into(),
            lease_id: Some("l".into()),
            operation_kind: "tab.act".into(),
            status: super::super::commands::Status::Succeeded,
            error: None,
            duration_ms: 1,
            bytes: 10,
            late_result: 0,
            dropped: 0,
            cleanup: None,
        };
        append(&temp, &record).unwrap();
        let value = serde_json::to_value(&record).unwrap();
        for name in ["title", "url", "image", "text", "credential", "arguments"] {
            assert!(value.get(name).is_none());
        }
        prune(&temp, now, 0).unwrap();
        assert!(Directory::open(&temp)
            .unwrap()
            .names(16)
            .unwrap()
            .is_empty());
        std::fs::remove_dir(temp).unwrap();
    }
}
