use super::*;
use crate::asks::store::AskState;
use serde_json::{json,Value};

/// Parent builds this from current authenticated per-resource permissions.
/// A ref is never itself a visibility or mutation credential.
#[derive(Clone, Debug, Default)]
pub struct NodeAccess {
    pub visible:bool,
    pub read_attempts:bool,
    pub available:bool,
    pub stop:bool,
    pub stop_jobs:bool,
    pub resume_continuation:bool,
}

#[derive(Clone, Debug)]
pub struct ReadScope {
    pub caller:String,
    pub project:Option<String>,
    pub permission_revision:u64,
    pub resources:BTreeMap<DelegationRef,NodeAccess>,
}

impl ReadScope {
    pub fn access(&self,reference:&DelegationRef)->NodeAccess {
        self.resources.get(reference).cloned().unwrap_or_default()
    }
}

/// Immutable query data, scoped to caller and authority revision. Parent keeps
/// it in its existing query snapshot cache; do not serialize this internal type.
pub struct TreeSnapshot {
    caller:String,
    permission_revision:u64,
    revision:String,
    rows:Vec<(Value,Vec<Value>,Vec<Value>)>,
    cursors:BTreeMap<String,usize>,
    offsets:BTreeMap<usize,String>,
    coverage:&'static str,
}

fn opaque(prefix:&str)->String {format!("{prefix}_{}",uuid::Uuid::new_v4().simple())}

struct RelationProjection {
    reference:Option<DelegationRef>,
    state:&'static str,
    redacted:Option<Value>,
}

fn relation(
    from:&DelegationRef,name:&str,target:Option<&DelegationRef>,known:bool,scope:&ReadScope,
    placeholders:&mut Vec<Value>,unresolved:&mut Vec<Value>,
)->RelationProjection {
    let Some(target)=target else {
        if !known {unresolved.push(json!({"from":from,"relation":name,"reason":"legacyUnknown"}));}
        return RelationProjection{reference:None,state:if known {"none"} else {"unknown"},redacted:None};
    };
    let access=scope.access(target);
    if access.visible {
        if !access.available {unresolved.push(json!({"from":from,"relation":name,"reason":"unavailable"}));}
        RelationProjection{reference:Some(target.clone()),state:"visible",redacted:None}
    } else {
        // Random per visible edge, never a hash/base64 of the hidden endpoint.
        // Parent/root/followUp get independent tokens even for the same target.
        let token=opaque("rp");
        placeholders.push(json!({"kind":"redactedAncestor","placeholderId":token,
            "labelKey":"delegation.ancestorRestricted","reason":"notVisible","expandable":false,"allowedActions":{}}));
        unresolved.push(json!({"from":from,"relation":name,"reason":"redacted"}));
        RelationProjection{reference:None,state:"redacted",redacted:Some(json!({"placeholderId":token}))}
    }
}

/// Fixed presentation codes only. Never copy native identities or RPC errors
/// out of the original intent; a claim is not proof that an interrupt ran.
fn stop_reason(ask:&crate::asks::model::AskRecord,fence:&StopFence,result:Option<&Value>)->&'static str {
    use crate::asks::model::StopDispatchOutcome;
    let already_closed=result.and_then(|result|result["nodeOutcomes"].as_array()).is_some_and(|rows|
        rows.iter().any(|row|row["id"]==ask.ask_id&&row["hostId"]==fence.root.host_id()&&row["phase"]=="alreadyClosed"))
        || (ask.closed.is_some()&&ask.closed_at.as_deref().and_then(crate::asks::parse_time)
            // Ask timestamps are persisted at millisecond precision. Do not
            // mistake the same stop transaction's truncated time for earlier closure.
            .is_some_and(|closed|closed.timestamp_millis()<fence.created_at.timestamp_millis()));
    if already_closed {return "alreadyClosed";}
    let intent=ask.current_attempt().and_then(|attempt|attempt.stop_intent.as_ref().filter(|intent|
        intent.cutex_session_id==ask.target_cutex_session_id&&intent.attempt==attempt.n
            &&intent.runtime_generation==attempt.runtime_generation&&intent.native_message_id==attempt.native_message_id
            &&intent.turn_id.as_ref().is_none_or(|turn|attempt.turn_id.as_ref()==Some(turn))));
    match intent.map(|intent|intent.outcome) {
        Some(StopDispatchOutcome::Claimed)=>"claimedUncertain",
        Some(StopDispatchOutcome::Unknown)=>"outcomeUnknown",
        Some(StopDispatchOutcome::Submitted)=>"interruptSubmitted",
        Some(StopDispatchOutcome::Unsupported)=>"unsupported",
        Some(StopDispatchOutcome::Stale)=>"stale",
        Some(StopDispatchOutcome::Pending) if intent.is_some_and(|intent|intent.claimed_at.is_some())=>"claimedUncertain",
        Some(StopDispatchOutcome::Pending)|None=>"requested",
    }
}

