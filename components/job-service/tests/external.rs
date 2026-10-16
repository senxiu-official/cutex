use cutex_job_service::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
const API: &[u8] = b"test-api-credential-at-least-32-bytes";
const KEY: &[u8] = b"test-grant-credential-at-least-32-bytes";
fn config(path: &std::path::Path) -> ServiceConfig {
    let launcher = std::fs::canonicalize("/bin/true").unwrap();
    ServiceConfig {
        state_root: path.into(),
        grant_key: KEY.into(),
        api_token: API.into(),
        runner_executable: launcher.clone(),
        per_stream_output_bytes: 4096,
        max_read_bytes: 4096,
        cancel_grace: Duration::from_millis(100),
        max_jobs: 32,
        max_active_jobs: 4,
        allowed_launchers: [(
            launcher.to_string_lossy().into_owned(),
            file_sha256(&launcher).unwrap(),
        )]
        .into(),
        completion_wire_version: CompletionWireVersion::V2,
        completion_enabled: true,
        external_sources: vec!["nci".into()],
        external_stale_after_secs: 86_400,
        external_notify_file: None,
    }
}
fn grant(subject: &str, op: CallerOperation, key: &str) -> CallerGrant {
    CallerGrantIssuer::new(KEY)
        .unwrap()
        .issue(
            subject.into(),
            op,
            key.into(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            60,
        )
        .unwrap()
}
fn prepare(service: &JobService) -> PrepareRemoteReceipt {
    service
        .prepare_remote(
            API,
            &grant("cutex.alice", CallerOperation::PrepareRemote, "nci"),
            "nci",
        )
        .unwrap()
}
fn request(prepared: &PrepareRemoteReceipt) -> RemoteJobRequest {
    RemoteJobRequest {
        source: "nci".into(),
        correlation_token: prepared.correlation_token.clone(),
        remote_id: "123.gadi-pbs".into(),
        remote_name: Some(prepared.suggested_job_name.clone()),
        project: None,
        queue: None,
        remote_output_path: Some("/scratch/project/result.txt".into()),
    }
}
fn register(service: &JobService, prepared: &PrepareRemoteReceipt) -> SubmitReceipt {
    service
        .register_remote(
            API,
            &grant(
                "cutex.alice",
                CallerOperation::RegisterRemote,
                &remote_request_sha256(&request(prepared)).unwrap(),
            ),
            request(prepared),
        )
        .unwrap()
}
#[test]
fn source_default_off_and_authenticated_preparation() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.external_sources.clear();
    let service = JobService::open(cfg).unwrap();
    assert!(
        service
            .prepare_remote(
                API,
                &grant("cutex.alice", CallerOperation::PrepareRemote, "nci"),
                "nci"
            )
            .is_err()
    );
    assert!(
        service.capabilities(API).unwrap()["sources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn prepare_register_is_scoped_one_use_and_exact_retry_is_durable() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    assert_eq!(prepared.correlation_token.len(), 32);
    let caller = grant(
        "cutex.alice",
        CallerOperation::RegisterRemote,
        &remote_request_sha256(&request(&prepared)).unwrap(),
    );
    assert!(
        service
            .register_remote(
                API,
                &grant(
                    "cutex.bob",
                    CallerOperation::RegisterRemote,
                    &prepared.correlation_token
                ),
                request(&prepared)
            )
            .is_err()
    );
    assert!(
        service
            .register_remote(
                API,
                &grant(
                    "cutex.alice",
                    CallerOperation::Query,
                    &prepared.correlation_token
                ),
                request(&prepared)
            )
            .is_err()
    );
    let mut tampered = request(&prepared);
    tampered.remote_output_path = Some("/scratch/other".into());
    assert!(service.register_remote(API, &caller, tampered).is_err());
    let receipt = register(&service, &prepared);
    assert_eq!(receipt.job.state, JobState::Registered);
    assert!(receipt.job.process_id.is_none());
    assert!(service.pending_outbox(API).unwrap().is_empty());
    let mut changed = request(&prepared);
    changed.remote_id = "456.gadi-pbs".into();
    assert!(service.register_remote(API, &caller, changed).is_err());
    drop(service);
    let service = JobService::open(cfg).unwrap();
    let retry = register(&service, &prepared);
    assert!(retry.deduplicated);
    assert_eq!(retry.job.job_id, receipt.job.job_id);
    assert!(
        service
            .query_for(
                API,
                &grant("cutex.bob", CallerOperation::Query, &receipt.job.job_id),
                &receipt.job.job_id
            )
            .is_err()
    );
    assert!(service.cancel(API, &receipt.job.job_id, 1).is_err());
    assert!(
        service
            .read_output(API, &receipt.job.job_id, "stdout", 0, 100)
            .is_err()
    );
}
#[test]
fn owner_mark_is_revision_guarded_audited_and_freezes_v2_without_untrusted_text() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    assert!(
        service
            .human_mark_external(API, &job.job_id, 2, JobState::Exited, Some(0), None)
            .is_err()
    );
    assert!(
        service
            .human_mark_external(API, &job.job_id, 1, JobState::Running, None, None)
            .is_err()
    );
    assert!(
        service
            .human_mark_external(API, &job.job_id, 1, JobState::Exited, None, None)
            .is_err()
    );
    let marked = service
        .human_mark_external(
            API,
            &job.job_id,
            1,
            JobState::Exited,
            Some(0),
            Some("private owner note".into()),
        )
        .unwrap();
    assert_eq!(marked.revision, 2);
    assert_eq!(marked.external.unwrap().human_audit.len(), 1);
    assert!(
        service
            .human_mark_external(API, &job.job_id, 2, JobState::Failed, None, None)
            .is_err()
    );
    let outbox = service.pending_outbox(API).unwrap();
    assert_eq!(outbox.len(), 1);
    let body = outbox[0].frozen_request_v2.as_ref().unwrap();
    assert_eq!(body.target_cutex_session_id, "cutex.alice");
    assert_eq!(body.facts.exit_code, Some(0));
    let text = serde_json::to_string(body).unwrap();
    assert!(!text.contains("private owner note"));
    assert_eq!(body.output_reference, "/scratch/project/result.txt");
    assert!(!text.contains(&prepared.correlation_token));
    drop(service);
    let service = JobService::open(cfg).unwrap();
    assert_eq!(service.pending_outbox(API).unwrap(), outbox);
}
#[test]
fn stale_is_nonterminal_and_restart_preserves_remote_running() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.external_stale_after_secs = 1;
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    drop(service);
    let path = root.path().join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["jobs"][&job.job_id]["state"] = "running".into();
    state["jobs"][&job.job_id]["external"]["registeredAt"] = 1.into();
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let service = JobService::open(cfg).unwrap();
    let stale = service.query(API, &job.job_id).unwrap();
    assert_eq!(stale.state, JobState::Running);
    assert!(stale.external.as_ref().unwrap().stale);
    assert!(!stale.state.terminal());
    assert!(service.pending_outbox(API).unwrap().is_empty());
    let row = service.human_list(API, serde_json::json!({})).unwrap();
    assert_eq!(row["data"][0]["stale"], true);
    assert_eq!(row["data"][0]["source"], "nci");
    service
        .human_mark_external(
            API,
            &job.job_id,
            stale.revision,
            JobState::Cancelled,
            None,
            None,
        )
        .unwrap();
}

