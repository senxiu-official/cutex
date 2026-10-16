use crate::grant::{semantic_request_sha256, verify_caller_grant, verify_grant};
use crate::model::*;
use crate::process::{self, LiveProcess};
use crate::store::{Store, StoredState, insert_outbox, project_outbox, read_bounded};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub state_root: PathBuf,
    pub grant_key: Vec<u8>,
    pub api_token: Vec<u8>,
    pub runner_executable: PathBuf,
    pub per_stream_output_bytes: u64,
    pub max_read_bytes: usize,
    pub cancel_grace: Duration,
    pub max_jobs: usize,
    pub max_active_jobs: usize,
    pub allowed_launchers: std::collections::BTreeMap<String, String>,
    pub completion_wire_version: CompletionWireVersion,
    pub completion_enabled: bool,
    pub external_sources: Vec<String>,
    pub external_stale_after_secs: u64,
    /// `--external-notify-file`: owner-configured notification address file
    /// (written by Cutex Management), read on every `prepare_remote`.
    pub external_notify_file: Option<PathBuf>,
}

impl ServiceConfig {
    pub fn validate(&self) -> Result<(), JobError> {
        if self.external_sources.iter().any(|source| source != "nci")
            || self.external_sources.len() > 1
            || self.external_stale_after_secs == 0
        {
            return Err(JobError::Invalid(
                "invalid external source configuration".into(),
            ));
        }
        if !self.external_sources.is_empty()
            && self.completion_enabled
            && self.completion_wire_version != CompletionWireVersion::V2
        {
            return Err(JobError::Invalid(
                "external completion delivery requires v2".into(),
            ));
        }
        if self.grant_key.len() < 32 {
            return Err(JobError::Invalid(
                "grant key must contain at least 32 bytes".into(),
            ));
        }
        if self.api_token.len() < 32 {
            return Err(JobError::Invalid(
                "API token must contain at least 32 bytes".into(),
            ));
        }
        if !self.runner_executable.is_absolute() || !self.runner_executable.is_file() {
            return Err(JobError::Invalid(
                "runner executable must be an existing absolute file".into(),
            ));
        }
        if !(4096..=64 * 1024 * 1024).contains(&self.per_stream_output_bytes) {
            return Err(JobError::Invalid(
                "per-stream output limit must be 4 KiB..=64 MiB".into(),
            ));
        }
        if !(1..=1024 * 1024).contains(&self.max_read_bytes) {
            return Err(JobError::Invalid("read limit must be 1..=1 MiB".into()));
        }
        if !(Duration::from_millis(100)..=Duration::from_secs(30)).contains(&self.cancel_grace) {
            return Err(JobError::Invalid(
                "cancel grace must be 100 ms..=30 s".into(),
            ));
        }
        if self.max_jobs == 0
            || self.max_jobs > 100_000
            || self.max_active_jobs == 0
            || self.max_active_jobs > 256
            || self.max_active_jobs > self.max_jobs
        {
            return Err(JobError::Invalid("job count limits are invalid".into()));
        }
        if self.allowed_launchers.is_empty() {
            return Err(JobError::Invalid(
                "at least one sandbox launcher is required".into(),
            ));
        }
        for (path, digest) in &self.allowed_launchers {
            let canonical = std::fs::canonicalize(path)?;
            if canonical.to_string_lossy() != path.as_str()
                || crate::grant::file_sha256(&canonical)? != *digest
            {
                return Err(JobError::Invalid(
                    "sandbox launcher allowlist identity mismatch".into(),
                ));
            }
        }
        Ok(())
    }
}

struct Inner {
    config: ServiceConfig,
    store: Store,
    state: Mutex<StoredState>,
    live: Mutex<HashMap<String, LiveProcess>>,
    executable: PathBuf,
}

#[derive(Clone)]
pub struct JobService {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone)]
pub(crate) struct CompletionAttempt {
    pub record: OutboxRecord,
    pub query_only: bool,
}

pub(crate) struct CompletionAttemptResult {
    pub delivery_state: CompletionDeliveryState,
    pub receipt: Option<CompletionReceipt>,
    pub last_error: Option<String>,
    pub next_attempt_at_epoch_millis: u64,
    pub final_for_retention: bool,
}

