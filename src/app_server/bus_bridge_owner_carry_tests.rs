//! 057 builder keeps each source's existing model text and display provenance.
use super::*;
use serde_json::json;

const OWNER: &str = "cutex.11111111-1111-4111-8111-111111111111";
const SENDER: &str = "cutex.22222222-2222-4222-8222-222222222222";

fn binding() -> crate::launch::stock::ExternalInputBinding {
    crate::launch::stock::ExternalInputBinding {
        version: 1,
        owner_id: OWNER.into(),
        thread_id: "native-owner-thread".into(),
        runtime_generation: 7,
        canonical_byte_limit: Default::default(),
    }
}

#[test]
fn carries_keep_agent_sender_metadata_and_authenticated_source() {
    let message: AgentBusMessage = serde_json::from_value(json!({
        "id": "agent-carry", "from": "runtime-from", "to": "runtime-to",
        "fromCutexSessionId": SENDER, "toCutexSessionId": OWNER,
        "content": "I am the owner", "createdAtEpochSecs": 1,
        "deliveryMode": "after_turn", "triggerTurn": true
    }))
    .unwrap();
    let meta = crate::management::v2::agent_bus_state::AgentBusMessageMeta {
        sender: Some(crate::management::v2::agent_bus_state::AgentParty {
            id: SENDER.into(),
            name: Some("Original sender".into()),
            runtime: Some("claude".into()),
        }),
        ..Default::default()
    };
    let e = owner_carry_envelope(&message, &binding(), Some(&meta), Some("fallback")).unwrap();
    assert_eq!(e.message.source.kind, SourceKind::Agent);
    assert_eq!(e.message.source.id, SENDER);
    assert!(e.message.text.contains("Original sender"));
    assert_eq!(e.view.as_ref().unwrap().schema, "cutex.agent-message.v1");
    assert_eq!(e.semantic_sha256, e.digest());
    e.validate().unwrap();
}

#[test]
fn stopped_claude_task_carries_existing_service_text_and_task_view() {
    use crate::agent_bus::claude_task;
    let notice: claude_task::ClaudeTaskNotice = serde_json::from_value(json!({
        "noticeId": "notice-stopped", "taskId": "background-1", "taskType": "local_agent",
        "status": "stopped", "description": "Review the code", "reason": "owner stopped"
    }))
    .unwrap();
    let request = claude_task::send_request(OWNER, &notice).unwrap();
    let message: AgentBusMessage = serde_json::from_value(json!({
        "id": claude_task::message_id(OWNER, &notice.notice_id), "from": claude_task::SOURCE_ID,
        "to": "runtime-to", "toCutexSessionId": OWNER, "content": request.content,
        "createdAtEpochSecs": 1, "deliveryMode": "after_turn", "triggerTurn": true,
        "controlType": claude_task::CONTROL_TYPE, "controlPayload": notice,
        "externalMessageId": request.external_message_id
    }))
    .unwrap();
    let e = owner_carry_envelope(&message, &binding(), None, None).unwrap();
    assert_eq!(e.message.source.kind, SourceKind::Service);
    assert_eq!(e.message.source.id, claude_task::SOURCE_ID);
    assert_eq!(e.message.event_type, claude_task::EVENT_TYPE);
    assert_eq!(e.message.text, claude_task::model_text(&message).unwrap().1);
    assert_eq!(e.view.as_ref().unwrap().schema, claude_task::VIEW_SCHEMA);
    assert_eq!(e.message.delivery, Delivery::AfterTurn);
    assert_eq!(e.semantic_sha256, e.digest());
    e.validate().unwrap();
    let mut forged = message.clone();
    forged.from_cutex_session_id = Some(SENDER.into());
    assert!(owner_carry_envelope(&forged, &binding(), None, None).is_err());
}
