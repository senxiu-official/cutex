use crate::model::{
    CALLER_GRANT_CONTRACT, CallerGrant, CallerGrantPayload, CallerOperation, ExecutionGrant,
    ExecutionGrantPayload, GRANT_CONTRACT, JobError, JobRequest,
};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct TrustedSandboxContext {
    pub subject_cutex_session_id: String,
    pub cwd: String,
    pub sandbox_state: Value,
    pub launcher_path: String,
    pub launcher_sha256: String,
    pub operating_system_uid: u32,
}

pub struct GrantIssuer<'a> {
    secret: &'a [u8],
}

pub struct CallerGrantIssuer<'a> {
    secret: &'a [u8],
}

impl<'a> CallerGrantIssuer<'a> {
    /// The trusted adapter derives `subject` from its transport binding, never tool arguments.
    pub fn new(secret: &'a [u8]) -> Result<Self, JobError> {
        if secret.len() < 32 {
            return Err(JobError::Invalid(
                "grant key must contain at least 32 bytes".into(),
            ));
        }
        Ok(Self { secret })
    }

    pub fn issue(
        &self,
        subject: String,
        operation: CallerOperation,
        job_id: String,
        now: u64,
        ttl_secs: u64,
    ) -> Result<CallerGrant, JobError> {
        if subject.trim().is_empty() || job_id.trim().is_empty() || !(1..=300).contains(&ttl_secs) {
            return Err(JobError::Invalid(
                "caller grant subject/job must be nonempty and TTL must be 1..=300 seconds".into(),
            ));
        }
        let payload = CallerGrantPayload {
            schema: CALLER_GRANT_CONTRACT.into(),
            grant_id: format!("jcgr_{}", uuid::Uuid::new_v4().simple()),
            subject_cutex_session_id: subject,
            operating_system_uid: unsafe { libc::geteuid() },
            operation,
            job_id,
            issued_at_epoch_secs: now,
            expires_at_epoch_secs: now.saturating_add(ttl_secs),
        };
        let hmac_sha256 = sign_value(self.secret, &payload)?;
        Ok(CallerGrant {
            payload,
            hmac_sha256,
        })
    }
}

impl<'a> GrantIssuer<'a> {
    /// Only the trusted MCP adapter may hold this key. Model arguments never enter this constructor.
    pub fn new(secret: &'a [u8]) -> Result<Self, JobError> {
        if secret.len() < 32 {
            return Err(JobError::Invalid(
                "grant key must contain at least 32 bytes".into(),
            ));
        }
        Ok(Self { secret })
    }

    pub fn issue(
        &self,
        request: &JobRequest,
        context: TrustedSandboxContext,
        now: u64,
        ttl_secs: u64,
    ) -> Result<ExecutionGrant, JobError> {
        validate_trusted_context(request, &context)?;
        if !(1..=300).contains(&ttl_secs) {
            return Err(JobError::Invalid(
                "grant TTL must be 1..=300 seconds".into(),
            ));
        }
        let payload = ExecutionGrantPayload {
            schema: GRANT_CONTRACT.into(),
            grant_id: format!("jgr_{}", uuid::Uuid::new_v4().simple()),
            request_sha256: request_sha256(request)?,
            subject_cutex_session_id: context.subject_cutex_session_id,
            operating_system_uid: context.operating_system_uid,
            cwd: context.cwd,
            sandbox_state: canonical_value(context.sandbox_state),
            launcher_path: context.launcher_path,
            launcher_sha256: context.launcher_sha256,
            issued_at_epoch_secs: now,
            expires_at_epoch_secs: now.saturating_add(ttl_secs),
        };
        let hmac_sha256 = sign_payload(self.secret, &payload)?;
        Ok(ExecutionGrant {
            payload,
            hmac_sha256,
        })
    }
}

