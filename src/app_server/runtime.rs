//! Per-`cutex_session` app-server endpoint layout and persisted runtime binding.

use std::fs;
use std::io;
#[cfg(windows)]
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use uuid::Uuid;

use crate::agent_bus::identity::fnv1a_hex;
use crate::agent_bus::identity::sanitize_session_component;
use crate::config::paths::runtime_dir;
use crate::launch::command::LaunchCommand;
use crate::runtime::args::{effective_permission_alias, effective_runtime_permission_defaults};
use crate::session::model::normalize_runtime_token;
use crate::session::model::CutexAppServerRuntimeBinding;
use crate::session::model::CutexAppServerTransport;
use crate::session::model::CutexSessionRecord;
use crate::session::service::cutex_session_launch_cwd;

use super::client::AppServerEndpoint;
use super::commands::{ThreadResumeParams, ThreadStartParams};
use super::journal::APP_SERVER_EXPERIMENTAL_SCHEMA_SHA256;
use super::journal::APP_SERVER_SCHEMA_VERSION;

pub const CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR: &str = "CUTEX_APP_SERVER_AUTH_TOKEN";

const APP_SERVER_RUNTIME_DIR_NAME: &str = "app-server";
const APP_SERVER_JOURNAL_DIR_NAME: &str = "app-server-journal";
#[cfg(unix)]
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 100;

#[derive(Debug, Clone)]
pub struct AppServerRuntimeLayout {
    runtime_dir: PathBuf,
    endpoint: AppServerEndpoint,
    listen_url: String,
    auth_token_path: Option<PathBuf>,
    auth_token: Option<String>,
    diagnostic_journal_path: PathBuf,
}

impl AppServerRuntimeLayout {
    pub fn prepare(cutex_session_id: &str) -> anyhow::Result<Self> {
        Self::prepare_under(&runtime_dir()?, cutex_session_id)
    }

    /// Same private managed root, compact Unix path for explicit stock owners.
    pub fn prepare_stock(cutex_session_id: &str) -> anyhow::Result<Self> {
        Self::prepare_layout(&runtime_dir()?, cutex_session_id, true)
    }

    fn prepare_under(runtime_root: &Path, cutex_session_id: &str) -> anyhow::Result<Self> {
        Self::prepare_layout(runtime_root, cutex_session_id, false)
    }

    fn prepare_layout(
        runtime_root: &Path,
        cutex_session_id: &str,
        compact: bool,
    ) -> anyhow::Result<Self> {
        let session_key = runtime_session_key(cutex_session_id);
        let launch_key = Uuid::new_v4().simple().to_string();
        let launch_key = &launch_key[..8];
        let endpoint_key = fnv1a_hex(cutex_session_id);
        let runtime_dir = runtime_root
            .join(APP_SERVER_RUNTIME_DIR_NAME)
            .join(if compact {
                Uuid::new_v4().simple().to_string()[..12].to_string()
            } else {
                format!("{}-{launch_key}", &endpoint_key[..10])
            });
        create_private_dir(&runtime_dir)?;
        let journal_dir = runtime_root.join(APP_SERVER_JOURNAL_DIR_NAME);
        create_private_dir(&journal_dir)?;
        let diagnostic_journal_path = journal_dir.join(format!("{session_key}.jsonl"));

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let socket_path = runtime_dir.join(if compact { "s" } else { "app.sock" });
            if socket_path.as_os_str().as_bytes().len() > MAX_UNIX_SOCKET_PATH_BYTES {
                let _ = fs::remove_dir(&runtime_dir);
                anyhow::bail!(
                    "app-server Unix socket path is too long: {}",
                    socket_path.display()
                );
            }
            let listen_url = format!("unix://{}", socket_path.display());
            return Ok(Self {
                runtime_dir,
                endpoint: AppServerEndpoint::UnixSocket { socket_path },
                listen_url,
                auth_token_path: None,
                auth_token: None,
                diagnostic_journal_path,
            });
        }

