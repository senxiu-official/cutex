use super::*;

#[test]
fn cutex_session_reconcile_creates_record_from_live_agent() {
    let mut store = CutexSessionStore::default();
    let mut agent = sample_bus_agent(
        "cutex.orchid.scgpt.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-alpha".to_string());
    agent.thread_name = Some("aria-data".to_string());
    agent.groups = vec!["project:scgpt".to_string(), "aria".to_string()];

    let outcome = reconcile_cutex_session_store_from_agent(
        &mut store,
        &agent,
        "devbox",
        "2026-06-25T00:00:00Z",
    )
    .expect("reconcile should succeed")
    .expect("agent with session id should reconcile");

    assert_eq!(outcome.cutex_session_id, "cutex.019e-alpha");
    assert_eq!(outcome.codex_session_id, "019e-alpha");
    assert_eq!(outcome.events.len(), 1);
    assert_eq!(outcome.events[0].event_type, "runtime_endpoint_registered");
    let record = store
        .sessions
        .get("cutex.019e-alpha")
        .expect("session should exist");
    assert_eq!(record.codex_session_id.as_deref(), Some("019e-alpha"));
    assert_eq!(
        record.current_runtime_agent_id.as_deref(),
        Some("cutex.orchid.scgpt.runtime1")
    );
    assert_eq!(record.thread_name.as_deref(), Some("aria-data"));
    assert_eq!(record.host_id, "devbox");
    assert_eq!(record.profile, None);
    assert!(record.agent_enabled);
    assert_eq!(record.agent_groups, agent.groups);
    assert_eq!(record.runtime_generation, 1);
}

#[test]
fn runtime_and_im_reconciliation_preserve_session_profile_inheritance_and_overrides() {
    let mut store = CutexSessionStore::default();
    let mut inherited = CutexSessionRecord::new_at(
        "cutex.019e-inherited".to_string(),
        Some("019e-inherited".to_string()),
        "devbox".to_string(),
        "/tmp/inherited".to_string(),
        None,
        "2026-08-07T00:00:00Z".to_string(),
    )
    .expect("inherited record");
    inherited.registration_class = AgentRegistrationClass::Persistent;
    let mut explicit = CutexSessionRecord::new_at(
        "cutex.019e-explicit".to_string(),
        Some("019e-explicit".to_string()),
        "devbox".to_string(),
        "/tmp/explicit".to_string(),
        Some("relay".to_string()),
        "2026-08-07T00:00:00Z".to_string(),
    )
    .expect("explicit record");
    explicit.registration_class = AgentRegistrationClass::Persistent;
    store
        .sessions
        .insert(inherited.cutex_session_id.clone(), inherited);
    store
        .sessions
        .insert(explicit.cutex_session_id.clone(), explicit);

    for (session_id, expected) in [("019e-inherited", None), ("019e-explicit", Some("relay"))] {
        let mut agent = sample_bus_agent(
            &format!("cutex.orchid.{session_id}"),
            "profile-test.abcdef0",
            Some("profile-test"),
            Some("abcdef0"),
        );
        agent.session_id = Some(session_id.to_string());
        agent.profile = "orchid".to_string();
        reconcile_cutex_session_store_from_agent(
            &mut store,
            &agent,
            "devbox",
            "2026-08-07T00:01:00Z",
        )
        .expect("agent reconcile");

        let mut entry = sample_im_registration(session_id);
        entry.profile = Some("orchid".to_string());
        reconcile_cutex_session_store_from_im_registration(
            &mut store,
            &entry,
            "2026-08-07T00:02:00Z",
        )
        .expect("IM reconcile");

        assert_eq!(
            store.sessions[&format!("cutex.{session_id}")]
                .profile
                .as_deref(),
            expected
        );
    }
}

#[test]
fn cutex_session_reconcile_preserves_persistent_registration_class() {
    let mut store = CutexSessionStore::default();
    let entry = sample_im_registration("019e-alpha");
    reconcile_cutex_session_store_from_im_registration(&mut store, &entry, "2026-06-25T00:00:00Z")
        .expect("IM reconcile should succeed");

    let mut agent = sample_bus_agent(
        "cutex.orchid.scgpt.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-alpha".to_string());
    agent.registration_class = AgentRegistrationClass::LocalOnly;

    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:01:00Z")
        .expect("agent reconcile should succeed")
        .expect("agent with session id should reconcile");

    let record = store
        .sessions
        .get("cutex.019e-alpha")
        .expect("session should exist");
    assert_eq!(
        record.registration_class,
        AgentRegistrationClass::Persistent
    );
    assert_eq!(
        record.current_runtime_agent_id.as_deref(),
        Some("cutex.orchid.scgpt.runtime1")
    );
}

