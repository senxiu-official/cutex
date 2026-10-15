//! Codex home session lookup helpers for runtime resume planning.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};

use crate::config::atomic::write_private_bytes_atomic;
use crate::config::paths::{config_dir, host_codex_home_dir};
use crate::role_revision::Rfc3339;

pub fn codex_session_exists_in_home(session_id: &str) -> anyhow::Result<bool> {
    let codex_home = host_codex_home_dir()?;
    if codex_session_index_contains(&codex_home, session_id)? {
        return Ok(true);
    }
    Ok(codex_session_rollout_file_exists(
        &codex_home.join("sessions"),
        session_id,
    )?)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterruptedHistoryRepair {
    pub rollout_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub repaired_turn_ids: Vec<String>,
    pub normalized_ordinals: bool,
}

#[derive(Clone, Debug)]
struct TurnStartBoundary {
    turn_id: String,
    line_index: usize,
    started_at: Option<i64>,
}

/// Repair terminal events omitted by a previously hard-stopped managed runtime.
///
/// This is deliberately an explicit, offline recovery operation rather than a
/// permanent reinterpretation of native Codex history. The original rollout is
/// backed up before each missing terminal event is inserted at the boundary
/// immediately before the next turn. A terminal event found after a later turn
/// is relocated to that boundary because native reverse reconstruction cannot
/// associate such an out-of-order event safely. Re-running the repair is a no-op.
pub fn repair_interrupted_rollout_history(
    session_id: &str,
) -> anyhow::Result<InterruptedHistoryRepair> {
    let codex_home = host_codex_home_dir()?;
    let backup_root = config_dir()?.join("history-repair-backups");
    repair_interrupted_rollout_history_in_home(&codex_home, &backup_root, session_id)
}

fn repair_interrupted_rollout_history_in_home(
    codex_home: &Path,
    backup_root: &Path,
    session_id: &str,
) -> anyhow::Result<InterruptedHistoryRepair> {
    let rollout_path =
        unique_rollout_file_for_session(&codex_home.join("sessions"), session_id)?
            .with_context(|| format!("native rollout not found for session {session_id}"))?;
    let input = fs::read(&rollout_path)
        .with_context(|| format!("Failed to read native rollout: {}", rollout_path.display()))?;
    let lines = input
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    let mut starts = Vec::<TurnStartBoundary>::new();
    let mut terminals = BTreeMap::<String, Vec<usize>>::new();
    let mut previous_ordinal = None::<u64>;
    let mut first_ordinal_fault = None::<usize>;
    let mut observed_session_id = None::<String>;
    for (line_index, line) in lines.iter().enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(line).with_context(|| {
            format!(
                "Failed to parse native rollout line {}: {}",
                line_index + 1,
                rollout_path.display()
            )
        })?;
        if let Some(ordinal) = value.get("ordinal").and_then(serde_json::Value::as_u64) {
            if previous_ordinal.is_some_and(|previous| ordinal != previous.saturating_add(1)) {
                first_ordinal_fault.get_or_insert(line_index);
            }
            previous_ordinal = Some(ordinal);
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("session_meta") {
            let metadata_session_id = value
                .pointer("/payload/id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .with_context(|| {
                    format!(
                        "native rollout session_meta omitted its id at line {}: {}",
                        line_index + 1,
                        rollout_path.display()
                    )
                })?;
            if observed_session_id
                .as_deref()
                .is_some_and(|observed| observed != metadata_session_id)
            {
                anyhow::bail!(
                    "native rollout contains conflicting session ids: {}",
                    rollout_path.display()
                );
            }
            observed_session_id = Some(metadata_session_id.to_string());
        }
        if value.get("type").and_then(serde_json::Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            anyhow::bail!(
                "native rollout event omitted payload at line {}: {}",
                line_index + 1,
                rollout_path.display()
            );
        };
        let event_type = payload.get("type").and_then(serde_json::Value::as_str);
        let turn_id = payload
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
            .filter(|turn_id| !turn_id.trim().is_empty());
        match (event_type, turn_id) {
            (Some("task_started" | "turn_started"), Some(turn_id)) => {
                starts.push(TurnStartBoundary {
                    turn_id: turn_id.to_string(),
                    line_index,
                    started_at: payload
                        .get("started_at")
                        .and_then(serde_json::Value::as_i64),
                });
            }
            (Some("task_complete" | "turn_complete" | "turn_aborted"), Some(turn_id)) => {
                terminals
                    .entry(turn_id.to_string())
                    .or_default()
                    .push(line_index);
            }
            _ => {}
        }
    }
    let observed_session_id = observed_session_id.with_context(|| {
        format!(
            "native rollout omitted session_meta: {}",
            rollout_path.display()
        )
    })?;
    if observed_session_id != session_id {
        anyhow::bail!(
            "native rollout identity mismatch: expected {session_id}, found {observed_session_id}"
        );
    }

    let mut insertions = BTreeMap::<usize, Vec<Vec<u8>>>::new();
    let mut skipped_lines = BTreeSet::<usize>::new();
    let mut repaired_turn_ids = Vec::<String>::new();
    for (start_index, start) in starts.iter().enumerate() {
        let next_start = starts.get(start_index + 1);
        let terminal_lines = terminals
            .get(&start.turn_id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|line_index| *line_index > start.line_index)
            .collect::<Vec<_>>();
        let Some(next_start) = next_start else {
            if terminal_lines.is_empty() {
                insertions
                    .entry(lines.len())
                    .or_default()
                    .push(interrupted_turn_line(
                        start,
                        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        0,
                    )?);
                repaired_turn_ids.push(start.turn_id.clone());
            }
            continue;
        };

        let has_ordered_terminal = terminal_lines
            .iter()
            .any(|line_index| *line_index < next_start.line_index);
        let late_terminals = terminal_lines
            .iter()
            .copied()
            .filter(|line_index| *line_index >= next_start.line_index)
            .collect::<Vec<_>>();
        if has_ordered_terminal && late_terminals.is_empty() {
            continue;
        }
        skipped_lines.extend(late_terminals);
        if !has_ordered_terminal {
            let insertion_index = first_ordinal_fault
                .filter(|line_index| {
                    *line_index > start.line_index && *line_index <= next_start.line_index
                })
                .unwrap_or(next_start.line_index);
            insertions
                .entry(insertion_index)
                .or_default()
                .push(interrupted_turn_line(
                    start,
                    line_timestamp(lines[insertion_index]).unwrap_or_else(|| {
                        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                    }),
                    0,
                )?);
        }
        repaired_turn_ids.push(start.turn_id.clone());
    }
    let rewrite_from = insertions
        .keys()
        .copied()
        .chain(skipped_lines.iter().copied())
        .chain(first_ordinal_fault)
        .min();
    if repaired_turn_ids.is_empty() && rewrite_from.is_none() {
        return Ok(InterruptedHistoryRepair {
            rollout_path,
            backup_path: None,
            repaired_turn_ids,
            normalized_ordinals: false,
        });
    }

    let backup_dir = backup_root.join(session_id);
    fs::create_dir_all(&backup_dir).with_context(|| {
        format!(
            "Failed to create history repair backup directory: {}",
            backup_dir.display()
        )
    })?;
    let backup_path = backup_dir.join(format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        rollout_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rollout.jsonl")
    ));
    fs::copy(&rollout_path, &backup_path).with_context(|| {
        format!(
            "Failed to back up native rollout {} to {}",
            rollout_path.display(),
            backup_path.display()
        )
    })?;

    let rewrite_from = rewrite_from.context("history repair omitted its rewrite boundary")?;
    let mut next_ordinal = ordinal_before(&lines, rewrite_from)?
        .checked_add(1)
        .context("native rollout ordinal overflow")?;
    let mut output = Vec::with_capacity(input.len().saturating_add(1024));
    for (line_index, line) in lines.iter().enumerate() {
        append_insertions(&mut output, insertions.get(&line_index), &mut next_ordinal)?;
        if !skipped_lines.contains(&line_index) {
            if line_index < rewrite_from || line.iter().all(u8::is_ascii_whitespace) {
                output.extend_from_slice(line);
            } else {
                append_line_with_ordinal(&mut output, line, next_ordinal)?;
                next_ordinal = next_ordinal
                    .checked_add(1)
                    .context("native rollout ordinal overflow")?;
            }
        }
    }
    append_insertions(&mut output, insertions.get(&lines.len()), &mut next_ordinal)?;
    write_private_bytes_atomic(&rollout_path, &output).with_context(|| {
        format!(
            "Failed to atomically replace repaired rollout: {}",
            rollout_path.display()
        )
    })?;

    Ok(InterruptedHistoryRepair {
        rollout_path,
        backup_path: Some(backup_path),
        repaired_turn_ids,
        normalized_ordinals: first_ordinal_fault.is_some(),
    })
}

