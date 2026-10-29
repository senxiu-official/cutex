//! Project pause reasons share the existing whole-agent admission gate.
//! Membership/authority snapshots are supplied under the roster mutation fence.
use std::collections::{BTreeMap, BTreeSet};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use super::{store::{AskError, AskState}, routing_policy::{RoutingContext, RoutingActorAudit}};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub enum Mode { #[default] AfterTurn, Interrupt }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub enum Scope { #[default] Members, All }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
pub struct PauseRequest {
    pub action_id: String,
    pub expected_epoch: u64,
    #[serde(default)] pub mode: Mode,
    #[serde(default)] pub scope: Scope,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
pub struct ResumeRequest { pub action_id: String, pub pause_id: String, pub expected_epoch: u64 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct RunControl {
    pub project_id: String,
    pub epoch: u64,
    pub pause_id: Option<String>,
    pub requested_by: Option<RoutingActorAudit>,
    pub requested_at: Option<DateTime<Utc>>,
    pub mode: Mode,
    pub scope: Scope,
    pub membership_revision: u64,
    pub excluded_session_ids: BTreeSet<String>,
    /// Sticky reasons: moved-out members remain paused until this ID resumes.
    pub members: BTreeSet<String>,
    #[serde(default)] pub interrupt_unknown: BTreeSet<String>,
}
impl RunControl {
    fn initial(project: &str) -> Self {
        Self { project_id:project.into(), epoch:0,pause_id:None,requested_by:None,
            requested_at:None,mode:Mode::AfterTurn,scope:Scope::Members,
            membership_revision:0,excluded_session_ids:BTreeSet::new(),members:BTreeSet::new(),interrupt_unknown:BTreeSet::new() }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct ProjectControlState { pub projects: BTreeMap<String,RunControl> }

pub struct Membership {
    pub revision: u64,
    pub members: BTreeSet<String>,
    pub excluded: BTreeSet<String>,
}

/// Request-local projection only: never installs pause reasons, queues an
/// interrupt, bumps an epoch, or clones AskState. Sticky history is retained.
pub fn project(control:&RunControl,membership:&Membership)->RunControl {
    let mut projected=control.clone();
    if projected.pause_id.is_some() {
        projected.membership_revision=membership.revision;
        projected.excluded_session_ids=if projected.scope==Scope::Members{membership.excluded.clone()}else{BTreeSet::new()};
        projected.members.extend(membership.members.iter().cloned());
    }
    projected
}

/// Build once per query from the roster already loaded outside AskStore.
pub fn project_snapshot(state:&AskState,roster:&crate::agent_management::AgentManagementSnapshot)->ProjectControlState {
    ProjectControlState{projects:state.project_control.projects.iter().map(|(id,control)|{
        let projected=if control.pause_id.is_some(){project(control,&crate::management::v2::delegations::membership(roster,id))}else{control.clone()};
        (id.clone(),projected)
    }).collect()}
}

pub fn reasons_in(projects:&ProjectControlState,session:&str)->Vec<Value> {
    projects.projects.values().filter_map(|p|{
        p.pause_id.as_ref().filter(|_|p.members.contains(session)&&!p.excluded_session_ids.contains(session))
            .map(|id|json!({"kind":"project","projectId":p.project_id,"pauseId":id,"epoch":p.epoch}))
    }).collect()
}

pub fn is_paused_in(projects:&ProjectControlState,session:&str)->bool {
    projects.projects.values().any(|p| p.pause_id.is_some()
        && p.members.contains(session) && !p.excluded_session_ids.contains(session))
}

/// Fresh pure gate, without even copying RunControl on the runtime hot path.
pub fn is_paused_with_roster(state:&AskState,session:&str,roster:&crate::agent_management::AgentManagementSnapshot)->bool {
    state.project_control.projects.values().filter(|p|p.pause_id.is_some()).any(|p|{
        let current=crate::management::v2::delegations::membership(roster,&p.project_id);
        (p.scope==Scope::All||!current.excluded.contains(session))
            && (p.members.contains(session)||current.members.contains(session))
    })
}

pub fn is_paused(state: &AskState, session: &str) -> bool {
    is_paused_in(&state.project_control,session)
}

pub fn reasons(state: &AskState, session: &str) -> Vec<Value> {
    reasons_in(&state.project_control,session)
}

fn allowed(ctx:&RoutingContext, scope:Scope) -> bool {
    ctx.management_authority().is_ok() && (scope != Scope::All || ctx.actor.is_owner())
}

fn notify_changed(state:&mut AskState,ctx:&RoutingContext,members:&BTreeSet<String>,epoch:u64) {
    let mut recipients=members.clone();
    if let Some(director)=&ctx.current_director {recipients.insert(director.id().into());}
    recipients.insert(crate::management::v2::services::HOST_EVENT_SESSION_ID.into());
    state.notice("cutex/project/runControlChanged",recipients.into_iter().collect(),
        json!({"projectId":ctx.project_id,"epoch":epoch}));
}

pub fn view(state:&AskState,ctx:&RoutingContext)->Value {
    let initial=RunControl::initial(&ctx.project_id);
    let control=state.project_control.projects.get(&ctx.project_id).unwrap_or(&initial);
    view_control(state,ctx,control)
}

pub fn view_with_membership(state:&AskState,ctx:&RoutingContext,membership:&Membership)->Value {
    let initial=RunControl::initial(&ctx.project_id);
    let control=project(state.project_control.projects.get(&ctx.project_id).unwrap_or(&initial),membership);
    view_control(state,ctx,&control)
}

/// GET outcomes use the same projected member set as view/reasons and never
/// claim that a read changed an agent or dispatched an interrupt.
pub fn outcomes_with_membership(state:&AskState,ctx:&RoutingContext,membership:&Membership,host_id:&str,now:DateTime<Utc>)->Vec<Value> {
    let initial=RunControl::initial(&ctx.project_id);
    let control=project(state.project_control.projects.get(&ctx.project_id).unwrap_or(&initial),membership);
    control.members.difference(&control.excluded_session_ids).map(|id|{
        let (phase,reason)=if control.pause_id.is_some(){paused_status(state,id,&control)}else{remaining_status(state,id)};
        member_outcome(id,host_id,phase,reason,now,false)
    }).collect()
}

fn view_control(state:&AskState,ctx:&RoutingContext,control:&RunControl)->Value {
    let pending=control.members.difference(&control.excluded_session_ids).any(|s|member_busy(state,s));
    let unknown=control.members.difference(&control.excluded_session_ids).any(|s|
        state.presence.get(s).is_none() || (member_busy(state,s)
            && (control.interrupt_unknown.contains(s) || (control.mode==Mode::Interrupt && occurrence(state,s).is_none()))));
    let phase=if control.pause_id.is_none(){"running"}else if unknown{"partial"}else if pending{"pausing"}else{"paused"};
    let mut value=serde_json::to_value(control).expect("run control serializes");
    value.as_object_mut().unwrap().remove("members");
    value["phase"]=json!(phase);
    value["allowedActions"]=json!({"pause":allowed(ctx,Scope::Members),"pauseAll":ctx.actor.is_owner(),"resume":allowed(ctx,control.scope)});
    value
}

/// `host_id` and `now` are prepared by the authenticated caller before taking
/// AskStore. No host detection, activity store read, or clock sampling here.
pub fn pause(state:&mut AskState,ctx:&RoutingContext,input:&PauseRequest,membership:&Membership,host_id:&str,now:DateTime<Utc>)->Result<Value,AskError>{
    if !allowed(ctx,input.scope){return Err(AskError::new("forbidden","current Director or owner required; all scope is owner-only"));}
    let mut control=state.project_control.projects.get(&ctx.project_id).cloned().unwrap_or_else(||RunControl::initial(&ctx.project_id));
    if control.epoch!=input.expected_epoch{return Err(AskError::new("epoch_conflict","refresh current run control"));}
    if control.pause_id.is_some(){return Err(AskError::new("already_paused","resume the current pause before replacing it"));}
    control.epoch=control.epoch.checked_add(1).ok_or_else(||AskError::new("epoch_exhausted","epoch exhausted"))?;
    control.pause_id=Some(super::new_id("pause"));
    control.requested_by=Some(ctx.actor.audit());control.requested_at=Some(now);
    control.mode=input.mode;control.scope=input.scope;control.membership_revision=membership.revision;
    control.interrupt_unknown.clear();
    control.excluded_session_ids=if input.scope==Scope::Members{membership.excluded.clone()}else{BTreeSet::new()};
    control.members=membership.members.difference(&control.excluded_session_ids).cloned().collect();
    let members=control.members.clone();
    let epoch=control.epoch;
    state.project_control.projects.insert(ctx.project_id.clone(),control);
    for id in &members {state.presence.entry(id.clone()).or_default().paused=true;}
    if input.mode==Mode::Interrupt {
        queue_interrupts(state,&ctx.project_id,now);
    }
    notify_changed(state,ctx,&members,epoch);
    let control=&state.project_control.projects[&ctx.project_id];
    Ok(json!({"runControl":view(state,ctx),"memberOutcomes":members.iter().map(|id|{
        let (phase,reason)=paused_status(state,id,control);
        member_outcome(id,host_id,phase,reason,now,true)
    }).collect::<Vec<_>>()}))
}

fn member_outcome(session:&str,host:&str,phase:&str,reason:Option<&str>,now:DateTime<Utc>,changed:bool)->Value {
    json!({"sessionId":session,"hostId":host,"phase":phase,"reasonCode":reason,
        "lastActivityAt":null,"observedAt":super::now_rfc3339(now),"changed":changed,"retryable":false})
}

fn paused_status(state:&AskState,session:&str,control:&RunControl)->(&'static str,Option<&'static str>) {
    if member_busy(state,session) {
        if control.mode==Mode::AfterTurn {return ("pendingTurnEnd",Some("project_pause_active"));}
        if control.interrupt_unknown.contains(session) {return ("unknown",Some("interrupt_outcome_unknown"));}
        if occurrence(state,session).is_none() {return ("unknown",Some("runtime_occurrence_unavailable"));}
        return ("pendingInterrupt",Some("project_pause_active"));
    }
    if !state.presence.contains_key(session) {return ("unknown",Some("presence_unavailable"));}
    ("paused",Some("project_pause_active"))
}

/// Removing one reason does not imply a running/idle runtime or open gate.
/// Keep remaining projects, individual pauses and the older delegation pause.
fn remaining_status(state:&AskState,session:&str)->(&'static str,Option<&'static str>) {
    let mut project_status=None;
    for control in state.project_control.projects.values().filter(|p|p.pause_id.is_some()
        &&p.members.contains(session)&&!p.excluded_session_ids.contains(session)) {
        let status=paused_status(state,session,control);
        if status.0=="unknown" {return status;}
        if project_status.is_none()||status.0=="pendingInterrupt" {project_status=Some(status);}
    }
    if let Some(status)=project_status {return status;}
    if state.presence.get(session).is_some_and(|p|p.agent_pause.is_some()) {
        return (if member_busy(state,session){"pendingTurnEnd"}else{"paused"},Some("agent_pause_active"));
    }
    if state.pauses.contains_key(session) {
        return (if member_busy(state,session){"pendingTurnEnd"}else{"paused"},Some("delegation_pause_active"));
    }
    ("unchanged",None)
}

pub fn resume(state:&mut AskState,ctx:&RoutingContext,input:&ResumeRequest,host_id:&str,now:DateTime<Utc>)->Result<Value,AskError>{
    let control=state.project_control.projects.get(&ctx.project_id).ok_or_else(||AskError::new("pause_not_found","no matching pause"))?;
    if !allowed(ctx,control.scope){return Err(AskError::new("forbidden","no authority to resume this scope"));}
    if control.epoch!=input.expected_epoch||control.pause_id.as_deref()!=Some(input.pause_id.as_str()){
        return Err(AskError::new("epoch_conflict","pause ID or epoch changed"));
    }
    let epoch=control.epoch.checked_add(1).ok_or_else(||AskError::new("epoch_exhausted","epoch exhausted"))?;
    let members=control.members.clone();
    let control=state.project_control.projects.get_mut(&ctx.project_id).unwrap();
    control.epoch=epoch;control.pause_id=None;control.members.clear();
    for id in &members {
        let paused=state.pauses.contains_key(id)||is_paused(state,id);
        if let Some(p)=state.presence.get_mut(id){p.paused=paused||p.agent_pause.is_some();}
    }
    notify_changed(state,ctx,&members,epoch);
    Ok(json!({"runControl":view(state,ctx),"memberOutcomes":members.iter().map(|id|{
        let (phase,reason)=remaining_status(state,id);
        member_outcome(id,host_id,phase,reason,now,true)
    }).collect::<Vec<_>>(),"interruptedDelegations":[]}))
}

/// Run under the same authority fence before admitting a member after roster
/// changes. Retain moved-out members; current Director/operator are excluded.
pub fn reconcile(state:&mut AskState,project:&str,membership:&Membership){
    let Some(control)=state.project_control.projects.get_mut(project).filter(|p|p.pause_id.is_some())else{return};
    *control=self::project(control,membership);
    // Keep the affected-member history through temporary role exclusions.
    // Demotion or moving out must not silently erase this pause's reason.
    let members=control.members.clone();
    for id in members {
        let project_reason=state.project_control.projects.values().any(|p|
            p.pause_id.is_some()&&p.members.contains(&id)&&!p.excluded_session_ids.contains(&id));
        let other_reason=state.pauses.contains_key(&id)||state.presence.get(&id).is_some_and(|p|p.agent_pause.is_some());
        state.presence.entry(id).or_default().paused=project_reason||other_reason;
    }
    queue_interrupts(state,project,Utc::now());
}

/// Late joiners use the same exact-occurrence command as the initial pause.
/// interrupt_unknown is a durable dispatch claim, not a guess about missing
/// presence. Missing proof must not pre-claim an interrupt we have not sent.
fn queue_interrupts(state:&mut AskState,project:&str,now:DateTime<Utc>) {
    let Some(control)=state.project_control.projects.get(project)
        .filter(|p|p.pause_id.is_some()&&p.mode==Mode::Interrupt) else {return};
    let pause_id=control.pause_id.clone().unwrap();
    let occurrences:Vec<_>=control.members.difference(&control.excluded_session_ids)
        .filter(|id|!control.interrupt_unknown.contains(*id)).filter_map(|id|{
            occurrence(state,id).map(|(generation,turn)|(id.clone(),generation,turn))
        }).collect();
    for (session,generation,turn_id) in occurrences {
        super::control::queue_command(state,super::store::RuntimeCommand::ProjectPauseInterrupt{
            project_id:project.into(),pause_id:pause_id.clone(),cutex_session_id:session,runtime_generation:generation,turn_id,
        },now);
    }
}

fn member_busy(state:&AskState,session:&str)->bool {
    state.presence.get(session).is_some_and(|p|p.busy)
        || state.leases.get(session).is_some_and(|l|l.turn_id.is_some())
}

fn occurrence(state:&AskState,session:&str)->Option<(u64,String)> {
    let seen=state.presence.get(session).filter(|p|p.busy).and_then(|p|
        Some((p.runtime_generation.or(p.lifecycle_generation)?,p.turn_id.clone()?)));
    // The command checks actual runtime activity again; a reserved/unbound
    // lease never counts as an actual running turn.
    seen.or_else(||state.leases.get(session).and_then(|l|Some((l.runtime_generation,l.turn_id.clone()?))))
}

#[cfg(test)]
mod tests{
    use super::*;
    use crate::asks::routing_policy::RoutingActor;
    fn assert_outcome_contract(value:&Value,host:&str,now:DateTime<Utc>,changed:bool) {
        let expected:BTreeSet<_>=["sessionId","hostId","phase","reasonCode","lastActivityAt","observedAt","changed","retryable"].into();
        assert_eq!(value.as_object().unwrap().keys().map(String::as_str).collect::<BTreeSet<_>>(),expected);
        assert_eq!(value["hostId"],host);
        assert!(value["lastActivityAt"].is_null());
        assert_eq!(value["observedAt"],super::super::now_rfc3339(now));
        assert_eq!(value["changed"],changed);
        assert_eq!(value["retryable"],false);
        assert!(value["reasonCode"].is_null()||value["reasonCode"].is_string());
    }

    #[test]
    fn mutation_and_query_outcomes_have_exact_contract_fields_and_honest_observations() {
        let now="2026-10-27T08:00:00Z".parse().unwrap();
        let ctx=RoutingContext{project_id:"p".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
        let mut s=AskState::default();
        s.presence.insert("busy".into(),super::super::store::Presence{online:true,busy:true,runtime_generation:Some(3),turn_id:Some("turn-3".into()),..Default::default()});
        s.presence.insert("missing-proof".into(),super::super::store::Presence{online:true,busy:true,..Default::default()});
        s.presence.insert("idle".into(),super::super::store::Presence{online:true,..Default::default()});
        let membership=Membership{revision:1,members:["busy".into(),"missing-proof".into(),"idle".into()].into(),excluded:BTreeSet::new()};
        let result=pause(&mut s,&ctx,&PauseRequest{action_id:"pause".into(),expected_epoch:0,mode:Mode::Interrupt,scope:Scope::Members},&membership,"canonical-host",now).unwrap();
        let outcomes=result["memberOutcomes"].as_array().unwrap();
        for value in outcomes {assert_outcome_contract(value,"canonical-host",now,true);}
        assert!(outcomes.iter().any(|v|v["sessionId"]=="busy"&&v["phase"]=="pendingInterrupt"));
        assert!(outcomes.iter().any(|v|v["sessionId"]=="missing-proof"&&v["phase"]=="unknown"&&v["reasonCode"]=="runtime_occurrence_unavailable"));
        assert!(outcomes.iter().any(|v|v["sessionId"]=="idle"&&v["phase"]=="paused"));
        let later=now+chrono::Duration::seconds(5);
        s.project_control.projects.get_mut("p").unwrap().interrupt_unknown.insert("busy".into());
        let before=serde_json::to_vec(&s).unwrap();
        let projected=outcomes_with_membership(&s,&ctx,&membership,"canonical-host",later);
        for value in &projected {assert_outcome_contract(value,"canonical-host",later,false);}
        assert!(projected.iter().any(|v|v["sessionId"]=="busy"&&v["phase"]=="unknown"&&v["reasonCode"]=="interrupt_outcome_unknown"));
        assert_eq!(serde_json::to_vec(&s).unwrap(),before);
    }

    #[test]
    fn resume_reports_other_active_reasons_instead_of_claiming_every_member_released() {
        let now="2026-10-27T08:00:00Z".parse().unwrap();
        let ctx=RoutingContext{project_id:"p".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
        let mut s=AskState::default();
        let membership=Membership{revision:1,members:["project".into(),"individual".into(),"delegation".into(),"free".into()].into(),excluded:BTreeSet::new()};
        let result=pause(&mut s,&ctx,&PauseRequest{action_id:"pause-p".into(),expected_epoch:0,mode:Mode::AfterTurn,scope:Scope::Members},&membership,"canonical-host",now).unwrap();
        let other=RoutingContext{project_id:"q".into(),..ctx.clone()};
        pause(&mut s,&other,&PauseRequest{action_id:"pause-q".into(),expected_epoch:0,mode:Mode::AfterTurn,scope:Scope::Members},
            &Membership{revision:1,members:["project".into()].into(),excluded:BTreeSet::new()},"canonical-host",now).unwrap();
        super::super::agent_pause::pause(&mut s,"individual","owner",None,now);
        super::super::control::owner_pause(&mut s,"delegation",now);
        let later=now+chrono::Duration::seconds(5);
        let resumed=resume(&mut s,&ctx,&ResumeRequest{action_id:"resume-p".into(),pause_id:result["runControl"]["pauseId"].as_str().unwrap().into(),expected_epoch:1},"canonical-host",later).unwrap();
        let outcomes=resumed["memberOutcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(),4);
        for value in outcomes {assert_outcome_contract(value,"canonical-host",later,true);}
        for (session,reason) in [("project","project_pause_active"),("individual","agent_pause_active"),("delegation","delegation_pause_active")] {
            assert!(outcomes.iter().any(|v|v["sessionId"]==session&&v["phase"]=="paused"&&v["reasonCode"]==reason));
            assert!(s.presence[session].paused);
        }
        assert!(outcomes.iter().any(|v|v["sessionId"]=="free"&&v["phase"]=="unchanged"&&v["reasonCode"].is_null()));
        assert!(!s.presence["free"].paused);
        assert!(resumed["interruptedDelegations"].as_array().unwrap().is_empty());
    }
    #[test]
    fn pause_and_resume_invalidate_members_current_director_and_owner_with_minimal_payload(){
        use crate::asks::routing_policy::RoutingRef;
        let mut s=AskState::default();
        let ctx=RoutingContext{project_id:"p".into(),authority_epoch:1,
            current_director:Some(RoutingRef::new("local","director-1").unwrap()),
            actor:RoutingActor::Owner,operator_may_route:false};
        let result=pause(&mut s,&ctx,&PauseRequest{action_id:"pause".into(),expected_epoch:0,mode:Mode::AfterTurn,scope:Scope::Members},
            &Membership{revision:1,members:["director-1".into(),"member".into()].into(),excluded:["director-1".into()].into()},"test-host",Utc::now()).unwrap();
        assert!(!is_paused(&s,"director-1"));
        let notice=s.notices.last().unwrap();
        assert_eq!(notice.method,"cutex/project/runControlChanged");
        assert_eq!(notice.sessions.iter().cloned().collect::<BTreeSet<_>>(),
            ["member".into(),"director-1".into(),crate::management::v2::services::HOST_EVENT_SESSION_ID.into()].into());
        assert_eq!(notice.params,json!({"projectId":"p","epoch":1}));
        // After a handover the new current Director gets invalidation even
        // when neither Director was ever among the affected paused members.
        let director=RoutingRef::new("local","director-2").unwrap();
        let next=RoutingContext{authority_epoch:2,current_director:Some(director.clone()),
            actor:RoutingActor::Agent{reference:director,name:None},..ctx};
        let request=ResumeRequest{action_id:"resume".into(),pause_id:result["runControl"]["pauseId"].as_str().unwrap().into(),expected_epoch:1};
        resume(&mut s,&next,&request,"test-host",Utc::now()).unwrap();
        let notice=s.notices.last().unwrap();
        assert_eq!(notice.sessions.iter().cloned().collect::<BTreeSet<_>>(),
            ["member".into(),"director-2".into(),crate::management::v2::services::HOST_EVENT_SESSION_ID.into()].into());
        assert_eq!(notice.params,json!({"projectId":"p","epoch":2}));
        let count=s.notices.len();
        assert!(resume(&mut s,&next,&request,"test-host",Utc::now()).is_err());
        assert_eq!(s.notices.len(),count);
    }
    #[test]
    fn defaults_exclude_director_and_resume_preserves_other_reasons(){
        let mut s=AskState::default();
        let ctx=RoutingContext{project_id:"p".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
        let m=Membership{revision:1,members:["director".into(),"member".into()].into(),excluded:["director".into()].into()};
        let req:PauseRequest=serde_json::from_value(json!({"actionId":"a","expectedEpoch":0})).unwrap();
        let r=pause(&mut s,&ctx,&req,&m,"test-host",Utc::now()).unwrap();
        assert!(!is_paused(&s,"director"));assert!(is_paused(&s,"member"));
        super::super::agent_pause::pause(&mut s,"member","owner",None,Utc::now());
        let resume_req=ResumeRequest{action_id:"r".into(),pause_id:r["runControl"]["pauseId"].as_str().unwrap().into(),expected_epoch:1};
        resume(&mut s,&ctx,&resume_req,"test-host",Utc::now()).unwrap();
        assert!(super::super::agent_pause::is_paused(&s,"member"));
        assert!(resume(&mut s,&ctx,&resume_req,"test-host",Utc::now()).is_err());
        assert!(!s.presence["member"].online);
    }

    #[test]
    fn read_projection_includes_fresh_join_current_exclusions_and_sticky_history_without_writes(){
        let mut s=AskState::default();
        let ctx=RoutingContext{project_id:"p".into(),authority_epoch:0,current_director:None,actor:RoutingActor::Owner,operator_may_route:false};
        pause(&mut s,&ctx,&PauseRequest{action_id:"pause".into(),expected_epoch:0,mode:Mode::AfterTurn,scope:Scope::Members},
            &Membership{revision:1,members:["moved".into(),"promoted".into()].into(),excluded:BTreeSet::new()},"test-host",Utc::now()).unwrap();
        s.presence.get_mut("moved").unwrap().busy=true;
        let before=serde_json::to_vec(&s).unwrap();
        let membership=Membership{revision:2,members:["joined".into(),"promoted".into()].into(),excluded:["promoted".into()].into()};
        let current=project(&s.project_control.projects["p"],&membership);
        assert!(current.members.contains("moved"));
        assert!(current.members.contains("joined"));
        assert!(current.members.contains("promoted"));
        assert_eq!(current.epoch,1);assert_eq!(current.membership_revision,2);
        let projected=ProjectControlState{projects:[("p".into(),current)].into()};
        assert_eq!(reasons_in(&projected,"joined").len(),1);
        assert_eq!(reasons_in(&projected,"moved").len(),1);
        assert!(reasons_in(&projected,"promoted").is_empty());
        assert!(reasons_in(&projected,"unrelated").is_empty());
        assert_eq!(view_with_membership(&s,&ctx,&membership)["phase"],"partial");
        let outcomes=outcomes_with_membership(&s,&ctx,&membership,"test-host",Utc::now());
        assert_eq!(outcomes.len(),2);
        assert!(outcomes.iter().any(|v|v["sessionId"]=="joined"&&v["phase"]=="unknown"));
        assert!(outcomes.iter().any(|v|v["sessionId"]=="moved"&&v["phase"]=="pendingTurnEnd"));
        assert!(outcomes.iter().all(|v|v["changed"]==false));
        assert_eq!(serde_json::to_vec(&s).unwrap(),before);
        // Losing the role exclusion restores the retained reason even after
        // moving out. Scope=all ignores current role exemptions altogether.
        let demoted=Membership{revision:3,members:BTreeSet::new(),excluded:BTreeSet::new()};
        assert!(project(&projected.projects["p"],&demoted).members.contains("promoted"));
        let mut all=projected.projects["p"].clone();all.scope=Scope::All;
        assert!(project(&all,&membership).excluded_session_ids.is_empty());
    }
}
