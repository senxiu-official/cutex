//! Cheap project-table facts: keyed member reads or already projected sessions.
use crate::management::server::ManagementRuntimeStatusLoader;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const FEATURE: &str = "project-table-summary-v1";

pub(super) fn enabled(path: &str) -> bool {
    crate::http::query::query_value(path, "includeTableSummary").as_deref() == Some("true")
}

#[derive(Clone)]
pub(super) struct Stats {
    online: u64,
    online_known: bool,
    latest: Option<DateTime<Utc>>,
    activity_known: bool,
}
impl Default for Stats {
    fn default() -> Self {
        Self {
            online: 0,
            online_known: true,
            latest: None,
            activity_known: true,
        }
    }
}
impl Stats {
    fn observe(&mut self, online: Option<bool>, activity: Result<Option<&str>, ()>) {
        match online {
            Some(true) => self.online += 1,
            Some(false) => {}
            None => self.online_known = false,
        }
        match activity {
            Ok(Some(at)) => match DateTime::parse_from_rfc3339(at) {
                Ok(at) => {
                    let at = at.with_timezone(&Utc);
                    self.latest = Some(self.latest.map_or(at, |old| old.max(at)));
                }
                Err(_) => self.activity_known = false,
            },
            Ok(None) => {}
            Err(()) => self.activity_known = false,
        }
    }
    pub(super) fn apply(&self, row: &mut Value) {
        row["online_member_count"] = if self.online_known {
            json!(self.online)
        } else {
            Value::Null
        };
        row["last_activity_at"] = if self.activity_known {
            json!(self.latest.map(|at| at.to_rfc3339()))
        } else {
            Value::Null
        };
    }
    fn session(&mut self, session: Option<&Value>) {
        let Some(session) = session else {
            self.observe(None, Err(()));
            return;
        };
        if matches!(session["lifecycle"].as_str(), Some("retired" | "archived")) {
            return;
        }
        self.observe(
            session
                .pointer("/runtime/status")
                .and_then(Value::as_str)
                .map(|s| s == "online"),
            Ok(session["lastActivityAt"].as_str().or_else(|| {
                session
                    .pointer("/activity/lastConversationAt")
                    .and_then(Value::as_str)
            })),
        );
    }
}

pub(super) fn from_sessions(
    memberships: &BTreeMap<String, String>,
    sessions: &[Value],
) -> BTreeMap<String, Stats> {
    let sessions: BTreeMap<_, _> = sessions
        .iter()
        .filter_map(|s| Some((s["cutexSessionId"].as_str()?, s)))
        .collect();
    let mut projects = BTreeMap::<String, Stats>::new();
    for (id, project) in memberships {
        projects
            .entry(project.clone())
            .or_default()
            .session(sessions.get(id.as_str()).copied());
    }
    projects
}

fn observe_member(stats: &mut Stats, id: &str, load: ManagementRuntimeStatusLoader) {
    let record = match crate::session::store::shared_session_row(id) {
        Ok(Some(record)) if record.is_active() => record,
        Ok(Some(_)) => return,
        _ => {
            stats.observe(None, Err(()));
            return;
        }
    };
    let online = load(id).ok().map(|status| {
        status.is_some_and(|s| {
            s.connected
                && s.cutex_session_id == id
                && s.runtime_generation == record.runtime_generation
        })
    });
    let activity = super::activity::session_activity(id);
    let at = activity
        .as_ref()
        .ok()
        .and_then(|a| a.as_ref())
        .and_then(|a| a.conversation_at());
    stats.observe(
        online,
        if activity.is_ok() {
            Ok(at.as_deref())
        } else {
            Err(())
        },
    );
}

/// The collection already carries the live member/project index. Never load
/// session/roster/history tables to enrich it, including for scoped users.
pub(super) fn enrich_collection(value: &mut Value, load: ManagementRuntimeStatusLoader) {
    enrich_collection_with(value, |stats, id| observe_member(stats, id, load));
}

fn enrich_collection_with(value: &mut Value, mut observe: impl FnMut(&mut Stats, &str)) {
    let mut stats = BTreeMap::<String, Stats>::new();
    if let Some(agents) = value["available_agents"].as_array() {
        for agent in agents {
            if let (Some(id), Some(project)) = (
                agent["cutex_session_id"].as_str(),
                agent["current_project_id"].as_str(),
            ) {
                observe(stats.entry(project.into()).or_default(), id);
            }
        }
    }
    for field in ["projects", "archived_projects"] {
        if let Some(projects) = value[field].as_array_mut() {
            for row in projects {
                stats
                    .get(row["project_id"].as_str().unwrap_or(""))
                    .cloned()
                    .unwrap_or_default()
                    .apply(row);
            }
        }
    }
}