#[test]
fn stale_flip_does_not_bump_revision_for_owner_mark() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.external_stale_after_secs = 1;
    let service = JobService::open(cfg).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    let fresh = service.query(API, &job.job_id).unwrap();
    assert!(!fresh.external.as_ref().unwrap().stale);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let stale = service.query(API, &job.job_id).unwrap();
    assert!(stale.external.as_ref().unwrap().stale);
    assert_eq!(stale.revision, fresh.revision);
    let row = service.human_list(API, serde_json::json!({})).unwrap();
    assert_eq!(row["data"][0]["stale"], true);
    assert_eq!(row["data"][0]["revision"], fresh.revision);
    // The owner's desktop read `fresh` before the flag flipped; its CAS still holds.
    let marked = service
        .human_mark_external(
            API,
            &job.job_id,
            fresh.revision,
            JobState::Failed,
            Some(3),
            None,
        )
        .unwrap();
    assert_eq!(marked.revision, fresh.revision + 1);
    assert!(
        !service
            .query(API, &job.job_id)
            .unwrap()
            .external
            .unwrap()
            .stale
    );
}

#[test]
fn expired_preparation_and_invalid_registration_fields_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    for path in ["https://example.test/output", "/scratch/a\nInjected"] {
        let mut invalid = request(&prepared);
        invalid.remote_output_path = Some(path.into());
        let caller = grant(
            "cutex.alice",
            CallerOperation::RegisterRemote,
            &remote_request_sha256(&invalid).unwrap(),
        );
        assert!(service.register_remote(API, &caller, invalid).is_err());
    }
    let mut invalid = request(&prepared);
    invalid.remote_name = Some("missing-marker".into());
    let caller = grant(
        "cutex.alice",
        CallerOperation::RegisterRemote,
        &remote_request_sha256(&invalid).unwrap(),
    );
    assert!(service.register_remote(API, &caller, invalid).is_err());
    drop(service);
    let path = root.path().join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["preparedRemote"][&prepared.correlation_token]["expiresAt"] = 1.into();
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let service = JobService::open(cfg).unwrap();
    let caller = grant(
        "cutex.alice",
        CallerOperation::RegisterRemote,
        &remote_request_sha256(&request(&prepared)).unwrap(),
    );
    assert!(
        service
            .register_remote(API, &caller, request(&prepared))
            .is_err()
    );
}

