use chrono::Utc;
use serde_json::json;

use crate::app_server::client::AppServerEvent;
use crate::app_server::protocol::correlations;

use super::model::AppServerSchema;
use super::model::CutexMessage;
use super::model::EventCorrelation;
use super::model::EventSource;
use super::model::NativeMessage;
use super::model::NativeMessageKind;
use super::model::PendingEvent;

#[derive(Debug, Clone)]
pub struct NativeEventContext {
    pub cutex_session_id: String,
    pub thread_id: String,
    pub host_id: String,
    pub runtime_generation: u64,
    pub runtime_backend: String,
    pub schema: AppServerSchema,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeEventDisposition {
    Publish(PendingEvent),
    /// A native notification or server request addressed to a thread other
    /// than the bound one (for example a Codex sub-agent thread). The caller
    /// decides from verified sub-agent lineage whether to publish it; the
    /// native `threadId` stays intact in `pending`.
    OtherThread {
        thread_id: String,
        pending: PendingEvent,
    },
}

pub fn pending_event_from_app_server(
    context: &NativeEventContext,
    event: &AppServerEvent,
) -> anyhow::Result<NativeEventDisposition> {
    let pending = match event {
        AppServerEvent::Notification(notification) => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::AppServer,
            schema: Some(context.schema.clone()),
            correlation: correlation_for_raw(&notification.raw, context.runtime_generation, None),
            native: Some(NativeMessage {
                kind: NativeMessageKind::Notification,
                message: notification.raw.clone(),
            }),
            cutex: None,
        },
        AppServerEvent::ServerRequest(request) => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::AppServer,
            schema: Some(context.schema.clone()),
            correlation: correlation_for_raw(
                &request.raw,
                context.runtime_generation,
                Some(request.id.clone()),
            ),
            native: Some(NativeMessage {
                kind: NativeMessageKind::ServerRequest,
                message: request.raw.clone(),
            }),
            cutex: None,
        },
        AppServerEvent::ProtocolViolation { message } => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation::default(),
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/runtime/protocolViolation".to_string(),
                params: json!({
                    "code": "invalid_native_message",
                    "message": message,
                    "nativeEventId": null,
                    "resyncRequired": true,
                    "detectedAt": Utc::now().to_rfc3339(),
                }),
            }),
        },
        AppServerEvent::Disconnected { reason } => PendingEvent {
            cutex_session_id: context.cutex_session_id.clone(),
            host_id: context.host_id.clone(),
            source: EventSource::Cutex,
            schema: None,
            correlation: EventCorrelation::default(),
            native: None,
            cutex: Some(CutexMessage {
                method: "cutex/runtime/disconnected".to_string(),
                params: json!({
                    "runtimeGeneration": context.runtime_generation,
                    "backend": context.runtime_backend,
                    "status": "error",
                    "runtimeAgentId": null,
                    "reason": reason,
                    "occurredAt": Utc::now().to_rfc3339(),
                    "error": {
                        "source": "cutex",
                        "code": "app_server_disconnected",
                        "message": reason,
                        "retryable": true,
                        "details": {},
                    }
                }),
            }),
        },
    };
    pending.validate()?;

    let thread_id = validated_thread_correlation(&pending)?;
    if let Some(thread_id) = thread_id {
        if thread_id != context.thread_id {
            let thread_id = thread_id.to_string();
            return Ok(NativeEventDisposition::OtherThread { thread_id, pending });
        }
    }
    Ok(NativeEventDisposition::Publish(pending))
}

fn validated_thread_correlation(pending: &PendingEvent) -> anyhow::Result<Option<&str>> {
    let Some(native) = pending.native.as_ref() else {
        return Ok(pending.correlation.thread_id.as_deref());
    };
    let params = native.message.get("params").unwrap_or(&native.message);
    let Some(params) = params.as_object() else {
        return Ok(pending.correlation.thread_id.as_deref());
    };

    let mut thread_id = None;
    for field in ["threadId", "thread_id"] {
        let Some(value) = params.get(field) else {
            continue;
        };
        let value = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("native event has an invalid {field} correlation"))?;
        if let Some(previous) = thread_id {
            if previous != value {
                anyhow::bail!("native event has conflicting thread correlations");
            }
        }
        thread_id = Some(value);
    }

    if thread_id != pending.correlation.thread_id.as_deref() {
        anyhow::bail!("native event thread correlation could not be validated");
    }
    Ok(thread_id)
}

