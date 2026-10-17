//! Ordinary foreground sessions using the installed light CLI's explicit inputs.
//! No durable identity, managed owner, or runtime review action is created here.
use anyhow::Context;
use cutex::launch::local_deployment::LocalDeployment;
use cutex::launch::selected_profile::Config;
use cutex::launch::selected_status::Status;
use cutex::profiles::model::{CliKind, RuntimeConfig, StoredAccount};
use std::path::{Path, PathBuf};

fn executable_path(program: &str) -> Option<PathBuf> {
    let candidate = |path: PathBuf| -> Option<PathBuf> {
        if path.is_file() { return path.canonicalize().ok(); }
        #[cfg(windows)]
        if path.extension().is_none() {
            let exe=path.with_extension("exe");
            if exe.is_file() { return exe.canonicalize().ok(); }
        }
        None
    };
    if Path::new(program).components().count()>1 {
        return candidate(PathBuf::from(program));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .find_map(|dir| candidate(dir.join(program)))
}
fn selected_cli_matches(program: &Path, cli: &cutex::launch::stock::VerifiedFile) -> anyhow::Result<bool> {
    if program.canonicalize()? == cli.path.canonicalize()? { return Ok(true); }
    // Windows shortcuts are executable copies, not symlinks. Recognize the
    // selected artifact by content without treating arbitrary overrides as native.
    Ok(std::fs::metadata(program)?.len() == std::fs::metadata(&cli.path)?.len()
        && cutex::agent_management::file_sha256(program)? == cli.sha256)
}

pub(super) fn selected_entry_available() -> anyhow::Result<bool> {
    let Some(deployment) = LocalDeployment::selected()? else { return Ok(false); };
    let bundle: cutex::launch::stock::StockBundle = serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)?;
    let Some(program) = executable_path(&cutex::launch::program::codex_program()) else { return Ok(false); };
    selected_cli_matches(&program, bundle.cli.as_ref().unwrap_or(&bundle.executable))
}

