//! Agent Bus audit log settings (`config.json` `agent_bus_audit`):
//! size-based rotation of `runtime/agent-bus-audit.jsonl`, how many rotated
//! files are kept, gzip of rotated files, and coalescing of identical
//! repeated polls. Read once when the bus starts (restart to apply).

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_FILE_MB: u32 = 64;
pub const MIN_MAX_FILE_MB: u32 = 1;
pub const MAX_MAX_FILE_MB: u32 = 4096;
pub const DEFAULT_KEEP_FILES: u32 = 4;
pub const MAX_KEEP_FILES: u32 = 100;
pub const DEFAULT_COALESCE_POLLS_SECS: u32 = 600;
pub const MAX_COALESCE_POLLS_SECS: u32 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentBusAuditSettings {
    /// Rotate the live file once it reaches this size (MiB).
    pub max_file_mb: u32,
    /// Rotated files kept beside the live one (0 = none).
    pub keep_files: u32,
    /// Compress rotated files (`.jsonl.gz`) in the background.
    pub gzip: bool,
    /// A poll returning exactly the same message ids for the same agent as
    /// its last recorded poll is not written again within this window; the
    /// next written poll carries `suppressed_repeats`. 0 records every poll.
    pub coalesce_polls_secs: u32,
}

impl Default for AgentBusAuditSettings {
    fn default() -> Self {
        Self {
            max_file_mb: DEFAULT_MAX_FILE_MB,
            keep_files: DEFAULT_KEEP_FILES,
            gzip: true,
            coalesce_polls_secs: DEFAULT_COALESCE_POLLS_SECS,
        }
    }
}

impl AgentBusAuditSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Out-of-range hand-edited values fall back to their default (each
    /// field on its own).
    pub fn effective(&self) -> Self {
        let defaults = Self::default();
        Self {
            max_file_mb: if (MIN_MAX_FILE_MB..=MAX_MAX_FILE_MB).contains(&self.max_file_mb) {
                self.max_file_mb
            } else {
                defaults.max_file_mb
            },
            keep_files: if self.keep_files <= MAX_KEEP_FILES {
                self.keep_files
            } else {
                defaults.keep_files
            },
            gzip: self.gzip,
            coalesce_polls_secs: if self.coalesce_polls_secs <= MAX_COALESCE_POLLS_SECS {
                self.coalesce_polls_secs
            } else {
                defaults.coalesce_polls_secs
            },
        }
    }

    pub fn max_file_bytes(&self) -> u64 {
        u64::from(self.effective().max_file_mb) * 1024 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_fallbacks() {
        let settings = AgentBusAuditSettings::default();
        assert_eq!(settings.max_file_mb, 64);
        assert_eq!(settings.keep_files, 4);
        assert!(settings.gzip);
        assert_eq!(settings.coalesce_polls_secs, 600);
        assert_eq!(settings.max_file_bytes(), 64 * 1024 * 1024);
        let parsed: AgentBusAuditSettings =
            serde_json::from_value(serde_json::json!({"maxFileMb": 8, "gzip": false})).unwrap();
        assert_eq!(parsed.max_file_mb, 8);
        assert_eq!(parsed.keep_files, 4);
        assert!(!parsed.gzip);
        let empty: AgentBusAuditSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.is_default());
        let broken = AgentBusAuditSettings {
            max_file_mb: 0,
            keep_files: 1000,
            gzip: true,
            coalesce_polls_secs: 1_000_000,
        };
        assert_eq!(broken.effective(), AgentBusAuditSettings::default());
        let zero = AgentBusAuditSettings {
            keep_files: 0,
            coalesce_polls_secs: 0,
            ..AgentBusAuditSettings::default()
        };
        assert_eq!(zero.effective(), zero);
    }
}