fn correlation_for_raw(
    raw: &serde_json::Value,
    runtime_generation: u64,
    native_request_id: Option<serde_json::Value>,
) -> EventCorrelation {
    let ids = correlations(raw);
    EventCorrelation {
        runtime_generation: Some(runtime_generation),
        thread_id: ids.thread_id,
        turn_id: ids.turn_id,
        item_id: ids.item_id,
        client_user_message_id: ids.client_user_message_id,
        native_request_id,
        ..EventCorrelation::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::app_server::protocol::RpcNotification;
    use crate::app_server::protocol::RpcServerRequest;
    use crate::management::v2::model::AppServerSchemaChannel;

    fn context() -> NativeEventContext {
        NativeEventContext {
            cutex_session_id: "cutex.session-1".to_string(),
            thread_id: "thread-1".to_string(),
            host_id: "devbox".to_string(),
            runtime_generation: 9,
            runtime_backend: "cute_alden".to_string(),
            schema: AppServerSchema {
                protocol: "codex-app-server".to_string(),
                major_version: 2,
                version: "0.144.1+cutex-inter-agent-v2".to_string(),
                sha256: "a".repeat(64),
                channel: AppServerSchemaChannel::Experimental,
                capabilities: json!({ "experimentalApi": true }),
                extensions: vec!["cutex-inter-agent-v2".to_string()],
            },
        }
    }

    #[test]
    fn unknown_notification_is_preserved_without_projection() {
        let raw = json!({
            "method": "future/native/event",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {
                    "id": "item-1",
                    "clientId": "client-message-1",
                    "explicitNull": null,
                    "future": { "mustSurvive": true }
                }
            },
            "futureTopLevel": null
        });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "future/native/event".to_string(),
                params: raw.get("params").cloned(),
                raw: raw.clone(),
            }),
        )
        .expect("convert notification");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("root notification must publish");
        };
        assert_eq!(pending.native.unwrap().message, raw);
        assert_eq!(pending.correlation.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(
            pending.correlation.client_user_message_id.as_deref(),
            Some("client-message-1")
        );
    }

    #[test]
    fn thread_reverted_notification_is_published_to_the_session_stream() {
        let raw = json!({ "method": "thread/reverted", "params": { "threadId": "thread-1" } });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "thread/reverted".to_string(),
                params: raw.get("params").cloned(),
                raw: raw.clone(),
            }),
        )
        .expect("convert thread/reverted");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("thread/reverted for the bound thread must publish");
        };
        assert_eq!(pending.native.unwrap().message, raw);
        assert_eq!(pending.correlation.thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn server_request_keeps_full_signed_id_domain() {
        let id = json!(-9_223_372_036_854_775_808_i64);
        let raw = json!({
            "id": id,
            "method": "future/native/request",
            "params": { "threadId": "thread-1", "explicitNull": null }
        });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::ServerRequest(RpcServerRequest {
                id: id.clone(),
                method: "future/native/request".to_string(),
                params: raw.get("params").cloned(),
                raw: raw.clone(),
            }),
        )
        .expect("convert server request");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("root server request must publish");
        };
        assert_eq!(pending.correlation.native_request_id, Some(id));
        assert_eq!(pending.correlation.runtime_generation, Some(9));
        assert_eq!(pending.native.unwrap().message, raw);
    }

    #[test]
    fn foreign_thread_notification_is_explicitly_ignored_after_validation() {
        let raw = json!({
            "method": "item/completed",
            "params": {
                "threadId": "foreign-thread",
                "turnId": "turn-1",
                "item": { "type": "agentMessage", "id": "item-1", "text": "no" }
            }
        });
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "item/completed".to_string(),
                params: raw.get("params").cloned(),
                raw,
            }),
        )
        .expect("well-formed foreign notification");
        let NativeEventDisposition::OtherThread { thread_id, pending } = disposition else {
            panic!("other-thread notification must be handed to the lineage router");
        };
        assert_eq!(thread_id, "foreign-thread");
        assert_eq!(pending.correlation.thread_id.as_deref(), Some("foreign-thread"));
        assert_eq!(
            pending.native.unwrap().message["params"]["threadId"],
            "foreign-thread"
        );
    }

    #[test]
    fn foreign_thread_server_request_is_handed_to_the_lineage_router() {
        let raw = json!({
            "id": "request-1",
            "method": "future/native/request",
            "params": { "threadId": "foreign-thread" }
        });
        let result = pending_event_from_app_server(
            &context(),
            &AppServerEvent::ServerRequest(RpcServerRequest {
                id: json!("request-1"),
                method: "future/native/request".to_string(),
                params: raw.get("params").cloned(),
                raw,
            }),
        );
        assert!(matches!(
            result,
            Ok(NativeEventDisposition::OtherThread { ref thread_id, .. })
                if thread_id == "foreign-thread"
        ));
    }

    #[test]
    fn malformed_foreign_thread_correlation_is_rejected_before_filtering() {
        let raw = json!({
            "method": "item/completed",
            "params": {
                "threadId": 7,
                "item": { "type": "agentMessage", "id": "item-1" }
            }
        });
        let result = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Notification(RpcNotification {
                method: "item/completed".to_string(),
                params: raw.get("params").cloned(),
                raw,
            }),
        );
        assert!(result.is_err());
    }

    #[test]
    fn disconnect_is_namespaced_cutex_state_not_a_native_item() {
        let disposition = pending_event_from_app_server(
            &context(),
            &AppServerEvent::Disconnected {
                reason: "socket closed".to_string(),
            },
        )
        .expect("convert disconnect");
        let NativeEventDisposition::Publish(pending) = disposition else {
            panic!("disconnect must publish");
        };
        assert_eq!(pending.source, EventSource::Cutex);
        assert!(pending.native.is_none());
        assert_eq!(pending.cutex.unwrap().method, "cutex/runtime/disconnected");
    }
}
