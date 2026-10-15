//! Daemon-to-daemon peers. These credentials never confer management authority.
#[cfg(test)]
mod accounts_prep_tests;
pub mod asks;
pub mod audit;
pub mod client;
#[cfg(test)]
mod fixes_tests;
#[cfg(test)]
mod http_tests;
pub mod model;
pub mod pair;
pub mod policy;
pub mod routes;
pub mod store;
pub mod strip;
pub mod tells;
#[cfg(test)]
mod tells_tests;
#[cfg(test)]
mod tests;
pub mod token;
pub mod wire;
#[cfg(test)]
mod wire_tests;
pub mod worker;

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Error {
    pub code: String,
    pub status: u16,
    pub retryable: bool,
    pub details: Value,
}
impl Error {
    pub fn new(code: &str, status: u16) -> Self {
        Self {
            code: code.into(),
            status,
            retryable: (status == 429 || status >= 500)
                && !matches!(
                    code,
                    "peer_protocol_error" | "peer_response_too_large" | "peer_unsupported"
                ),
            details: json!({}),
        }
    }
    pub fn is_protocol(&self) -> bool {
        matches!(
            self.code.as_str(),
            "peer_protocol_error" | "peer_response_too_large" | "peer_unsupported"
        )
    }
    pub fn is_transport(&self) -> bool {
        self.status >= 500 && !self.is_protocol()
    }
    pub fn is_address_rejected(&self) -> bool {
        matches!(
            self.code.as_str(),
            "peer_url_insecure" | "peer_callback_forbidden"
        )
    }
    /// Only fixed local codes may become durable failure reasons or diagnostics.
    pub fn request_failure_code(&self) -> &'static str {
        match self.code.as_str() {
            "peer_url_insecure" => "peer_url_insecure",
            "peer_callback_forbidden" => "peer_callback_forbidden",
            "peer_response_too_large" => "peer_response_too_large",
            "peer_unreachable" => "peer_unreachable",
            "persistence_unavailable" => "persistence_unavailable",
            "peer_store_corrupt" => "peer_store_corrupt",
            "peer_event_expired" => "peer_event_expired",
            code => Self::remote_failure_code(code),
        }
    }
    /// The peer can report a known refusal, but cannot impersonate a failure of
    /// our own address validation, response reader, transport or storage.
    pub(super) fn remote_failure_code(code: &str) -> &'static str {
        match code {
            "invalid_request" => "invalid_request",
            "request_too_large" => "request_too_large",
            "not_found" => "not_found",
            "recipient_outside_boundary" => "recipient_outside_boundary",
            "peer_policy_denied" => "peer_policy_denied",
            "peer_asks_disabled" => "peer_asks_disabled",
            "peer_hops_exceeded" => "peer_hops_exceeded",
            "peer_rate_limited" => "peer_rate_limited",
            "ask_id_conflict" => "ask_id_conflict",
            "idempotency_conflict" => "idempotency_conflict",
            "ask_not_found" => "ask_not_found",
            "ask_gone" => "ask_gone",
            "remote_unsupported" => "remote_unsupported",
            "peer_request_incompatible" => "peer_request_incompatible",
            "peer_unavailable" => "peer_unavailable",
            "peer_not_found" => "peer_not_found",
            "peer_user_unsupported" => "peer_user_unsupported",
            "peer_already_active" => "peer_already_active",
            "pair_incomplete" => "pair_incomplete",
            "invalid_pairing_code" => "invalid_pairing_code",
            "self_peer" => "self_peer",
            "peer_exists" => "peer_exists",
            "label_taken" => "label_taken",
            "id_not_found" => "id_not_found",
            "id_ambiguous" => "id_ambiguous",
            "delegation_loop" => "delegation_loop",
            "peer_protocol_error" => "peer_protocol_error",
            "peer_unsupported" => "peer_unsupported",
            "identity_changed" => "identity_changed",
            "peer_credential_required" => "peer_credential_required",
            "remote_credential_refused" => "remote_credential_refused",
            "loopback_only" => "loopback_only",
            "peer_callback_unavailable" => "peer_callback_unavailable",
            _ => "peer_request_failed",
        }
    }
    pub fn value(&self) -> Value {
        json!({"code":self.code,"message":self.code.replace('_', " "),"retryable":self.retryable,"details":self.details})
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code)
    }
}
impl std::error::Error for Error {}
impl From<anyhow::Error> for Error {
    fn from(_: anyhow::Error) -> Self {
        Self::new("persistence_unavailable", 503)
    }
}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::new("persistence_unavailable", 503)
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new("invalid_request", 400)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

pub fn instance_id() -> String {
    crate::management::instance::instance_id()
}
pub fn now() -> String {
    crate::management::instance::now_utc()
}
pub fn valid_id(id: &str) -> bool {
    id.strip_prefix("ci-")
        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 32
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod review2_client_tests;
#[cfg(test)]
mod review2_policy_tests;
#[cfg(test)]
mod review2_worker_tests;
#[cfg(test)]
mod review_protocol_tests;
#[cfg(test)]
mod review_snapshot_tests;
#[cfg(test)]
mod review_view_tests;
#[cfg(test)]
mod review_worker_tests;
