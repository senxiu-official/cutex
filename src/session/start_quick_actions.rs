//! Start-menu quick-action projection for durable `cutex_session` records.

use crate::agent_bus::model::AgentBusAgent;
use crate::agent_bus::model::AgentRegistrationClass;
use crate::runtime::alden::CuteAldenSession;
use crate::session::model::CutexSessionQuickActionMode;
use crate::session::model::CutexSessionRecord;
use crate::session::model::CutexSessionRuntimeBackend;
use crate::session::model::CutexSessionStore;
use crate::session::model::CutexSessionUserAction;
use crate::session::projection::cutex_session_is_attachable;
use crate::session::service::cutex_session_display_name;
use crate::session::service::cutex_session_launch_cwd;

#[derive(Debug, Clone)]
pub struct StartQuickAction {
    pub key: String,
    pub display_name: String,
    pub kind: StartQuickActionKind,
    pub reason: String,
    score: i64,
    last_user_selected_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartQuickActionKind {
    OpenDetails,
    Attach,
    Takeover,
    ResumeAttach,
    VisibleTui,
    Online,
    ResumeHere,
    ResumeManaged,
}

impl StartQuickActionKind {
    pub fn start_menu_label(self) -> &'static str {
        if self.opens_detail_first() {
            "open"
        } else {
            self.menu_label()
        }
    }

    pub fn menu_label(self) -> &'static str {
        match self {
            Self::OpenDetails => "open",
            Self::Attach => "attach",
            Self::Takeover => "takeover",
            Self::ResumeAttach => "takeover",
            Self::VisibleTui => "open TUI",
            Self::Online => "online",
            Self::ResumeHere => "resume here",
            Self::ResumeManaged => "resume managed",
        }
    }

    pub fn opens_detail_first(self) -> bool {
        matches!(self, Self::OpenDetails | Self::Attach | Self::Takeover)
    }

    pub fn user_action(self) -> CutexSessionUserAction {
        match self {
            Self::OpenDetails => CutexSessionUserAction::ResumeManaged,
            Self::Attach => CutexSessionUserAction::Attach,
            Self::Takeover => CutexSessionUserAction::Takeover,
            Self::ResumeAttach => CutexSessionUserAction::ResumeAttach,
            Self::VisibleTui => CutexSessionUserAction::ResumeManaged,
            Self::Online => CutexSessionUserAction::Online,
            Self::ResumeHere => CutexSessionUserAction::ResumeHere,
            Self::ResumeManaged => CutexSessionUserAction::ResumeManaged,
        }
    }
}

pub fn recommended_start_quick_actions(
    store: &CutexSessionStore,
    alden_sessions: &[CuteAldenSession],
    current_cwd: &str,
) -> Vec<StartQuickAction> {
    recommended_start_quick_actions_with_agents(store, alden_sessions, &[], current_cwd)
}

pub fn recommended_start_quick_actions_with_agents(
    store: &CutexSessionStore,
    alden_sessions: &[CuteAldenSession],
    live_agents: &[AgentBusAgent],
    current_cwd: &str,
) -> Vec<StartQuickAction> {
    let mut actions = store
        .sessions
        .iter()
        .filter(|(_, record)| record.is_active())
        .filter_map(|(key, record)| {
            start_quick_action_for_record(key, record, alden_sessions, live_agents, current_cwd)
        })
        .collect::<Vec<_>>();
    actions.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.last_user_selected_at.cmp(&left.last_user_selected_at))
            .then_with(|| left.display_name.cmp(&right.display_name))
            .then_with(|| left.key.cmp(&right.key))
    });
    actions.truncate(8);
    actions
}

pub fn primary_start_action_kind_for_record(
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
) -> Option<StartQuickActionKind> {
    // Explicit stock sessions have a separate reviewed launch and same-owner
    // attach protocol. Never project a generic start/resume quick action for
    // them; the detailed session surface can offer stock attach when a ready
    // owner exists and otherwise explain that explicit stock start is needed.
    if record.explicit_launch.is_some() {
        return Some(StartQuickActionKind::OpenDetails);
    }
    let attachable = cutex_session_is_attachable(record, alden_sessions);
    let kind = if record.runtime_backend == CutexSessionRuntimeBackend::HostForeground {
        StartQuickActionKind::VisibleTui
    } else if record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden
        && record.codex_session_id.is_some()
    {
        StartQuickActionKind::ResumeAttach
    } else if attachable {
        StartQuickActionKind::Attach
    } else if record.exposed_to_backend
        || record.registration_class == AgentRegistrationClass::Persistent
    {
        StartQuickActionKind::Online
    } else if record.managed_cwd.is_some() {
        StartQuickActionKind::ResumeManaged
    } else {
        StartQuickActionKind::ResumeHere
    };

    if matches!(
        kind,
        StartQuickActionKind::ResumeAttach
            | StartQuickActionKind::VisibleTui
            | StartQuickActionKind::ResumeHere
            | StartQuickActionKind::ResumeManaged
    ) && record.codex_session_id.is_none()
    {
        None
    } else {
        Some(kind)
    }
}

