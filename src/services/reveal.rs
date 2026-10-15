//! Best-effort owner reveal requests, rate limited before publishing.
use super::store::ServiceRecord;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
#[derive(Default)]
struct Limiter {
    slugs: HashMap<String, DateTime<Utc>>,
    agents: HashMap<String, Vec<DateTime<Utc>>>,
}
impl Limiter {
    fn allow(&mut self, slug: &str, agent: &str, now: DateTime<Utc>) -> bool {
        self.slugs.retain(|_, at| *at > now - Duration::seconds(60));
        self.agents.retain(|_, times| {
            times.retain(|at| *at > now - Duration::hours(1));
            !times.is_empty()
        });
        if self.slugs.contains_key(slug)
            || self
                .agents
                .get(agent)
                .is_some_and(|times| times.len() >= 10)
        {
            return false;
        }
        self.slugs.insert(slug.into(), now);
        self.agents.entry(agent.into()).or_default().push(now);
        true
    }
}
pub fn request(record: &ServiceRecord, agent: &str) -> &'static str {
    static LIMITER: OnceLock<Mutex<Limiter>> = OnceLock::new();
    let now = Utc::now();
    if !LIMITER
        .get_or_init(Default::default)
        .lock()
        .expect("service reveal limiter")
        .allow(&record.slug, agent, now)
    {
        return "rateLimited";
    }
    let result = crate::management::v2::services::publish_reveal(
        json!({"slug":record.slug,"hostId":crate::platform::host::current_host_name(),"ownerCutexSessionId":record.owner_cutex_session_id,"path":record.path.as_deref().unwrap_or("/"),"requestedAt":now.to_rfc3339()}),
    );
    if result.is_ok() {
        "requested"
    } else {
        "unavailable"
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reveal_limits_slug_and_agent() {
        let mut limiter = Limiter::default();
        let now = Utc::now();
        assert!(limiter.allow("a", "agent", now));
        assert!(!limiter.allow("a", "other", now));
        for n in 1..10 {
            assert!(limiter.allow(&format!("s{n}"), "agent", now));
        }
        assert!(!limiter.allow("last", "agent", now + Duration::minutes(1)));
        assert!(limiter.allow("a", "agent", now + Duration::hours(1)));
    }
}
