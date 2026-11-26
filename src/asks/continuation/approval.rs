//! Owner/Director decision on a yielded continuation whose segment expired.
use super::{Actor, Authority, LimitReason, Phase, ResetRequest, Submission};
use crate::asks::{
    delivery,
    model::{AskRecord, AttemptState},
    routing_policy::{self, RoutingActor, RoutingContext, RoutingDecision},
    stop_settlement,
    store::{AskError, AskState, AskStore},
    terminal_notices,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: String,
    pub delegation_id: String,
    pub budget_epoch: u64,
    pub authority_epoch: u64,
    pub director_session_id: Option<String>,
    pub revision: u64,
    pub digest: String,
    pub reason: LimitReason,
    pub requested_at: DateTime<Utc>,
    pub brief_preview: String,
    pub progress_preview: Option<String>,
    pub decision: Option<RoutingDecision>,
    pub decision_note: Option<String>,
    pub decided_by: Option<Actor>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionRequest {
    pub action_id: String,
    pub expected_revision: u64,
    pub expected_digest: String,
    pub expected_epoch: u64,
    pub decision: RoutingDecision,
    pub decision_note: Option<String>,
}

fn eligible(ask: &AskRecord) -> bool {
    ask.remote.is_none()
        && ask.is_open()
        && !ask.follow_up_ended
        && ask
            .current_attempt()
            .is_some_and(|a| a.state == AttemptState::Yielded && !a.stop_requested)
        && ask
            .bounded_continuation
            .current
            .as_ref()
            .is_some_and(|record| {
                record.state == Phase::Limited
                    && record
                        .limit_notice
                        .as_ref()
                        .is_some_and(|notice| notice.enqueued)
                    && record.running_attempt.is_none()
                    && !record.reconciling
                    && record
                        .wake
                        .as_ref()
                        .is_none_or(|wake| matches!(wake.submission, Submission::Unclaimed))
            })
}

fn director_recipient<'a>(ask: &AskRecord, ctx: Option<&'a RoutingContext>) -> Option<&'a str> {
    ctx.and_then(|c| c.current_director.as_ref()).map(|r| r.id())
        .filter(|id| *id != ask.target_cutex_session_id)
}

fn matches_seat(ask: &AskRecord, approval: &Approval, ctx: Option<&RoutingContext>) -> bool {
    approval.authority_epoch == ctx.map_or(0, |c| c.authority_epoch)
        && approval.director_session_id.as_deref() == director_recipient(ask, ctx)
}

fn in_scope(ask: &AskRecord, ctx: Option<&RoutingContext>) -> bool {
    ask.project_id.as_deref() == ctx.map(|c| c.project_id.as_str())
}

fn pending(ask: &AskRecord) -> Option<&Approval> {
    if !eligible(ask) {
        return None;
    }
    let epoch = ask.bounded_continuation.current.as_ref()?.budget_epoch;
    ask.bounded_continuation
        .approval
        .as_ref()
        .filter(|a| a.budget_epoch == epoch && a.decision.is_none())
}

fn sync_needed(ask: &AskRecord, ctx: Option<&RoutingContext>) -> bool {
    in_scope(ask, ctx) && eligible(ask) && pending(ask).is_none_or(|a| !matches_seat(ask, a, ctx))
}

fn address(ask: &AskRecord, approval: &mut Approval, ctx: Option<&RoutingContext>) {
    approval.authority_epoch = ctx.map_or(0, |c| c.authority_epoch);
    approval.director_session_id = director_recipient(ask, ctx).map(str::to_owned);
    approval.digest.clear();
    approval.digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&approval).expect("approval serializes"))
    );
}

fn refreshed(ask: &AskRecord, approval: &Approval, ctx: Option<&RoutingContext>) -> Approval {
    let mut approval = approval.clone();
    if !matches_seat(ask, &approval, ctx) {
        approval.revision += 1;
        address(ask, &mut approval, ctx);
    }
    approval
}

fn approval_event(state: &mut AskState, ask: &AskRecord, approval: &Approval) {
    let mut recipients = vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.into()];
    recipients.extend(approval.director_session_id.iter().cloned());
    state.notice(
        routing_policy::integration::ROUTING_CHANGED,
        recipients,
        json!({
            "projectId": ask.project_id, "resourceId": approval.id, "revision": approval.revision,
        }),
    );
}