fn interrupted_turn_line(
    start: &TurnStartBoundary,
    timestamp: String,
    ordinal: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut payload = serde_json::json!({
        "type": "turn_aborted",
        "turn_id": start.turn_id,
        "reason": "interrupted",
    });
    if let Some(started_at) = start.started_at {
        payload["started_at"] = serde_json::Value::from(started_at);
    }
    let mut line = serde_json::to_vec(&serde_json::json!({
        "timestamp": timestamp,
        "ordinal": ordinal,
        "type": "event_msg",
        "payload": payload,
    }))?;
    line.push(b'\n');
    Ok(line)
}

fn line_timestamp(line: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()?
        .get("timestamp")?
        .as_str()
        .map(str::to_string)
}

fn ordinal_before(lines: &[&[u8]], boundary: usize) -> anyhow::Result<u64> {
    lines[..boundary]
        .iter()
        .rev()
        .find_map(|line| {
            serde_json::from_slice::<serde_json::Value>(line)
                .ok()?
                .get("ordinal")?
                .as_u64()
        })
        .context("native rollout omitted an ordinal before the repair boundary")
}

fn append_line_with_ordinal(output: &mut Vec<u8>, line: &[u8], ordinal: u64) -> anyhow::Result<()> {
    let mut value: serde_json::Value = serde_json::from_slice(line)?;
    let ordinal_value = value
        .get_mut("ordinal")
        .context("native rollout line omitted ordinal inside the repair suffix")?;
    *ordinal_value = serde_json::Value::from(ordinal);
    serde_json::to_writer(&mut *output, &value)?;
    output.push(b'\n');
    Ok(())
}

