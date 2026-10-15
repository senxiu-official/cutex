//! Editable profile fields for the declarative quota reader.
use super::quota::{Auth, Config, Mode, ResetFormat};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Enabled,
    Url,
    Auth,
    Label,
    Interval,
    Mode,
    Value,
    Limit,
    Reset,
    ResetFormat,
}
impl Field {
    pub const ALL: [Self; 10] = [
        Self::Enabled,
        Self::Url,
        Self::Auth,
        Self::Label,
        Self::Interval,
        Self::Mode,
        Self::Value,
        Self::Limit,
        Self::Reset,
        Self::ResetFormat,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Enabled => "Enabled",
            Self::Url => "URL",
            Self::Auth => "Authentication",
            Self::Label => "Short label",
            Self::Interval => "Refresh seconds",
            Self::Mode => "Calculation",
            Self::Value => "Value JSON pointer",
            Self::Limit => "Limit JSON pointer",
            Self::Reset => "Reset JSON pointer",
            Self::ResetFormat => "Reset format",
        }
    }
    pub fn choices(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Self::Enabled => vec![("Off", "false"), ("On", "true")],
            Self::Auth => vec![
                ("Profile API key (Bearer)", "profile_api_key"),
                ("Profile API key (no Bearer)", "profile_api_key_raw"),
                ("None", "none"),
            ],
            Self::Mode => vec![
                ("Remaining / limit", "remaining_and_limit"),
                ("1 - used / limit", "used_and_limit"),
                ("Remaining percent", "remaining_percent"),
                ("100 - used percent", "used_percent"),
            ],
            Self::ResetFormat => vec![
                ("RFC3339 / ISO timestamp", "rfc3339"),
                ("Unix seconds", "unix_seconds"),
                ("Unix milliseconds", "unix_millis"),
            ],
            _ => vec![],
        }
    }
    pub fn value(self, c: &Config) -> String {
        match self {
            Self::Enabled => c.enabled.to_string(),
            Self::Url => c.url.clone(),
            Self::Label => c.label.clone(),
            Self::Interval => c.refresh_seconds.to_string(),
            Self::Value => c.value_pointer.clone(),
            Self::Limit => c.limit_pointer.clone(),
            Self::Reset => c.reset_pointer.clone(),
            Self::Auth => match c.auth {
                Auth::ProfileApiKey => "profile_api_key",
                Auth::ProfileApiKeyRaw => "profile_api_key_raw",
                Auth::None => "none",
            }
            .into(),
            Self::Mode => match c.mode {
                Mode::RemainingAndLimit => "remaining_and_limit",
                Mode::UsedAndLimit => "used_and_limit",
                Mode::RemainingPercent => "remaining_percent",
                Mode::UsedPercent => "used_percent",
                Mode::Balance => "balance",
            }
            .into(),
            Self::ResetFormat => match c.reset_format {
                ResetFormat::Rfc3339 => "rfc3339",
                ResetFormat::UnixSeconds => "unix_seconds",
                ResetFormat::UnixMillis => "unix_millis",
            }
            .into(),
        }
    }
    pub fn apply(self, c: &mut Config, value: &str) -> anyhow::Result<()> {
        let v = value.trim();
        match self {
            Self::Enabled => c.enabled = v.parse()?,
            Self::Url => c.url = v.into(),
            Self::Label => c.label = v.into(),
            Self::Interval => c.refresh_seconds = v.parse()?,
            Self::Value => c.value_pointer = v.into(),
            Self::Limit => c.limit_pointer = v.into(),
            Self::Reset => c.reset_pointer = v.into(),
            Self::Auth => c.auth = serde_json::from_value(serde_json::json!(v))?,
            Self::Mode => {
                // Balance is resolved internally by the Auto endpoint preset.
                // It must not be persisted as a custom mapping by the TUI.
                anyhow::ensure!(v != "balance", "Balance mode is Auto-preset only");
                c.mode = serde_json::from_value(serde_json::json!(v))?;
            }
            Self::ResetFormat => c.reset_format = serde_json::from_value(serde_json::json!(v))?,
        }
        // A draft may be enabled before its URL is entered. Validate the complete configuration on Save.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_mode_is_not_editable_as_a_custom_quota_mapping() {
        assert!(Field::Mode
            .choices()
            .iter()
            .all(|(_, value)| *value != "balance"));

        let mut config = Config::default();
        assert!(Field::Mode.apply(&mut config, "balance").is_err());
        assert_eq!(config.mode, Mode::RemainingAndLimit);
    }
}
