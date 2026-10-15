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

    let filter =
        crate::cli_app::session::cutex_session_list_filter_from_args(&SessionListArgs::default());
    let (records, hidden) = filtered_cutex_session_records(&store, &alden_sessions, &filter);
    let ids = records
        .iter()
        .map(|(_, record)| record.cutex_session_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["cutex.attachable", "cutex.persistent"]);
    assert_eq!(hidden, 1);

    let all = SessionListArgs {
        all: true,
        sort: SessionListSort::Name,
        ..SessionListArgs::default()
    };
    let all_filter = crate::cli_app::session::cutex_session_list_filter_from_args(&all);
    let (records, hidden) = filtered_cutex_session_records(&store, &alden_sessions, &all_filter);
    let ids = records
        .iter()
        .map(|(_, record)| record.cutex_session_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["cutex.attachable", "cutex.historical", "cutex.persistent"]
    );
    assert_eq!(hidden, 0);

    let group_filter = SessionListArgs {
        groups: vec!["aria".to_string()],
        ..SessionListArgs::default()
    };
    let group_filter = crate::cli_app::session::cutex_session_list_filter_from_args(&group_filter);
    let (records, hidden) = filtered_cutex_session_records(&store, &alden_sessions, &group_filter);
    let ids = records
        .iter()
        .map(|(_, record)| record.cutex_session_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["cutex.persistent"]);
    assert_eq!(hidden, 2);
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
    let actions =
        recommended_start_quick_actions(&store, &alden_sessions, "/home/example/Projects/cutex");
    let keys = actions
        .iter()
        .map(|action| action.key.as_str())
        .collect::<Vec<_>>();

    assert!(keys.contains(&"cutex.pinned"));
    assert!(keys.contains(&"cutex.cwd"));
    assert!(!keys.contains(&"cutex.local-cwd"));
    assert!(!keys.contains(&"cutex.heartbeat"));
    assert!(!keys.contains(&"cutex.child"));
    assert!(!keys.contains(&"cutex.hidden"));

    let pinned_action = actions
        .iter()
        .find(|action| action.key == "cutex.pinned")
        .expect("pinned action should exist");
    assert_eq!(pinned_action.kind, StartQuickActionKind::ResumeHere);
}

#[test]
fn start_quick_attach_actions_open_details_before_terminal_takeover() {
    assert!(StartQuickActionKind::Attach.opens_detail_first());
    assert!(StartQuickActionKind::Takeover.opens_detail_first());
    assert_eq!(StartQuickActionKind::Attach.start_menu_label(), "open");
    assert_eq!(StartQuickActionKind::Takeover.start_menu_label(), "open");
    assert_eq!(
        StartQuickActionKind::ResumeAttach.start_menu_label(),
        "takeover"
    );

    assert!(!StartQuickActionKind::Online.opens_detail_first());
    assert_eq!(StartQuickActionKind::Online.start_menu_label(), "online");
    assert_eq!(
        StartQuickActionKind::ResumeHere.start_menu_label(),
        "resume here"
    );
    assert_eq!(
        StartQuickActionKind::ResumeManaged.start_menu_label(),
        "resume managed"
    );
}
