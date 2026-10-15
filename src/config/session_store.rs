//! Session store maintenance settings (`config.json` `session_store`,
//! `/v2/settings` `sessionStore`): how long completed explicit-launch
//! receipts are kept (cutex-desktop `docs/research/session-store-sqlite.md`
//! §5 Phase 0).

use serde::{Deserialize, Serialize};

pub const DEFAULT_RECEIPT_RETENTION_DAYS: u32 = 7;
/// 0 keeps only the receipts something still needs (see
/// `crate::session::receipt_retention`).
pub const MIN_RECEIPT_RETENTION_DAYS: u32 = 0;
pub const MAX_RECEIPT_RETENTION_DAYS: u32 = 365;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionStoreSettings {
    /// Completed explicit-launch receipts younger than this are kept even
    /// when nothing needs them any more.
    pub receipt_retention_days: u32,
}

impl Default for SessionStoreSettings {
    fn default() -> Self {
        Self {
            receipt_retention_days: DEFAULT_RECEIPT_RETENTION_DAYS,
        }
    }
}

fn days_in_range(days: u64) -> bool {
    (u64::from(MIN_RECEIPT_RETENTION_DAYS)..=u64::from(MAX_RECEIPT_RETENTION_DAYS)).contains(&days)
}

impl SessionStoreSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Invalid hand-edited values fall back to the default.
    pub fn effective(&self) -> Self {
        if days_in_range(u64::from(self.receipt_retention_days)) {
            *self
        } else {
            Self::default()
        }
    }

    /// Apply a partial `/v2/settings` `sessionStore` object.
    pub fn patched(&self, patch: &serde_json::Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("sessionStore: must be an object")?;
        let mut next = *self;
        for (key, value) in object {
            match key.as_str() {
                "receiptRetentionDays" => {
                    next.receipt_retention_days = value
                        .as_u64()
                        .filter(|days| days_in_range(*days))
                        .map(|days| days as u32)
                        .ok_or(format!(
                            "sessionStore.receiptRetentionDays: must be {MIN_RECEIPT_RETENTION_DAYS}-{MAX_RECEIPT_RETENTION_DAYS} (0 keeps only receipts still in use)"
                        ))?;
                }
                other => return Err(format!("sessionStore: unsupported field {other}")),
            }
        }
        Ok(next)
    }

    /// `/v2/settings` fields: `sessionStore`, `sessionStoreDefault`,
    /// `sessionStoreRange`.
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionStore": self,
            "sessionStoreDefault": Self::default(),
            "sessionStoreRange": {
                "receiptRetentionDays": [MIN_RECEIPT_RETENTION_DAYS, MAX_RECEIPT_RETENTION_DAYS],
            },
        })
    }
}

/// The current effective settings from `config.json`.
pub fn current() -> SessionStoreSettings {
    crate::config::store::load_codez_config()
        .session_store
        .effective()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_range_patch_and_hand_edited_fallback() {
        let settings = SessionStoreSettings::default();
        assert_eq!(settings.receipt_retention_days, 7);
        assert!(settings.is_default());
        let patched = settings
            .patched(&serde_json::json!({"receiptRetentionDays": 0}))
            .unwrap();
        assert_eq!(patched.receipt_retention_days, 0);
        assert_eq!(
            settings
                .patched(&serde_json::json!({"receiptRetentionDays": 365}))
                .unwrap()
                .receipt_retention_days,
            365
        );
        for bad in [
            serde_json::json!({"receiptRetentionDays": 366}),
            serde_json::json!({"receiptRetentionDays": -1}),
            serde_json::json!({"receiptRetentionDays": 1.5}),
            serde_json::json!({"receiptRetentionDays": "7"}),
            serde_json::json!({"bogus": 1}),
            serde_json::json!(7),
        ] {
            assert!(settings.patched(&bad).is_err(), "{bad}");
        }
        assert_eq!(settings.patched(&serde_json::json!({})).unwrap(), settings);
        let broken = SessionStoreSettings {
            receipt_retention_days: 9999,
        };
        assert_eq!(broken.effective().receipt_retention_days, 7);
        let view = settings.view();
        assert_eq!(view["sessionStore"]["receiptRetentionDays"], 7);
        assert_eq!(
            view["sessionStoreRange"]["receiptRetentionDays"],
            serde_json::json!([0, 365])
        );
        // config.json form: absent block = default; camelCase inside.
        let parsed: SessionStoreSettings =
            serde_json::from_value(serde_json::json!({"receiptRetentionDays": 3})).unwrap();
        assert_eq!(parsed.receipt_retention_days, 3);
        let empty: SessionStoreSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.is_default());
    }
}