#[test]
fn disabled_completion_freezes_v2_and_later_enable_preserves_body() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path());
    cfg.completion_enabled = false;
    cfg.completion_wire_version = CompletionWireVersion::V1;
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    service
        .human_mark_external(API, &job.job_id, 1, JobState::Failed, None, None)
        .unwrap();
    let before = service.pending_outbox(API).unwrap().remove(0);
    assert_eq!(before.wire_version, Some(2));
    assert_eq!(before.delivery_state, CompletionDeliveryState::Disabled);
    drop(service);
    cfg.completion_enabled = true;
    assert!(JobService::open(cfg.clone()).is_err());
    cfg.completion_wire_version = CompletionWireVersion::V2;
    let service = JobService::open(cfg).unwrap();
    let after = service.pending_outbox(API).unwrap().remove(0);
    assert_eq!(after.frozen_request_v2, before.frozen_request_v2);
    assert_eq!(after.event_id, before.event_id);
    assert_eq!(after.delivery_state, CompletionDeliveryState::Ready);
}

#[test]
fn agent_projection_hides_owner_notes_and_tokens_on_query_and_registration_replay() {
    let root = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let prepared = prepare(&service);
    let receipt = register(&service, &prepared);
    let initial = serde_json::to_string(&receipt).unwrap();
    assert!(!initial.contains(&prepared.correlation_token));
    assert!(!initial.contains("humanAudit"));
    service
        .human_mark_external(
            API,
            &receipt.job.job_id,
            1,
            JobState::Exited,
            Some(0),
            Some("owner private note".into()),
        )
        .unwrap();
    let owner = service.query(API, &receipt.job.job_id).unwrap();
    assert_eq!(
        owner.external.as_ref().unwrap().human_audit[0]
            .actor
            .as_ref()
            .unwrap()
            .credential_kind
            .as_str(),
        "job_service_root"
    );
    for job in [
        service
            .query_for(
                API,
                &grant("cutex.alice", CallerOperation::Query, &receipt.job.job_id),
                &receipt.job.job_id,
            )
            .unwrap(),
        register(&service, &prepared).job,
    ] {
        let text = serde_json::to_string(&job).unwrap();
        assert!(!text.contains("owner private note"));
        assert!(!text.contains("humanAudit"));
        assert!(!text.contains("correlationToken"));
        assert!(!text.contains(&prepared.correlation_token));
    }
    let outbox = service.pending_outbox(API).unwrap();
    assert_eq!(
        outbox[0]
            .frozen_request_v2
            .as_ref()
            .unwrap()
            .facts
            .remote_id
            .as_deref(),
        Some("123.gadi-pbs")
    );
    assert_eq!(
        service
            .human_list(API, serde_json::json!({"query":"123.gadi"}))
            .unwrap()["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn preparation_quota_is_per_subscriber_and_consumption_releases_slot() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    for _ in 1..16 {
        prepare(&service);
    }
    assert!(
        service
            .prepare_remote(
                API,
                &grant("cutex.alice", CallerOperation::PrepareRemote, "nci"),
                "nci"
            )
            .is_err()
    );
    service
        .prepare_remote(
            API,
            &grant("cutex.bob", CallerOperation::PrepareRemote, "nci"),
            "nci",
        )
        .unwrap();
    let receipt = register(&service, &prepared);
    prepare(&service);
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("state.json")).unwrap()).unwrap();
    assert!(
        persisted["preparedRemote"]
            .get(&prepared.correlation_token)
            .is_none()
    );
    drop(service);
    let service = JobService::open(cfg).unwrap();
    assert_eq!(register(&service, &prepared).job.job_id, receipt.job.job_id);
}