        #[cfg(windows)]
        {
            let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .context("failed to reserve loopback app-server port")?;
            let port = listener.local_addr()?.port();
            drop(listener);
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let token_path = runtime_dir.join("capability-token");
            write_private_file(&token_path, token.as_bytes())?;
            let listen_url = loopback_websocket_listen_url(port);
            return Ok(Self {
                runtime_dir,
                endpoint: AppServerEndpoint::LoopbackWebSocket {
                    url: listen_url.clone(),
                    bearer_token: Some(token.clone()),
                },
                listen_url,
                auth_token_path: Some(token_path),
                auth_token: Some(token),
                diagnostic_journal_path,
            });
        }

        #[allow(unreachable_code)]
        Err(anyhow::anyhow!(
            "app-server runtime endpoints are unsupported on this platform"
        ))
    }

    pub fn from_binding(binding: &CutexAppServerRuntimeBinding) -> anyhow::Result<Self> {
        Self::from_binding_under(binding, &runtime_dir()?)
    }

    fn from_binding_under(
        binding: &CutexAppServerRuntimeBinding,
        runtime_root: &Path,
    ) -> anyhow::Result<Self> {
        let endpoint = endpoint_from_runtime_binding_under(binding, runtime_root)?;
        let auth_token = match &endpoint {
            #[cfg(unix)]
            AppServerEndpoint::UnixSocket { .. } => None,
            AppServerEndpoint::LoopbackWebSocket { bearer_token, .. } => bearer_token.clone(),
        };
        Ok(Self {
            runtime_dir: PathBuf::from(&binding.runtime_dir),
            endpoint,
            listen_url: binding.endpoint.clone(),
            auth_token_path: binding.auth_token_path.as_deref().map(PathBuf::from),
            auth_token,
            diagnostic_journal_path: PathBuf::from(&binding.diagnostic_journal_path),
        })
    }

    pub fn endpoint(&self) -> AppServerEndpoint {
        self.endpoint.clone()
    }

    pub fn diagnostic_journal_path(&self) -> &Path {
        &self.diagnostic_journal_path
    }

    pub fn endpoint_ready(&self) -> bool {
        match &self.endpoint {
            #[cfg(unix)]
            AppServerEndpoint::UnixSocket { socket_path } => socket_path
                .metadata()
                .map(|metadata| {
                    use std::os::unix::fs::FileTypeExt;

                    metadata.file_type().is_socket()
                })
                .unwrap_or(false),
            AppServerEndpoint::LoopbackWebSocket { url, .. } => loopback_websocket_addr(url)
                .is_some_and(|address| {
                    std::net::TcpStream::connect_timeout(
                        &address,
                        std::time::Duration::from_millis(100),
                    )
                    .is_ok()
                }),
        }
    }

    pub fn app_server_args(&self) -> Vec<String> {
        let mut args = vec![
            "app-server".to_string(),
            "--listen".to_string(),
            self.listen_url.clone(),
        ];
        if let Some(token_path) = self.auth_token_path.as_ref() {
            args.extend([
                "--ws-auth".to_string(),
                "capability-token".to_string(),
                "--ws-token-file".to_string(),
                token_path.display().to_string(),
            ]);
        }
        args
    }

    pub fn remote_tui_args(&self) -> Vec<String> {
        let mut args = vec!["--remote".to_string(), self.listen_url.clone()];
        if self.auth_token.is_some() {
            args.extend([
                "--remote-auth-token-env".to_string(),
                CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR.to_string(),
            ]);
        }
        args
    }

    pub fn apply_remote_tui_auth(&self, launch: LaunchCommand) -> LaunchCommand {
        let launch = launch.env_remove(CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR);
        match self.auth_token.as_ref() {
            Some(token) => launch.env(CUTEX_APP_SERVER_AUTH_TOKEN_ENV_VAR, token.clone()),
            None => launch,
        }
    }

    pub fn binding(&self, pid: u32, started_at: String) -> CutexAppServerRuntimeBinding {
        CutexAppServerRuntimeBinding {
            transport: match &self.endpoint {
                #[cfg(unix)]
                AppServerEndpoint::UnixSocket { .. } => CutexAppServerTransport::UnixSocket,
                AppServerEndpoint::LoopbackWebSocket { .. } => {
                    CutexAppServerTransport::LoopbackWebSocket
                }
            },
            endpoint: self.listen_url.clone(),
            pid,
            runtime_dir: self.runtime_dir.display().to_string(),
            launched_profile: None,
            launched_profile_id: None,
            launch_profile_source: None,
            auth_token_path: self
                .auth_token_path
                .as_ref()
                .map(|path| path.display().to_string()),
            diagnostic_journal_path: self.diagnostic_journal_path.display().to_string(),
            schema_version: APP_SERVER_SCHEMA_VERSION.to_string(),
            schema_sha256: APP_SERVER_EXPERIMENTAL_SCHEMA_SHA256.to_string(),
            started_at,
        }
    }

    pub fn cleanup_files(&self) -> anyhow::Result<()> {
        cleanup_runtime_binding_files(&self.binding(0, String::new()))
    }
}