fn append_insertions(
    output: &mut Vec<u8>,
    insertions: Option<&Vec<Vec<u8>>>,
    next_ordinal: &mut u64,
) -> anyhow::Result<()> {
    let Some(insertions) = insertions else {
        return Ok(());
    };
    if !output.is_empty() && !output.ends_with(b"\n") {
        output.push(b'\n');
    }
    for insertion in insertions {
        append_line_with_ordinal(output, insertion, *next_ordinal)?;
        *next_ordinal = next_ordinal
            .checked_add(1)
            .context("native rollout ordinal overflow")?;
    }
    Ok(())
}

fn unique_rollout_file_for_session(
    root: &Path,
    session_id: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read Codex sessions dir: {}", root.display()));
        }
    };
    let expected_suffix = format!("-{session_id}.jsonl");
    let mut found = None::<PathBuf>;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            anyhow::bail!(
                "native session source contains a symlink: {}",
                path.display()
            );
        }
        if file_type.is_dir() {
            if let Some(candidate) = unique_rollout_file_for_session(&path, session_id)? {
                if let Some(previous) = found {
                    anyhow::bail!(
                        "multiple native rollouts match session {session_id}: {}, {}",
                        previous.display(),
                        candidate.display()
                    );
                }
                found = Some(candidate);
            }
            continue;
        }
        if file_type.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&expected_suffix))
        {
            if let Some(previous) = found {
                anyhow::bail!(
                    "multiple native rollouts match session {session_id}: {}, {}",
                    previous.display(),
                    path.display()
                );
            }
            found = Some(path);
        }
    }
    Ok(found)
}

/// Conservatively checks the complete native Codex session sources for a
/// session created during one historical bootstrap attempt. This is an
/// absence proof: malformed or unreadable evidence is an error, never empty.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeSessionCorrelation {
    ProvenAbsent,
    Present { session_id: String },
    Ambiguous { reason: String },
}

#[derive(Clone, Debug)]
struct NativeSessionCandidate {
    session_id: String,
    cwd: Option<PathBuf>,
}

/// Correlates all native Codex evidence in the attempt window with the exact
/// reserved managed cwd. An explicitly different cwd is unrelated; evidence
/// that cannot be correlated remains ambiguous rather than authorizing retry.
pub fn correlate_codex_session_between(
    started_at: &Rfc3339,
    failed_at: &Rfc3339,
    managed_cwd: &Path,
) -> anyhow::Result<NativeSessionCorrelation> {
    let codex_home = host_codex_home_dir()?;
    correlate_codex_session_between_in_home(&codex_home, started_at, failed_at, managed_cwd)
}

/// Performs the same conservative correlation against an explicitly selected
/// runtime's actual Codex home (for example, the host-mounted Docker home).
pub fn correlate_codex_session_between_in_home(
    codex_home: &Path,
    started_at: &Rfc3339,
    failed_at: &Rfc3339,
    managed_cwd: &Path,
) -> anyhow::Result<NativeSessionCorrelation> {
    let start = DateTime::parse_from_rfc3339(started_at.as_str())?.with_timezone(&Utc);
    let end = DateTime::parse_from_rfc3339(failed_at.as_str())?.with_timezone(&Utc);
    if end < start {
        anyhow::bail!("native bootstrap reconciliation window is reversed");
    }
    // Rollout file names have second precision. Expanding one second on both
    // sides can only produce a safe false-positive fence.
    let start = start - Duration::seconds(1);
    let end = end + Duration::seconds(1);
    let mut candidates = session_index_entries_between(codex_home, start, end)?;
    for rollout in rollout_entries_between(&codex_home.join("sessions"), start, end)? {
        match candidates.get_mut(&rollout.session_id) {
            Some(candidate) => {
                if let (Some(index_cwd), Some(rollout_cwd)) = (&candidate.cwd, &rollout.cwd) {
                    if index_cwd != rollout_cwd {
                        return Ok(NativeSessionCorrelation::Ambiguous {
                            reason: "native index and rollout cwd markers conflict".to_string(),
                        });
                    }
                }
                if candidate.cwd.is_none() {
                    candidate.cwd = rollout.cwd;
                }
            }
            None => {
                candidates.insert(rollout.session_id.clone(), rollout);
            }
        }
    }
    let uncorrelated = candidates
        .values()
        .filter(|candidate| candidate.cwd.is_none())
        .map(|candidate| candidate.session_id.as_str())
        .collect::<Vec<_>>();
    if !uncorrelated.is_empty() {
        return Ok(NativeSessionCorrelation::Ambiguous {
            reason: format!(
                "native session(s) {} have no managed-cwd correlation marker",
                uncorrelated.join(", ")
            ),
        });
    }
    let matching = candidates
        .values()
        .filter(|candidate| candidate.cwd.as_deref() == Some(managed_cwd))
        .map(|candidate| candidate.session_id.clone())
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [] => Ok(NativeSessionCorrelation::ProvenAbsent),
        [session_id] => Ok(NativeSessionCorrelation::Present {
            session_id: session_id.clone(),
        }),
        _ => Ok(NativeSessionCorrelation::Ambiguous {
            reason: format!(
                "multiple native sessions match the exact managed cwd: {}",
                matching.join(", ")
            ),
        }),
    }
}

