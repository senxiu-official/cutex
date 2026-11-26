use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::session::{model::CutexSessionRecord, store::load_session_row};

pub(super) const PATH: &str = "/v2/storage/archive-migrations/preview";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PreviewRequest {
    session_ids: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileEntry {
    pub(super) relative_path: PathBuf,
    pub(super) bytes: u64,
}

pub(super) fn inspect(root: &Path, request: PreviewRequest) -> anyhow::Result<Value> {
    ensure!(
        request.session_ids.len() <= 100,
        "archive inventory supports up to 100 sessions per request"
    );
    ensure!(
        request
            .session_ids
            .iter()
            .all(|id| !id.is_empty() && id.len() <= 256),
        "invalid archive session ID"
    );
    let mut ids = request.session_ids;
    ids.sort();
    ids.dedup();
    let root = root.canonicalize()?;
    let path = root.join("cutex-sessions.json");
    let location = super::archive_location::read(&root)?;
    let mut records = Vec::new();
    let mut items = Vec::new();
    for id in ids {
        match load_session_row(&path, &id)? {
            Some((_, row)) if row.cutex_session_id == id => records.push(row),
            _ => items.push(json!({"sessionId":id,"status":"blocked","code":"session_not_found"})),
        }
    }
    let mut bytes = 0u64;
    let mut candidates = 0;
    if !records.is_empty() {
        let blocks = super::storage_cleanup_homes::archive_dependency_blocks(&root, &records)?;
        for record in records {
            let id = &record.cutex_session_id;
            if let Some(code) = blocks.get(id) {
                items.push(json!({"sessionId":id,"status":"blocked","code":code}));
                continue;
            }
            // Absence of an allowlisted layout is expected on older deployments.
            match inventory_rollout(&root, &record) {
                Ok(file) => {
                    bytes = bytes
                        .checked_add(file.bytes)
                        .context("archive inventory byte count overflow")?;
                    candidates += 1;
                    items.push(json!({"sessionId":id,"status":"candidate","files":[file],"bytes":file.bytes}));
                }
                Err(error) => items.push(json!({
                    "sessionId":id, "status":"blocked", "code":"rollout_unproven",
                    "reason":error.to_string()
                })),
            }
        }
    }
    Ok(
        json!({"location":location.path,"items":items,"candidateCount":candidates,"totalBytes":bytes}),
    )
}

pub(super) fn inventory_rollout(
    root: &Path,
    record: &CutexSessionRecord,
) -> anyhow::Result<FileEntry> {
    let contract = record
        .explicit_launch
        .as_ref()
        .context("explicit native home required")?;
    let home = &contract.native_home;

    // Inventory the directory itself before trusting any provider DB path.
    // Other rollouts, including inherited/reverted history, require a
    // separate proof of ownership and dependency completeness.
    let mut pending = vec![home.join("sessions")];
    let archived = home.join("archived_sessions");
    super::storage_cleanup_homes::observe(&archived)?;
    match fs::symlink_metadata(&archived) {
        Ok(_) => pending.push(archived),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut budget = 2048usize;
    let mut rollout = None;
    while let Some(path) = pending.pop() {
        budget = budget
            .checked_sub(1)
            .context("rollout inventory limit exceeded")?;
        super::storage_cleanup_homes::observe(&path)?;
        let metadata =
            fs::symlink_metadata(&path).context("native rollout directory unavailable")?;
        ensure!(
            !super::archive_location::linked(&metadata),
            "rollout path is linked"
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)?.take(budget + 1) {
                pending.push(entry?.path());
                ensure!(pending.len() <= budget, "rollout inventory limit exceeded");
            }
        } else {
            ensure!(
                metadata.is_file()
                    && path.starts_with(home.join("sessions"))
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("rollout-")
                            && name.ends_with(&format!("-{}.jsonl", contract.native_id))),
                "unexpected file or inherited rollout in native history"
            );
            ensure!(
                rollout.replace(path).is_none(),
                "multiple native rollouts require dependency review"
            );
        }
    }
    let rollout = rollout.context("native rollout missing")?;
    let relative = rollout.strip_prefix(root)?;
    let file = super::storage_cleanup_fs::open_archive_rollout(root, relative)?;
    let bytes = file.metadata()?.len();
    let mut first = Vec::new();
    BufReader::new(file.take(64 * 1024)).read_until(b'\n', &mut first)?;
    ensure!(
        first.last() == Some(&b'\n'),
        "native history metadata exceeds 64 KiB or is incomplete"
    );
    let value: Value = serde_json::from_slice(&first)?;
    ensure!(
        value["type"] == "session_meta"
            && value["payload"]["id"] == contract.native_id
            && value["payload"]["history_base"].is_null()
            && value["payload"]["forked_from_id"].is_null(),
        "native rollout has an unverified identity or history dependency"
    );
    Ok(FileEntry {
        relative_path: rollout.strip_prefix(home)?.to_owned(),
        bytes,
    })
}
