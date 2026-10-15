//! Native-only application boundary. No durable identity, roster or activation.
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use cutex::catalog::{CatalogEndpoint, OwnedStdioEndpoint, StdioAppServerOptions};
use cutex::launch::command::LaunchCommand;
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NativeLaunch {
    pub cwd: PathBuf,
    pub native_home: PathBuf,
    pub profile: Option<String>,
    pub model: Option<String>,
}

impl NativeLaunch {
    fn command(&self, operation: &[String]) -> anyhow::Result<Command> {
        anyhow::ensure!(
            self.cwd.is_absolute() && self.cwd.is_dir(),
            "native cwd must be an existing absolute directory"
        );
        anyhow::ensure!(
            self.native_home.is_absolute(),
            "native source home must be absolute"
        );
        let mut args = vec![
            "--cd".into(),
            self.cwd.to_string_lossy().into_owned(),
            "-c".into(),
            "notify=[]".into(),
        ];
        if let Some(model) = &self.model {
            anyhow::ensure!(
                !model.trim().is_empty() && !model.chars().any(char::is_control),
                "invalid model"
            );
            args.extend(["--model".into(), model.clone()]);
        }
        args.extend_from_slice(operation);
        let launch = if let Some(profile) = &self.profile {
            let resolved = super::launch::resolve_launch_profile_override(profile)?;
            anyhow::ensure!(
                matches!(
                    resolved.account.runtime,
                    cutex::profiles::model::RuntimeConfig::Host
                ) && resolved.account.cli_kind == cutex::profiles::model::CliKind::Codex,
                "native-only workflow currently supports host Codex profiles only"
            );
            super::launch_command::codex_launch_command_with_prevalidated_profile(
                &resolved.account,
                &args,
                false,
                &[],
                &resolved.files,
            )?
        } else {
            LaunchCommand::new(cutex::launch::program::codex_program()).args(args)
        };
        Ok(isolated_command(&launch, &self.cwd, &self.native_home))
    }

    /// Called only inside TerminalShell handoff. Never exits the outer process
    /// or retries a launch whose external result is unknown.
    pub(super) fn interactive(&self, native_id: Option<&str>) -> anyhow::Result<ExitStatus> {
        run_interactive(&mut self.interactive_command(native_id)?)
    }

    fn interactive_command(&self, native_id: Option<&str>) -> anyhow::Result<Command> {
        let operation = match native_id {
            Some(id) => {
                anyhow::ensure!(
                    cutex::session::identity::normalize_codex_session_id(id)
                        .ok()
                        .as_deref()
                        == Some(id),
                    "exact native ID required"
                );
                vec!["resume".into(), id.into()]
            }
            None => Vec::new(),
        };
        let mut command = self.command(&operation)?;
        // Static display values are launcher input, not native session config.
        // A bare resume otherwise renders the configured IDs as unavailable.
        let config_path = self.native_home.join("config.toml");
        let config_text = match std::fs::read_to_string(config_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let config: toml::Value = toml::from_str(&config_text)?;
        let order = config.get("tui").and_then(|t| t.get("status_line"))
            .and_then(toml::Value::as_array).map(|items| items.iter()
                .filter_map(toml::Value::as_str).map(str::to_owned).collect::<Vec<_>>());
        let profile_id = self.profile.as_deref().map(super::launch::resolve_launch_profile_override)
            .transpose()?.map(|p| p.account.id);
        let order = super::status_preferences::selected_order(order.as_ref(), profile_id.as_deref())?;
        let preferences = super::status_preferences::apply(LaunchCommand::new(""), Some(&order), profile_id.as_deref())?;
        command.args(preferences.args).envs(preferences.envs);
        if let Some(path) = cutex::launch::session_display::status_file(native_id, &order)? {
            command.arg("--status-items-file").arg(path);
        }
        command.env("CUTEX_NOTIFICATION_CONTROL", std::env::current_exe()?);
        Ok(command)
    }

    pub(super) fn endpoint(&self) -> anyhow::Result<OwnedStdioEndpoint> {
        let command = if let Some(deployment) =
            cutex::launch::local_deployment::LocalDeployment::selected()?
        {
            let bundle: cutex::launch::stock::StockBundle =
                serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)?;
            bundle.executable.validate()?;
            isolated_command(
                &LaunchCommand::new(bundle.executable.path.to_string_lossy().into_owned())
                    .args(["--listen", "stdio://"]),
                &self.cwd,
                &self.native_home,
            )
        } else {
            self.command(&["app-server".into(), "--stdio".into()])?
        };
        let options = StdioAppServerOptions::new(command.get_program(), self.native_home.clone());
        let mut endpoint = OwnedStdioEndpoint::spawn_command(options, command)?;
        let initialized = endpoint.request("initialize", json!({"clientInfo": {"name":"cutex_native_workflow", "version":env!("CARGO_PKG_VERSION")}, "capabilities":{"experimentalApi":true}}))?;
        anyhow::ensure!(
            initialized.get("codexHome").and_then(Value::as_str)
                .and_then(|home| std::path::Path::new(home).canonicalize().ok())
                == Some(self.native_home.canonicalize()?),
            "native source home mismatch"
        );
        endpoint.notify("initialized", None)?;
        Ok(endpoint)
    }