pub fn thread_resume_params_for_session(
    record: &CutexSessionRecord,
) -> anyhow::Result<ThreadResumeParams> {
    thread_resume_params_for_session_with_model_provider(record, None)
}

pub fn thread_resume_params_for_session_with_model_provider(
    record: &CutexSessionRecord,
    model_provider: Option<&str>,
) -> anyhow::Result<ThreadResumeParams> {
    let thread_id = record
        .codex_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("cutex session has no Codex session id")?;
    let cwd = PathBuf::from(cutex_session_launch_cwd(record));
    if !cwd.is_absolute() {
        anyhow::bail!("app-server thread cwd must be absolute: {}", cwd.display());
    }
    let permissions = native_permission_profile_id(effective_permission_alias(record).as_deref());
    let (mut sandbox_mode, approval_policy) = effective_runtime_permission_defaults(record);
    if permissions.is_some() {
        sandbox_mode = None;
    }
    let mut config = std::collections::BTreeMap::new();
    if let Some(reasoning) = nonempty(record.reasoning_defaults.as_deref()) {
        config.insert(
            "model_reasoning_effort".to_string(),
            serde_json::Value::String(reasoning.to_string()),
        );
    }
    let permissions = apply_workspace_extras(record, &mut sandbox_mode, permissions, &mut config);
    Ok(ThreadResumeParams {
        thread_id: thread_id.to_string(),
        model: nonempty(record.model_defaults.as_deref()).map(str::to_string),
        model_provider: nonempty(model_provider).map(str::to_string),
        service_tier: session_service_tier(record),
        cwd: Some(cwd),
        approval_policy: approval_policy.map(serde_json::Value::String),
        approvals_reviewer: nonempty(record.approvals_reviewer.as_deref()).map(str::to_string),
        sandbox: sandbox_mode,
        permissions,
        config: (!config.is_empty()).then_some(config),
        ..Default::default()
    })
}

/// `session-fast-mode`: the session's own service tier, sent on thread start /
/// resume (the native restores model and effort on resume, not the tier).
/// Omitted when unset so the profile config still decides.
pub fn session_service_tier(record: &CutexSessionRecord) -> Option<Option<String>> {
    nonempty(record.service_tier.as_deref()).map(|tier| Some(tier.to_string()))
}

pub fn thread_start_params_for_session(
    record: &CutexSessionRecord,
    developer_instructions: Option<String>,
) -> anyhow::Result<ThreadStartParams> {
    if record.codex_session_id.is_some() {
        anyhow::bail!("new-thread session already has a Codex session id");
    }
    let cwd = PathBuf::from(cutex_session_launch_cwd(record));
    if !cwd.is_absolute() {
        anyhow::bail!("app-server thread cwd must be absolute: {}", cwd.display());
    }
    let permissions = native_permission_profile_id(effective_permission_alias(record).as_deref());
    let (mut sandbox_mode, approval_policy) = effective_runtime_permission_defaults(record);
    if permissions.is_some() {
        sandbox_mode = None;
    }
    let mut config = std::collections::BTreeMap::new();
    if let Some(reasoning) = nonempty(record.reasoning_defaults.as_deref()) {
        config.insert(
            "model_reasoning_effort".to_string(),
            serde_json::Value::String(reasoning.to_string()),
        );
    }
    let permissions = apply_workspace_extras(record, &mut sandbox_mode, permissions, &mut config);
    Ok(ThreadStartParams {
        model: nonempty(record.model_defaults.as_deref()).map(str::to_string),
        service_tier: session_service_tier(record),
        cwd: Some(cwd),
        approval_policy: approval_policy.map(serde_json::Value::String),
        approvals_reviewer: nonempty(record.approvals_reviewer.as_deref()).map(str::to_string),
        sandbox: sandbox_mode,
        permissions,
        config: (!config.is_empty()).then_some(config),
        developer_instructions,
        ephemeral: Some(false),
        session_start_source: Some("startup".to_string()),
        ..Default::default()
    })
}

