use crate::model::{
    COMPLETION_CONTRACT_V2, CompletionDeliveryState, CompletionDeliverySummary, CompletionFactsV1,
    CompletionWireVersion, FrozenCompletionRequestV2, JobError, JobRecord, JobState, OutboxRecord,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredState {
    pub version: u8,
    pub jobs: BTreeMap<String, JobRecord>,
    pub action_index: BTreeMap<String, String>,
    pub outbox: BTreeMap<String, OutboxRecord>,
    #[serde(default)]
    pub prepared_remote: BTreeMap<String, crate::model::PreparedRemote>,
}

pub(crate) struct Store {
    root: PathBuf,
    state_path: PathBuf,
    _process_lock: fs::File,
}

impl Store {
    pub fn open(
        root: PathBuf,
        completion_wire_version: CompletionWireVersion,
        completion_enabled: bool,
    ) -> Result<(Self, StoredState), JobError> {
        fs::create_dir_all(&root)?;
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(JobError::Invalid("state root must not be a symlink".into()));
        }
        if fs::metadata(&root)?.uid() != unsafe { libc::geteuid() } {
            return Err(JobError::Unauthorized(
                "state root must be owned by the service UID".into(),
            ));
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let process_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join("service.lock"))?;
        if process_lock.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err(JobError::Unauthorized(
                "state lock must be owned by the service UID".into(),
            ));
        }
        process_lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&process_lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            return Err(JobError::Conflict(
                "another Job Service owns this state root".into(),
            ));
        }
        let state_path = root.join("state.json");
        if state_path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(JobError::Invalid("state file must not be a symlink".into()));
        }
        if let Ok(metadata) = fs::metadata(&state_path) {
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(JobError::Unauthorized(
                    "state file must be owned by the service UID".into(),
                ));
            }
            fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600))?;
        }
        let store = Self {
            root,
            state_path,
            _process_lock: process_lock,
        };
        let mut state = if store.state_path.exists() {
            serde_json::from_slice(&fs::read(&store.state_path)?)?
        } else {
            StoredState {
                version: 1,
                ..StoredState::default()
            }
        };
        if !matches!(state.version, 1 | 2 | 3) {
            return Err(JobError::Invalid("unsupported state version".into()));
        }
        validate_loaded_state(&state)?;
        let now = crate::service::now_secs();
        let mut changed = false;
        for job in state.jobs.values_mut() {
            if job.completion_delivery.enabled != completion_enabled {
                job.completion_delivery.enabled = completion_enabled;
                changed = true;
            }
            if job.state == JobState::Stale {
                if let Some(external) = &mut job.external {
                    job.state = if external.last_source_event_at.is_some() {
                        JobState::Running
                    } else {
                        JobState::Registered
                    };
                    external.stale = true;
                    changed = true;
                }
            }
            let next = match job.state {
                _ if job.external.is_some() => None,
                JobState::LaunchPending => {
                    Some((JobState::LaunchUnknown, "service restarted during launch"))
                }
                JobState::Running => Some((
                    JobState::Interrupted,
                    "service restarted while process was running",
                )),
                _ => None,
            };
            if let Some((next, reason)) = next {
                job.state = next;
                job.revision += 1;
                job.updated_at_epoch_secs = now;
                job.terminal_reason = Some(reason.into());
                job.process_id = None;
                job.process_start_ticks = None;
                changed = true;
                insert_outbox(&mut state.outbox, job, completion_wire_version)?;
            }
        }
        for item in state.outbox.values_mut() {
            // Normalize legacy unattempted Disabled entries; retain their exact
            // frozen payload, event ID, acknowledgement and delivered receipts.
            if matches!(
                item.delivery_state,
                CompletionDeliveryState::Disabled | CompletionDeliveryState::Ready
            ) {
                let next = if completion_enabled {
                    CompletionDeliveryState::Ready
                } else {
                    CompletionDeliveryState::Disabled
                };
                changed |= item.delivery_state != next;
                item.delivery_state = next;
            }
            if item.delivery_state == CompletionDeliveryState::Sending {
                item.delivery_state = CompletionDeliveryState::RetryPending;
                item.next_attempt_at_epoch_millis = 0;
                item.last_error = Some("service restarted during completion delivery".into());
                changed = true;
            }
        }
        for item in state.outbox.values() {
            if let Some(job) = state.jobs.get_mut(&item.job_id) {
                project_outbox(job, item);
            }
        }
        if changed {
            store.save(&mut state)?;
        }
        Ok((store, state))
    }

    pub fn save(&self, state: &mut StoredState) -> Result<(), JobError> {
        if state.version == 1
            && (state.jobs.values().any(|job| job.execution.is_some())
                || state
                    .outbox
                    .values()
                    .any(|item| item.wire_version.is_some()))
        {
            state.version = 2;
        }
        if !state.prepared_remote.is_empty()
            || state.jobs.values().any(|job| job.external.is_some())
        {
            state.version = 3;
        }
        let temp = self.root.join(format!(".state.{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        serde_json::to_writer(&mut file, state)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, &self.state_path)?;
        OpenOptions::new().read(true).open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub fn output_path(&self, job_id: &str, stream: &str) -> PathBuf {
        self.root.join("output").join(format!("{job_id}.{stream}"))
    }

    pub fn ensure_output_dir(&self) -> Result<(), JobError> {
        let path = self.root.join("output");
        fs::create_dir_all(&path)?;
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(JobError::Invalid(
                "output directory must not be a symlink".into(),
            ));
        }
        if fs::metadata(&path)?.uid() != unsafe { libc::geteuid() } {
            return Err(JobError::Unauthorized(
                "output directory must be owned by the service UID".into(),
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
}

pub(crate) fn insert_outbox(
    outbox: &mut BTreeMap<String, OutboxRecord>,
    job: &JobRecord,
    wire_version: CompletionWireVersion,
) -> Result<(), JobError> {
    let event_id = format!("job-terminal:{}:{}", job.job_id, job.revision);
    let facts = completion_facts(job);
    let result = serde_json::json!({
        "jobId": job.job_id,
        "revision": job.revision,
        "state": job.state,
        "exitCode": job.exit_code,
        "reason": job.terminal_reason,
        "stdout": job.stdout,
        "stderr": job.stderr,
        "outputReference": job.output_reference,
    });
    let (digest, stored_wire, frozen_request) = match wire_version {
        CompletionWireVersion::V1 => (
            hex::encode(sha2::Sha256::digest(serde_json::to_vec(&result)?)),
            None,
            None,
        ),
        CompletionWireVersion::V2 => {
            let digest = result_digest_v2(job, &facts)?;
            let request = FrozenCompletionRequestV2 {
                schema: COMPLETION_CONTRACT_V2.into(),
                event_id: event_id.clone(),
                job_id: job.job_id.clone(),
                job_revision: job.revision,
                terminal_status: terminal_status(job.state).into(),
                result_sha256: digest.clone(),
                target_cutex_session_id: job.request.subscriber_cutex_session_id.clone(),
                facts,
                output_reference: job.output_reference.clone(),
            };
            (digest, Some(2), Some(request))
        }
    };
    let candidate = OutboxRecord {
        event_id: event_id.clone(),
        job_id: job.job_id.clone(),
        job_revision: job.revision,
        subscriber_cutex_session_id: job.request.subscriber_cutex_session_id.clone(),
        terminal_state: job.state,
        result_sha256: digest,
        output_reference: job.output_reference.clone(),
        wire_version: stored_wire,
        frozen_request_v2: frozen_request,
        acknowledged: false,
        delivery_state: if job.completion_delivery.enabled {
            CompletionDeliveryState::Ready
        } else {
            CompletionDeliveryState::Disabled
        },
        attempt_count: 0,
        next_attempt_at_epoch_millis: 0,
        last_attempt_at_epoch_millis: None,
        last_error: None,
        receipt: None,
    };
    if let Some(existing) = outbox.get(&event_id) {
        if existing.job_id != candidate.job_id
            || existing.job_revision != candidate.job_revision
            || existing.subscriber_cutex_session_id != candidate.subscriber_cutex_session_id
            || existing.terminal_state != candidate.terminal_state
            || existing.result_sha256 != candidate.result_sha256
            || existing.output_reference != candidate.output_reference
            || existing.wire_version != candidate.wire_version
            || existing.frozen_request_v2 != candidate.frozen_request_v2
        {
            return Err(JobError::Conflict(
                "terminal event ID was reused with changed completion facts".into(),
            ));
        }
        return Ok(());
    }
    outbox.insert(event_id, candidate);
    Ok(())
}

fn result_digest_v2(job: &JobRecord, facts: &CompletionFactsV1) -> Result<String, JobError> {
    let result_bytes = result_snapshot_v2_bytes(job, facts)?;
    let mut digest_input = b"cutex:job-result:v2\0".to_vec();
    digest_input.extend(result_bytes);
    Ok(hex::encode(sha2::Sha256::digest(digest_input)))
}

fn result_snapshot_v2_bytes(
    job: &JobRecord,
    facts: &CompletionFactsV1,
) -> Result<Vec<u8>, JobError> {
    let snapshot = CompletionResultSnapshotV2 {
        job_id: &job.job_id,
        revision: job.revision,
        state: job.state,
        exit_code: job.exit_code,
        reason: job.terminal_reason.as_deref(),
        stdout: &job.stdout,
        stderr: &job.stderr,
        output_reference: &job.output_reference,
        facts,
    };
    Ok(serde_json::to_vec(&snapshot)?)
}

fn completion_facts(job: &JobRecord) -> CompletionFactsV1 {
    CompletionFactsV1 {
        remote_id: job
            .external
            .as_ref()
            .map(|external| external.remote_id.clone()),
        external_resources: job
            .external
            .as_ref()
            .and_then(|external| external.resources.clone()),
        facts_version: 1,
        action_id: job.request.action_id.clone(),
        exit_code: job.exit_code.filter(|code| *code >= 0),
        terminal_reason: job
            .terminal_reason
            .as_ref()
            .filter(|reason| reason.len() <= 2_048)
            .cloned(),
        execution: job.execution.clone(),
        stdout: job.stdout.clone(),
        stderr: job.stderr.clone(),
    }
}

fn validate_loaded_state(state: &StoredState) -> Result<(), JobError> {
    if state.version < 3
        && (!state.prepared_remote.is_empty()
            || state.jobs.values().any(|job| job.external.is_some()))
    {
        return Err(JobError::Invalid(
            "external jobs require state version 3".into(),
        ));
    }
    if state.jobs.values().any(|job| {
        job.external.is_some()
            && (job.process_id.is_some()
                || job.process_start_ticks.is_some()
                || matches!(
                    job.state,
                    JobState::LaunchPending | JobState::LaunchUnknown | JobState::Interrupted
                ))
    }) {
        return Err(JobError::Invalid(
            "external job contains local process state".into(),
        ));
    }
    if state.version == 1
        && (state.jobs.values().any(|job| job.execution.is_some())
            || state
                .outbox
                .values()
                .any(|item| item.wire_version.is_some() || item.frozen_request_v2.is_some()))
    {
        return Err(JobError::Invalid(
            "state version 1 contains version 2 execution or completion fields".into(),
        ));
    }
    for item in state.outbox.values() {
        match (item.wire_version, item.frozen_request_v2.as_ref()) {
            (None, None) => {}
            (Some(2), Some(request)) => {
                let job = state.jobs.get(&item.job_id).ok_or_else(|| {
                    JobError::Invalid("v2 outbox references a missing job".into())
                })?;
                let mut expected_facts = completion_facts(job);
                // Older frozen v2 events predate remote identity facts. Never
                // rewrite their bodies or digests when opening the store.
                if request.facts.remote_id.is_none() {
                    expected_facts.remote_id = None;
                }
                if request.facts.external_resources.is_none() {
                    expected_facts.external_resources = None;
                }
                if request.schema != COMPLETION_CONTRACT_V2
                    || request.event_id != item.event_id
                    || request.job_id != item.job_id
                    || request.job_revision != item.job_revision
                    || request.terminal_status != terminal_status(item.terminal_state)
                    || request.result_sha256 != item.result_sha256
                    || request.target_cutex_session_id != item.subscriber_cutex_session_id
                    || request.output_reference != item.output_reference
                    || request.facts != expected_facts
                    || result_digest_v2(job, &request.facts)? != item.result_sha256
                {
                    return Err(JobError::Invalid(
                        "v2 outbox frozen request does not match its terminal snapshot".into(),
                    ));
                }
            }
            _ => {
                return Err(JobError::Invalid(
                    "completion wire version and frozen request are inconsistent".into(),
                ));
            }
        }
    }
    for job in state.jobs.values() {
        if let Some(execution) = &job.execution {
            if execution.basis != crate::process::EXECUTION_OBSERVATION_BASIS {
                return Err(JobError::Invalid(
                    "execution observation has an unsupported basis".into(),
                ));
            }
            if let (Some(start), Some(exit)) = (
                execution.start_observed_at_epoch_millis,
                execution.exit_observed_at_epoch_millis,
            ) && exit < start
            {
                return Err(JobError::Invalid(
                    "execution wall-clock observations are reversed".into(),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionResultSnapshotV2<'a> {
    job_id: &'a str,
    revision: u64,
    state: JobState,
    exit_code: Option<i32>,
    reason: Option<&'a str>,
    stdout: &'a crate::model::StreamSummary,
    stderr: &'a crate::model::StreamSummary,
    output_reference: &'a str,
    facts: &'a CompletionFactsV1,
}

pub(crate) fn terminal_status(state: JobState) -> &'static str {
    match state {
        JobState::Exited => "exited",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Interrupted => "interrupted",
        JobState::Registered
        | JobState::Queued
        | JobState::Stale
        | JobState::LaunchUnknown
        | JobState::LaunchPending
        | JobState::Running => "launch_unknown",
    }
}

pub(crate) fn project_outbox(job: &mut JobRecord, outbox: &OutboxRecord) {
    job.completion_delivery = CompletionDeliverySummary {
        enabled: job.completion_delivery.enabled,
        state: outbox.delivery_state,
        event_id: Some(outbox.event_id.clone()),
        last_error: outbox.last_error.clone(),
    };
}

pub(crate) fn read_bounded(
    path: &Path,
    offset: u64,
    max: usize,
) -> Result<(Vec<u8>, u64), JobError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = OpenOptions::new().read(true).open(path)?;
    let len = file.metadata()?.len();
    let at = offset.min(len);
    file.seek(SeekFrom::Start(at))?;
    let mut bytes = Vec::new();
    file.take(max as u64).read_to_end(&mut bytes)?;
    Ok((bytes, at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CONTRACT, CompletionDeliverySummary, ExecutionObservation, PersistedJobRequest,
        StreamSummary,
    };

    fn terminal_job() -> JobRecord {
        JobRecord {
            external: None,
            schema: CONTRACT.into(),
            job_id: "job_0123456789abcdef".into(),
            revision: 3,
            request_sha256: "11".repeat(32),
            request: PersistedJobRequest {
                action_id: "action-display-1".into(),
                argument_count: 2,
                command_summary: None,
                cwd: "/tmp/example".into(),
                environment_names: Vec::new(),
                subscriber_cutex_session_id: "cutex.11111111-1111-4111-8111-111111111111".into(),
                origin: None,
            },
            state: JobState::Exited,
            created_at_epoch_secs: 10,
            updated_at_epoch_secs: 12,
            process_id: None,
            process_start_ticks: None,
            exit_code: Some(0),
            terminal_reason: None,
            execution: Some(ExecutionObservation {
                basis: "runner_release_to_wait_v1".into(),
                start_observed_at_epoch_millis: Some(10_100),
                exit_observed_at_epoch_millis: Some(12_345),
                observed_run_duration_millis: Some(2_245),
            }),
            stdout: StreamSummary {
                retained_bytes: 5,
                observed_bytes: 9,
                truncated: true,
            },
            stderr: StreamSummary {
                retained_bytes: 0,
                observed_bytes: 0,
                truncated: false,
            },
            output_reference: "job-output:job_0123456789abcdef".into(),
            completion_delivery: CompletionDeliverySummary::default(),
        }
    }

    #[test]
    fn frozen_external_completion_without_additive_facts_remains_readable() {
        let mut job = terminal_job();
        job.external = Some(
            serde_json::from_value(serde_json::json!({
                "source":"nci","remoteId":"123.gadi-pbs","remoteName":null,
                "correlationToken":"a".repeat(32),"project":null,"queue":null,
                "remoteOutputPath":null,"registeredAt":10,"lastSourceEventAt":null,
            "resources":{"cpuSeconds":5}
            }))
            .unwrap(),
        );
        let mut state = StoredState {
            version: 3,
            ..StoredState::default()
        };
        insert_outbox(&mut state.outbox, &job, CompletionWireVersion::V2).unwrap();
        let item = state.outbox.values_mut().next().unwrap();
        let request = item.frozen_request_v2.as_mut().unwrap();
        request.facts.remote_id = None;
        request.facts.external_resources = None;
        request.result_sha256 = result_digest_v2(&job, &request.facts).unwrap();
        item.result_sha256 = request.result_sha256.clone();
        state.jobs.insert(job.job_id.clone(), job);
        validate_loaded_state(&state).unwrap();
    }

    #[test]
    fn v2_request_and_result_digest_have_fixed_vectors() {
        let mut outbox = BTreeMap::new();
        let job = terminal_job();
        insert_outbox(&mut outbox, &job, CompletionWireVersion::V2).unwrap();
        let item = outbox.values().next().unwrap();
        let facts = item.frozen_request_v2.as_ref().unwrap().facts.clone();
        assert_eq!(
            String::from_utf8(result_snapshot_v2_bytes(&job, &facts).unwrap()).unwrap(),
            concat!(
                "{\"jobId\":\"job_0123456789abcdef\",\"revision\":3,",
                "\"state\":\"exited\",\"exitCode\":0,\"reason\":null,",
                "\"stdout\":{\"retainedBytes\":5,\"observedBytes\":9,\"truncated\":true},",
                "\"stderr\":{\"retainedBytes\":0,\"observedBytes\":0,\"truncated\":false},",
                "\"outputReference\":\"job-output:job_0123456789abcdef\",",
                "\"facts\":{\"factsVersion\":1,\"actionId\":\"action-display-1\",",
                "\"exitCode\":0,\"execution\":{\"basis\":\"runner_release_to_wait_v1\",",
                "\"startObservedAtEpochMillis\":10100,\"exitObservedAtEpochMillis\":12345,",
                "\"observedRunDurationMillis\":2245},",
                "\"stdout\":{\"retainedBytes\":5,\"observedBytes\":9,\"truncated\":true},",
                "\"stderr\":{\"retainedBytes\":0,\"observedBytes\":0,\"truncated\":false}}}"
            )
        );
        let request = serde_json::to_string(item.frozen_request_v2.as_ref().unwrap()).unwrap();
        assert_eq!(
            request,
            concat!(
                "{\"schema\":\"cutex.job_service.completion.v2\",",
                "\"eventId\":\"job-terminal:job_0123456789abcdef:3\",",
                "\"jobId\":\"job_0123456789abcdef\",\"jobRevision\":3,",
                "\"terminalStatus\":\"exited\",",
                "\"resultSha256\":\"a6beba755d57cffcadf03cd758d661b300febe5183d5fcd016321a37fe969ce1\",",
                "\"targetCutexSessionId\":\"cutex.11111111-1111-4111-8111-111111111111\",",
                "\"facts\":{\"factsVersion\":1,\"actionId\":\"action-display-1\",",
                "\"exitCode\":0,\"execution\":{\"basis\":\"runner_release_to_wait_v1\",",
                "\"startObservedAtEpochMillis\":10100,\"exitObservedAtEpochMillis\":12345,",
                "\"observedRunDurationMillis\":2245},",
                "\"stdout\":{\"retainedBytes\":5,\"observedBytes\":9,\"truncated\":true},",
                "\"stderr\":{\"retainedBytes\":0,\"observedBytes\":0,\"truncated\":false}},",
                "\"outputReference\":\"job-output:job_0123456789abcdef\"}"
            )
        );
        assert_eq!(
            item.result_sha256,
            "a6beba755d57cffcadf03cd758d661b300febe5183d5fcd016321a37fe969ce1"
        );
    }

    #[test]
    fn stable_event_rejects_changed_facts_but_ignores_delivery_counters() {
        let mut outbox = BTreeMap::new();
        let job = terminal_job();
        insert_outbox(&mut outbox, &job, CompletionWireVersion::V2).unwrap();
        outbox.values_mut().next().unwrap().attempt_count = 7;
        insert_outbox(&mut outbox, &job, CompletionWireVersion::V2).unwrap();
        let mut changed = job;
        changed.stdout.observed_bytes += 1;
        assert!(matches!(
            insert_outbox(&mut outbox, &changed, CompletionWireVersion::V2),
            Err(JobError::Conflict(_))
        ));
    }
}
