use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::TimeZone;
use serde_json::json;

use super::*;

fn noop(_: &Rotation) {}

struct Fake {
    now: Mutex<DateTime<Utc>>,
    project: Mutex<Option<ProjectFacts>>,
    agents: Mutex<HashMap<String, AgentFacts>>,
    ready: Mutex<bool>,
    turn_active: Mutex<bool>,
    /// (session, clientUserMessageId, text)
    messages: Mutex<Vec<(String, String, String)>>,
    created: Mutex<Vec<String>>,
    /// (action id, expected epoch, successor)
    rebinds: Mutex<Vec<(String, u64, String)>>,
    disposals: Mutex<Vec<String>>,
    found: Mutex<Option<String>>,
    fail_send: Mutex<Option<EffectError>>,
    fail_rebind: Mutex<Option<EffectError>>,
    fail_dispose: Mutex<Option<EffectError>>,
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 10, 3, 0, 0).unwrap() + ChronoDuration::seconds(seconds)
}

impl Fake {
    fn new() -> Self {
        let mut agents = HashMap::new();
        agents.insert(
            "cutex.old".to_string(),
            AgentFacts {
                cutex_session_id: "cutex.old".into(),
                name: "eva-r2".into(),
                profile: Some("GLM".into()),
                model: Some("glm-5.3".into()),
                reasoning_effort: Some("high".into()),
                cwd: abs("work/eva"),
                sandbox: Some("workspace-write".into()),
                approval_policy: Some("never".into()),
                retired: false,
                project_id: Some("gamma".into()),
            },
        );
        Self {
            now: Mutex::new(at(0)),
            project: Mutex::new(Some(ProjectFacts {
                project_id: "gamma".into(),
                display_name: "Gamma".into(),
                active: true,
                director: Some("cutex.old".into()),
                authority_epoch: 3,
                workspace_root: Some(abs("work")),
            })),
            agents: Mutex::new(agents),
            ready: Mutex::new(false),
            turn_active: Mutex::new(true),
            messages: Mutex::new(Vec::new()),
            created: Mutex::new(Vec::new()),
            rebinds: Mutex::new(Vec::new()),
            disposals: Mutex::new(Vec::new()),
            found: Mutex::new(None),
            fail_send: Mutex::new(None),
            fail_rebind: Mutex::new(None),
            fail_dispose: Mutex::new(None),
        }
    }

    fn advance(&self, seconds: i64) {
        let mut now = self.now.lock().unwrap();
        *now += ChronoDuration::seconds(seconds);
    }

    fn handoff_done(&self) {
        *self.ready.lock().unwrap() = true;
        *self.turn_active.lock().unwrap() = false;
    }
}

fn abs(relative: &str) -> String {
    if cfg!(windows) {
        format!("C:\\{}", relative.replace('/', "\\"))
    } else {
        format!("/{relative}")
    }
}