impl JobService {
    pub fn open(config: ServiceConfig) -> Result<Self, JobError> {
        config.validate()?;
        let (store, state) = Store::open(
            config.state_root.clone(),
            config.completion_wire_version,
            config.completion_enabled,
        )?;
        store.ensure_output_dir()?;
        let executable = config.runner_executable.clone();
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                store,
                state: Mutex::new(state),
                live: Mutex::new(HashMap::new()),
                executable,
            }),
        })
    }

    pub fn prepare_remote(
        &self,
        api_token: &[u8],
        grant: &CallerGrant,
        source: &str,
    ) -> Result<PrepareRemoteReceipt, JobError> {
        self.authenticate(api_token)?;
        self.require_external_source(source)?;
        verify_caller_grant(
            &self.inner.config.grant_key,
            grant,
            CallerOperation::PrepareRemote,
            source,
            &grant.payload.subject_cutex_session_id,
            now_secs(),
        )?;
        let now = now_secs();
        let mut state = self.inner.state.lock().expect("state mutex poisoned");
        state
            .prepared_remote
            .retain(|_, value| value.job_id.is_none() && value.expires_at > now);
        if state
            .prepared_remote
            .values()
            .filter(|value| value.subscriber == grant.payload.subject_cutex_session_id)
            .count()
            >= 16
        {
            return Err(JobError::Conflict(
                "subscriber remote preparation capacity reached".into(),
            ));
        }
        if state.prepared_remote.len() >= self.inner.config.max_jobs {
            return Err(JobError::Conflict(
                "remote preparation capacity reached".into(),
            ));
        }
        let mut random = [0u8; 16];
        std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut random)?;
        let token = hex::encode(random);
        let expires_at = now + 86_400;
        state.prepared_remote.insert(
            token.clone(),
            PreparedRemote {
                source: source.into(),
                subscriber: grant.payload.subject_cutex_session_id.clone(),
                expires_at,
                job_id: None,
            },
        );
        self.inner.store.save(&mut state)?;
        drop(state);
        let name = format!("cx{token}");
        let submission_arguments: Vec<String> =
            vec!["-N".into(), name.clone(), "-m".into(), "abe".into()];
        let mut pbs_args = submission_arguments.clone();
        let (notification_address, notification_status, owner_action) =
            match crate::notify::read_notify_address(
                self.inner.config.external_notify_file.as_deref(),
                source,
            ) {
                crate::notify::NotifyAddress::Configured(address) => {
                    pbs_args.extend(["-M".to_string(), address.clone()]);
                    (Some(address), "configured", None)
                }
                crate::notify::NotifyAddress::NotConfigured => (
                    None,
                    "mailNotConfigured",
                    Some("No notification mailbox is configured for this source, so PBS mail cannot reach Cutex. Tell the owner to set the NCI mailbox in Cutex settings (Remote jobs); until then the owner checks the job and marks the result manually. Do not invent or ask for a mail address.".to_string()),
                ),
                crate::notify::NotifyAddress::Unreadable => (
                    None,
                    "mailConfigUnreadable",
                    Some("The owner's notification mailbox setting could not be read. Tell the owner to check the Remote jobs mailbox settings in Cutex. Do not invent or ask for a mail address.".to_string()),
                ),
            };
        Ok(PrepareRemoteReceipt {
            source: source.into(),
            correlation_token: token,
            suggested_job_name: name,
            submission_arguments,
            notification_destination_required: true,
            expires_at,
            pbs_args,
            notification_address,
            notification_status: notification_status.into(),
            owner_action,
        })
    }

    pub fn register_remote(
        &self,
        api_token: &[u8],
        grant: &CallerGrant,
        mut request: RemoteJobRequest,
    ) -> Result<SubmitReceipt, JobError> {
        self.authenticate(api_token)?;
        self.require_external_source(&request.source)?;
        let digest = crate::grant::remote_request_sha256(&request)?;
        verify_caller_grant(
            &self.inner.config.grant_key,
            grant,
            CallerOperation::RegisterRemote,
            &digest,
            &grant.payload.subject_cutex_session_id,
            now_secs(),
        )?;
        request.remote_id = normalize_nci_remote_id(&request.remote_id)?;
        let digest = crate::grant::remote_request_sha256(&request)?;
        if request.correlation_token.len() != 32
            || !request
                .correlation_token
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
        {
            return Err(JobError::Invalid("invalid correlation token".into()));
        }
        validate_external_text(&request.remote_id, 128, false)?;
        if !request
            .remote_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-[]".contains(&c))
        {
            return Err(JobError::Invalid("invalid remote ID".into()));
        }
        if request
            .remote_name
            .as_ref()
            .is_some_and(|name| !name.ends_with(&format!("cx{}", request.correlation_token)))
        {
            return Err(JobError::Invalid(
                "remote name must end in the prepared correlation marker".into(),
            ));
        }
        for value in [&request.remote_name, &request.project, &request.queue]
            .into_iter()
            .flatten()
        {
            validate_external_text(value, 236, false)?;
        }
        if let Some(path) = &request.remote_output_path {
            if !path.starts_with('/')
                || path.len() > 1024
                || !path
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c))
            {
                return Err(JobError::Invalid(
                    "remote output path must be an absolute bounded path".into(),
                ));
            }
        }
        let mut state = self.inner.state.lock().expect("state mutex poisoned");
        if let Some(job) = state.jobs.values().find(|job| {
            job.external
                .as_ref()
                .is_some_and(|external| external.correlation_token == request.correlation_token)
        }) {
            if job.request.subscriber_cutex_session_id != grant.payload.subject_cutex_session_id
                || job.external.as_ref().expect("external match").source != request.source
            {
                return Err(JobError::Unauthorized(
                    "remote registration belongs to another subscriber or source".into(),
                ));
            }
            let mut legacy_request = request.clone();
            legacy_request.remote_id = job
                .external
                .as_ref()
                .expect("external match")
                .remote_id
                .clone();
            if normalize_nci_remote_id(&legacy_request.remote_id)? != request.remote_id
                || (job.request_sha256 != digest
                    && job.request_sha256 != crate::grant::remote_request_sha256(&legacy_request)?)
            {
                return Err(JobError::Conflict(
                    "token already registered with different semantics".into(),
                ));
            }
            return Ok(SubmitReceipt {
                status: "committed".into(),
                job: agent_job_projection(job.clone()),
                deduplicated: true,
            });
        }
        let prepared = state
            .prepared_remote
            .get(&request.correlation_token)
            .ok_or_else(|| JobError::Unauthorized("unknown remote preparation".into()))?;
        if prepared.subscriber != grant.payload.subject_cutex_session_id
            || prepared.source != request.source
        {
            return Err(JobError::Unauthorized(
                "remote preparation belongs to another subscriber or source".into(),
            ));
        }
        if prepared.expires_at <= now_secs() {
            return Err(JobError::Unauthorized("remote preparation expired".into()));
        }
        if state.jobs.len() >= self.inner.config.max_jobs {
            return Err(JobError::Conflict("job capacity reached".into()));
        }
        if state.jobs.values().any(|j| {
            j.external.as_ref().is_some_and(|e| {
                e.source == request.source
                    && normalize_nci_remote_id(&e.remote_id).ok().as_deref()
                        == Some(request.remote_id.as_str())
            }) && !j.state.terminal()
        }) {
            return Err(JobError::Conflict(
                "remote job is already registered".into(),
            ));
        }
        let now = now_secs();
        let job_id = format!("job_{}", uuid::Uuid::new_v4().simple());
        let output_reference = request
            .remote_output_path
            .clone()
            .unwrap_or_else(|| format!("external-job:{job_id}"));
        let record = JobRecord {
            external: Some(ExternalJob {
                source: request.source,
                remote_id: request.remote_id,
                remote_name: request.remote_name,
                correlation_token: request.correlation_token.clone(),
                project: request.project,
                queue: request.queue,
                remote_output_path: request.remote_output_path,
                stale: false,
                registered_at: now,
                last_source_event_at: None,
                human_audit: Vec::new(),
                source_events: Vec::new(),
                resources: None,
            }),
            schema: CONTRACT.into(),
            job_id: job_id.clone(),
            revision: 1,
            request_sha256: digest,
            request: PersistedJobRequest {
                action_id: format!("remote:{job_id}"),
                argument_count: 0,
                command_summary: None,
                cwd: String::new(),
                environment_names: Vec::new(),
                subscriber_cutex_session_id: prepared.subscriber.clone(),
                origin: None,
            },
            state: JobState::Registered,
            created_at_epoch_secs: now,
            updated_at_epoch_secs: now,
            process_id: None,
            process_start_ticks: None,
            exit_code: None,
            terminal_reason: None,
            execution: None,
            stdout: StreamSummary {
                retained_bytes: 0,
                observed_bytes: 0,
                truncated: false,
            },
            stderr: StreamSummary {
                retained_bytes: 0,
                observed_bytes: 0,
                truncated: false,
            },
            output_reference,
            completion_delivery: CompletionDeliverySummary {
                enabled: self.inner.config.completion_enabled,
                state: if self.inner.config.completion_enabled {
                    CompletionDeliveryState::AwaitingTerminal
                } else {
                    CompletionDeliveryState::Disabled
                },
                ..Default::default()
            },
        };
        state.prepared_remote.remove(&request.correlation_token);
        state.jobs.insert(job_id, record.clone());
        self.inner.store.save(&mut state)?;
        Ok(SubmitReceipt {
            status: "committed".into(),
            job: agent_job_projection(record),
            deduplicated: false,
        })
    }

    /// Root-only bounded enumeration for verification when mail is unavailable.
    pub fn list_external_verification_targets(
        &self,
        api_token: &[u8],
        request: ExternalVerificationList,
    ) -> Result<ExternalVerificationPage, JobError> {
        self.authenticate(api_token)?;
        if !(1..=64).contains(&request.limit) {
            return Err(JobError::Invalid("verification limit must be 1..64".into()));
        }
        if let Some(cursor) = &request.cursor {
            validate_external_text(cursor, 128, false)?;
        }
        if self.require_external_source("nci").is_err() {
            return Ok(ExternalVerificationPage {
                targets: Vec::new(),
                next_cursor: None,
            });
        }
        let state = self.inner.state.lock().expect("state mutex poisoned");
        let mut jobs: Vec<_> = state
            .jobs
            .values()
            .filter(|job| {
                !job.state.terminal()
                    && request
                        .cursor
                        .as_ref()
                        .is_none_or(|cursor| job.job_id > *cursor)
                    && job.external.as_ref().is_some_and(|external| {
                        external.source == "nci" && external.source_events.len() < 256
                    })
            })
            .collect();
        jobs.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        let next_cursor =
            (jobs.len() > request.limit).then(|| jobs[request.limit - 1].job_id.clone());
        let targets = jobs
            .into_iter()
            .take(request.limit)
            .map(|job| {
                let external = job.external.as_ref().expect("external job");
                Ok(ExternalVerificationTarget {
                    current_state: job.state,
                    remote_id: normalize_nci_remote_id(&external.remote_id)?,
                    correlation_token: external.correlation_token.clone(),
                    remote_name: external.remote_name.clone(),
                    project: external.project.clone(),
                })
            })
            .collect::<Result<Vec<_>, JobError>>()?;
        Ok(ExternalVerificationPage {
            targets,
            next_cursor,
        })
    }

    /// Resolve a trusted source trigger to stored metadata before any remote query.
    pub fn lookup_external_event(
        &self,
        api_token: &[u8],
        request: ExternalEventLookup,
    ) -> Result<ExternalEventLookupReceipt, JobError> {
        self.authenticate(api_token)?;
        let receipt = |status: &str| ExternalEventLookupReceipt {
            status: status.into(),
            target: None,
        };
        if self.require_external_source(&request.source).is_err() {
            return Ok(receipt("disabled_source"));
        }
        let remote_id = normalize_nci_remote_id(&request.remote_id)?;
        if request.correlation_token.len() != 32
            || !request
                .correlation_token
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
        {
            return Err(JobError::Invalid(
                "invalid external correlation token".into(),
            ));
        }
        let state = self.inner.state.lock().expect("state mutex poisoned");
        let mut matches = state.jobs.values().filter(|job| {
            job.external.as_ref().is_some_and(|external| {
                external.source == request.source
                    && normalize_nci_remote_id(&external.remote_id).ok().as_deref()
                        == Some(remote_id.as_str())
                    && external.correlation_token == request.correlation_token
            })
        });
        let Some(job) = matches.next() else {
            return Ok(receipt("unmatched"));
        };
        if matches.next().is_some() {
            return Ok(receipt("permanent_conflict"));
        }
        if job.state.terminal() {
            return Ok(receipt("terminal_ignored"));
        }
        let external = job.external.as_ref().expect("matched external job");
        if external.source_events.len() >= 256 {
            return Ok(receipt("permanent_conflict"));
        }
        Ok(ExternalEventLookupReceipt {
            status: "matched".into(),
            target: Some(ExternalVerificationTarget {
                current_state: job.state,
                remote_id: normalize_nci_remote_id(&external.remote_id)?,
                correlation_token: external.correlation_token.clone(),
                remote_name: external.remote_name.clone(),
                project: external.project.clone(),
            }),
        })
    }

    /// Trusted source ingress, never exposed by the model-facing adapter.
    pub fn ingest_external_event(
        &self,
        api_token: &[u8],
        mut event: ExternalSourceEvent,
    ) -> Result<ExternalEventReceipt, JobError> {
        use sha2::Digest;
        self.authenticate(api_token)?;
        if self.require_external_source(&event.source).is_err() {
            return Ok(ExternalEventReceipt {
                status: "disabled_source".into(),
                retryable: false,
                job_id: None,
                revision: None,
            });
        }
        event.remote_id = normalize_nci_remote_id(&event.remote_id)?;
        validate_external_text(&event.event_id, 1024, false)?;
        validate_external_text(&event.remote_id, 128, false)?;
        if event.correlation_token.len() != 32
            || !event
                .correlation_token
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
            || !matches!(
                event.state,
                JobState::Queued
                    | JobState::Running
                    | JobState::Exited
                    | JobState::Failed
                    | JobState::Cancelled
            )
            || (event.state == JobState::Exited && event.exit_code != Some(0))
            || event.exit_code.is_some_and(|code| code < 0)
            || (matches!(event.state, JobState::Queued | JobState::Running)
                && (event.exit_code.is_some() || event.reason.is_some()))
        {
            return Err(JobError::Invalid("invalid external source event".into()));
        }
        if let Some(resources) = &event.resources {
            if resources.cpu_seconds.is_some_and(|n| n > 1_000_000_000)
                || resources
                    .walltime_seconds
                    .is_some_and(|n| n > 1_000_000_000)
                || resources.memory_bytes.is_some_and(|n| n > 1 << 50)
                || resources.vmem_bytes.is_some_and(|n| n > 1 << 50)
                || resources.jobfs_bytes.is_some_and(|n| n > 1 << 50)
                || resources.cpu_percent.is_some_and(|n| n > 100_000_000)
                || resources.ncpus.is_some_and(|n| n == 0 || n > 1_000_000)
            {
                return Err(JobError::Invalid(
                    "external resource values exceed bounds".into(),
                ));
            }
        }
        let event_id_sha256 = hex::encode(Sha256::digest(event.event_id.as_bytes()));
        let event_sha256 = hex::encode(Sha256::digest(serde_json::to_vec(&event)?));
        let mut state = self.inner.state.lock().expect("state mutex poisoned");
        let matches: Vec<_> = state
            .jobs
            .values()
            .filter(|job| {
                job.external.as_ref().is_some_and(|external| {
                    external.source == event.source
                        && normalize_nci_remote_id(&external.remote_id).ok().as_deref()
                            == Some(event.remote_id.as_str())
                        && external.correlation_token == event.correlation_token
                })
            })
            .map(|job| job.job_id.clone())
            .collect();
        let [job_id] = matches.as_slice() else {
            if matches.is_empty() {
                return Ok(ExternalEventReceipt {
                    status: "unmatched".into(),
                    retryable: true,
                    job_id: None,
                    revision: None,
                });
            }
            return Ok(ExternalEventReceipt {
                status: "permanent_conflict".into(),
                retryable: false,
                job_id: None,
                revision: None,
            });
        };
        let mut job = state.jobs.get(job_id).expect("matched job").clone();
        let external = job.external.as_mut().expect("matched external job");
        if let Some(previous) = external
            .source_events
            .iter()
            .find(|receipt| receipt.event_id_sha256 == event_id_sha256)
        {
            if previous.event_sha256 != event_sha256 {
                return Ok(ExternalEventReceipt {
                    status: "permanent_conflict".into(),
                    retryable: false,
                    job_id: Some(job_id.clone()),
                    revision: Some(job.revision),
                });
            }
            return Ok(ExternalEventReceipt {
                status: "duplicate".into(),
                retryable: false,
                job_id: Some(job_id.clone()),
                revision: Some(job.revision),
            });
        }
        if job.state.terminal() {
            return Ok(ExternalEventReceipt {
                status: "terminal_ignored".into(),
                retryable: false,
                job_id: Some(job_id.clone()),
                revision: Some(job.revision),
            });
        }
        if job.state == JobState::Running && event.state == JobState::Queued {
            return Ok(ExternalEventReceipt {
                status: "superseded".into(),
                retryable: false,
                job_id: Some(job_id.clone()),
                revision: Some(job.revision),
            });
        }
        let unchanged_nonterminal = !event.state.terminal() && job.state == event.state;
        if !unchanged_nonterminal && external.source_events.len() >= 256 {
            return Ok(ExternalEventReceipt {
                status: "permanent_conflict".into(),
                retryable: false,
                job_id: Some(job_id.clone()),
                revision: Some(job.revision),
            });
        }
        let now = now_secs();
        if !unchanged_nonterminal {
            external.source_events.push(ExternalSourceReceipt {
                event_id_sha256,
                event_sha256,
            });
        }
        external.last_source_event_at = Some(now);
        external.stale = false;
        if event.resources.is_some() {
            external.resources = event.resources;
        }
        // Stale is a freshness indication, not a regression in scheduler state;
        // a newly verified running event clears it without starting any process.
        job.state = event.state;
        job.exit_code = event.exit_code;
        job.terminal_reason = event.reason.map(|reason| {
            match reason {
                ExternalReason::WalltimeExceeded => "walltime_exceeded",
                ExternalReason::Aborted => "aborted",
                ExternalReason::NonzeroExit => "nonzero_exit",
                ExternalReason::Cancelled => "cancelled",
            }
            .into()
        });
        job.revision += 1;
        job.updated_at_epoch_secs = now;
        if job.state.terminal() {
            insert_outbox(&mut state.outbox, &job, CompletionWireVersion::V2)?;
            let item = state
                .outbox
                .values()
                .find(|item| item.job_id == *job_id)
                .expect("inserted outbox");
            project_outbox(&mut job, item);
        }
        let revision = job.revision;
        state.jobs.insert(job_id.clone(), job);
        self.inner.store.save(&mut state)?;
        Ok(ExternalEventReceipt {
            status: "applied".into(),
            retryable: false,
            job_id: Some(job_id.clone()),
            revision: Some(revision),
        })
    }

    /// Owner administration only. A note is audited privately, never included in completion facts.
    pub fn human_mark_external(
        &self,
        api_token: &[u8],
        job_id: &str,
        expected_revision: u64,
        next: JobState,
        exit_code: Option<i32>,
        note: Option<String>,
    ) -> Result<JobRecord, JobError> {
        self.authenticate(api_token)?;
        if !matches!(
            next,
            JobState::Exited | JobState::Failed | JobState::Cancelled
        ) || (next == JobState::Exited && exit_code.is_none())
            || exit_code.is_some_and(|code| code < 0)
        {
            return Err(JobError::Invalid(
                "invalid external terminal state or exit code".into(),
            ));
        }
        if note.as_ref().is_some_and(|value| {
            value.len() > 2048
                || value
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
        }) {
            return Err(JobError::Invalid("invalid owner note".into()));
        }
        let mut state = self.inner.state.lock().expect("state mutex poisoned");
        let job = state
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| JobError::NotFound(job_id.into()))?;
        if job.revision != expected_revision || job.state.terminal() {
            return Err(JobError::Conflict(
                "job revision mismatch or already terminal".into(),
            ));
        }
        let external = job
            .external
            .as_mut()
            .ok_or_else(|| JobError::Invalid("not an external job".into()))?;
        let now = now_secs();
        external.human_audit.push(ExternalHumanAudit {
            actor: Some(ExternalAuditActor::default()),
            at: now,
            previous_revision: job.revision,
            state: next,
            exit_code,
            note,
        });
        external.stale = false;
        job.state = next;
        job.exit_code = exit_code;
        job.revision += 1;
        job.updated_at_epoch_secs = now;
        job.terminal_reason = Some("external_owner_mark".into());
        let mut record = job.clone();
        insert_outbox(&mut state.outbox, &record, CompletionWireVersion::V2)?;
        let item = state
            .outbox
            .values()
            .find(|item| item.job_id == job_id)
            .expect("inserted outbox");
        project_outbox(&mut record, item);
        state.jobs.insert(job_id.into(), record.clone());
        self.inner.store.save(&mut state)?;
        Ok(record)
    }

    fn require_external_source(&self, source: &str) -> Result<(), JobError> {
        if !self
            .inner
            .config
            .external_sources
            .iter()
            .any(|s| s == source)
        {
            return Err(JobError::Invalid("external source is disabled".into()));
        }
        Ok(())
    }

    /// Staleness is derived at read time from the last verified source event,
    /// never stored as a mutation: flipping it must not bump `revision`, or an
    /// owner `humanMarkExternal` with the revision just read would lose a race
    /// against a pure freshness change.
    fn external_stale(&self, job: &JobRecord, now: u64) -> bool {
        job.external.as_ref().is_some_and(|external| {
            !job.state.terminal()
                && now.saturating_sub(
                    external
                        .last_source_event_at
                        .unwrap_or(external.registered_at),
                ) >= self.inner.config.external_stale_after_secs
        })
    }

    pub fn submit(
        &self,
        api_token: &[u8],
        request: JobRequest,
        grant: ExecutionGrant,
    ) -> Result<SubmitReceipt, JobError> {
        self.authenticate(api_token)?;
        validate_request(&request)?;
        if !request.environment.is_empty() {
            return Err(JobError::Unauthorized(
                "JS1 does not permit request environment injection".into(),
            ));
        }
        let now = now_secs();
        verify_grant(
            &self.inner.config.grant_key,
            &self.inner.config.allowed_launchers,
            &request,
            &grant,
            now,
        )?;
        let digest = semantic_request_sha256(&request)?;
        {
            let state = self.inner.state.lock().expect("state mutex poisoned");
            if let Some(job_id) = state.action_index.get(&request.action_id) {
                let existing = state.jobs.get(job_id).expect("action index is consistent");
                if existing.request_sha256 != digest {
                    return Err(JobError::Conflict(
                        "action ID was reused with different semantics".into(),
                    ));
                }
                return Ok(SubmitReceipt {
                    status: "committed".into(),
                    job: existing.clone(),
                    deduplicated: true,
                });
            }
        }
        let job_id = format!("job_{}", uuid::Uuid::new_v4().simple());
        let output_reference = format!("job-output:{job_id}");
        let record = JobRecord {
            external: None,
            schema: CONTRACT.into(),
            job_id: job_id.clone(),
            revision: 1,
            request_sha256: digest,
            request: PersistedJobRequest::from(&request),
            state: JobState::LaunchPending,
            created_at_epoch_secs: now,
            updated_at_epoch_secs: now,
            process_id: None,
            process_start_ticks: None,
            exit_code: None,
            terminal_reason: None,
            execution: None,
            stdout: StreamSummary {
                retained_bytes: 0,
                observed_bytes: 0,
                truncated: false,
            },
            stderr: StreamSummary {
                retained_bytes: 0,
                observed_bytes: 0,
                truncated: false,
            },
            output_reference,
            completion_delivery: CompletionDeliverySummary {
                enabled: self.inner.config.completion_enabled,
                state: if self.inner.config.completion_enabled {
                    CompletionDeliveryState::AwaitingTerminal
                } else {
                    CompletionDeliveryState::Disabled
                },
                ..Default::default()
            },
        };
        {
            let mut state = self.inner.state.lock().expect("state mutex poisoned");
            // CAS after the earlier read covers concurrent submissions in this process.
            if let Some(job_id) = state.action_index.get(&request.action_id) {
                let existing = state.jobs.get(job_id).expect("action index is consistent");
                if existing.request_sha256 != record.request_sha256 {
                    return Err(JobError::Conflict(
                        "action ID was reused with different semantics".into(),
                    ));
                }
                return Ok(SubmitReceipt {
                    status: "committed".into(),
                    job: existing.clone(),
                    deduplicated: true,
                });
            }
            if state.jobs.len() >= self.inner.config.max_jobs {
                return Err(JobError::Conflict("service job-record capacity reached; acknowledged terminal cleanup is required".into()));
            }
            if self.inner.live.lock().expect("live mutex poisoned").len()
                >= self.inner.config.max_active_jobs
            {
                return Err(JobError::Conflict("active job capacity reached".into()));
            }
            state
                .action_index
                .insert(request.action_id.clone(), job_id.clone());
            state.jobs.insert(job_id.clone(), record.clone());
            self.inner.store.save(&mut state)?;
        }

        let sandbox_json = serde_json::to_string(&grant.payload.sandbox_state)?;
        let spawned = match process::spawn_contained(
            &self.inner.executable,
            &grant.payload.launcher_path,
            &sandbox_json,
            &request.argv,
            &request.cwd,
            &request.environment,
            &self.inner.store.output_path(&job_id, "stdout"),
            &self.inner.store.output_path(&job_id, "stderr"),
            self.inner.config.per_stream_output_bytes,
        ) {
            Ok(spawned) => spawned,
            Err(error) => {
                let mut state = self.inner.state.lock().expect("state mutex poisoned");
                let job = state.jobs.get_mut(&job_id).expect("committed job exists");
                job.state = JobState::Failed;
                job.revision += 1;
                job.updated_at_epoch_secs = now_secs();
                job.terminal_reason = Some(format!("launch failed: {error}"));
                let snapshot = job.clone();
                insert_outbox(
                    &mut state.outbox,
                    &snapshot,
                    self.inner.config.completion_wire_version,
                )?;
                if let Some(outbox) = state
                    .outbox
                    .values()
                    .find(|item| item.job_id == job_id)
                    .cloned()
                    && let Some(job) = state.jobs.get_mut(&job_id)
                {
                    project_outbox(job, &outbox);
                }
                self.inner.store.save(&mut state)?;
                return Ok(SubmitReceipt {
                    status: "committed".into(),
                    job: state
                        .jobs
                        .get(&job_id)
                        .expect("committed job exists")
                        .clone(),
                    deduplicated: false,
                });
            }
        };
        let pid = spawned.live.pid;
        let start_ticks = spawned.live.start_ticks;
        let cancelled = Arc::clone(&spawned.live.cancelled);
        self.inner
            .live
            .lock()
            .expect("live mutex poisoned")
            .insert(job_id.clone(), spawned.live);
        {
            let mut state = self.inner.state.lock().expect("state mutex poisoned");
            let job = state.jobs.get_mut(&job_id).expect("committed job exists");
            job.state = JobState::Running;
            job.revision += 1;
            job.updated_at_epoch_secs = now_secs();
            job.process_id = Some(pid);
            job.process_start_ticks = Some(start_ticks);
            job.execution = Some(ExecutionObservation {
                basis: process::EXECUTION_OBSERVATION_BASIS.into(),
                start_observed_at_epoch_millis: spawned.execution_start.wall_epoch_millis,
                exit_observed_at_epoch_millis: None,
                observed_run_duration_millis: None,
            });
            self.inner.store.save(&mut state)?;
        }
        let inner = Arc::clone(&self.inner);
        let watcher_job_id = job_id.clone();
        std::thread::spawn(move || {
            let mut child = spawned.child;
            let status = child.wait();
            let wait_elapsed = spawned.execution_start.monotonic.elapsed();
            let wait_wall = SystemTime::now();
            // Closing the liveness descriptor asks the sentinel to sweep descendants.
            let live = inner
                .live
                .lock()
                .expect("live mutex poisoned")
                .remove(&watcher_job_id);
            drop(live);
            let mut sentinel = spawned.sentinel;
            let _ = sentinel.wait();
            let _ = spawned.stdout_drain.join();
            let _ = spawned.stderr_drain.join();
            let mut state = inner.state.lock().expect("state mutex poisoned");
            let Some(job) = state.jobs.get_mut(&watcher_job_id) else {
                return;
            };
            let observed_out = spawned
                .stdout_observed
                .load(std::sync::atomic::Ordering::Relaxed);
            let observed_err = spawned
                .stderr_observed
                .load(std::sync::atomic::Ordering::Relaxed);
            job.stdout = summary(
                observed_out,
                &inner.store.output_path(&watcher_job_id, "stdout"),
            );
            job.stderr = summary(
                observed_err,
                &inner.store.output_path(&watcher_job_id, "stderr"),
            );
            job.process_id = None;
            job.process_start_ticks = None;
            job.revision += 1;
            job.updated_at_epoch_secs = now_secs();
            match status {
                Ok(status) => {
                    if let Some(observation) = job.execution.as_mut() {
                        process::complete_observation(observation, wait_wall, wait_elapsed);
                    }
                    job.exit_code = process::exit_code(status);
                    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        job.state = JobState::Cancelled;
                        job.terminal_reason = Some("cancelled".into());
                    } else if status.success() {
                        job.state = JobState::Exited;
                    } else {
                        job.state = JobState::Failed;
                        job.terminal_reason = Some("command exited unsuccessfully".into());
                    }
                }
                Err(error) => {
                    job.state = JobState::LaunchUnknown;
                    job.terminal_reason = Some(format!("wait failed: {error}"));
                }
            }
            let snapshot = job.clone();
            let persisted = insert_outbox(
                &mut state.outbox,
                &snapshot,
                inner.config.completion_wire_version,
            )
            .and_then(|_| {
                if let Some(outbox) = state
                    .outbox
                    .values()
                    .find(|item| item.job_id == watcher_job_id)
                    .cloned()
                    && let Some(job) = state.jobs.get_mut(&watcher_job_id)
                {
                    project_outbox(job, &outbox);
                }
                inner.store.save(&mut state)
            });
            let _ = persisted;
        });
        Ok(SubmitReceipt {
            status: "committed".into(),
            job: self.query_inner(&job_id)?,
            deduplicated: false,
        })
    }

    /// Installation preflight: report the exact launchers accepted by this daemon.
    pub fn capabilities(&self, api_token: &[u8]) -> Result<serde_json::Value, JobError> {
        self.authenticate(api_token)?;
        Ok(
            serde_json::json!({"schema":"cutex/job-service-capabilities/v1",
            "allowedLaunchers":self.inner.config.allowed_launchers,
            "independentExecutionCwd":true,
            "sources":self.inner.config.external_sources,
            "externalNotifyFile":self.inner.config.external_notify_file.is_some(),
            "humanOperations":["list","cancel","readOutput","markExternal"],"completionDelivery":{"enabled":self.inner.config.completion_enabled,"wireVersion":match self.inner.config.completion_wire_version { CompletionWireVersion::V1 => 1, CompletionWireVersion::V2 => 2 }}}),
        )
    }

    /// Human read projection: bounded metadata plus a bounded command summary
    /// (`command`: argv joined with spaces, at most 300 chars; null for records
    /// persisted before summaries existed). No environment values or output bodies.
    pub fn human_list(
        &self,
        api_token: &[u8],
        params: serde_json::Value,
    ) -> Result<serde_json::Value, JobError> {
        self.authenticate(api_token)?;
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Query {
            #[serde(default)]
            query: String,
            cursor: Option<String>,
            limit: Option<usize>,
        }
        let q: Query = serde_json::from_value(params)?;
        if q.query.len() > 256 || q.cursor.as_ref().is_some_and(|c| c.len() > 160) {
            return Err(JobError::Invalid("Job query is too long".into()));
        }
        let before = q
            .cursor
            .as_deref()
            .map(|c| {
                let (time, id) = c
                    .split_once(':')
                    .ok_or_else(|| JobError::Invalid("Invalid job cursor".into()))?;
                Ok::<_, JobError>((
                    time.parse::<u64>()
                        .map_err(|_| JobError::Invalid("Invalid job cursor".into()))?,
                    id,
                ))
            })
            .transpose()?;
        let now = now_secs();
        let query = q.query.to_lowercase();
        let limit = q.limit.unwrap_or(50).clamp(1, 50);
        let state = self.inner.state.lock().expect("state mutex poisoned");
        let mut jobs: Vec<_> = state
            .jobs
            .values()
            .filter(|j| {
                before.is_none_or(|key| (j.created_at_epoch_secs, j.job_id.as_str()) < key)
                    && [
                        j.job_id.as_str(),
                        j.request.action_id.as_str(),
                        j.request.subscriber_cutex_session_id.as_str(),
                        j.external
                            .as_ref()
                            .map(|external| external.remote_id.as_str())
                            .unwrap_or(""),
                    ]
                    .iter()
                    .any(|s| s.to_lowercase().contains(&query))
            })
            .collect();
        jobs.sort_by(|a, b| {
            (b.created_at_epoch_secs, &b.job_id).cmp(&(a.created_at_epoch_secs, &a.job_id))
        });
        let more = jobs.len() > limit;
        jobs.truncate(limit);
        let cursor = if more {
            jobs.last()
                .map(|j| format!("{}:{}", j.created_at_epoch_secs, j.job_id))
        } else {
            None
        };
        let data: Vec<_> = jobs.into_iter().map(|j| serde_json::json!({
            "jobId":j.job_id, "actionId":j.request.action_id.chars().take(512).collect::<String>(),
            "sessionId":j.request.subscriber_cutex_session_id, "state":j.state,
            "command":j.request.command_summary.as_deref().map(|c| c.chars().take(crate::model::COMMAND_SUMMARY_MAX_CHARS).collect::<String>()),
            "createdAt":j.created_at_epoch_secs, "updatedAt":j.updated_at_epoch_secs,
            "exitCode":j.exit_code, "cwd":j.request.cwd.chars().take(1024).collect::<String>(),
            "stdout":j.stdout, "stderr":j.stderr, "execution":j.execution,
            "delivery":j.completion_delivery.state, "outputReference":j.output_reference,
            "source":j.external.as_ref().map(|e| e.source.as_str()).unwrap_or("local"),
            "remoteId":j.external.as_ref().map(|e| &e.remote_id),
            "remoteName":j.external.as_ref().and_then(|e| e.remote_name.as_ref()),
            "lastSourceEventAt":j.external.as_ref().and_then(|e| e.last_source_event_at),
            "stale":self.external_stale(j, now),
            "revision":j.revision
        })).collect();
        Ok(serde_json::json!({"schema":"cutex/job-list/v1", "data":data, "nextCursor":cursor}))
    }

    pub fn query(&self, api_token: &[u8], job_id: &str) -> Result<JobRecord, JobError> {
        self.authenticate(api_token)?;
        self.query_inner(job_id)
    }

    pub fn query_for(
        &self,
        api_token: &[u8],
        grant: &CallerGrant,
        job_id: &str,
    ) -> Result<JobRecord, JobError> {
        self.authenticate(api_token)?;
        let job = self.query_inner(job_id)?;
        self.authorize_job_caller(grant, CallerOperation::Query, &job)?;
        Ok(agent_job_projection(job))
    }

    fn query_inner(&self, job_id: &str) -> Result<JobRecord, JobError> {
        let mut job = self
            .inner
            .state
            .lock()
            .expect("state mutex poisoned")
            .jobs
            .get(job_id)
            .cloned()
            .ok_or_else(|| JobError::NotFound(job_id.into()))?;
        let stale = self.external_stale(&job, now_secs());
        if let Some(external) = &mut job.external {
            external.stale = stale;
        }
        Ok(job)
    }

    pub fn cancel(
        &self,
        api_token: &[u8],
        job_id: &str,
        expected_revision: u64,
    ) -> Result<JobRecord, JobError> {
        self.authenticate(api_token)?;
        let current = self.query_inner(job_id)?;
        if current.external.is_some() {
            return Err(JobError::Invalid(
                "external jobs cannot be cancelled locally; use human mark-external".into(),
            ));
        }
        if current.revision != expected_revision {
            return Err(JobError::Conflict("job revision mismatch".into()));
        }
        if current.state.terminal() {
            return Ok(current);
        }
        let live = self.inner.live.lock().expect("live mutex poisoned");
        let process = live.get(job_id).ok_or_else(|| {
            JobError::Conflict("running record has no locally owned process".into())
        })?;
        process::cancel(process, self.inner.config.cancel_grace)?;
        // submit and terminal watchers acquire state before live. Never acquire
        // state while this guard is held, including in the return expression.
        drop(live);
        self.query_inner(job_id)
    }

    pub fn cancel_for(
        &self,
        api_token: &[u8],
        grant: &CallerGrant,
        job_id: &str,
        expected_revision: u64,
    ) -> Result<JobRecord, JobError> {
        self.authenticate(api_token)?;
        let job = self.query_inner(job_id)?;
        self.authorize_job_caller(grant, CallerOperation::Cancel, &job)?;
        self.cancel(api_token, job_id, expected_revision)
    }

    pub fn read_output(
        &self,
        api_token: &[u8],
        job_id: &str,
        stream: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<OutputPage, JobError> {
        self.authenticate(api_token)?;
        let job = self.query_inner(job_id)?;
        if job.external.is_some() {
            return Err(JobError::Invalid(
                "external output is not stored locally".into(),
            ));
        }
        if stream != "stdout" && stream != "stderr" {
            return Err(JobError::Invalid("stream must be stdout or stderr".into()));
        }
        let max = max_bytes.min(self.inner.config.max_read_bytes);
        let path = self.inner.store.output_path(job_id, stream);
        let (bytes, from, next, omitted) = read_output_ranges(&path, offset, max)?;
        let summary = if stream == "stdout" {
            &job.stdout
        } else {
            &job.stderr
        };
        Ok(OutputPage {
            job_id: job_id.into(),
            stream: stream.into(),
            from_offset: from,
            next_offset: next,
            bytes_hex: hex::encode(&bytes),
            gap: omitted > 0,
            omitted_bytes: omitted,
            eof: bytes.is_empty()
                && !matches!(job.state, JobState::Running | JobState::LaunchPending),
            truncated: summary.truncated,
        })
    }

    pub fn read_output_for(
        &self,
        api_token: &[u8],
        grant: &CallerGrant,
        job_id: &str,
        stream: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<OutputPage, JobError> {
        self.authenticate(api_token)?;
        let job = self.query_inner(job_id)?;
        self.authorize_job_caller(grant, CallerOperation::ReadOutput, &job)?;
        self.read_output(api_token, job_id, stream, offset, max_bytes)
    }

    pub fn pending_outbox(&self, api_token: &[u8]) -> Result<Vec<OutboxRecord>, JobError> {
        self.authenticate(api_token)?;
        Ok(self
            .inner
            .state
            .lock()
            .expect("state mutex poisoned")
            .outbox
            .values()
            .filter(|item| !item.acknowledged)
            .cloned()
            .collect())
    }

    pub fn acknowledge_outbox(
        &self,
        api_token: &[u8],
        event_id: &str,
        result_sha256: &str,
    ) -> Result<(), JobError> {
        self.authenticate(api_token)?;
        let mut state = self.inner.state.lock().expect("state mutex poisoned");
        let item = state
            .outbox
            .get_mut(event_id)
            .ok_or_else(|| JobError::NotFound(event_id.into()))?;
        if item.result_sha256 != result_sha256 {
            return Err(JobError::Conflict("outbox digest mismatch".into()));
        }
        if !matches!(
            item.delivery_state,
            CompletionDeliveryState::Delivered | CompletionDeliveryState::Orphaned
        ) {
            return Err(JobError::Conflict(
                "outbox is not in a terminal delivery state".into(),
            ));
        }
        item.acknowledged = true;
        self.inner.store.save(&mut state)
    }

    pub(crate) fn claim_due_completion(
        &self,
        now_millis: u64,
    ) -> Result<Option<CompletionAttempt>, JobError> {
        let mut state = self.inner.state.lock().expect("state mutex poisoned");
        let Some(event_id) = state
            .outbox
            .iter()
            .find(|(_, item)| {
                !item.acknowledged
                    && item.next_attempt_at_epoch_millis <= now_millis
                    && matches!(
                        item.delivery_state,
                        CompletionDeliveryState::Disabled
                            | CompletionDeliveryState::Ready
                            | CompletionDeliveryState::RetryPending
                            | CompletionDeliveryState::AcceptedPending
                            | CompletionDeliveryState::Archived
                            | CompletionDeliveryState::Unavailable
                    )
            })
            .map(|(event_id, _)| event_id.clone())
        else {
            return Ok(None);
        };
        let item = state
            .outbox
            .get_mut(&event_id)
            .expect("selected outbox exists");
        let query_only = matches!(
            item.delivery_state,
            CompletionDeliveryState::AcceptedPending | CompletionDeliveryState::Archived
        );
        item.delivery_state = CompletionDeliveryState::Sending;
        item.attempt_count = item.attempt_count.saturating_add(1);
        item.last_attempt_at_epoch_millis = Some(now_millis);
        item.last_error = None;
        let record = item.clone();
        if let Some(job) = state.jobs.get_mut(&record.job_id) {
            project_outbox(job, &record);
        }
        self.inner.store.save(&mut state)?;
        Ok(Some(CompletionAttempt { record, query_only }))
    }

    pub(crate) fn finish_completion_attempt(
        &self,
        event_id: &str,
        expected_attempt: u32,
        result: CompletionAttemptResult,
    ) -> Result<(), JobError> {
        let mut state = self.inner.state.lock().expect("state mutex poisoned");
        let item = state
            .outbox
            .get_mut(event_id)
            .ok_or_else(|| JobError::NotFound(event_id.into()))?;
        if item.attempt_count != expected_attempt
            || item.delivery_state != CompletionDeliveryState::Sending
        {
            return Err(JobError::Conflict(
                "completion delivery attempt changed before commit".into(),
            ));
        }
        item.delivery_state = result.delivery_state;
        item.receipt = result.receipt;
        item.last_error = result.last_error;
        item.next_attempt_at_epoch_millis = result.next_attempt_at_epoch_millis;
        item.acknowledged = result.final_for_retention;
        let projection = item.clone();
        if let Some(job) = state.jobs.get_mut(&projection.job_id) {
            project_outbox(job, &projection);
        }
        self.inner.store.save(&mut state)
    }

    fn authenticate(&self, supplied: &[u8]) -> Result<(), JobError> {
        use hmac::{Hmac, Mac};
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.inner.config.api_token).expect("validated token");
        mac.update(b"cutex-job-service-api-v1");
        let expected = mac.finalize().into_bytes();
        let mut candidate = Hmac::<Sha256>::new_from_slice(supplied)
            .map_err(|_| JobError::Unauthorized("invalid API credential".into()))?;
        candidate.update(b"cutex-job-service-api-v1");
        candidate
            .verify_slice(&expected)
            .map_err(|_| JobError::Unauthorized("invalid API credential".into()))
    }

    fn authorize_job_caller(
        &self,
        grant: &CallerGrant,
        operation: CallerOperation,
        job: &JobRecord,
    ) -> Result<(), JobError> {
        verify_caller_grant(
            &self.inner.config.grant_key,
            grant,
            operation,
            &job.job_id,
            &job.request.subscriber_cutex_session_id,
            now_secs(),
        )
    }
}

