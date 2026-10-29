use std::sync::OnceLock;

use serde_json::Value;

const CUTEX_METHOD_SCHEMA: &str = include_str!("schema/cutex-methods-v2.json");

/// Digest of the embedded method schema, computed from the compiled bytes.
pub fn cutex_method_schema_sha256() -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(CUTEX_METHOD_SCHEMA.as_bytes()))
}

static CUTEX_EVENT_VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
static CUTEX_REQUEST_VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
static CUTEX_RESPONSE_VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();

pub fn validate_cutex_event_message(message: &Value) -> Result<(), String> {
    validate_with(
        CUTEX_EVENT_VALIDATOR.get_or_init(|| validator_for_definition("eventMessage")),
        message,
        "cutex event",
    )?;
    validate_semantic_event_contract(message)
}

pub fn validate_cutex_request(request: &Value) -> Result<(), String> {
    validate_with(
        CUTEX_REQUEST_VALIDATOR.get_or_init(|| validator_for_definition("request")),
        request,
        "cutex request",
    )
}

pub fn validate_cutex_response(response: &Value) -> Result<(), String> {
    validate_with(
        CUTEX_RESPONSE_VALIDATOR.get_or_init(|| validator_for_definition("response")),
        response,
        "cutex response",
    )
}

fn validator_for_definition(definition: &str) -> jsonschema::Validator {
    let mut schema: Value = serde_json::from_str(CUTEX_METHOD_SCHEMA)
        .expect("embedded management v2 cutex method schema must be valid JSON");
    schema
        .as_object_mut()
        .expect("management v2 cutex method schema root must be an object")
        .insert(
            "$ref".to_string(),
            Value::String(format!("#/$defs/{definition}")),
        );
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&schema)
        .expect("embedded management v2 cutex method schema must compile")
}

fn validate_with(
    validator: &jsonschema::Validator,
    value: &Value,
    label: &str,
) -> Result<(), String> {
    validator
        .validate(value)
        .map_err(|error| format!("{label} does not match management v2: {error}"))
}