#[test]
fn expired_and_legacy_consumed_preparations_are_cleaned_without_losing_replay() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let receipt = register(&service, &prepared);
    drop(service);
    let path = root.path().join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for i in 0..32 {
        state["preparedRemote"][format!("expired-{i}")] = serde_json::json!({"source":"nci","subscriber":"cutex.alice","expiresAt":1,"jobId":null});
    }
    state["preparedRemote"][&prepared.correlation_token] = serde_json::json!({"source":"nci","subscriber":"cutex.alice","expiresAt":u64::MAX,"jobId":receipt.job.job_id});
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let service = JobService::open(cfg).unwrap();
    prepare(&service);
    let cleaned: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(cleaned["preparedRemote"].as_object().unwrap().len(), 1);
    assert!(register(&service, &prepared).deduplicated);
}

fn source_event(prepared: &PrepareRemoteReceipt, id: &str, state: JobState) -> ExternalSourceEvent {
    ExternalSourceEvent {
        source: "nci".into(),
        remote_id: "123.gadi-pbs".into(),
        correlation_token: prepared.correlation_token.clone(),
        event_id: id.into(),
        state,
        exit_code: if state == JobState::Exited {
            Some(0)
        } else {
            None
        },
        reason: None,
        resources: None,
    }
}

#[test]
fn source_event_unmatched_retry_auth_and_source_enable_are_enforced() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let event = source_event(&prepared, "mail-1", JobState::Running);
    assert!(
        service
            .ingest_external_event(b"wrong-token", event.clone())
            .is_err()
    );
    let unmatched = service.ingest_external_event(API, event.clone()).unwrap();
    assert_eq!(unmatched.status, "unmatched");
    assert!(unmatched.retryable);
    let job = register(&service, &prepared).job;
    let mut wrong = event.clone();
    wrong.remote_id = "456".into();
    assert_eq!(
        service.ingest_external_event(API, wrong).unwrap().status,
        "unmatched"
    );
    let applied = service.ingest_external_event(API, event.clone()).unwrap();
    assert_eq!(applied.status, "applied");
    assert_eq!(applied.job_id.as_deref(), Some(job.job_id.as_str()));
    let running = service.query(API, &job.job_id).unwrap();
    assert_eq!(running.state, JobState::Running);
    assert!(
        running
            .external
            .as_ref()
            .unwrap()
            .last_source_event_at
            .is_some()
    );
    assert!(running.process_id.is_none());
    drop(service);
    let mut disabled = cfg;
    disabled.external_sources.clear();
    let service = JobService::open(disabled).unwrap();
    assert_eq!(
        service.ingest_external_event(API, event).unwrap().status,
        "disabled_source"
    );
}

#[test]
fn source_event_deduplicates_and_terminal_completion_survives_restart() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    let running = source_event(&prepared, "mail-run", JobState::Running);
    service.ingest_external_event(API, running.clone()).unwrap();
    let mut finish = source_event(&prepared, "mail-exit", JobState::Failed);
    finish.exit_code = Some(271);
    finish.reason = Some(ExternalReason::NonzeroExit);
    finish.resources = Some(ExternalResources {
        cpu_seconds: Some(5),
        walltime_seconds: Some(10),
        memory_bytes: Some(1024),
        vmem_bytes: None,
        ncpus: Some(2),
        jobfs_bytes: Some(2048),
        cpu_percent: Some(50),
    });
    service.ingest_external_event(API, finish.clone()).unwrap();
    let terminal = service.query(API, &job.job_id).unwrap();
    assert_eq!(terminal.state, JobState::Failed);
    let outbox = service.pending_outbox(API).unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(
        outbox[0]
            .frozen_request_v2
            .as_ref()
            .unwrap()
            .facts
            .external_resources,
        finish.resources
    );
    assert_eq!(
        service.ingest_external_event(API, running).unwrap().status,
        "duplicate"
    );
    let late = source_event(&prepared, "mail-late-running", JobState::Running);
    assert_eq!(
        service.ingest_external_event(API, late).unwrap().status,
        "terminal_ignored"
    );
    let mut conflict = finish.clone();
    conflict.exit_code = Some(1);
    assert_eq!(
        service.ingest_external_event(API, conflict).unwrap().status,
        "permanent_conflict"
    );
    assert_eq!(
        service.query(API, &job.job_id).unwrap().revision,
        terminal.revision
    );
    drop(service);
    let service = JobService::open(cfg).unwrap();
    assert_eq!(
        service.ingest_external_event(API, finish).unwrap().status,
        "duplicate"
    );
    assert_eq!(service.pending_outbox(API).unwrap(), outbox);
}