fn validate_request(request: &JobRequest) -> Result<(), JobError> {
    if request.action_id.trim().is_empty() || request.action_id.len() > 256 {
        return Err(JobError::Invalid("action ID is blank or too long".into()));
    }
    if request.argv.is_empty()
        || request.argv.len() > 1024
        || request.argv.iter().map(String::len).sum::<usize>() > 128 * 1024
        || request.argv.iter().any(|v| v.contains('\0'))
    {
        return Err(JobError::Invalid(
            "argv is empty, too large, or contains NUL".into(),
        ));
    }
    let cwd = std::path::Path::new(&request.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(JobError::Invalid(
            "cwd must be an existing absolute directory".into(),
        ));
    }
    if request.subscriber_cutex_session_id.trim().is_empty() {
        return Err(JobError::Invalid(
            "subscriber durable session is required".into(),
        ));
    }
    if request.origin.runtime_agent_id.trim().is_empty()
        || request.origin.native_thread_id.trim().is_empty()
        || !matches!(
            request.origin.permission_profile_type.as_str(),
            "managed" | "disabled"
        )
    {
        return Err(JobError::Invalid(
            "trusted execution origin is missing or unsupported".into(),
        ));
    }
    if request.environment.len() > 64
        || request.environment.iter().any(|(k, v)| {
            k.is_empty()
                || k.contains('=')
                || k.contains('\0')
                || v.contains('\0')
                || k.len() > 128
                || v.len() > 8192
        })
    {
        return Err(JobError::Invalid(
            "environment is outside explicit bounds".into(),
        ));
    }
    Ok(())
}

