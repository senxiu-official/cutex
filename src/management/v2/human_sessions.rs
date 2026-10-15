//! Root-only Human quick-chat creation: `POST /v2/human/sessions`.
//!
//! Creates a plain owned session (no Agent identity, no Project authority) —
//! the same kind `cutex new` and the TUI "New session" create — with the
//! quick-chat launch defaults (`new_thread_defaults.quick_chat`) plus optional
//! per-request launch overrides. Without `cwd` the chat gets a private scratch
//! directory `<cutex config dir>/chats/<id>`. Idempotent by `requestId`.

use std::net::TcpStream;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::http::server::SimpleHttpRequest;
use crate::management::server::ManagementRequestContext;

use super::human_agents::run_idempotent_create;
use super::human_agents::validation_error;
use super::human_agents::IdempotentCreate;
use super::repository::EventRepository;
use super::user_input::UserInputExecutionError;

pub const PATH: &str = "/v2/human/sessions";
/// Idempotency namespace; requestIds are scoped to this route.
pub const HUMAN_SESSION_IDEMPOTENCY_SCOPE: &str = "cutex/human/sessions";
/// Same limit as `cutex/session/metadata/update` `displayName`.
pub const HUMAN_SESSION_DISPLAY_NAME_MAX_CHARS: usize = 64;
/// Scratch directories live under `<cutex config dir>/chats`.
pub const SCRATCH_DIR_NAME: &str = "chats";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HumanCreateSessionRequest {
    pub request_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::session::metadata::deserialize_optional_session_cwd"
    )]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Sets `displayNameOverride`; the chat then no longer follows the native
    /// thread title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub start: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<crate::config::new_thread::LaunchOverrides>,
    /// Internal only (never on the wire): keep the session hidden from every
    /// client list (workflow step sessions).
    #[serde(skip)]
    pub hidden: bool,
}

impl HumanCreateSessionRequest {
    /// Shape checks that need no filesystem or store access.
    pub fn validate(&self) -> Result<(), UserInputExecutionError> {
        if self.request_id.is_empty() || self.request_id.chars().count() > 256 {
            return Err(validation_error(
                "requestId must be a non-empty string of at most 256 characters",
            ));
        }
        if let Some(profile) = &self.profile {
            if profile.trim().is_empty() || profile.chars().any(char::is_control) {
                return Err(validation_error("profile must be a non-empty string"));
            }
        }
        if let Some(cwd) = &self.cwd {
            if cwd.is_empty()
                || cwd.chars().any(char::is_control)
                || !std::path::Path::new(cwd).is_absolute()
            {
                return Err(validation_error("cwd must be an absolute path"));
            }
        }
        if let Some(name) = &self.display_name {
            if name.trim().is_empty()
                || name.trim() != name
                || name.chars().count() > HUMAN_SESSION_DISPLAY_NAME_MAX_CHARS
                || name.chars().any(char::is_control)
            {
                return Err(validation_error(
                    "displayName must be a trimmed, non-empty name of at most 64 characters",
                ));
            }
        }
        if let Some(launch) = &self.launch {
            launch.validate().map_err(|message| validation_error(&message))?;
        }
        Ok(())
    }
}

/// Create a fresh owner-private scratch directory for a cwd-less chat.
pub fn create_scratch_dir() -> anyhow::Result<std::path::PathBuf> {
    create_scratch_dir_in(&crate::config::paths::config_dir()?)
}

/// `<config_dir>/chats/<yyyymmdd>-<8 hex>`, owner-only (0700 / protected DACL).
pub fn create_scratch_dir_in(config_dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    create_private_workspace_in(config_dir, SCRATCH_DIR_NAME)
}

/// `<config_dir>/<area>/<yyyymmdd>-<8 hex>`: a fresh owner-only (0700 /
/// protected DACL) directory; `<area>` is created owner-only on first use.
/// Shared by quick chats (`chats`) and cwd-less Human agents
/// (`agents-scratch`, `human_agents::AGENT_SCRATCH_DIR_NAME`).
pub fn create_private_workspace_in(
    config_dir: &std::path::Path,
    area: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let parent = config_dir.join(area);
    create_private_dir(&parent, true)?;
    let name = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%d"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let dir = parent.join(name);
    create_private_dir(&dir, false)?;
    Ok(dir)
}