pub(super) fn enrich_workspace(value: &mut Value, load: ManagementRuntimeStatusLoader) {
    let ids = workspace_ids(value);
    let mut stats = Stats::default();
    for id in ids {
        observe_member(&mut stats, &id, load);
    }
    stats.apply(value);
}

fn workspace_ids(value: &Value) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if let Some(id) = value
        .pointer("/director/member/agent/cutex_session_id")
        .and_then(Value::as_str)
    {
        ids.insert(id.to_owned());
    }
    for field in ["active_agents", "agent_operators"] {
        for member in value[field].as_array().into_iter().flatten() {
            let member = if field == "agent_operators" {
                &member["member"]
            } else {
                member
            };
            if let Some(id) = member
                .pointer("/agent/cutex_session_id")
                .and_then(Value::as_str)
            {
                ids.insert(id.to_owned());
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_table_uses_conversation_time_and_excludes_archived_members() {
        let members = [("a", "p"), ("b", "p"), ("archived", "p")]
            .into_iter()
            .map(|(a, p)| (a.into(), p.into()))
            .collect();
        let sessions = vec![
            json!({"cutexSessionId":"a","runtime":{"status":"online"},"lastActivityAt":"2026-11-25T12:00:00+11:00"}),
            json!({"cutexSessionId":"b","runtime":{"status":"offline"},"lastActivityAt":"2026-11-25T02:00:00Z"}),
            json!({"cutexSessionId":"archived","lifecycle":"retired","runtime":{"status":"online"},"lastActivityAt":"2099-01-01T00:00:00Z"}),
        ];
        let stats = from_sessions(&members, &sessions);
        let mut row = json!({});
        stats["p"].apply(&mut row);
        assert_eq!(
            row,
            json!({"online_member_count":1,"last_activity_at":"2026-11-25T02:00:00+00:00"})
        );
        let missing = from_sessions(&members, &sessions[..1]);
        missing["p"].apply(&mut row);
        assert!(row["online_member_count"].is_null() && row["last_activity_at"].is_null());
    }

    #[test]
    fn project_table_collection_reads_only_the_existing_live_member_index() {
        let mut value = json!({"projects":[{"project_id":"p"},{"project_id":"empty"}],
            "archived_projects":[{"project_id":"old"}],"archived_agents":[{"cutex_session_id":"archived"}],
            "available_agents":[{"cutex_session_id":"live","current_project_id":"p"},
                {"cutex_session_id":"independent"}]});
        let mut touched = Vec::new();
        enrich_collection_with(&mut value, |stats, id| {
            touched.push(id.to_owned());
            stats.observe(Some(true), Ok(None));
        });
        assert_eq!(touched, ["live"]);
        assert_eq!(value["projects"][0]["online_member_count"], 1);
        assert_eq!(value["projects"][1]["online_member_count"], 0);
        assert!(value["projects"][1]["last_activity_at"].is_null());
        assert_eq!(value["archived_projects"][0]["online_member_count"], 0);
        let member = |id| json!({"agent":{"cutex_session_id":id}});
        let workspace = json!({"director":{"member":member("director")},
            "active_agents":[member("worker"),member("director")],
            "agent_operators":[{"member":member("operator")}],
            "archived_agents":[member("archived")],"retired_agents":[member("retired")]});
        assert_eq!(
            workspace_ids(&workspace),
            ["director".into(), "worker".into(), "operator".into()].into()
        );
    }

    #[test]
    fn project_table_is_opt_in_and_unavailable_is_not_zero() {
        assert!(!enabled("/v2/agent-management/projects"));
        assert!(enabled(
            "/v2/agent-management/projects?includeTableSummary=true"
        ));
        let mut row = json!({});
        let mut stats = Stats::default();
        stats.observe(None, Ok(Some("2026-11-25T02:00:00Z")));
        stats.apply(&mut row);
        assert!(row["online_member_count"].is_null());
        assert!(row["last_activity_at"].is_string());
        stats.observe(Some(false), Ok(Some("invalid")));
        stats.apply(&mut row);
        assert!(row["last_activity_at"].is_null());
    }
}