#[test]
fn source_event_rejects_unbounded_resources_and_invalid_status_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    let mut event = source_event(&prepared, "mail-1", JobState::Exited);
    event.exit_code = Some(271);
    assert!(service.ingest_external_event(API, event.clone()).is_err());
    event.state = JobState::Failed;
    event.exit_code = Some(-1);
    assert!(service.ingest_external_event(API, event.clone()).is_err());
    event.exit_code = None;
    event.resources = Some(ExternalResources {
        cpu_seconds: Some(1_000_000_001),
        walltime_seconds: None,
        memory_bytes: None,
        vmem_bytes: None,
        ncpus: None,
        jobfs_bytes: None,
        cpu_percent: None,
    });
    assert!(service.ingest_external_event(API, event.clone()).is_err());
    assert_eq!(service.query(API, &job.job_id).unwrap().revision, 1);
    event.resources = None;
    event.reason = Some(ExternalReason::WalltimeExceeded);
    assert_eq!(
        service.ingest_external_event(API, event).unwrap().status,
        "applied"
    );
    let outbox = service.pending_outbox(API).unwrap();
    assert_eq!(
        outbox[0]
            .frozen_request_v2
            .as_ref()
            .unwrap()
            .facts
            .terminal_reason
            .as_deref(),
        Some("walltime_exceeded")
    );
    assert_eq!(
        outbox[0]
            .frozen_request_v2
            .as_ref()
            .unwrap()
            .facts
            .exit_code,
        None
    );
    assert!(
        serde_json::from_value::<ExternalResources>(serde_json::json!({"rawMail":"never"}))
            .is_err()
    );
}

#[test]
fn nci_ids_normalize_after_grant_verification_and_replay_equivalent_spellings() {
    let root = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let prepared = prepare(&service);
    let mut bare = request(&prepared);
    bare.remote_id = "123".into();
    let caller = grant(
        "cutex.alice",
        CallerOperation::RegisterRemote,
        &remote_request_sha256(&bare).unwrap(),
    );
    let receipt = service.register_remote(API, &caller, bare).unwrap();
    assert_eq!(receipt.job.external.unwrap().remote_id, "123.gadi-pbs");
    assert!(register(&service, &prepared).deduplicated);
    let mut event = source_event(&prepared, "bare-mail", JobState::Running);
    event.remote_id = "123".into();
    service.ingest_external_event(API, event.clone()).unwrap();
    event.remote_id = "123.gadi-pbs".into();
    assert_eq!(
        service.ingest_external_event(API, event).unwrap().status,
        "duplicate"
    );
    assert!(normalize_nci_remote_id(&"1".repeat(21)).is_err());
    assert!(normalize_nci_remote_id(&format!("{}.gadi-pbs", "1".repeat(21))).is_err());
    for invalid in ["123.other", "123[]", "123.gadi-pbs.extra", "", "-1"] {
        assert!(normalize_nci_remote_id(invalid).is_err());
    }
}

#[test]
fn legacy_bare_registration_replays_and_matches_canonical_source_event() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    drop(service);
    let path = root.path().join("state.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut legacy = request(&prepared);
    legacy.remote_id = "123".into();
    stored["jobs"][&job.job_id]["external"]["remoteId"] = "123".into();
    stored["jobs"][&job.job_id]["requestSha256"] = remote_request_sha256(&legacy).unwrap().into();
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
    let service = JobService::open(cfg).unwrap();
    assert!(register(&service, &prepared).deduplicated);
    assert_eq!(
        service
            .ingest_external_event(
                API,
                source_event(&prepared, "canonical-mail", JobState::Exited)
            )
            .unwrap()
            .status,
        "applied"
    );
}