    /// Call only after the owning workflow has durably recorded its intent.
    /// A returned native ID is not success until the native provider can read
    /// its persisted metadata. Never create a second thread on this path.
    #[cfg(test)]
    fn bootstrap(&self) -> anyhow::Result<String> {
        let mut endpoint = self.endpoint()?;
        let created = endpoint.request("thread/start", json!({"cwd":self.cwd, "model":self.model,
            "ephemeral":false, "approvalPolicy":"never", "sandbox":"read-only", "sessionStartSource":"startup"}))?;
        let id = created
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("native creation unknown: missing ID; do not retry creation")
            })?
            .to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if endpoint
                .request("thread/read", json!({"threadId":id, "includeTurns":false}))
                .is_ok()
            {
                // A separate provider process must be able to resume this ID;
                // memory in the creator alone is not durable evidence.
                drop(endpoint);
                let mut fresh = self.endpoint()?;
                let resumed = fresh.request("thread/resume", json!({"threadId":id}))
                    .map_err(|error| anyhow::anyhow!("native identity {id} created but persistence unconfirmed: {error}; do not create again"))?;
                anyhow::ensure!(
                    resumed.pointer("/thread/id").and_then(Value::as_str) == Some(id.as_str()),
                    "native resume identity mismatch"
                );
                return Ok(id);
            }
            anyhow::ensure!(std::time::Instant::now() < deadline, "native identity {id} persistence unknown; inspect native history, do not create again");
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

fn run_interactive(command: &mut Command) -> anyhow::Result<ExitStatus> {
    let (status, diagnostic) = super::stock_lifecycle::foreground_status(command)
        .map_err(|error| anyhow::anyhow!("native launch result unknown; inspect Recent before creating again: {error}"))?;
    anyhow::ensure!(status.success(), "local native CLI failed ({status}){}",
        if diagnostic.is_empty() { String::new() } else { format!(": {diagnostic}") });
    Ok(status)
}

