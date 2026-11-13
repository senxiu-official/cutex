//! Persistent managed-service idle policy. Older registrations default to forever.
use super::store::{ServiceLifetime, ServiceRecord, ServiceRuntime, ServicesError, ServicesResult};
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "service-idle-presets-v1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdleStopPreset {
    #[serde(rename = "1h")]
    Hour,
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[default]
    #[serde(rename = "forever")]
    Forever,
}
impl IdleStopPreset {
    pub fn parse(value: &str) -> ServicesResult<Self> {
        match value {
            "1h" => Ok(Self::Hour),
            "24h" => Ok(Self::Day),
            "7d" => Ok(Self::Week),
            "forever" => Ok(Self::Forever),
            _ => Err(ServicesError::bad_request(
                "idleStopPreset must be 1h, 24h, 7d or forever",
            )),
        }
    }
    pub fn seconds(self) -> Option<i64> {
        match self {
            Self::Hour => Some(3600),
            Self::Day => Some(86400),
            Self::Week => Some(604800),
            Self::Forever => None,
        }
    }
}
pub fn eligible(record: &ServiceRecord) -> bool {
    record.runtime.supervised() && record.lifetime == ServiceLifetime::Persistent
}
pub fn policy(
    runtime: ServiceRuntime,
    lifetime: ServiceLifetime,
    requested: Option<&str>,
    previous: Option<&ServiceRecord>,
) -> ServicesResult<IdleStopPreset> {
    if !runtime.supervised() || lifetime != ServiceLifetime::Persistent {
        if requested.is_some() {
            return Err(ServicesError::bad_request(
                "idleStopPreset applies only to persistent managed or worker services",
            ));
        }
        return Ok(IdleStopPreset::Forever);
    }
    requested.map(IdleStopPreset::parse).unwrap_or_else(|| {
        Ok(previous
            .filter(|r| eligible(r))
            .map_or(if runtime == ServiceRuntime::Worker { IdleStopPreset::Forever } else { IdleStopPreset::Day }, |r| r.idle_stop_preset))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    fn old_record() -> ServiceRecord {
        serde_json::from_value(serde_json::json!({
            "slug":"idle-policy", "displayName":"Idle policy", "host":"fixture", "port":31481,
            "runtime":"managed", "lifetime":"persistent", "createdAt":"2026-11-04T00:00:00Z", "updatedAt":"2026-11-04T00:00:00Z"
        })).unwrap()
    }
    #[test]
    fn old_records_remain_forever_new_eligible_records_default_to_day() {
        let old = old_record();
        assert_eq!(old.idle_stop_preset, IdleStopPreset::Forever);
        assert_eq!(
            policy(old.runtime, old.lifetime, None, Some(&old)).unwrap(),
            IdleStopPreset::Forever
        );
        assert_eq!(
            policy(old.runtime, old.lifetime, None, None).unwrap(),
            IdleStopPreset::Day
        );
        for preset in ["1h", "24h", "7d", "forever"] {
            assert!(policy(old.runtime, old.lifetime, Some(preset), Some(&old)).is_ok());
        }
        for preset in ["", "0", "never", "30m"] {
            assert!(IdleStopPreset::parse(preset).is_err());
        }
        assert!(policy(ServiceRuntime::External, old.lifetime, Some("1h"), None).is_err());
        assert!(policy(ServiceRuntime::Static, old.lifetime, Some("1h"), None).is_err());
        assert!(policy(old.runtime, ServiceLifetime::Ephemeral, Some("1h"), None).is_err());
    }
    #[test]
    fn idle_sleep_and_failed_attempt_survive_record_roundtrip_separately_from_manual_stop() {
        let mut record = old_record();
        record.stopped_idle_at = Some("2026-11-04T01:00:00Z".into());
        record.idle_wake_failed_at = Some("2026-11-04T01:01:00Z".into());
        let saved = serde_json::to_vec(&record).unwrap();
        let restored: ServiceRecord = serde_json::from_slice(&saved).unwrap();
        assert_eq!(restored, record);
        assert!(!restored.stopped);
        let view = super::super::store::service_view(&restored, None);
        assert_eq!(view["health"], "stoppedIdle");
        assert_eq!(view["stopped"], false);
        assert_eq!(view["stoppedIdleAt"], "2026-11-04T01:00:00Z");
    }
    #[test]
    fn traffic_active_connections_and_leases_postpone_sleep_but_manual_stop_never_becomes_idle() {
        let start = Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap();
        let now = start + Duration::hours(2);
        let life = super::super::lifecycle::Lifecycle::new(start);
        let mut record = old_record();
        record.idle_stop_preset = IdleStopPreset::Hour;
        assert_eq!(
            life.idle_stop_at(&record, None, None, 0, now),
            Some(start + Duration::hours(1))
        );
        let recent = (now - Duration::minutes(10)).to_rfc3339();
        // Explicit Wake starts a fresh period even when old proxy use is stale.
        assert_eq!(
            life.idle_stop_at(&record, None, Some(&recent), 0, now),
            Some(now + Duration::minutes(50))
        );
        assert_eq!(
            life.idle_stop_at(&record, Some(&recent), None, 0, now),
            Some(now + Duration::minutes(50))
        );
        assert_eq!(life.idle_stop_at(&record, None, None, 1, now), None);
        let lease = life.lease(&record.slug, None, None, now).unwrap();
        assert_eq!(life.idle_stop_at(&record, None, None, 0, now), None);
        life.release(&record.slug, &lease.lease_id, now);
        assert_eq!(
            life.idle_stop_at(&record, None, None, 0, now),
            Some(now + Duration::hours(1))
        );
        record.stopped = true;
        assert_eq!(life.idle_stop_at(&record, None, None, 0, now), None);
        record.stopped = false;
        record.stopped_idle_at = Some(now.to_rfc3339());
        assert_eq!(life.idle_stop_at(&record, None, None, 0, now), None);
    }
}