impl TreeSnapshot {
    /// Reads canonical asks plus their persisted metadata. No ID/name/time
    /// inference, remote fetch, runtime action or writer is used by projection.
    pub fn capture(
        state:&AskState,lineage:&LineageState,host:&str,scope:&ReadScope,root:Option<&DelegationRef>,
    )->Result<Self,AskError> {
        if scope.caller.is_empty() {return Err(error("invalid_scope"));}
        if root.is_some_and(|root|!scope.access(root).visible) {return Err(error("ask_not_found"));}
        let included=if let Some(root)=root {Some(super::stop::verified_descendants(lineage,root,true)?)} else {None};
        // Bound only this authorized project/root query. A different project's
        // live/cold history cannot disable an otherwise small tree.
        let mut selected=BTreeMap::new();
        for ask in state.asks.values() {
            if scope.project.as_deref().is_some_and(|p|ask.project_id.as_deref()!=Some(p)){continue;}
            let reference=DelegationRef::new(host,&ask.ask_id).map_err(|_|error("invalid_reference"))?;
            if included.as_ref().is_some_and(|included|!included.contains(&reference)) {continue;}
            let access=scope.access(&reference);
            if !access.visible {continue;}
            if selected.len()>=MAX_LINEAGE_NODES {return Err(error("lineage_limit"));}
            selected.insert(reference,(ask,access));
        }
        let mut rows=Vec::new();
        // A verified descendant can exist only as linked/cold metadata, or be
        // hidden by the current scope. Do not silently turn that into complete
        // coverage. No missing endpoint, count or depth is exposed in the view.
        let mut partial=included.as_ref().is_some_and(|included|
            included.iter().any(|reference|!selected.contains_key(reference)));
        partial|=lineage.records.values().any(|record|
            matches!(&record.parent,ParentFact::Verified(parent)
                if selected.contains_key(&parent.reference)&&!selected.contains_key(&record.reference)));
        for (reference,(ask,own_access)) in selected {
            partial|=!own_access.available||ask.remote.is_some();
            let record=lineage.get(&reference);
            let verified=record.is_some_and(|record|!matches!(record.parent,ParentFact::Unknown));
            let parent_fact=record.and_then(|record|match &record.parent {ParentFact::Verified(parent)=>Some(parent),_=>None});
            let mut placeholders=Vec::new(); let mut unresolved=Vec::new();
            let parent=relation(&reference,"parent",parent_fact.map(|parent|&parent.reference),verified,scope,&mut placeholders,&mut unresolved);
            let root_fact=record.and_then(|record|record.root.as_ref());
            let mut root_projection=relation(&reference,"root",root_fact,root_fact.is_some(),scope,&mut placeholders,&mut unresolved);
            if root_projection.state=="none" {root_projection.state="unknown";}
            let follow_up=relation(&reference,"followUp",record.and_then(|record|record.follow_up.as_ref()),verified,scope,&mut placeholders,&mut unresolved);
            let legacy_id=|relation:&RelationProjection|relation.reference.as_ref().filter(|r|r.host_id()==host).map(|r|r.id().to_string());
            let parent_attempt=parent_fact.filter(|p|parent.state=="visible" && scope.access(&p.reference).read_attempts).map(|p|p.attempt_id.clone());
            partial|=!unresolved.is_empty();
            let attempt=ask.current_attempt();
            // Only this attempt's explicit progress carries an activity time.
            // Resource updatedAt and target presence can describe other work.
            let activity=attempt.and_then(|a|a.activity.last_activity_at.as_deref().into_iter()
                .chain(a.progress.last().map(|p|p.at.as_str())).max());
            let requester=match &ask.requester {
                crate::asks::model::Requester::Agent{cutex_session_id,name}=>json!({"id":cutex_session_id,"hostId":host,"name":name}),
                crate::asks::model::Requester::Owner=>json!({"kind":"owner"}),
                crate::asks::model::Requester::Peer{..}=>json!({"kind":"redacted","reason":"crossHostIdentityUnverified"}),
            };
            let continuation=ask.bounded_continuation.current.as_ref().map(|c|{
                let visible_wait=match &c.declaration.wake{
                    Some(crate::asks::continuation::WakeCondition::Children{children,..})=>children.iter().all(|child|
                        child.kind==crate::asks::continuation::ChildKind::Delegation&&DelegationRef::new(host,&child.id).is_ok_and(|r|scope.access(&r).visible)),
                    _=>true,
                };
                json!({
                "state":c.state,"revision":c.revision,"budgetEpoch":c.budget_epoch,
                "roundNumber":c.round_number,"startedAt":c.started_at,"deadlineAt":c.deadline_at,
                "nextWakeAt":c.next_wake_at,"limitReason":c.limit_reason,"cancelReason":c.cancel_reason,
                "reconciling":c.reconciling,
                "maxRounds":crate::asks::continuation::MAX_ROUNDS,"maxElapsedSeconds":crate::asks::continuation::MAX_ELAPSED_SECONDS,
                "waitingFor":if visible_wait{serde_json::to_value(&c.declaration.wake).unwrap()}else{Value::Null},
                "sourceAttemptId":own_access.read_attempts.then_some(&c.declaration.source.attempt_id),
            })});
            let stop=lineage.stop_fences.get(&reference_key(&reference)).map(|f|{
                let key=format!("{}\u{1f}{}",f.requested_by,f.action_id);
                let result=state.actions.get(&key).and_then(|a|a.result.as_ref());
                let operation=result.and_then(|r|r.pointer("/receipt/operationId"));
                json!({"requestedAt":f.created_at,"phase":if ask.closed.is_some(){"stopped"}else{"pending"},"reason":stop_reason(ask,f,result),"operationId":operation})
            });
            let routing_approval=ask.routing_approval_id.as_ref().and_then(|id|state.routing.approvals.get(id))
                .map(|a|json!({"id":a.id,"state":a.state,"revision":a.revision}));
            let target=if ask.remote.is_some(){
                json!({"kind":"redacted","reason":"crossHostIdentityUnverified"})
            }else{json!({"id":ask.target_cutex_session_id,"hostId":host,"name":ask.target_name})};
            let mut node=json!({
                "kind":"delegation","ref":reference,"id":ask.ask_id,"hostId":host,
                "projectId":ask.project_id,"requester":requester,
                "target":target,
                "title":ask.title,"execution":ask.execution,"hold":ask.hold,"review":ask.review,
                "parent":parent.reference,"parentState":parent.state,"redactedParentRef":parent.redacted,
                "root":root_projection.reference,"rootState":root_projection.state,"redactedRootRef":root_projection.redacted,
                "followUp":follow_up.reference,"followUpState":follow_up.state,"redactedFollowUpRef":follow_up.redacted,
                "parentId":legacy_id(&parent),"rootId":legacy_id(&root_projection),"followUpOf":legacy_id(&follow_up),
                "parentAttemptId":parent_attempt,"redactedParent":parent.state=="redacted",
                "lineageState":if verified {"verified"} else {"legacyUnknown"},
                "allowedActions":{"readAttempts":own_access.read_attempts,"stop":own_access.stop,"stopSelf":own_access.stop,
                    "stopJobs":own_access.stop&&own_access.stop_jobs,"declareContinuation":false,"resumeContinuation":own_access.resume_continuation},
            });
            let details=json!({
                "closed":ask.closed,"createdAt":ask.created_at,"updatedAt":ask.updated_at,"revision":ask.revision,
                "attemptId":attempt.map(|a|a.attempt_id(&ask.ask_id)),"attemptNumber":attempt.map(|a|a.n),
                "attemptStartedAt":attempt.and_then(|a|a.started_at.as_deref()),
                "lastActivityAt":activity,"activitySource":if activity.is_some(){"attempt"}else{"unknown"},
                "continuation":continuation,
                "routingApproval":routing_approval,"stop":stop,"resumeEvidence":{
                    "kind":if attempt.is_some_and(|a|a.n>1){"unknown"}else{"none"},
                    "previousAttemptId":null,"previousAttemptEndedAt":null,
                    "resumedAt":null,"cause":null,"actionId":null},
            });
            node.as_object_mut().unwrap().extend(details.as_object().unwrap().clone());
            rows.push((node,placeholders,unresolved));
        }
        // Resource ordering is deterministic inside a frozen query snapshot.
        rows.sort_by(|a,b|a.0["id"].as_str().cmp(&b.0["id"].as_str()));
        Ok(Self{caller:scope.caller.clone(),permission_revision:scope.permission_revision,revision:opaque("tree"),
            rows,cursors:BTreeMap::new(),offsets:BTreeMap::new(),coverage:if partial {"partial"} else {"complete"}})
    }

