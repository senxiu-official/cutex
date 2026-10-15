//! Ephemeral remote rows. Credentials and lifecycle effects stay on the owning host.
use super::*;
use cutex::management::{
    connections::{Connection, Hosts},
    v2::host_sessions::HostSession,
};
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub connection: Connection,
    pub session: HostSession,
    pub stale: bool,
}
impl Entry {
    fn key(&self) -> String {
        format!("remote/{}/{}", self.connection.id, self.session.id)
    }
    fn view(&self) -> AgentSessionView {
        let s = &self.session;
        AgentSessionView {
            remote_provider:s.provider, remote_profile_style:s.profile_style.clone(),
            host: s.host_id.clone(),
            badge: None,
            project_id: s.project_id.clone(),
            subject: SubjectRef::Native {
                catalog: format!("remote/{}", self.connection.id),
                thread: s.id.clone(),
            },
            name: s.name.clone(),
            native_title: None,
            native_thread: s.native_id.clone(),
            native_workspace: None,
            compaction_count: None,
            runtime: if self.stale {
                Observation::Stale(
                    s.state.clone(),
                    "Remote host unreachable; last observed state".into(),
                )
            } else if s.state == "Unobserved" {
                Observation::Unavailable("Runtime not observed".into())
            } else {
                Observation::Known(s.state.clone())
            },
            runtime_version: s.runtime_version.clone(),
            runtime_version_latest: s.runtime_version_latest.clone(),
            runtime_version_recorded_at: s.runtime_version_recorded_at.clone(),
            project: s
                .project_name
                .clone()
                .map(Observation::Known)
                .unwrap_or_else(|| Observation::Known("unassigned".into())),
            configured_profile: s.profile.clone(),
            effective_profile: Observation::Unavailable("Not observed".into()),
            role: String::new(),
            activity: String::new(),
            activity_details: None,
            updated: "—".into(),
            cwd: s.cwd.clone(),
            retirement_note: None,
        }
    }
    fn agent_row(&self) -> SelectorRow {
        let s = &self.session;
        let view = self.view();
        SelectorRow {
            view: Some(view),
            target: SelectorTarget::RemoteAgent(self.connection.id.clone(), s.id.clone()),
            agent: s.name.clone(),
            thread_title: None,
            project: s.project_id.as_ref().map(|id| SelectorProjectContext {
                agent_name: s.name.clone(),
                project_id: id.clone(),
                display_name: s.project_name.clone().unwrap_or_else(|| id.clone()),
                badge_label: String::new(),
                color: ProjectPaletteColor::Blue,
            }),
            configured_profile: s.profile.clone(),
            lifecycle: if self.stale {
                None
            } else {
                match s.state.as_str() {
                    "Online" => Some(CutexSessionLifecycleState::Online),
                    "Offline" => Some(CutexSessionLifecycleState::Offline),
                    _ => None,
                }
            },
            host: s.host_id.clone(),
            backend: "remote".into(),
            managed_path: s.cwd.clone(),
            retired_at: None,
            revision: s.generation,
            activity_session_id: None,
            activity: None,
            actions: vec![],
            settings: vec![],
            settings_snapshot: None,
            global_settings_snapshot: None,
            attachable: !self.stale,
            pinned: false,
            managed: true,
        }
    }
    fn recent_row(&self) -> super::super::session_tui_recent::RecentThreadRow {
        use super::super::session_tui_recent::{RecentThreadRow, RecentThreadState};
        RecentThreadRow {
            view: self.view(),
            thread_id: self.key(),
            owned_runtime_id: None,
            title: self.session.name.clone(),
            managed_name: None,
            cwd: Some(self.session.cwd.clone()),
            provider: self.session.profile.clone().unwrap_or_default(),
            source: "remote".into(),
            project_id: self.session.project_id.clone(),
            recency_at: None,
            state: RecentThreadState::MissingCwd,
        }
    }
}
pub(super) fn selected(model: &SelectorModel) -> Option<Entry> {
    if matches!(model.mode, SelectorMode::Agents) {
        let SelectorTarget::RemoteAgent(c, id) = &model.selected_row()?.target else {
            return None;
        };
        model
            .remote_entries
            .iter()
            .find(|e| &e.connection.id == c && &e.session.id == id)
            .cloned()
    } else if matches!(model.mode, SelectorMode::RecentSessions) {
        let row = model.recent.selected_row()?;
        model
            .remote_entries
            .iter()
            .find(|e| e.key() == row.thread_id)
            .cloned()
    } else {
        None
    }
}
pub(super) fn apply(model: &mut SelectorModel) {
    let hosts = Hosts::load().unwrap_or_default();
    model.remote_entries.retain(|e| {
        hosts.connections.iter().any(|c| {
            c.enabled
                && c.id == e.connection.id
                && c.host_id.eq_ignore_ascii_case(&e.session.host_id)
        })
    });

    model.rows.retain(|r| {
        !matches!(r.target, SelectorTarget::RemoteAgent(..))
            && !hosts
                .connections
                .iter()
                .any(|c| c.host_id.eq_ignore_ascii_case(&r.host))
    });
    model.rows.extend(
        model
            .remote_entries
            .iter()
            .filter(|e| e.session.kind == "Agent")
            .map(Entry::agent_row),
    );
    sort_rows(&mut model.rows);
    model.recent.replace_remote(
        model
            .remote_entries
            .iter()
            .filter(|e| e.session.kind == "Session")
            .map(Entry::recent_row)
            .collect(),
    );
}
pub(super) type Reply = (Connection, Result<Vec<HostSession>, String>);
pub(super) fn start() -> std::sync::mpsc::Receiver<Reply> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for c in Hosts::load()
            .unwrap_or_default()
            .connections
            .into_iter()
            .filter(|c| c.enabled)
        {
            let result = (|| -> anyhow::Result<Vec<HostSession>> {
                let mut rows = vec![];
                let mut cursor = None;
                let mut seen = std::collections::HashSet::new();
                for _ in 0..20 {
                    let v = super::super::remote_sessions::list(&c, "", cursor.as_deref())?;
                    let page: Vec<HostSession> = serde_json::from_value(v["data"].clone())?;
                    anyhow::ensure!(
                        page.iter()
                            .all(|s| s.host_id.eq_ignore_ascii_case(&c.host_id)),
                        "Remote row host mismatch"
                    );
                    rows.extend(page);
                    cursor = v["nextCursor"].as_str().map(str::to_owned);
                    if cursor.is_none() {
                        return Ok(rows);
                    }
                    anyhow::ensure!(seen.insert(cursor.clone()), "Remote cursor repeated");
                }
                anyhow::bail!("Remote catalog exceeds 1000 rows; use Hosts browser pagination")
            })()
            .map_err(|e| format!("{e:#}"));
            if tx.send((c, result)).is_err() {
                break;
            }
        }
    });
    rx
}
pub(super) fn receive(
    model: &mut SelectorModel,
    c: Connection,
    result: Result<Vec<HostSession>, String>,
) {
    match result {
        Ok(rows) => {
            model.remote_warnings.remove(&c.id);
            model.remote_entries.retain(|e| e.connection.id != c.id);
            model
                .remote_entries
                .extend(rows.into_iter().map(|session| Entry {
                    connection: c.clone(),
                    session,
                    stale: false,
                }));
        }
        Err(e) => {
            for row in &mut model.remote_entries {
                if row.connection.id == c.id {
                    row.stale = true;
                }
            }
            model.remote_warnings.insert(c.id.clone(), format!("{}: {e}", c.name));
        }
    }
    apply(model);
    model.ensure_selection();
}
#[cfg(test)]
mod tests {
    use super::*;
    fn entry(kind: &str) -> Entry {
        Entry {
            connection: Connection {
                id: "eva".into(),
                name: "EVA".into(),
                host_id: "eva".into(),
                ssh_target: "eva".into(),
                local_port: 24670,
                remote_port: 24270,
                token_file: std::path::PathBuf::from("/unused"),
                enabled: true,
            },
            session: HostSession {
                provider:None,profile_style:None,
                id: "cutex.018e0000-0000-7000-8000-0001c00032de".into(),
                name: "worker".into(),
                kind: kind.into(),
                host_id: "eva".into(),
                profile: None,
                project_id: Some("p".into()),
                project_name: Some("Project".into()),
                cwd: "C:\\work".into(),
                state: "Online".into(),
                generation: 1,
                native_id: None,
                runtime_version: None,
                runtime_version_latest: None,
                runtime_version_recorded_at: None,
            },
            stale: false,
        }
    }
    #[test]
    fn remote_poll_does_not_replace_local_action_error() {
        let e = entry("Agent");
        let mut m = SelectorModel::new(Vec::new(), false, false);
        m.warning = Some("local native CLI failed: active writer".into());
        receive(&mut m, e.connection.clone(), Err("Connection refused".into()));
        assert_eq!(m.warning.as_deref(), Some("local native CLI failed: active writer"));
        assert!(m.remote_warnings["eva"].contains("Connection refused"));
        receive(&mut m, e.connection, Ok(Vec::new()));
        assert!(m.remote_warnings.is_empty());
        assert_eq!(m.warning.as_deref(), Some("local native CLI failed: active writer"));
    }

