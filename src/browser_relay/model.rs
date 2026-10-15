use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_DEVICES: usize = 32;
pub const MAX_DEVICE_QUEUE: usize = 32;
pub const MAX_DEVICE_REQUESTS: usize = 16;
pub const MAX_REQUESTS: usize = 128;
pub const MAX_SEQUENCE: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayError {
    pub code: &'static str,
    pub status: u16,
}
pub type Result<T> = std::result::Result<T, RelayError>;
impl RelayError {
    pub fn new(code: &'static str) -> Self {
        Self {
            code,
            status: match code {
                "unauthorized" => 401,
                "permission_denied" | "browser_disabled" | "loopback_only" => 403,
                "target_closed" => 404,
                "request_conflict"
                | "stale_connection"
                | "lease_conflict"
                | "request_result_not_retained" => 409,
                "busy" | "browser_unavailable" | "idempotency_capacity_exceeded" => 503,
                "message_too_large" => 413,
                _ => 400,
            },
        }
    }
    pub fn value(self) -> Value {
        json!({"error":{"code":self.code,"message":self.code,"retryable":matches!(self.code,"busy"|"idempotency_capacity_exceeded"),"details":{}}})
    }
    /// Provider exceptions are untrusted. Only stable protocol codes may cross
    /// this boundary; in particular the shell's typed scope refusal is retained.
    pub fn provider(code: &str) -> Self {
        Self::new(match code {
            "invalid_request" => "invalid_request",
            "unsupported_operation" => "unsupported_operation",
            "unsupported_provider" => "unsupported_provider",
            "permission_denied" => "permission_denied",
            "browser_disabled" => "browser_disabled",
            "browser_unavailable" => "browser_unavailable",
            "target_closed" => "target_closed",
            "lease_revoked" => "lease_revoked",
            "lease_ended_desktop_restarted" => "lease_ended_desktop_restarted",
            "target_hidden" => "target_hidden",
            "scope_unsupported" => "scope_unsupported",
            "scope_invalid" => "scope_invalid",
            "owner_paused" => "owner_paused",
            "stale_connection" => "stale_connection",
            "stale_request" => "stale_request",
            "stale_ref" => "stale_ref",
            "deadline_exceeded" => "deadline_exceeded",
            "cancelled" => "cancelled",
            "outcome_unknown" => "outcome_unknown",
            "request_conflict" => "request_conflict",
            "request_result_not_retained" => "request_result_not_retained",
            "approval_required" => "approval_required",
            "approval_denied" => "approval_denied",
            "lease_limit" => "lease_limit",
            "lease_conflict" => "lease_conflict",
            "busy" => "busy",
            "idempotency_capacity_exceeded" => "idempotency_capacity_exceeded",
            "external_pages_unavailable" => "external_pages_unavailable",
            "service_proxy_required" => "service_proxy_required",
            "navigation_failed" => "navigation_failed",
            "service_unavailable" => "service_unavailable",
            "service_auth_failed" => "service_auth_failed",
            "browser_identity_unavailable" => "browser_identity_unavailable",
            "message_too_large" => "message_too_large",
            _ => "invalid_result",
        })
    }
}
impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for RelayError {}

pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_' | b'.' | b':'))
}
pub fn display_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
}
pub fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T> {
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(RelayError::new("message_too_large"));
    }
    serde_json::from_slice(body).map_err(|_| RelayError::new("invalid_request"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registration {
    pub device_id: String,
    pub host_id: String,
    pub provider_id: String,
    pub executor: String,
}
impl Registration {
    pub fn validate(&self) -> Result<()> {
        if self.provider_id != "browser" {
            return Err(RelayError::new("unsupported_provider"));
        }
        if !identifier(&self.device_id)
            || !identifier(&self.host_id)
            || !matches!(self.executor.as_str(), "inApp" | "extension")
        {
            return Err(RelayError::new("invalid_request"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub host_id: String,
    pub device_id: String,
    pub guest_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum GrantSource {
    Owner {},
    Claim {
        #[serde(rename = "approvalId")]
        approval_id: String,
    },
    AgentOpen {
        #[serde(rename = "requestId")]
        request_id: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantRequest {
    pub device_id: String,
    pub guest_id: String,
    pub cutex_session_id: String,
    pub title: String,
    pub url: String,
    pub source: GrantSource,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceBody {
    pub device_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Heartbeat {
    #[serde(default)]
    pub owner_activity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseView {
    pub lease_id: String,
    pub cutex_session_id: String,
    pub target: Target,
    pub state: String,
    pub title: String,
    pub url: String,
    pub origin: String,
    pub agent_name: String,
}

/// Presentation only. Authority always uses the original validated operation.
pub fn safe_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return String::new();
    };
    if !matches!(url.scheme(), "http" | "https" | "cutex" | "about") {
        return String::new();
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    let value = url.to_string();
    if value.len() <= 4096 {
        value
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_wire_and_display() {
        assert!(parse::<Empty>(br#"{"key":"secret"}"#).is_err());
        assert!(parse::<DeviceBody>(br#"{"deviceId":"d","generation":1}"#).is_err());
        assert!(parse::<GrantSource>(br#"{"kind":"owner","requestId":"spoof"}"#).is_err());
        assert!(!identifier("../d"));
        assert!(!identifier("d\n"));
        assert_eq!(
            safe_url("https://u:p@example.com/a?q=secret#secret"),
            "https://example.com/a"
        );
        assert_eq!(safe_url("file:///private"), "");
        assert!(!display_text("\n", 200));
    }
    #[test]
    fn browser_scope_failures_are_typed_and_raw_errors_are_not_reflected() {
        for code in ["scope_unsupported", "scope_invalid"] {
            assert_eq!(RelayError::provider(code).code, code);
        }
        assert_eq!(
            RelayError::provider("secret stack or URL").code,
            "invalid_result"
        );
    }
}
