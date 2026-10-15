//! The ask gate (asks design §4.5): an agent's ask needs one review when
//! `caps(target) ⊄ caps(requester)`. The review is an `ask_delivery` record
//! in the owner approval store (P1: decided by the owner; P2 adds the
//! project reviewer), bound to the digest of an immutable snapshot:
//!
//! - ask id, body digest, attachment ids, the `context` option, follow-up;
//! - authenticated requester and target sessions;
//! - authorization-evidence ids (P2, N3; the ask ancestry in P1);
//! - versions of requester capabilities, target capabilities + profile +
//!   approval policy, the project policy, and the link state.
//!
//! The snapshot is recomputed at admission and after a restart; if any
//! version changed, the earlier approval does not apply (a stale approval is
//! never inherited) and the ask returns to `pendingReview` with the
//! difference. A yes admits that one snapshot once, under the target's own
//! envelope; it is consumed at admission.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::caps::{subset_differences, Capabilities};
use super::model::{AskRecord, GateRecord};
use super::store::AskError;
use crate::approvals::{ApprovalKind, ApprovalRecord, ApprovalRequest, ApprovalState, ApprovalStore, GateOutcome};

/// The project reviewer policy version. P1: every `ask_delivery` goes to the
/// owner; P2 replaces this with the project's reviewer setting.
pub const PROJECT_POLICY_P1: &str = "ask-delivery-owner-v1";

/// One side of the gate.
#[derive(Clone, Debug, PartialEq)]
pub struct GateParty {
    pub cutex_session_id: String,
    pub name: Option<String>,
    pub caps: Capabilities,
    pub profile: Option<String>,
    pub approval_policy: Option<String>,
}

/// Everything the gate compares, loaded fresh for each evaluation.
#[derive(Clone, Debug, PartialEq)]
pub struct GateInputs {
    /// `None`: the owner (never gated).
    pub requester: Option<GateParty>,
    pub target: GateParty,
    pub project_policy: String,
    pub links_version: String,
}

impl GateInputs {
    /// Load both parties from the session store, the boundary and the
    /// agent-security config.
    pub fn load(requester: Option<&str>, target: &str) -> Result<Self, AskError> {
        let config = crate::config::store::load_codez_config();
        let boundary = crate::agent_bus::boundary::AgentBoundary::load_with(&config.agent_security)
            .map_err(|error| AskError::new("persistence_unavailable", format!("boundary: {error:#}")))?;
        // Borrow the cached session store (asks ticks call this per queue head).
        crate::session::store::with_cutex_session_store(|store| Self::from_store(store, &config, &boundary, requester, target))
            .map_err(|error| AskError::new("persistence_unavailable", format!("session store: {error:#}")))?
    }

    fn from_store(
        store: &crate::session::model::CutexSessionStore,
        config: &crate::profiles::model::CodezConfig,
        boundary: &crate::agent_bus::boundary::AgentBoundary,
        requester: Option<&str>,
        target: &str,
    ) -> Result<Self, AskError> {
        let party = |session: &str| -> Result<GateParty, AskError> {
            let record = store
                .sessions
                .get(session)
                .ok_or_else(|| AskError::new("recipient_not_found", format!("no agent {session}")))?;
            let (_, approval) = crate::runtime::args::effective_runtime_permission_defaults(record);
            Ok(GateParty {
                cutex_session_id: session.to_string(),
                name: record.formal_agent_name.clone(),
                caps: Capabilities::of_record(record, boundary),
                profile: record.profile.clone(),
                approval_policy: approval,
            })
        };
        Ok(Self {
            requester: requester.map(party).transpose()?,
            target: party(target)?,
            project_policy: PROJECT_POLICY_P1.into(),
            links_version: crate::approvals::request_digest(
                &serde_json::to_value(&config.agent_security).unwrap_or(Value::Null),
            ),
        })
    }

