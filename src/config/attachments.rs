//! Session attachment settings (`config.json` `attachments`), used by
//! `POST /v2/sessions/{id}/attachments` (remote clients upload images that
//! become host-local `localImage` paths).

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const MIN_MAX_BYTES: u64 = 64 * 1024;
/// The Management HTTP layer refuses bodies above 16 MiB anyway.
pub const MAX_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
pub const MAX_RETENTION_DAYS: u32 = 3650;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentSettings {
    /// Largest accepted upload in bytes.
    pub max_bytes: u64,
    /// Uploaded files older than this are removed (0 keeps them until the
    /// session is deleted).
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
