//! Owner notify settings (`config.json` `owner_notify`; cutex-desktop
//! `docs/research/owner-notify.md` §3): how an agent's `owner_notify`
//! message reaches the owner (channels per level), who is offered the tool,
//! rate limits and per-agent overrides. The agent never sees any of this.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const CHANNELS: &[&str] = &["desktop", "phone", "bigAlert"];
pub const LEVELS: &[&str] = &["info", "attention", "urgent"];
pub const AUDIENCES: &[&str] = &["ownerFacing", "everyone", "directors", "none"];
pub const OVERRIDES: &[&str] = &["mute", "force"];
pub const DEFAULT_PER_HOUR: u32 = 12;
pub const DEFAULT_URGENT_PER_HOUR: u32 = 3;
pub const MAX_PER_HOUR: u32 = 1000;
pub const MAX_URGENT_PER_HOUR: u32 = 100;
pub const MAX_AGENT_OVERRIDES: usize = 500;

/// Channels per level (`info` / `attention` / `urgent`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnerNotifyDelivery {
    pub info: Vec<String>,
    pub attention: Vec<String>,
    pub urgent: Vec<String>,
}

impl Default for OwnerNotifyDelivery {
    fn default() -> Self {
        Self {
            info: vec!["desktop".into()],
            attention: vec!["desktop".into(), "phone".into()],
            urgent: vec!["desktop".into(), "phone".into(), "bigAlert".into()],
        }
    }
}

impl OwnerNotifyDelivery {
    pub fn for_level(&self, level: &str) -> &[String] {
        match level {
            "urgent" => &self.urgent,
            "attention" => &self.attention,
            _ => &self.info,
        }
    }

    pub fn set_level(&mut self, level: &str, channels: Vec<String>) {
        match level {
            "urgent" => self.urgent = channels,
            "attention" => self.attention = channels,
            _ => self.info = channels,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnerNotifyRateLimit {
    /// Messages per agent per rolling hour before they are downgraded to
    /// `info` without any channel.
    pub per_hour: u32,
    /// Effective-urgent messages per agent per rolling hour before urgent is
    /// downgraded to `attention`.
    pub urgent_per_hour: u32,
}

impl Default for OwnerNotifyRateLimit {
    fn default() -> Self {
        Self { per_hour: DEFAULT_PER_HOUR, urgent_per_hour: DEFAULT_URGENT_PER_HOUR }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnerNotifySettings {
    pub delivery: OwnerNotifyDelivery,
    pub big_alert_sound: bool,
    /// `ownerFacing` (Directors + agents not created by another agent),
    /// `everyone`, `directors`, `none`.
    pub tool_audience: String,
    pub rate_limit: OwnerNotifyRateLimit,
    /// cutexSessionId → `mute` (record only, no channel) | `force` (never
    /// rate-limited).
    pub agent_overrides: BTreeMap<String, String>,
}

impl Default for OwnerNotifySettings {
    fn default() -> Self {
        Self {
            delivery: OwnerNotifyDelivery::default(),
            big_alert_sound: true,
            tool_audience: "ownerFacing".into(),
            rate_limit: OwnerNotifyRateLimit::default(),
            agent_overrides: BTreeMap::new(),
        }
    }
}

/// Channel list check: known names, no duplicates.
pub fn validate_channels(channels: &[String]) -> Result<(), String> {
    for (index, channel) in channels.iter().enumerate() {
        if !CHANNELS.contains(&channel.as_str()) {
            return Err(format!("unknown channel {channel} (desktop, phone, bigAlert)"));
        }
        if channels[..index].contains(channel) {
            return Err(format!("duplicate channel {channel}"));
        }
    }
    Ok(())
}

impl OwnerNotifySettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> Result<(), String> {
        for level in LEVELS {
            validate_channels(self.delivery.for_level(level)).map_err(|error| format!("delivery.{level}: {error}"))?;
        }
        if !AUDIENCES.contains(&self.tool_audience.as_str()) {
            return Err("toolAudience: must be ownerFacing, everyone, directors or none".into());
        }
        if !(1..=MAX_PER_HOUR).contains(&self.rate_limit.per_hour) {
            return Err(format!("rateLimit.perHour: must be 1–{MAX_PER_HOUR}"));
        }
        if self.rate_limit.urgent_per_hour > MAX_URGENT_PER_HOUR {
            return Err(format!("rateLimit.urgentPerHour: must be 0–{MAX_URGENT_PER_HOUR}"));
        }
        if self.agent_overrides.len() > MAX_AGENT_OVERRIDES {
            return Err(format!("agentOverrides: at most {MAX_AGENT_OVERRIDES} entries"));
        }
        if let Some((agent, value)) = self.agent_overrides.iter().find(|(_, value)| !OVERRIDES.contains(&value.as_str())) {
            return Err(format!("agentOverrides.{agent}: {value} is not mute or force"));
        }
        if self.agent_overrides.keys().any(|agent| agent.is_empty() || agent.len() > 200) {
            return Err("agentOverrides: keys must be cutexSessionIds".into());
        }
        Ok(())
    }

    /// Effective settings: an invalid hand edit falls back to the defaults
    /// part by part.
    pub fn effective(&self) -> Self {
        let defaults = Self::default();
        let mut delivery = defaults.delivery.clone();
        for level in LEVELS {
            let channels = self.delivery.for_level(level);
            if validate_channels(channels).is_ok() {
                delivery.set_level(level, channels.to_vec());
            }
        }
        Self {
            delivery,
            big_alert_sound: self.big_alert_sound,
            tool_audience: if AUDIENCES.contains(&self.tool_audience.as_str()) {
                self.tool_audience.clone()
            } else {
                defaults.tool_audience
            },
            rate_limit: OwnerNotifyRateLimit {
                per_hour: if (1..=MAX_PER_HOUR).contains(&self.rate_limit.per_hour) {
                    self.rate_limit.per_hour
                } else {
                    DEFAULT_PER_HOUR
                },
                urgent_per_hour: self.rate_limit.urgent_per_hour.min(MAX_URGENT_PER_HOUR),
            },
            agent_overrides: self
                .agent_overrides
                .iter()
                .filter(|(_, value)| OVERRIDES.contains(&value.as_str()))
                .map(|(agent, value)| (agent.clone(), value.clone()))
                .collect(),
        }
    }
}

/// The current effective settings (defaults when the config is unreadable).
pub fn current() -> OwnerNotifySettings {
    crate::config::store::load_codez_config_checked()
        .map(|config| config.owner_notify.effective())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_bad_parts_fall_back() {
        assert!(OwnerNotifySettings::default().validate().is_ok());
        let mut bad = OwnerNotifySettings::default();
        bad.delivery.info = vec!["fax".into()];
        bad.tool_audience = "nobody".into();
        bad.rate_limit.per_hour = 0;
        bad.agent_overrides.insert("a".into(), "loud".into());
        assert!(bad.validate().is_err());
        let effective = bad.effective();
        assert_eq!(effective.delivery.info, vec!["desktop".to_string()]);
        assert_eq!(effective.tool_audience, "ownerFacing");
        assert_eq!(effective.rate_limit.per_hour, DEFAULT_PER_HOUR);
        assert!(effective.agent_overrides.is_empty());
        assert!(validate_channels(&["desktop".into(), "desktop".into()]).is_err());
    }
}