/// workspace-write extras (network, writable roots) through the legacy
/// `[sandbox_workspace_write]` table: an explicit `sandbox` override selects
/// legacy permission syntax, whereas a built-in `permissions` profile would
/// ignore the table.
fn apply_workspace_extras(
    record: &CutexSessionRecord,
    sandbox_mode: &mut Option<String>,
    permissions: Option<String>,
    config: &mut std::collections::BTreeMap<String, serde_json::Value>,
) -> Option<String> {
    let (effective_sandbox, _) = effective_runtime_permission_defaults(record);
    let network = record.sandbox_network_access.unwrap_or(false);
    if effective_sandbox.as_deref() != Some("workspace-write")
        || (!network && record.sandbox_writable_roots.is_empty())
    {
        return permissions;
    }
    *sandbox_mode = Some("workspace-write".to_string());
    config.insert(
        "sandbox_workspace_write".to_string(),
        serde_json::json!({
            "network_access": network,
            "writable_roots": record.sandbox_writable_roots,
        }),
    );
    None
}

pub fn native_permission_profile_id(permission: Option<&str>) -> Option<String> {
    let permission = nonempty(permission)?;
    let profile = match normalize_runtime_token(permission).as_str() {
        "full-access" | "danger-full-access" | "danger" => ":danger-full-access",
        "workspace" | "workspace-write" | "ask-for-approval" => ":workspace",
        "read-only" | "readonly" | "read" => ":read-only",
        _ => permission,
    };
    Some(profile.to_string())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn loopback_websocket_addr(url: &str) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;

    let url = url::Url::parse(url).ok()?;
    let host = url.host_str()?;
    if !matches!(host, "127.0.0.1" | "::1" | "localhost") {
        return None;
    }
    let port = url.port_or_known_default()?;
    (host, port)
        .to_socket_addrs()
        .ok()?
        .find(|address| address.ip().is_loopback())
}

#[cfg(any(windows, test))]
fn loopback_websocket_listen_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}")
}

pub fn endpoint_from_runtime_binding(
    binding: &CutexAppServerRuntimeBinding,
) -> anyhow::Result<AppServerEndpoint> {
    endpoint_from_runtime_binding_under(binding, &runtime_dir()?)
}

fn endpoint_from_runtime_binding_under(
    binding: &CutexAppServerRuntimeBinding,
    runtime_root: &Path,
) -> anyhow::Result<AppServerEndpoint> {
    validate_runtime_binding_paths_under(binding, runtime_root)?;
    match binding.transport {
        CutexAppServerTransport::UnixSocket => {
            #[cfg(unix)]
            {
                let socket_path = binding
                    .endpoint
                    .strip_prefix("unix://")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .context("app-server Unix binding has an invalid endpoint")?;
                Ok(AppServerEndpoint::UnixSocket { socket_path })
            }
            #[cfg(not(unix))]
            anyhow::bail!("Unix app-server binding cannot be used on this platform");
        }
        CutexAppServerTransport::LoopbackWebSocket => {
            let token_path = binding
                .auth_token_path
                .as_deref()
                .context("loopback app-server binding omitted auth_token_path")?;
            let token = fs::read_to_string(token_path)
                .with_context(|| format!("failed to read app-server token file {token_path}"))?;
            let token = token.trim().to_string();
            if token.is_empty() {
                anyhow::bail!("app-server token file is empty: {token_path}");
            }
            Ok(AppServerEndpoint::LoopbackWebSocket {
                url: binding.endpoint.clone(),
                bearer_token: Some(token),
            })
        }
    }
}

pub fn cleanup_runtime_binding_files(binding: &CutexAppServerRuntimeBinding) -> anyhow::Result<()> {
    cleanup_runtime_binding_files_under(binding, &runtime_dir()?)
}

