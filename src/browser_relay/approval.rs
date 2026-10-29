//! Mechanical browser approval. Request correlation is trusted relay metadata,
//! never model-provided session/turn/host authority.
use super::{auth::AgentContext, commands::AgentCommand, model::*};
use crate::approvals::{ApprovalKind, ApprovalRequest, ApprovalState, ApprovalStore, GateOutcome};
use crate::config::browser::AgentOpenApproval;
use serde_json::json;

pub struct Context<'a> {
    pub agent: &'a AgentContext,
    pub agent_name: &'a str,
    pub host: &'a str,
    pub device: &'a str,
    pub generation: &'a str,
    /// The agent's session runs with full access (完全访问; see
    /// [`super::owner::full_access`]): its own `browser_open` needs no
    /// approval. `browser_claim` (an owner tab) is unaffected.
    pub full_access: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Proceed,
    Approved {
        approval_id: String,
    },
    Refused {
        code: &'static str,
        approval_id: String,
    },
}

pub(crate) fn approval_request(context: &Context<'_>, input: &AgentCommand) -> ApprovalRequest {
    let claim = input.tool == "browser_claim";
    let mut intent = json!({"kind":if claim{"browser_claim"}else{"browser_open"},"session":context.agent.session,"runtime":context.agent.runtime,
        "thread":context.agent.thread,"runtimeGeneration":context.agent.generation,"device":context.device,"generation":context.generation,
        "arguments":input.arguments});
    if claim {
        for key in ["arguments", "runtime", "thread", "runtimeGeneration"] {
            intent.as_object_mut().unwrap().remove(key);
        }
    }
    let digest = crate::approvals::request_digest(&intent);
    let mut details = json!({"version":1,"cutexSessionId":context.agent.session,"agentName":context.agent_name,
        "requestId":input.request_id,"deviceId":context.device,"hostId":context.host,"connectionGeneration":context.generation,
        "threadId":context.agent.thread,"turnId":context.agent.turn,"runtimeGeneration":context.agent.generation,"intentDigest":digest});
    if claim {
        details["reason"] = input.arguments["reason"].clone();
        if let Some(hint) = input.arguments["urlHint"].as_str() {
            details["urlHint"] = json!(safe_url(hint));
        }
        details["sharesOwnerLogin"] = json!(true);
    } else {
        details["url"] = json!(safe_url(
            input.arguments["url"].as_str().unwrap_or_default()
        ));
        details["title"] = input.arguments["title"].clone();
    }
    ApprovalRequest {
        kind: if claim { ApprovalKind::BrowserClaim } else { ApprovalKind::BrowserOpen },
        reason: if claim { "browser_owner_tab_selection" } else { "browser_agent_open" }.into(),
        requester_cutex_session_id: context.agent.session.clone(),
        requester_name: Some(context.agent_name.into()),
        project_id: None,
        summary: if claim {
            "Select an owner browser tab (shares its login state)"
        } else {
            "Open a browser tab in the agent login pool"
        }
        .into(),
        details,
        request_sha256: digest,
    }
}

