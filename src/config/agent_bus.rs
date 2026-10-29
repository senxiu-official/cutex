//! Agent Bus delivery settings (`config.json` `agent_bus`, `/v2/settings`
//! `agentBus`; capability `wake-busy-wait`).
//!
//! `wake_busy_wait_secs` (r217h1 / r217h2, typed in r220): how long a wake or
//! a delegation result for a busy Codex recipient waits for its running turn
//! to end before it is read at the recipient's next step instead (native
//! `soon`). Absent or not a number = 120 s; `0` = next step at once; negative
//! or `null` = never early (wait for the turn to end, the r217 behaviour; the
//! owner decides after a few days of `cutex-wake-read` logs whether to keep
//! it). Read per delivery: a change applies without a restart.
//!
//! The block stays lenient like the r217h1 raw object: a hand-edited value of
//! the wrong type never breaks the rest of `config.json` (it reads as the
//! default). A negative value is saved back as `null` (same meaning).

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};

/// Default of `agent_bus.wake_busy_wait_secs`.
pub const DEFAULT_WAKE_BUSY_WAIT_SECS: u64 = 120;
/// Largest value `/v2/settings` accepts (a hand-edited larger one still
/// works, capped at 1e9 s).
pub const MAX_WAKE_BUSY_WAIT_SECS: u64 = 86_400;

/// The stored `wake_busy_wait_secs`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum WakeBusyWait {
    /// Absent (or not a number): [`DEFAULT_WAKE_BUSY_WAIT_SECS`].
    #[default]
    Default,
    /// `null` or negative: never early (wait for the turn to end).
    AfterTurn,
    /// Seconds, `>= 0` (`0` = next step at once).
    Secs(f64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentBusSettings {
    pub wake_busy_wait: WakeBusyWait,
    pub result_coalescing: super::result_coalescing::ResultCoalescing,
}

const WAKE_BUSY_WAIT_KEY: &str = "wake_busy_wait_secs";

/// A whole number of seconds as a JSON integer, else as a float.
fn secs_json(secs: f64) -> Value {
    if secs.fract() == 0.0 && secs >= 0.0 && secs <= u64::MAX as f64 {
        json!(secs as u64)
    } else {
        json!(secs)
    }
}

impl WakeBusyWait {
    /// The r217h1 reading of a raw `wake_busy_wait_secs` value.
    fn from_raw(value: Option<&Value>) -> Self {
        match value {
            None => Self::Default,
            Some(Value::Null) => Self::AfterTurn,
            Some(Value::Number(number)) => match number.as_f64() {
                Some(secs) if secs < 0.0 => Self::AfterTurn,
                Some(secs) if secs.is_finite() => Self::Secs(secs),
                _ => Self::Default,
            },
            Some(_) => Self::Default,
        }
    }

    /// The effective wait; `None` = never early.
    pub fn duration(&self) -> Option<Duration> {
        match self {
            Self::Default => Some(Duration::from_secs(DEFAULT_WAKE_BUSY_WAIT_SECS)),
            Self::AfterTurn => None,
            Self::Secs(secs) => Some(Duration::from_secs_f64(secs.clamp(0.0, 1e9))),
        }
    }

    /// `/v2/settings` value: seconds, or `null` = never early.
    pub fn view(&self) -> Value {
        match self {
            Self::Default => json!(DEFAULT_WAKE_BUSY_WAIT_SECS),
            Self::AfterTurn => Value::Null,
            Self::Secs(secs) => secs_json(*secs),
        }
    }
}

impl AgentBusSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// How long a wake / delegation result for a busy recipient waits for
    /// its turn to end; `None` = never early.
    pub fn wake_busy_wait(&self) -> Option<Duration> {
        self.wake_busy_wait.duration()
    }

    /// Apply a partial `/v2/settings` `agentBus` object:
    /// `{"wakeBusyWaitSecs": <0..=86400> | <negative> | null}` (negative and
    /// `null` = never early, stored as `null`).
    pub fn patched(&self, patch: &Value) -> Result<Self, String> {
        let object = patch.as_object().ok_or("agentBus: must be an object")?;
        let mut next = *self;
        for (key, value) in object {
            match key.as_str() {
                "resultCoalescing" => next.result_coalescing = next.result_coalescing.patched(value)?,
                "wakeBusyWaitSecs" => {
                    next.wake_busy_wait = match value {
                        Value::Null => WakeBusyWait::AfterTurn,
                        Value::Number(number) => match number.as_f64() {
                            Some(secs) if secs < 0.0 => WakeBusyWait::AfterTurn,
                            Some(secs) if secs <= MAX_WAKE_BUSY_WAIT_SECS as f64 => WakeBusyWait::Secs(secs),
                            _ => {
                                return Err(format!(
                                    "agentBus.wakeBusyWaitSecs: must be 0-{MAX_WAKE_BUSY_WAIT_SECS} seconds, or negative / null (wait for the turn to end)"
                                ))
                            }
                        },
                        _ => {
                            return Err(
                                "agentBus.wakeBusyWaitSecs: must be a number of seconds or null".to_string()
                            )
                        }
                    };
                }
                other => return Err(format!("agentBus: unsupported field {other}")),
            }
        }
        Ok(next)
    }

    /// `/v2/settings` fields: `agentBus`, `agentBusDefault`, `agentBusRange`.
    pub fn view(&self) -> Value {
        json!({
            "agentBus": { "wakeBusyWaitSecs": self.wake_busy_wait.view(), "resultCoalescing": self.result_coalescing },
            "agentBusDefault": { "wakeBusyWaitSecs": DEFAULT_WAKE_BUSY_WAIT_SECS, "resultCoalescing": super::result_coalescing::ResultCoalescing::default() },
            "agentBusRange": { "wakeBusyWaitSecs": [0, MAX_WAKE_BUSY_WAIT_SECS], "resultCoalescing": {"quietSecs":[0,3600],"maxWaitSecs":[0,3600],"coldQuietSecs":[0,3600],"coldMaxWaitSecs":[0,3600]} },
        })
    }
}