fn cleanup_runtime_binding_files_under(
    binding: &CutexAppServerRuntimeBinding,
    runtime_root: &Path,
) -> anyhow::Result<()> {
    validate_runtime_binding_paths_under(binding, runtime_root)?;
    let runtime_path = PathBuf::from(&binding.runtime_dir);
    if let Some(token_path) = binding.auth_token_path.as_deref() {
        remove_file_if_present(Path::new(token_path))?;
    }
    if let Some(socket_path) = binding.endpoint.strip_prefix("unix://") {
        remove_file_if_present(Path::new(socket_path))?;
    }
    match fs::remove_dir(&runtime_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove app-server runtime directory {}",
                runtime_path.display()
            )
        }),
    }
}

fn validate_runtime_binding_paths_under(
    binding: &CutexAppServerRuntimeBinding,
    runtime_root: &Path,
) -> anyhow::Result<()> {
    let runtime_path = PathBuf::from(&binding.runtime_dir);
    let expected_root = runtime_root.join(APP_SERVER_RUNTIME_DIR_NAME);
    if !runtime_path.starts_with(&expected_root) || runtime_path == expected_root {
        anyhow::bail!(
            "app-server runtime directory is outside the managed root: {}",
            runtime_path.display()
        );
    }
    if let Some(token_path) = binding.auth_token_path.as_deref() {
        ensure_path_is_within(Path::new(token_path), &runtime_path, "token")?;
    }
    if let Some(socket_path) = binding.endpoint.strip_prefix("unix://") {
        ensure_path_is_within(Path::new(socket_path), &runtime_path, "socket")?;
    }
    Ok(())
}