#[test]
fn stale_flag_preserves_registered_and_migrates_legacy_stale_records() {
    for had_source_event in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        cfg.external_stale_after_secs = 1;
        let service = JobService::open(cfg.clone()).unwrap();
        let prepared = prepare(&service);
        let job = register(&service, &prepared).job;
        drop(service);
        let path = root.path().join("state.json");
        let mut stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stored["jobs"][&job.job_id]["state"] = "stale".into();
        stored["jobs"][&job.job_id]["external"]["registeredAt"] = 1.into();
        if had_source_event {
            stored["jobs"][&job.job_id]["external"]["lastSourceEventAt"] = 1.into();
        }
        std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
        let service = JobService::open(cfg).unwrap();
        let stale = service.query(API, &job.job_id).unwrap();
        assert_eq!(
            stale.state,
            if had_source_event {
                JobState::Running
            } else {
                JobState::Registered
            }
        );
        assert!(stale.external.unwrap().stale);
        service
            .ingest_external_event(API, source_event(&prepared, "fresh", JobState::Running))
            .unwrap();
        assert!(
            !service
                .query(API, &job.job_id)
                .unwrap()
                .external
                .unwrap()
                .stale
        );
    }
}

#[test]
fn source_event_capacity_is_a_permanent_nonretryable_result() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    drop(service);
    let path = root.path().join("state.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored["jobs"][&job.job_id]["external"]["sourceEvents"] = serde_json::Value::Array((0..256).map(|i|
        serde_json::json!({"eventIdSha256":format!("{i:064x}"),"eventSha256":"0".repeat(64)})).collect());
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
    let service = JobService::open(cfg).unwrap();
    let lookup = service
        .lookup_external_event(
            API,
            ExternalEventLookup {
                source: "nci".into(),
                remote_id: "123".into(),
                correlation_token: prepared.correlation_token.clone(),
            },
        )
        .unwrap();
    assert_eq!(lookup.status, "permanent_conflict");
    assert!(lookup.target.is_none());
    let receipt = service
        .ingest_external_event(API, source_event(&prepared, "overflow", JobState::Running))
        .unwrap();
    assert_eq!(receipt.status, "permanent_conflict");
    assert!(!receipt.retryable);
    assert_eq!(service.query(API, &job.job_id).unwrap().revision, 1);
}

#[test]
fn external_lookup_is_authenticated_read_only_and_uses_registered_metadata() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let prepared = prepare(&service);
    let mut lookup = ExternalEventLookup {
        source: "nci".into(),
        remote_id: "123".into(),
        correlation_token: prepared.correlation_token.clone(),
    };
    assert!(matches!(
        service.lookup_external_event(b"wrong", lookup.clone()),
        Err(JobError::Unauthorized(_))
    ));
    assert_eq!(
        service
            .lookup_external_event(API, lookup.clone())
            .unwrap()
            .status,
        "unmatched"
    );
    let mut req = request(&prepared);
    req.project = Some("zz00".into());
    let job = service
        .register_remote(
            API,
            &grant(
                "cutex.alice",
                CallerOperation::RegisterRemote,
                &remote_request_sha256(&req).unwrap(),
            ),
            req,
        )
        .unwrap()
        .job;
    let before = std::fs::read(root.path().join("state.json")).unwrap();
    let receipt = service.lookup_external_event(API, lookup.clone()).unwrap();
    assert_eq!(receipt.status, "matched");
    let target = receipt.target.unwrap();
    assert_eq!(target.remote_id, "123.gadi-pbs");
    assert_eq!(target.correlation_token, prepared.correlation_token);
    assert_eq!(
        target.remote_name.as_deref(),
        Some(prepared.suggested_job_name.as_str())
    );
    assert_eq!(target.project.as_deref(), Some("zz00"));
    lookup.remote_id = "123.gadi-pbs".into();
    assert_eq!(
        service
            .lookup_external_event(API, lookup.clone())
            .unwrap()
            .status,
        "matched"
    );
    assert_eq!(
        std::fs::read(root.path().join("state.json")).unwrap(),
        before
    );
    assert!(service.pending_outbox(API).unwrap().is_empty());
    let mut invalid = lookup.clone();
    invalid.correlation_token = "bad".into();
    assert!(service.lookup_external_event(API, invalid).is_err());
    let mut invalid = lookup.clone();
    invalid.remote_id = "123;whoami".into();
    assert!(service.lookup_external_event(API, invalid).is_err());
    assert!(
        serde_json::from_value::<ExternalEventLookup>(serde_json::json!({
            "source":"nci", "remoteId":"123", "correlationToken":prepared.correlation_token,
            "host":"untrusted"
        }))
        .is_err()
    );
    service
        .human_mark_external(API, &job.job_id, 1, JobState::Exited, Some(0), None)
        .unwrap();
    let receipt = service.lookup_external_event(API, lookup.clone()).unwrap();
    assert_eq!(receipt.status, "terminal_ignored");
    assert!(receipt.target.is_none());
    drop(service);
    let mut cfg = cfg;
    cfg.external_sources.clear();
    let service = JobService::open(cfg).unwrap();
    let receipt = service.lookup_external_event(API, lookup).unwrap();
    assert_eq!(receipt.status, "disabled_source");
    assert!(receipt.target.is_none());
}

