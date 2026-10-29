//! Deliberately lossy, bounded metadata for unknown operations. Never accepts
//! error messages, arbitrary JSON, paths, credentials or native error bodies.
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct UnknownDiagnostic {
    code: &'static str,
    stage: &'static str,
    correlation: Value,
}

impl UnknownDiagnostic {
    pub fn new(code: &str, stage: &str) -> Self {
        let code = match code {
            "outcome_unknown" => "outcome_unknown",
            "request_outcome_unknown" => "request_outcome_unknown",
            "persistence_uncertain" => "persistence_uncertain",
            "event_persistence_unavailable" => "event_persistence_unavailable",
            "response_persistence_failed" => "response_persistence_failed",
            "native_request_failed" => "native_request_failed",
            "app_server_unavailable" => "app_server_unavailable",
            "internal_error" => "internal_error",
            "interrupt_timeout" => "interrupt_timeout",
            "revert_partial" => "revert_partial",
            _ => "unclassified",
        };
        let stage = match stage {
            "route" => "route",
            "native_request" => "native_request",
            "post_submit" => "post_submit",
            "response_write" => "response_write",
            "response_persistence" => "response_persistence",
            "persistence" => "persistence",
            "queue" => "queue",
            "interrupt" => "interrupt",
            _ => "unclassified",
        };
        Self {
            code,
            stage,
            correlation: json!({}),
        }
    }

    /// Only exact UUID-shaped IDs. Unknown keys and malformed values are
    /// omitted, not truncated or persisted as free-form diagnostics.
    pub fn with_correlation(mut self, source: &Value) -> Self {
        for key in ["queueId", "clientUserMessageId", "turnId", "threadId"] {
            if let Some(id) = source.get(key).and_then(Value::as_str).and_then(uuid_id) {
                self.correlation[key] = json!(id);
            }
        }
        self
    }

    pub(crate) fn for_claim(&self, session: &str, request: &str) -> Value {
        let mut correlation = self.correlation.clone();
        if let Some(id) = uuid_id(request) {
            correlation["requestId"] = json!(id);
        }
        if let Some(id) = session.strip_prefix("cutex.").and_then(uuid_id) {
            correlation["cutexSessionId"] = json!(format!("cutex.{id}"));
        }
        json!({"schema":"cutex.request-unknown-diagnostic.v1",
            "originalCode":self.code,"stage":self.stage,"correlation":correlation})
    }
}

fn uuid_id(value: &str) -> Option<&str> {
    (value.len() == 36
        && uuid::Uuid::parse_str(value).is_ok()
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        }))
    .then_some(value)
}
