use crate::model::{
    COMPLETION_CONTRACT, COMPLETION_CONTRACT_V2, CompletionDeliveryState, CompletionReceipt,
    JobError, OutboxRecord,
};
use crate::service::{CompletionAttempt, CompletionAttemptResult, JobService};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_HTTP_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct CompletionDeliveryConfig {
    pub endpoint: String,
    pub token_file: PathBuf,
    pub request_timeout: Duration,
    pub minimum_backoff: Duration,
    pub maximum_backoff: Duration,
    pub idle_interval: Duration,
}

impl CompletionDeliveryConfig {
    pub fn validate(&self) -> Result<(), JobError> {
        parse_endpoint(&self.endpoint)?;
        validate_token_path(&self.token_file)?;
        if !(Duration::from_millis(100)..=Duration::from_secs(30)).contains(&self.request_timeout)
            || !(Duration::from_millis(10)..=Duration::from_secs(60))
                .contains(&self.minimum_backoff)
            || self.maximum_backoff < self.minimum_backoff
            || self.maximum_backoff > Duration::from_secs(24 * 60 * 60)
            || !(Duration::from_millis(10)..=Duration::from_secs(10)).contains(&self.idle_interval)
        {
            return Err(JobError::Invalid(
                "completion delivery timing configuration is invalid".into(),
            ));
        }
        Ok(())
    }
}

