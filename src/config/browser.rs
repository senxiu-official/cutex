//! Browser relay settings. Transport budgets are ceilings, never model budgets.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOpenApproval {
    #[default]
    Off,
    Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
// config.json `browser`: unknown fields ignored (persistence audit, backend
// auto-update RFC §6.2); API input is validated by `patched`.
#[serde(default)]
pub struct BrowserSettings {
    pub agent_open_approval: AgentOpenApproval,
    pub heartbeat_ms: u64,
    pub offline_ms: u64,
    pub lease_idle_secs: u64,
    pub image_retain_secs: u64,
    pub image_retain_bytes: u64,
}
impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            agent_open_approval: AgentOpenApproval::Off,
            heartbeat_ms: 10_000,
            offline_ms: 30_000,
            lease_idle_secs: 1800,
            image_retain_secs: 120,
            image_retain_bytes: 64 * 1024 * 1024,
        }
    }
}
impl BrowserSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
    pub fn valid(&self) -> bool {
        (1000..=60_000).contains(&self.heartbeat_ms)
            && (3000..=300_000).contains(&self.offline_ms)
            && self.offline_ms >= self.heartbeat_ms.saturating_mul(3)
            && (60..=86_400).contains(&self.lease_idle_secs)
            && (1..=120).contains(&self.image_retain_secs)
            && (1024 * 1024..=64 * 1024 * 1024).contains(&self.image_retain_bytes)
    }
    pub fn effective(self) -> Self {
        if self.valid() {
            self
        } else {
            Self {
                agent_open_approval: self.agent_open_approval,
                ..Self::default()
            }
        }
    }
    pub fn view(&self) -> Value {
        json!({
            "agentOpenApproval": self.agent_open_approval,
            "heartbeatMs": self.heartbeat_ms, "offlineMs": self.offline_ms,
            "leaseIdleSecs": self.lease_idle_secs, "imageRetainSecs": self.image_retain_secs,
            "imageRetainBytes": self.image_retain_bytes,
        })
    }
    pub fn patched(&self, patch: &Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("browser: must be an object")?;
        let mut next = *self;
        for (key, value) in object {
            if key == "agentOpenApproval" {
                next.agent_open_approval = match value.as_str() {
                    Some("off") => AgentOpenApproval::Off,
                    Some("ask") => AgentOpenApproval::Ask,
                    _ => return Err("browser.agentOpenApproval: must be off or ask".into()),
                };
                continue;
            }
            let slot = match key.as_str() {
                "heartbeatMs" => &mut next.heartbeat_ms,
                "offlineMs" => &mut next.offline_ms,
                "leaseIdleSecs" => &mut next.lease_idle_secs,
                "imageRetainSecs" => &mut next.image_retain_secs,
                "imageRetainBytes" => &mut next.image_retain_bytes,
                _ => return Err(format!("browser: unsupported field {key}")),
            };
            *slot = value
                .as_u64()
                .ok_or_else(|| format!("browser.{key}: must be an integer"))?;
        }
        if !next.valid() {
            return Err(
                "browser: limits out of range (offlineMs must be at least three heartbeats)".into(),
            );
        }
        Ok(next)
    }
}
pub fn current() -> BrowserSettings {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<SettingsCache>> = std::sync::OnceLock::new();
    let Ok(path) = crate::config::paths::config_path() else {
        return unavailable();
    };
    let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() else {
        return unavailable();
    };
    cache.read(&path, || {
        crate::config::store::load_codez_config_checked()
            .map(|c| c.browser.effective())
            .ok()
    })
}
fn unavailable() -> BrowserSettings {
    BrowserSettings {
        agent_open_approval: AgentOpenApproval::Ask,
        ..BrowserSettings::default()
    }
}
#[derive(Default)]
pub(crate) struct SettingsCache {
    entry: Option<(
        std::path::PathBuf,
        std::time::SystemTime,
        u64,
        BrowserSettings,
    )>,
}
impl SettingsCache {
    pub(crate) fn read(
        &mut self,
        path: &std::path::Path,
        load: impl FnOnce() -> Option<BrowserSettings>,
    ) -> BrowserSettings {
        let stamp = || {
            std::fs::metadata(path)
                .ok()
                .filter(|m| m.is_file())
                .and_then(|m| Some((m.modified().ok()?, m.len())))
        };
        let Some((mtime, len)) = stamp() else {
            self.entry = None;
            return unavailable();
        };
        if let Some((old_path, old_time, old_len, value)) = &self.entry {
            if old_path == path && *old_time == mtime && *old_len == len {
                return *value;
            }
        }
        let Some(value) = load() else {
            self.entry = None;
            return unavailable();
        };
        // A replacement while parsing is not cached under an old stamp.
        if stamp() != Some((mtime, len)) {
            self.entry = None;
            return unavailable();
        }
        self.entry = Some((path.to_path_buf(), mtime, len, value));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_fresh_missing_config_fails_closed_even_though_default_dto_is_off() {
        let path = std::env::temp_dir().join(format!(
            "browser-config-does-not-exist-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cache = SettingsCache::default();
        assert_eq!(
            BrowserSettings::default().agent_open_approval,
            AgentOpenApproval::Off
        );
        for _ in 0..2 {
            assert_eq!(
                cache
                    .read(&path, || panic!(
                        "metadata absence must not use permissive loader fallback"
                    ))
                    .agent_open_approval,
                AgentOpenApproval::Ask
            );
        }
    }
    #[test]
    fn browser_settings_cache_tracks_mtime_size_path_delete_and_failed_parse() {
        let root =
            std::env::temp_dir().join(format!("browser-settings-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("config.json");
        std::fs::write(&path, b"first").unwrap();
        let mut cache = SettingsCache::default();
        let loads = std::cell::Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Some(BrowserSettings::default())
        };
        for _ in 0..20 {
            assert_eq!(cache.read(&path, load), BrowserSettings::default());
        }
        assert_eq!(loads.get(), 1);
        std::fs::write(&path, b"changed-size").unwrap();
        cache.read(&path, load);
        assert_eq!(loads.get(), 2);
        let copy = root.join("other.json");
        std::fs::copy(&path, &copy).unwrap();
        cache.read(&copy, load);
        assert_eq!(loads.get(), 3);
        std::fs::remove_file(&copy).unwrap();
        assert_eq!(
            cache.read(&copy, load).agent_open_approval,
            AgentOpenApproval::Ask
        );
        std::fs::write(&copy, b"bad").unwrap();
        assert_eq!(
            cache.read(&copy, || None).agent_open_approval,
            AgentOpenApproval::Ask
        );
        assert_eq!(cache.read(&copy, load), BrowserSettings::default());
        assert_eq!(loads.get(), 4);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn browser_defaults_and_strict_patch() {
        let d = BrowserSettings::default();
        assert!(d.valid() && d.is_default());
        assert_eq!(d.view()["agentOpenApproval"], "off");
        let ask = d.patched(&json!({"agentOpenApproval":"ask"})).unwrap();
        assert_eq!(ask.agent_open_approval, AgentOpenApproval::Ask);
        assert_eq!(
            serde_json::to_value(ask).unwrap()["agent_open_approval"],
            "ask"
        );
        for bad in [
            json!({"x":1}),
            json!({"imageRetainSecs":121}),
            json!({"heartbeatMs":20000}),
            json!({"leaseIdleSecs":1.5}),
            json!({"agentOpenApproval":null}),
            json!([]),
            json!({"imageRetainBytes":67108865}),
            json!({"imageRetainSecs":0}),
        ] {
            assert!(d.patched(&bad).is_err(), "{bad}");
        }
        assert!(d
            .patched(&json!({"heartbeatMs":20000,"offlineMs":60000}))
            .is_ok());
        assert_eq!(
            BrowserSettings {
                image_retain_secs: u64::MAX,
                ..d
            }
            .effective(),
            d
        );
    }
}