pub(super) fn command(
    account: &StoredAccount,
    args: &[String],
) -> anyhow::Result<Option<std::process::Command>> {
    if account.cli_kind != CliKind::Codex || !matches!(account.runtime, RuntimeConfig::Host) {
        return Ok(None);
    }
    let Some(deployment) = LocalDeployment::selected()? else {
        return Ok(None);
    };
    let bundle: cutex::launch::stock::StockBundle =
        serde_json::from_slice(&std::fs::read(&deployment.bundle_manifest)?)?;
    let cli = bundle.cli.as_ref().unwrap_or(&bundle.executable);
    let program = cutex::launch::program::codex_program();
    let Some(resolved) = executable_path(&program) else { return Ok(None); };
    if !selected_cli_matches(&resolved, cli)? { return Ok(None); }
    cutex::profiles::catalog_overlay::refresh_for_launch(&account.id);
    let files = cutex::profiles::materialize::ensure_materialized_account_files(account)?;
    let mut value: toml::Value = toml::from_str(&std::fs::read_to_string(&files.config_path)?)?;
    let table = value
        .as_table_mut()
        .context("profile configuration must be a table")?;
    table.insert("cutex_provider_mode".into(), "selected_profile_v2".into());
    // Profiles may intentionally inherit model, effort, and TUI settings from the native home.
    // Resolve only those missing values; never rewrite the stored profile.
    if ["model", "model_reasoning_effort", "tui"]
        .iter()
        .any(|key| !table.contains_key(*key))
    {
        let shared: toml::Value = toml::from_str(&std::fs::read_to_string(
            deployment.native_home.join("config.toml"),
        )?)?;
        for key in ["model", "model_reasoning_effort", "tui"] {
            if !table.contains_key(key) {
                if let Some(value) = shared.get(key) {
                    table.insert(key.into(), value.clone());
                }
            }
        }
    }

    let legacy_job = table
        .get("mcp_servers")
        .and_then(|servers| servers.get("cutex_job"))
        .cloned();
    let global = cutex::config::store::load_codez_config_checked()?;
    let new_session = !args.iter().any(|arg| matches!(arg.as_str(), "resume" | "fork"));
    let defaults = new_session.then(|| global.new_session_defaults.get(&account.name)).flatten();
    let (mut projection, model, reasoning) = Config::parse(&toml::to_string(&value)?)?.review(
        &account.id,
        files.auth_path,
        defaults.and_then(|d| d.model.as_ref()),
        defaults.and_then(|d| d.reasoning.as_ref()),
    )?;
    cutex::profiles::model_variants::validate_runtime(&projection, &bundle, &deployment.native_home)?;
    if let Some(tui) = &projection.settings.tui {
        projection.status = Status::review(
            &files.custom_status_items_path,
            tui.status_line.as_deref().unwrap_or_default(),
            &account.name,
        )?;
    }
    let mut launch = cutex::launch::command::LaunchCommand::new(program)
        .env("CODEX_HOME", deployment.native_home.to_string_lossy())
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_INSTALL_DIR")
        .env_remove("CUTEX_AGENT_ID")
        .env_remove("CUTEX_RUNTIME_GENERATION")
        .env_remove("CUTEX_AGENT_BUS_URL")
        .env_remove("CUTEX_AGENT_BUS_TOKEN")
        .args(projection.migrated_native_args(
            true,
            &cutex::config::paths::host_codex_home_dir()?,
            &deployment.native_home,
        )?);
    launch = super::stock_lifecycle::option(
        launch,
        "model_provider",
        projection.provider_id(),
    )?;
    launch = launch.args(cutex::profiles::reviewer_catalog::launch_args(&projection, &model, &bundle, &deployment.native_home));
    launch = super::stock_lifecycle::option(launch, "model", model)?;
    if let Some(reasoning) = reasoning {
        launch = super::stock_lifecycle::option(launch, "model_reasoning_effort", reasoning)?;
    }
    if projection.requires_job {
        // Job's trusted-runtime adapter requires CUTEX_AGENT_ID. A plain
        // session has no managed identity; keep that adapter explicitly off.
        let mut job = legacy_job.context("Selected Job configuration missing")?;
        job.as_table_mut()
            .context("Job configuration must be a table")?
            .insert("enabled".into(), false.into());
        launch = super::stock_lifecycle::option(launch, "mcp_servers.cutex_job", job)?;
        if !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V"))
        {
            eprintln!(
                "Ordinary session: Job Service is available after adoption as a managed Agent."
            );
        }
    }
    if let Some(status) = &projection.status {
        launch = launch
            .arg("--status-items-file")
            .arg(status.materialize()?.to_string_lossy());
    }
    launch = super::status_preferences::apply(launch,
        projection.settings.tui.as_ref().and_then(|t| t.status_line.as_ref()), Some(&account.id))?;
    // Explicit invocation options take precedence over the selected profile.
    let mut command = launch.args(args.iter().cloned()).to_command();
    command.envs(cutex::config::proxy::native_proxy_envs(
        account, &cutex::config::store::load_codez_config_checked()?,
    ));
    command.env("CUTEX_NOTIFICATION_CONTROL", std::env::current_exe()?);
    command.env("CUTEX_SESSION_PROFILE_ID", &account.id);
    command.env("CUTEX_SESSION_PROFILE_NAME", &account.name);
    // The helper only initializes UUIDs created after this launch, so /resume
    // within the foreground UI cannot change an old session's preference.
    if new_session {
        command.env("CUTEX_NEW_NOTIFICATION", serde_json::to_string(&global.new_session_notification)?);
        command.env("CUTEX_NEW_NOTIFICATION_SINCE", chrono::Utc::now().timestamp_millis().to_string());
    } else {
        command.env_remove("CUTEX_NEW_NOTIFICATION");
        command.env_remove("CUTEX_NEW_NOTIFICATION_SINCE");
    }
    if let Some(secret) = projection.secret()? {
        secret.apply(&mut command);
    }
    Ok(Some(command))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_entry_uses_selected_profile_but_other_binary_does_not() {
        let root=std::env::temp_dir().join(format!("cutex-cli-copy-{}",uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let original=root.join("codex.exe");let copied=root.join("cute-codex.exe");
        std::fs::write(&original,b"native").unwrap();std::fs::copy(&original,&copied).unwrap();
        let cli=cutex::launch::stock::VerifiedFile{path:original.clone(),sha256:cutex::agent_management::file_sha256(&original).unwrap()};
        assert!(selected_cli_matches(&copied,&cli).unwrap());
        std::fs::write(&copied,b"other!").unwrap();
        assert!(!selected_cli_matches(&copied,&cli).unwrap());
        #[cfg(windows)]
        assert_eq!(executable_path(root.join("cute-codex").to_str().unwrap()),Some(copied.canonicalize().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