pub fn gate(
    store: &ApprovalStore,
    context: &Context<'_>,
    input: &AgentCommand,
    open: AgentOpenApproval,
) -> Result<Decision> {
    if input.tool != "browser_claim"
        && (input.tool != "browser_open"
            || open == AgentOpenApproval::Off
            || context.full_access)
    {
        return Ok(Decision::Proceed);
    }
    input.validate()?;
    let claim = input.tool == "browser_claim";
    // Claims are a session/device-generation request, not a new prompt for
    // every tool requestId/title/reason. Denials therefore survive retries.
    let request = approval_request(context, input);
    let now = chrono::Utc::now();
    if claim {
        let (record, changed) = store
            .request_browser_claim_with_change(request, now)
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        if changed {
            crate::management::v2::approvals::publish_changed(&record, "requested");
        }
        return Ok(Decision::Refused {
            code: if record.state == ApprovalState::Denied {
                "approval_denied"
            } else {
                "approval_required"
            },
            approval_id: record.approval_id,
        });
    }
    match store
        .gate_browser_open(request, now)
        .map_err(|_| RelayError::new("browser_unavailable"))?
    {
        // Removing deterministic refusal rows must not turn the originally
        // refused call into a deferred command. Consumption needs a fresh RPC.
        GateOutcome::Approved(record) if record.details["requestId"] == input.request_id => Ok(Decision::Refused {
            code: "approval_required",
            approval_id: record.approval_id,
        }),
        GateOutcome::Approved(record) => Ok(Decision::Approved {
            approval_id: record.approval_id,
        }),
        GateOutcome::Pending { record, created } => {
            if created {
                crate::management::v2::approvals::publish_changed(&record, "requested");
            }
            Ok(Decision::Refused {
                code: "approval_required",
                approval_id: record.approval_id,
            })
        }
        GateOutcome::Denied(record) | GateOutcome::DeniedEarlier(record) => Ok(Decision::Refused {
            code: "approval_denied",
            approval_id: record.approval_id,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ApprovalStore, std::path::PathBuf, AgentContext) {
        let dir = std::env::temp_dir().join(format!("browser-approval-{}", uuid::Uuid::new_v4()));
        (
            ApprovalStore::at(dir.join("approvals.json")),
            dir,
            AgentContext {
                session: "cutex.s".into(),
                runtime: "r".into(),
                thread: "t".into(),
                generation: 1,
                turn: "turn".into(),
                credential_identity: "credential-not-stored".into(),
            },
        )
    }
    fn input(id: &str) -> AgentCommand {
        AgentCommand {
            request_id: id.into(),
            tool: "browser_open".into(),
            arguments: json!({"title":"Open page","url":"http://localhost/?token=secret"}),
        }
    }
    #[test]
    fn browser_open_off_ask_denial_one_use_and_exact_intent() {
        let (store, dir, agent) = fixture();
        let context = Context {
            agent: &agent,
            agent_name: "Agent",
            host: "local",
            device: "d",
            generation: "g",
            full_access: false,
        };
        assert_eq!(
            gate(&store, &context, &input("r1"), AgentOpenApproval::Off).unwrap(),
            Decision::Proceed
        );
        let Decision::Refused { approval_id, .. } =
            gate(&store, &context, &input("r1"), AgentOpenApproval::Ask).unwrap()
        else {
            panic!("approval required")
        };
        let record = store.list(true, chrono::Utc::now()).unwrap().pop().unwrap();
        assert!(!record.details.to_string().contains("secret"));
        store
            .decide(&approval_id, true, None, chrono::Utc::now())
            .unwrap();
        assert_eq!(gate(&store, &context, &input("r1"), AgentOpenApproval::Ask).unwrap(),
            Decision::Refused { code: "approval_required", approval_id: approval_id.clone() });
        let mut other = input("r2");
        other.arguments["url"] = json!("http://localhost/?token=different");
        assert!(matches!(
            gate(&store, &context, &other, AgentOpenApproval::Ask).unwrap(),
            Decision::Refused { .. }
        ));
        assert_eq!(
            gate(&store, &context, &input("r2"), AgentOpenApproval::Ask).unwrap(),
            Decision::Approved {
                approval_id: approval_id.clone()
            }
        );
        // Failed queue admission does not burn the approved intent.
        assert_eq!(
            store
                .try_consume_browser_open(
                    &approval_id,
                    &agent.session,
                    chrono::Utc::now(),
                    || Err::<(), _>("busy")
                )
                .unwrap(),
            Some(Err("busy"))
        );
        assert_eq!(
            store
                .get(&approval_id, chrono::Utc::now())
                .unwrap()
                .unwrap()
                .state,
            ApprovalState::Approved
        );
        assert_eq!(
            store
                .try_consume_browser_open(
                    &approval_id,
                    &agent.session,
                    chrono::Utc::now(),
                    || Ok::<_, ()>(())
                )
                .unwrap(),
            Some(Ok(()))
        );
        assert!(matches!(
            gate(&store, &context, &input("r3"), AgentOpenApproval::Ask).unwrap(),
            Decision::Refused { .. }
        ));
        let denied = gate(&store, &context, &other, AgentOpenApproval::Ask).unwrap();
        let Decision::Refused { approval_id, .. } = denied else {
            panic!("pending")
        };
        store
            .decide(&approval_id, false, None, chrono::Utc::now())
            .unwrap();
        assert!(matches!(
            gate(&store, &context, &other, AgentOpenApproval::Ask).unwrap(),
            Decision::Refused {
                code: "approval_denied",
                ..
            }
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn full_access_sessions_open_without_approval_but_claims_still_ask() {
        let (store, dir, agent) = fixture();
        let context = Context {
            agent: &agent,
            agent_name: "Agent",
            host: "local",
            device: "d",
            generation: "g",
            full_access: true,
        };
        assert_eq!(
            gate(&store, &context, &input("r1"), AgentOpenApproval::Ask).unwrap(),
            Decision::Proceed
        );
        assert!(store.list(true, chrono::Utc::now()).unwrap().is_empty());
        let claim = AgentCommand {
            request_id: "c1".into(),
            tool: "browser_claim".into(),
            arguments: json!({"title":"Request control","reason":"Inspect page"}),
        };
        for open in [AgentOpenApproval::Ask, AgentOpenApproval::Off] {
            assert!(matches!(
                gate(&store, &context, &claim, open).unwrap(),
                Decision::Refused { code: "approval_required", .. }
            ));
        }
        let restricted = Context { full_access: false, ..context };
        assert!(matches!(
            gate(&store, &restricted, &input("r2"), AgentOpenApproval::Ask).unwrap(),
            Decision::Refused { code: "approval_required", .. }
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn browser_claim_reuses_approval_and_is_never_consumed_by_agent() {
        let (store, dir, agent) = fixture();
        let context = Context {
            agent: &agent,
            agent_name: "Agent",
            host: "local",
            device: "d",
            generation: "g",
            full_access: false,
        };
        let mut input = AgentCommand {
            request_id: "r1".into(),
            tool: "browser_claim".into(),
            arguments: json!({"title":"Request control","reason":"Inspect page"}),
        };
        let Decision::Refused {
            approval_id: first, ..
        } = gate(&store, &context, &input, AgentOpenApproval::Off).unwrap()
        else {
            panic!("claim required")
        };
        input.request_id = "different-rpc".into();
        input.arguments["reason"] = json!("Different wording");
        let Decision::Refused {
            approval_id: pending,
            ..
        } = gate(&store, &context, &input, AgentOpenApproval::Off).unwrap()
        else {
            panic!()
        };
        assert_eq!(first, pending);
        store
            .decide(&first, true, None, chrono::Utc::now())
            .unwrap();
        assert!(matches!(
            gate(&store, &context, &input, AgentOpenApproval::Off).unwrap(),
            Decision::Refused {
                code: "approval_required",
                ..
            }
        ));
        input.request_id = "r2".into();
        let Decision::Refused {
            approval_id: second,
            ..
        } = gate(&store, &context, &input, AgentOpenApproval::Off).unwrap()
        else {
            panic!("claim required")
        };
        assert_eq!(first, second);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn browser_claim_denial_is_stable_across_rpc_and_runtime_until_expiry() {
        let (store, dir, mut agent) = fixture();
        let input = AgentCommand {
            request_id: "r1".into(),
            tool: "browser_claim".into(),
            arguments: json!({"title":"Control","reason":"Inspect"}),
        };
        let context = Context {
            agent: &agent,
            agent_name: "Agent",
            host: "local",
            device: "d",
            generation: "g",
            full_access: false,
        };
        let Decision::Refused { approval_id, .. } =
            gate(&store, &context, &input, AgentOpenApproval::Off).unwrap()
        else {
            panic!()
        };
        store
            .decide(&approval_id, false, None, chrono::Utc::now())
            .unwrap();
        agent.generation += 1;
        agent.thread = "replacement".into();
        agent.runtime = "r2".into();
        let context = Context {
            agent: &agent,
            agent_name: "Agent",
            host: "local",
            device: "d",
            generation: "g",
            full_access: false,
        };
        let next = AgentCommand {
            request_id: "r2".into(),
            ..input
        };
        assert_eq!(
            gate(&store, &context, &next, AgentOpenApproval::Off).unwrap(),
            Decision::Refused {
                code: "approval_denied",
                approval_id
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