impl RotationEffects for Fake {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }
    fn project(&self, project_id: &str) -> Result<Option<ProjectFacts>, EffectError> {
        Ok(self
            .project
            .lock()
            .unwrap()
            .clone()
            .filter(|project| project.project_id == project_id))
    }
    fn agent(&self, cutex_session_id: &str) -> Result<Option<AgentFacts>, EffectError> {
        Ok(self.agents.lock().unwrap().get(cutex_session_id).cloned())
    }
    fn delegations(&self, _: &str) -> DelegationCounts {
        DelegationCounts { requested: 2, handling: 1 }
    }
    fn send_message(&self, session: &str, client_id: &str, text: &str) -> Result<(), EffectError> {
        if let Some(error) = self.fail_send.lock().unwrap().clone() {
            return Err(error);
        }
        self.messages
            .lock()
            .unwrap()
            .push((session.into(), client_id.into(), text.into()));
        Ok(())
    }
    fn turn_active(&self, _: &str) -> Result<bool, EffectError> {
        Ok(*self.turn_active.lock().unwrap())
    }
    fn handoff_ready(&self, _: &Path) -> bool {
        *self.ready.lock().unwrap()
    }
    fn find_agent_by_name(&self, _: &str, _: &str) -> Result<Option<String>, EffectError> {
        Ok(self.found.lock().unwrap().clone())
    }
    fn create_successor(&self, rotation: &Rotation) -> Result<String, EffectError> {
        assert!(rotation.successor_create_started_at.is_some(), "reserved before the effect");
        self.created.lock().unwrap().push(rotation.successor_spec.name.clone());
        Ok("cutex.new".into())
    }
    fn rebind(&self, project: &str, action: &str, epoch: u64, successor: &str) -> Result<u64, EffectError> {
        assert_eq!(project, "gamma");
        self.rebinds
            .lock()
            .unwrap()
            .push((action.into(), epoch, successor.into()));
        match self.fail_rebind.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(epoch + 1),
        }
    }
    fn archive(&self, session: &str, request_id: &str, _: &str) -> Result<(), EffectError> {
        self.disposals.lock().unwrap().push(format!("archive {session} {request_id}"));
        self.fail_dispose.lock().unwrap().clone().map_or(Ok(()), Err)
    }
    fn ensure_member(&self, _: &str, session: &str, action: &str) -> Result<(), EffectError> {
        self.disposals.lock().unwrap().push(format!("member {session} {action}"));
        self.fail_dispose.lock().unwrap().clone().map_or(Ok(()), Err)
    }
    fn grant_operator(&self, _: &str, session: &str, action: &str) -> Result<(), EffectError> {
        self.disposals.lock().unwrap().push(format!("operator {session} {action}"));
        Ok(())
    }
}

struct Harness {
    dir: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    fn new() -> Self {
        Self {
            dir: std::env::temp_dir().join(format!("cutex-owner-rotation-{}", uuid::Uuid::new_v4().simple())),
        }
    }

    /// A fresh driver (a "daemon process") over the same state file.
    fn driver(&self, fake: Fake) -> Driver<Fake> {
        Driver::new(RotationStore::at(self.dir.join("rotations.json"), noop), fake)
    }
}

fn request(id: &str) -> StartRequest {
    StartRequest {
        request_id: id.into(),
        project_id: "gamma".into(),
        expected_director_cutex_session_id: "cutex.old".into(),
        expected_authority_epoch: 3,
        predecessor: PredecessorDisposition::Member,
        handoff_timeout_secs: Some(120),
        successor: SuccessorInput::default(),
    }
}

fn states(rotation: &Rotation) -> Vec<RotationState> {
    rotation.history.iter().map(|entry| entry.state).collect()
}

fn get(driver: &Driver<Fake>, id: &str) -> Rotation {
    driver.store.get(id).unwrap().unwrap()
}

