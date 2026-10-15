//! Machine-local frontend preferences, independent of runtime homes and profiles.
#[cfg(test)]
use anyhow::Context;
use cutex::launch::command::LaunchCommand;
use serde::Deserialize;
use std::path::Path;

#[derive(Default, Deserialize)]
struct Preferences {
    #[serde(default)]
    tui: Tui,
}

#[derive(Default, Deserialize)]
struct Tui {
    status_line: Option<Vec<String>>,
    status_line_use_colors: Option<bool>,
}

#[cfg(test)]
fn read(path: &Path) -> anyhow::Result<Preferences> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).context("Invalid saved Cutex status line preferences"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn selected_order(defaults: Option<&Vec<String>>, profile: Option<&str>) -> anyhow::Result<Vec<String>> {
    let (_, value) = cutex::config::status_line::load(profile)?;
    let saved: Preferences = value.try_into()?;
    Ok(saved.tui.status_line.unwrap_or_else(|| super::notify::status_line(defaults)))
}

pub(super) fn apply(launch: LaunchCommand, defaults: Option<&Vec<String>>, profile: Option<&str>) -> anyhow::Result<LaunchCommand> {
    let (path, value) = cutex::config::status_line::load(profile)?;
    apply_saved(launch, defaults, &path, value.try_into()?)
}

#[cfg(test)]
fn apply_at(launch: LaunchCommand, defaults: Option<&Vec<String>>, path: &Path) -> anyhow::Result<LaunchCommand> {
    let saved = read(path)?;
    apply_saved(launch, defaults, path, saved)
}

fn apply_saved(mut launch: LaunchCommand, defaults: Option<&Vec<String>>, path: &Path, saved: Preferences) -> anyhow::Result<LaunchCommand> {
    // An explicit empty list means disabled. Do not reinsert notification items
    // into a user selection; only add them to the initial profile defaults.
    let order = saved.tui.status_line.unwrap_or_else(|| super::notify::status_line(defaults));
    launch = super::stock_lifecycle::option(launch, "tui.status_line", order)?;
    if let Some(colors) = saved.tui.status_line_use_colors {
        launch = super::stock_lifecycle::option(launch, "tui.status_line_use_colors", colors)?;
    }
    let launch = launch.env("CUTEX_STATUS_LINE_CONFIG", path.to_string_lossy());
    Ok(apply_native_version_environment(launch))
}

fn apply_native_version_environment(launch: LaunchCommand) -> LaunchCommand {
    match cutex::launch::version_registry::registry_path()
        .and_then(|path| cutex::launch::version_registry::load(&path))
    {
        Ok(Some(registry)) => {
            launch.env("CUTEX_LATEST_CLI_VERSION", registry.latest_cli.version)
        }
        Ok(None) | Err(_) => launch.env_remove("CUTEX_LATEST_CLI_VERSION"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_order_overrides_profiles_and_preserves_disabled_items() {
        let root = std::env::temp_dir().join(format!("status-preferences-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("status-line.toml");
        std::fs::write(&path, "[tui]\nstatus_line=['current-dir','cutex_welcome']\nstatus_line_use_colors=false\n").unwrap();
        let saved = read(&path).unwrap();
        assert_eq!(saved.tui.status_line, Some(vec!["current-dir".into(), "cutex_welcome".into()]));
        assert_eq!(saved.tui.status_line_use_colors, Some(false));
        for defaults in [vec!["model-name".into()], vec!["cutex_profile".into()]] {
            let launch = apply_at(LaunchCommand::new("cute-codex"), Some(&defaults), &path).unwrap();
            let options: toml::Value = toml::from_str(&launch.args.chunks_exact(2).map(|pair| pair[1].as_str()).collect::<Vec<_>>().join("\n")).unwrap();
            assert_eq!(options["tui"]["status_line"].clone().try_into::<Vec<String>>().unwrap(), vec!["current-dir", "cutex_welcome"]);
            assert_eq!(options["tui"]["status_line_use_colors"].as_bool(), Some(false));
            assert_eq!(launch.envs, vec![("CUTEX_STATUS_LINE_CONFIG".into(), path.to_string_lossy().into_owned())]);
        }
        std::fs::write(&path, "[tui]\nstatus_line=[]\n").unwrap();
        assert_eq!(read(&path).unwrap().tui.status_line, Some(vec![]));
        std::fs::remove_file(&path).unwrap();
        assert!(read(&path).unwrap().tui.status_line.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
