//! Instance identity and timestamps for Management clients (cutex-desktop
//! docs/research/cutex-mobile-api.md §Identity).
//!
//! `hostId` is the machine name and stays what it is: session records, event
//! envelopes and Agent Bus records use it for locality. Two cutex homes on one
//! machine (live and dev) share it, so clients that remember a daemon key it
//! by `instanceId` instead: a random id created once per cutex home
//! (`<cutex home>/instance-id`) and never changed.

use std::sync::OnceLock;

static INSTANCE_ID: OnceLock<String> = OnceLock::new();

const FILE_NAME: &str = "instance-id";

fn valid(id: &str) -> bool {
    id.len() >= 8 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn load_or_create(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let id = text.trim();
        if valid(id) {
            return Ok(id.to_string());
        }
    }
    let id = format!("ci-{}", uuid::Uuid::new_v4().simple());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename; a concurrent first start that wins keeps its id.
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, format!("{id}\n"))?;
    if let Ok(text) = std::fs::read_to_string(path) {
        if valid(text.trim()) {
            let _ = std::fs::remove_file(&temp);
            return Ok(text.trim().to_string());
        }
    }
    std::fs::rename(&temp, path)?;
    Ok(id)
}

/// This cutex home's instance id (`ci-<32 hex>`), created on first use.
pub fn instance_id() -> String {
    INSTANCE_ID
        .get_or_init(|| {
            if cfg!(test) {
                return "ci-test-instance".to_string();
            }
            let result = crate::config::paths::config_dir()
                .and_then(|dir| load_or_create(&dir.join(FILE_NAME)));
            match result {
                Ok(id) => id,
                Err(error) => {
                    eprintln!("\x1b[33mwarning:\x1b[0m instance id unavailable ({error:#}); using a temporary one");
                    format!("ci-tmp-{}", uuid::Uuid::new_v4().simple())
                }
            }
        })
        .clone()
}

/// RFC 3339 UTC with `Z` and milliseconds, the one timestamp format of the
/// mobile-facing resources (summary, pairing, client tokens).
pub fn format_utc(time: chrono::DateTime<chrono::Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn now_utc() -> String {
    format_utc(chrono::Utc::now())
}

/// Normalise any RFC 3339 timestamp (`+00:00`, other offsets, nanoseconds)
/// to [`format_utc`]; values that do not parse are returned unchanged.
pub fn normalize_utc(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| format_utc(time.with_timezone(&chrono::Utc)))
        .unwrap_or_else(|_| value.to_string())
}

pub fn normalize_utc_opt(value: Option<&str>) -> Option<String> {
    value.map(normalize_utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("cutex-instance-{}", uuid::Uuid::new_v4()));
        let path = dir.join(FILE_NAME);
        let first = load_or_create(&path).unwrap();
        assert!(first.starts_with("ci-") && valid(&first), "{first}");
        assert_eq!(load_or_create(&path).unwrap(), first);
        std::fs::write(&path, "garbage id with spaces").unwrap();
        let replaced = load_or_create(&path).unwrap();
        assert!(valid(&replaced));
        assert_eq!(instance_id(), "ci-test-instance");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timestamps_normalise_to_utc_z_millis() {
        assert_eq!(normalize_utc("2026-10-08T01:02:03+00:00"), "2026-10-08T01:02:03.000Z");
        assert_eq!(normalize_utc("2026-10-08T11:02:03.123456789+10:00"), "2026-10-08T01:02:03.123Z");
        assert_eq!(normalize_utc("2026-10-08T01:02:03Z"), "2026-10-08T01:02:03.000Z");
        assert_eq!(normalize_utc("not a time"), "not a time");
        assert!(now_utc().ends_with('Z'));
    }
}