pub(crate) fn verify_grant(
    secret: &[u8],
    allowed_launchers: &std::collections::BTreeMap<String, String>,
    request: &JobRequest,
    grant: &ExecutionGrant,
    now: u64,
) -> Result<(), JobError> {
    if secret.len() < 32 {
        return Err(JobError::Unauthorized(
            "grant verifier is not configured".into(),
        ));
    }
    if grant.payload.schema != GRANT_CONTRACT {
        return Err(JobError::Unauthorized("unknown grant schema".into()));
    }
    let supplied = hex::decode(&grant.hmac_sha256)
        .map_err(|_| JobError::Unauthorized("malformed grant signature".into()))?;
    let bytes = canonical_bytes(&grant.payload)?;
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| JobError::Unauthorized("invalid verifier key".into()))?;
    mac.update(&bytes);
    mac.verify_slice(&supplied)
        .map_err(|_| JobError::Unauthorized("grant signature mismatch".into()))?;
    if now < grant.payload.issued_at_epoch_secs || now > grant.payload.expires_at_epoch_secs {
        return Err(JobError::Unauthorized(
            "grant is outside its validity window".into(),
        ));
    }
    if grant.payload.operating_system_uid != unsafe { libc::geteuid() } {
        return Err(JobError::Unauthorized(
            "grant OS identity differs from service identity".into(),
        ));
    }
    if request_sha256(request)? != grant.payload.request_sha256 {
        return Err(JobError::Unauthorized(
            "grant does not bind this request".into(),
        ));
    }
    if request.subscriber_cutex_session_id != grant.payload.subject_cutex_session_id
        || request.cwd != grant.payload.cwd
    {
        return Err(JobError::Unauthorized(
            "grant subject or cwd mismatch".into(),
        ));
    }
    validate_sandbox_state(&grant.payload.sandbox_state, &grant.payload.cwd)?;
    if sandbox_profile_type(&grant.payload.sandbox_state)?
        != request.origin.permission_profile_type.as_str()
    {
        return Err(JobError::Unauthorized(
            "recorded origin differs from granted permission profile".into(),
        ));
    }
    verify_launcher_allowed(
        allowed_launchers,
        &grant.payload.launcher_path,
        &grant.payload.launcher_sha256,
    )?;
    Ok(())
}

fn validate_trusted_context(
    request: &JobRequest,
    context: &TrustedSandboxContext,
) -> Result<(), JobError> {
    if request.subscriber_cutex_session_id != context.subject_cutex_session_id
        || request.cwd != context.cwd
    {
        return Err(JobError::Invalid(
            "trusted context does not bind request subject/cwd".into(),
        ));
    }
    if context.operating_system_uid != unsafe { libc::geteuid() } {
        return Err(JobError::Invalid(
            "trusted context OS identity is not the service identity".into(),
        ));
    }
    validate_sandbox_state(&context.sandbox_state, &context.cwd)?;
    if sandbox_profile_type(&context.sandbox_state)?
        != request.origin.permission_profile_type.as_str()
    {
        return Err(JobError::Invalid(
            "request origin differs from trusted permission profile".into(),
        ));
    }
    validate_launcher_claim(&context.launcher_path, &context.launcher_sha256)
}

fn validate_sandbox_state(value: &Value, expected_cwd: &str) -> Result<(), JobError> {
    let object = value
        .as_object()
        .ok_or_else(|| JobError::Unauthorized("sandbox state must be an object".into()))?;
    let profile = object
        .get("permissionProfile")
        .and_then(Value::as_object)
        .ok_or_else(|| JobError::Unauthorized("sandbox state omits permissionProfile".into()))?;
    match profile.get("type").and_then(Value::as_str) {
        Some("managed") | Some("disabled") => {}
        Some("external") => {
            return Err(JobError::Unauthorized(
                "external sandbox authority cannot be reproduced by Job Service".into(),
            ));
        }
        _ => {
            return Err(JobError::Unauthorized(
                "originating permission profile is missing or unsupported".into(),
            ));
        }
    }
    let raw_cwd = object
        .get("sandboxCwd")
        .and_then(Value::as_str)
        .ok_or_else(|| JobError::Unauthorized("sandbox state omits sandboxCwd".into()))?;
    let encoded = raw_cwd
        .strip_prefix("file://")
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| {
            JobError::Unauthorized("sandboxCwd is not an absolute local file URI".into())
        })?;
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|_| JobError::Unauthorized("sandboxCwd has invalid encoding".into()))?;
    let sandbox_cwd = std::path::PathBuf::from(decoded.as_ref());
    let sandbox_cwd = std::fs::canonicalize(sandbox_cwd)
        .map_err(|_| JobError::Unauthorized("sandboxCwd cannot be resolved".into()))?;
    let expected_cwd = std::fs::canonicalize(expected_cwd)
        .map_err(|_| JobError::Unauthorized("request cwd cannot be resolved".into()))?;
    if !sandbox_cwd.is_dir() || !expected_cwd.is_dir() {
        return Err(JobError::Unauthorized(
            "sandbox and request cwd must be directories".into(),
        ));
    }
    // sandboxCwd anchors the originating policy, not the job's execution cwd.
    // The runner changes directory inside that unchanged sandbox; access remains
    // subject to its original filesystem permissions.
    Ok(())
}

