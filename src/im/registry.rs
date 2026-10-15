//! IM/workbench coding-session registry model and persistence.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

use crate::agent_bus::model::AgentRegistrationClass;
use crate::config::atomic::write_pretty_json_atomic;
use crate::config::paths::config_dir;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ImRegistry {
    #[serde(default)]
    pub sessions: HashMap<String, CodingSessionRegistration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingSessionRegistration {
    pub session_id: String,
    pub display_name: String,
    pub host_id: String,
    pub cwd: String,
    pub profile: Option<String>,
    pub groups: Vec<String>,
    pub registration_class: AgentRegistrationClass,
    pub visible: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub last_runtime_agent_id: Option<String>,
}

pub fn im_registry_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("im-sessions.json"))
}

pub fn load_im_registry() -> anyhow::Result<ImRegistry> {
    let path = im_registry_path()?;
    if !path.exists() {
        return Ok(ImRegistry::default());
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read IM registry: {}", path.display()))?;
    serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse IM registry: {}", path.display()))
}

pub fn save_im_registry(registry: &ImRegistry) -> anyhow::Result<()> {
    let path = im_registry_path()?;
    write_pretty_json_atomic(&path, registry, "IM registry")
}