#[test]
fn im_reconcile_preserves_explicit_managed_runtime_backend() {
    let mut store = CutexSessionStore::default();
    let entry = sample_im_registration("019e-alpha");
    reconcile_cutex_session_store_from_im_registration(&mut store, &entry, "2026-06-25T00:00:00Z")
        .expect("initial IM reconcile should succeed");
    store
        .sessions
        .get_mut("cutex.019e-alpha")
        .expect("session should exist")
        .runtime_backend = CutexSessionRuntimeBackend::Host;

    reconcile_cutex_session_store_from_im_registration(&mut store, &entry, "2026-06-25T00:01:00Z")
        .expect("repeat IM reconcile should succeed");

    assert_eq!(
        store
            .sessions
            .get("cutex.019e-alpha")
            .expect("session should exist")
            .runtime_backend,
        CutexSessionRuntimeBackend::Host
    );
}

#[test]
fn cutex_session_reconcile_refreshes_same_endpoint_without_event() {
    let mut store = CutexSessionStore::default();
    let mut agent = sample_bus_agent(
        "cutex.orchid.scgpt.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-alpha".to_string());

    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:00:00Z")
        .expect("initial reconcile should succeed");
    let outcome = reconcile_cutex_session_store_from_agent(
        &mut store,
        &agent,
        "devbox",
        "2026-06-25T00:01:00Z",
    )
    .expect("refresh should succeed")
    .expect("agent with session id should reconcile");

    assert!(outcome.events.is_empty());
    let record = store
        .sessions
        .get("cutex.019e-alpha")
        .expect("session should exist");
    assert_eq!(record.runtime_generation, 1);
    assert_eq!(record.durable_revision(), 1);
    assert_eq!(record.last_seen_at.as_deref(), Some("2026-06-25T00:01:00Z"));
}

#[test]
fn agent_reconcile_revisions_only_effective_durable_changes() {
    let mut store = CutexSessionStore::default();
    let mut agent = sample_bus_agent(
        "cutex.orchid.scgpt.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-revision".to_string());
    agent.groups = vec!["aria".to_string()];

    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:00:00Z")
        .expect("initial reconcile");
    assert_eq!(store.sessions["cutex.019e-revision"].durable_revision(), 1);

    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:01:00Z")
        .expect("heartbeat reconcile");
    assert_eq!(store.sessions["cutex.019e-revision"].durable_revision(), 1);

    agent.groups.push("waveline".to_string());
    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:02:00Z")
        .expect("durable groups reconcile");
    assert_eq!(store.sessions["cutex.019e-revision"].durable_revision(), 2);

    agent.id = "cutex.orchid.scgpt.runtime2".to_string();
    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:03:00Z")
        .expect("runtime endpoint reconcile");
    let record = &store.sessions["cutex.019e-revision"];
    assert_eq!(record.durable_revision(), 2);
    assert_eq!(record.runtime_generation, 2);
}

#[test]
fn retired_session_ignores_agent_reconciliation() {
    let mut store = CutexSessionStore::default();
    let mut agent = sample_bus_agent(
        "cutex.orchid.scgpt.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-retired".to_string());
    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:00:00Z")
        .expect("initial reconcile");
    let record = store
        .sessions
        .get_mut("cutex.019e-retired")
        .expect("record");
    record.archive_state = cutex::session::model::CutexSessionArchiveState::Retired;
    record.retired_at = Some("2026-06-25T00:01:00Z".to_string());
    let original = record.clone();

    agent.id = "cutex.orchid.scgpt.runtime2".to_string();
    agent.groups.push("must-not-apply".to_string());
    assert!(reconcile_cutex_session_store_from_agent(
        &mut store,
        &agent,
        "devbox",
        "2026-06-25T00:02:00Z",
    )
    .expect("retired reconcile")
    .is_none());
    assert_eq!(store.sessions["cutex.019e-retired"], original);
}