#[test]
fn rotation_runs_every_state_in_order() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    let (rotation, created) = driver.start(&request("req-1")).unwrap();
    assert!(created);
    assert_eq!(rotation.state, RotationState::Requested);
    assert_eq!(rotation.successor_spec.name, "eva-r3");
    assert_eq!(rotation.successor_spec.profile.as_deref(), Some("GLM"));
    assert_eq!(rotation.successor_spec.model.as_deref(), Some("glm-5.3"));
    assert_eq!(rotation.successor_spec.cwd, abs("work/eva"));
    assert_eq!(rotation.successor_spec.sandbox.as_deref(), Some("workspace-write"));
    assert_eq!(rotation.predecessor_delegations, DelegationCounts { requested: 2, handling: 1 });
    assert_eq!(
        rotation.handoff.path,
        Path::new(&abs("work")).join("handoff").join("20261010T030000Z.md").to_string_lossy()
    );

    driver.tick();
    let current = get(&driver, &rotation.rotation_id);
    assert_eq!(current.state, RotationState::HandoffRequested);
    assert_eq!(current.handoff.deadline.as_deref(), Some("2026-10-10T03:02:00.000Z"));
    {
        let messages = driver.effects.messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        let (session, client, text) = &messages[0];
        assert_eq!(session, "cutex.old");
        assert_eq!(client, &format!("owner-rotation-{}-handoff", rotation.rotation_id));
        assert!(text.contains(&rotation.handoff.path), "{text}");
        assert!(text.contains("end your turn"), "{text}");
        assert!(text.contains("stay in the Project as a member"), "{text}");
    }
    // The file alone (turn still running) is not enough.
    *driver.effects.ready.lock().unwrap() = true;
    driver.tick();
    driver.tick();
    assert_eq!(get(&driver, &rotation.rotation_id).state, RotationState::HandoffRequested);
    // File + turn end in two consecutive polls.
    driver.effects.handoff_done();
    driver.effects.advance(2);
    driver.tick();
    assert_eq!(get(&driver, &rotation.rotation_id).state, RotationState::HandoffRequested);
    driver.effects.advance(2);
    driver.tick();
    let done = get(&driver, &rotation.rotation_id);
    assert_eq!(done.state, RotationState::Done, "{done:#?}");
    assert_eq!(
        states(&done),
        vec![
            RotationState::Requested,
            RotationState::HandoffRequested,
            RotationState::HandoffWritten,
            RotationState::SuccessorCreated,
            RotationState::AuthorityRebound,
            RotationState::PredecessorDisposed,
            RotationState::Done,
        ]
    );
    assert_eq!(done.successor_cutex_session_id.as_deref(), Some("cutex.new"));
    assert_eq!(done.authority_epoch, Some(4));
    assert!(done.finished_at.is_some() && done.warnings.is_empty() && done.error.is_none());
    assert_eq!(*driver.effects.created.lock().unwrap(), vec!["eva-r3".to_string()]);
    assert_eq!(
        *driver.effects.rebinds.lock().unwrap(),
        vec![(format!("owner-rotation:{}:rebind", rotation.rotation_id), 3, "cutex.new".to_string())]
    );
    assert_eq!(
        *driver.effects.disposals.lock().unwrap(),
        vec![format!("member cutex.old owner-rotation:{}:member", rotation.rotation_id)]
    );
    let messages = driver.effects.messages.lock().unwrap();
    let (session, client, text) = messages.last().unwrap();
    assert_eq!(session, "cutex.new");
    assert_eq!(client, &format!("owner-rotation-{}-successor", rotation.rotation_id));
    assert!(text.contains("Read the handoff note first"), "{text}");
    assert!(text.contains(&done.handoff.path), "{text}");
    drop(messages);
    // Nothing more happens to a finished rotation.
    driver.tick();
    assert_eq!(get(&driver, &rotation.rotation_id), done);
}

#[test]
fn timeout_waits_for_the_owner_and_continue_skips_the_handoff() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    let mut start = request("req-timeout");
    start.predecessor = PredecessorDisposition::Archive;
    let (rotation, _) = driver.start(&start).unwrap();
    driver.tick();
    driver.effects.advance(121);
    driver.tick();
    let timed_out = get(&driver, &rotation.rotation_id);
    assert_eq!(timed_out.state, RotationState::Timeout);
    // Timeout waits: no successor without an owner decision.
    driver.effects.advance(600);
    driver.tick();
    assert_eq!(get(&driver, &rotation.rotation_id).state, RotationState::Timeout);
    assert!(driver.effects.created.lock().unwrap().is_empty());
    let continued = driver.continue_without_handoff(&rotation.rotation_id).unwrap();
    assert!(continued.handoff.skipped);
    // Idempotent.
    assert_eq!(driver.continue_without_handoff(&rotation.rotation_id).unwrap(), continued);
    driver.tick();
    let done = get(&driver, &rotation.rotation_id);
    assert_eq!(done.state, RotationState::Done);
    assert!(done.handoff.written_at.is_none());
    assert_eq!(
        *driver.effects.disposals.lock().unwrap(),
        vec![format!("archive cutex.old owner-rotation-{}-archive", rotation.rotation_id)]
    );
    let messages = driver.effects.messages.lock().unwrap();
    let text = &messages.last().unwrap().2;
    assert!(text.contains("without a handoff note"), "{text}");
    assert!(text.contains("was archived"), "{text}");
    drop(messages);
    // A repeated continue is idempotent; cancel no longer applies.
    assert_eq!(driver.continue_without_handoff(&rotation.rotation_id).unwrap(), done);
    assert_eq!(driver.cancel(&rotation.rotation_id).unwrap_err().code, "rotation_not_cancellable");
}

