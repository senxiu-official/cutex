//! Claude r8 native external-input routing. The native envelope is frozen before
//! sending; pending acceptance and context persistence remain separate facts.
//! Legacy ids are reconciled before migration and never resubmitted under a new
//! identity. Asks retain their legacy metadata-bearing submit/status surface.
use anyhow::{ensure, Context};
use serde_json::{json, Value};

use cutex::app_server::client::{AppServerClientError, AppServerHandle};
use cutex::app_server::external_input::{
    Delivery, DeliveryState, Envelope, Message, Response, Source, Status,
};
use cutex::launch::stock::ExternalInputBinding;

pub(crate) fn offers_native(capabilities: &Value) -> bool {
    capabilities["externalInputVersion"] == 1
        && capabilities["externalInputVersions"]
            .as_array()
            .is_some_and(|versions| versions.contains(&json!(2)))
        && capabilities["externalInputDeliveries"]
            .as_array()
            .is_some_and(|deliveries| {
                ["after_turn", "passive"]
                    .iter()
                    .all(|mode| deliveries.iter().any(|value| value.as_str() == Some(*mode)))
            })
}

pub(crate) fn offers_controls(capabilities: &Value) -> bool {
    offers_native(capabilities)
        && capabilities["externalInputControlVersion"]
            .as_u64()
            .is_some_and(|version| version >= 1)
}

pub(crate) fn offers_soon(capabilities: &Value) -> bool {
    offers_native(capabilities) && capabilities["externalInputDeliveries"].as_array()
        .is_some_and(|modes| modes.iter().any(|mode| mode == "soon"))
}

pub(crate) fn binding_params(owner: &str, thread: &str, generation: u64) -> Value {
    json!({"version": 1, "ownerId": owner, "threadId": thread, "runtimeGeneration": generation})
}

pub(crate) fn envelope(
    owner: &str,
    thread: &str,
    generation: u64,
    legacy: &Value,
) -> anyhow::Result<Envelope> {
    ensure!(
        legacy["threadId"] == thread
            && (legacy["eventType"] == "agent_message"
                || (legacy["eventType"] == cutex::agent_bus::peer_tell::EVENT_TYPE
                    && legacy["source"]["kind"] == "service"
                    && legacy["source"]["id"] == cutex::agent_bus::peer_tell::SOURCE_ID
                    && legacy["metadata"]["deliveryMode"] == "passive")
                || (legacy["eventType"] == "alarm"
                    && legacy["source"]["kind"] == "service"
                    && legacy["source"]["id"] == cutex::agent_bus::alarms::ALARM_SYSTEM_SENDER
                    && legacy["metadata"]["deliveryMode"] == "after-turn")),
        "not a Claude agent-message, peer-message or alarm projection"
    );
    let mut value = Envelope {
        version: 2,
        owner_id: owner.into(),
        thread_id: thread.into(),
        runtime_generation: generation,
        message: Message {
            id: legacy["messageId"]
                .as_str()
                .context("message id missing")?
                .into(),
            source: serde_json::from_value::<Source>(legacy["source"].clone())?,
            event_type: legacy["eventType"]
                .as_str()
                .context("message type missing")?
                .into(),
            delivery: match legacy
                .pointer("/metadata/deliveryMode")
                .and_then(Value::as_str)
            {
                Some("passive") => Delivery::Passive,
                Some("after-turn") => Delivery::AfterTurn,
                Some("soon") => Delivery::Soon,
                _ => anyhow::bail!("unsupported Claude agent-message delivery"),
            },
            text: legacy["text"]
                .as_str()
                .context("message text missing")?
                .into(),
        },
        view: Some(serde_json::from_value(
            legacy
                .get("view")
                .context("agent message view missing")?
                .clone(),
        )?),
        semantic_sha256: String::new(),
    };
    value.semantic_sha256 = value.digest();
    value.validate()?;
    Ok(value)
}

fn validate_status(raw: Value, envelope: &Envelope, generation: u64) -> anyhow::Result<Status> {
    envelope.validate()?;
    let response: Response = serde_json::from_value(raw)?;
    response.validate(
        &ExternalInputBinding {
            version: 1,
            owner_id: envelope.owner_id.clone(),
            thread_id: envelope.thread_id.clone(),
            runtime_generation: generation,
            canonical_byte_limit: Default::default(),
        },
        &[envelope.key()],
    )?;
    response
        .statuses
        .into_iter()
        .next()
        .context("external input status absent")
}

