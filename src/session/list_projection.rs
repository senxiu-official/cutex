//! List and choice-row projection for durable `cutex_session` records.

use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentRegistrationClass;
use crate::runtime::alden::CuteAldenSession;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::model::CutexSessionStore;
use crate::session::projection::cutex_session_is_attachable;
use crate::session::projection::cutex_session_scope_label;
use crate::session::projection::cutex_session_status_label_with_agents;
use crate::session::projection::runtime_backend_short_label;
use crate::session::service::cutex_session_display_name;
use crate::session::service::cutex_session_launch_cwd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutexSessionListSort {
    Status,
    Recent,
    Name,
    Project,
}

#[derive(Debug, Clone, Default)]
pub struct CutexSessionListFilter {
    pub all: bool,
    pub offline: bool,
    pub one_shot: bool,
    pub host: bool,
    pub attachable: bool,
    pub projects: Vec<String>,
    pub groups: Vec<String>,
    pub sort: CutexSessionListSort,
}

impl Default for CutexSessionListSort {
    fn default() -> Self {
        Self::Status
    }
}

impl CutexSessionListFilter {
    pub fn matches_default_scope(&self) -> bool {
        !self.all
            && !self.offline
            && !self.one_shot
            && !self.host
            && !self.attachable
            && self.projects.is_empty()
            && self.groups.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionListRow {
    pub status: &'static str,
    pub display_name: String,
    pub scope: &'static str,
    pub profile: String,
    pub backend: &'static str,
    pub codex_session_id: String,
    pub cwd: String,
    pub attach_session_name: Option<String>,
    pub managed_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionChoiceRow {
    pub key: String,
    pub display_name: String,
    pub status: &'static str,
    pub backend: &'static str,
    pub scope: &'static str,
    pub has_managed_cwd: bool,
    pub launch_cwd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutexSessionFilterNote {
    DefaultHidden { hidden_count: usize },
}

pub fn filtered_cutex_session_records<'a>(
    store: &'a CutexSessionStore,
    alden_sessions: &[CuteAldenSession],
    filter: &CutexSessionListFilter,
) -> (Vec<(&'a String, &'a CutexSessionRecord)>, usize) {
    filtered_cutex_session_records_with_agents(store, alden_sessions, filter, &[])
}

/// As [`filtered_cutex_session_records`], judging `--offline` against the
/// Agent Bus live agents so running host/app-server agents are not offline.
pub fn filtered_cutex_session_records_with_agents<'a>(
    store: &'a CutexSessionStore,
    alden_sessions: &[CuteAldenSession],
    filter: &CutexSessionListFilter,
    live_agents: &[AgentBusAgent],
) -> (Vec<(&'a String, &'a CutexSessionRecord)>, usize) {
    let mut visible = Vec::new();
    let mut hidden_count = 0_usize;
    for (key, record) in &store.sessions {
        if record.is_retired() {
            continue;
        }
        if cutex_session_matches_list_filters(record, alden_sessions, live_agents, filter) {
            visible.push((key, record));
        } else {
            hidden_count += 1;
        }
    }
    sort_cutex_session_records(&mut visible, alden_sessions, filter.sort);
    (visible, hidden_count)
}

pub fn cutex_session_filter_note(
    hidden_count: usize,
    filter: &CutexSessionListFilter,
) -> Option<CutexSessionFilterNote> {
    if hidden_count == 0 {
        return None;
    }
    filter
        .matches_default_scope()
        .then_some(CutexSessionFilterNote::DefaultHidden { hidden_count })
}

pub fn cutex_session_list_row(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
) -> CutexSessionListRow {
    cutex_session_list_row_with_agents(record, alden_sessions, &[])
}

/// List row whose status sees Agent Bus live agents: a running host or
/// app-server agent is `online`, not `stale`.
pub fn cutex_session_list_row_with_agents(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
) -> CutexSessionListRow {
    let status = cutex_session_status_label_with_agents(record, alden_sessions, live_agents);
    CutexSessionListRow {
        status,
        display_name: cutex_session_display_name(record),
        scope: cutex_session_scope_label(record),
        profile: record.profile.clone().unwrap_or_else(|| "-".to_string()),
        backend: runtime_backend_short_label(record.runtime_backend),
        codex_session_id: record
            .codex_session_id
            .clone()
            .unwrap_or_else(|| "-".to_string()),
        cwd: record.cwd.clone(),
        attach_session_name: (status == "attachable")
            .then(|| record.alden_session_name.clone())
            .flatten(),
        managed_cwd: record
            .managed_cwd
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != record.cwd)
            .map(ToString::to_string),
    }
}

pub fn cutex_session_choice_rows(
    records: &[(&String, &CutexSessionRecord)],
    alden_sessions: &[CuteAldenSession],
) -> Vec<CutexSessionChoiceRow> {
    cutex_session_choice_rows_with_agents(records, alden_sessions, &[])
}

pub fn cutex_session_choice_rows_with_agents(
    records: &[(&String, &CutexSessionRecord)],
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
) -> Vec<CutexSessionChoiceRow> {
    records
        .iter()
        .map(|(key, record)| {
            cutex_session_choice_row_with_agents(key, record, alden_sessions, live_agents)
        })
        .collect()
}

pub fn cutex_session_choice_row(
    key: &str,
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
) -> CutexSessionChoiceRow {
    cutex_session_choice_row_with_agents(key, record, alden_sessions, &[])
}

pub fn cutex_session_choice_row_with_agents(
    key: &str,
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
) -> CutexSessionChoiceRow {
    CutexSessionChoiceRow {
        key: key.to_string(),
        display_name: cutex_session_display_name(record),
        status: cutex_session_status_label_with_agents(record, alden_sessions, live_agents),
        backend: runtime_backend_short_label(record.runtime_backend),
        scope: cutex_session_scope_label(record),
        has_managed_cwd: record
            .managed_cwd
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        launch_cwd: cutex_session_launch_cwd(record).to_string(),
    }
}

fn cutex_session_matches_list_filters(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
    filter: &CutexSessionListFilter,
) -> bool {
    if !filter.groups.is_empty() && !session_record_matches_any_group(record, &filter.groups) {
        return false;
    }
    if !filter.projects.is_empty() && !session_record_matches_any_project(record, &filter.projects)
    {
        return false;
    }

    if filter.all {
        return true;
    }

    let attachable = cutex_session_is_attachable(record, alden_sessions);
    let explicit_scope = filter.offline || filter.one_shot || filter.host || filter.attachable;
    if !explicit_scope {
        return record.exposed_to_backend
            || record.registration_class == AgentRegistrationClass::Persistent
            || attachable;
    }

    (filter.offline
        && cutex_session_status_label_with_agents(record, alden_sessions, live_agents)
            == "offline")
        || (filter.one_shot
            && matches!(
                record.registration_class,
                AgentRegistrationClass::Ephemeral | AgentRegistrationClass::LocalOnly
            ))
        || (filter.host
            && matches!(
                record.runtime_backend,
                CutexSessionRuntimeBackend::Host | CutexSessionRuntimeBackend::HostForeground
            ))
        || (filter.attachable && attachable)
}

fn session_record_matches_any_group(record: &CutexSessionRecord, groups: &[String]) -> bool {
    groups.iter().any(|group| {
        record
            .agent_groups
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(group.trim()))
    })
}