#[test]
fn a_late_handoff_after_the_timeout_still_proceeds() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    let (rotation, _) = driver.start(&request("req-late")).unwrap();
    driver.tick();
    driver.effects.advance(200);
    driver.tick();
    assert_eq!(get(&driver, &rotation.rotation_id).state, RotationState::Timeout);
    driver.effects.handoff_done();
    driver.tick();
    driver.tick();
    let done = get(&driver, &rotation.rotation_id);
    assert_eq!(done.state, RotationState::Done);
    assert!(states(&done).contains(&RotationState::Timeout));
    assert!(states(&done).contains(&RotationState::HandoffWritten));
}

#[test]
fn cancel_applies_only_before_the_successor_is_created() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    let (first, _) = driver.start(&request("req-cancel")).unwrap();
    // A second rotation of the same Project is refused while one runs.
    let mut second = request("req-second");
    second.handoff_timeout_secs = None;
    assert_eq!(driver.start(&second).unwrap_err().code, "rotation_in_progress");
    driver.tick();
    let cancelled = driver.cancel(&first.rotation_id).unwrap();
    assert_eq!(cancelled.state, RotationState::Cancelled);
    assert!(cancelled.finished_at.is_some());
    assert_eq!(driver.cancel(&first.rotation_id).unwrap(), cancelled);
    assert_eq!(
        driver.continue_without_handoff(&first.rotation_id).unwrap_err().code,
        "rotation_not_waiting"
    );
    driver.effects.handoff_done();
    driver.tick();
    driver.tick();
    assert_eq!(get(&driver, &first.rotation_id), cancelled);
    assert!(driver.effects.created.lock().unwrap().is_empty());
    // The Project is free again.
    assert!(driver.start(&second).unwrap().1);
    assert_eq!(driver.cancel("rot-missing").unwrap_err().code, "rotation_not_found");
}

#[test]
fn start_checks_fences_defaults_and_idempotency() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    let mut stale = request("req-stale");
    stale.expected_authority_epoch = 2;
    assert_eq!(driver.start(&stale).unwrap_err().code, "stale_project_authority");
    let mut other = request("req-other-director");
    other.expected_director_cutex_session_id = "cutex.someone".into();
    assert_eq!(driver.start(&other).unwrap_err().code, "stale_project_authority");
    let mut missing = request("req-missing");
    missing.project_id = "delta".into();
    assert_eq!(driver.start(&missing).unwrap_err().code, "project_not_found");
    let mut invalid = request("req-invalid");
    invalid.handoff_timeout_secs = Some(5);
    assert_eq!(driver.start(&invalid).unwrap_err().code, "invalid_request");
    invalid.handoff_timeout_secs = None;
    invalid.successor.cwd = Some("relative/dir".into());
    assert_eq!(driver.start(&invalid).unwrap_err().code, "invalid_request");
    invalid.successor.cwd = None;
    invalid.successor.name = Some("eva-r2".into());
    assert_eq!(driver.start(&invalid).unwrap_err().code, "invalid_request");

    // Another profile: model / effort are not inherited.
    let mut custom = request("req-custom");
    custom.successor = SuccessorInput {
        name: Some("atlas".into()),
        profile: Some("DeepSeek".into()),
        model: None,
        reasoning_effort: None,
        cwd: Some(abs("elsewhere")),
    };
    let (rotation, created) = driver.start(&custom).unwrap();
    assert!(created);
    assert_eq!(rotation.successor_spec.name, "atlas");
    assert_eq!(rotation.successor_spec.model, None);
    assert_eq!(rotation.successor_spec.reasoning_effort, None);
    assert_eq!(rotation.successor_spec.cwd, abs("elsewhere"));
    // Exact replay returns the same record; another body is a conflict.
    let (replay, created) = driver.start(&custom).unwrap();
    assert!(!created);
    assert_eq!(replay, rotation);
    custom.successor.name = Some("atlas-2".into());
    assert_eq!(driver.start(&custom).unwrap_err().code, "idempotency_conflict");

    let without_director = ProjectFacts {
        director: None,
        ..driver.effects.project.lock().unwrap().clone().unwrap()
    };
    *driver.effects.project.lock().unwrap() = Some(without_director);
    assert_eq!(driver.start(&request("req-none")).unwrap_err().code, "project_has_no_director");
}