#[test]
fn cutex_session_reconcile_rebinds_runtime_endpoint_after_internal_resume() {
    let mut store = CutexSessionStore::default();
    let mut agent = sample_bus_agent(
        "cutex.orchid.scgpt.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-alpha".to_string());

    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:00:00Z")
        .expect("initial reconcile should succeed");

    agent.session_id = Some("019e-beta".to_string());
    agent.thread_name = Some("aria-eval".to_string());
    let outcome = reconcile_cutex_session_store_from_agent(
        &mut store,
        &agent,
        "devbox",
        "2026-06-25T00:02:00Z",
    )
    .expect("resume reconcile should succeed")
    .expect("agent with new session id should reconcile");

    assert_eq!(outcome.cutex_session_id, "cutex.019e-beta");
    assert!(outcome
        .events
        .iter()
        .any(|event| event.event_type == "cutex_session_rebound"));
    assert!(outcome
        .events
        .iter()
        .any(|event| event.event_type == "runtime_endpoint_registered"));

    let old_record = store
        .sessions
        .get("cutex.019e-alpha")
        .expect("old session should remain");
    assert!(old_record.current_runtime_agent_id.is_none());
    assert_eq!(
        old_record.last_runtime_agent_id.as_deref(),
        Some("cutex.orchid.scgpt.runtime1")
    );

    let new_record = store
        .sessions
        .get("cutex.019e-beta")
        .expect("new session should exist");
    assert_eq!(
        new_record.current_runtime_agent_id.as_deref(),
        Some("cutex.orchid.scgpt.runtime1")
    );
    assert_eq!(new_record.thread_name.as_deref(), Some("aria-eval"));
    assert_eq!(new_record.runtime_generation, 1);
}

#[test]
fn cutex_session_reconcile_marks_im_registration_exposure() {
    let mut store = CutexSessionStore::default();
    let entry = sample_im_registration("019e-alpha");

    reconcile_cutex_session_store_from_im_registration(&mut store, &entry, "2026-06-25T00:00:00Z")
        .expect("IM reconcile should succeed");

    let record = store
        .sessions
        .get("cutex.019e-alpha")
        .expect("session should exist");
    assert_eq!(record.display_name_hint.as_deref(), Some("aria-data"));
    assert_eq!(record.agent_groups, entry.groups);
    assert!(record.exposed_to_backend);
    assert_eq!(
        record.registration_class,
        AgentRegistrationClass::Persistent
    );
    assert_eq!(
        record.runtime_backend,
        default_managed_session_runtime_backend()
    );
    assert!(record.agent_enabled);
}

#[test]
fn cutex_session_im_reconcile_hides_without_removing_runtime_endpoint() {
    let mut store = CutexSessionStore::default();
    let mut agent = sample_bus_agent(
        "cutex.orchid.scgpt.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-alpha".to_string());
    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:00:00Z")
        .expect("agent reconcile should succeed");

    let mut entry = sample_im_registration("019e-alpha");
    entry.visible = false;
    reconcile_cutex_session_store_from_im_registration(&mut store, &entry, "2026-06-25T00:03:00Z")
        .expect("IM reconcile should succeed");

    let record = store
        .sessions
        .get("cutex.019e-alpha")
        .expect("session should exist");
    assert!(!record.exposed_to_backend);
    assert_eq!(
        record.current_runtime_agent_id.as_deref(),
        Some("cutex.orchid.scgpt.runtime1")
    );
    assert!(record.agent_enabled);
}

#[test]
fn agent_reconcile_preserves_original_cwd_when_managed_cwd_is_set() {
    let mut store = CutexSessionStore::default();
    let mut record = CutexSessionRecord::new_at(
        "cutex.019e-alpha".to_string(),
        Some("019e-alpha".to_string()),
        "devbox".to_string(),
        "/tmp/session-original".to_string(),
        Some("orchid".to_string()),
        "2026-06-25T00:00:00Z".to_string(),
    )
    .expect("record should be created");
    record.managed_cwd = Some("/tmp/session-managed".to_string());
    store
        .sessions
        .insert(record.cutex_session_id.clone(), record);

    let mut agent = sample_bus_agent(
        "cutex.orchid.session-managed.runtime1",
        "aria-data.abcdef0",
        Some("aria-data"),
        Some("abcdef0"),
    );
    agent.session_id = Some("019e-alpha".to_string());
    agent.cwd = "/tmp/session-managed".to_string();

    reconcile_cutex_session_store_from_agent(&mut store, &agent, "devbox", "2026-06-25T00:03:00Z")
        .expect("agent reconcile should succeed");

    let record = store
        .sessions
        .get("cutex.019e-alpha")
        .expect("session should exist");
    assert_eq!(record.cwd, "/tmp/session-original");
    assert_eq!(record.managed_cwd.as_deref(), Some("/tmp/session-managed"));
    assert_eq!(cutex_session_launch_cwd(record), "/tmp/session-managed");
}