/// Called with the roster authority fence and the AskStore transaction held.
pub(crate) fn sync_one(
    state: &mut AskState,
    id: &str,
    ctx: Option<&RoutingContext>,
    now: DateTime<Utc>,
) {
    let Some(ask) = state.asks.get(id).filter(|ask| sync_needed(ask, ctx)) else {
        return;
    };
    let record = ask.bounded_continuation.current.as_ref().unwrap();
    let Some(reason) = record.limit_reason else {
        return;
    };
    let old = ask
        .bounded_continuation
        .approval
        .as_ref()
        .filter(|a| a.budget_epoch == record.budget_epoch);
    let first = old.is_none();
    let approval = if let Some(previous) = old {
        refreshed(ask, previous, ctx)
    } else {
        let attempt = ask.current_attempt().unwrap();
        let mut approval = Approval {
            id: format!(
                "continuation-approval:{}:{}",
                ask.ask_id, record.budget_epoch
            ),
            delegation_id: ask.ask_id.clone(),
            budget_epoch: record.budget_epoch,
            authority_epoch: 0,
            director_session_id: None,
            revision: 1,
            digest: String::new(),
            reason,
            requested_at: now,
            brief_preview: crate::asks::truncate_chars(&ask.body.text, 280),
            progress_preview: attempt
                .stage_note
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| attempt.progress.last().map(|p| p.text.as_str()))
                .or(Some(record.declaration.note.as_str()))
                .map(|s| crate::asks::truncate_chars(s, 480)),
            decision: None,
            decision_note: None,
            decided_by: None,
            decided_at: None,
        };
        address(ask, &mut approval, ctx);
        approval
    };
    state.update_ask(id, now, |ask| {
        ask.bounded_continuation.approval = Some(approval.clone())
    });
    let ask = state.asks[id].clone();
    approval_event(state, &ask, &approval);
    if first {
        delivery::record_state_delivery(state, &ask, "continuationExpired", now);
    }
}

pub fn owner_context(
    roster: &crate::agent_management::AgentManagementSnapshot,
    project: &str,
    host: &str,
) -> RoutingContext {
    routing_policy::integration::trusted_context(roster, project, RoutingActor::Owner, host)
        .unwrap_or_else(|_| fallback_owner_context(roster, project))
}

fn fallback_owner_context(
    roster: &crate::agent_management::AgentManagementSnapshot,
    project: &str,
) -> RoutingContext {
    let epoch = crate::agent_management::ProjectId::new(project.to_owned()).ok()
        .map_or(0, |id| crate::agent_management::effective_authority_epoch(roster, &id, roster.projects.get(&id)));
    RoutingContext { project_id: project.into(), authority_epoch: epoch, current_director: None,
        actor: RoutingActor::Owner, operator_may_route: false }
}

struct MissingProjects {
    roster: Option<(std::path::PathBuf, u64)>,
    projects: std::collections::BTreeMap<String, RoutingContext>,
}
static MISSING_PROJECTS: std::sync::Mutex<MissingProjects> =
    std::sync::Mutex::new(MissingProjects { roster: None, projects: std::collections::BTreeMap::new() });

const SYNC_BATCH_SIZE: usize = 64;

pub(super) fn project_changes(
    store: &AskStore,
    snapshot: &AskState,
    roster: &crate::agent_management::AgentManagementSnapshot,
    host: &str,
) -> Vec<(String, RoutingContext)> {
    let mut cache = MISSING_PROJECTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.roster.as_ref().is_none_or(|(dir, revision)| dir != store.dir() || *revision != roster.store_revision) {
        cache.roster = Some((store.dir().into(), roster.store_revision));
        cache.projects.clear();
    }
    let mut projects = std::collections::BTreeMap::new();
    snapshot.asks.values().filter(|ask| eligible(ask)).filter_map(|ask| {
        let project = ask.project_id.as_deref()?;
        let ctx = projects.entry(project).or_insert_with(|| {
            if let Some(ctx) = cache.projects.get(project) { return ctx.clone(); }
            routing_policy::integration::trusted_context(roster, project, RoutingActor::Owner, host)
                .unwrap_or_else(|_| {
                    let ctx = fallback_owner_context(roster, project);
                    cache.projects.insert(project.into(), ctx.clone());
                    ctx
                })
        });
        sync_needed(ask, Some(ctx)).then(|| (ask.ask_id.clone(), ctx.clone()))
    }).take(SYNC_BATCH_SIZE).collect()
}

