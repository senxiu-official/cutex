//! Owner HTTP entry points for the existing AskStore delegation controls.
//! Agent authority uses authenticated MCP context, never HTTP role parameters.
use std::net::TcpStream;
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde_json::{json,Value};
use crate::asks::{operations::{self,EffectState},project_control as control,routing_policy as routing,
    store::{AskError,AskStore}};
use crate::http::server::{SimpleHttpRequest,write_json_response};

#[derive(Debug,PartialEq,Eq)]
pub enum Route { Tree(String), Attempts(String), Stop(String), Policy(String), Grants(String), Approvals(String), Approval(String), Revoke(String), Decision(String), ResumeContinuation(String), RunControl(String), Pause(String), Resume(String), Operation(String) }
pub fn parse_path(path:&str)->Option<Route>{
    let parts:Vec<_>=path.split('/').collect();
    let valid=|s:&str|!s.is_empty()&&s.len()<=256&&s.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.:".contains(&b));
    match parts.as_slice(){
        ["","v2","projects",id,tail] if valid(id)=>match *tail{
            "delegations"=>Some(Route::Tree((*id).into())),
            "delegation-policy"=>Some(Route::Policy((*id).into())),
            "delegation-grants"=>Some(Route::Grants((*id).into())),
            "delegation-approvals"=>Some(Route::Approvals((*id).into())),
            "run-control"=>Some(Route::RunControl((*id).into())),
            "pause"=>Some(Route::Pause((*id).into())),
            "resume"=>Some(Route::Resume((*id).into())),_=>None},
        ["","v2","delegation-grants",id,"revoke"] if valid(id)=>Some(Route::Revoke((*id).into())),
        ["","v2","delegation-approvals",id,"decision"] if valid(id)=>Some(Route::Decision((*id).into())),
        ["","v2","delegation-approvals",id] if valid(id)=>Some(Route::Approval((*id).into())),
        ["","v2","delegations",id,"resume-continuation"] if valid(id)=>Some(Route::ResumeContinuation((*id).into())),
        ["","v2","delegations",id,"attempts"] if valid(id)=>Some(Route::Attempts((*id).into())),
        ["","v2","delegations",id,"stop"] if valid(id)=>Some(Route::Stop((*id).into())),
        ["","v2","delegation-operations",id] if valid(id)=>Some(Route::Operation((*id).into())),
        _=>None
    }
}
fn invalid(message:impl Into<String>)->AskError{AskError::new("invalid_arguments",message)}
fn unavailable(error:impl std::fmt::Display)->AskError{AskError::new("persistence_unavailable",format!("{error}"))}
fn parse<T:DeserializeOwned>(value:&Value)->Result<T,AskError>{serde_json::from_value(value.clone()).map_err(|e|invalid(e.to_string()))}
fn route_error(error:routing::RoutingError)->AskError{AskError::new(error.code(),error.to_string())}

pub fn owner_context(snapshot:&crate::agent_management::AgentManagementSnapshot,project:&str)->Result<routing::RoutingContext,AskError>{
    routing::integration::trusted_context(snapshot,project,routing::RoutingActor::Owner,&routing::integration::local_host_id())
}

pub fn membership(snapshot:&crate::agent_management::AgentManagementSnapshot,project:&str)->control::Membership{
    let mut members=std::collections::BTreeSet::new();
    for agent in snapshot.agents.values().filter(|a|a.retired_at.is_none()){
        if crate::agent_management::current_project_id(snapshot,agent).is_some_and(|p|p.as_str()==project){members.insert(agent.cutex_session_id.as_str().to_owned());}
    }
    for (id,m) in &snapshot.current_project_memberships{
        if m.project_id.as_ref().is_some_and(|p|p.as_str()==project)
            && !snapshot.agents.get(id).is_some_and(|a|a.retired_at.is_some()) {members.insert(id.as_str().into());}
    }
    let mut excluded=std::collections::BTreeSet::new();
    if let Some(authority)=snapshot.projects.values().find(|a|a.project_id.as_str()==project){
        members.insert(authority.authorized_director_session.as_str().into());
        excluded.insert(authority.authorized_director_session.as_str().into());
    }
    for (_,grants) in snapshot.operator_grants.iter().filter(|(p,_)|p.as_str()==project){
        for id in grants.keys(){if members.contains(id.as_str()){excluded.insert(id.as_str().into());}}
    }
    control::Membership{revision:snapshot.store_revision,members,excluded}
}