impl Serialize for AgentBusSettings {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serde_json::Map::new();
        if self.result_coalescing != super::result_coalescing::ResultCoalescing::default() {
            object.insert("result_coalescing".into(), serde_json::to_value(self.result_coalescing).map_err(serde::ser::Error::custom)?);
        }
        match self.wake_busy_wait {
            WakeBusyWait::Default => {}
            WakeBusyWait::AfterTurn => {
                object.insert(WAKE_BUSY_WAIT_KEY.into(), Value::Null);
            }
            WakeBusyWait::Secs(secs) => {
                object.insert(WAKE_BUSY_WAIT_KEY.into(), secs_json(secs));
            }
        }
        object.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentBusSettings {
    /// Never fails: anything but an object reads as the default.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(Self {
            wake_busy_wait: WakeBusyWait::from_raw(value.get(WAKE_BUSY_WAIT_KEY)),
            result_coalescing: value.get("result_coalescing").map(super::result_coalescing::ResultCoalescing::from_raw).unwrap_or_default(),
        })
    }
}

/// The current settings from `config.json` (default when unreadable).
pub fn current() -> AgentBusSettings {
    crate::config::store::load_codez_config().agent_bus
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(json: &str) -> AgentBusSettings {
        let config: crate::profiles::model::CodezConfig = serde_json::from_str(json).unwrap();
        config.agent_bus
    }

    #[test]
    fn config_reading_matches_r217h1() {
        let default = Some(Duration::from_secs(DEFAULT_WAKE_BUSY_WAIT_SECS));
        for (json, expected) in [
            ("{}", default),
            (r#"{"agent_bus":{}}"#, default),
            (r#"{"agent_bus":"x"}"#, default),
            (r#"{"agent_bus":null}"#, default),
            (r#"{"agent_bus":{"wake_busy_wait_secs":"x"}}"#, default),
            (r#"{"agent_bus":{"wake_busy_wait_secs":0}}"#, Some(Duration::ZERO)),
            (r#"{"agent_bus":{"wake_busy_wait_secs":30}}"#, Some(Duration::from_secs(30))),
            (r#"{"agent_bus":{"wake_busy_wait_secs":2.5}}"#, Some(Duration::from_millis(2500))),
            (r#"{"agent_bus":{"wake_busy_wait_secs":-1}}"#, None),
            (r#"{"agent_bus":{"wake_busy_wait_secs":null}}"#, None),
        ] {
            assert_eq!(parsed(json).wake_busy_wait(), expected, "{json}");
        }
        // A wrong-typed value never breaks the rest of the config.
        let config: crate::profiles::model::CodezConfig =
            serde_json::from_str(r#"{"agent_bus":{"wake_busy_wait_secs":[1]},"default_profile":"p"}"#).unwrap();
        assert_eq!(config.default_profile.as_deref(), Some("p"));
        assert!(config.agent_bus.is_default());
    }

    #[test]
    fn config_round_trip() {
        let saved = |settings: AgentBusSettings| {
            let mut config = crate::profiles::model::CodezConfig::default();
            config.agent_bus = settings;
            serde_json::to_value(&config).unwrap()
        };
        // Default: no block at all.
        assert!(saved(AgentBusSettings::default()).get("agent_bus").is_none());
        // Never early is an explicit null (absent would be the default).
        let never = parsed(r#"{"agent_bus":{"wake_busy_wait_secs":-1}}"#);
        assert_eq!(saved(never)["agent_bus"], json!({"wake_busy_wait_secs": null}));
        let reparsed: crate::profiles::model::CodezConfig = serde_json::from_value(saved(never)).unwrap();
        assert_eq!(reparsed.agent_bus.wake_busy_wait(), None);
        let thirty = parsed(r#"{"agent_bus":{"wake_busy_wait_secs":30}}"#);
        assert_eq!(saved(thirty)["agent_bus"], json!({"wake_busy_wait_secs": 30}));
        let zero = parsed(r#"{"agent_bus":{"wake_busy_wait_secs":0}}"#);
        assert_eq!(saved(zero)["agent_bus"], json!({"wake_busy_wait_secs": 0}));
    }

    #[test]
    fn settings_patch_and_view() {
        let settings = AgentBusSettings::default();
        let view = settings.view();
        assert_eq!(view["agentBus"]["wakeBusyWaitSecs"], 120);
        assert_eq!(view["agentBusDefault"]["wakeBusyWaitSecs"], 120);
        assert_eq!(view["agentBus"]["resultCoalescing"]["claudeSubscription"]["quietSecs"], 120);
        let patched = settings.patched(&json!({"resultCoalescing":{"codex":{"enabled":false}}})).unwrap();
        assert!(!patched.result_coalescing.codex.enabled);
        let roundtrip: AgentBusSettings = serde_json::from_value(serde_json::to_value(patched).unwrap()).unwrap();
        assert_eq!(roundtrip, patched);
        assert_eq!(view["agentBusRange"]["wakeBusyWaitSecs"], json!([0, 86400]));
        let zero = settings.patched(&json!({"wakeBusyWaitSecs": 0})).unwrap();
        assert_eq!(zero.wake_busy_wait(), Some(Duration::ZERO));
        assert_eq!(zero.view()["agentBus"]["wakeBusyWaitSecs"], 0);
        let never = settings.patched(&json!({"wakeBusyWaitSecs": null})).unwrap();
        assert_eq!(never.wake_busy_wait(), None);
        assert_eq!(never.view()["agentBus"]["wakeBusyWaitSecs"], Value::Null);
        assert_eq!(settings.patched(&json!({"wakeBusyWaitSecs": -5})).unwrap(), never);
        let max = settings.patched(&json!({"wakeBusyWaitSecs": 86400})).unwrap();
        assert_eq!(max.wake_busy_wait(), Some(Duration::from_secs(86_400)));
        assert_eq!(settings.patched(&json!({})).unwrap(), settings);
        for bad in [
            json!({"wakeBusyWaitSecs": 86401}),
            json!({"wakeBusyWaitSecs": "60"}),
            json!({"wakeBusyWaitSecs": true}),
            json!({"bogus": 1}),
            json!(60),
        ] {
            assert!(settings.patched(&bad).is_err(), "{bad}");
        }
    }
}