#[test]
fn restart_resumes_every_persisted_state() {
    let harness = Harness::new();
    // Process 1 records the rotation and dies before sending anything.
    let (rotation, _) = harness.driver(Fake::new()).start(&request("req-restart")).unwrap();
    // Process 2 sends the request (same clientUserMessageId on every retry).
    let second = harness.driver(Fake::new());
    second.tick();
    assert_eq!(get(&second, &rotation.rotation_id).state, RotationState::HandoffRequested);
    let client_id = second.effects.messages.lock().unwrap()[0].1.clone();
    assert_eq!(client_id, rotation.handoff.client_user_message_id);
    // Process 3 died after reserving the successor creation.
    let third = harness.driver(Fake::new());
    third
        .store
        .modify(&rotation.rotation_id, "2026-10-10T03:00:05.000Z", |current| {
            current.handoff.written_at = Some("2026-10-10T03:00:04.000Z".into());
            current.transition(RotationState::HandoffWritten, "2026-10-10T03:00:04.000Z", None);
            current.successor_create_started_at = Some("2026-10-10T03:00:05.000Z".into());
        })
        .unwrap();
    // Process 4 finds the agent it created and goes on (no second creation).
    let fourth = harness.driver(Fake::new());
    *fourth.effects.found.lock().unwrap() = Some("cutex.new".into());
    fourth.tick();
    let done = get(&fourth, &rotation.rotation_id);
    assert_eq!(done.state, RotationState::Done);
    assert!(fourth.effects.created.lock().unwrap().is_empty());
    assert_eq!(fourth.effects.rebinds.lock().unwrap().len(), 1);
    assert!(done
        .history
        .iter()
        .any(|entry| entry.note.as_deref() == Some("successor found after an interrupted creation")));
}

#[test]
fn an_unproven_successor_creation_fails_cleanly() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    let (rotation, _) = driver.start(&request("req-lost")).unwrap();
    driver.tick();
    driver
        .store
        .modify(&rotation.rotation_id, "2026-10-10T03:00:05.000Z", |current| {
            current.handoff.skipped = true;
            current.successor_create_started_at = Some("2026-10-10T03:00:05.000Z".into());
        })
        .unwrap();
    // Cancel is refused once the creation began.
    assert_eq!(driver.cancel(&rotation.rotation_id).unwrap_err().code, "rotation_not_cancellable");
    let restarted = harness.driver(Fake::new());
    restarted.tick();
    let failed = get(&restarted, &rotation.rotation_id);
    assert_eq!(failed.state, RotationState::Failed);
    assert_eq!(failed.error.as_ref().unwrap().code, "successor_creation_interrupted");
    assert!(restarted.effects.rebinds.lock().unwrap().is_empty());
}

#[test]
fn restart_after_the_successor_was_created_replays_the_same_rebind() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    *driver.effects.fail_rebind.lock().unwrap() =
        Some(EffectError::new("persistence_unavailable", "busy", true));
    let (rotation, _) = driver.start(&request("req-rebind")).unwrap();
    driver.tick();
    driver.continue_without_handoff(&rotation.rotation_id).unwrap();
    driver.tick();
    let waiting = get(&driver, &rotation.rotation_id);
    assert_eq!(waiting.state, RotationState::SuccessorCreated);
    assert_eq!(waiting.attempts, 1);
    // A new process retries with the same action id and finishes.
    let restarted = harness.driver(Fake::new());
    restarted.tick();
    assert_eq!(get(&restarted, &rotation.rotation_id).state, RotationState::Done);
    let first = driver.effects.rebinds.lock().unwrap()[0].0.clone();
    assert_eq!(restarted.effects.rebinds.lock().unwrap()[0].0, first);
}