fn grant_view(grant:&routing::DelegationGrant)->Value{
    let mut result=serde_json::to_value(grant).expect("grant serializes");
    result.as_object_mut().unwrap().remove("consumption");
    result["allowedActions"]=json!({"revoke":grant.revoked_at.is_none()});result
}

/// Invoked only after the root AND human-principal checks in server.rs.
pub fn handle(stream:&mut TcpStream,request:&SimpleHttpRequest,route:Route)->anyhow::Result<()>{
    let body=if request.body.is_empty(){Ok(json!({}))}else{serde_json::from_slice(&request.body).map_err(|e|invalid(e.to_string()))};
    let result=body.and_then(|body| {
        let store=AskStore::open_default().map_err(unavailable)?;
        if let Route::Attempts(id)=&route{
            if request.method!="GET"{return Err(invalid("GET required"));}
            let history=store.lookup_snapshot(&[id.as_str()]).map_err(unavailable)?;
            return super::delegation_tree::attempts(&history,id,&request.path);
        }
        if let Route::Stop(id)=&route{
            if request.method!="POST"{return Err(invalid("POST required"));}
            #[derive(serde::Deserialize)]
            #[serde(rename_all="camelCase",deny_unknown_fields)]
            struct StopInput{action_id:String,#[serde(default)]cascade:crate::asks::lineage::Cascade,#[serde(default)]stop_jobs:bool,expected_revision:Option<u64>}
            let input:StopInput=parse(&body)?;
            return crate::asks::lineage::stop(&store,None,id,input.cascade,input.stop_jobs,&input.action_id,input.expected_revision,Utc::now());
        }
        if let Route::Operation(action)=&route{
            if request.method!="GET"{return Err(invalid("GET required"));}
            return store.inspect(|s|operations::lookup(s,"owner",action)).map_err(unavailable);
        }
        routing::integration::with_authority(|snapshot|{
            let controllers=routing::integration::ControllerView::load(snapshot)?;
            let project=match &route{
                Route::Tree(p)|Route::Policy(p)|Route::Grants(p)|Route::Approvals(p)|Route::RunControl(p)|Route::Pause(p)|Route::Resume(p)=>p.clone(),
                Route::Revoke(id)=>store.inspect(|s|s.routing.grants.get(id).map(|g|g.project_id.clone())).map_err(unavailable)?
                    .ok_or_else(||AskError::new("grant_not_found","no such grant"))?,
                Route::Decision(id)|Route::Approval(id)=>store.inspect(|s|s.routing.approvals.get(id).map(|a|a.project_id.clone())).map_err(unavailable)?
                    .ok_or_else(||AskError::new("approval_not_found","no such approval"))?,
                Route::ResumeContinuation(id)=>store.inspect(|s|s.asks.get(id).and_then(|a|a.project_id.clone())).map_err(unavailable)?
                    .ok_or_else(||AskError::new("ask_not_found","no such project delegation"))?,
                Route::Operation(_)|Route::Attempts(_)|Route::Stop(_)=>unreachable!(),
            };
            let ctx=owner_context(snapshot,&project)?;
            let host=routing::integration::local_host_id();
            let membership=membership(snapshot,&project);
            let now=Utc::now();
            if request.method=="GET"{
                if matches!(&route,Route::Tree(_)) {
                    // Include retained cold asks and build/cache the owned
                    // projection after releasing the AskStore file guard.
                    let history=store.history_snapshot().map_err(unavailable)?;
                    let directory=crate::asks::directory::Directory::load().ok();
                    return super::delegation_tree::page(&history,&project,snapshot,directory.as_ref(),&request.path);
                }
                let approval_view=|s:&crate::asks::store::AskState,a:&routing::RoutingApproval|{
                    let mut value=serde_json::to_value(a).expect("approval serializes");
                    let actions=s.asks.get(a.delegation_ref.id()).and_then(|ask|routing::integration::request_and_root(s,ask,&host,&controllers).ok())
                        .map(|(request,root)|s.routing.approval_actions(&a.id,&ctx,&request,&root))
                        .unwrap_or(routing::ApprovalActions{approve:false,deny:false});
                    value["allowedActions"]=json!(actions);value
                };
                return store.inspect(|s|match &route{
                    Route::Policy(_)=>{let mut p=serde_json::to_value(s.routing.policy(&project)).unwrap();p["allowedActions"]=json!({"update":true});Ok(p)},
                    Route::Grants(_)=>Ok(json!({"grants":s.routing.grants.values().filter(|g|g.project_id==project).map(grant_view).collect::<Vec<_>>(),"allowedActions":{"create":true}})),
                    Route::Approvals(_)=>{
                        let filter=crate::http::query::query_value(&request.path,"state");
                        if filter.as_deref().is_some_and(|value| !matches!(value,"pending"|"all")) {
                            return Err(invalid("state must be pending or all"));
                        }
                        Ok(json!({"routingApprovals":s.routing.approvals.values()
                            .filter(|a|a.project_id==project)
                            .filter(|a|filter.as_deref()!=Some("pending")||a.state==routing::ApprovalState::Pending)
                            .map(|a|approval_view(s,a)).collect::<Vec<_>>()}))
                    },
                    Route::Approval(id)=>s.routing.approvals.get(id).map(|a|approval_view(s,a)).ok_or_else(||AskError::new("approval_not_found","no such approval")),
                    Route::RunControl(_)=>Ok(control::view_with_membership(s,&ctx,&membership)),
                    _=>Err(invalid("unsupported GET")),
                }).map_err(unavailable)?;
            }
            let action=body.get("actionId").and_then(Value::as_str).ok_or_else(||invalid("actionId required"))?;
            let operation=match (&route,request.method.as_str()){
                (Route::Policy(_),"PUT")=>"policy.update",(Route::Grants(_),"POST")=>"grant.create",
                (Route::Revoke(_),"POST")=>"grant.revoke",(Route::Pause(_),"POST")=>"project.pause",
                (Route::Resume(_),"POST")=>"project.resume",(Route::Decision(_),"POST")=>"approval.decide",
                (Route::ResumeContinuation(_),"POST")=>"continuation.resume",_=>return Err(invalid("unsupported method")),
            };
            let digest_input=json!({"operation":operation,"path":request.path,"body":body});
            // Directory discovery can touch other stores; finish it before
            // acquiring the asks transaction lock.
            let continuation_directory=if matches!(&route,Route::ResumeContinuation(_)){
                Some(crate::asks::directory::Directory::load()?)
            }else{None};
            store.with_state(|s|operations::execute(s,"owner",operation,action,&digest_input,now,|s|{
                match &route{
                    Route::Policy(_)=>{
                        let input=parse(&body)?;
                        let p=s.routing.update_policy(&ctx,&input,now).map_err(route_error)?;
                        Ok((json!({"policy":p}),EffectState::Committed))
                    },
                    Route::Grants(_)=>{
                        let input:routing::CreateGrantRequest=parse(&body)?;
                        if input.root_delegation_ref.host_id()!=host||input.requester_ref.host_id()!=host
                            || input.target_refs.iter().any(|r|r.host_id()!=host){return Err(AskError::new("cross_host_scope_unverified","verified peer grant scope required"));}
                        if !membership.members.contains(input.requester_ref.id())||input.target_refs.iter().any(|r|!membership.members.contains(r.id())){return Err(AskError::new("scope_mismatch","grant participants must be current project members"));}
                        let root=s.asks.get(input.root_delegation_ref.id()).filter(|a|a.project_id.as_deref()==Some(project.as_str())&&a.is_open())
                            .ok_or_else(||AskError::new("root_closed","open project root required"))?;
                        let (_,root)=routing::integration::request_and_root(s,root,&host,&controllers)?;
                        let grant=s.routing.create_grant(&crate::asks::new_id("grant"),&ctx,&input,&root,now).map_err(route_error)?;
                        Ok((json!({"grant":grant_view(&grant)}),EffectState::Committed))
                    },
                    Route::Revoke(id)=>{
                        let grant=s.routing.revoke_grant(id,&ctx,&parse(&body)?,now).map_err(route_error)?;
                        Ok((json!({"grant":grant_view(&grant)}),EffectState::Committed))
                    },
                    Route::Decision(id)=>{
                        let approval=s.routing.approvals.get(id).ok_or_else(||AskError::new("approval_not_found","no such approval"))?;
                        let ask=s.asks.get(approval.delegation_ref.id()).ok_or_else(||AskError::new("ask_not_found","no such delegation"))?;
                        let (routing_request,root)=routing::integration::request_and_root(s,ask,&host,&controllers)?;
                        let decision=s.routing.decide_approval(id,&ctx,&routing_request,&root,&parse(&body)?,now).map_err(route_error)?;
                        let mut recipients=vec![super::services::HOST_EVENT_SESSION_ID.to_owned()];
                        if let Some(director)=&ctx.current_director{recipients.push(director.id().to_owned());}
                        s.notice(routing::integration::ROUTING_CHANGED,recipients,
                            json!({"projectId":project,"resourceId":decision.id,"revision":decision.revision}));
                        Ok((json!({"routingApproval":decision,"delegationId":routing_request.delegation_ref.id()}),EffectState::Committed))
                    },
                    Route::ResumeContinuation(id)=>{
                        let actor=crate::asks::continuation::Actor::Owner;
                        let director=ctx.current_director.as_ref().map(|r|r.id());
                        let authority=crate::asks::continuation::Authority{actor:&actor,current_director:director};
                        let ask=s.asks.get(id).ok_or_else(||AskError::new("ask_not_found","no such delegation"))?;
                        let directory=continuation_directory.as_ref().expect("continuation directory loaded before transaction");
                        let target=directory.get(&ask.target_cutex_session_id);
                        let presence=s.presence.get(&ask.target_cutex_session_id);
                        let guards=crate::asks::continuation::Guards{
                            archived:target.is_none_or(|t|t.archived),
                            paused:crate::asks::pause_admission::is_paused_in(s,snapshot,&ask.target_cutex_session_id),
                            online:presence.is_some_and(|p|p.online)&&!s.owner_offline.contains(&ask.target_cutex_session_id),
                            permission_valid:ctx.management_authority().is_ok(),
                            occurrence_valid:target.is_some_and(|t|ask.current_attempt().is_some_and(|a|a.runtime_generation==t.runtime_generation)),
                            running_turn:presence.is_some_and(|p|p.busy),
                            submission_unknown:ask.bounded_continuation.current.as_ref().is_some_and(|r|r.reconciling),
                            ..Default::default()
                        };
                        let result=crate::asks::continuation::runtime::resume_limited(s,id,authority,&parse(&body)?,guards,now)?;
                        Ok((result,EffectState::Committed))
                    },
                    Route::Pause(_)=>{
                        let input:control::PauseRequest=parse(&body)?;
                        let result=control::pause(s,&ctx,&input,&membership,&host,now)?;
                        let effect=if result["runControl"]["phase"]=="paused"{EffectState::Committed}else{EffectState::Partial};
                        Ok((result,effect))
                    },
                    Route::Resume(_)=>Ok((control::resume(s,&ctx,&parse(&body)?,&host,now)?,EffectState::Committed)),
                    _=>Err(invalid("unsupported mutation")),
                }
            })).map_err(unavailable)?
        })
    });
    match result{
        Ok(value)=>write_json_response(stream,200,"OK",&value),
        Err(error)=>super::server::write_v2_error(stream,409,"Conflict",error.code,&error.detail,false,json!({})),
    }
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn paths_are_exact_and_reads_cannot_mutate(){
        assert_eq!(parse_path("/v2/projects/p/pause"),Some(Route::Pause("p".into())));
        // Deliberately invalid shapes are not route literals for access's
        // source inventory; keep its scan strict for actual endpoints.
        assert!(parse_path(concat!("/v2", "/projects//pause")).is_none());
        assert!(parse_path(concat!("/v2", "/delegation-operations/a/extra")).is_none());
    }
}
