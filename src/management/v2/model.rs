use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const CONTRACT_VERSION: u8 = 2;
pub const MAX_SAFE_SEQUENCE: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppServerSchema {
    pub protocol: String,
    pub major_version: u8,
    pub version: String,
    pub sha256: String,
    pub channel: AppServerSchemaChannel,
    pub capabilities: Value,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AppServerSchemaChannel {
    Stable,
    Experimental,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventCorrelation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_bus_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_request_id: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    AppServer,
    Cutex,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NativeMessageKind {
    Notification,
    ServerRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativeMessage {
    pub kind: NativeMessageKind,
    pub message: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CutexMessage {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingEvent {
    pub cutex_session_id: String,
    pub host_id: String,
    pub source: EventSource,
    pub schema: Option<AppServerSchema>,
    pub correlation: EventCorrelation,
    pub native: Option<NativeMessage>,
    pub cutex: Option<CutexMessage>,
}

impl PendingEvent {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.cutex_session_id.is_empty() {
            anyhow::bail!("cutexSessionId must not be empty");
        }
        if self.host_id.is_empty() {
            anyhow::bail!("hostId must not be empty");
        }
        match self.source {
            EventSource::AppServer => {
                if self.schema.is_none() || self.native.is_none() || self.cutex.is_some() {
                    anyhow::bail!(
                        "app_server event requires schema/native and forbids cutex payload"
                    );
                }
                if self.correlation.runtime_generation.is_none() {
                    anyhow::bail!("app_server event requires runtime generation correlation");
                }
            }
            EventSource::Cutex => {
                if self.cutex.is_none() || self.schema.is_some() || self.native.is_some() {
                    anyhow::bail!(
                        "cutex event requires cutex payload and forbids schema/native payloads"
                    );
                }
                if self.correlation.runtime_generation.is_some() {
                    anyhow::bail!("cutex event forbids runtime generation correlation");
                }
            }
        }
        if self
            .correlation
            .runtime_generation
            .is_some_and(|generation| generation == 0 || generation > MAX_SAFE_SEQUENCE)
        {
            anyhow::bail!("runtime generation is outside the positive JSON-safe range");
        }
        if let Some(native) = &self.native {
            let object = native
                .message
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("native.message must be an object"))?;
            if object
                .get("method")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                anyhow::bail!("native.message.method must be a non-empty string");
            }
            if native.kind == NativeMessageKind::ServerRequest
                && object.get("id").is_none_or(Value::is_null)
            {
                anyhow::bail!("native server request requires its exact id");
            }
        }
        if let Some(cutex) = &self.cutex {
            if !cutex.method.starts_with("cutex/") || cutex.method.matches('/').count() < 2 {
                anyhow::bail!("cutex method must be a namespaced cutex method");
            }
            if !cutex.params.is_object() {
                anyhow::bail!("cutex params must be an object");
            }
            super::contract_validation::validate_cutex_event_message(&serde_json::to_value(cutex)?)
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    pub contract_version: u8,
    pub event_id: String,
    pub cursor: String,
    pub stream_id: String,
    pub sequence: u64,
    pub received_at: String,
    pub cutex_session_id: String,
    pub host_id: String,
    pub source: EventSource,
    pub sensitivity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<AppServerSchema>,
    pub correlation: EventCorrelation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cutex: Option<CutexMessage>,
}

impl EventEnvelope {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.contract_version != CONTRACT_VERSION {
            anyhow::bail!("unsupported contractVersion: {}", self.contract_version);
        }
        if self.sequence == 0 || self.sequence > MAX_SAFE_SEQUENCE {
            anyhow::bail!("event sequence is outside the JSON-safe range");
        }
        if self.event_id.is_empty() || self.cursor.is_empty() || self.stream_id.is_empty() {
            anyhow::bail!("event, cursor, and stream identities must not be empty");
        }
        if self.sensitivity != "owner" {
            anyhow::bail!("management v2 events must use owner sensitivity");
        }
        PendingEvent {
            cutex_session_id: self.cutex_session_id.clone(),
            host_id: self.host_id.clone(),
            source: self.source,
            schema: self.schema.clone(),
            correlation: self.correlation.clone(),
            native: self.native.clone(),
            cutex: self.cutex.clone(),
        }
        .validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventCheckpoint {
    pub stream_id: String,
    pub sequence: u64,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventStreamBoundary {
    pub sequence: u64,
    pub cursor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventStreamMetadata {
    pub stream_id: String,
    pub earliest: Option<EventStreamBoundary>,
    pub latest: Option<EventStreamBoundary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventPageScope {
    pub cutex_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventPage {
    pub contract_version: u8,
    pub host_id: String,
    pub stream_id: String,
    pub scope: EventPageScope,
    pub events: Vec<EventEnvelope>,
    pub next_cursor: Option<String>,
    pub checkpoint: EventCheckpoint,
    pub scanned_count: usize,
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn envelope_round_trip_preserves_unknown_native_fields_and_nulls() {
        let envelope = EventEnvelope {
            contract_version: CONTRACT_VERSION,
            event_id: "event-1".to_string(),
            cursor: "c2:cursor-1".to_string(),
            stream_id: "stream-1".to_string(),
            sequence: 1,
            received_at: "2026-07-13T00:00:00Z".to_string(),
            cutex_session_id: "cutex.session-1".to_string(),
            host_id: "devbox".to_string(),
            source: EventSource::AppServer,
            sensitivity: "owner".to_string(),
            schema: Some(AppServerSchema {
                protocol: "codex-app-server".to_string(),
                major_version: 2,
                version: "0.144.1".to_string(),
                sha256: "a".repeat(64),
                channel: AppServerSchemaChannel::Experimental,
                capabilities: json!({ "experimentalApi": true }),
                extensions: vec!["cutex-inter-agent-v2".to_string()],
            }),
            correlation: EventCorrelation {
                runtime_generation: Some(304),
                native_request_id: Some(json!(-9_223_372_036_854_775_808_i64)),
                ..EventCorrelation::default()
            },
            native: Some(NativeMessage {
                kind: NativeMessageKind::ServerRequest,
                message: json!({
                    "id": -9_223_372_036_854_775_808_i64,
                    "method": "future/request",
                    "params": { "explicitNull": null },
                    "future": { "mustSurvive": true }
                }),
            }),
            cutex: None,
        };

        envelope.validate().expect("valid envelope");
        let encoded = serde_json::to_vec(&envelope).expect("serialize envelope");
        let decoded: EventEnvelope =
            serde_json::from_slice(&encoded).expect("deserialize envelope");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn source_payloads_are_mutually_exclusive() {
        let pending = PendingEvent {
            cutex_session_id: "cutex.session-1".to_string(),
            host_id: "devbox".to_string(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation::default(),
            native: None,
            cutex: None,
        };
        assert!(pending.validate().is_err());
    }

    #[test]
    fn runtime_generation_correlation_matches_event_source() {
        let cutex = PendingEvent {
            cutex_session_id: "cutex.session-1".to_string(),
            host_id: "devbox".to_string(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation {
                runtime_generation: Some(304),
                ..EventCorrelation::default()
            },
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/runtime/online".to_string(),
                params: json!({}),
            }),
        };
        assert!(cutex.validate().is_err());

        let mut native = PendingEvent {
            cutex_session_id: "cutex.session-1".to_string(),
            host_id: "devbox".to_string(),
            source: EventSource::AppServer,
            schema: Some(AppServerSchema {
                protocol: "codex-app-server".to_string(),
                major_version: 2,
                version: "0.144.1".to_string(),
                sha256: "a".repeat(64),
                channel: AppServerSchemaChannel::Experimental,
                capabilities: json!({ "experimentalApi": true }),
                extensions: Vec::new(),
            }),
            correlation: EventCorrelation::default(),
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: json!({
                    "method": "item/started",
                    "params": {}
                }),
            }),
            cutex: None,
        };
        native.correlation.runtime_generation = None;
        assert!(native.validate().is_err());
        native.correlation.runtime_generation = Some(0);
        assert!(native.validate().is_err());
        native.correlation.runtime_generation = Some(MAX_SAFE_SEQUENCE + 1);
        assert!(native.validate().is_err());
    }
}
