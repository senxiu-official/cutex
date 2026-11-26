//! Recent service notification receiver reports from owner-paired Android devices.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::http::server::SimpleHttpRequest;
use crate::management::{
    client_tokens::{ClientTokenStore, IssuedVia, VerifiedClientToken, KIND_DEVICE},
    listeners::current_client_token,
    users::{principal_in, UserStore},
};
use crate::services::store::{ServicesError, ServicesResult};

pub(super) const FEATURE: &str = "service-notification-receivers-v1";
pub(super) const PATH: &str = "/v2/service-notifications/receiver";
const REPORT_TTL: Duration = Duration::from_secs(90);

#[derive(Default)]
struct Receivers(HashMap<String, Instant>);

impl Receivers {
    fn prune(&mut self, now: Instant) {
        self.0
            .retain(|_, observed| now.saturating_duration_since(*observed) < REPORT_TTL);
    }

    fn report(&mut self, token_id: String, allowed: bool, now: Instant) {
        self.prune(now);
        if allowed {
            self.0.insert(token_id, now);
        } else {
            self.0.remove(&token_id);
        }
    }

    fn reachable(&mut self, paired: &HashSet<String>, now: Instant) -> bool {
        self.prune(now);
        self.0.retain(|token, _| paired.contains(token));
        !self.0.is_empty()
    }
}

static RECEIVERS: LazyLock<Mutex<Receivers>> = LazyLock::new(|| Mutex::new(Receivers::default()));

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PhoneStatus {
    android_paired: Option<bool>,
    reachable: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Report {
    allowed: bool,
}

fn paired_android() -> anyhow::Result<HashSet<String>> {
    let tokens = ClientTokenStore::open_default()?.list()?;
    let users = UserStore::open_default()?.snapshot()?;
    Ok(tokens
        .into_iter()
        .filter(|token| {
            token.kind == KIND_DEVICE
                && token.issued_via == IssuedVia::Pairing
                && token.scope.is_full()
                && token.platform.as_deref() == Some("android")
                && principal_in(&users, &token.token_id).is_owner()
        })
        .map(|token| token.token_id)
        .collect())
}

fn owner_pairing(token: &VerifiedClientToken) -> bool {
    token.issued_via == IssuedVia::Pairing && token.scope.is_full() && token.principal.is_owner()
}

pub(super) fn advertises_to_summary_caller() -> bool {
    current_client_token().as_ref().is_some_and(owner_pairing)
}

pub(super) fn status() -> PhoneStatus {
    match paired_android() {
        Ok(paired) => PhoneStatus {
            android_paired: Some(!paired.is_empty()),
            reachable: RECEIVERS
                .lock()
                .ok()
                .map(|mut live| live.reachable(&paired, Instant::now())),
        },
        Err(_) => PhoneStatus {
            android_paired: None,
            reachable: None,
        },
    }
}

pub(super) fn report(request: &SimpleHttpRequest, value: Report) -> ServicesResult<()> {
    let refused = || {
        ServicesError::new(
            403,
            "android_receiver_pairing_required",
            "an owner-paired Android device is required",
        )
    };
    if request.headers.contains_key("origin") || request.headers.contains_key("sec-fetch-site") {
        return Err(refused());
    }
    let token = current_client_token()
        .filter(owner_pairing)
        .ok_or_else(refused)?;
    let unavailable = || {
        ServicesError::new(
            503,
            "service_receiver_state_unavailable",
            "Android receiver status is unavailable",
        )
    };
    let paired = paired_android().map_err(|_| unavailable())?;
    if !paired.contains(&token.token_id) {
        return Err(refused());
    }
    RECEIVERS.lock().map_err(|_| unavailable())?.report(
        token.token_id,
        value.allowed,
        Instant::now(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_notifications_receiver_status_expires_and_respects_pairing_or_permission_changes() {
        let now = Instant::now();
        let paired = HashSet::from(["phone".to_string()]);
        let mut receivers = Receivers::default();
        receivers.report("phone".into(), true, now);
        assert!(receivers.reachable(&paired, now + REPORT_TTL - Duration::from_secs(1)));
        assert!(!receivers.reachable(&paired, now + REPORT_TTL));
        receivers.report("phone".into(), true, now);
        receivers.report("phone".into(), false, now);
        assert!(!receivers.reachable(&paired, now));
        receivers.report("phone".into(), true, now);
        assert!(!receivers.reachable(&HashSet::new(), now));
    }
}