fn summary(observed: u64, path: &std::path::Path) -> StreamSummary {
    let retained = std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let retained = retained
        + std::fs::metadata(path.with_file_name(format!(
            "{}.tail",
            path.file_name().expect("output filename").to_string_lossy()
        )))
        .map(|m| m.len().saturating_sub(8))
        .unwrap_or(0);
    StreamSummary {
        retained_bytes: retained.min(observed),
        observed_bytes: observed,
        truncated: retained < observed,
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// Offsets always refer to the original stream, including skipped bytes.
fn read_output_ranges(
    path: &std::path::Path,
    offset: u64,
    max: usize,
) -> Result<(Vec<u8>, u64, u64, u64), JobError> {
    let head_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let tail_path = path.with_file_name(format!(
        "{}.tail",
        path.file_name().expect("output filename").to_string_lossy()
    ));
    let tail_start = if tail_path.exists() {
        let (header, _) = read_bounded(&tail_path, 0, 8)?;
        Some(u64::from_le_bytes(header.try_into().map_err(|_| {
            JobError::Invalid("invalid output tail header".into())
        })?))
    } else {
        None
    };
    if offset < head_len {
        let (bytes, from) = read_bounded(path, offset, max)?;
        let end = from + bytes.len() as u64;
        // Expose the gap on the following read; never silently advance over it.
        return Ok((bytes, from, end, 0));
    }
    if let Some(start) = tail_start {
        let from = offset.max(start);
        let (bytes, _) = read_bounded(&tail_path, 8 + from.saturating_sub(start), max)?;
        let next = from + bytes.len() as u64;
        return Ok((bytes, from, next, start.saturating_sub(offset)));
    }
    Ok((Vec::new(), offset, offset, offset.saturating_sub(head_len)))
}

#[cfg(test)]
mod output_alignment_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    #[test]
    fn retained_head_tail_has_original_offsets_and_survives_reopen() {
        let root = tempfile::tempdir().unwrap();
        for stream in ["stdout", "stderr"] {
            let path = root.path().join(format!("job.{stream}"));
            let bytes = [b"HEAD".as_slice(), &vec![b'x'; 20_000], b"FINAL ERROR"].concat();
            let observed = Arc::new(AtomicU64::new(0));
            process::drain_bounded(std::io::Cursor::new(bytes.clone()), &path, 4096, observed)
                .unwrap()
                .join()
                .unwrap()
                .unwrap();
            let stats = summary(bytes.len() as u64, &path);
            assert_eq!(stats.retained_bytes, 4096);
            assert!(stats.truncated);
            let (head, from, next, gap) = read_output_ranges(&path, 0, 8192).unwrap();
            assert_eq!((from, next, gap), (0, 2048, 0));
            assert!(head.starts_with(b"HEAD"));
            let (tail, from, next, gap) = read_output_ranges(&path, next, 8192).unwrap();
            assert_eq!(from, bytes.len() as u64 - 2048);
            assert_eq!(gap, bytes.len() as u64 - 4096);
            assert_eq!(next, bytes.len() as u64);
            assert!(tail.ends_with(b"FINAL ERROR"));
            assert!(read_output_ranges(&path, next, 8192).unwrap().0.is_empty());
        }
        // Separate stream sidecars must never collide.
        assert!(root.path().join("job.stdout.tail").exists());
        assert!(root.path().join("job.stderr.tail").exists());
    }
    #[test]
    fn short_output_and_legacy_prefix_remain_readable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("old.stdout");
        std::fs::write(&path, b"legacy").unwrap();
        let (bytes, from, next, gap) = read_output_ranges(&path, 0, 3).unwrap();
        assert_eq!((bytes, from, next, gap), (b"leg".to_vec(), 0, 3, 0));
        assert_eq!(read_output_ranges(&path, next, 100).unwrap().0, b"acy");
    }
}

fn validate_external_text(value: &str, max: usize, spaces: bool) -> Result<(), JobError> {
    if value.is_empty()
        || value.len() > max
        || value
            .chars()
            .any(|c| c.is_control() || (!spaces && c.is_whitespace()))
    {
        return Err(JobError::Invalid(
            "external field is empty, too long or contains invalid characters".into(),
        ));
    }
    Ok(())
}

fn agent_job_projection(mut job: JobRecord) -> JobRecord {
    if let Some(external) = &mut job.external {
        if let Some(name) = &external.remote_name {
            let marker = format!("cx{}", external.correlation_token);
            if let Some(prefix) = name.strip_suffix(&marker) {
                external.remote_name = (!prefix.is_empty()).then(|| prefix.to_owned());
            }
        }
        external.correlation_token.clear();
        external.human_audit.clear();
    }
    job
}
