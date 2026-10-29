//! Bounded ephemeral query snapshots. These contain no business authority;
//! losing a snapshot invalidates its cursor and never executes an action.
use std::collections::BTreeSet;
use std::sync::{Mutex,OnceLock};
use std::time::{Duration,Instant};
use serde_json::{Value,json};
use crate::asks::{lineage::*,store::{AskError,AskState}};

struct Cached {
    project:String,root:Option<DelegationRef>,expires:Instant,
    snapshot:TreeSnapshot,cursors:BTreeSet<String>,
}
static CACHE:OnceLock<Mutex<Vec<Cached>>>=OnceLock::new();

pub fn page(state:&AskState,project:&str,authority:&crate::agent_management::AgentManagementSnapshot,directory:Option<&crate::asks::directory::Directory>,full_path:&str)->Result<Value,AskError>{
    let authority_revision=authority.store_revision;
    let query=|key:&str|crate::http::query::query_value(full_path,key);
    let host=crate::asks::routing_policy::integration::local_host_id();
    let limit=query("limit").map(|s|s.parse::<usize>()).transpose().map_err(|_|error("invalid_limit"))?.unwrap_or(100);
    if !(1..=200).contains(&limit){return Err(error("invalid_limit"));}
    let root=match (query("rootId"),query("rootHostId")){
        (None,None)=>None,(Some(id),host_id)=>Some(DelegationRef::new(host_id.as_deref().unwrap_or(&host),id).map_err(|_|error("invalid_reference"))?),
        _=>return Err(error("invalid_reference")),
    };
    let mut cache=CACHE.get_or_init(||Mutex::new(Vec::new())).lock().map_err(|_|error("query_unavailable"))?;
    cache.retain(|s|s.expires>Instant::now());
    if let Some(cursor)=query("cursor"){
        let snapshot=cache.iter_mut().find(|s|s.project==project&&s.root==root&&s.cursors.contains(&cursor)).ok_or_else(||error("cursor_invalidated"))?;
        let result=snapshot.snapshot.page("owner",authority_revision,Some(&cursor),limit)?;
        if let Some(next)=result["nextCursor"].as_str(){snapshot.cursors.insert(next.into());}
        return Ok(result);
    }
    let resources=state.asks.values().map(|ask|{
        let reference=DelegationRef::new(&host,&ask.ask_id).map_err(|_|error("invalid_reference"))?;
        let target=directory.and_then(|directory|directory.get(&ask.target_cutex_session_id));
        let resume_continuation=ask.is_open()&&ask.bounded_continuation.current.as_ref().is_some_and(|c|
                c.state==crate::asks::continuation::Phase::Limited&&!c.reconciling&&c.running_attempt.is_none()
                &&c.wake.as_ref().is_none_or(|w|matches!(w.submission,crate::asks::continuation::Submission::Unclaimed))
                &&c.limit_notice.as_ref().is_none_or(|n|n.enqueued))
            &&target.is_some_and(|target|!target.archived&&target.agent_enabled
                &&ask.current_attempt().is_some_and(|a|a.runtime_generation==target.runtime_generation))
            &&state.presence.get(&ask.target_cutex_session_id).is_some_and(|p|p.online&&!p.busy)
            &&!state.owner_offline.contains(&ask.target_cutex_session_id)
            &&!state.leases.contains_key(&ask.target_cutex_session_id)
            &&!ask.bounded_continuation.current.as_ref().is_some_and(|c|c.reconciling)
            &&!crate::asks::pause_admission::is_paused_in(state,authority,&ask.target_cutex_session_id)
            &&crate::asks::lineage::check_admission(state,&host,&ask.ask_id).is_ok();
        Ok((reference,NodeAccess{visible:true,read_attempts:true,available:true,stop:ask.is_open(),stop_jobs:ask.is_open(),resume_continuation}))
    }).collect::<Result<_,AskError>>()?;
    let scope=ReadScope{caller:"owner".into(),project:Some(project.into()),permission_revision:authority_revision,resources};
    let mut snapshot=TreeSnapshot::capture(state,&state.lineage,&host,&scope,root.as_ref())?;
    let result=snapshot.page("owner",authority_revision,None,limit)?;
    if let Some(next)=result["nextCursor"].as_str(){
        if cache.len()>=16 {cache.remove(0);}
        cache.push(Cached{project:project.into(),root,expires:Instant::now()+Duration::from_secs(120),snapshot,cursors:[next.into()].into()});
    }
    Ok(result)
}

pub fn attempts(state:&AskState,id:&str,full_path:&str)->Result<Value,AskError>{
    let ask=state.asks.get(id).ok_or_else(||error("ask_not_found"))?;
    let cursor=crate::http::query::query_value(full_path,"cursor").map(|s|s.parse::<usize>()).transpose().map_err(|_|error("invalid_cursor"))?.unwrap_or(0);
    if cursor>ask.attempts.len(){return Err(error("invalid_cursor"));}
    let end=(cursor+100).min(ask.attempts.len());
    let items:Vec<_>=ask.attempts[cursor..end].iter().map(|a|json!({
        "attemptId":a.attempt_id(id),"attemptNumber":a.n,"startedAt":a.started_at,"endedAt":a.ended_at,
        "cause":a.continuation.as_ref().map(|c|&c.reason),"actionId":null,
    })).collect();
    Ok(json!({"items":items,"nextCursor":if end<ask.attempts.len(){Some(end.to_string())}else{None}}))
}

fn error(code:&'static str)->AskError{AskError::new(code,"delegation query precondition failed")}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tree_cursor_keeps_its_snapshot_and_rejects_changed_authority() {
        let mut state=AskState::default();
        let project=format!("tree-{}",uuid::Uuid::new_v4().simple());
        for action in ["first","second"] {
            let mut request=crate::asks::store::tests::new_ask(action,"work");
            request.project_id=Some(project.clone());
            state.create_ask(request,chrono::Utc::now()).unwrap();
        }
        let mut authority=crate::agent_management::AgentManagementSnapshot::empty();
        let before=state.clone();
        let first=page(&state,&project,&authority,None,"/tree?limit=1").unwrap();
        let cursor=first["nextCursor"].as_str().expect("second page");
        let path=format!("/tree?limit=1&cursor={cursor}");
        state.asks.clear(); // A later read must not mix new state into this page.
        let second=page(&state,&project,&authority,None,&path).unwrap();
        assert_eq!(second["nodes"].as_array().unwrap().len(),1);
        assert_ne!(first["nodes"][0],second["nodes"][0]);
        authority.store_revision+=1;
        assert!(page(&state,&project,&authority,None,&path).is_err());
        assert_eq!(before.asks.len(),2);
        assert!(state.asks.is_empty(),"query does not restore or mutate business records");
    }
}
