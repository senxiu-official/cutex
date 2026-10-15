//! Presentation metadata for ordinary sessions; this does not adopt an Agent.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProfile {
    pub profile_id: String,
    pub profile_name: String,
}
fn path(thread: &str) -> anyhow::Result<PathBuf> {
    let id = uuid::Uuid::parse_str(thread).context("native session UUID required")?;
    Ok(crate::config::paths::runtime_dir()?.join("session-presentation").join(format!("{id}.json")))
}

/// Called by the existing notification helper, which already knows the native
/// thread ID. Save only explicit launcher context, never guess from defaults.
pub fn record_from_launch(thread: &str) -> anyhow::Result<()> {
    let Ok(profile_id) = std::env::var("CUTEX_SESSION_PROFILE_ID") else { return Ok(()) };
    let profile_name = std::env::var("CUTEX_SESSION_PROFILE_NAME")?;
    save(thread, SessionProfile { profile_id, profile_name })
}
pub fn save(thread: &str, profile: SessionProfile) -> anyhow::Result<()> {
    for value in [&profile.profile_id, &profile.profile_name] {
        ensure!(!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control), "invalid session profile metadata");
    }
    let file = path(thread)?;
    if read(thread)?.as_ref() == Some(&profile) { return Ok(()) }
    crate::config::atomic::write_private_pretty_json_atomic(&file, &profile, "session presentation")
}
pub fn read(thread: &str) -> anyhow::Result<Option<SessionProfile>> {
    match std::fs::read(path(thread)?) {
        Ok(bytes) => { ensure!(bytes.len() <= 2048, "session presentation too large"); Ok(Some(serde_json::from_slice(&bytes)?)) }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Ordinary native resume has no reviewed Agent receipt. Materialize current
/// display settings, labeling unknown historical profiles honestly as N/A.
pub fn status_file(thread: Option<&str>, order: &[String]) -> anyhow::Result<Option<PathBuf>> {
    if !order.iter().any(|s| super::selected_status::is_static_id(s) || matches!(s.as_str(), "cutex_aligned_profile" | "cutex_welcome_indentation")) { return Ok(None) }
    let profile = thread.map(read).transpose()?.flatten();
    let config = crate::config::store::load_codez_config_checked()?;
    let catalog = crate::profiles::materialize::custom_status_items_catalog_json(&config)?
        .context("static status catalog missing")?;
    super::selected_status::materialize_session_display(catalog.as_bytes(), order,
        profile.as_ref().map(|p| p.profile_name.as_str()).unwrap_or("N/A")).map(Some)
}

#[cfg(test)]
mod tests {
    #[test]
    fn metadata_rejects_path_instead_of_native_identity() {
        assert!(super::path("../../other").is_err());
    }
}