    #[test]
    fn remote_projection_preserves_host_reported_runtime_version() {
        let mut e = entry("Agent");
        e.session.runtime_version = Some("0.157.1-r44".into());
        e.session.runtime_version_latest = Some("0.157.1-r45".into());
        e.session.runtime_version_recorded_at = Some("2026-09-28T01:52+10:00".into());
        let view = e.agent_row().view.expect("remote agent view");
        assert_eq!(view.runtime_version.as_deref(), Some("0.157.1-r44"));
        assert_eq!(view.runtime_version_latest.as_deref(), Some("0.157.1-r45"));
        assert_eq!(
            view.runtime_version_recorded_at.as_deref(),
            Some("2026-09-28T01:52+10:00")
        );
    }

    #[test]
    fn remote_agent_never_dispatches_local_lifecycle_or_settings() {
        let e = entry("Agent");
        let mut m = SelectorModel::new(vec![e.agent_row()], false, false);
        m.remote_entries = vec![e.clone()];
        assert!(m.selected_row().unwrap().target.agent_key().is_none());
        assert!(
            matches!(route_selector_key(&mut m,KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),SelectorKeyRoute::Control(Some(SelectorControl::RemoteBrowse(_,id))) if id==e.session.id)
        );
        assert!(
            matches!(selector_command(&mut m,Command::Actions),SelectorKeyRoute::Control(Some(SelectorControl::RemoteBrowse(_,id))) if id==e.session.id)
        );
        assert!(matches!(
            selector_command(&mut m, Command::Edit),
            SelectorKeyRoute::Control(None)
        ));
    }
    #[test]
    fn remote_session_never_dispatches_native_adoption_or_local_resume() {
        let e = entry("Session");
        let mut m = SelectorModel::new(vec![], false, false);
        m.mode = SelectorMode::RecentSessions;
        m.remote_entries = vec![e.clone()];
        m.recent.replace_remote(vec![e.recent_row()]);
        assert!(
            matches!(route_selector_key(&mut m,KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),SelectorKeyRoute::Control(Some(SelectorControl::RemoteBrowse(_,id))) if id==e.session.id)
        );
        assert!(matches!(
            selector_command(&mut m, Command::Actions),
            SelectorKeyRoute::Control(Some(SelectorControl::RemoteBrowse(_, _)))
        ));
    }
    #[test]
    fn lost_connection_marks_cached_observation_stale() {
        let mut e = entry("Agent");
        e.stale = true;
        assert!(matches!(e.view().runtime, Observation::Stale(..)));
        assert!(e.agent_row().lifecycle.is_none());
    }
}
