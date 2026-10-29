use super::*;
use crate::asks::store::AskState;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub enum Cascade { None, #[default] Descendants }

/// Internal authority-checked plan. Do not serialize candidate refs: hidden
/// members and remote topology must not leak through a control receipt.
pub struct StopPlan {
    pub(crate) state_revision:u64,
    pub(crate) lineage_revision:u64,
    pub(crate) permission_revision:u64,
    pub(crate) local_host:String,
    pub(crate) fence:StopFence,
    pub(crate) selected:Vec<StopSelection>,
    pub(crate) partial:bool,
    pub(crate) stop_jobs:bool,
}

pub(crate) struct StopSelection {
    pub reference:DelegationRef,
    pub readable:bool,
    pub authorized:bool,
    pub jobs:bool,
}

pub(crate) fn verified_descendants(lineage:&LineageState,root:&DelegationRef,include_children:bool)->Result<BTreeSet<DelegationRef>,AskError> {
    let mut selected=BTreeSet::from([root.clone()]);
    // Self-stop must not depend on the size/shape of unrelated retained history.
    if !include_children {return Ok(selected);}
    // The canonical map has no reverse-parent index. Borrow its edges once;
    // never clone all historical refs or count them against this component.
    let mut edges:BTreeMap<&DelegationRef,Vec<&DelegationRef>>=BTreeMap::new();
    for record in lineage.records.values() {
        if let ParentFact::Verified(parent)=&record.parent {
            edges.entry(&parent.reference).or_default().push(&record.reference);
        }
    }
    let mut pending=vec![(root,0)];
    while let Some((reference,depth))=pending.pop() {
        // Bound discovery, not only the later pop: a wide component cannot
        // first allocate an arbitrarily large pending stack.
        for &child in edges.get(&reference).into_iter().flatten() {
            if selected.contains(child) {return Err(error("lineage_cycle"));}
            if depth>=MAX_LINEAGE_DEPTH {return Err(error("lineage_depth"));}
            if selected.len()>=MAX_LINEAGE_NODES {return Err(error("lineage_limit"));}
            selected.insert(child.clone());
            pending.push((child,depth+1));
        }
    }
    Ok(selected)
}

/// Parent calls under authority ordering + AskStore transaction. Defaults are
/// descendants and stopJobs=false. Follow-up links NEVER become child edges.
pub fn prepare_stop(
    state:&AskState,lineage:&LineageState,local_host:&str,scope:&ReadScope,
    root:&DelegationRef,cascade:Cascade,stop_jobs:bool,action_id:&str,now:DateTime<Utc>,
)->Result<StopPlan,AskError> {
    if action_id.is_empty() || action_id.len()>256 || scope.caller.is_empty() {return Err(error("invalid_arguments"));}
    let root_access=scope.access(root);
    if !root_access.visible || !root_access.stop || (stop_jobs&&!root_access.stop_jobs) {return Err(error("ask_not_found"));}
    if root.host_id()==local_host && !state.asks.contains_key(root.id()) {return Err(error("ask_not_found"));}
    let refs=verified_descendants(lineage,root,cascade==Cascade::Descendants)?;
    let mut partial=lineage.get(root).is_none_or(|record|matches!(record.parent,ParentFact::Unknown));
    if cascade==Cascade::Descendants {
        partial|=state.asks.values().any(|ask|ask.parent_ask.as_ref().is_some_and(|parent|
            refs.iter().any(|r|r.host_id()==local_host&&r.id()==parent))
            && DelegationRef::new(local_host,&ask.ask_id).ok().and_then(|r|lineage.get(&r))
                .is_none_or(|r|matches!(r.parent,ParentFact::Unknown)));
    }
    let selected=refs.into_iter().map(|reference|{
        let access=scope.access(&reference);
        let authorized=access.stop && (!stop_jobs||access.stop_jobs);
        partial|=!authorized;
        StopSelection {reference,readable:access.visible,authorized,jobs:stop_jobs&&access.stop_jobs}
    }).collect();
    Ok(StopPlan {state_revision:state.revision,lineage_revision:lineage.revision,
        permission_revision:scope.permission_revision,local_host:local_host.into(),selected,partial,stop_jobs,
        fence:StopFence {root:root.clone(),action_id:action_id.into(),requested_by:scope.caller.clone(),
            descendants:cascade==Cascade::Descendants,created_at:now}})
}

/// In-memory commit only; parent persists this with all local close results in
/// the same AskStore transaction. No resume path removes a stop fence.
pub fn commit_stop_fence(lineage:&mut LineageState,plan:&StopPlan)->Result<(),AskError> {
    if lineage.revision!=plan.lineage_revision {return Err(error("lineage_conflict"));}
    let key=reference_key(&plan.fence.root);
    if let Some(old)=lineage.stop_fences.get(&key) {
        if old==&plan.fence {return Ok(());}
        // A later self-only stop must never weaken an earlier descendants fence.
        if old.descendants && !plan.fence.descendants {return Ok(());}
    }
    let revision=lineage.revision.checked_add(1).ok_or_else(||error("revision_overflow"))?;
    lineage.stop_fences.insert(key,plan.fence.clone()); lineage.revision=revision;
    Ok(())
}

impl StopPlan {
    pub fn root(&self)->&DelegationRef {&self.fence.root}
    pub fn lineage_revision(&self)->u64 {self.lineage_revision}
    pub fn validate_state(&self,state:&AskState,permission_revision:u64)->Result<(),AskError> {
        if state.revision!=self.state_revision || permission_revision!=self.permission_revision {Err(error("stop_plan_stale"))} else {Ok(())}
    }
}

/// Commit the fence and close only the exact selected asks under one lock.
/// Caller has already authenticated root control and per-node visibility.
pub fn apply_stop(state:&mut AskState,plan:StopPlan,permission_revision:u64,now:DateTime<Utc>)->Result<serde_json::Value,AskError>{
    use serde_json::json;
    plan.validate_state(state,permission_revision)?;
    commit_stop_fence(&mut state.lineage,&plan)?;
    let mut outcomes=Vec::new();
    let mut root_result=json!({});
    let mut partial=plan.partial;
    for selection in &plan.selected {
        if !selection.authorized {partial=true;continue;}
        // Each descendant keeps its own stop reason if the root is explicitly
        // reopened later. Resume never removes a fence.
        let key=reference_key(&selection.reference);
        state.lineage.stop_fences.entry(key).or_insert_with(||StopFence{
            root:selection.reference.clone(),descendants:true,..plan.fence.clone()
        });
        if selection.reference.host_id()!=plan.local_host {
            partial=true;
            if selection.readable{outcomes.push(json!({"id":selection.reference.id(),"hostId":selection.reference.host_id(),"phase":"pendingRemote","changed":false,"retryable":false,"reasonCode":"remote_ack_required"}));}
            continue;
        }
        let Some(ask)=state.asks.get(selection.reference.id()).cloned()else{partial=true;continue};
        // Revocation is permanent even for an already-closed root. A later
        // explicit reopen must not revive an old reviewer grant.
        state.routing.cancel_for_delegation(&selection.reference)
            .map_err(|e|AskError::new(e.code(),e.to_string()))?;
        state.routing.close_root_grants(&selection.reference,now)
            .map_err(|e|AskError::new(e.code(),e.to_string()))?;
        if !ask.is_open(){
            if selection.readable{outcomes.push(json!({"id":ask.ask_id,"hostId":plan.local_host,"phase":"alreadyClosed","changed":false,"retryable":false,"reasonCode":null}));}
            continue;
        }
        let result=crate::asks::tools::close_in_state(state,None,&ask.ask_id,"stop",selection.jobs,now);
        if selection.reference==plan.fence.root {
            if let Ok(value)=&result {root_result=value.clone();}
        }
        let (phase,changed,reason)=match result {
            Ok(value)=>{
                if ask.remote.is_some(){partial=true;("pendingRemote",true,None)}
                else if value["stopping"]==true{("pendingInterrupt",true,None)}
                else{("stopped",true,None)}
            },
            Err(error)=>{partial=true;("failed",false,Some(error.code))},
        };
        if let Some(current)=state.asks.get_mut(&ask.ask_id){current.bounded_continuation.cancel("ancestorStopped");}
        state.notice("cutex/delegation/lineageChanged",AskState::ask_sessions(&ask),
            json!({"projectId":ask.project_id,"resourceId":ask.ask_id,"revision":state.lineage.revision}));
        if selection.readable{outcomes.push(json!({"id":ask.ask_id,"hostId":plan.local_host,"phase":phase,"changed":changed,"retryable":false,"reasonCode":reason}));}
    }
    // Preserve the established root stop response (execution, stopping,
    // cancellingJobs) for older MCP clients; cascade fields are additive.
    root_result["nodeOutcomes"]=json!(outcomes);
    root_result["coverage"]=json!(if partial{"partial"}else{"complete"});
    Ok(root_result)
}

/// Call only after the existing explicit reopen authorization succeeds.
/// Descendant fences survive: reopening one task never revives its children.
pub fn explicit_reopen(state:&mut AskState,host:&str,ask_id:&str){
    if let Ok(reference)=DelegationRef::new(host,ask_id){
        if state.lineage.stop_fences.remove(&reference_key(&reference)).is_some(){
            state.lineage.revision=state.lineage.revision.saturating_add(1);
        }
    }
}

/// Authenticated requester/owner entry. No outer `once` wrapper: this function
/// owns the same action namespace and commits its control receipt with effects.
pub fn stop(
    store:&crate::asks::store::AskStore,requester:Option<&str>,ask_id:&str,
    cascade:Cascade,stop_jobs:bool,action:&str,expected_revision:Option<u64>,now:DateTime<Utc>,
)->Result<serde_json::Value,AskError>{
    use serde_json::json;
    crate::asks::routing_policy::integration::with_authority(|roster|{
        let controllers=crate::asks::routing_policy::integration::ControllerView::load(roster)?;
        let caller=requester.unwrap_or("owner");
        let host=crate::asks::routing_policy::integration::local_host_id();
        let input=json!({"id":ask_id,"cascade":cascade,"stopJobs":stop_jobs,"expectedRevision":expected_revision});
        store.with_records(&[ask_id],|state|crate::asks::operations::execute(state,caller,"delegation.stop",action,&input,now,|state|{
            let root=state.asks.get(ask_id).ok_or_else(||error("ask_not_found"))?;
            if let Some(requester)=requester {
                if !controllers.controlled_by(root,requester)?{return Err(error("ask_not_found"));}
            }
            if expected_revision.is_some_and(|revision|revision!=root.revision){return Err(error("revision_conflict"));}
            let root_ref=DelegationRef::new(&host,ask_id).map_err(|_|error("invalid_reference"))?;
            let selected=verified_descendants(&state.lineage,&root_ref,cascade==Cascade::Descendants)?;
            let resources=state.asks.values().filter_map(|ask|{
                let reference=DelegationRef::new(&host,&ask.ask_id).ok()?;
                let visible=requester.is_none_or(|caller|controllers.readable_by(ask,caller));
                let authorized=selected.contains(&reference);
                Some((reference,NodeAccess{visible,available:true,read_attempts:visible,stop:authorized,stop_jobs:authorized,resume_continuation:false}))
            }).collect();
            let scope=ReadScope{caller:caller.into(),project:None,permission_revision:roster.store_revision,resources};
            let plan=prepare_stop(state,&state.lineage,&host,&scope,&root_ref,cascade,stop_jobs,action,now)?;
            let result=apply_stop(state,plan,roster.store_revision,now)?;
            let pending=result["coverage"]!="complete"||result["nodeOutcomes"].as_array().is_some_and(|rows|rows.iter().any(|r|r["phase"]=="pendingRemote"||r["phase"]=="pendingInterrupt"||r["phase"]=="failed"));
            Ok((result,if pending{crate::asks::operations::EffectState::Partial}else{crate::asks::operations::EffectState::Committed}))
        })).map_err(|e|AskError::new("persistence_unavailable",format!("{e:#}")))?
    })
}

pub fn check_admission(state:&AskState,host:&str,ask_id:&str)->Result<(),AskError>{
    let reference=DelegationRef::new(host,ask_id).map_err(|_|error("invalid_reference"))?;
    if let Some(record)=state.lineage.get(&reference){state.lineage.check_creation_fences(record)}
    else if state.lineage.stop_fences.contains_key(&reference_key(&reference)){Err(error("stop_fenced"))}
    else{Ok(())}
}

/// Late child facts remain readable, but cannot automatically wake a stopped
/// parent chain. Only authenticated ancestry is followed; a child's own stop
/// may still notify a live parent. No receipt or delivery state is rewritten.
pub fn stopped_parent(state:&AskState,host:&str,ask_id:&str)->bool {
    let Ok(reference)=DelegationRef::new(host,ask_id) else {return false};
    let mut cursor=state.lineage.get(&reference);
    let mut seen=BTreeSet::new();
    for _ in 0..MAX_LINEAGE_DEPTH {
        let Some(LineageRecord{parent:ParentFact::Verified(parent),..})=cursor else {return false};
        if !seen.insert(parent.reference.clone()) {return true;}
        if state.lineage.stop_fences.contains_key(&reference_key(&parent.reference)) {return true;}
        cursor=state.lineage.get(&parent.reference);
    }
    true
}