pub(crate) fn status(
    handle: &AppServerHandle,
    envelope: &Envelope,
    generation: u64,
) -> anyhow::Result<Status> {
    let mut params = binding_params(&envelope.owner_id, &envelope.thread_id, generation);
    params["messages"] = json!([envelope.key()]);
    validate_status(
        handle.request("thread/externalInput/status", params)?,
        envelope,
        generation,
    )
}

pub(crate) fn persisted_turn(status: &Status) -> Option<&str> {
    (status.delivery_state == DeliveryState::ContextPersisted)
        .then(|| {
            status
                .receipt
                .as_ref()
                .map(|receipt| receipt.turn_id.as_str())
        })
        .flatten()
}

/// Only the adapter's explicit not-found response permits choosing a new route.
/// Timeout, disconnect, and all other failures are never interpreted as absence.
pub(crate) fn legacy_receipt(
    result: Result<Value, AppServerClientError>,
) -> anyhow::Result<Option<Value>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(AppServerClientError::Rpc(error)) if error.code == -32004 => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn submit(
    handle: &AppServerHandle,
    envelope: &Envelope,
    generation: u64,
) -> anyhow::Result<Value> {
    submit_with(
        |method, params| handle.request(method, params),
        envelope,
        generation,
    )
}

fn submit_with(
    mut request: impl FnMut(&str, Value) -> Result<Value, AppServerClientError>,
    envelope: &Envelope,
    generation: u64,
) -> anyhow::Result<Value> {
    envelope.validate()?;
    let mut params = serde_json::to_value(envelope)?;
    params["runtimeGeneration"] = json!(generation);
    let status = validate_status(
        request("thread/externalInput/submit", params)?,
        envelope,
        generation,
    )?;
    ensure!(
        matches!(
            status.delivery_state,
            DeliveryState::Pending | DeliveryState::ContextPersisted
        ),
        "native input was not accepted: {:?}; preserve its frozen identity for reconciliation",
        status.delivery_state
    );
    // Native pending is not a context receipt. Read the actual adapter durable
    // inbox receipt, keeping the repository's established acceptance semantics.
    let mut local = request(
        "claude/input/status",
        json!({"threadId": envelope.thread_id, "messageId": envelope.message.id}),
    )?;
    ensure!(
        local["threadId"] == envelope.thread_id
            && local["messageId"] == envelope.message.id
            && local["durable"] == true
            && local["receiptScope"] == "local-inbox",
        "invalid Claude local inbox receipt"
    );
    local["nativeExternalInput"] = serde_json::to_value(envelope)?;
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cutex::app_server::external_input::Receipt;
    use cutex::app_server::protocol::RpcError;

    fn sample() -> Envelope {
        envelope("owner", "thread", 7, &json!({
            "threadId": "thread", "messageId": "message", "source": {"kind": "service", "id": "service-id"},
            "eventType": "agent_message", "text": "hello\n世界", "metadata": {"deliveryMode": "after-turn"},
            "view": {"schema": "test.v1", "data": {"a": "🦀\n", "z": [true, null, 7]}}
        })).unwrap()
    }

    fn response(envelope: &Envelope) -> Value {
        json!({"version": 1, "ownerId": envelope.owner_id, "threadId": envelope.thread_id, "runtimeGeneration": 7,
            "statuses": [{"messageId": envelope.message.id, "semanticSha256": envelope.semantic_sha256,
                "deliveryState": "pending", "receipt": null,
                "processing": {"state": "pending", "attemptId": null, "reason": null}}]})
    }

    #[test]
    fn alarm_native_submit_keeps_system_source_and_pending_is_only_local_acceptance() {
        let payload = json!({"threadId":"thread","messageId":"alarm-test",
            "source":{"kind":"service","id":"Cutex Alarm"},"eventType":"alarm",
            "text":"Message Type: ALARM\nPayload:\nreminder", "metadata":{"deliveryMode":"after-turn"},
            "view":{"schema":cutex::agent_bus::alarms::ALARM_VIEW_SCHEMA,"data":{"sourceName":"Cutex Alarm"}}});
        let envelope = envelope("owner", "thread", 7, &payload).unwrap();
        let receipt = submit_with(|method, params| {
            Ok(if method == "thread/externalInput/submit" {
                assert_eq!(params["message"]["source"], json!({"kind":"service","id":"Cutex Alarm"}));
                assert_eq!(params["message"]["type"], "alarm");
                assert_eq!(params["message"]["delivery"], "after_turn");
                response(&envelope)
            } else {
                assert_eq!(method, "claude/input/status");
                assert_eq!(params["messageId"], "alarm-test");
                json!({"threadId":"thread","messageId":"alarm-test","durable":true,"receiptScope":"local-inbox","state":"queued"})
            })
        }, &envelope, 7).unwrap();
        assert_eq!(receipt["state"], "queued");
        assert!(receipt.get("receipt").is_none());
        let mut forged = payload;
        forged["source"]["kind"] = json!("agent");
        assert!(super::envelope("owner", "thread", 7, &forged).is_err());
    }

    #[test]
    fn native_capability_is_explicit_and_reference_digest_matches_python() {
        assert!(!offers_native(
            &json!({"input": {"version": 1, "views": true}})
        ));
        let cap = json!({"externalInputVersion": 1, "externalInputVersions": [1,2],
            "externalInputDeliveries": ["after_turn", "passive"], "externalInputControlVersion": 1});
        assert!(offers_native(&cap) && offers_controls(&cap));
        let mut envelope = sample();
        envelope.message.event_type = "opaque.v1".into();
        envelope.semantic_sha256 = envelope.digest();
        assert_eq!(
            envelope.semantic_sha256,
            "871d2fc606fe6d2230e1c3cfc63c2f568f2105a214e6bf2668bf6ecfa2e0f05c"
        );
        envelope.runtime_generation += 1;
        assert_eq!(envelope.digest(), envelope.semantic_sha256);
    }

    #[test]
    fn pending_is_not_delivered_and_receipt_binding_is_verified() {
        let envelope = sample();
        let pending = response(&envelope);
        assert!(persisted_turn(&validate_status(pending.clone(), &envelope, 7).unwrap()).is_none());
        assert!(validate_status(pending.clone(), &envelope, 8).is_err());
        let mut wrong = pending.clone();
        wrong["statuses"][0]["semanticSha256"] = json!("0".repeat(64));
        assert!(validate_status(wrong, &envelope, 7).is_err());
        let mut receipt = Receipt {
            schema: "codex.external-input-receipt.v1".into(),
            receipt_id: String::new(),
            owner_id: "owner".into(),
            thread_id: "thread".into(),
            message_id: "message".into(),
            semantic_sha256: envelope.semantic_sha256.clone(),
            response_item_id: "message".into(),
            turn_id: "turn".into(),
            ordinal: 1,
        };
        receipt.receipt_id = receipt.digest_id();
        let mut persisted = pending;
        persisted["statuses"][0]["deliveryState"] = json!("context_persisted");
        persisted["statuses"][0]["receipt"] = json!(receipt);
        assert_eq!(
            persisted_turn(&validate_status(persisted.clone(), &envelope, 7).unwrap()),
            Some("turn")
        );
        persisted["statuses"][0]["receipt"]["ownerId"] = json!("wrong");
        assert!(validate_status(persisted, &envelope, 7).is_err());
    }

    #[test]
    fn old_accepted_ids_and_transport_uncertainty_never_become_absence() {
        let old = json!({"threadId": "thread", "messageId": "message", "durable": true, "receiptScope": "local-inbox"});
        assert_eq!(legacy_receipt(Ok(old.clone())).unwrap(), Some(old));
        assert!(legacy_receipt(Err(AppServerClientError::Rpc(RpcError {
            code: -32004,
            message: "input not found".into(),
            data: None
        })))
        .unwrap()
        .is_none());
        assert!(legacy_receipt(Err(AppServerClientError::Timeout {
            method: "claude/input/status".into()
        }))
        .is_err());
    }

    #[test]
    fn accepted_pending_uses_real_local_receipt_without_context_claim() {
        let envelope = sample();
        let mut called = Vec::new();
        let local = submit_with(
            |method, _| {
                called.push(method.to_string());
                Ok(if method == "thread/externalInput/submit" {
                    response(&envelope)
                } else {
                    json!({"threadId": "thread", "messageId": "message", "durable": true,
                    "receiptScope": "local-inbox", "state": "queued", "turnId": "local-turn"})
                })
            },
            &envelope,
            7,
        )
        .unwrap();
        assert_eq!(
            called,
            ["thread/externalInput/submit", "claude/input/status"]
        );
        assert_eq!(local["state"], "queued");
        assert_eq!(
            local["nativeExternalInput"]["semanticSha256"],
            envelope.semantic_sha256
        );
        assert!(local.get("receipt").is_none());
    }
}
