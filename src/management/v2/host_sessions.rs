//! Bounded Human catalog for one host; never exports runtime configuration.
use crate::{
    http::server::{write_json_response, SimpleHttpRequest},
    management::server::ManagementRequestContext,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::TcpStream;
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    #[serde(default)]
    query: String,
    cursor: Option<String>,
    limit: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSession {
    #[serde(default)]
    pub provider: Option<crate::profiles::appearance::Provider>,
    #[serde(default)]
    pub profile_style: Option<crate::profiles::appearance::TextStyle>,
    pub id: String,
    pub name: String,
    pub kind: String,
    pub host_id: String,
    pub profile: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub project_name: Option<String>,
    pub cwd: String,
    pub state: String,
    pub generation: u64,
    pub native_id: Option<String>,
    /// Exact running artifact version observed on the host that owns this
    /// record. Older management peers may omit these additive fields.
    #[serde(default)]
    pub runtime_version: Option<String>,
    #[serde(default)]
    pub runtime_version_latest: Option<String>,
    #[serde(default)]
    pub runtime_version_recorded_at: Option<String>,
}

pub(super) fn exact_id(path: &str) -> Option<String> {
    let encoded=path.split('?').next()?.strip_prefix("/v2/host/sessions/")?;
    if encoded.is_empty() || encoded.len()>480 || encoded.contains('/') { return None; }
    super::server::decode_path_segment(encoded).filter(|id|id.len()<=160 && !id.chars().any(char::is_control))
}

pub(super) fn handle_exact(stream:&mut TcpStream,request:&SimpleHttpRequest,context:ManagementRequestContext)->anyhow::Result<()> {
    let id=exact_id(&request.path).expect("exact host route dispatch validated its ID");
    let value=exact_value_at(&crate::session::store::cutex_sessions_path()?,
        &crate::platform::host::current_host_name(),&id,context.load_runtime_status)?;
    write_json_response(stream,200,"OK",&value)
}

fn exact_value_at(path:&std::path::Path,host:&str,id:&str,load:crate::management::server::ManagementRuntimeStatusLoader)->anyhow::Result<serde_json::Value> {
    let mut store=crate::session::model::CutexSessionStore::default();
    if let Some(record)=crate::session::store::host_session_row_at(path,host,id)? {
        store.sessions.insert_shared(record.cutex_session_id.clone(),record);
    }
    let native_versions=crate::launch::version_registry::load_relaxed();
    let mut value=project(&store,host,&Query::default(),load,native_versions.as_ref());
    annotate_projects(&mut value);
    Ok(value)
}
pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
) -> anyhow::Result<()> {
    let query: Query = match serde_json::from_slice(&request.body) {
        Ok(q) => q,
        Err(_) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_query",
                "Expected query, cursor and limit",
                false,
                json!({}),
            )
        }
    };
    if query.query.len() > 256 || query.cursor.as_ref().is_some_and(|s| s.len() > 160) {
        return super::server::write_v2_error(
            stream,
            400,
            "Bad Request",
            "invalid_query",
            "Query too long",
            false,
            json!({}),
        );
    }
    let host = crate::platform::host::current_host_name();
    let (records, next) = crate::session:: store::host_session_page_at(
        & crate::session::store::cutex_sessions_path()?,
        & host,
        query.cursor.as_deref(),
        &query.query,
        query.limit.unwrap_or(50),
    )?;
    let mut store = crate::session::model::CutexSessionStore::default();
    for record in records {
        store
            .sessions
            .insert_shared(record.cutex_session_id.clone(), record);
    }
    let native_versions = crate::launch::version_registry::load_relaxed();
    let mut value=project(&store,&host,&query,context.load_runtime_status,native_versions.as_ref());
    value["nextCursor"] = json!(next);
    annotate_projects(&mut value);
    write_json_response(stream,200,"OK",&value)
}

