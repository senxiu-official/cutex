//! Owner-only bounded catalog keys, optional public projections, and changes.
use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::session::store::{session_changes_at, session_page_at, SessionChanges};
use crate::store::{ChangeCursor, PageBudget};
use anyhow::Context;
use serde_json::json;
use std::net::TcpStream;

pub const CAPABILITY: &str = "session-deltas-v1";
pub const HOST_CAPABILITY: &str = "session-deltas-host-v1";
pub const RESOURCES_CAPABILITY: &str = "session-deltas-resources-v1";

fn with_host_metadata(mut value: serde_json::Value) -> serde_json::Value {
    value["hostId"] = json!(crate::platform::host::current_host_name());
    // Keep this identical to the legacy list's authority, including future
    // additive fields. An empty feed still observes a host runtime selection.
    value["claudeRuntime"] = super::claude_runtime_api::host_selected_json();
    value
}

pub(super) fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest,
    context: crate::management::server::ManagementRequestContext,
    repository: &super::repository::EventRepository,
) -> anyhow::Result<()> {
    match resource_body(&request.path, Some((context.load_registry, context.load_runtime_status, repository))).map(with_host_metadata) {
        Ok(value) => write_json_response(stream, 200, "OK", &value),
        Err(error) => match crate::store::StoreError::from(error) {
            crate::store::StoreError::Invalid(message) => super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_session_cursor",
                &message,
                false,
                json!({}),
            ),
            crate::store::StoreError::Busy => super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "host_busy",
                "Session storage is busy; retry the bounded read",
                true,
                json!({}),
            ),
            crate::store::StoreError::Resync(reason) => write_json_response(
                stream,
                200,
                "OK",
                &with_host_metadata(json!({"resyncRequired":true,"reason":format!("{reason:?}")})),
            ),
            crate::store::StoreError::Other(error) => Err(error),
            error => Err(error.into()),
        },
    }
}

#[cfg(test)]
fn resource(path: &str) -> anyhow::Result<serde_json::Value> {
    resource_body(path, None).map(with_host_metadata)
}