pub fn tick(store: &AskStore, now: DateTime<Utc>) -> anyhow::Result<()> {
    // Open approvals can predate this release or a seat change. Discover
    // them from the shared hot snapshot; each tick writes only bounded batches
    // of candidates and rechecks their eligibility in the keyed transaction.
    let snapshot = store.shared()?;
    let unscoped: Vec<_> = snapshot
        .asks
        .values()
        .filter(|ask| sync_needed(ask, None))
        .take(SYNC_BATCH_SIZE)
        .map(|ask| ask.ask_id.as_str())
        .collect();
    if !unscoped.is_empty() {
        store.with_records(&unscoped, |state| {
            for &id in &unscoped {
                sync_one(state, id, None, now);
            }
        })?;
    }
    if !snapshot
        .asks
        .values()
        .any(|ask| ask.project_id.is_some() && eligible(ask))
    {
        return Ok(());
    }
    let host = routing_policy::integration::local_host_id();
    let Ok(roster) = crate::asks::directory::Directory::director_snapshot_shared() else {
        return Ok(());
    };
    let candidates = project_changes(store, &snapshot, &roster, &host);
    if candidates.is_empty() {
        return Ok(());
    }
    let keys: Vec<_> = candidates.iter().map(|(id, _)| id.as_str()).collect();
    let _ = crate::asks::pause_admission::try_with_authority(|roster| {
        store.with_records(&keys, |state| {
            let mut projects = std::collections::BTreeMap::new();
            for id in keys.iter().copied() {
                let Some(project) = state.asks.get(id).and_then(|ask| ask.project_id.clone()) else {
                    continue;
                };
                let ctx = projects.entry(project).or_insert_with_key(|project| owner_context(roster, project, &host));
                sync_one(state, id, Some(ctx), now);
            }
        })
    })?;
    Ok(())
}

pub fn has_authority(ask: &AskRecord, ctx: Option<&RoutingContext>) -> bool {
    in_scope(ask, ctx) && ctx.is_none_or(|c| {
        c.management_authority().is_ok()
            && c.actor.reference().is_none_or(|actor| Some(actor.id()) == director_recipient(ask, ctx))
    })
}

pub fn can_decide(ask: &AskRecord, ctx: Option<&RoutingContext>) -> bool {
    has_authority(ask, ctx) && pending(ask).is_some()
}

pub fn action_card(ask: &AskRecord, ctx: Option<&RoutingContext>) -> Option<Value> {
    if !can_decide(ask, ctx) { return None; }
    let card = refreshed(ask, pending(ask)?, ctx);
    Some(json!({"id":card.id,"revision":card.revision,"digest":card.digest,
        "authorityEpoch":card.authority_epoch,"allowedActions":{"approve":true,"deny":true}}))
}

fn view(ask: &AskRecord, ctx: Option<&RoutingContext>, approval: &Approval) -> Value {
    // A refresh can use the current roster immediately. The next tick or
    // decision commits this same deterministic revision and digest.
    let allowed = can_decide(ask, ctx);
    let mut view = if allowed && approval.decision.is_none() {
        serde_json::to_value(refreshed(ask, approval, ctx))
    } else {
        serde_json::to_value(approval)
    }
    .expect("approval serializes");
    view["allowedActions"] = json!({"approve": allowed, "deny": allowed});
    view
}

