//! Process-local proxy traffic. The supervisor remains the sole state.json writer.
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct TrafficSnapshot {
    pub active_connections: usize,
    pub last_traffic_at: Option<String>,
}
#[derive(Default)]
struct Entry {
    snapshot: TrafficSnapshot,
    persisted: Option<Instant>,
}
static TRAFFIC: OnceLock<Mutex<BTreeMap<String, Entry>>> = OnceLock::new();
fn entries() -> &'static Mutex<BTreeMap<String, Entry>> {
    TRAFFIC.get_or_init(Mutex::default)
}

pub fn snapshot(slug: &str) -> TrafficSnapshot {
    entries()
        .lock()
        .unwrap()
        .get(slug)
        .map(|e| e.snapshot.clone())
        .unwrap_or_default()
}
/// The traffic of this record: the slug's counters, with a last-traffic
/// time from before the record was created dropped (it belonged to an
/// earlier service of the same slug that was unregistered or reclaimed).
pub fn snapshot_for(record: &super::store::ServiceRecord) -> TrafficSnapshot {
    let mut snapshot = snapshot(&record.slug);
    snapshot.last_traffic_at = current_traffic(record, snapshot.last_traffic_at);
    snapshot
}
/// `at`, unless it predates `record` (an earlier record of the same slug).
pub fn current_traffic(record: &super::store::ServiceRecord, at: Option<String>) -> Option<String> {
    let at = at?;
    let created = super::lifecycle::parse_time(&record.created_at);
    let seen = super::lifecycle::parse_time(&at);
    match (created, seen) {
        (Some(created), Some(seen)) if seen < created => None,
        _ => Some(at),
    }
}
/// Drop a slug's counters (its record is gone or was replaced by a new
/// record of the same slug). A guard still open on the old record only
/// decrements (saturating) when it closes.
pub fn forget(slug: &str) {
    entries().lock().unwrap().remove(slug);
}
pub fn touch(slug: &str) {
    entries()
        .lock()
        .unwrap()
        .entry(slug.into())
        .or_default()
        .snapshot
        .last_traffic_at = Some(super::store::now_rfc3339());
}
pub struct ConnectionGuard(String);
pub fn begin(slug: &str) -> ConnectionGuard {
    let mut entries = entries().lock().unwrap();
    let entry = entries.entry(slug.into()).or_default();
    entry.snapshot.active_connections += 1;
    entry.snapshot.last_traffic_at = Some(super::store::now_rfc3339());
    ConnectionGuard(slug.into())
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(entry) = entries().lock().unwrap().get_mut(&self.0) {
            entry.snapshot.active_connections = entry.snapshot.active_connections.saturating_sub(1);
        }
    }
}
/// Publish a changed timestamp at most once per minute (even if health causes
/// more frequent state writes). Returns None when there is nothing due.
pub fn take_persisted_timestamp(slug: &str) -> Option<String> {
    let mut entries = entries().lock().unwrap();
    let entry = entries.get_mut(slug)?;
    if entry
        .persisted
        .is_some_and(|last| last.elapsed() < Duration::from_secs(60))
    {
        return None;
    }
    let value = entry.snapshot.last_traffic_at.clone()?;
    entry.persisted = Some(Instant::now());
    Some(value)
}
#[cfg(test)]
mod tests {
    #[test]
    fn guard_and_persistence() {
        let slug = format!("traffic-{}", uuid::Uuid::new_v4());
        let guard = super::begin(&slug);
        assert_eq!(super::snapshot(&slug).active_connections, 1);
        assert!(super::take_persisted_timestamp(&slug).is_some());
        super::touch(&slug);
        assert!(super::take_persisted_timestamp(&slug).is_none());
        drop(guard);
        assert_eq!(super::snapshot(&slug).active_connections, 0);
        super::forget(&slug);
        assert!(super::snapshot(&slug).last_traffic_at.is_none());
    }

    #[test]
    fn traffic_from_before_the_record_belongs_to_an_earlier_service() {
        let slug = format!("traffic-{}", uuid::Uuid::new_v4());
        super::touch(&slug);
        let old = super::snapshot(&slug).last_traffic_at.unwrap();
        let mut record: crate::services::store::ServiceRecord = serde_json::from_value(serde_json::json!({
            "slug": slug, "displayName": slug, "host": "127.0.0.1", "port": 31999,
            "runtime": "external", "createdAt": "", "updatedAt": "",
        }))
        .unwrap();
        record.created_at = (chrono::Utc::now() + chrono::Duration::seconds(5))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        assert!(super::snapshot_for(&record).last_traffic_at.is_none());
        assert!(super::current_traffic(&record, Some(old.clone())).is_none());
        record.created_at = "2020-01-01T00:00:00.000Z".into();
        assert_eq!(super::snapshot_for(&record).last_traffic_at, Some(old));
        super::forget(&slug);
    }
}