#[test]
fn queued_source_events_do_not_regress_running_or_accept_terminal_metadata() {
    let root = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    for (id, code, reason) in [
        ("exit", Some(0), None),
        ("reason", None, Some(ExternalReason::Cancelled)),
    ] {
        let mut event = source_event(&prepared, id, JobState::Queued);
        event.exit_code = code;
        event.reason = reason;
        assert!(service.ingest_external_event(API, event).is_err());
    }
    assert_eq!(service.query(API, &job.job_id).unwrap().revision, 1);
    service
        .ingest_external_event(API, source_event(&prepared, "queued", JobState::Queued))
        .unwrap();
    assert_eq!(
        service.query(API, &job.job_id).unwrap().state,
        JobState::Queued
    );
    service
        .ingest_external_event(API, source_event(&prepared, "running", JobState::Running))
        .unwrap();
    service
        .ingest_external_event(
            API,
            source_event(&prepared, "late-queued", JobState::Queued),
        )
        .unwrap();
    let current = service.query(API, &job.job_id).unwrap();
    assert_eq!(current.state, JobState::Running);
    assert_eq!(current.revision, 3);
    assert!(service.pending_outbox(API).unwrap().is_empty());
}

#[test]
fn verification_list_is_root_only_paginated_and_excludes_terminal_and_local_jobs() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let service = JobService::open(cfg.clone()).unwrap();
    let page = |cursor| ExternalVerificationList { limit: 1, cursor };
    assert!(matches!(
        service.list_external_verification_targets(b"wrong", page(None)),
        Err(JobError::Unauthorized(_))
    ));
    for limit in [0, 65] {
        assert!(
            service
                .list_external_verification_targets(
                    API,
                    ExternalVerificationList {
                        limit,
                        cursor: None
                    }
                )
                .is_err()
        );
    }
    assert!(
        serde_json::from_value::<ExternalVerificationList>(
            serde_json::json!({"limit":1,"source":"nci"})
        )
        .is_err()
    );
    let mut ids = Vec::new();
    for remote_id in ["123", "124", "125"] {
        let prepared = prepare(&service);
        let mut req = request(&prepared);
        req.remote_id = remote_id.into();
        let job = service
            .register_remote(
                API,
                &grant(
                    "cutex.alice",
                    CallerOperation::RegisterRemote,
                    &remote_request_sha256(&req).unwrap(),
                ),
                req,
            )
            .unwrap()
            .job;
        ids.push(job.job_id);
    }
    service
        .human_mark_external(API, &ids[2], 1, JobState::Exited, Some(0), None)
        .unwrap();
    drop(service);
    // A stored local record must never become an SSH verification target.
    let path = root.path().join("state.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut local = stored["jobs"][&ids[0]].clone();
    local["jobId"] = "local-job".into();
    local.as_object_mut().unwrap().remove("external");
    stored["jobs"]["local-job"] = local;
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
    let service = JobService::open(cfg.clone()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let first = service
        .list_external_verification_targets(API, page(None))
        .unwrap();
    assert_eq!(first.targets.len(), 1);
    let second = service
        .list_external_verification_targets(API, page(first.next_cursor))
        .unwrap();
    assert_eq!(second.targets.len(), 1);
    assert!(second.next_cursor.is_none());
    let mut remote_ids = vec![
        first.targets[0].remote_id.clone(),
        second.targets[0].remote_id.clone(),
    ];
    remote_ids.sort();
    assert_eq!(remote_ids, ["123.gadi-pbs", "124.gadi-pbs"]);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(service);
    let mut cfg = cfg;
    cfg.external_sources.clear();
    let service = JobService::open(cfg).unwrap();
    assert!(
        service
            .list_external_verification_targets(API, page(None))
            .unwrap()
            .targets
            .is_empty()
    );
}

#[test]
fn repeated_running_observations_refresh_without_exhausting_terminal_receipts() {
    let root = tempfile::tempdir().unwrap();
    let service = JobService::open(config(root.path())).unwrap();
    let prepared = prepare(&service);
    let job = register(&service, &prepared).job;
    for i in 0..300 {
        service
            .ingest_external_event(
                API,
                source_event(&prepared, &format!("poll-{i}"), JobState::Running),
            )
            .unwrap();
    }
    let running = service.query(API, &job.job_id).unwrap();
    assert_eq!(running.external.as_ref().unwrap().source_events.len(), 1);
    assert!(
        running
            .external
            .as_ref()
            .unwrap()
            .last_source_event_at
            .is_some()
    );
    let receipt = service
        .ingest_external_event(API, source_event(&prepared, "finish", JobState::Exited))
        .unwrap();
    assert_eq!(receipt.status, "applied");
    assert_eq!(
        service.query(API, &job.job_id).unwrap().state,
        JobState::Exited
    );
    assert_eq!(service.pending_outbox(API).unwrap().len(), 1);
}

#[test]
fn scheduler_cancel_and_walltime_outcomes_settle_typed() {
    // Shapes emitted by the optional NCI qstat verifier.
    for (state, reason, expected) in [
        // Finished without Exit_status: deleted while queued.
        (JobState::Cancelled, ExternalReason::Cancelled, "cancelled"),
        // Exit_status -29 (PBS JOB_EXEC_KILL_WALLTIME).
        (
            JobState::Failed,
            ExternalReason::WalltimeExceeded,
            "walltime_exceeded",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let service = JobService::open(config(root.path())).unwrap();
        let prepared = prepare(&service);
        let job = register(&service, &prepared).job;
        let mut event = source_event(&prepared, expected, state);
        event.reason = Some(reason);
        assert_eq!(
            service.ingest_external_event(API, event).unwrap().status,
            "applied"
        );
        let settled = service.query(API, &job.job_id).unwrap();
        assert_eq!(settled.state, state);
        assert_eq!(settled.exit_code, None);
        assert_eq!(settled.terminal_reason.as_deref(), Some(expected));
        assert_eq!(service.pending_outbox(API).unwrap().len(), 1);
    }
}
#[test]
fn prepare_remote_hands_out_owner_notification_address() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let notify = root.path().join("notify-address-v1.json");
    let mut cfg = config(&root.path().join("state"));
    cfg.external_notify_file = Some(notify.clone());
    let service = JobService::open(cfg).unwrap();
    assert_eq!(
        service.capabilities(API).unwrap()["externalNotifyFile"],
        serde_json::json!(true)
    );
    // No file yet: arguments without -M plus an owner hint.
    let first = prepare(&service);
    assert_eq!(first.notification_status, "mailNotConfigured");
    assert_eq!(first.pbs_args, first.submission_arguments);
    assert!(first.notification_address.is_none());
    assert!(first.owner_action.as_deref().unwrap().contains("owner"));
    assert!(!first.pbs_args.iter().any(|a| a == "-M"));
    // Owner configured the mailbox: the address is read on every call, no restart.
    std::fs::write(
        &notify,
        r#"{"schema":"cutex/external-notify/v1","sources":{"nci":{"mailTo":"owner.nci@example.org"}}}"#,
    )
    .unwrap();
    std::fs::set_permissions(&notify, std::fs::Permissions::from_mode(0o600)).unwrap();
    let second = prepare(&service);
    assert_eq!(second.notification_status, "configured");
    assert_eq!(
        second.pbs_args,
        vec![
            "-N".to_string(),
            second.suggested_job_name.clone(),
            "-m".into(),
            "abe".into(),
            "-M".into(),
            "owner.nci@example.org".into()
        ]
    );
    assert_eq!(second.submission_arguments, second.pbs_args[..4].to_vec());
    assert!(second.owner_action.is_none());
    let wire = serde_json::to_value(&second).unwrap();
    assert_eq!(wire["notificationAddress"], "owner.nci@example.org");
    assert!(wire.get("ownerAction").is_none());
    // A malformed or unsafe address is never handed out.
    std::fs::write(
        &notify,
        r#"{"schema":"cutex/external-notify/v1","sources":{"nci":{"mailTo":"a@b.org -W x"}}}"#,
    )
    .unwrap();
    let third = prepare(&service);
    assert_eq!(third.notification_status, "mailConfigUnreadable");
    assert!(!third.pbs_args.iter().any(|a| a == "-M"));
    // Registration still works with prepared tokens regardless of mail status.
    register(&service, &second);
}