fn session_index_entries_between(
    codex_home: &Path,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> anyhow::Result<BTreeMap<String, NativeSessionCandidate>> {
    let mut candidates = BTreeMap::new();
    let path = codex_home.join("session_index.jsonl");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(candidates),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to open session index: {}", path.display()));
        }
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("Failed to parse native session index: {}", path.display()))?;
        let Some(session_id) = value.get("id").and_then(serde_json::Value::as_str) else {
            anyhow::bail!("native session index entry omitted id: {}", path.display());
        };
        if session_id.trim().is_empty() {
            anyhow::bail!(
                "native session index entry has empty id: {}",
                path.display()
            );
        }
        let timestamp = [
            "timestamp",
            "created_at",
            "createdAt",
            "updated_at",
            "updatedAt",
        ]
        .into_iter()
        .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .ok_or_else(|| anyhow::anyhow!("native session index entry omitted timestamp"))?;
        let timestamp = DateTime::parse_from_rfc3339(timestamp)
            .with_context(|| format!("native session index timestamp is invalid: {timestamp}"))?
            .with_timezone(&Utc);
        if timestamp >= start && timestamp <= end {
            let cwd = value
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from);
            candidates.insert(
                session_id.to_string(),
                NativeSessionCandidate {
                    session_id: session_id.to_string(),
                    cwd,
                },
            );
        }
    }
    Ok(candidates)
}

fn rollout_entries_between(
    root: &Path,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> anyhow::Result<Vec<NativeSessionCandidate>> {
    let mut candidates = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(candidates),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to read Codex sessions dir: {}", root.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            anyhow::bail!(
                "native session source contains a symlink: {}",
                path.display()
            );
        }
        if file_type.is_dir() {
            candidates.extend(rollout_entries_between(&path, start, end)?);
            continue;
        }
        if !file_type.is_file() {
            anyhow::bail!(
                "native session source has an unsupported entry: {}",
                path.display()
            );
        }
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            anyhow::bail!("native rollout file name is not UTF-8: {}", path.display());
        };
        if !file_name.starts_with("rollout-") || !file_name.ends_with(".jsonl") {
            continue;
        }
        let wall_clock = file_name
            .get(8..27)
            .ok_or_else(|| anyhow::anyhow!("native rollout file omitted timestamp"))?;
        let wall_clock = NaiveDateTime::parse_from_str(wall_clock, "%Y-%m-%dT%H-%M-%S")
            .with_context(|| {
                format!("native rollout filename timestamp is invalid: {file_name}")
            })?;
        // Codex rollout filenames use local wall-clock time without an offset.
        // Use that value only as a wide discovery bound, never as identity or
        // attempt-time proof. Every civil time-zone offset is inside 24 hours;
        // the authoritative embedded RFC3339 metadata below decides inclusion.
        let wall_clock_as_utc = Utc.from_utc_datetime(&wall_clock);
        if wall_clock_as_utc < start - Duration::hours(24)
            || wall_clock_as_utc > end + Duration::hours(24)
        {
            continue;
        }
        let (created_at, _event_timestamp, candidate) = rollout_session_metadata(&path)?;
        // SessionMeta records creation time. Its envelope records a later
        // write: native rollouts may remain buffered until explicit persist.
        if created_at >= start && created_at <= end {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn rollout_session_metadata(
    path: &Path,
) -> anyhow::Result<(DateTime<Utc>, DateTime<Utc>, NativeSessionCandidate)> {
    let file = fs::File::open(path)
        .with_context(|| format!("Failed to open native rollout: {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "Failed to parse native rollout metadata: {}",
                path.display()
            )
        })?;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
            continue;
        }
        let event_timestamp = value
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "native rollout session_meta omitted event timestamp: {}",
                    path.display()
                )
            })?;
        let event_timestamp = DateTime::parse_from_rfc3339(event_timestamp)
            .with_context(|| {
                format!(
                    "native rollout session_meta event timestamp is invalid: {}",
                    path.display()
                )
            })?
            .with_timezone(&Utc);
        let payload = value.get("payload").ok_or_else(|| {
            anyhow::anyhow!(
                "native rollout session_meta omitted payload: {}",
                path.display()
            )
        })?;
        let created_at = payload
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "native rollout session_meta omitted creation timestamp: {}",
                    path.display()
                )
            })?;
        let created_at = DateTime::parse_from_rfc3339(created_at)
            .with_context(|| {
                format!(
                    "native rollout session_meta creation timestamp is invalid: {}",
                    path.display()
                )
            })?
            .with_timezone(&Utc);
        let session_id = payload
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("native rollout session_meta omitted id: {}", path.display())
            })?;
        if let Some(legacy_session_id) = payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            // Modern subagent rollouts use `id` for the child thread while
            // retaining the parent thread in the legacy `session_id` field.
            // That is a parent relationship, not an identity conflict. Keep
            // rejecting all other mismatches so malformed evidence cannot
            // weaken the absence proof used to authorize an exact retry.
            let declared_parent = [
                payload
                    .get("forked_from_id")
                    .and_then(serde_json::Value::as_str),
                payload
                    .get("parent_thread_id")
                    .and_then(serde_json::Value::as_str),
                payload
                    .pointer("/source/subagent/thread_spawn/parent_thread_id")
                    .and_then(serde_json::Value::as_str),
            ]
            .into_iter()
            .flatten()
            .any(|value| value == legacy_session_id);
            let is_subagent_parent = payload
                .get("thread_source")
                .and_then(serde_json::Value::as_str)
                == Some("subagent")
                && declared_parent;
            if legacy_session_id != session_id && !is_subagent_parent {
                anyhow::bail!(
                    "native rollout session_meta session identities conflict: {}",
                    path.display()
                );
            }
        }
        let cwd = payload
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        return Ok((
            created_at,
            event_timestamp,
            NativeSessionCandidate {
                session_id: session_id.to_string(),
                cwd,
            },
        ));
    }
    anyhow::bail!("native rollout omitted session_meta: {}", path.display())
}

