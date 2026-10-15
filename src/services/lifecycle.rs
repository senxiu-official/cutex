//! In-memory viewing leases and deterministic ephemeral reclamation policy.
use super::store::{
    ServiceLifetime, ServiceRecord, ServiceStore, ServiceTombstone, ServicesError, ServicesResult,
    ServicesSettings,
};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

pub const LEASE_TTL_SECONDS: i64 = 150;
const MAX_LEASES: usize = 64;
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub lease_id: String,
    pub expires_at: DateTime<Utc>,
    pub ttl_seconds: i64,
    /// Idempotency scope `(caller, clientLeaseId)`; lives and expires with the lease.
    #[serde(skip)]
    client_key: Option<(String, String)>,
}
/// A lease open's idempotency key: the client's `clientLeaseId`, scoped to the
/// calling credential (client token id, or `root`).
#[derive(Clone, Copy, Debug)]
pub struct ClientLeaseKey<'a> {
    pub caller: &'a str,
    pub client_lease_id: &'a str,
}
#[derive(Default)]
struct Memory {
    leases: HashMap<String, BTreeMap<String, Lease>>,
    released: HashMap<String, DateTime<Utc>>,
}
pub struct Lifecycle {
    pub started_at: DateTime<Utc>,
    memory: Mutex<Memory>,
}
impl Lifecycle {
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            memory: Mutex::new(Memory::default()),
        }
    }
    pub fn lease(
        &self,
        slug: &str,
        id: Option<&str>,
        holder: Option<&str>,
        now: DateTime<Utc>,
    ) -> ServicesResult<Lease> {
        self.lease_keyed(slug, id, holder, None, now)
    }
    /// [`Self::lease`] with an optional idempotency key for a new lease: a
    /// repeated open with the same key from the same caller renews and
    /// returns the live lease it created instead of creating a second one.
    /// The key is ignored on a renewal (`id`).
    pub fn lease_keyed(
        &self,
        slug: &str,
        id: Option<&str>,
        holder: Option<&str>,
        key: Option<ClientLeaseKey<'_>>,
        now: DateTime<Utc>,
    ) -> ServicesResult<Lease> {
        if key.is_some_and(|k| {
            k.client_lease_id.is_empty()
                || k.client_lease_id.chars().count() > 128
                || k.client_lease_id.chars().any(char::is_control)
        }) {
            return Err(ServicesError::bad_request(
                "clientLeaseId must be 1 to 128 printable characters",
            ));
        }
        if holder.is_some_and(|s| s.chars().count() > 64 || s.chars().any(char::is_control)) {
            return Err(ServicesError::bad_request(
                "holder must be at most 64 printable characters",
            ));
        }
        let mut memory = self.memory.lock().expect("service leases");
        expire(&mut memory, slug, now);
        let leases = memory.leases.entry(slug.into()).or_default();
        let key = key
            .filter(|_| id.is_none())
            .map(|k| (k.caller.to_owned(), k.client_lease_id.to_owned()));
        if let Some(key) = &key {
            if let Some(existing) = leases
                .values_mut()
                .find(|lease| lease.client_key.as_ref() == Some(key))
            {
                existing.expires_at = now + Duration::seconds(LEASE_TTL_SECONDS);
                return Ok(existing.clone());
            }
        }
        if let Some(id) = id {
            if !leases.contains_key(id) {
                return Err(ServicesError::new(
                    404,
                    "lease_not_found",
                    "view lease expired or does not exist",
                ));
            }
        } else if leases.len() >= MAX_LEASES {
            return Err(ServicesError::new(
                409,
                "too_many_leases",
                "at most 64 viewing leases per service",
            ));
        }
        let client_key = match id {
            Some(id) => leases.get(id).and_then(|lease| lease.client_key.clone()),
            None => key,
        };
        let lease = Lease {
            lease_id: id
                .map(str::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            expires_at: now + Duration::seconds(LEASE_TTL_SECONDS),
            ttl_seconds: LEASE_TTL_SECONDS,
            client_key,
        };
        leases.insert(lease.lease_id.clone(), lease.clone());
        Ok(lease)
    }
    pub fn release(&self, slug: &str, id: &str, now: DateTime<Utc>) -> bool {
        let mut memory = self.memory.lock().expect("service leases");
        expire(&mut memory, slug, now);
        let released = memory
            .leases
            .get_mut(slug)
            .is_some_and(|leases| leases.remove(id).is_some());
        if released && memory.leases.get(slug).is_some_and(BTreeMap::is_empty) {
            memory.released.insert(slug.into(), now);
        }
        released
    }
    pub fn forget(&self, slug: &str) {
        let mut memory = self.memory.lock().expect("service leases");
        memory.leases.remove(slug);
        memory.released.remove(slug);
    }
    pub fn facts(&self, slug: &str, now: DateTime<Utc>) -> (usize, Option<DateTime<Utc>>) {
        let mut memory = self.memory.lock().expect("service leases");
        expire(&mut memory, slug, now);
        (
            memory.leases.get(slug).map_or(0, BTreeMap::len),
            memory.released.get(slug).copied(),
        )
    }
    pub fn idle_at(
        &self,
        record: &ServiceRecord,
        settings: &ServicesSettings,
        last_traffic: Option<&str>,
        active: usize,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let (leases, released) = self.facts(&record.slug, now);
        if record.lifetime == ServiceLifetime::Persistent || leases > 0 || active > 0 {
            return None;
        }
        let last = [
            parse_time(&record.created_at),
            last_traffic.and_then(parse_time),
            Some(self.started_at),
            released,
        ]
        .into_iter()
        .flatten()
        .max()?;
        Some(last + idle_duration(settings))
    }
    pub fn reason(
        &self,
        record: &ServiceRecord,
        settings: &ServicesSettings,
        last_traffic: Option<&str>,
        active: usize,
        owner_archived: bool,
        now: DateTime<Utc>,
    ) -> Option<&'static str> {
        if record.lifetime != ServiceLifetime::Ephemeral {
            return None;
        }
        if settings.reclaim_on_owner_archive && owner_archived {
            return Some("owner_archived");
        }
        if expires_at(record, settings).is_some_and(|at| now >= at) {
            return Some("ttl");
        }
        if self
            .idle_at(record, settings, last_traffic, active, now)
            .is_some_and(|at| now >= at)
        {
            return Some("idle");
        }
        None
    }
    pub fn augment_view(
        &self,
        record: &ServiceRecord,
        settings: &ServicesSettings,
        last_traffic: Option<&str>,
        active: usize,
        view: &mut Value,
    ) {
        let now = Utc::now();
        view["leases"] = json!(self.facts(&record.slug, now).0);
        view["expiresAt"] = json!(expires_at(record, settings));
        view["idleReclaimAt"] = json!(self.idle_at(record, settings, last_traffic, active, now));
        view["restartsLeft"] =
            if record.lifetime == ServiceLifetime::Ephemeral && record.runtime.supervised() {
                json!(settings
                    .ephemeral_max_restarts
                    .saturating_sub(view["process"]["restarts"].as_u64().unwrap_or(0) as u32))
            } else {
                Value::Null
            };
    }
}
fn idle_duration(settings: &ServicesSettings) -> Duration {
    // Private debug smoke only; the API range and release behavior stay intact.
    #[cfg(debug_assertions)]
    if let Some(boundary) = std::env::var_os("CUTEX_TEST_PRIVATE_HOME")
        .or_else(|| {
            crate::config::instance::is_isolated()
                .then(|| std::env::var_os("HOME"))
                .flatten()
        })
        .map(PathBuf::from)
    {
        if boundary
            .join(crate::config::paths::PRIVATE_HOME_MARKER)
            .is_file()
            && !crate::config::paths::is_real_user_home(&boundary)
            && crate::config::paths::config_dir()
                .ok()
                .is_some_and(|p| p.starts_with(&boundary))
        {
            if let Some(seconds) = std::env::var("CUTEX_SERVICES_DEBUG_IDLE_SECONDS")
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|n| (1..=300).contains(n))
            {
                return Duration::seconds(seconds);
            }
        }
    }
    Duration::minutes(settings.ephemeral_idle_minutes.into())
}
fn expire(memory: &mut Memory, slug: &str, now: DateTime<Utc>) {
    if let Some(leases) = memory.leases.get_mut(slug) {
        let last_expired = leases
            .values()
            .filter(|l| l.expires_at <= now)
            .map(|l| l.expires_at)
            .max();
        leases.retain(|_, lease| lease.expires_at > now);
        if leases.is_empty() {
            if let Some(at) = last_expired {
                memory.released.insert(slug.into(), at);
            }
        }
    }
}
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}
pub fn expires_at(record: &ServiceRecord, settings: &ServicesSettings) -> Option<DateTime<Utc>> {
    (record.lifetime == ServiceLifetime::Ephemeral)
        .then(|| {
            parse_time(&record.created_at)
                .map(|at| at + Duration::hours(settings.ephemeral_ttl_hours.into()))
        })
        .flatten()
}
pub fn for_store(store: &ServiceStore) -> Arc<Lifecycle> {
    static ALL: OnceLock<Mutex<HashMap<PathBuf, Arc<Lifecycle>>>> = OnceLock::new();
    ALL.get_or_init(Default::default)
        .lock()
        .expect("service lifecycles")
        .entry(store.dir().to_path_buf())
        .or_insert_with(|| Arc::new(Lifecycle::new(Utc::now())))
        .clone()
}
pub fn tombstone(record: &ServiceRecord, reason: &str, now: DateTime<Utc>) -> ServiceTombstone {
    ServiceTombstone {
        slug: record.slug.clone(),
        display_name: record.display_name.clone(),
        owner_cutex_session_id: record.owner_cutex_session_id.clone(),
        lifetime: record.lifetime,
        reason: reason.into(),
        reclaimed_at: now.to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(at: DateTime<Utc>) -> ServiceRecord {
        serde_json::from_value(json!({"slug":"test","displayName":"Test","host":"host","port":1,"runtime":"external","lifetime":"ephemeral","createdAt":at.to_rfc3339(),"updatedAt":at.to_rfc3339()})).unwrap()
    }
    #[test]
    fn restart_grace_leases_traffic_and_ttl_are_independent() {
        let now = Utc::now();
        let life = Lifecycle::new(now);
        let settings = ServicesSettings::default();
        let record = record(now - Duration::hours(1));
        assert_eq!(life.reason(&record, &settings, None, 0, false, now), None);
        let idle = now + Duration::minutes(31);
        assert_eq!(
            life.reason(&record, &settings, None, 0, false, idle),
            Some("idle")
        );
        assert_eq!(life.reason(&record, &settings, None, 1, false, idle), None);
        let lease = life.lease("test", None, None, idle).unwrap();
        assert_eq!(life.reason(&record, &settings, None, 0, false, idle), None);
        assert!(life.release("test", &lease.lease_id, idle));
        assert!(!life.release("test", &lease.lease_id, idle));
        assert_eq!(
            life.reason(
                &record,
                &settings,
                None,
                0,
                false,
                idle + Duration::minutes(29)
            ),
            None
        );
        assert_eq!(
            life.reason(&record, &settings, None, 0, true, idle),
            Some("owner_archived")
        );
        assert_eq!(
            life.reason(
                &record,
                &settings,
                None,
                99,
                false,
                now + Duration::hours(24)
            ),
            Some("ttl")
        );
        let mut persistent = record.clone();
        persistent.lifetime = ServiceLifetime::Persistent;
        assert_eq!(
            life.reason(
                &persistent,
                &settings,
                None,
                0,
                true,
                now + Duration::days(100)
            ),
            None
        );
    }
    #[test]
    fn lease_limits_expiry_and_renewal() {
        let now = Utc::now();
        let life = Lifecycle::new(now);
        for _ in 0..64 {
            life.lease("x", None, None, now).unwrap();
        }
        assert_eq!(
            life.lease("x", None, None, now).unwrap_err().code,
            "too_many_leases"
        );
        let lease = life.lease("y", None, None, now).unwrap();
        assert!(life
            .lease(
                "y",
                Some(&lease.lease_id),
                None,
                now + Duration::seconds(100)
            )
            .is_ok());
        assert_eq!(
            life.lease(
                "y",
                Some(&lease.lease_id),
                None,
                now + Duration::seconds(251)
            )
            .unwrap_err()
            .code,
            "lease_not_found"
        );
        assert_eq!(life.facts("x", now + Duration::seconds(151)).0, 0);
    }
    #[test]
    fn keyed_lease_opens_are_idempotent_per_caller() {
        let now = Utc::now();
        let life = Lifecycle::new(now);
        let key = |caller| {
            Some(ClientLeaseKey {
                caller,
                client_lease_id: "open-1",
            })
        };
        let first = life.lease_keyed("s", None, None, key("root"), now).unwrap();
        // A retried open (same caller, same key) returns the same lease, renewed.
        let later = now + Duration::seconds(100);
        let retry = life
            .lease_keyed("s", None, None, key("root"), later)
            .unwrap();
        assert_eq!(retry.lease_id, first.lease_id);
        assert_eq!(
            retry.expires_at,
            later + Duration::seconds(LEASE_TTL_SECONDS)
        );
        assert_eq!(life.facts("s", later).0, 1);
        // Another caller with the same key gets its own lease.
        let other = life
            .lease_keyed("s", None, None, key("tok-2"), later)
            .unwrap();
        assert_ne!(other.lease_id, first.lease_id);
        assert_eq!(life.facts("s", later).0, 2);
        // The same key on another service is a separate lease too.
        let elsewhere = life
            .lease_keyed("t", None, None, key("root"), later)
            .unwrap();
        assert_ne!(elsewhere.lease_id, first.lease_id);
        // A renewal by id keeps the key bound to the lease.
        life.lease("s", Some(&first.lease_id), None, later).unwrap();
        assert_eq!(
            life.lease_keyed("s", None, None, key("root"), later)
                .unwrap()
                .lease_id,
            first.lease_id
        );
        // No key: every open creates a new lease (old behaviour).
        let a = life.lease("s", None, None, later).unwrap();
        let b = life.lease("s", None, None, later).unwrap();
        assert_ne!(a.lease_id, b.lease_id);
        assert_eq!(life.facts("s", later).0, 4);
        // The mapping goes with the lease: after release a new lease is created.
        assert!(life.release("s", &first.lease_id, later));
        let fresh = life
            .lease_keyed("s", None, None, key("root"), later)
            .unwrap();
        assert_ne!(fresh.lease_id, first.lease_id);
        // ...and after expiry.
        let expired = later + Duration::seconds(LEASE_TTL_SECONDS + 1);
        let after = life
            .lease_keyed("s", None, None, key("root"), expired)
            .unwrap();
        assert_ne!(after.lease_id, fresh.lease_id);
        assert_eq!(life.facts("s", expired).0, 1);
        for bad in ["", "a\nb"] {
            let k = Some(ClientLeaseKey {
                caller: "root",
                client_lease_id: bad,
            });
            assert_eq!(
                life.lease_keyed("s", None, None, k, now).unwrap_err().code,
                "invalid_request"
            );
        }
    }
}