fn start_quick_action_for_record(
    key: &str,
    record: &CutexSessionRecord,
    alden_sessions: &[CuteAldenSession],
    _live_agents: &[AgentBusAgent],
    current_cwd: &str,
) -> Option<StartQuickAction> {
    if record.quick_action == CutexSessionQuickActionMode::Hidden {
        return None;
    }

    let pinned = record.quick_action == CutexSessionQuickActionMode::Pinned;
    let cwd_score = cutex_session_cwd_relevance_score(record, current_cwd);
    let explicit_recent = record.last_user_selected_at.is_some();
    if !pinned && cwd_score == 0 && !explicit_recent {
        return None;
    }
    if !pinned && !explicit_recent && cwd_score > 0 {
        let quick_worthy_from_cwd =
            record.exposed_to_backend || cutex_session_is_attachable(record, alden_sessions);
        if !quick_worthy_from_cwd {
            return None;
        }
    }

    let attachable = cutex_session_is_attachable(record, alden_sessions);
    let mut kind = primary_start_action_kind_for_record(record, alden_sessions)?;
    if kind == StartQuickActionKind::ResumeManaged && cwd_score > 0 {
        kind = StartQuickActionKind::ResumeHere;
    }

    let mut reasons = Vec::new();
    let mut score = 0_i64;
    if pinned {
        reasons.push("pinned");
        score += 10_000;
    }
    if cwd_score > 0 {
        reasons.push("cwd");
        score += cwd_score;
    }
    if explicit_recent {
        reasons.push(
            record
                .last_user_action
                .map_or("recent", |action| action.label()),
        );
        score += 500;
    }
    if attachable {
        reasons.push("attachable");
        score += 100;
    }
    if record.exposed_to_backend || record.registration_class == AgentRegistrationClass::Persistent
    {
        score += 50;
    }

    Some(StartQuickAction {
        key: key.to_string(),
        display_name: cutex_session_display_name(record),
        kind,
        reason: reasons.join(", "),
        score,
        last_user_selected_at: record.last_user_selected_at.clone(),
    })
}

fn cutex_session_cwd_relevance_score(record: &CutexSessionRecord, current_cwd: &str) -> i64 {
    let launch = cutex_session_launch_cwd(record);
    if cwd_exact_or_current_inside_session(current_cwd, launch) {
        return 900;
    }
    if record
        .managed_cwd
        .as_deref()
        .is_some_and(|cwd| cwd_exact_or_current_inside_session(current_cwd, cwd))
    {
        return 850;
    }
    if cwd_exact_or_current_inside_session(current_cwd, &record.cwd) {
        return 800;
    }
    0
}

fn cwd_exact_or_current_inside_session(current_cwd: &str, session_cwd: &str) -> bool {
    let current = normalize_cwd_match_path(current_cwd);
    let session = normalize_cwd_match_path(session_cwd);
    !current.is_empty()
        && !session.is_empty()
        && (current == session || current.starts_with(&format!("{session}/")))
}

