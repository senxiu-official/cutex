//! Read-only, generation-bound busy-wake capability from negotiated ingress.
//! No native RPC, runtime launch or capability inference from a profile name.
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::session::model::CutexSessionRecord;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "soon")] Soon,
    #[serde(rename = "afterTurn")] AfterTurn,
    #[default]
    #[serde(rename = "none")] None,
}

/// Exactly the native `require_delivery(Soon)` negotiation, including legacy
/// omission of the delivery list. The independently verified bundle gates it.
pub fn native_soon(bundle_soon: bool, initialize: &Value) -> bool {
    bundle_soon && initialize.get("externalInputDeliveries").and_then(Value::as_array)
        .is_none_or(|modes| modes.iter().any(|mode| mode.as_str() == Some("soon")))
}

/// Exactly the Claude native ingress + `soon` capability, shared with delivery.
pub fn claude_soon(capabilities: &Value) -> bool {
    capabilities["externalInputVersion"] == 1
        && capabilities["externalInputVersions"].as_array().is_some_and(|v| v.contains(&json!(2)))
        && capabilities["externalInputDeliveries"].as_array().is_some_and(|modes|
            ["after_turn", "passive", "soon"].iter().all(|mode| modes.iter().any(|v| v.as_str() == Some(*mode))))
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Observed {
    session: String, thread: String, generation: u64, initialize: Value,
}

fn directory() -> anyhow::Result<PathBuf> { Ok(crate::config::paths::runtime_dir()?.join("busy-wake-capabilities")) }
fn path(dir: &Path, session: &str, generation: u64) -> Option<PathBuf> {
    (!session.is_empty() && !session.starts_with('.') && session.len() <= 200
        && session.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .then(|| dir.join(format!("{session}-{generation}.json")))
}

/// Keep the newest observed generation, including when an old negotiation
/// finishes late. Exact canonical filenames scope deletion to this session.
fn prune_generations(dir: &Path, session: &str, generation: u64) -> std::io::Result<()> {
    let prefix = format!("{session}-");
    let mut files = Vec::new();
    let mut newest = generation;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        match entry.file_type() {
            Ok(kind) if kind.is_file() => {},
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let name = entry.file_name();
        let Some(number) = name.to_str().and_then(|name| name.strip_prefix(&prefix))
            .and_then(|name| name.strip_suffix(".json")) else { continue; };
        let Ok(found) = number.parse::<u64>() else { continue; };
        if number != found.to_string() { continue; }
        newest = newest.max(found);
        files.push((found, entry.path()));
    }
    for (found, file) in files {
        if found < newest {
            match std::fs::remove_file(file) {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

/// The runtime manager already negotiated these fields. Keep only ingress
/// capability facts, never endpoint/auth/config values, and never fail launch.
pub fn record(session: &str, thread: &str, generation: u64, initialize: &Value) {
    let result = (|| -> anyhow::Result<()> {
        let dir = directory()?; let Some(file) = path(&dir, session, generation) else { return Ok(()) };
        let mut kept = serde_json::Map::new();
        for key in ["externalInputVersion", "externalInputVersions", "externalInputDeliveries"] {
            if let Some(value) = initialize.get(key) { kept.insert(key.into(), value.clone()); }
        }
        std::fs::create_dir_all(&dir)?;
        crate::config::atomic::write_private_pretty_json_atomic(&file, &Observed {
            session: session.into(), thread: thread.into(), generation, initialize: Value::Object(kept),
        }, "negotiated busy wake capability")?;
        prune_generations(&dir, session, generation)?;
        Ok(())
    })();
    if let Err(error) = result { eprintln!("busy wake capability: {error:#}"); }
}

pub fn for_record(record: &CutexSessionRecord) -> Capability {
    let observed = directory().ok().and_then(|dir| path(&dir, &record.cutex_session_id, record.runtime_generation))
        .and_then(|file| std::fs::read(file).ok()).and_then(|bytes| serde_json::from_slice::<Observed>(&bytes).ok());
    let Some(observed) = observed.filter(|v| v.session == record.cutex_session_id
        && v.generation == record.runtime_generation && Some(v.thread.as_str()) == record.app_server_thread_id()) else { return Capability::None };
    if record.claude_session.is_some() {
        return if claude_soon(&observed.initialize) { Capability::Soon } else { Capability::AfterTurn };
    }
    if record.dsh_session.is_some() { return Capability::AfterTurn; }
    let bundle_soon = record.explicit_launch.as_ref().and_then(|contract| {
        (crate::agent_management::file_sha256(&contract.bundle_manifest).ok().as_ref() == Some(&contract.bundle_sha256))
            .then(|| std::fs::read(&contract.bundle_manifest).ok()).flatten()
    }).and_then(|bytes| serde_json::from_slice::<crate::launch::stock::StockBundle>(&bytes).ok()).map(|bundle| bundle.soon_ingress());
    match bundle_soon {
        Some(soon) if native_soon(soon, &observed.initialize) => Capability::Soon,
        Some(_) => Capability::AfterTurn,
        None => Capability::None,
    }
}

pub fn for_session(session: &str) -> Capability {
    crate::session::store::shared_session_row(session).ok().flatten()
        .map(|record| for_record(&record)).unwrap_or_default()
}

#[cfg(test)]
#[path = "busy_delivery_tests.rs"]
mod tests;