pub struct CompletionDeliveryWorker {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl CompletionDeliveryWorker {
    pub fn start(service: JobService, config: CompletionDeliveryConfig) -> Result<Self, JobError> {
        config.validate()?;
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("cutex-job-completion".into())
            .spawn(move || worker_loop(service, config, worker_stop))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let (lock, wake) = &*self.stop;
        *lock.lock().expect("completion stop mutex poisoned") = true;
        wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CompletionDeliveryWorker {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn worker_loop(
    service: JobService,
    config: CompletionDeliveryConfig,
    stop: Arc<(Mutex<bool>, Condvar)>,
) {
    loop {
        if *stop.0.lock().expect("completion stop mutex poisoned") {
            return;
        }
        match service.claim_due_completion(now_millis()) {
            Ok(Some(attempt)) => {
                let outcome = deliver(&config, &attempt);
                let (state, receipt, error, delay, final_for_retention) =
                    classify_outcome(&config, &attempt, outcome);
                let next = now_millis().saturating_add(duration_millis(delay));
                let _ = service.finish_completion_attempt(
                    &attempt.record.event_id,
                    attempt.record.attempt_count,
                    CompletionAttemptResult {
                        delivery_state: state,
                        receipt,
                        last_error: error,
                        next_attempt_at_epoch_millis: next,
                        final_for_retention,
                    },
                );
            }
            Ok(None) => {}
            Err(_) => {}
        }
        let (lock, wake) = &*stop;
        let stopped = lock.lock().expect("completion stop mutex poisoned");
        if *stopped {
            return;
        }
        let (stopped, _) = wake
            .wait_timeout(stopped, config.idle_interval)
            .expect("completion stop mutex poisoned");
        if *stopped {
            return;
        }
    }
}

#[derive(Debug)]
enum DeliveryOutcome {
    Receipt(CompletionReceipt),
    Authorization(String),
    Transient(String),
    Operator(String),
}

fn classify_outcome(
    config: &CompletionDeliveryConfig,
    attempt: &CompletionAttempt,
    outcome: DeliveryOutcome,
) -> (
    CompletionDeliveryState,
    Option<CompletionReceipt>,
    Option<String>,
    Duration,
    bool,
) {
    match outcome {
        DeliveryOutcome::Authorization(error) => (
            CompletionDeliveryState::AuthorizationFailed,
            None,
            Some(error),
            config.maximum_backoff,
            false,
        ),
        DeliveryOutcome::Operator(error) => (
            CompletionDeliveryState::OperatorRequired,
            None,
            Some(error),
            config.maximum_backoff,
            false,
        ),
        DeliveryOutcome::Transient(error) => (
            CompletionDeliveryState::RetryPending,
            None,
            Some(error),
            retry_backoff(config, attempt.record.attempt_count),
            false,
        ),
        DeliveryOutcome::Receipt(receipt) => {
            if receipt.event_id != attempt.record.event_id
                || receipt.schema != completion_contract(&attempt.record)
            {
                return (
                    CompletionDeliveryState::OperatorRequired,
                    Some(receipt),
                    Some("Cutex receipt identity/schema mismatch".into()),
                    config.maximum_backoff,
                    false,
                );
            }
            if receipt.message_id.as_deref().is_some_and(|message_id| {
                message_id != completion_message_id(&attempt.record.event_id)
            }) {
                return (
                    CompletionDeliveryState::OperatorRequired,
                    Some(receipt),
                    Some("Cutex receipt message identity mismatch".into()),
                    config.maximum_backoff,
                    false,
                );
            }
            if receipt.status == "no_write" {
                return match receipt.error_code.as_deref() {
                    Some("event_conflict") => (
                        CompletionDeliveryState::Conflict,
                        Some(receipt),
                        Some("Cutex rejected changed semantics for stable event ID".into()),
                        config.maximum_backoff,
                        false,
                    ),
                    Some("not_found") | Some("target_classification_unavailable") => (
                        CompletionDeliveryState::Unavailable,
                        Some(receipt),
                        Some("Cutex target classification is unavailable".into()),
                        retry_backoff(config, attempt.record.attempt_count),
                        false,
                    ),
                    _ => (
                        CompletionDeliveryState::OperatorRequired,
                        Some(receipt),
                        Some("Cutex returned an unsupported no-write receipt".into()),
                        config.maximum_backoff,
                        false,
                    ),
                };
            }
            if receipt.status != "committed" {
                return (
                    CompletionDeliveryState::OperatorRequired,
                    Some(receipt),
                    Some("Cutex receipt status is unsupported".into()),
                    config.maximum_backoff,
                    false,
                );
            }
            match receipt.disposition.as_str() {
                "pending" => (
                    CompletionDeliveryState::AcceptedPending,
                    Some(receipt),
                    None,
                    config.minimum_backoff,
                    false,
                ),
                "archived" => (
                    CompletionDeliveryState::Archived,
                    Some(receipt),
                    None,
                    config.maximum_backoff,
                    false,
                ),
                "delivered" if receipt.a4_receipt.is_some() => (
                    CompletionDeliveryState::Delivered,
                    Some(receipt),
                    None,
                    config.maximum_backoff,
                    true,
                ),
                "orphaned" => (
                    CompletionDeliveryState::Orphaned,
                    Some(receipt),
                    Some("target durable Agent is permanently retired".into()),
                    config.maximum_backoff,
                    true,
                ),
                "not_found" => (
                    CompletionDeliveryState::Unavailable,
                    Some(receipt),
                    Some("Cutex completion record or target is unavailable".into()),
                    retry_backoff(config, attempt.record.attempt_count),
                    false,
                ),
                "delivered" => (
                    CompletionDeliveryState::OperatorRequired,
                    Some(receipt),
                    Some("delivered receipt omitted A4 persistence evidence".into()),
                    config.maximum_backoff,
                    false,
                ),
                _ => (
                    CompletionDeliveryState::OperatorRequired,
                    Some(receipt),
                    Some("Cutex receipt disposition is unsupported".into()),
                    config.maximum_backoff,
                    false,
                ),
            }
        }
    }
}

fn deliver(config: &CompletionDeliveryConfig, attempt: &CompletionAttempt) -> DeliveryOutcome {
    let token = match read_token(&config.token_file) {
        Ok(token) => token,
        Err(error) => return DeliveryOutcome::Authorization(error.to_string()),
    };
    let request = if attempt.query_only {
        serde_json::to_vec(&CompletionQuery {
            schema: completion_contract(&attempt.record),
            event_id: &attempt.record.event_id,
        })
    } else {
        match (
            attempt.record.wire_version,
            attempt.record.frozen_request_v2.as_ref(),
        ) {
            (None, None) => serde_json::to_vec(&completion_request(&attempt.record)),
            (Some(2), Some(request)) => serde_json::to_vec(request),
            _ => {
                return DeliveryOutcome::Operator(
                    "completion wire version and frozen request are inconsistent".into(),
                );
            }
        }
    };
    let body = match request {
        Ok(body) => body,
        Err(error) => return DeliveryOutcome::Operator(error.to_string()),
    };
    let path = if attempt.query_only {
        "/api/job-service/v1/completions/query"
    } else {
        "/api/job-service/v1/completions"
    };
    match post(config, path, &token, &body) {
        Ok((200, body)) => match serde_json::from_slice(&body) {
            Ok(receipt) => DeliveryOutcome::Receipt(receipt),
            Err(error) => DeliveryOutcome::Operator(format!("invalid Cutex receipt: {error}")),
        },
        Ok((401 | 403, _)) => {
            DeliveryOutcome::Authorization("Cutex rejected the completion credential".into())
        }
        Ok((status, _)) if status >= 500 => {
            DeliveryOutcome::Transient(format!("Cutex HTTP {status}"))
        }
        Ok((status, _)) => DeliveryOutcome::Operator(format!("Cutex HTTP {status}")),
        Err(error) => DeliveryOutcome::Transient(error.to_string()),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionRequest<'a> {
    schema: &'static str,
    event_id: &'a str,
    job_id: &'a str,
    job_revision: u64,
    terminal_status: &'static str,
    result_sha256: &'a str,
    target_cutex_session_id: &'a str,
    summary: String,
    output_reference: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionQuery<'a> {
    schema: &'static str,
    event_id: &'a str,
}

fn completion_request(record: &OutboxRecord) -> CompletionRequest<'_> {
    let terminal_status = crate::store::terminal_status(record.terminal_state);
    CompletionRequest {
        schema: COMPLETION_CONTRACT,
        event_id: &record.event_id,
        job_id: &record.job_id,
        job_revision: record.job_revision,
        terminal_status,
        result_sha256: &record.result_sha256,
        target_cutex_session_id: &record.subscriber_cutex_session_id,
        summary: format!(
            "Job {} reached terminal state {terminal_status}",
            record.job_id
        ),
        output_reference: &record.output_reference,
    }
}

fn completion_contract(record: &OutboxRecord) -> &'static str {
    if record.wire_version == Some(2) {
        COMPLETION_CONTRACT_V2
    } else {
        COMPLETION_CONTRACT
    }
}

fn completion_message_id(event_id: &str) -> String {
    format!("jsc_{:x}", Sha256::digest(event_id.as_bytes()))
}

fn post(
    config: &CompletionDeliveryConfig,
    path: &str,
    token: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>), JobError> {
    let address = parse_endpoint(&config.endpoint)?;
    let mut stream = TcpStream::connect_timeout(&address, config.request_timeout)?;
    stream.set_read_timeout(Some(config.request_timeout))?;
    stream.set_write_timeout(Some(config.request_timeout))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address.port(),
        body.len()
    )?;
    stream.write_all(body)?;
    let mut response = Vec::new();
    stream
        .take((MAX_HTTP_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response)?;
    if response.len() > MAX_HTTP_RESPONSE_BYTES {
        return Err(JobError::Invalid(
            "Cutex completion response exceeds 64 KiB".into(),
        ));
    }
    let split = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or_else(|| JobError::Invalid("Cutex HTTP response is malformed".into()))?;
    let head = std::str::from_utf8(&response[..split])
        .map_err(|_| JobError::Invalid("Cutex HTTP headers are malformed".into()))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| JobError::Invalid("Cutex HTTP status is malformed".into()))?;
    Ok((status, response[split + 4..].to_vec()))
}

fn parse_endpoint(endpoint: &str) -> Result<SocketAddr, JobError> {
    let port = endpoint
        .strip_prefix("http://127.0.0.1:")
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            JobError::Invalid("completion endpoint must be exact IPv4 loopback HTTP".into())
        })?;
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

fn validate_token_path(path: &Path) -> Result<(), JobError> {
    if !path.is_absolute() {
        return Err(JobError::Invalid(
            "completion token path must be absolute".into(),
        ));
    }
    let canonical = std::fs::canonicalize(path)?;
    if canonical != path {
        return Err(JobError::Invalid(
            "completion token path must not traverse symlinks".into(),
        ));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| JobError::Invalid("completion token has no parent".into()))?;
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    if !parent_metadata.file_type().is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
        || parent_metadata.permissions().mode() & 0o022 != 0
    {
        return Err(JobError::Unauthorized(
            "completion token parent must be a non-writable-by-others directory owned by the service UID"
                .into(),
        ));
    }
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(JobError::Unauthorized(
            "completion token must be a private regular file owned by the service UID".into(),
        ));
    }
    Ok(())
}

fn read_token(path: &Path) -> Result<String, JobError> {
    validate_token_path(path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(JobError::Unauthorized(
            "completion token changed during secure open".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(JobError::Unauthorized(
            "completion credential exceeds 4 KiB".into(),
        ));
    }
    let token = std::str::from_utf8(&bytes)
        .map_err(|_| JobError::Unauthorized("completion credential is not UTF-8".into()))?
        .trim();
    if token.len() < 32 || token.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(JobError::Unauthorized(
            "completion credential is malformed".into(),
        ));
    }
    Ok(token.to_string())
}

fn retry_backoff(config: &CompletionDeliveryConfig, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(20);
    let multiplier = 1u32 << exponent;
    config
        .minimum_backoff
        .saturating_mul(multiplier)
        .min(config.maximum_backoff)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn completion_token_requires_private_owned_direct_path() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let token = root.path().join("token");
        std::fs::write(&token, b"completion-token-with-at-least-thirty-two-bytes").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_token(&token).is_ok());

        let link = root.path().join("link");
        symlink(&token, &link).unwrap();
        assert!(validate_token_path(&link).is_err());

        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&token).is_err());
    }
}
