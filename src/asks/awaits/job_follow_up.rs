//! Administrative job outcomes are not physical completions or model wakeups.
use std::{collections::BTreeMap,path::PathBuf,sync::{Mutex,OnceLock},time::{Duration,Instant}};
use chrono::{DateTime,Utc};
use super::{AskState,AskStore,JobTerminal,now_rfc3339};

const POLL_INTERVAL: Duration=Duration::from_secs(30);
const POLL_BATCH: usize=4;

pub fn job_follow_up_ended(state:&mut AskState,job_id:&str,revision:u64,now:DateTime<Utc>)->usize {
    let ids:Vec<_>=state.asks.values().filter(|ask|ask.is_open() && ask.remote.is_none()
        && ask.job(job_id).is_some_and(|job|job.terminal.is_none()))
        .map(|ask|ask.ask_id.clone()).collect();
    for id in &ids {
        state.update_ask(id,now,|ask|{
            if let Some(job)=ask.jobs.iter_mut().find(|job|job.job_id==job_id && job.terminal.is_none()) {
                job.terminal=Some(JobTerminal{status:"unconfirmed".into(),exit_code:None,ok:false,
                    event_id:format!("job-follow-up-ended:{job_id}:{revision}"),at:now_rfc3339(now)});
            }
        });
        super::wake(state,id,now);
    }
    ids.len()
}

/// The root Job bridge calls this only after a newly committed disposition.
/// No completion event or runtime operation is generated, even for offline agents.
pub fn job_follow_up_ended_default(job_id:&str,revision:u64) {
    let result=(||->anyhow::Result<()>{
        let store=AskStore::open_default()?;
        if store.peek()?.is_none(){return Ok(());}
        let keys=store.project_snapshot(|s|s.asks.values().filter(|a|a.is_open()&&a.job(job_id).is_some()).map(|a|a.ask_id.clone()).collect::<Vec<_>>())?;
        if keys.is_empty(){return Ok(());}
        store.with_records(&keys.iter().map(String::as_str).collect::<Vec<_>>(),|s|job_follow_up_ended(s,job_id,revision,Utc::now()))?;
        crate::management::v2::asks::publish_pending_notices(&store);
        Ok(())
    })();
    if let Err(error)=result {eprintln!("asks: job follow-up settlement failed: {error:#}");}
}

type PollKey=(PathBuf,String,String);
fn attempts()->&'static Mutex<BTreeMap<PollKey,Instant>> {
    static ATTEMPTS:OnceLock<Mutex<BTreeMap<PollKey,Instant>>>=OnceLock::new();
    ATTEMPTS.get_or_init(||Mutex::new(BTreeMap::new()))
}

/// Bounded read-only compensation for a missed hook / daemon restart. The
/// runtime adapter uses the root Job socket, not a model tool or an agent turn.
pub fn poll_follow_up_ended(runtime:&dyn crate::asks::scheduler::AskRuntime,store:&AskStore,now:DateTime<Utc>)->anyhow::Result<()> {
    let state=store.shared()?;
    let mut jobs=std::collections::BTreeSet::new();
    for ask in state.asks.values().filter(|a|a.is_open()&&a.remote.is_none()) {
        if let Some(wait)=&ask.awaiting {
            for id in &wait.jobs {
                if ask.job(id).is_some_and(|job|job.terminal.is_none()) {jobs.insert((ask.target_cutex_session_id.clone(),id.clone()));}
            }
        }
    }
    if jobs.is_empty(){return Ok(());}
    let started=Instant::now();
    let selected={
        let mut last=attempts().lock().map_err(|_|anyhow::anyhow!("job query throttle poisoned"))?;
        last.retain(|_,at|started.duration_since(*at)<Duration::from_secs(900));
        let mut due=jobs.into_iter().filter_map(|(session,id)|{
            let at=last.get(&(store.dir().to_path_buf(),session.clone(),id.clone())).copied();
            at.is_none_or(|at|started.duration_since(at)>=POLL_INTERVAL).then_some((at,session,id))
        }).collect::<Vec<_>>();
        // Unqueried and oldest attempts first: early not-found records must
        // not starve later waits when a failed query ends this pass.
        due.sort();
        due.into_iter().take(POLL_BATCH).map(|(_,session,id)|(session,id)).collect::<Vec<_>>()
    };
    for (session,id) in selected {
        if started.elapsed()>=Duration::from_millis(100){break;}
        attempts().lock().map_err(|_|anyhow::anyhow!("job query throttle poisoned"))?
            .insert((store.dir().to_path_buf(),session.clone(),id.clone()),Instant::now());
        let job=match runtime.query_awaited_job(&session,&id) {Ok(job)=>job,Err(_)=>break};
        if job["jobId"].as_str()!=Some(id.as_str()) || job["request"]["subscriberCutexSessionId"].as_str()!=Some(session.as_str()) {continue;}
        if job["state"].as_str()!=Some("unconfirmed") || job["terminalReason"].as_str()!=Some("followUpEnded"){continue;}
        let Some(revision)=job["revision"].as_u64() else{continue};
        let keys:Vec<&str>=state.asks.values().filter(|a|a.is_open()&&a.target_cutex_session_id==session&&a.job(&id).is_some())
            .map(|a|a.ask_id.as_str()).collect();
        store.with_records(&keys,|state|{
            // Recheck that an authenticated binding to this session still
            // exists after the socket read; never settle a replacement wait.
            if state.asks.values().any(|a|a.is_open()&&a.target_cutex_session_id==session&&a.job(&id).is_some()) {
                job_follow_up_ended(state,&id,revision,now);
            }
        })?;
    }
    Ok(())
}