fn normalize_cwd_match_path(path: &str) -> String {
    let normalized = path.trim().replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if cfg!(windows) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::projection::cutex_session_status_label_with_agents;

    fn live_native_agent(id: &str, session_id: &str) -> AgentBusAgent {
        AgentBusAgent {
            id: id.to_string(),
            name: "native.abcdef0".to_string(),
            base_name: Some("native".to_string()),
            thread_name: Some("native".to_string()),
            path_key: Some("abcdef0".to_string()),
            session_id: Some(session_id.to_string()),
            cutex_session_id: None,
            profile: "orchid".to_string(),
            cwd: "E:\\Projects (Orchid)\\waveline-backend".to_string(),
            pid: std::process::id(),
            host_id: None,
            groups: vec!["waveline".to_string()],
            registration_class: AgentRegistrationClass::Persistent,
            last_seen_epoch_secs: 42,
        }
    }

    #[test]
    fn stale_windows_native_endpoint_is_not_online() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.native".to_string(),
            Some("019e-native".to_string()),
            "WINBOX".to_string(),
            "E:\\Projects (Orchid)\\waveline-backend".to_string(),
            Some("orchid".to_string()),
            "2026-06-30T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("native".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        record.current_runtime_agent_id = Some("cutex.native.runtime".to_string());
        record.managed_cwd = Some("E:\\Projects (Orchid)\\waveline-backend".to_string());
        record.exposed_to_backend = true;

        assert_eq!(
            cutex_session_status_label_with_agents(&record, &[], &[]),
            "stale"
        );

        let mut store = CutexSessionStore::default();
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
        let actions = recommended_start_quick_actions_with_agents(
            &store,
            &[],
            &[],
            "E:\\Projects (Orchid)\\waveline-backend",
        );

        let action = actions
            .iter()
            .find(|action| action.key == "cutex.native")
            .expect("native action");
        assert_eq!(action.kind, StartQuickActionKind::VisibleTui);
    }

    #[test]
    fn explicit_stock_quick_action_opens_details_instead_of_generic_online() {
        let mut record = CutexSessionRecord::new_at(
            "cutex.stock".to_string(),
            Some("019e-stock".to_string()),
            "devbox".to_string(),
            "/tmp/stock".to_string(),
            Some("orchid".to_string()),
            "2026-06-30T00:00:00Z".to_string(),
        )
        .expect("record");
        record.registration_class = AgentRegistrationClass::Persistent;
        record.explicit_launch = Some(crate::agent_management::ExplicitLaunchContract {
            version: 1,
            migration_action_id: None,
            native_id: "019e-stock".to_string(),
            native_home: "/tmp/stock-home".into(),
            bundle_manifest: "/tmp/stock-bundle.json".into(),
            bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });

        assert_eq!(
            primary_start_action_kind_for_record(&record, &[]),
            Some(StartQuickActionKind::OpenDetails)
        );
    }

    #[test]
    fn start_quick_actions_use_user_selection_not_heartbeat_recency() {
        let mut store = CutexSessionStore::default();
        let timestamp = "2026-06-28T00:00:00Z".to_string();

        let mut pinned = CutexSessionRecord::new_at(
            "cutex.pinned".to_string(),
            Some("019e-pinned".to_string()),
            "devbox".to_string(),
            "/tmp/elsewhere".to_string(),
            Some("orchid".to_string()),
            timestamp.clone(),
        )
        .expect("pinned record");
        pinned.display_name_hint = Some("pinned".to_string());
        pinned.quick_action = CutexSessionQuickActionMode::Pinned;
        store
            .sessions
            .insert(pinned.cutex_session_id.clone(), pinned);

        let mut cwd_match = CutexSessionRecord::new_at(
            "cutex.cwd".to_string(),
            Some("019e-cwd".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
            timestamp.clone(),
        )
        .expect("cwd record");
        cwd_match.display_name_hint = Some("cwd-match".to_string());
        cwd_match.exposed_to_backend = true;
        store
            .sessions
            .insert(cwd_match.cutex_session_id.clone(), cwd_match);

        let mut local_cwd = CutexSessionRecord::new_at(
            "cutex.local-cwd".to_string(),
            Some("019e-local-cwd".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
            timestamp.clone(),
        )
        .expect("local cwd record");
        local_cwd.display_name_hint = Some("local-cwd".to_string());
        store
            .sessions
            .insert(local_cwd.cutex_session_id.clone(), local_cwd);

        let mut heartbeat_only = CutexSessionRecord::new_at(
            "cutex.heartbeat".to_string(),
            Some("019e-heartbeat".to_string()),
            "devbox".to_string(),
            "/tmp/heartbeat".to_string(),
            Some("orchid".to_string()),
            timestamp.clone(),
        )
        .expect("heartbeat record");
        heartbeat_only.display_name_hint = Some("heartbeat".to_string());
        heartbeat_only.last_seen_at = Some("2026-06-28T00:10:00Z".to_string());
        store
            .sessions
            .insert(heartbeat_only.cutex_session_id.clone(), heartbeat_only);

        let mut child_cwd = CutexSessionRecord::new_at(
            "cutex.child".to_string(),
            Some("019e-child".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex/scripts".to_string(),
            Some("orchid".to_string()),
            timestamp.clone(),
        )
        .expect("child cwd record");
        child_cwd.display_name_hint = Some("child-cwd".to_string());
        child_cwd.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        child_cwd.alden_session_name = Some("cutex.child.runtime".to_string());
        child_cwd.alden_pid = Some(std::process::id());
        store
            .sessions
            .insert(child_cwd.cutex_session_id.clone(), child_cwd);

        let mut hidden = CutexSessionRecord::new_at(
            "cutex.hidden".to_string(),
            Some("019e-hidden".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
            timestamp,
        )
        .expect("hidden record");
        hidden.display_name_hint = Some("hidden".to_string());
        hidden.quick_action = CutexSessionQuickActionMode::Hidden;
        store
            .sessions
            .insert(hidden.cutex_session_id.clone(), hidden);

        let alden_sessions = vec![CuteAldenSession {
            pid: std::process::id(),
            name: Some("cutex.child.runtime".to_string()),
        }];
        let actions = recommended_start_quick_actions(
            &store,
            &alden_sessions,
            "/home/example/Projects/cutex/scripts",
        );
        let keys = actions
            .iter()
            .map(|action| action.key.as_str())
            .collect::<Vec<_>>();

        assert!(keys.contains(&"cutex.pinned"));
        assert!(keys.contains(&"cutex.cwd"));
        assert!(!keys.contains(&"cutex.local-cwd"));
        assert!(!keys.contains(&"cutex.heartbeat"));
        assert!(!keys.contains(&"cutex.hidden"));

        let pinned_action = actions
            .iter()
            .find(|action| action.key == "cutex.pinned")
            .expect("pinned action");
        assert_eq!(pinned_action.kind, StartQuickActionKind::ResumeHere);

        let child_action = actions
            .iter()
            .find(|action| action.key == "cutex.child")
            .expect("child action");
        assert!(StartQuickActionKind::Attach.opens_detail_first());
        assert!(StartQuickActionKind::Takeover.opens_detail_first());
        assert_eq!(StartQuickActionKind::Attach.start_menu_label(), "open");
        assert_eq!(StartQuickActionKind::Takeover.start_menu_label(), "open");
        assert_eq!(child_action.kind, StartQuickActionKind::ResumeAttach);
        assert!(!StartQuickActionKind::ResumeAttach.opens_detail_first());
        assert_eq!(
            StartQuickActionKind::ResumeAttach.start_menu_label(),
            "takeover"
        );
        assert!(!StartQuickActionKind::Online.opens_detail_first());
        assert_eq!(StartQuickActionKind::Online.start_menu_label(), "online");
        assert_eq!(
            StartQuickActionKind::VisibleTui.start_menu_label(),
            "open TUI"
        );
        assert_eq!(
            StartQuickActionKind::ResumeHere.start_menu_label(),
            "resume here"
        );
        assert_eq!(
            StartQuickActionKind::ResumeManaged.start_menu_label(),
            "resume managed"
        );
    }

    #[test]
    fn linux_alden_quick_action_displays_takeover_instead_of_raw_online() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.alden".to_string(),
            Some("019e-alden".to_string()),
            "devbox".to_string(),
            "/home/example/Projects/cutex".to_string(),
            Some("orchid".to_string()),
            "2026-06-30T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("alden".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record.exposed_to_backend = true;
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);

        let actions = recommended_start_quick_actions(&store, &[], "/home/example/Projects/cutex");

        let action = actions
            .iter()
            .find(|action| action.key == "cutex.alden")
            .expect("alden action");
        assert_eq!(action.kind, StartQuickActionKind::ResumeAttach);
        assert_eq!(action.kind.start_menu_label(), "takeover");
    }

    #[test]
    fn live_windows_native_quick_action_opens_tui_without_takeover() {
        let mut store = CutexSessionStore::default();
        let mut record = CutexSessionRecord::new_at(
            "cutex.native".to_string(),
            Some("019e-native".to_string()),
            "WINBOX".to_string(),
            "E:\\Projects (Orchid)\\waveline-backend".to_string(),
            Some("orchid".to_string()),
            "2026-06-30T00:00:00Z".to_string(),
        )
        .expect("record");
        record.display_name_hint = Some("native".to_string());
        record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        record.current_runtime_agent_id = Some("cutex.native.runtime".to_string());
        record.exposed_to_backend = true;
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);

        let actions = recommended_start_quick_actions_with_agents(
            &store,
            &[],
            &[live_native_agent("cutex.native.runtime", "019e-native")],
            "E:\\Projects (Orchid)\\waveline-backend",
        );

        let action = actions
            .iter()
            .find(|action| action.key == "cutex.native")
            .expect("native action");
        assert_eq!(action.kind, StartQuickActionKind::VisibleTui);
        assert!(!action.kind.opens_detail_first());
        assert_eq!(action.kind.start_menu_label(), "open TUI");
        let record = store.sessions.get("cutex.native").expect("record");
        assert_eq!(
            cutex_session_status_label_with_agents(
                record,
                &[],
                &[live_native_agent("cutex.native.runtime", "019e-native")]
            ),
            "online"
        );
    }

    #[test]
    fn per_record_primary_action_preserves_platform_specific_labels() {
        let mut alden = CutexSessionRecord::new_at(
            "cutex.alden-primary".to_string(),
            Some("019e-alden-primary".to_string()),
            "devbox".to_string(),
            "/tmp/alden".to_string(),
            Some("orchid".to_string()),
            "2026-08-05T00:00:00Z".to_string(),
        )
        .expect("alden record");
        alden.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        assert_eq!(
            primary_start_action_kind_for_record(&alden, &[])
                .expect("alden primary")
                .menu_label(),
            "takeover"
        );

        let mut native = alden;
        native.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        assert_eq!(
            primary_start_action_kind_for_record(&native, &[])
                .expect("native primary")
                .menu_label(),
            "open TUI"
        );
    }
}