fn sandbox_profile_type(value: &Value) -> Result<&str, JobError> {
    value
        .pointer("/permissionProfile/type")
        .and_then(Value::as_str)
        .ok_or_else(|| JobError::Unauthorized("sandbox permission profile type is missing".into()))
}

fn validate_launcher_claim(path: &str, expected: &str) -> Result<(), JobError> {
    let path = Path::new(path);
    if !path.is_absolute() || !path.is_file() {
        return Err(JobError::Unauthorized(
            "sandbox launcher must be an existing absolute file".into(),
        ));
    }
    if expected.len() != 64 || hex::decode(expected).is_err() {
        return Err(JobError::Unauthorized(
            "sandbox launcher digest is malformed".into(),
        ));
    }
    Ok(())
}

fn verify_launcher_allowed(
    allowed: &std::collections::BTreeMap<String, String>,
    path: &str,
    expected: &str,
) -> Result<(), JobError> {
    validate_launcher_claim(path, expected)?;
    let canonical = std::fs::canonicalize(path)?;
    let key = canonical.to_string_lossy();
    if allowed.get(key.as_ref()).map(String::as_str) != Some(expected) {
        return Err(JobError::Unauthorized(
            "sandbox launcher is not in the service allowlist".into(),
        ));
    }
    Ok(())
}

pub fn file_sha256(path: &Path) -> Result<String, JobError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn remote_request_sha256(request: &crate::RemoteJobRequest) -> Result<String, JobError> {
    Ok(hex::encode(Sha256::digest(canonical_bytes(request)?)))
}

pub(crate) fn request_sha256(request: &JobRequest) -> Result<String, JobError> {
    Ok(hex::encode(Sha256::digest(canonical_bytes(request)?)))
}

pub(crate) fn semantic_request_sha256(request: &JobRequest) -> Result<String, JobError> {
    let value = serde_json::json!({
        "actionId": request.action_id,
        "argv": request.argv,
        "cwd": request.cwd,
        "environment": request.environment,
        "subscriberCutexSessionId": request.subscriber_cutex_session_id,
    });
    Ok(hex::encode(Sha256::digest(canonical_bytes(&value)?)))
}

pub(crate) fn sign_payload(
    secret: &[u8],
    payload: &ExecutionGrantPayload,
) -> Result<String, JobError> {
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| JobError::Invalid("invalid grant key".into()))?;
    mac.update(&canonical_bytes(payload)?);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub(crate) fn verify_caller_grant(
    secret: &[u8],
    grant: &CallerGrant,
    operation: CallerOperation,
    job_id: &str,
    expected_subject: &str,
    now: u64,
) -> Result<(), JobError> {
    if secret.len() < 32 || grant.payload.schema != CALLER_GRANT_CONTRACT {
        return Err(JobError::Unauthorized(
            "caller grant verifier or schema is invalid".into(),
        ));
    }
    let supplied = hex::decode(&grant.hmac_sha256)
        .map_err(|_| JobError::Unauthorized("malformed caller grant signature".into()))?;
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| JobError::Unauthorized("invalid caller grant key".into()))?;
    mac.update(&canonical_bytes(&grant.payload)?);
    mac.verify_slice(&supplied)
        .map_err(|_| JobError::Unauthorized("caller grant signature mismatch".into()))?;
    if now < grant.payload.issued_at_epoch_secs || now > grant.payload.expires_at_epoch_secs {
        return Err(JobError::Unauthorized(
            "caller grant is outside its validity window".into(),
        ));
    }
    if grant.payload.operating_system_uid != unsafe { libc::geteuid() }
        || grant.payload.operation != operation
        || grant.payload.job_id != job_id
        || grant.payload.subject_cutex_session_id != expected_subject
    {
        return Err(JobError::Unauthorized(
            "caller grant identity, operation, or job mismatch".into(),
        ));
    }
    Ok(())
}

fn sign_value<T: serde::Serialize>(secret: &[u8], payload: &T) -> Result<String, JobError> {
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| JobError::Invalid("invalid grant key".into()))?;
    mac.update(&canonical_bytes(payload)?);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub(crate) fn canonical_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, JobError> {
    Ok(serde_json::to_vec(&canonical_value(serde_json::to_value(
        value,
    )?))?)
}

fn canonical_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, canonical_value(v)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_value).collect()),
        other => other,
    }
}