fn resource_body(path: &str, projection: Option<(crate::management::server::ManagementRegistryLoader, super::session::RuntimeStatusLoader,
    &super::repository::EventRepository)>) -> anyhow::Result<serde_json::Value> {
    let (delta, budget, cursor, retired, include) = (|| -> anyhow::Result<_> {
        anyhow::ensure!(path.len() <= 8192, "session query too long");
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let delta = route == "/v2/sessions/changes";
        let mut budget = if delta {
            PageBudget::DELTA_DEFAULT
        } else {
            PageBudget::LIST_DEFAULT
        };
        let mut cursor = None;
        let mut retired = false;
        let mut include = std::collections::HashSet::new();
        let mut seen = std::collections::HashSet::new();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            anyhow::ensure!(seen.insert(key.to_string()), "duplicate query parameter");
            match key.as_ref() {
                "include" if !delta => {
                    for field in value.split(',') {
                        anyhow::ensure!(matches!(field, "resource" | "hostRow") && include.insert(field.to_string()), "invalid include");
                    }
                }
                "cursor" => cursor = Some(value.into_owned()),
                "limit" => budget.max_rows = value.parse()?,
                "maxBytes" => budget.max_bytes = value.parse()?,
                "lifecycle" if !delta => {
                    retired = match value.as_ref() {
                        "active" => false,
                        "retired" => true,
                        _ => anyhow::bail!("invalid lifecycle"),
                    }
                }
                _ => anyhow::bail!("unknown query parameter"),
            }
        }
        anyhow::ensure!(
            budget.max_rows > 0
                && budget.max_rows <= 1000
                && budget.max_bytes > 0
                && budget.max_bytes <= 1024 * 1024,
            "invalid session query budget"
        );
        anyhow::ensure!(!delta || cursor.is_some(), "cursor required");
        Ok((delta, budget, cursor, retired, include))
    })()
    .map_err(|error| crate::store::StoreError::Invalid(error.to_string()))?;
    let path = crate::session::store::cutex_sessions_path()?;
    if delta {
        let cursor = ChangeCursor::decode(
            crate::store::manifest::spec("session-store"),
            cursor.as_deref().context("cursor required")?,
        )?;
        return Ok(match session_changes_at(&path, &cursor, budget)? {
            Some(SessionChanges::Page {
                changes,
                cursor,
                watermark,
                more,
                oversize_first,
            }) => {
                json!({"changes":changes,"nextCursor":cursor.encode(),"watermark":watermark,"more":more,"oversizeFirst":oversize_first,"resyncRequired":false})
            }
            Some(SessionChanges::Resync { reason, watermark }) => {
                json!({"resyncRequired":true,"reason":format!("{reason:?}"),"watermark":watermark})
            }
            None => json!({"resyncRequired":true,"reason":"StoreAbsent"}),
        });
    }
    Ok(
        match session_page_at(&path, retired, cursor.as_deref(), budget)? {
            Some(page) => {
                let mut rows: Vec<_> = page.rows.iter().zip(&page.revisions).map(|(r,rev)|json!({"cutexSessionId":r.cutex_session_id,"revision":rev})).collect();
                if !include.is_empty() {
                    let (load_registry, load_runtime_status, repository) = projection.context("catalog projection context required")?;
                    if include.contains("resource") {
                        let registry = load_registry()?;
                        for (row, resource) in rows.iter_mut().zip(super::session::catalog_resources(&page.rows, &registry, load_runtime_status, repository)?) {
                            row["resource"] = resource;
                        }
                    }
                    if include.contains("hostRow") {
                        let hosts = super::host_sessions::catalog_rows(&page.rows, load_runtime_status);
                        let by_id: std::collections::HashMap<_,_> = hosts["data"].as_array().unwrap().iter().map(|row| (row["id"].as_str().unwrap(), row)).collect();
                        for row in &mut rows {
                            row["hostRow"] = by_id.get(row["cutexSessionId"].as_str().unwrap()).map(|v| (*v).clone()).unwrap_or(serde_json::Value::Null);
                        }
                    }
                }
                json!({"sessions":rows,
            "nextCursor":page.next,"changeCursor":page.change_cursor.encode(),"oversizeFirst":page.oversize_first})
            }
            None => {
                json!({"sessions":[],"nextCursor":null,"changeCursor":null,"oversizeFirst":false})
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn comparable(mut resource: serde_json::Value) -> serde_json::Value {
        resource.as_object_mut().unwrap().remove("observedAt");
        resource
    }
    #[test]
    fn included_projections_match_exact_reads_and_preserve_key_only_pages() {
        let _home_lock = crate::config::paths::test_home_env_lock();
        use crate::store::{ApplyBatch, Expect, KeyedStore, Scope};
        let path = crate::session::store::cutex_sessions_path().unwrap();
        let repo = crate::session::store::SessionRepository::open(&path).unwrap();
        let id = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut record = crate::session::model::CutexSessionRecord::new_at(id.clone(), None,
            crate::platform::host::current_host_name(), "/synthetic".into(), None,
            "2026-11-04T00:00:00Z".into()).unwrap();
        record.agent_enabled = true;
        repo.apply(ApplyBatch::new(Scope::Host).put(id.clone(), std::sync::Arc::new(record), Expect::Absent)).unwrap();
        let events = super::super::repository::management_v2_repository().unwrap();
        let registry = crate::im::registry::ImRegistry::default();
        let value = resource_body("/v2/sessions/page?include=resource,hostRow",
            Some((|| Ok(crate::im::registry::ImRegistry::default()), |_| Ok(None), events))).unwrap();
        let row = value["sessions"].as_array().unwrap().iter().find(|r| r["cutexSessionId"] == id).unwrap();
        let exact = super::super::session::session_resource_including_hidden(&id, &registry, |_| Ok(None), events).unwrap().unwrap();
        assert_eq!(comparable(row["resource"].clone()), comparable(exact));
        let host = super::super::host_sessions::exact_value_at(&path,
            &crate::platform::host::current_host_name(), &id, |_| Ok(None)).unwrap();
        assert_eq!(row["hostRow"], host["data"][0]);
        let schema: serde_json::Value = serde_json::from_str(include_str!("schema/export/session-deltas.json")).unwrap();
        assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&with_host_metadata(value)));
        let foreign_id = format!("cutex.{}", uuid::Uuid::new_v4());
        let foreign = crate::session::model::CutexSessionRecord::new_at(foreign_id.clone(), None,
            "foreign-host".into(), "/synthetic".into(), None, "2026-11-04T00:00:00Z".into()).unwrap();
        repo.apply(ApplyBatch::new(Scope::Host).put(foreign_id.clone(), std::sync::Arc::new(foreign), Expect::Absent)).unwrap();
        let value = resource_body("/v2/sessions/page?include=resource,hostRow",
            Some((|| Ok(crate::im::registry::ImRegistry::default()), |_| Ok(None), events))).unwrap();
        let row = value["sessions"].as_array().unwrap().iter().find(|r| r["cutexSessionId"] == foreign_id).unwrap();
        assert!(row["resource"].is_null() && row["hostRow"].is_null());
        crate::session::store::update_session_row(&path, &id, |r| {
            r.archive_state = crate::session::model::CutexSessionArchiveState::Retired; Ok(())
        }).unwrap();
        let value = resource_body("/v2/sessions/page?lifecycle=retired&include=resource,hostRow",
            Some((|| Ok(crate::im::registry::ImRegistry::default()), |_| panic!("retired row must not load runtime"), events))).unwrap();
        let row = value["sessions"].as_array().unwrap().iter().find(|r| r["cutexSessionId"] == id).unwrap();
        assert_eq!(comparable(row["resource"].clone()), comparable(super::super::session::retired_session_resource(&id).unwrap().unwrap()));
        assert!(row["hostRow"].is_null());
        let keys = resource("/v2/sessions/page").unwrap();
        assert!(keys["sessions"].as_array().unwrap().iter().all(|row| row.get("resource").is_none() && row.get("hostRow").is_none()));
        for query in ["include=", "include=raw", "include=resource,resource", "include=resource&include=hostRow"] {
            assert!(resource(&format!("/v2/sessions/page?{query}")).is_err());
        }
        assert!(resource("/v2/sessions/changes?cursor=a&include=resource").is_err());
    }
    #[test]
    fn real_catalog_delta_and_resync_responses_match_the_exported_schema() {
        let _home_lock=crate::config::paths::test_home_env_lock();
        use crate::store::{ApplyBatch,Expect,KeyedStore,Scope};
        let schema:serde_json::Value=serde_json::from_str(include_str!("schema/export/session-deltas.json")).unwrap();
        let validator=jsonschema::validator_for(&schema).unwrap();
        let path=crate::session::store::cutex_sessions_path().unwrap();
        let repo=crate::session::store::SessionRepository::open(&path).unwrap();
        let id=format!("cutex.{}",uuid::Uuid::new_v4());
        let row=crate::session::model::CutexSessionRecord::new_at(id.clone(),None,crate::platform::host::current_host_name(),"/synthetic".into(),None,"2026-11-04T00:00:00Z".into()).unwrap();
        repo.apply(ApplyBatch::new(Scope::Host).put(id.clone(),std::sync::Arc::new(row),Expect::Absent)).unwrap();
        let page=resource("/v2/sessions/page?limit=1&maxBytes=1").unwrap();
        assert!(validator.is_valid(&page),"{page}");assert_eq!(page["oversizeFirst"],true);
        let cursor=page["changeCursor"].as_str().unwrap();
        crate::session::store::update_session_row(&path,&id,|row|{row.note=Some("updated".into());Ok(())}).unwrap();
        let changes=resource(&format!("/v2/sessions/changes?cursor={cursor}")).unwrap();
        assert!(validator.is_valid(&changes),"{changes}");assert_eq!(changes["changes"][0]["key"],id);
        let mut wrong=ChangeCursor::decode(crate::store::manifest::spec("session-store"),cursor).unwrap();
        wrong.instance="another-instance".into();
        let resync=resource(&format!("/v2/sessions/changes?cursor={}",wrong.encode())).unwrap();
        assert!(validator.is_valid(&resync),"{resync}");assert_eq!(resync["reason"],"WrongInstance");
    }
    #[test]
    fn empty_reads_observe_the_same_claude_host_authority_as_the_legacy_list() {
        let _home_lock=crate::config::paths::test_home_env_lock();
        use crate::runtime::claude::ClaudeDeployment;
        let root=crate::config::paths::config_dir().unwrap().join(format!("c2-host-{}",uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        struct Restore(std::path::PathBuf,Option<Vec<u8>>);
        impl Drop for Restore {fn drop(&mut self) {if let Some(bytes)=&self.1 {std::fs::write(&self.0,bytes).unwrap();} else {let _=std::fs::remove_file(&self.0);}}}
        let selected=crate::runtime::claude::deployment_path().unwrap();
        let _restore=Restore(selected.clone(),std::fs::read(&selected).ok());
        let executable=root.join("synthetic.exe");std::fs::write(&executable,[]).unwrap();
        let path=crate::session::store::cutex_sessions_path().unwrap();
        let _repo=crate::session::store::SessionRepository::open(&path).unwrap();
        let page=resource("/v2/sessions/page").unwrap();
        let cursor=page["changeCursor"].as_str().unwrap();
        for label in ["claude-r901","claude-r902"] {
            let adapter=root.join(label);
            for file in ["src/cute_claude/__main__.py","scripts/connect_tui.py"] {
                let file=adapter.join(file);std::fs::create_dir_all(file.parent().unwrap()).unwrap();std::fs::write(file,[]).unwrap();
            }
            ClaudeDeployment {python:executable.clone(),adapter_source:adapter,claude_cli:executable.clone(),claude_home:root.clone(),tui_binary:executable.clone()}.install().unwrap();
            let authority=super::super::claude_runtime_api::host_selected_json();
            assert_eq!(authority["selectedRelease"],label);
            assert_eq!(resource("/v2/sessions/page").unwrap()["claudeRuntime"],authority);
            let delta=resource(&format!("/v2/sessions/changes?cursor={cursor}")).unwrap();
            assert_eq!(delta["claudeRuntime"],authority);
            assert!(delta["changes"].as_array().unwrap().is_empty());
            assert_eq!(delta["nextCursor"],cursor,"host metadata refresh needs no session mutation");
            assert_eq!(delta["hostId"],crate::platform::host::current_host_name());
        }
    }
    #[test]
    fn cursor_feed_is_owner_only_and_queries_are_bounded() {
        for route in ["/v2/sessions/page", "/v2/sessions/changes"] {
            assert_eq!(
                super::super::access::classify("GET", route),
                Some(super::super::access::Class::OwnerOnly)
            );
        }
        for query in [
            "cursor=a&cursor=b",
            "unknown=true",
            "lifecycle=all",
            "limit=NaN",
        ] {
            assert!(resource(&format!("/v2/sessions/page?{query}")).is_err());
        }
        assert!(resource("/v2/sessions/changes").is_err());
        assert!(resource(&format!("/v2/sessions/page?cursor={}", "x".repeat(8192))).is_err());
    }
}