fn codex_session_index_contains(codex_home: &Path, session_id: &str) -> anyhow::Result<bool> {
    let path = codex_home.join("session_index.jsonl");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to open session index: {}", path.display()));
        }
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_str) == Some(session_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn codex_session_rollout_file_exists(root: &Path, session_id: &str) -> anyhow::Result<bool> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to read Codex sessions dir: {}", root.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            if codex_session_rollout_file_exists(&path, session_id)? {
                return Ok(true);
            }
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if file_name.starts_with("rollout-")
            && file_name.ends_with(".jsonl")
            && file_name.contains(session_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> (Rfc3339, Rfc3339) {
        (
            Rfc3339::new("2026-08-30T01:02:03Z").unwrap(),
            Rfc3339::new("2026-08-30T01:02:05Z").unwrap(),
        )
    }

    fn root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cutex-native-reconciliation-{label}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn native_reconciliation_correlates_rollout_cwd_and_ignores_unrelated_session() {
        let index_root = root("correlated");
        let rollout_day = index_root
            .join("sessions")
            .join("2026")
            .join("08")
            .join("30");
        fs::create_dir_all(&rollout_day).unwrap();
        fs::write(
            index_root.join("session_index.jsonl"),
            concat!(
                "{\"id\":\"native-matching\",\"timestamp\":\"2026-08-30T01:02:04Z\"}\n",
                "{\"id\":\"native-unrelated\",\"timestamp\":\"2026-08-30T01:02:04Z\"}\n"
            ),
        )
        .unwrap();
        fs::write(
            rollout_day.join("rollout-2026-08-30T01-02-04-native-matching.jsonl"),
            "{\"timestamp\":\"2026-08-30T01:02:04.100Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"native-matching\",\"timestamp\":\"2026-08-30T01:02:04Z\",\"cwd\":\"/managed/worker\"}}\n",
        )
        .unwrap();
        fs::write(
            rollout_day.join("rollout-2026-08-30T01-02-04-native-unrelated.jsonl"),
            "{\"timestamp\":\"2026-08-30T01:02:04.100Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"native-unrelated\",\"timestamp\":\"2026-08-30T01:02:04Z\",\"cwd\":\"/other/worker\"}}\n",
        )
        .unwrap();
        let (started_at, failed_at) = window();
        assert_eq!(
            correlate_codex_session_between_in_home(
                &index_root,
                &started_at,
                &failed_at,
                Path::new("/managed/worker")
            )
            .unwrap(),
            NativeSessionCorrelation::Present {
                session_id: "native-matching".to_string()
            }
        );
        assert_eq!(
            correlate_codex_session_between_in_home(
                &index_root,
                &started_at,
                &failed_at,
                Path::new("/absent/worker")
            )
            .unwrap(),
            NativeSessionCorrelation::ProvenAbsent
        );
        fs::remove_dir_all(index_root).unwrap();
    }

    #[test]
    fn native_reconciliation_reports_uncorrelated_incident_shape_and_rejects_malformed_evidence() {
        let empty_root = root("empty");
        fs::create_dir_all(&empty_root).unwrap();
        let (started_at, failed_at) = window();
        assert_eq!(
            correlate_codex_session_between_in_home(
                &empty_root,
                &started_at,
                &failed_at,
                Path::new("/managed/worker")
            )
            .unwrap(),
            NativeSessionCorrelation::ProvenAbsent
        );

        let incident_root = root("incident");
        fs::create_dir_all(&incident_root).unwrap();
        fs::write(
            incident_root.join("session_index.jsonl"),
            "{\"id\":\"historical-native\",\"timestamp\":\"2026-08-30T01:02:04Z\"}\n",
        )
        .unwrap();
        assert!(matches!(
            correlate_codex_session_between_in_home(
                &incident_root,
                &started_at,
                &failed_at,
                Path::new("/managed/worker")
            )
            .unwrap(),
            NativeSessionCorrelation::Ambiguous { reason }
                if reason.contains("no managed-cwd correlation marker")
        ));

        let malformed_root = root("malformed");
        fs::create_dir_all(&malformed_root).unwrap();
        fs::write(malformed_root.join("session_index.jsonl"), "not-json\n").unwrap();
        assert!(correlate_codex_session_between_in_home(
            &malformed_root,
            &started_at,
            &failed_at,
            Path::new("/managed/worker")
        )
        .is_err());

        fs::remove_dir_all(empty_root).unwrap();
        fs::remove_dir_all(incident_root).unwrap();
        fs::remove_dir_all(malformed_root).unwrap();
    }

    #[test]
    fn native_reconciliation_rejects_multiple_exact_cwd_sessions() {
        let codex_home = root("multiple-exact");
        let rollout_day = codex_home
            .join("sessions")
            .join("2026")
            .join("08")
            .join("30");
        fs::create_dir_all(&rollout_day).unwrap();
        for session_id in ["native-one", "native-two"] {
            fs::write(
                rollout_day.join(format!(
                    "rollout-2026-08-30T01-02-04-{session_id}.jsonl"
                )),
                format!(
                    "{{\"timestamp\":\"2026-08-30T01:02:04.100Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"timestamp\":\"2026-08-30T01:02:04Z\",\"cwd\":\"/managed/worker\"}}}}\n"
                ),
            )
            .unwrap();
        }
        let (started_at, failed_at) = window();
        assert!(matches!(
            correlate_codex_session_between_in_home(
                &codex_home,
                &started_at,
                &failed_at,
                Path::new("/managed/worker")
            )
            .unwrap(),
            NativeSessionCorrelation::Ambiguous { reason }
                if reason.contains("multiple native sessions")
        ));
        fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn native_reconciliation_uses_embedded_utc_time_not_local_wall_clock_filename() {
        let codex_home = root("local-filename-utc-metadata");
        let rollout_day = codex_home
            .join("sessions")
            .join("2026")
            .join("08")
            .join("30");
        fs::create_dir_all(&rollout_day).unwrap();
        let session_id = "018e0000-0000-7000-8000-0001e0005e95";
        let managed_cwd = "/home/example/Projects/cutex/agent-home/r23-toolchain-review-glm-r1";
        fs::write(
            rollout_day.join(format!(
                "rollout-2026-08-30T12-03-06-{session_id}.jsonl"
            )),
            format!(
                "{{\"timestamp\":\"2026-08-30T02:03:07.028Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"session_id\":\"{session_id}\",\"timestamp\":\"2026-08-30T02:03:06.981Z\",\"cwd\":\"{managed_cwd}\"}}}}\n"
            ),
        )
        .unwrap();
        let started_at = Rfc3339::new("2026-08-30T02:03:05Z").unwrap();
        let failed_at = Rfc3339::new("2026-08-30T02:03:19Z").unwrap();

        assert_eq!(
            correlate_codex_session_between_in_home(
                &codex_home,
                &started_at,
                &failed_at,
                Path::new(managed_cwd)
            )
            .unwrap(),
            NativeSessionCorrelation::Present {
                session_id: session_id.to_string()
            }
        );
        fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn native_reconciliation_accepts_subagent_parent_session_id_without_matching_child() {
        let codex_home = root("subagent-parent-session-id");
        let rollout_day = codex_home
            .join("sessions")
            .join("2026")
            .join("08")
            .join("30");
        fs::create_dir_all(&rollout_day).unwrap();
        fs::write(
            rollout_day.join("rollout-2026-08-30T01-02-04-child-native.jsonl"),
            "{\"timestamp\":\"2026-08-30T01:02:04.100Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-native\",\"session_id\":\"parent-native\",\"forked_from_id\":\"parent-native\",\"parent_thread_id\":\"parent-native\",\"timestamp\":\"2026-08-30T01:02:04Z\",\"cwd\":\"/director\",\"thread_source\":\"subagent\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"parent-native\"}}}}}\n",
        )
        .unwrap();
        let (started_at, failed_at) = window();

        assert_eq!(
            correlate_codex_session_between_in_home(
                &codex_home,
                &started_at,
                &failed_at,
                Path::new("/managed/worker")
            )
            .unwrap(),
            NativeSessionCorrelation::ProvenAbsent
        );
        fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn native_reconciliation_rejects_unrelated_legacy_session_id_mismatch() {
        let codex_home = root("unrelated-session-id-mismatch");
        let rollout_day = codex_home.join("sessions").join("2026");
        fs::create_dir_all(&rollout_day).unwrap();
        fs::write(
            rollout_day.join("rollout-2026-08-30T01-02-04-native.jsonl"),
            "{\"timestamp\":\"2026-08-30T01:02:04.100Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"native\",\"session_id\":\"different-native\",\"timestamp\":\"2026-08-30T01:02:04Z\",\"cwd\":\"/managed/worker\"}}\n",
        )
        .unwrap();
        let (started_at, failed_at) = window();

        assert!(correlate_codex_session_between_in_home(
            &codex_home,
            &started_at,
            &failed_at,
            Path::new("/managed/worker")
        )
        .is_err());
        fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn native_reconciliation_rejects_missing_and_malformed_embedded_timestamps() {
        let (started_at, failed_at) = window();
        let cases = [
            (
                "missing-event-time",
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"native\",\"timestamp\":\"2026-08-30T01:02:04Z\",\"cwd\":\"/managed/worker\"}}\n",
            ),
            (
                "malformed-creation-time",
                "{\"timestamp\":\"2026-08-30T01:02:04Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"native\",\"timestamp\":\"not-rfc3339\",\"cwd\":\"/managed/worker\"}}\n",
            ),

        ];
        for (label, metadata) in cases {
            let codex_home = root(label);
            let rollout_day = codex_home.join("sessions").join("2026");
            fs::create_dir_all(&rollout_day).unwrap();
            fs::write(
                rollout_day.join("rollout-2026-08-30T11-02-04-native.jsonl"),
                metadata,
            )
            .unwrap();
            assert!(correlate_codex_session_between_in_home(
                &codex_home,
                &started_at,
                &failed_at,
                Path::new("/managed/worker")
            )
            .is_err());
            fs::remove_dir_all(codex_home).unwrap();
        }
    }

    #[test]
    fn native_reconciliation_uses_creation_time_for_delayed_persistence() {
        let home = root("delayed-persistence");
        let day = home.join("sessions/2026/08/30");
        fs::create_dir_all(&day).unwrap();
        let (start, end) = window();
        for (name, created, written, cwd) in [
            (
                "target",
                "2026-08-30T01:02:04Z",
                "2026-08-30T01:07:04Z",
                "/managed/worker",
            ),
            (
                "unrelated",
                "2026-08-30T00:02:04Z",
                "2026-08-30T01:02:04Z",
                "/other",
            ),
        ] {
            fs::write(day.join(format!("rollout-2026-08-30T11-02-04-{name}.jsonl")), serde_json::to_string(&serde_json::json!({"timestamp":written,"type":"session_meta","payload":{"id":name,"timestamp":created,"cwd":cwd}})).unwrap()).unwrap();
        }
        assert_eq!(
            correlate_codex_session_between_in_home(
                &home,
                &start,
                &end,
                Path::new("/managed/worker")
            )
            .unwrap(),
            NativeSessionCorrelation::Present {
                session_id: "target".into()
            }
        );
        assert_eq!(
            correlate_codex_session_between_in_home(&home, &start, &end, Path::new("/other"))
                .unwrap(),
            NativeSessionCorrelation::ProvenAbsent
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn interrupted_history_repair_inserts_missing_terminal_before_the_next_turn() {
        // Leave room for the atomic writer's UUID suffix below Windows MAX_PATH.
        // This test exercises turn repair/idempotence, not long-path support.
        let codex_home = std::env::temp_dir()
            .join(format!("cutex-repair-{}", uuid::Uuid::new_v4()));
        let backup_root = codex_home.join("repair-backups");
        let rollout_day = codex_home
            .join("sessions")
            .join("2026")
            .join("09")
            .join("08");
        fs::create_dir_all(&rollout_day).unwrap();
        let session_id = "018e0000-0000-7000-8000-0001f000cb52";
        let rollout_path =
            rollout_day.join(format!("rollout-2026-09-08T09-00-00-{session_id}.jsonl"));
        fs::write(
            &rollout_path,
            concat!(
                "{\"timestamp\":\"2026-09-08T09:00:00Z\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"018e0000-0000-7000-8000-0001f000cb52\",\"timestamp\":\"2026-09-08T09:00:00Z\",\"cwd\":\"/managed\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:01Z\",\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"orphaned\",\"started_at\":1788858001}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:02Z\",\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"completed\",\"started_at\":1788858002}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:03Z\",\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"completed\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:04Z\",\"ordinal\":4,\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_started\",\"turn_id\":\"v2-completed\",\"started_at\":1788858004}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:05Z\",\"ordinal\":5,\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_complete\",\"turn_id\":\"v2-completed\"}}"
            ),
        )
        .unwrap();

        let repaired =
            repair_interrupted_rollout_history_in_home(&codex_home, &backup_root, session_id)
                .unwrap();
        assert_eq!(repaired.rollout_path, rollout_path);
        assert_eq!(repaired.repaired_turn_ids, vec!["orphaned"]);
        assert!(!repaired.normalized_ordinals);
        assert!(repaired.backup_path.as_ref().unwrap().is_file());
        let lines = BufReader::new(fs::File::open(&rollout_path).unwrap())
            .lines()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let repaired_position = lines
            .iter()
            .position(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                value
                    .pointer("/payload/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("turn_aborted")
            })
            .expect("inserted terminal event");
        let repaired_event: serde_json::Value =
            serde_json::from_str(&lines[repaired_position]).unwrap();
        let following_event: serde_json::Value =
            serde_json::from_str(&lines[repaired_position + 1]).unwrap();
        assert_eq!(repaired_event["ordinal"], 2);
        assert_eq!(repaired_event["payload"]["turn_id"], "orphaned");
        assert_eq!(following_event["payload"]["type"], "task_started");
        assert_eq!(following_event["payload"]["turn_id"], "completed");

        let second =
            repair_interrupted_rollout_history_in_home(&codex_home, &backup_root, session_id)
                .unwrap();
        assert!(second.repaired_turn_ids.is_empty());
        assert!(!second.normalized_ordinals);
        assert!(second.backup_path.is_none());
        assert_eq!(
            BufReader::new(fs::File::open(&rollout_path).unwrap())
                .lines()
                .count(),
            lines.len()
        );
        fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn interrupted_history_repair_relocates_a_late_terminal_event() {
        let codex_home = root("late-interrupted-history-repair");
        let backup_root = codex_home.join("repair-backups");
        let rollout_day = codex_home.join("sessions").join("2026");
        fs::create_dir_all(&rollout_day).unwrap();
        let session_id = "018e0000-0000-7000-8000-0001f000cb52";
        let rollout_path = rollout_day.join(format!("rollout-test-{session_id}.jsonl"));
        fs::write(
            &rollout_path,
            concat!(
                "{\"timestamp\":\"2026-09-08T09:00:00Z\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"018e0000-0000-7000-8000-0001f000cb52\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:01Z\",\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"old\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:02Z\",\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"new\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:03Z\",\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"new\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:04Z\",\"ordinal\":4,\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_aborted\",\"turn_id\":\"old\",\"reason\":\"interrupted\"}}\n"
            ),
        )
        .unwrap();

        let repaired =
            repair_interrupted_rollout_history_in_home(&codex_home, &backup_root, session_id)
                .unwrap();
        assert_eq!(repaired.repaired_turn_ids, vec!["old"]);
        let lifecycle = BufReader::new(fs::File::open(&rollout_path).unwrap())
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
                (
                    value
                        .pointer("/payload/type")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    value
                        .pointer("/payload/turn_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle,
            vec![
                (None, None),
                (Some("task_started".to_string()), Some("old".to_string())),
                (Some("turn_aborted".to_string()), Some("old".to_string())),
                (Some("task_started".to_string()), Some("new".to_string())),
                (Some("task_complete".to_string()), Some("new".to_string())),
            ]
        );

        let second =
            repair_interrupted_rollout_history_in_home(&codex_home, &backup_root, session_id)
                .unwrap();
        assert!(second.repaired_turn_ids.is_empty());
        fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn interrupted_history_repair_normalizes_restart_ordinal_suffix() {
        let codex_home = root("restart-ordinal-history-repair");
        let backup_root = codex_home.join("repair-backups");
        let rollout_day = codex_home.join("sessions").join("2026");
        fs::create_dir_all(&rollout_day).unwrap();
        let session_id = "018e0000-0000-7000-8000-0001f000cb52";
        let rollout_path = rollout_day.join(format!("rollout-test-{session_id}.jsonl"));
        fs::write(
            &rollout_path,
            concat!(
                "{\"timestamp\":\"2026-09-08T09:00:00Z\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"id\":\"018e0000-0000-7000-8000-0001f000cb52\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:01Z\",\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"old\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:02Z\",\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:03Z\",\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:04Z\",\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"new\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:05Z\",\"ordinal\":4,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"new\"}}\n",
                "{\"timestamp\":\"2026-09-08T09:00:06Z\",\"ordinal\":5,\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_aborted\",\"turn_id\":\"old\",\"reason\":\"interrupted\"}}\n"
            ),
        )
        .unwrap();

        let repaired =
            repair_interrupted_rollout_history_in_home(&codex_home, &backup_root, session_id)
                .unwrap();
        assert_eq!(repaired.repaired_turn_ids, vec!["old"]);
        assert!(repaired.normalized_ordinals);
        let events = BufReader::new(fs::File::open(&rollout_path).unwrap())
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .map(|event| event["ordinal"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            (0..events.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(events[3]["payload"]["type"], "turn_aborted");
        assert_eq!(events[3]["payload"]["turn_id"], "old");
        assert_eq!(events[4]["payload"]["type"], "thread_settings_applied");
        assert_eq!(events[5]["payload"]["type"], "task_started");
        assert_eq!(events[5]["payload"]["turn_id"], "new");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["payload"]["type"] == "turn_aborted")
                .count(),
            1
        );

        let second =
            repair_interrupted_rollout_history_in_home(&codex_home, &backup_root, session_id)
                .unwrap();
        assert!(second.repaired_turn_ids.is_empty());
        assert!(!second.normalized_ordinals);
        fs::remove_dir_all(codex_home).unwrap();
    }
}