fn create_private_dir(path: &std::path::Path, allow_existing: bool) -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if allow_existing && error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(path)
        .map_err(|error| anyhow::anyhow!("securing {}: {error:?}", path.display()))?;
    Ok(())
}

pub(super) fn handle_create(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    context: ManagementRequestContext,
    repository: &EventRepository,
) -> anyhow::Result<()> {
    let request_id_hint = serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|value| value.get("requestId")?.as_str().map(str::to_string));
    let payload: HumanCreateSessionRequest = match serde_json::from_slice(&request.body) {
        Ok(payload) => payload,
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &format!("strict Human create-session request parsing failed: {error}"),
                false,
                json!({ "requestId": request_id_hint }),
            )
        }
    };
    if let Err(error) = payload.validate() {
        return super::server::write_v2_error(
            stream,
            400,
            "Bad Request",
            &error.code,
            &error.message,
            false,
            json!({ "requestId": payload.request_id }),
        );
    }
    let canonical = serde_json::to_value(&payload)?;
    let principal = crate::management::control_plane::HumanManagementPrincipal::authenticated();
    run_idempotent_create(
        stream,
        context,
        repository,
        IdempotentCreate {
            scope: HUMAN_SESSION_IDEMPOTENCY_SCOPE,
            noun: "create-session",
            request_id: &payload.request_id,
            canonical: &canonical,
        },
        || (context.create_human_session)(&principal, &payload),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> Result<HumanCreateSessionRequest, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn create_session_request_is_strict_and_validated() {
        let minimal = request(json!({ "requestId": "r1" })).expect("minimal");
        minimal.validate().expect("valid");
        assert!(minimal.cwd.is_none() && !minimal.start && minimal.launch.is_none());
        assert_eq!(serde_json::to_value(&minimal).unwrap(), json!({ "requestId": "r1", "start": false }));
        let full = request(json!({
            "requestId": "r2",
            "cwd": std::env::temp_dir(),
            "profile": "GLM",
            "displayName": "Groceries",
            "start": true,
            "launch": { "sandbox": "read-only", "approvalPolicy": "never",
                        "networkAccess": false, "model": "glm-5", "reasoningEffort": "high" }
        }))
        .expect("full");
        full.validate().expect("valid");

        assert!(request(json!({ "requestId": "r", "name": "agent" })).is_err());
        assert!(request(json!({ "cwd": "/tmp" })).is_err());
        for invalid in [
            json!({ "requestId": "" }),
            json!({ "requestId": "r", "cwd": "relative" }),
            json!({ "requestId": "r", "profile": "" }),
            json!({ "requestId": "r", "displayName": " padded" }),
            json!({ "requestId": "r", "displayName": "x".repeat(65) }),
            json!({ "requestId": "r", "launch": { "sandbox": "full" } }),
            json!({ "requestId": "r", "launch": { "writableRoots": ["relative"] } }),
        ] {
            let parsed = request(invalid.clone()).expect("shape parses");
            let error = parsed.validate().expect_err(&invalid.to_string());
            assert_eq!((error.stage.as_str(), error.code.as_str()), ("validation", "invalid_request"));
        }
    }

    #[test]
    fn scratch_dirs_are_unique_private_directories_under_chats() {
        let base = std::env::temp_dir().join(format!(
            "cutex-scratch-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir(&base).unwrap();
        let first = create_scratch_dir_in(&base).expect("scratch");
        let second = create_scratch_dir_in(&base).expect("scratch");
        assert_ne!(first, second);
        for dir in [&first, &second] {
            assert!(dir.is_dir());
            assert_eq!(dir.parent().unwrap().file_name().unwrap(), SCRATCH_DIR_NAME);
            assert!(dir.starts_with(&base));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
            }
            #[cfg(windows)]
            crate::platform::private_fs::open_validated_directory(dir).expect("owner-only DACL");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
