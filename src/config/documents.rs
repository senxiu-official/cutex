//! Input overflow → documents settings (`config.json` `documents`,
//! `/v2/settings` `documents`): render margin, preview size, the document
//! quota and the retention of closed records (cutex-desktop
//! `docs/research/input-overflow-documents.md` §5).

use serde::{Deserialize, Serialize};

/// `(field, min, max)` of every numeric setting (inclusive).
pub const RANGES: &[(&str, u64, u64)] = &[
    ("marginPercent", 0, 50),
    ("previewBytes", 200, 4000),
    ("quotaBytes", 100 * 1024 * 1024, 100 * 1024 * 1024 * 1024),
    ("closedRetentionDays", 1, 3650),
    ("archiveRetentionDays", 0, 3650),
    ("idleReminderDays", 0, 365),
    ("uploadGraceHours", 1, 720),
    ("sweepIntervalHours", 1, 168),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DocumentSettings {
    /// Safety margin on the receiver's byte limit (percent).
    pub margin_percent: u8,
    /// Inline preview of a part moved to a document (bytes).
    pub preview_bytes: u32,
    /// Live document store quota (blob bytes; the archive is not counted).
    pub quota_bytes: u64,
    /// Closed delegations, finished agent messages and their documents stay
    /// this long before they are archived.
    pub closed_retention_days: u32,
    /// Archived months are deleted after this many days (0 keeps them, the
    /// default since r252: owner decision 2026-10-23; was 365).
    pub archive_retention_days: u32,
    /// One owner reminder about an open delegation idle this long (0: off).
    pub idle_reminder_days: u32,
    /// Documents nothing refers to are kept this long.
    pub upload_grace_hours: u32,
    /// Sweep cadence (also once at start).
    pub sweep_interval_hours: u32,
}

impl Default for DocumentSettings {
    fn default() -> Self {
        Self {
            margin_percent: crate::app_server::external_input::size::DEFAULT_MARGIN_PERCENT,
            preview_bytes: 800,
            quota_bytes: 1024 * 1024 * 1024,
            closed_retention_days: 30,
            archive_retention_days: 0,
            idle_reminder_days: 14,
            upload_grace_hours: 24,
            sweep_interval_hours: 6,
        }
    }
}

impl DocumentSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn values(&self) -> [(&'static str, u64); 8] {
        [
            ("marginPercent", u64::from(self.margin_percent)),
            ("previewBytes", u64::from(self.preview_bytes)),
            ("quotaBytes", self.quota_bytes),
            ("closedRetentionDays", u64::from(self.closed_retention_days)),
            ("archiveRetentionDays", u64::from(self.archive_retention_days)),
            ("idleReminderDays", u64::from(self.idle_reminder_days)),
            ("uploadGraceHours", u64::from(self.upload_grace_hours)),
            ("sweepIntervalHours", u64::from(self.sweep_interval_hours)),
        ]
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for ((name, value), (_, min, max)) in self.values().into_iter().zip(RANGES) {
            anyhow::ensure!((*min..=*max).contains(&value), "documents.{name} must be {min}–{max}");
        }
        Ok(())
    }

    /// Invalid hand-edited values fall back to the defaults field by field.
    pub fn effective(&self) -> Self {
        let defaults = Self::default();
        let ok = |name: &str, value: u64| {
            RANGES.iter().find(|(field, _, _)| *field == name).is_some_and(|(_, min, max)| (*min..=*max).contains(&value))
        };
        Self {
            margin_percent: if ok("marginPercent", self.margin_percent.into()) { self.margin_percent } else { defaults.margin_percent },
            preview_bytes: if ok("previewBytes", self.preview_bytes.into()) { self.preview_bytes } else { defaults.preview_bytes },
            quota_bytes: if ok("quotaBytes", self.quota_bytes) { self.quota_bytes } else { defaults.quota_bytes },
            closed_retention_days: if ok("closedRetentionDays", self.closed_retention_days.into()) {
                self.closed_retention_days
            } else {
                defaults.closed_retention_days
            },
            archive_retention_days: if ok("archiveRetentionDays", self.archive_retention_days.into()) {
                self.archive_retention_days
            } else {
                defaults.archive_retention_days
            },
            idle_reminder_days: if ok("idleReminderDays", self.idle_reminder_days.into()) {
                self.idle_reminder_days
            } else {
                defaults.idle_reminder_days
            },
            upload_grace_hours: if ok("uploadGraceHours", self.upload_grace_hours.into()) {
                self.upload_grace_hours
            } else {
                defaults.upload_grace_hours
            },
            sweep_interval_hours: if ok("sweepIntervalHours", self.sweep_interval_hours.into()) {
                self.sweep_interval_hours
            } else {
                defaults.sweep_interval_hours
            },
        }
    }

    /// Apply a partial `/v2/settings` `documents` object.
    pub fn patched(&self, patch: &serde_json::Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("documents: must be an object")?;
        let mut value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        for (key, field) in object {
            let Some((_, min, max)) = RANGES.iter().find(|(name, _, _)| name == key) else {
                return Err(format!("documents: unsupported field {key}"));
            };
            let number = field
                .as_u64()
                .filter(|number| (*min..=*max).contains(number))
                .ok_or(format!("documents.{key}: must be {min}-{max}"))?;
            value[key] = number.into();
        }
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    /// `/v2/settings` view: `{documents, documentsDefault, documentsRange}`.
    pub fn view(&self) -> serde_json::Value {
        let ranges: serde_json::Map<String, serde_json::Value> =
            RANGES.iter().map(|(name, min, max)| (name.to_string(), serde_json::json!([min, max]))).collect();
        serde_json::json!({"documents": self, "documentsDefault": Self::default(), "documentsRange": ranges})
    }
}

/// The current effective settings from `config.json`.
pub fn current() -> DocumentSettings {
    crate::config::store::load_codez_config().documents.effective()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_partial_patches_are_checked() {
        let settings = DocumentSettings::default();
        settings.validate().unwrap();
        let patched = settings.patched(&serde_json::json!({"quotaBytes": 209715200u64, "marginPercent": 5})).unwrap();
        assert_eq!((patched.quota_bytes, patched.margin_percent, patched.preview_bytes), (209_715_200, 5, 800));
        assert!(settings.patched(&serde_json::json!({"marginPercent": 99})).is_err());
        assert!(settings.patched(&serde_json::json!({"bogus": 1})).is_err());
        let broken = DocumentSettings { preview_bytes: 1, ..settings };
        assert!(broken.validate().is_err());
        assert_eq!(broken.effective().preview_bytes, 800);
        let view = settings.view();
        assert_eq!(view["documents"]["closedRetentionDays"], 30);
        assert_eq!(view["documents"]["archiveRetentionDays"], 0, "the archive is kept by default since r252");
        // An explicit value (an owner setting, or a block an older release wrote) still applies.
        assert_eq!(settings.patched(&serde_json::json!({"archiveRetentionDays": 365})).unwrap().archive_retention_days, 365);
        assert_eq!(view["documentsRange"]["archiveRetentionDays"], serde_json::json!([0, 3650]));
    }
}