fn session_record_matches_any_project(record: &CutexSessionRecord, projects: &[String]) -> bool {
    projects.iter().any(|project| {
        let needle = project.trim().to_ascii_lowercase();
        !needle.is_empty()
            && [
                record.cwd.as_str(),
                cutex_session_launch_cwd(record),
                record.cutex_session_id.as_str(),
                record.codex_session_id.as_deref().unwrap_or(""),
                record.thread_name.as_deref().unwrap_or(""),
                record.display_name_hint.as_deref().unwrap_or(""),
            ]
            .iter()
            .any(|value| value.to_ascii_lowercase().contains(&needle))
    })
}

fn sort_cutex_session_records(
    records: &mut Vec<(&String, &CutexSessionRecord)>,
    alden_sessions: &[CuteAldenSession],
    sort: CutexSessionListSort,
) {
    records.sort_by(|(left_key, left), (right_key, right)| match sort {
        CutexSessionListSort::Status => cutex_session_status_rank(right, alden_sessions)
            .cmp(&cutex_session_status_rank(left, alden_sessions))
            .then_with(|| right.exposed_to_backend.cmp(&left.exposed_to_backend))
            .then_with(|| {
                (right.registration_class == AgentRegistrationClass::Persistent)
                    .cmp(&(left.registration_class == AgentRegistrationClass::Persistent))
            })
            .then_with(|| cutex_session_display_name(left).cmp(&cutex_session_display_name(right)))
            .then_with(|| left_key.cmp(right_key)),
        CutexSessionListSort::Recent => cutex_session_recent_sort_key(right)
            .cmp(&cutex_session_recent_sort_key(left))
            .then_with(|| cutex_session_display_name(left).cmp(&cutex_session_display_name(right)))
            .then_with(|| left_key.cmp(right_key)),
        CutexSessionListSort::Name => cutex_session_display_name(left)
            .cmp(&cutex_session_display_name(right))
            .then_with(|| left_key.cmp(right_key)),
        CutexSessionListSort::Project => cutex_session_launch_cwd(left)
            .cmp(cutex_session_launch_cwd(right))
            .then_with(|| cutex_session_display_name(left).cmp(&cutex_session_display_name(right)))
            .then_with(|| left_key.cmp(right_key)),
    });
}