pub fn list(state: &AskState, ctx: Option<&RoutingContext>, pending_only: bool) -> Vec<Value> {
    if ctx.is_some_and(|ctx| ctx.management_authority().is_err()) {
        return Vec::new();
    }
    state
        .asks
        .values()
        .filter(|ask| has_authority(ask, ctx))
        .filter_map(|ask| {
            let approval = if pending_only {
                pending(ask)?
            } else {
                ask.bounded_continuation.approval.as_ref()?
            };
            Some(view(ask, ctx, approval))
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum OwnerDecision {
    Current(DecisionRequest),
    Legacy(ResetRequest),
}

impl OwnerDecision {
    pub fn action_id(&self) -> &str {
        match self {
            Self::Current(request) => &request.action_id,
            Self::Legacy(request) => &request.action_id,
        }
    }
}

pub fn decide_owner(
    state: &mut AskState,
    id: &str,
    ctx: Option<&RoutingContext>,
    input: &OwnerDecision,
    now: DateTime<Utc>,
) -> Result<Value, AskError> {
    let request = match input {
        OwnerDecision::Current(request) => request.clone(),
        OwnerDecision::Legacy(request) => {
            let ask = state
                .asks
                .get(id)
                .filter(|ask| in_scope(ask, ctx))
                .ok_or_else(|| AskError::new("ask_not_found", "No such delegation"))?;
            let approval = pending(ask).ok_or_else(|| {
                AskError::new("approval_not_pending", "No pending continuation approval")
            })?;
            if ask
                .bounded_continuation
                .current
                .as_ref()
                .is_none_or(|record| record.revision != request.expected_revision)
            {
                return Err(AskError::new(
                    "approval_changed",
                    "Continuation changed; refresh the delegation before deciding",
                ));
            }
            let approval = refreshed(ask, approval, ctx);
            DecisionRequest {
                action_id: request.action_id.clone(),
                expected_revision: approval.revision,
                expected_digest: approval.digest,
                expected_epoch: approval.authority_epoch,
                decision: RoutingDecision::Approve,
                decision_note: None,
            }
        }
    };
    decide(state, id, ctx, &Actor::Owner, &request, now)
}

pub fn decide(
    state: &mut AskState,
    id: &str,
    ctx: Option<&RoutingContext>,
    actor: &Actor,
    request: &DecisionRequest,
    now: DateTime<Utc>,
) -> Result<Value, AskError> {
    let authority = Authority {
        actor,
        current_director: ctx
            .and_then(|c| c.current_director.as_ref())
            .map(|r| r.id()),
    };
    if !authority.may_reset() || ctx.is_some_and(|ctx| ctx.management_authority().is_err()) {
        return Err(AskError::new(
            "forbidden",
            "Only the owner or current project Director can decide",
        ));
    }
    let ask = state
        .asks
        .get(id)
        .filter(|a| in_scope(a, ctx))
        .ok_or_else(|| AskError::new("ask_not_found", "No such project delegation"))?;
    if !has_authority(ask, ctx) {
        return Err(AskError::new("forbidden", "A project Director cannot decide their own continuation"));
    }
    let mut approval = pending(ask)
        .map(|a| refreshed(ask, a, ctx))
        .ok_or_else(|| AskError::new("approval_not_pending", "No pending continuation approval"))?;
    if request.expected_epoch != approval.authority_epoch
        || request.expected_revision != approval.revision
        || request.expected_digest != approval.digest
    {
        return Err(AskError::new(
            "approval_changed",
            "Continuation approval changed; refresh the approval list before deciding",
        ));
    }
    let note = request
        .decision_note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if note.is_some_and(|s| s.chars().count() > 200)
        || (request.decision == RoutingDecision::Deny && note.is_none())
    {
        return Err(AskError::new(
            "invalid_arguments",
            "Denial needs a reason of at most 200 characters",
        ));
    }
    if super::runtime::ancestor_stopped(state, ask)
        || !stop_settlement::terminal_without_lease(state, ask)
    {
        return Err(AskError::new(
            "approval_guarded",
            "The staged attempt no longer permits a continuation decision",
        ));
    }
    let revision = ask.bounded_continuation.current.as_ref().unwrap().revision;
    approval.revision += 1;
    approval.decision = Some(request.decision);
    approval.decision_note = note.map(str::to_owned);
    approval.decided_by = Some(actor.clone());
    approval.decided_at = Some(now);
    let result = match request.decision {
        RoutingDecision::Approve => {
            let receipt = state.update_ask(id, now, |ask| {
                let receipt = ask.bounded_continuation.approve_budget(
                    authority, &ResetRequest { action_id: request.action_id.clone(), expected_revision: revision },
                    now,
                )?;
                ask.bounded_continuation.approval = Some(approval.clone());
                Ok(receipt)
            }).ok_or_else(|| super::runtime::ask_error(super::Error::NotLimited))?
                .map_err(super::runtime::ask_error)?;
            super::runtime::changed_and_notice(state, id, approval.director_session_id.as_deref(), now);
            let result = json!({"accepted":true,"continuation":super::runtime::projection_record(&receipt.continuation),
                "replayed":receipt.replayed});
            let ask = state.asks[id].clone();
            delivery::record_state_delivery(state, &ask, "continuationApproved", now);
            result
        }
        RoutingDecision::Deny => {
            let actor = match actor {
                Actor::Owner => terminal_notices::Actor::Owner,
                Actor::Agent { session_id } => terminal_notices::Actor::Agent {
                    session_id: session_id.clone(),
                },
            };
            terminal_notices::set_cause(state, id, actor, note.unwrap());
            state.update_ask(id, now, |ask| {
                ask.bounded_continuation.approval = Some(approval.clone());
                ask.bounded_continuation.cancel("continuation_denied");
            });
            stop_settlement::close_stage(state, id, now, false);
            json!({"accepted": true})
        }
    };
    approval_event(state, &state.asks[id].clone(), &approval);
    let mut result = result;
    result["continuationApproval"] = view(&state.asks[id], ctx, &approval);
    Ok(result)
}
