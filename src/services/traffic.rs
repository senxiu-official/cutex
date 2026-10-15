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
    }
}