fn validate_semantic_event_contract(message: &Value) -> Result<(), String> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(params) = message.get("params") else {
        return Ok(());
    };
    let result = match method {
        "cutex/jobService/terminalObserved" => {
            let fact: crate::agent_bus::job_completion::CompletionV2 =
                serde_json::from_value(params.clone())
                    .map_err(|error| format!("invalid Job terminal fact: {error}"))?;
            fact.validate().map_err(|error| error.to_string())
        }
        _ => Ok(()),
    };
    result.map_err(|error| format!("cutex event does not match management v2: {error}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn delegation_control_receipt_and_invalidation_events_match_the_wire_schema() {
        let mut state=crate::asks::store::AskState::default();
        let result=crate::asks::operations::execute(&mut state,"owner","project.pause","wire-1",
            &json!({"expectedEpoch":0}),chrono::Utc::now(),|_|Ok((json!({"changed":true}),crate::asks::operations::EffectState::Committed))).unwrap();
        validator_for_definition("delegationOperationReceipt").validate(&result["receipt"]).unwrap();
        for method in ["cutex/delegation/routingApprovalChanged","cutex/delegation/lineageChanged","cutex/delegation/continuationChanged"] {
            let mut event=json!({"method":method,"params":{"projectId":"p","resourceId":"ask-a","revision":2}});
            validate_cutex_event_message(&event).unwrap();
            event["params"]["hiddenParentId"]=json!("private");
            assert!(validate_cutex_event_message(&event).is_err());
        }
        validate_cutex_event_message(&json!({"method":"cutex/project/runControlChanged","params":{"projectId":"p","epoch":1}})).unwrap();
    }

    #[test]
    fn owner_join_is_strict_and_additive_for_direct_and_queue_requests() {
        let join = json!({"askId": "ask-1", "attempt": 1, "expectedTurnId": "turn-1", "runtimeGeneration": 7});
        for (method, mut params) in [
            ("cutex/userInput/submit", json!({"clientUserMessageId":"m", "origin":{"kind":"other","clientId":"desktop"},"strategy":"auto","input":[{"type":"text","text":"owner"}]})),
            ("cutex/userInput/queue/steer", json!({"queueId":"q", "expectedRevision":1, "ifIdle":"fail"})),
        ] {
            params["joinDelegation"] = join.clone();
            validate_cutex_request(&json!({"requestId":"join", "method":method, "params":params})).unwrap();
            for invalid in [json!(null), json!({"askId":"ask-1"}), json!({"askId":"ask-1","attempt":0,"expectedTurnId":"turn-1","runtimeGeneration":7})] {
                params["joinDelegation"] = invalid;
                assert!(validate_cutex_request(&json!({"requestId":"join", "method":method, "params":params})).is_err());
            }
        }
    }

    #[test]
    fn runtime_event_validation_is_exact() {
        validate_cutex_event_message(&json!({
            "method": "cutex/runtime/online",
            "params": {
                "runtimeGeneration": 2,
                "backend": "host",
                "status": "online",
                "runtimeAgentId": null,
                "occurredAt": "2026-07-13T00:00:00Z"
            }
        }))
        .expect("valid runtime event");
        validate_cutex_request(&json!({
            "requestId": "runtime-online-1",
            "method": "cutex/runtime/online",
            "params": {
                "expectedRuntimeGeneration": 2,
                "openVisibleTerminal": false,
                "launchProfile": "beta"
            }
        }))
        .expect("valid online lifecycle controls");
        assert!(validate_cutex_request(&json!({
            "requestId": "runtime-online-empty-profile",
            "method": "cutex/runtime/online",
            "params": {
                "expectedRuntimeGeneration": 2,
                "launchProfile": ""
            }
        }))
        .is_err());
        assert!(validate_cutex_request(&json!({
            "requestId": "runtime-online-2",
            "method": "cutex/runtime/online",
            "params": {
                "expectedRuntimeGeneration": 2,
                "force": true
            }
        }))
        .is_err());
        assert!(validate_cutex_event_message(&json!({
            "method": "cutex/runtime/online",
            "params": {
                "runtimeGeneration": 2,
                "backend": "host",
                "status": "not-a-status",
                "occurredAt": "2026-07-13T00:00:00Z"
            }
        }))
        .is_err());
    }

    #[test]
    fn user_input_blob_references_are_event_only() {
        let blob = json!({
            "type":"cutexBlob", "blobId":"a".repeat(64), "sha256":"a".repeat(64),
            "mime":"image/png", "size":65537,
            "url":format!("cutex-blob:cutex.a/{}", "a".repeat(64))
        });
        let input = json!([{"type":"image", "url":blob}]);
        validate_cutex_event_message(&json!({
            "method":"cutex/userInput/routeAccepted",
            "params":{
                "managementRequestId":"request-1", "clientUserMessageId":"message-1",
                "origin":{"kind":"other", "clientId":"desktop-test"}, "input":input,
                "strategy":"auto", "state":"routed_to_cutex", "acceptedAt":"2026-10-25T00:00:00Z"
            }
        })).expect("durable events may refer to externalized media");
        validate_cutex_request(&json!({
            "requestId":"request-1", "method":"cutex/userInput/submit",
            "params":{
                "clientUserMessageId":"message-1", "origin":{"kind":"other", "clientId":"desktop-test"},
                "strategy":"auto", "input":[{"type":"image", "url":"data:image/png;base64,AA=="}]
            }
        })).expect("clients may submit image URLs as strings");
        assert!(validate_cutex_request(&json!({
            "requestId":"request-1", "method":"cutex/userInput/submit",
            "params":{
                "clientUserMessageId":"message-1", "origin":{"kind":"other", "clientId":"desktop-test"},
                "strategy":"auto", "input":input
            }
        })).is_err(), "clients must still submit image URLs as strings");
    }

    #[test]
    fn managed_agent_bus_events_have_strict_additive_shapes() {
        for event in [
            json!({
                "method": "cutex/agentManagement/actionCompleted",
                "params": {
                    "schema": "cutex/agent-management-receipt/v1",
                    "action_id": "create-1",
                    "request_sha256": "a".repeat(64),
                    "operation": "create",
                    "project_id": "cutex-stack-main",
                    "completed_at": "2026-08-27T00:00:00Z",
                    "result": { "kind": "created", "agent": {} }
                }
            }),
            json!({
                "method": "cutex/agentManagement/actionFailed",
                "params": {
                    "schema": "cutex/agent-management-failure/v1",
                    "event_id": "agent-management:create-1:failure",
                    "action_id": "create-1",
                    "project_id": "cutex-stack-main",
                    "operation": "restart",
                    "code": "owner_action_required",
                    "detail": "external outcome unknown",
                    "routing_status": "routable",
                    "route_to_director_session": "cutex.director",
                    "target_cutex_session_id": "cutex.worker",
                    "created_at": "2026-08-27T00:00:00Z"
                }
            }),
            json!({
                "method": "cutex/agentManagement/actionPhaseTransitionCommitted",
                "params": {
                    "schema": "cutex/agent-management-phase-transition/v1",
                    "phase_event_id": "agent-management:rotate-1:phase:14",
                    "action_id": "rotate-1",
                    "project_id": "cutex-stack-main",
                    "operation": "director_rotate",
                    "phase": "successor_ready",
                    "phase_sequence": 14,
                    "committed_at": "2026-08-27T00:00:00Z",
                    "primary_presentation_target_cutex_session_id": "cutex.director-new",
                    "primary_presentation_target_metadata": {
                        "displayName": "New Director",
                        "cutexSessionId": "cutex.director-new",
                        "profile": "orchid",
                        "model": "gpt-5.6-sol",
                        "reasoning": "high",
                        "role": null,
                        "runtimeBackend": "host"
                    },
                    "predecessor_cutex_session_id": "cutex.director-old",
                    "predecessor_metadata": null,
                    "successor_cutex_session_id": "cutex.director-new",
                    "successor_metadata": null,
                    "replace_policy": null,
                    "rotation_mode": "close_predecessor_then_create_with_message",
                    "authority_epoch": 8
                }
            }),
            json!({
                "method": "cutex/agentBus/messageSent",
                "params": {
                    "messageId": "message-1",
                    "fromCutexSessionId": "cutex.director",
                    "toCutexSessionId": "cutex.worker",
                    "fromRuntimeAgentId": "runtime-director",
                    "toRuntimeAgentId": "runtime-worker",
                    "deliveryMode": "after_turn",
                    "content": "follow-up",
                    "sentAt": "2026-08-27T00:00:00Z"
                }
            }),
        ] {
            validate_cutex_event_message(&event).expect("valid additive integration event");
        }

        assert!(validate_cutex_event_message(&json!({
            "method": "cutex/agentBus/messageSent",
            "params": {
                "messageId": "message-1",
                "fromCutexSessionId": "cutex.director",
                "toCutexSessionId": "cutex.worker",
                "deliveryMode": "after_turn",
                "content": "follow-up",
                "sentAt": "2026-08-27T00:00:00Z",
                "modelVisible": true
            }
        }))
        .is_err());
        // r219: Task Service events have no typed contract any more; persisted r218
        // history still validates as generic cutex events.
        validate_cutex_event_message(&json!({
            "method": "cutex/taskService/assignmentCommitted",
            "params": { "schema": "cutex/task-service-receipt/v2", "action_id": "assign-1" }
        }))
        .expect("legacy Task Service event stays a generic event");
    }

    #[test]
    fn runtime_response_accepts_one_launch_profile_receipt() {
        validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "runtime-online-1",
            "cutexSessionId": "session-1",
            "cutex": {
                "method": "cutex/runtime/online",
                "result": {
                    "runtimeGeneration": 3,
                    "status": "online",
                    "launchProfile": {
                        "requested": "profile-id",
                        "selected": "beta",
                        "effective": "beta",
                        "source": "one_launch_override",
                        "applicationScope": "runtime_and_tui",
                        "persisted": false
                    }
                }
            }
        }))
        .expect("valid one-launch receipt");

        assert!(validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "runtime-online-2",
            "cutexSessionId": "session-1",
            "cutex": {
                "method": "cutex/runtime/online",
                "result": {
                    "runtimeGeneration": 3,
                    "status": "online",
                    "launchProfile": {
                        "requested": "beta",
                        "selected": "beta",
                        "effective": "beta",
                        "source": "one_launch_override",
                        "applicationScope": "runtime",
                        "persisted": true
                    }
                }
            }
        }))
        .is_err());
    }

    #[test]
    fn runtime_response_accepts_legacy_tui_unknown_profile_provenance() {
        validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "runtime-online-legacy-1",
            "cutexSessionId": "session-1",
            "cutex": {
                "method": "cutex/runtime/online",
                "result": {
                    "runtimeGeneration": 3,
                    "status": "online",
                    "launchProfile": {
                        "requested": "legacy-profile",
                        "selected": "legacy-profile",
                        "effective": "legacy-profile",
                        "source": "unknown",
                        "applicationScope": "tui",
                        "persisted": false
                    }
                }
            }
        }))
        .expect("valid legacy TUI receipt");
    }

    #[test]
    fn archive_contract_accepts_exact_optional_reason_and_lifecycle_receipts() {
        for params in [
            json!({
                "expectedRevision": 4,
                "expectedRuntimeGeneration": 7
            }),
            json!({
                "expectedRevision": 4,
                "expectedRuntimeGeneration": 7,
                "reason": "owner_cleanup"
            }),
        ] {
            validate_cutex_request(&json!({
                "requestId": "retire-1",
                "method": "cutex/session/retire",
                "params": params
            }))
            .expect("valid retire request");
        }

        for params in [
            json!({
                "expectedRevision": 4,
                "expectedRuntimeGeneration": 7,
                "reason": false
            }),
            json!({
                "expectedRevision": 4,
                "expectedRuntimeGeneration": 7,
                "unknown": true
            }),
        ] {
            assert!(validate_cutex_request(&json!({
                "requestId": "retire-invalid",
                "method": "cutex/session/retire",
                "params": params
            }))
            .is_err());
        }

        validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "retire-1",
            "cutexSessionId": "cutex-session-1",
            "cutex": {
                "method": "cutex/session/retire",
                "result": {
                    "cutexSessionId": "cutex-session-1",
                    "revision": 5,
                    "lifecycle": "retired",
                    "runtimeGeneration": 7,
                    "status": "offline",
                    "retiredAt": "2026-08-10T00:01:00Z"
                }
            }
        }))
        .expect("valid retire response");
        validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "restore-1",
            "cutexSessionId": "cutex-session-1",
            "cutex": {
                "method": "cutex/session/restore",
                "result": {
                    "cutexSessionId": "cutex-session-1",
                    "revision": 6,
                    "lifecycle": "active",
                    "runtimeGeneration": 7,
                    "status": "offline",
                    "retiredAt": null
                }
            }
        }))
        .expect("valid restore response");
        assert!(validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "retire-state-alias",
            "cutexSessionId": "cutex-session-1",
            "cutex": {
                "method": "cutex/session/retire",
                "result": {
                    "cutexSessionId": "cutex-session-1",
                    "revision": 5,
                    "state": "retired",
                    "runtimeGeneration": 7,
                    "status": "offline",
                    "retiredAt": "2026-08-10T00:01:00Z"
                }
            }
        }))
        .is_err());
        assert!(validate_cutex_response(&json!({
            "contractVersion": 2,
            "requestId": "restore-wrong-lifecycle",
            "cutexSessionId": "cutex-session-1",
            "cutex": {
                "method": "cutex/session/restore",
                "result": {
                    "cutexSessionId": "cutex-session-1",
                    "revision": 6,
                    "lifecycle": "retired",
                    "runtimeGeneration": 7,
                    "status": "offline",
                    "retiredAt": "2026-08-10T00:01:00Z"
                }
            }
        }))
        .is_err());

        validate_cutex_event_message(&json!({
            "method": "cutex/session/retired",
            "params": {
                "cutexSessionId": "cutex-session-1",
                "sessionRevision": 5,
                "runtimeGeneration": 7,
                "retiredAt": "2026-08-10T00:01:00Z",
                "reason": "owner_cleanup"
            }
        }))
        .expect("retire event with reason");
        validate_cutex_event_message(&json!({
            "method": "cutex/session/retired",
            "params": {
                "cutexSessionId": "cutex-session-1",
                "sessionRevision": 5,
                "runtimeGeneration": 7,
                "retiredAt": "2026-08-10T00:01:00Z"
            }
        }))
        .expect("retire event without reason");
        validate_cutex_event_message(&json!({
            "method": "cutex/session/restored",
            "params": {
                "cutexSessionId": "cutex-session-1",
                "sessionRevision": 6,
                "runtimeGeneration": 7,
                "restoredAt": "2026-08-10T00:02:00Z"
            }
        }))
        .expect("restore event");
    }
}
    #[test]
    fn nullable_badge_tint_matches_request_and_resource_contracts() {
        validate_cutex_request(&serde_json::json!({"requestId":"badge-null","method":"cutex/session/metadata/update","params":{"expectedRevision":1,"badge":{"text":"A","tint":null}}})).unwrap();
        crate::management::v2::schema_export::assert_valid("sessions","badge",&serde_json::json!({"text":"A","tint":null}));
    }