fn annotate_projects(value:&mut serde_json::Value) {
    if value["data"].as_array().is_none_or(|rows|rows.is_empty()) { return; }
    if let Ok(snapshot)=crate::agent_management::AgentManagementStore::open_default().and_then(|s|s.directory_snapshot_shared().map_err(anyhow::Error::new)) {
        if let Some(rows)=value["data"].as_array_mut(){for row in rows {
            if let Some(agent)=row["id"].as_str().and_then(|id|snapshot.agent_by_session(id)) {
                if let Some(id)=crate::agent_management::current_project_id(&snapshot,agent) {
                    let presentation=crate::agent_management::effective_presentation(&id,snapshot.project_presentations.get(&id),);
                    row["projectId"]=json!(id.as_str());row["projectName"]=json!(presentation.display_name);
                }
            }
        }}
    }
}
fn project(
    store:&crate::session::model::CutexSessionStore,
    host:&str,
    query:&Query,
    load:crate::management::server::ManagementRuntimeStatusLoader,
    native_versions:Option<&crate::launch::version_registry::NativeVersionRegistry>,
)->serde_json::Value {
    let needle = query.query.to_lowercase();
    let mut records: Vec<_> = store
        .sessions
        .values()
        .filter(|r| {
            r.is_active()
                && !r.is_retired()
                && (r.agent_enabled || r.is_owned_session())
                && crate::runtime::lifecycle::cutex_session_host_is_local(&r.host_id, &host)
        })
        .filter(|r| {
            query
                .cursor
                .as_ref()
                .is_none_or(|c| r.cutex_session_id.as_str() > c.as_str())
        })
        .filter(|r| {
            r.cutex_session_id.to_lowercase().contains(&needle)
                || crate::session::metadata::cutex_session_presented_name(r)
                    .to_lowercase()
                    .contains(&needle)
        })
        .collect();
    records.sort_by(|a, b| a.cutex_session_id.cmp(&b.cutex_session_id));
    records.dedup_by_key(|r| r.cutex_session_id.clone());
    // Desktop requests 200 rows; keep the small default while bounding explicit batches.
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let more = records.len() > limit;
    records.truncate(limit);
    let next = if more {
        records.last().map(|r| r.cutex_session_id.clone())
    } else {
        None
    };
    let profile_styles=crate::profiles::appearance::catalog();
    let data: Vec<_> = records
        .into_iter()
        .map(|r| {
            let observation = (load)(&r.cutex_session_id)
                .ok()
                .flatten();
            let state = if observation
                .as_ref()
                .is_some_and(|s| s.connected && s.runtime_generation == r.runtime_generation)
            {
                "Online"
            } else if r.runtime_pid.is_none()
                && r.app_server_runtime.is_none()
                && r.app_server_launch_claim_id.is_none()
            {
                "Offline"
            } else {
                "Unobserved"
            };
            let runtime_version = native_versions.and_then(|registry| {
                r.app_server_runtime
                    .as_ref()
                    .and_then(|binding| registry.runtime_version_for_pid(binding.pid))
                    .or_else(|| {
                        r.runtime_pid
                            .and_then(|pid| registry.runtime_version_for_pid(pid))
                    })
                    .or_else(|| {
                        r.explicit_launch
                            .as_ref()
                            .and_then(|contract| { registry.runtime_version_for_launch_contract(contract)
                        })
                    })
            });
            HostSession {
                provider:r.profile.as_ref().and_then(|p|profile_styles.get(p).map(|v|v.0)),
                profile_style:r.profile.as_ref().and_then(|p|profile_styles.get(p).and_then(|v|v.1.clone())),
                id: r.cutex_session_id.clone(),
                name: crate::session::metadata::cutex_session_presented_name(r)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect(),
                kind: if r.is_owned_session() {
                    "Session"
                } else {
                    "Agent"
                }
                .into(),
                host_id: host.to_owned(),
                profile: r.profile.clone(),
                project_id:None,project_name:None,
                cwd: crate::session::service::cutex_session_launch_cwd(r)
                    .chars()
                    .take(1024)
                    .collect(),
                state: state.into(),
                generation: r.runtime_generation,
                native_id: r.codex_session_id.clone(),
                runtime_version,
                runtime_version_latest: native_versions
                    .map(|registry| registry.latest_runtime.version.clone()),
                runtime_version_recorded_at: native_versions
                    .map(|registry| registry.latest_runtime.recorded_at.clone()),
            }
        })
        .collect();
    json!({"schema":"cutex/host-sessions/v1","hostId":host,"data":data,"nextCursor":next})
}
#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn exact_id_route_is_owner_heavy_bounded_and_not_a_substring_or_map_alias() {
  let _home_lock=crate::config::paths::test_home_env_lock();
  use crate::session::{model::{CutexSessionRecord,CutexSessionStore,CutexSessionArchiveState},store::*};
  let root=std::env::temp_dir().join(format!("c2-exact-{}",uuid::Uuid::new_v4()));std::fs::create_dir_all(&root).unwrap();
  let path=root.join("cutex-sessions.json");let host="local-test";
  let mut store=CutexSessionStore::default();
  for id in ["cutex.target","cutex.target-more","cutex.hidden","cutex.foreign","cutex.retired"] {
   let mut row=CutexSessionRecord::new_at(id.into(),None,host.into(),"/synthetic".into(),None,"2026-11-04T00:00:00Z".into()).unwrap();row.agent_enabled=id!="cutex.hidden";
   if id=="cutex.foreign" {row.host_id="foreign-host".into();}
   if id=="cutex.retired" {row.archive_state=CutexSessionArchiveState::Retired;}
   let key=if id=="cutex.target" {"imported-map-key"} else if id=="cutex.target-more" {"cutex.target"} else {id};
   store.sessions.insert(key.into(),row);
  }
  save_cutex_session_store_to_path(&path,&store).unwrap();
  let schema:serde_json::Value=serde_json::from_str(include_str!("schema/export/host-session.json")).unwrap();let validator=jsonschema::validator_for(&schema).unwrap();
  let row=exact_value_at(&path,host,"cutex.target", |_|Ok(None)).unwrap();
  assert!(validator.is_valid(&row));assert_eq!(row["data"].as_array().unwrap().len(),1);assert_eq!(row["data"][0]["id"],"cutex.target");assert!(row["nextCursor"].is_null());
  for id in ["cutex.targe","cutex.TARGET","cutex.missing","cutex.hidden","cutex.foreign","cutex.retired","imported-map-key"] {
   let value=exact_value_at(&path,host,id, |_|panic!("ineligible/missing ID must not load runtime")).unwrap();assert!(validator.is_valid(&value));assert!(value["data"].as_array().unwrap().is_empty(),"{id}");
  }
  assert_eq!(exact_id("/v2/host/sessions/cutex%2Etarget").as_deref(),Some("cutex.target"));
  for path in ["/v2/host/sessions/","/v2/host/sessions/a/b","/v2/host/sessions/a%2Fb","/v2/host/sessions/a%00","/v2/host/sessions/%ZZ"] {assert!(exact_id(path).is_none(),"{path}");}
  let route="/v2/host/sessions/cutex.target";
  assert_eq!(super::super::access::classify("GET",route),Some(super::super::access::Class::OwnerOnly));
  let request=crate::http::server::SimpleHttpRequest {method:"GET".into(),path:route.into(),headers:Default::default(),body:Vec::new()};
  assert_eq!(super::super::control_capacity::classify(&request),super::super::control_capacity::WorkClass::HeavyRead);
  std::fs::remove_dir_all(root).unwrap();
 }
 #[test] fn catalog_pages_exclude_foreign_records_and_runtime_secrets(){
  let mut store=crate::session::model::CutexSessionStore::default();
  for i in 0..206 {
   let id=format!("cutex.{}",uuid::Uuid::new_v4());
   let mut r=crate::session::model::CutexSessionRecord::new(id.clone(),None,if i==205{"foreign"}else{"local-test"}.into(),"/work".into(),Some("preset".into()),).unwrap();
   r.agent_enabled=true;r.formal_agent_name=Some(format!("Worker-{i}"));r.thread_name=Some("SECRET_HISTORY_SENTINEL".into());store.sessions.insert(id,r);
  }
  let default_page=project(&store,"local-test",&Query::default(), |_|Ok(None),None);
  assert_eq!(default_page["data"].as_array().unwrap().len(),50);
  let desktop_page=project(&store,"local-test",&Query{limit:Some(200),..Default::default()}, |_|Ok(None),None,);
  assert_eq!(desktop_page["data"].as_array().unwrap().len(),200);
  let first=project(&store,"local-test",&Query{limit:Some(5000),..Default::default()}, |_|Ok(None),None,);
  assert_eq!(first["data"].as_array().unwrap().len(),200);assert!(!first.to_string().contains("SECRET_HISTORY_SENTINEL"));
  let second=project(&store,"local-test",&Query{cursor:first["nextCursor"].as_str().map(str::to_owned),..Default::default()}, |_|Ok(None),None,);assert_eq!(second["data"].as_array().unwrap().len(),5);assert!(second["nextCursor"].is_null());
  let one=project(&store,"local-test",&Query{query:"WORKER-54".into(),..Default::default()}, |_|Ok(None),None,);assert_eq!(one["data"].as_array().unwrap().len(),1);assert_eq!(one["data"][0]["name"],"Worker-54");
 }

 #[test] fn host_sessions_report_additive_runtime_versions_and_accept_older_peers(){
  let mut store=crate::session::model::CutexSessionStore::default();
  let id="cutex.runtime-version";
  let mut record=crate::session::model::CutexSessionRecord::new(id.to_string(),None,"local-test".into(),"/work".into(),Some("preset".into()),).unwrap();
  record.agent_enabled=true;
  record.runtime_pid=Some(std::process::id());
  store.sessions.insert(id.to_string(),record);
  let version=|value:&str|crate::launch::version_registry::NativeVersionRecord{
   version:value.into(),recorded_at:"2026-09-28T00:00:00+10:00".into(),
  };
  // Resolve the real test process portably; Windows selects by executable digest,
  // while Linux also supports the exact runtime-path projection.
  let executable=std::env::current_exe().unwrap();
  let digest=crate::agent_management::file_sha256(&executable).unwrap();
  let registry=crate::launch::version_registry::NativeVersionRegistry{
   schema:crate::launch::version_registry::REGISTRY_SCHEMA.into(),
   latest_cli:version("0.157.1-r45"),
   latest_runtime:version("0.157.1-r45"),
   runtime_artifacts:std::collections::BTreeMap::from([(
    digest.as_str().to_string(),version("0.157.1-r44"),
   )]),
   runtime_paths:std::collections::BTreeMap::from([(
    executable.to_str().unwrap().to_string(),
    version("0.157.1-r44"),
   )]),
  };
  let value=project(&store,"local-test",&Query::default(),|_|Ok(None),Some(&registry),);
  assert_eq!(value["data"][0]["runtimeVersion"],"0.157.1-r44");
  assert_eq!(value["data"][0]["runtimeVersionLatest"],"0.157.1-r45");
  let old_peer=r#"{
   "id":"cutex.old-peer","name":"Old peer","kind":"Agent","hostId":"eva",
   "cwd":"/work","state":"Online","generation":1
  }"#;
  let parsed:HostSession=serde_json::from_str(old_peer).unwrap();
  assert!(parsed.runtime_version.is_none());
  assert!(parsed.runtime_version_latest.is_none());
 }
}