#[test]
fn retryable_failures_give_up_after_the_limit_and_conflicts_fail_at_once() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    *driver.effects.fail_send.lock().unwrap() =
        Some(EffectError::new("runtime_unavailable", "starting", true));
    let (rotation, _) = driver.start(&request("req-retry")).unwrap();
    for _ in 0..MAX_STEP_ATTEMPTS {
        driver.tick();
    }
    let failed = get(&driver, &rotation.rotation_id);
    assert_eq!(failed.state, RotationState::Failed);
    assert_eq!(failed.error.as_ref().unwrap().code, "handoff_request_failed");

    let driver = harness.driver(Fake::new());
    *driver.effects.fail_rebind.lock().unwrap() =
        Some(EffectError::new("stale_project_authority", "conflict: stale_project_authority", false));
    let (rotation, _) = driver.start(&request("req-stale-rebind")).unwrap();
    driver.tick();
    driver.continue_without_handoff(&rotation.rotation_id).unwrap();
    driver.tick();
    let failed = get(&driver, &rotation.rotation_id);
    assert_eq!(failed.state, RotationState::Failed);
    let error = failed.error.unwrap();
    assert_eq!(error.code, "rebind_failed");
    assert!(error.message.contains("stays a member"), "{}", error.message);
}

#[test]
fn a_failed_disposal_is_a_warning_and_the_successor_still_starts() {
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    *driver.effects.fail_dispose.lock().unwrap() =
        Some(EffectError::new("session_busy", "a launch is in progress", false));
    let mut start = request("req-dispose");
    start.predecessor = PredecessorDisposition::Operator;
    let (rotation, _) = driver.start(&start).unwrap();
    driver.tick();
    driver.continue_without_handoff(&rotation.rotation_id).unwrap();
    driver.tick();
    let done = get(&driver, &rotation.rotation_id);
    assert_eq!(done.state, RotationState::Done);
    assert_eq!(done.warnings.len(), 1);
    assert!(done.warnings[0].starts_with("predecessor_dispose_failed: session_busy"), "{:?}", done.warnings);
    assert_eq!(driver.effects.messages.lock().unwrap().last().unwrap().0, "cutex.new");
}

#[test]
fn helpers_and_capability() {
    assert_eq!(next_successor_name("eva"), "eva-r2");
    assert_eq!(next_successor_name("eva-r2"), "eva-r3");
    assert_eq!(next_successor_name("lab-director-r9"), "lab-director-r10");
    assert_eq!(next_successor_name("claude-rx"), "claude-rx-r2");
    assert_eq!(next_successor_name("-r1"), "-r1-r2");
    let capabilities = crate::management::v2::capabilities::capabilities_resource();
    assert_eq!(capabilities["features"]["ownerRotation"], true);
    assert!(capabilities["featureList"]
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "owner-rotation"));
    use crate::management::v2::owner_rotation_api::is_path;
    assert!(is_path("/v2/director-rotations"));
    assert!(is_path("/v2/director-rotations/rot-1/cancel"));
    assert!(!is_path("/v2/director-rotationsx"));
    assert!(!is_path("/v2/projects/gamma"));
}

#[test]
fn records_and_requests_match_the_exported_schema() {
    use crate::management::v2::schema_export::{assert_valid, validator};
    let harness = Harness::new();
    let driver = harness.driver(Fake::new());
    let (rotation, _) = driver.start(&request("req-schema")).unwrap();
    assert_valid("owner-rotation", "rotationBody", &rotation_body(&rotation));
    driver.tick();
    driver.continue_without_handoff(&rotation.rotation_id).unwrap();
    driver.tick();
    let done = get(&driver, &rotation.rotation_id);
    assert_valid("owner-rotation", "rotationBody", &rotation_body(&done));
    assert_valid(
        "owner-rotation",
        "rotationList",
        &json!({ "rotations": driver.store.list(None, 10).unwrap() }),
    );
    let mut full = request("req-full");
    full.successor = SuccessorInput {
        name: Some("atlas".into()),
        profile: Some("GLM".into()),
        model: Some("glm-5.3".into()),
        reasoning_effort: Some("high".into()),
        cwd: Some(abs("work")),
    };
    assert_valid("owner-rotation", "startRequest", &serde_json::to_value(&full).unwrap());
    let bogus = json!({"requestId": "r", "projectId": "p", "expectedDirectorCutexSessionId": "d", "expectedAuthorityEpoch": 1, "predecessor": "retire"});
    assert!(!validator("owner-rotation", "startRequest").unwrap().is_valid(&bogus));
}
