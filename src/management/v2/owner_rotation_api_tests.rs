//! r254 (design 044, review #1): the owner rotation route's real rebind
//! effect ([`DaemonEffects::rebind`], used by `/v2/director-rotations`)
//! moves the Director through the Director-address hook.

use super::*;
use crate::agent_management::AgentManagementProvider;
use crate::director_address::management::{self as da, tests as fx, ExtensionStateStore};

static PROVIDER: OnceLock<AgentManagementProvider> = OnceLock::new();

fn provider() -> &'static AgentManagementProvider {
    PROVIDER.get().expect("installed by the test")
}

fn context() -> crate::management::server::ManagementRequestContext {
    crate::management::server::ManagementRequestContext {
        explicit_launch_action: |_, _| panic!("unused"),
        adopt_saved_native: |_, _| panic!("unused"),
        review_agent_archive: |_, _| panic!("unused"),
        owner_archive_action: |_, _| panic!("unexpected permanent archive"),
        execute_agent_archive: |_, _| panic!("unused"),
        durable_agent_candidates: |_| panic!("unused"),
        import_durable_agent: |_, _| panic!("unused"),
        load_registry: || panic!("unused"),
        load_runtime_status: |_| panic!("unused"),
        forward_native_request: |_, _| panic!("unused"),
        respond_native_server_request: |_, _, _| panic!("unused"),
        handle_user_input: |_| panic!("unused"),
        flush_user_input_queue: |_, _, _| panic!("unused"),
        steer_user_input_queue: |_, _, _| panic!("unused"),
        interrupt_active_turn: |_, _| panic!("unused"),
        load_bootstrap_state: |_, _| panic!("unused"),
        load_latest_plan: |_| panic!("unused"),
        load_latest_token_usage: |_| panic!("unused"),
        revert_session_history: |_, _, _| panic!("unused"),
        delete_session: |_, _| panic!("unused"),
        fork_session: |_, _, _| panic!("unused"),
        side_chat: |_, _, _| panic!("unused"),
        open_offline_history: |_| panic!("unused"),
        mutate_session: |_, _, _| panic!("unused"),
        online_session: |_, _, _| panic!("unused"),
        bind_project_authority: |_| panic!("unused"),
        import_legacy_director_ownership: |_| panic!("unused"),
        reconcile_agent_reservation: |_| panic!("unused"),
        abandon_agent_create: |_| panic!("unused"),
        list_management_projects: |principal| provider().list_cutex_projects_for_management(principal),
        read_management_project: |_, _| panic!("unused"),
        update_management_project_presentation: |_, _| panic!("unused"),
        execute_management_operator_action: |_, _| panic!("unused"),
        execute_management_project_mutation: |principal, request| {
            provider().execute_project_mutation_for_management(principal, request)
        },
        create_human_agent: |_, _| panic!("unused"),
        create_human_session: |_, _| panic!("unused"),
    }
}

#[test]
fn the_owner_rotation_rebind_effect_prepares_finalizes_notifies_and_hands_over() {
    use crate::management::control_plane::HumanManagementProjectMutationKind as K;
    let root = fx::provider_root("owner-route");
    let (store, hook, calls) = fx::recording_hook(&root);
    let installed = fx::owner_provider(&root, hook);
    fx::run_owner(&installed, &fx::owner_mutation("create", None, K::CreateProject {
        presentation: crate::agent_management::ProjectPresentationInput {
            display_name: "P1".into(),
            badge_label: "P".into(),
            color: crate::agent_management::ProjectPaletteColor::Green,
        },
        workspace_root: None,
        director_cutex_session_id: Some(fx::sid("a")),
        create_workspace_root: false,
    }))
    .unwrap();
    assert!(PROVIDER.set(installed).is_ok(), "one route test per process");
    let effects = DaemonEffects { context: context() };
    let action = "owner-rotation:rot-1:rebind";
    // exactly what Driver::rebind calls for a rotation of p1 to cutex.b
    assert_eq!(effects.rebind("p1", action, 1, "cutex.b").unwrap(), 2);
    // a resumed driver replays the same action: no second change
    assert_eq!(effects.rebind("p1", action, 1, "cutex.b").unwrap(), 2);
    let ext = store.read().unwrap();
    let t = &ext.transitions[&da::change_id(&fx::pid("p1"), &fx::aid(action))];
    assert_eq!(
        (t.state, t.reason, t.handover, t.old_director.clone(), t.new_director.clone()),
        (
            da::TransitionState::Finalized,
            da::ChangeReason::OwnerAssigned,
            da::HandoverPolicy::Review,
            Some(fx::sid("a")),
            Some(fx::sid("b"))
        )
    );
    assert!(t.handover_dispatched && t.event_emitted);
    for member in ["a", "b"] {
        let notice = &ext.notices[&da::notice_key(&fx::pid("p1"), 2, &fx::sid(member))];
        assert!(notice.passive && !notice.wake && notice.state == da::NoticeState::Pending);
    }
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            "owner-scope p1 None->Some(\"cutex.a\")".to_string(),
            "event Some(\"cutex.a\")->Some(\"cutex.b\") cleared=false".into(),
            "handover p1 cutex.a->cutex.b trusted=false".into(),
        ]
    );
    // a stale rotation (epoch fence) changes nothing and prepares nothing
    assert!(effects.rebind("p1", "owner-rotation:rot-2:rebind", 1, "cutex.c").is_err());
    assert!(!store.read().unwrap().transitions.contains_key(&da::change_id(&fx::pid("p1"), &fx::aid("owner-rotation:rot-2:rebind"))));
}
