//! Owner-only, explicit, per-version runtime deletion. GET is advisory; POST rechecks under fences.
use super::runtime_prune_inventory::{self as inventory, Version};
use crate::http::server::{SimpleHttpRequest, write_json_response};
use anyhow::{Context, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

pub const PATH: &str = "/v2/runtimes/prune";
pub const CAPABILITY: &str = "runtime-prune-v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    ids: Vec<String>,
}

fn layout() -> Option<crate::upgrade::layout::Layout> {
    crate::components::kit_layout::candidate()
}

fn remove(path: &Path) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

struct Staged {
    id: String,
    kind: String,
    moved: Vec<(PathBuf, PathBuf)>,
}

fn restore(staged: Staged, reason: &str) -> Value {
    let mut restored = true;
    for (original, quarantine) in staged.moved.iter().rev() {
        restored &= !original.exists() && std::fs::rename(quarantine, original).is_ok();
    }
    json!({"id":staged.id,"status":if restored {"refused"} else {"failed"},"freedBytes":0,
        "reason":if restored {reason} else {"io_error"}})
}

/// The caller holds the reference and session writer fences while isolating targets.
/// Same-directory renames prevent new launches at old paths before the final process inventory.
fn prepare(version: &Version) -> Result<Staged, Value> {
    let mut moved = Vec::<(PathBuf, PathBuf)>::new();
    let result = (|| -> anyhow::Result<()> {
        for target in &version.targets {
            inventory::target_bytes(target)?; // re-check root ownership immediately before rename
            let parent = target.parent().context("runtime target parent missing")?;
            let quarantine = parent.join(format!(".runtime-prune-{}", uuid::Uuid::new_v4()));
            std::fs::rename(target, &quarantine)?;
            moved.push((target.clone(), quarantine));
        }
        Ok(())
    })();
    let staged = Staged {
        id: version.id.clone(),
        kind: version.kind.clone(),
        moved,
    };
    if result.is_err() {
        let mut value = restore(staged, "io_error");
        value["status"] = json!("failed");
        return Err(value);
    }
    Ok(staged)
}

fn finish(staged: Staged) -> Value {
    let mut freed = 0u64;
    let mut failed = false;
    for (original, quarantine) in &staged.moved {
        // Report bytes actually removed, including partial IO failures, rather than the GET estimate.
        let before = inventory::tree_bytes(quarantine).unwrap_or(0);
        if remove(quarantine).is_ok() {
            freed = freed.saturating_add(before);
        } else {
            let remaining = inventory::tree_bytes(quarantine).unwrap_or(before);
            freed = freed.saturating_add(before.saturating_sub(remaining));
            // Keep any remainder reachable at the original version path for an explicit retry.
            if !original.exists() {
                let _ = std::fs::rename(quarantine, original);
            }
            failed = true;
        }
    }
    let mut value =
        json!({"id":staged.id,"status":if failed {"failed"} else {"deleted"},"freedBytes":freed});
    if failed {
        value["reason"] = json!("io_error");
    }
    value
}

fn process_recheck(
    slots: Vec<Result<Staged, Value>>,
    context: &[PathBuf],
) -> Vec<Result<Staged, Value>> {
    let mut targets = slots
        .iter()
        .filter_map(|s| s.as_ref().ok())
        .flat_map(|s| s.moved.iter())
        .flat_map(|(original, quarantine)| [original.clone(), quarantine.clone()])
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return slots;
    }
    targets.extend_from_slice(context);
    let running = super::runtime_prune_processes::running_paths(&targets);
    slots
        .into_iter()
        .map(|slot| match slot {
            Ok(staged) => match &running {
                Err(_) => Err(restore(staged, "inspection_failed")),
                Ok(paths)
                    if staged.kind == "claude"
                        && paths.iter().any(|p| {
                            p == Path::new(super::runtime_prune_processes::UNKNOWN_CLAUDE)
                        }) =>
                {
                    Err(restore(staged, "inspection_failed"))
                }
                Ok(paths)
                    if staged.moved.iter().any(|(original, quarantine)| {
                        paths.iter().any(|path| {
                            inventory::overlaps(original, path)
                                || inventory::overlaps(quarantine, path)
                        })
                    }) =>
                {
                    Err(restore(staged, "running"))
                }
                Ok(_) => Ok(staged),
            },
            Err(value) => Err(value),
        })
        .collect()
}

