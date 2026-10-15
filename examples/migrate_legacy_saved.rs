use anyhow::{ensure, Context};
use cutex::{launch::local_deployment::LocalDeployment, session::store::*};
use fs2::FileExt;
use std::{fs, path::PathBuf};
fn main() -> anyhow::Result<()> {
 let args:Vec<String>=std::env::args().collect();
 ensure!(args.len()==4,"usage: migrate_legacy_saved EXACT_ID PROFILE PRIVATE_EVIDENCE_DIR");
 let (id,profile,out)=(&args[1],&args[2],PathBuf::from(&args[3]));
 ensure!(out.is_dir(),"evidence directory must exist");
 let root=cutex::config::paths::runtime_dir()?;
 let lock=fs::OpenOptions::new().read(true).write(true).open(root.join("agent-management/v1/agent-management-mutation-v1.lock"))?;
 FileExt::try_lock_exclusive(&lock).context("management operation active; retry later")?;
 let initial=load_cutex_session_store()?;
 let original=initial.sessions.get(id).context("unknown exact ID")?.clone();
 if original.explicit_launch.is_some() && original.formal_agent_name.is_none() {
  let committed:serde_json::Value=serde_json::from_slice(&fs::read(out.join("record-committed.json"))?)?;
  ensure!(committed==serde_json::to_value(&original)?,"committed target changed; inspect");
  let mut latest=initial;
  let name=original.display_name_hint.as_deref().context("explicit formal name required")?;
  cutex::session::service::set_cutex_session_display_name_by_key(&mut latest,id,name)?;
  save_cutex_session_store(&latest)?;
  fs::write(out.join("record-named.json"),serde_json::to_vec_pretty(latest.sessions.get(id).unwrap())?)?;
  println!("Completed formal name from verified existing display name: {name}");return Ok(());
 }
 ensure!(original.runtime_backend==cutex::session::model::CutexSessionRuntimeBackend::CuteAlden && original.explicit_launch.is_none(),"not an unmigrated cute_alden record");
 ensure!(!original.is_retired() && original.pending_launch_id.is_none() && original.alden_pid.is_none(),"archived or pending/live legacy runtime");
 let backup=out.join("record-before.json");
 ensure!(!backup.exists(),"evidence already exists; inspect previous attempt before retry");
 fs::write(&backup,serde_json::to_vec_pretty(&original)?)?;
 let mut candidate=original.clone();
 candidate.profile=Some(profile.clone());
 candidate.formal_agent_name=Some(original.formal_agent_name.clone().or(original.display_name_hint.clone()).context("explicit formal name required")?);
 LocalDeployment::selected()?.context("no selected light runtime")?.adopt(&mut candidate,&initial)?;
 candidate.alden_session_name=None;
 candidate.bump_durable_revision()?;
 fs::write(out.join("record-prepared.json"),serde_json::to_vec_pretty(&candidate)?)?;
 for attempt in 0..5 {
  let mut latest=load_cutex_session_store()?;
  ensure!(serde_json::to_value(latest.sessions.get(id))?==serde_json::to_value(Some(&original))?,"target changed; stop for inspection");
  latest.sessions.insert(id.clone(),candidate.clone());
  match save_cutex_session_store(&latest) {
   Ok(())=>{fs::write(out.join("record-committed.json"),serde_json::to_vec_pretty(&candidate)?)?;println!("Migrated offline: {id}. Native identity/history retained.");return Ok(());},
   Err(e) if e.downcast_ref::<CutexSessionStoreRevisionConflict>().is_some() && attempt<4 =>std::thread::sleep(std::time::Duration::from_millis(100)),
   Err(e)=>return Err(e),
  }
 }
 unreachable!()
}