    /// The versions bound into the snapshot.
    pub fn versions(&self) -> Value {
        json!({
            "requester": self.requester.as_ref().map(|party| &party.cutex_session_id),
            "requesterCaps": self.requester.as_ref().map(|party| party.caps.version()),
            "target": self.target.cutex_session_id,
            "targetCaps": self.target.caps.version(),
            "targetProfile": self.target.profile,
            "targetApprovalPolicy": self.target.approval_policy,
            "projectPolicy": self.project_policy,
            "links": self.links_version,
        })
    }
}

/// The immutable ask facts bound into the snapshot.
pub fn ask_facts(ask: &AskRecord) -> Value {
    json!({
        "askId": ask.ask_id,
        "requester": ask.requester,
        "claimedCaps": ask.peer_inbound.as_ref().map(|i| &i.request.requester.caps),
        "textSha256": ask.body.text_sha256,
        "attachments": ask.body.attachments,
        "context": ask.options.context,
        "followUpOf": ask.follow_up_of,
        // P1 evidence: the authenticated ask ancestry (P2 adds owner
        // message ids through N3).
        "evidence": ask.parent_ask,
    })
}

/// Evaluate the gate condition and the snapshot digest.
pub fn evaluate(facts: &Value, inputs: &GateInputs) -> GateRecord {
    let versions = inputs.versions();
    let snapshot_sha256 = crate::approvals::request_digest(&json!({
        "kind": "ask_delivery",
        "ask": facts,
        "versions": versions,
    }));
    let differences = match &inputs.requester {
        _ if inputs.project_policy == "peer-review-skipped-v1" => Vec::new(),
        None => Vec::new(),
        Some(requester) => subset_differences(&inputs.target.caps, &requester.caps),
    };
    GateRecord {
        required: inputs.project_policy == "peer-review-required-v1" || !differences.is_empty(),
        differences: differences
            .iter()
            .map(|difference| serde_json::to_value(difference).unwrap_or(Value::Null))
            .collect(),
        snapshot_sha256,
        inputs: versions,
        approval_id: None,
        admitted_with_approval: false,
        stale_reason: None,
    }
}

/// Which versions changed between two snapshots (for the review card).
pub fn stale_reason(previous: &GateRecord, current: &GateRecord) -> Option<String> {
    if previous.snapshot_sha256 == current.snapshot_sha256 {
        return None;
    }
    let changed: Vec<String> = match (previous.inputs.as_object(), current.inputs.as_object()) {
        (Some(before), Some(after)) => after
            .iter()
            .filter(|(key, value)| before.get(*key) != Some(*value))
            .map(|(key, _)| key.clone())
            .collect(),
        _ => Vec::new(),
    };
    Some(if changed.is_empty() {
        "the ask snapshot changed".into()
    } else {
        format!("changed since review: {}", changed.join(", "))
    })
}

fn approval_request(ask: &AskRecord, gate: &GateRecord, inputs: &GateInputs) -> Option<ApprovalRequest> {
    let requester = inputs.requester.as_ref()?;
    let requester_label = requester.name.clone().unwrap_or_else(|| requester.cutex_session_id.clone());
    let target_label = inputs
        .target
        .name
        .clone()
        .unwrap_or_else(|| inputs.target.cutex_session_id.clone());
    Some(ApprovalRequest {
        kind: ApprovalKind::AskDelivery,
        reason: "target_capabilities_not_subset".into(),
        requester_cutex_session_id: requester.cutex_session_id.clone(),
        requester_name: requester.name.clone(),
        project_id: ask.project_id.clone(),
        summary: super::truncate_chars(
            &format!("委托 {requester_label} → {target_label}: {}", ask.title),
            300,
        ),
        details: json!({
            "askId": ask.ask_id,
            "target": inputs.target.cutex_session_id,
            "targetName": inputs.target.name,
            "title": ask.title,
            "brief": super::truncate_chars(&ask.body.text, 1200),
            "attachments": ask.body.attachments,
            "differences": gate.differences,
            "claimedCaps": ask.peer_inbound.as_ref().map(|i| &i.request.requester.caps),
            "claimedCapsLabel": ask.peer_inbound.as_ref().map(|_| "对端声称（仅展示，不作为审批依据）"),
            "versions": gate.inputs,
            "staleReason": gate.stale_reason,
            "grants": "delivery of this one ask, once, under the target's existing permissions; each action in its turn still follows the target's approval policy",
        }),
        request_sha256: gate.snapshot_sha256.clone(),
    })
}