#[cfg(test)]
fn delete(version: &Version) -> Value {
    process_recheck(vec![prepare(version)], &version.targets)
        .pop()
        .unwrap()
        .map(finish)
        .unwrap_or_else(|value| value)
}

pub fn execute(ids: &[String]) -> anyhow::Result<Value> {
    ensure!(
        !ids.is_empty() && ids.len() <= 100,
        "select between 1 and 100 runtime versions"
    );
    ensure!(
        ids.iter()
            .all(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid runtime id"
    );
    let mut unique = std::collections::HashSet::new();
    ensure!(
        ids.iter().all(|id| unique.insert(id)),
        "duplicate runtime id"
    );
    let home = crate::config::paths::runtime_dir()?;
    let layout = layout();
    let _upgrade = layout
        .as_ref()
        .map(|layout| {
            crate::upgrade::state::OperationLock::try_acquire(layout)
                .and_then(|guard| guard.context("runtime operation busy"))
        })
        .transpose()?;
    let _references = crate::runtime::reference_guard::exclusive()?;
    let path = crate::session::store::cutex_sessions_path()?;
    let slots = crate::session::store::with_locked_session_store(&path, |store| {
        // One fresh authority snapshot under all writer fences. No GET estimate is trusted.
        let current = inventory::inspect(&home, layout.as_ref(), store)?;
        let mut slots = Vec::new();
        for id in ids {
            let slot = match current.versions.iter().find(|version| version.id == *id) {
                None => {
                    Err(json!({"id":id,"status":"refused","freedBytes":0,"reason":"not_found"}))
                }
                Some(version) if !version.can_prune => {
                    Err(json!({"id":id,"status":"refused","freedBytes":0,
                    "reason":version.blocked_reasons.first().map(String::as_str).unwrap_or("inspection_failed")}))
                }
                Some(version) => prepare(version),
            };
            slots.push(slot);
        }
        let context = current
            .versions
            .iter()
            .flat_map(|v| v.targets.iter().cloned())
            .collect::<Vec<_>>();
        Ok(process_recheck(slots, &context))
    })?;
    // Unlink outside the session writer lock. The reference + component fences still exclude new
    // references, while unrelated session persistence can proceed during a large directory removal.
    let results = slots
        .into_iter()
        .map(|slot| slot.map(finish).unwrap_or_else(|value| value))
        .collect::<Vec<_>>();
    let freed = results
        .iter()
        .map(|value| value["freedBytes"].as_u64().unwrap_or(0))
        .sum::<u64>();
    Ok(json!({"freedBytes":freed,"results":results}))
}

pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let result = match request.method.as_str() {
        "GET" => (|| {
            let home = crate::config::paths::runtime_dir()?;
            let store = crate::session::store::load_cutex_session_store()?;
            let list = inventory::inspect(&home, layout().as_ref(), &store)?;
            Ok(json!({"versions":list.versions}))
        })(),
        "POST" => (|| {
            let body: Request = serde_json::from_slice(&request.body)?;
            execute(&body.ids)
        })(),
        _ => {
            return super::server::write_v2_error(
                stream,
                405,
                "Method Not Allowed",
                "method_not_allowed",
                "use GET or POST /v2/runtimes/prune",
                false,
                json!({}),
            );
        }
    };
    match result {
        Ok(body) => write_json_response(stream, 200, "OK", &body),
        Err(_) => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "runtime_prune_refused",
            "runtime cleanup could not establish current authority; refresh and retry",
            false,
            json!({}),
        ),
    }
}

#[cfg(test)]
#[path = "runtime_prune_tests.rs"]
mod tests;