    /// Opaque cursors contain no offsets, endpoints, hidden counts or depth.
    /// Permission changes and cross-caller reuse invalidate the entire snapshot.
    pub fn page(&mut self,caller:&str,permission_revision:u64,cursor:Option<&str>,limit:usize)->Result<Value,AskError> {
        if caller!=self.caller || permission_revision!=self.permission_revision {return Err(error("cursor_invalidated"));}
        if limit==0 || limit>200 {return Err(error("invalid_limit"));}
        let offset=match cursor {Some(cursor)=>*self.cursors.get(cursor).ok_or_else(||error("cursor_invalidated"))?,None=>0};
        let end=offset.saturating_add(limit).min(self.rows.len());
        let mut nodes=Vec::new(); let mut unresolved=Vec::new();
        for (node,placeholders,edges) in &self.rows[offset..end] {
            nodes.push(node.clone()); nodes.extend(placeholders.iter().cloned()); unresolved.extend(edges.iter().cloned());
        }
        let next=if end<self.rows.len() {
            let token=self.offsets.entry(end).or_insert_with(||opaque("lc")).clone();
            self.cursors.insert(token.clone(),end); Some(token)
        } else {None};
        Ok(json!({"nodes":nodes,"nextCursor":next,"revision":self.revision,"coverage":self.coverage,"unresolvedEdges":unresolved}))
    }
}