fn isolated_command(launch: &LaunchCommand, cwd: &Path, native_home: &Path) -> Command {
    let mut command = Command::new(&launch.program);
    command.env_clear().args(&launch.args).current_dir(cwd);
    // Allowlist normal OS/terminal context; no ambient credentials, Cutex
    // identity/task/management/runtime/notification state or config overrides.
    for key in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "SYSTEMROOT",
        "WINDIR",
        "TERM",
        "COLORTERM",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TMPDIR",
        "TEMP",
        "TMP",
        // Instance selection (keeps a nested `cutex` in this instance).
        "CUTEX_HOME",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in &launch.envs {
        if matches!(
            key.as_str(),
            "CODEX_AUTH_FILE"
                | "CODEX_CONFIG_FILE"
                | "CODEX_INSTALL_DIR"
                | "OPENAI_API_KEY"
                | "HTTPS_PROXY"
                | "HTTP_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "https_proxy"
                | "http_proxy"
                | "all_proxy"
                | "no_proxy"
        ) {
            command.env(key, value);
        }
    }
    command.env("CODEX_HOME", native_home);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn native_failure_keeps_exit_status_and_stderr() {
        let error = run_interactive(Command::new("sh").args(["-c", "printf 'active writer test' >&2; exit 1"]))
            .unwrap_err().to_string();
        assert!(error.contains("local native CLI failed (exit status: 1)"));
        assert!(error.contains("active writer test"));
    }

    #[test]
    fn resumed_session_materializes_static_items_without_adopting_or_guessing_profile() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("session-display-resume").unwrap();
        let native_home = home.root().join("native");
        std::fs::create_dir_all(&native_home).unwrap();
        std::fs::write(native_home.join("config.toml"), "[tui]\nstatus_line=['cutex_welcome','cutex_profile']\n").unwrap();
        let launch = NativeLaunch { cwd: home.root().into(), native_home, profile: None, model: None };
        let id = "018e0000-0000-7000-8000-0001a000bce6";
        for profile in [None, Some("test-profile")] {
            if let Some(name) = profile {
                cutex::launch::session_display::save(id, cutex::launch::session_display::SessionProfile {
                    profile_id: "profile-id".into(), profile_name: name.into(),
                }).unwrap();
            }
            let command = launch.interactive_command(Some(id)).unwrap();
            let args = command.get_args().map(|s| s.to_string_lossy().to_string()).collect::<Vec<_>>();
            let pos = args.iter().position(|s| s == "--status-items-file").unwrap();
            let value: Value = serde_json::from_slice(&std::fs::read(&args[pos+1]).unwrap()).unwrap();
            let items = value["items"].as_array().unwrap();
            assert!(items.iter().any(|i| i["text"] == "Bon voyage !"));
            assert!(items.iter().any(|i| i["text"] == profile.unwrap_or("N/A")));
            assert!(command.get_envs().any(|(k,v)| k == "CUTEX_NOTIFICATION_CONTROL" && v.is_some()));
        }
        assert!(cutex::session::store::load_cutex_session_store().unwrap().sessions.is_empty());
    }

    #[test]
    #[ignore = "explicit saved private fixture only; no bootstrap or model turn"]
    fn ui_contract_d06_real_saved_native_resume_no_cutex_adoption() {
        let home = crate::cli_app::test_home::IsolatedTestHome::new("cutex-d2-resume").unwrap();
        let native_home = PathBuf::from(
            std::env::var("CUTEX_D2_PRIVATE_SAVED_HOME").expect("owned saved fixture required"),
        );
        assert!(native_home
            .to_string_lossy()
            .starts_with("/tmp/cutex-d2-native-"));
        let id = std::env::var("CUTEX_D2_PRIVATE_SAVED_ID").unwrap();
        cutex::session::store::save_cutex_session_store(
            &cutex::session::model::CutexSessionStore::default(),
        )
        .unwrap();
        let path = cutex::session::store::cutex_sessions_path().unwrap();
        let before = std::fs::read(&path).unwrap();
        let provider = cutex::agent_management::AgentManagementStore::open_default().unwrap();
        let roster = provider.snapshot().unwrap();
        let launch = NativeLaunch {
            cwd: home.root().into(),
            native_home,
            profile: None,
            model: None,
        };
        let mut endpoint = launch.endpoint().unwrap();
        let resumed = endpoint
            .request("thread/resume", json!({"threadId":id}))
            .unwrap();
        assert_eq!(
            resumed.pointer("/thread/id").and_then(Value::as_str),
            Some(id.as_str())
        );
        drop(endpoint);
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(provider.snapshot().unwrap(), roster);
    }
    #[test]
    fn ui_contract_d2_native_environment_drops_authority_and_notifications() {
        let launch = LaunchCommand::new("native")
            .env("CUTEX_AGENT_ID", "parent")
            .env("CUTEX_MANAGEMENT_TOKEN", "secret")
            .env("CUTEX_TASK_ID", "task")
            .env("CODEX_NOTIFY_URL", "notify")
            .env("CODEX_AUTH_FILE", "/explicit/auth");
        let command = isolated_command(&launch, Path::new("/tmp"), Path::new("/tmp/native"));
        let env = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert!(!env
            .keys()
            .any(|key| key.starts_with("CUTEX_") || key.contains("NOTIFY")));
        assert_eq!(env["CODEX_AUTH_FILE"].as_deref(), Some("/explicit/auth"));
        assert_eq!(env["CODEX_HOME"].as_deref(), Some("/tmp/native"));
    }

    #[test]
    #[ignore = "explicit installed-native private protocol oracle; never sends a model turn"]
    fn ui_contract_d2_real_native_bootstrap_persists_without_cutex_identity() {
        let home =
            crate::cli_app::test_home::IsolatedTestHome::new("cutex-d2-native-rust").unwrap();
        let native_home = home.root().join("native");
        std::fs::create_dir(&native_home).unwrap();
        let durable = cutex::session::model::CutexSessionStore::default();
        cutex::session::store::save_cutex_session_store(&durable).unwrap();
        let path = cutex::session::store::cutex_sessions_path().unwrap();
        let before = std::fs::read(&path).unwrap();
        let provider = cutex::agent_management::AgentManagementStore::open_default().unwrap();
        let roster = provider.snapshot().unwrap();
        let launch = NativeLaunch {
            cwd: home.root().into(),
            native_home,
            profile: None,
            model: None,
        };
        let id = launch.bootstrap().unwrap();
        assert!(!id.is_empty());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(provider.snapshot().unwrap(), roster);
    }
}
