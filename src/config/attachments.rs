//! Session attachment settings (`config.json` `attachments`), used by
//! `POST /v2/sessions/{id}/attachments` (remote clients upload images that
//! become host-local `localImage` paths).

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const MIN_MAX_BYTES: u64 = 64 * 1024;
/// The Management HTTP layer refuses bodies above 16 MiB anyway.
pub const MAX_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// 0 = keep (owner decision 2026-10-23; was 30 before r252).
pub const DEFAULT_RETENTION_DAYS: u32 = 0;
pub const MAX_RETENTION_DAYS: u32 = 3650;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentSettings {
    /// Largest accepted upload in bytes.
    pub max_bytes: u64,
    /// Releases before r252 removed uploaded files older than this (0 keeps
    /// them until the session is deleted). Since r252 it is stored and shown
    /// but not applied: an attachment stays while its session (and the
    /// transcript referring to it) exists.
    pub retention_days: u32,
}

impl Default for AttachmentSettings {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl AttachmentSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(&self.max_bytes),
            "attachments.maxBytes must be {MIN_MAX_BYTES}–{MAX_MAX_BYTES}"
        );
        anyhow::ensure!(
            self.retention_days <= MAX_RETENTION_DAYS,
            "attachments.retentionDays must be 0–{MAX_RETENTION_DAYS} (0 keeps files)"
        );
        Ok(())
    }

    /// The effective settings: invalid hand-edited values fall back to the
    /// defaults field by field.
    pub fn effective(&self) -> Self {
        let defaults = Self::default();
        Self {
            max_bytes: if (MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(&self.max_bytes) {
                self.max_bytes
            } else {
                defaults.max_bytes
            },
            retention_days: if self.retention_days <= MAX_RETENTION_DAYS {
                self.retention_days
            } else {
                defaults.retention_days
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_are_kept_by_default() {
        assert_eq!(AttachmentSettings::default().retention_days, 0);
        assert_eq!(AttachmentSettings::default().effective().retention_days, 0);
        // A value written by an older release still parses and validates.
        let older: AttachmentSettings = serde_json::from_str(r#"{"max_bytes":8388608,"retention_days":30}"#).unwrap();
        assert!(older.validate().is_ok());
        assert!(!older.is_default());
    }
}