fn cutex_session_status_rank(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
) -> u8 {
    if cutex_session_is_attachable(record, alden_sessions) {
        3
    } else if record.current_runtime_agent_id.is_some() {
        2
    } else if record.exposed_to_backend
        || record.registration_class == AgentRegistrationClass::Persistent
    {
        1
    } else {
        0
    }
}

fn cutex_session_recent_sort_key(record: &CutexSessionRecord) -> &str {
    record
        .last_seen_at
        .as_deref()
        .unwrap_or(record.updated_at.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_list_default_hides_historical_local_sessions() {
        let mut store = CutexSessionStore::default();
        let timestamp = "2026-06-28T00:00:00Z".to_string();

        let mut persistent = CutexSessionRecord::new_at(
            "cutex.persistent".to_string(),
            Some("019e-persistent".to_string()),
            "devbox".to_string(),
            "/tmp/persistent".to_string(),
            Some("orchid".to_string()),
            timestamp.clone(),
        )
        .expect("persistent record");
        persistent.display_name_hint = Some("persistent".to_string());
        persistent.registration_class = AgentRegistrationClass::Persistent;
        persistent.exposed_to_backend = true;
        persistent.agent_groups = vec!["aria".to_string()];
        store
            .sessions
            .insert(persistent.cutex_session_id.clone(), persistent);

        let mut historical = CutexSessionRecord::new_at(
            "cutex.historical".to_string(),
            Some("019e-historical".to_string()),
            "devbox".to_string(),
            "/tmp/historical".to_string(),
            Some("orchid".to_string()),
            timestamp.clone(),
        )
        .expect("historical record");
        historical.display_name_hint = Some("historical".to_string());
        store
            .sessions
            .insert(historical.cutex_session_id.clone(), historical);

        let mut attachable = CutexSessionRecord::new_at(
            "cutex.attachable".to_string(),
            Some("019e-attachable".to_string()),
            "devbox".to_string(),
            "/tmp/attachable".to_string(),
            Some("orchid".to_string()),
            timestamp,
        )
        .expect("attachable record");
        attachable.display_name_hint = Some("attachable".to_string());
        attachable.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        attachable.alden_session_name = Some("cutex.attachable.runtime".to_string());
        attachable.alden_pid = Some(std::process::id());
        store
            .sessions
            .insert(attachable.cutex_session_id.clone(), attachable);

        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: Some("cutex.attachable.runtime".to_string()),
        }];

        let (records, hidden) = filtered_cutex_session_records(
            &store,
            &alden_sessions,
            &CutexSessionListFilter::default(),
        );
        let ids = records
            .iter()
            .map(|(_, record)| record.cutex_session_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["cutex.attachable", "cutex.persistent"]);
        assert_eq!(hidden, 1);

        let all = CutexSessionListFilter {
            all: true,
            sort: CutexSessionListSort::Name,
            ..CutexSessionListFilter::default()
        };
        let (records, hidden) = filtered_cutex_session_records(&store, &alden_sessions, &all);
        let ids = records
            .iter()
            .map(|(_, record)| record.cutex_session_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["cutex.attachable", "cutex.historical", "cutex.persistent"]
        );
        assert_eq!(hidden, 0);

        let group_filter = CutexSessionListFilter {
            groups: vec!["aria".to_string()],
            ..CutexSessionListFilter::default()
        };
        let (records, hidden) =
            filtered_cutex_session_records(&store, &alden_sessions, &group_filter);
        let ids = records
            .iter()
            .map(|(_, record)| record.cutex_session_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["cutex.persistent"]);
        assert_eq!(hidden, 2);
    }

    #[test]
    fn session_filter_note_only_marks_default_hidden_rows() {
        let default_filter = CutexSessionListFilter::default();
        assert_eq!(cutex_session_filter_note(0, &default_filter), None);
        assert_eq!(
            cutex_session_filter_note(3, &default_filter),
            Some(CutexSessionFilterNote::DefaultHidden { hidden_count: 3 })
        );

        let explicit_filter = CutexSessionListFilter {
            all: true,
            ..CutexSessionListFilter::default()
        };
        assert_eq!(cutex_session_filter_note(3, &explicit_filter), None);
    }

    #[test]
    fn retired_only_store_is_empty_in_all_active_filters_and_counts() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.retired".to_string(),
            Some("019e-retired".to_string()),
            "devbox".to_string(),
            "/tmp/retired".to_string(),
            Some("orchid".to_string()),
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("record");
        record.registration_class = AgentRegistrationClass::Persistent;
        record.exposed_to_backend = true;
        record.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
        record.retired_at = Some("2026-08-10T00:01:00Z".to_string());
        let mut store = CutexSessionStore::default();
        store.sessions.insert("cutex.retired".to_string(), record);

        for filter in [
            CutexSessionListFilter::default(),
            CutexSessionListFilter {
                all: true,
                ..CutexSessionListFilter::default()
            },
            CutexSessionListFilter {
                offline: true,
                ..CutexSessionListFilter::default()
            },
        ] {
            let (records, hidden) = filtered_cutex_session_records(&store, &[], &filter);
            assert!(records.is_empty());
            assert_eq!(hidden, 0);
            assert_eq!(cutex_session_filter_note(hidden, &filter), None);
        }
    }

    #[test]
    fn session_list_row_projects_attachable_and_managed_cwd_facts() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.row".to_string(),
            Some("019e-row".to_string()),
            "devbox".to_string(),
            "/tmp/original".to_string(),
            Some("orchid".to_string()),
            "2026-06-28T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("row-display".to_string());
        record.exposed_to_backend = true;
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.alden_session_name = Some("cutex.row.runtime".to_string());
        record.alden_pid = Some(std::process::id());
        record.managed_cwd = Some(" /tmp/managed ".to_string());

        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: Some("cutex.row.runtime".to_string()),
        }];
        let row = cutex_session_list_row(&record, &alden_sessions);

        assert_eq!(row.status, "attachable");
        assert_eq!(row.display_name, "row-display");
        assert_eq!(row.scope, "im");
        assert_eq!(row.profile, "orchid");
        assert_eq!(row.backend, "alden");
        assert_eq!(row.codex_session_id, "019e-row");
        assert_eq!(row.cwd, "/tmp/original");
        assert_eq!(
            row.attach_session_name.as_deref(),
            Some("cutex.row.runtime")
        );
        assert_eq!(row.managed_cwd.as_deref(), Some("/tmp/managed"));
    }

    #[test]
    fn session_choice_row_uses_launch_cwd_and_preserves_key() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.choice".to_string(),
            Some("019e-choice".to_string()),
            "devbox".to_string(),
            "/tmp/original".to_string(),
            Some("orchid".to_string()),
            "2026-06-28T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("choice-display".to_string());
        record.managed_cwd = Some("/tmp/managed".to_string());

        let rows = cutex_session_choice_rows(&[(&record.cutex_session_id, &record)], &[]);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "cutex.choice");
        assert_eq!(rows[0].display_name, "choice-display");
        assert_eq!(rows[0].status, "offline");
        assert_eq!(rows[0].backend, "host");
        assert_eq!(rows[0].scope, "local");
        assert!(rows[0].has_managed_cwd);
        assert_eq!(rows[0].launch_cwd, "/tmp/managed");
    }

    #[test]
    fn session_list_sees_live_host_runtime_as_online_not_stale_or_offline() {
        use crate::session::model::{CutexAppServerRuntimeBinding, CutexAppServerTransport};
        let mut record = CutexSessionRecord::new_at(
            "cutex.live-host".to_string(),
            Some("019e-live-host".to_string()),
            crate::platform::host::current_host_name(),
            "/tmp/live-host".to_string(),
            Some("orchid".to_string()),
            "2026-06-28T00:00:00Z".to_string(),
        )
        .expect("record");
        record.registration_class = AgentRegistrationClass::Persistent;
        record.runtime_backend = CutexSessionRuntimeBackend::Host;
        record.current_runtime_agent_id = Some("cutex.live-host.runtime".to_string());
        record.app_server_runtime = Some(CutexAppServerRuntimeBinding {
            transport: CutexAppServerTransport::UnixSocket,
            endpoint: "unix:///tmp/runtime/app.sock".to_string(),
            pid: std::process::id(),
            runtime_dir: "/tmp/runtime".to_string(),
            launched_profile: Some("orchid".to_string()),
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: None,
            diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
            schema_version: "test".to_string(),
            schema_sha256: "hash".to_string(),
            started_at: "2026-08-08T00:00:00Z".to_string(),
        });
        let live_agents = vec![AgentBusAgent {
            id: "cutex.live-host.runtime".to_string(),
            name: "live-host.runtime".to_string(),
            base_name: Some("live-host".to_string()),
            thread_name: None,
            path_key: None,
            session_id: record.codex_session_id.clone(),
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: record.cwd.clone(),
            pid: std::process::id(),
            host_id: Some(crate::platform::host::current_host_name()),
            groups: Vec::new(),
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 42,
        }];
        // Without the bus view every running agent looked stale.
        assert_eq!(cutex_session_list_row(&record, &[]).status, "stale");
        assert_eq!(
            cutex_session_list_row_with_agents(&record, &[], &live_agents).status,
            "online"
        );
        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        let offline = CutexSessionListFilter {
            offline: true,
            ..CutexSessionListFilter::default()
        };
        let (records, _) =
            filtered_cutex_session_records_with_agents(&store, &[], &offline, &live_agents);
        assert!(records.is_empty(), "live agent listed by --offline");
        let mut stopped = record;
        stopped.current_runtime_agent_id = None;
        stopped.app_server_runtime = None;
        store
            .sessions
            .insert(stopped.cutex_session_id.clone(), stopped);
        let (records, _) =
            filtered_cutex_session_records_with_agents(&store, &[], &offline, &live_agents);
        assert_eq!(records.len(), 1);
    }
}