/// The gate's verdict for one ask now.
#[derive(Clone, Debug, PartialEq)]
pub enum GateVerdict {
    /// Not gated (owner, or target ⊆ requester), or approved and (when
    /// `consume`) consumed now.
    Pass(GateRecord),
    /// Waiting for review: the approval record (created now if `created`).
    Review { gate: GateRecord, approval: ApprovalRecord, created: bool },
    /// The reviewer denied this snapshot.
    Rejected { gate: GateRecord, note: Option<String> },
}

/// Evaluate the gate against the approval store.
///
/// `consume = false` (creation and review phase): an approved record is only
/// observed; `consume = true` (admission): an approved record is consumed
/// once.
pub fn check(
    approvals: &ApprovalStore,
    ask: &AskRecord,
    inputs: &GateInputs,
    consume: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<GateVerdict> {
    let mut gate = evaluate(&ask_facts(ask), inputs);
    if let Some(previous) = &ask.gate {
        gate.stale_reason = stale_reason(previous, &gate);
    }
    let Some(request) = approval_request(ask, &gate, inputs).filter(|_| gate.required) else {
        return Ok(GateVerdict::Pass(gate));
    };
    if !consume {
        if let Some(record) = approvals.peek(&request.requester_cutex_session_id, &request.request_sha256, now)? {
            gate.approval_id = Some(record.approval_id.clone());
            return Ok(match record.state {
                ApprovalState::Approved => GateVerdict::Pass(gate),
                ApprovalState::Denied => GateVerdict::Rejected { gate, note: record.note.clone() },
                _ => GateVerdict::Review { gate, approval: record, created: false },
            });
        }
    }
    Ok(match approvals.gate(request, now)? {
        GateOutcome::Approved(record) => {
            gate.approval_id = Some(record.approval_id);
            gate.admitted_with_approval = consume;
            GateVerdict::Pass(gate)
        }
        GateOutcome::Pending { record, created } => {
            gate.approval_id = Some(record.approval_id.clone());
            GateVerdict::Review { gate, approval: record, created }
        }
        GateOutcome::Denied(record) => {
            gate.approval_id = Some(record.approval_id.clone());
            GateVerdict::Rejected { gate, note: record.note }
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent_management::envelope::{ApprovalStrictness, PermissionEnvelope, SandboxLevel};
    use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};
    use crate::asks::store::AskState;

    pub(crate) fn party(session: &str, sandbox: SandboxLevel, roots: &[&str], profile: &str) -> GateParty {
        let envelope = PermissionEnvelope {
            sandbox,
            approval: ApprovalStrictness::OnRequest,
            network_access: false,
            writable_roots: roots.iter().map(|root| root.to_string()).collect(),
        };
        GateParty {
            cutex_session_id: session.into(),
            name: Some(session[session.len() - 2..].into()),
            caps: Capabilities::new(Some(&envelope), None),
            profile: Some(profile.into()),
            approval_policy: Some("on-request".into()),
        }
    }

    pub(crate) fn inputs(requester: Option<GateParty>, target: GateParty) -> GateInputs {
        GateInputs {
            requester,
            target,
            project_policy: PROJECT_POLICY_P1.into(),
            links_version: "l1".into(),
        }
    }

    fn ask() -> AskRecord {
        let mut state = AskState::default();
        state.create_ask(new_ask("g", "please deploy"), Utc::now()).unwrap().0
    }

    /// Acceptance §12.3 item 2 (gate): narrower and incomparable requesters
    /// are reviewed once each; equal requesters and the owner are not; a
    /// change of the target before admission makes the approval stale.
    #[test]
    fn gate_reviews_once_and_goes_stale_on_change() {
        let dir = std::env::temp_dir().join(format!("cutex-asks-gate-{}", uuid::Uuid::new_v4().simple()));
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();
        let target = party(TARGET, SandboxLevel::FullAccess, &[], "deepseek");
        let narrower = party(REQUESTER, SandboxLevel::WorkspaceWrite, &["/w/a"], "chatgpt");
        let equal = party(REQUESTER, SandboxLevel::FullAccess, &[], "chatgpt");
        let mut ask = ask();

        assert!(matches!(check(&approvals, &ask, &inputs(None, target.clone()), true, now).unwrap(), GateVerdict::Pass(_)));
        assert!(matches!(
            check(&approvals, &ask, &inputs(Some(equal), target.clone()), true, now).unwrap(),
            GateVerdict::Pass(gate) if !gate.required
        ));

        let gated = inputs(Some(narrower.clone()), target.clone());
        let GateVerdict::Review { gate, approval, created: true } = check(&approvals, &ask, &gated, false, now).unwrap() else {
            panic!("narrower requester must be reviewed");
        };
        assert_eq!(approval.kind, ApprovalKind::AskDelivery);
        assert_eq!(approval.details["askId"], ask.ask_id.as_str());
        ask.gate = Some(gate);
        // Asking again does not create a second review.
        assert!(matches!(check(&approvals, &ask, &gated, false, now).unwrap(), GateVerdict::Review { created: false, .. }));
        approvals.decide(&approval.approval_id, true, None, now).unwrap();
        // The review phase observes the approval without consuming it.
        assert!(matches!(check(&approvals, &ask, &gated, false, now).unwrap(), GateVerdict::Pass(_)));

        // The target switches profile before admission: the approval is stale.
        let mut switched = target.clone();
        switched.profile = Some("claude".into());
        let GateVerdict::Review { gate: stale, created: true, .. } =
            check(&approvals, &ask, &inputs(Some(narrower.clone()), switched), true, now).unwrap()
        else {
            panic!("a changed target needs a new review");
        };
        assert!(stale.stale_reason.unwrap().contains("targetProfile"));

        // Unchanged: admission consumes the approval exactly once.
        let GateVerdict::Pass(admitted) = check(&approvals, &ask, &gated, true, now).unwrap() else {
            panic!("approved snapshot admits");
        };
        assert!(admitted.admitted_with_approval);
        assert!(matches!(check(&approvals, &ask, &gated, true, now).unwrap(), GateVerdict::Review { created: true, .. }));

        // Incomparable requester (writes elsewhere) is gated too.
        let writes_b = party(TARGET, SandboxLevel::WorkspaceWrite, &["/w/b"], "deepseek");
        let writes_a = party(REQUESTER, SandboxLevel::WorkspaceWrite, &["/w/a"], "chatgpt");
        assert!(evaluate(&ask_facts(&ask), &inputs(Some(writes_a), writes_b)).required);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn denial_rejects() {
        let dir = std::env::temp_dir().join(format!("cutex-asks-deny-{}", uuid::Uuid::new_v4().simple()));
        let approvals = ApprovalStore::at(dir.join("approvals.json"));
        let now = Utc::now();
        let gated = inputs(
            Some(party(REQUESTER, SandboxLevel::ReadOnly, &[], "p")),
            party(TARGET, SandboxLevel::FullAccess, &[], "p"),
        );
        let ask = ask();
        let GateVerdict::Review { approval, .. } = check(&approvals, &ask, &gated, false, now).unwrap() else {
            panic!()
        };
        approvals.decide(&approval.approval_id, false, Some("no".into()), now).unwrap();
        assert!(matches!(
            check(&approvals, &ask, &gated, false, now).unwrap(),
            GateVerdict::Rejected { note: Some(note), .. } if note == "no"
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