fn ensure_path_is_within(path: &Path, parent: &Path, label: &str) -> anyhow::Result<()> {
    if !path.starts_with(parent) || path == parent {
        anyhow::bail!(
            "app-server {label} path is outside its runtime directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn runtime_session_key(cutex_session_id: &str) -> String {
    let label = sanitize_session_component(cutex_session_id, 20, "session");
    let hash = fnv1a_hex(cutex_session_id);
    format!("{label}-{}", &hash[..10])
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create app-server directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure app-server directory {}", path.display()))?;
    }
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(path)
        .with_context(|| format!("failed to secure app-server directory {}", path.display()))?;
    Ok(())
}

#[cfg(windows)]
fn write_private_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("app-server token parent missing")?;
    let name = path.file_name().and_then(|name| name.to_str())
        .context("invalid app-server token name")?;
    let (_directory, identity) = crate::platform::private_fs::open_validated_directory(parent)?;
    let mut file = crate::platform::private_fs::open_child(
        parent, identity, name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, false,
    ).with_context(|| format!("failed to create private app-server token file {}", path.display()))?;
    file.write_all(contents)?;
    file.flush()?;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove app-server file {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum CompatibleThreadStartSource {
        Startup,
        Clear,
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_token_has_private_acl_and_is_not_overwritten() {
        let root = std::env::temp_dir().join(format!("cutex-runtime-acl-{}", Uuid::new_v4()));
        let layout = AppServerRuntimeLayout::prepare_under(&root, "test-session").unwrap();
        crate::platform::private_fs::open_validated_directory(&layout.runtime_dir).unwrap();
        let path = layout.auth_token_path.as_ref().unwrap();
        let file = std::fs::File::open(path).unwrap();
        crate::platform::private_fs::validate_private_file(&file).unwrap();
        drop(file);
        let original = std::fs::read(path).unwrap();
        assert!(write_private_file(path, b"replacement").is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert_eq!(layout.auth_token.as_ref().unwrap().as_bytes(), original);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_layout_is_private_reconnectable_and_cleanup_safe() {
        use std::os::unix::fs::PermissionsExt;

        let root = short_test_root("cas-layout");
        let layout = AppServerRuntimeLayout::prepare_under(&root, "cutex/session A")
            .expect("prepare runtime layout");
        assert_eq!(
            fs::metadata(&layout.runtime_dir)
                .expect("runtime metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(layout.app_server_args()[0], "app-server");
        assert_eq!(layout.app_server_args()[1], "--listen");
        assert!(layout.app_server_args()[2].starts_with("unix://"));
        assert_eq!(layout.remote_tui_args()[0], "--remote");
        assert_eq!(layout.remote_tui_args().len(), 2);

        let binding = layout.binding(123, "2026-07-10T00:00:00Z".to_string());
        assert_eq!(binding.transport, CutexAppServerTransport::UnixSocket);
        assert!(binding.auth_token_path.is_none());
        assert_eq!(binding.schema_sha256, APP_SERVER_EXPERIMENTAL_SCHEMA_SHA256);
        let endpoint = endpoint_from_runtime_binding_under(&binding, &root)
            .expect("binding should reconstruct endpoint");
        assert_eq!(endpoint, layout.endpoint());
        let reconstructed = AppServerRuntimeLayout::from_binding_under(&binding, &root)
            .expect("binding should reconstruct layout");
        assert!(reconstructed.endpoint_ready() == layout.endpoint_ready());
        assert_eq!(reconstructed.remote_tui_args(), layout.remote_tui_args());

        cleanup_runtime_binding_files_under(&binding, &root).expect("cleanup runtime layout");
        assert!(!layout.runtime_dir.exists());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_rejects_paths_outside_managed_runtime_root() {
        let root = short_test_root("cas-reject");
        let layout = AppServerRuntimeLayout::prepare_under(&root, "session")
            .expect("prepare runtime layout");
        let mut binding = layout.binding(123, "2026-07-10T00:00:00Z".to_string());
        binding.runtime_dir = root.join("outside").display().to_string();

        assert!(validate_runtime_binding_paths_under(&binding, &root).is_err());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn workspace_extras_use_legacy_table_instead_of_builtin_profile() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "host".to_string(),
            std::env::temp_dir().display().to_string(),
            Some("profile".to_string()),
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.permission_defaults = Some("workspace-write".to_string());
        record.approval_policy = Some("on-request".to_string());
        let plain = thread_resume_params_for_session(&record).unwrap();
        assert_eq!(plain.permissions.as_deref(), Some(":workspace"));
        assert!(plain.config.is_none());
        record.sandbox_network_access = Some(true);
        record.sandbox_writable_roots = vec!["/extra".to_string()];
        let params = thread_resume_params_for_session(&record).unwrap();
        assert_eq!(params.permissions, None);
        assert_eq!(params.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(
            params.config.as_ref().unwrap()["sandbox_workspace_write"],
            serde_json::json!({"network_access": true, "writable_roots": ["/extra"]})
        );
        record.permission_defaults = Some("danger-full-access".to_string());
        let danger = thread_resume_params_for_session(&record).unwrap();
        assert_eq!(danger.permissions.as_deref(), Some(":danger-full-access"));
        assert!(danger.config.is_none());
    }

    #[test]
    fn stale_alias_follows_explicit_sandbox() {
        // A running-thread sandbox change once left the alias stale; the
        // explicit sandbox is the owner's latest choice.
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "host".to_string(),
            std::env::temp_dir().display().to_string(),
            Some("profile".to_string()),
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.permission_defaults = Some("workspace-write".to_string());
        record.sandbox_mode = Some("danger-full-access".to_string());
        record.approval_policy = Some("never".to_string());
        let params = thread_resume_params_for_session(&record).unwrap();
        assert_eq!(params.permissions.as_deref(), Some(":danger-full-access"));
        assert_eq!(params.sandbox, None);
        let start = {
            record.codex_session_id = None;
            thread_start_params_for_session(&record, None).unwrap()
        };
        assert_eq!(start.permissions.as_deref(), Some(":danger-full-access"));
    }

    #[test]
    fn session_resume_params_restore_runtime_defaults() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "host".to_string(),
            std::env::temp_dir().display().to_string(),
            Some("profile".to_string()),
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.permission_defaults = Some("workspace-write".to_string());
        record.approval_policy = Some("on-request".to_string());
        record.model_defaults = Some("gpt-test".to_string());
        record.reasoning_defaults = Some("high".to_string());

        let params = thread_resume_params_for_session(&record).expect("resume params");
        assert_eq!(params.model.as_deref(), Some("gpt-test"));
        assert_eq!(params.model_provider, None);
        assert_eq!(params.sandbox, None);
        assert_eq!(params.permissions.as_deref(), Some(":workspace"));
        assert_eq!(
            params.approval_policy,
            Some(serde_json::Value::String("on-request".to_string()))
        );
        assert_eq!(
            params
                .config
                .as_ref()
                .and_then(|config| config.get("model_reasoning_effort")),
            Some(&serde_json::Value::String("high".to_string()))
        );
    }

    #[test]
    fn session_service_tier_is_sent_on_resume_and_start_only_when_set() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "host".to_string(),
            std::env::temp_dir().display().to_string(),
            None,
            "2026-10-15T00:00:00Z".to_string(),
        )
        .expect("session record");
        let resume = thread_resume_params_for_session(&record).expect("resume params");
        assert_eq!(resume.service_tier, None);
        assert!(serde_json::to_value(&resume).unwrap().get("serviceTier").is_none());

        record.service_tier = Some("priority".to_string());
        let resume = thread_resume_params_for_session(&record).expect("resume params");
        assert_eq!(resume.service_tier, Some(Some("priority".to_string())));
        assert_eq!(serde_json::to_value(&resume).unwrap()["serviceTier"], "priority");

        record.codex_session_id = None;
        record.service_tier = Some("default".to_string());
        let start = thread_start_params_for_session(&record, None).expect("start params");
        assert_eq!(start.service_tier, Some(Some("default".to_string())));
    }

    #[test]
    fn approvals_reviewer_is_sent_on_resume_and_start_only_when_set() {
        let mut record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "host".to_string(),
            std::env::temp_dir().display().to_string(),
            None,
            "2026-10-16T00:00:00Z".to_string(),
        )
        .expect("session record");
        record.permission_defaults = Some("workspace-write".to_string());
        record.approval_policy = Some("on-request".to_string());
        let resume = thread_resume_params_for_session(&record).expect("resume params");
        assert!(serde_json::to_value(&resume).unwrap().get("approvalsReviewer").is_none());

        record.approvals_reviewer = Some("auto_review".to_string());
        let resume = thread_resume_params_for_session(&record).expect("resume params");
        assert_eq!(serde_json::to_value(&resume).unwrap()["approvalsReviewer"], "auto_review");
        record.codex_session_id = None;
        let start = thread_start_params_for_session(&record, None).expect("start params");
        assert_eq!(serde_json::to_value(&start).unwrap()["approvalsReviewer"], "auto_review");
    }

    #[test]
    fn session_resume_params_override_thread_provider_with_launch_profile_provider() {
        let record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            Some("018f0000-0000-7000-8000-000000000001".to_string()),
            "host".to_string(),
            std::env::temp_dir().display().to_string(),
            None,
            "2026-09-02T00:00:00Z".to_string(),
        )
        .expect("session record");

        let params = thread_resume_params_for_session_with_model_provider(&record, Some("openai"))
            .expect("resume params");

        assert_eq!(params.model_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn new_thread_start_serializes_protocol_valid_startup_source() {
        let record = CutexSessionRecord::new_at(
            "cutex-new-thread".to_string(),
            None,
            "host".to_string(),
            std::env::temp_dir().display().to_string(),
            Some("release".to_string()),
            "2026-08-26T00:00:00Z".to_string(),
        )
        .expect("session record");

        let params = thread_start_params_for_session(&record, Some("verified role".to_string()))
            .expect("new thread params");
        assert_eq!(params.session_start_source.as_deref(), Some("startup"));

        let serialized = serde_json::to_value(&params).expect("serialize thread start params");
        assert_eq!(serialized["sessionStartSource"], "startup");
        let compatible: CompatibleThreadStartSource =
            serde_json::from_value(serialized["sessionStartSource"].clone())
                .expect("installed protocol accepts startup");
        assert_eq!(compatible, CompatibleThreadStartSource::Startup);
        assert_eq!(
            serde_json::from_str::<CompatibleThreadStartSource>("\"clear\"")
                .expect("installed protocol accepts clear"),
            CompatibleThreadStartSource::Clear
        );
    }

    #[test]
    fn loopback_websocket_listen_url_has_no_path() {
        let url = loopback_websocket_listen_url(12345);

        assert_eq!(url, "ws://127.0.0.1:12345");
        assert!(!url.ends_with('/'));
        assert_eq!(loopback_websocket_addr(&url).unwrap().port(), 12345);
    }

    fn short_test_root(label: &str) -> PathBuf {
        let key = Uuid::new_v4().simple().to_string();
        std::env::temp_dir().join(format!("{label}-{}", &key[..8]))
    }
}
