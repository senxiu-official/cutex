//! Director review is a durable, actionable system message, not just UI SSE.
use super::*;
use crate::agent_management::AgentManagementSnapshot;
use crate::asks::store::AskState;
use integration::{request_and_root, trusted_context, ControllerView};

#[derive(Clone)]
pub struct ReviewNotice {
    pub key: String,
    pub to: String,
    pub text: String,
}

pub fn pending(
    state: &AskState,
    snapshot: &AgentManagementSnapshot,
    host: &str,
    controllers: &ControllerView<'_>,
) -> Vec<ReviewNotice> {
    state.routing.approvals.values().filter_map(|approval| {
        if approval.state != ApprovalState::Pending { return None; }
        let ask = state.asks.get(approval.delegation_ref.id())?;
        if !ask.is_open() || ask.remote.is_some() || ask.routing_approval_id.as_deref() != Some(&approval.id) { return None; }
        let (request, root) = request_and_root(state, ask, host, controllers).ok()?;
        let RootStatus::VerifiedOpen(root_ref) = &root else { return None; };
        if root_ref.host_id()!=host || !state.asks.get(root_ref.id()).is_some_and(|root_ask|
            root_ask.remote.is_none() && root_ask.project_id.as_deref()==Some(approval.project_id.as_str())) { return None; }
        let requester = request.requester_ref.as_ref().filter(|r| r.host_id() == host)?;
        if request.target_ref.host_id() != host { return None; }
        let ctx = trusted_context(snapshot, &approval.project_id,
            RoutingActor::Agent { reference: requester.clone(), name: None }, host).ok()?;
        trusted_context(snapshot, &approval.project_id,
            RoutingActor::Agent { reference: request.target_ref.clone(), name: None }, host).ok()?;
        if state.routing.check_approval(approval, &ctx, &request).is_err() { return None; }
        let to = ctx.current_director?.id().to_owned();
        let key = format!("routing-review:{}:{}:{}", approval.id, approval.revision,
            crate::asks::sha256_hex(to.as_bytes()));
        let facts = serde_json::json!({"projectId":approval.project_id,"approvalId":approval.id,
            "revision":approval.revision,"requestDigest":approval.request_digest,
            "delegationId":approval.delegation_ref.id(),"requester":requester,
            "target":request.target_ref,"briefPreview":ask.body.text.chars().take(400).collect::<String>()});
        let text = format!("[Project routing approval requested]\n{facts}\nReview this local project member delegation with delegation_routing. Read the current approval before deciding; use its exact revision and digest. Approving completes only this rooted one-use review route. Capability approvals remain the owner's, and pause/stop checks still apply.");
        Some(ReviewNotice { key, to, text })
    }).collect()
}

/// No AskStore/authority lock spans transport IO. The alarm's stable key is
/// durable deduplication; this cache keeps only current pending notices and
/// avoids repeated store reads without starving reviews beyond a fixed cap.
pub fn publish(dir: &std::path::Path, notices: Vec<ReviewNotice>) {
    use std::sync::{Mutex, OnceLock};
    type Key = (std::path::PathBuf, String);
    static SENT: OnceLock<Mutex<std::collections::BTreeSet<Key>>> = OnceLock::new();
    let sent = SENT.get_or_init(Default::default);
    let current: std::collections::BTreeSet<_> = notices.iter().map(|n| n.key.clone()).collect();
    sent.lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(home, key)| home == dir && current.contains(key));
    let candidates: Vec<_> = notices
        .into_iter()
        .filter(|n| {
            !sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&(dir.to_path_buf(), n.key.clone()))
        })
        .take(8)
        .collect();
    for notice in candidates {
        match crate::agent_bus::alarms::set_system_notice(
            crate::agent_bus::alarms::APPROVAL_NOTICE_OWNER,
            &notice.key,
            &notice.to,
            &notice.text,
        ) {
            Ok(_) => {
                let mut cache = sent.lock().unwrap_or_else(|e| e.into_inner());
                cache.insert((dir.to_path_buf(), notice.key));
            }
            Err(error) => eprintln!("asks: Director routing notice deferred: {error:#}"),
        }
    }
}
