//! Profile quota snapshots for trend lines
//! (`GET /v2/usage/quota-history`). Whenever profile quota gauges are read
//! through `profiles::quota::gauges`, each successfully read gauge is
//! appended at most once per `SNAPSHOT_INTERVAL_SECS` (by its upstream
//! `observedAt`) to `<runtime>/management-v2/usage-quota-history/YYYY-MM.jsonl`.
//! Append-only, newline-prefixed lines (a torn tail never corrupts the next
//! line), private permissions; unreadable lines are skipped on read.

use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use chrono::DateTime;
use chrono::Datelike;
use chrono::NaiveDate;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use super::store::secure_directory;
use super::store::secure_file;

pub const SNAPSHOT_INTERVAL_SECS: i64 = 15 * 60;
const HISTORY_DIR: &str = "usage-quota-history";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    /// Unix seconds of the upstream observation.
    pub ts: i64,
    pub profile_id: String,
    pub profile: String,
    pub gauge_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub remaining_percent: Option<f64>,
    #[serde(default)]
    pub balance: Option<String>,
    #[serde(default)]
    pub reset_at: Option<i64>,
}

/// `(root dir, profileId, gaugeId) -> last recorded ts`, lazily seeded per
/// root from the newest history files.
static LAST_RECORDED: Mutex<Option<HashMap<(PathBuf, String, String), i64>>> = Mutex::new(None);

pub fn history_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?
        .join("management-v2")
        .join(HISTORY_DIR))
}

/// Best-effort hook for the quota read path. Never fails the read.
pub fn record_quota_gauges(
    profile_id: &str,
    profile_name: &str,
    gauges: &[crate::profiles::quota::Gauge],
) {
    let snapshots = gauges
        .iter()
        .filter(|gauge| gauge.error.is_none())
        .filter(|gauge| gauge.remaining_percent.is_some() || gauge.balance.is_some())
        .map(|gauge| QuotaSnapshot {
            ts: gauge.observed_at.unwrap_or_else(|| Utc::now().timestamp()),
            profile_id: profile_id.to_string(),
            profile: profile_name.to_string(),
            gauge_id: gauge.id.clone(),
            label: Some(gauge.label.clone()).filter(|label| !label.is_empty()),
            kind: Some(gauge.kind.to_string()),
            remaining_percent: gauge.remaining_percent.filter(|value| value.is_finite()),
            balance: gauge.balance.clone(),
            reset_at: gauge.reset_at,
        })
        .collect::<Vec<_>>();
    if snapshots.is_empty() {
        return;
    }
    let result = history_root().and_then(|root| record_snapshots_at(&root, &snapshots));
    if let Err(error) = result {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to record quota history: {error:#}");
    }
}

/// Append the snapshots that are at least `SNAPSHOT_INTERVAL_SECS` newer than
/// the last one recorded for the same gauge. Returns how many were written.
pub(super) fn record_snapshots_at(
    root: &Path,
    snapshots: &[QuotaSnapshot],
) -> anyhow::Result<usize> {
    let mut guard = LAST_RECORDED
        .lock()
        .map_err(|_| anyhow::anyhow!("quota history lock was poisoned"))?;
    let last = guard.get_or_insert_with(HashMap::new);
    let root_key = root.to_path_buf();
    if !last.keys().any(|(dir, _, _)| dir == &root_key) {
        for snapshot in read_recent(root)? {
            let key = (root_key.clone(), snapshot.profile_id, snapshot.gauge_id);
            let entry = last.entry(key).or_insert(snapshot.ts);
            *entry = (*entry).max(snapshot.ts);
        }
        // Marker so an empty history is not rescanned on every read.
        last.entry((root_key.clone(), String::new(), String::new()))
            .or_insert(i64::MIN);
    }
    let mut written = 0;
    for snapshot in snapshots {
        let key = (
            root_key.clone(),
            snapshot.profile_id.clone(),
            snapshot.gauge_id.clone(),
        );
        if last
            .get(&key)
            .is_some_and(|previous| snapshot.ts < previous.saturating_add(SNAPSHOT_INTERVAL_SECS))
        {
            continue;
        }
        append_snapshot(root, snapshot)?;
        last.insert(key, snapshot.ts);
        written += 1;
    }
    Ok(written)
}

fn month_file(root: &Path, ts: i64) -> anyhow::Result<PathBuf> {
    let at = DateTime::<Utc>::from_timestamp(ts, 0).context("quota snapshot ts out of range")?;
    Ok(root.join(format!("{:04}-{:02}.jsonl", at.year(), at.month())))
}

fn append_snapshot(root: &Path, snapshot: &QuotaSnapshot) -> anyhow::Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("Failed to create quota history: {}", root.display()))?;
    secure_directory(root)?;
    let path = month_file(root, snapshot.ts)?;
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("Failed to open quota history: {}", path.display()))?;
    secure_file(&path)?;
    let mut line = Vec::with_capacity(256);
    line.push(b'\n');
    serde_json::to_writer(&mut line, snapshot)?;
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

/// Snapshots of the two newest month files (enough to seed the throttle).
fn read_recent(root: &Path) -> anyhow::Result<Vec<QuotaSnapshot>> {
    let mut stems = list_month_stems(root)?;
    stems.sort();
    let start = stems.len().saturating_sub(2);
    let mut out = Vec::new();
    for stem in &stems[start..] {
        read_file(&root.join(format!("{stem}.jsonl")), &mut out)?;
    }
    Ok(out)
}

fn list_month_stems(root: &Path) -> anyhow::Result<Vec<String>> {
    match fs::read_dir(root) {
        Ok(entries) => Ok(entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
            .filter(|stem| stem.len() == 7)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to read quota history: {}", root.display()))
        }
    }
}

fn read_file(path: &Path, out: &mut Vec<QuotaSnapshot>) -> anyhow::Result<()> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(snapshot) = serde_json::from_str::<QuotaSnapshot>(&line) {
            out.push(snapshot);
        }
    }
    Ok(())
}

/// Snapshots whose local day (via `local_date`) is in `[from, to]`, optionally
/// for one profile (matched by ID or name), sorted by `(ts, profile, gauge)`.
pub fn read_quota_history_at(
    root: &Path,
    from: NaiveDate,
    to: NaiveDate,
    profile: Option<&str>,
    local_date: &dyn Fn(DateTime<Utc>) -> NaiveDate,
) -> anyhow::Result<Vec<QuotaSnapshot>> {
    let (first, last) = super::query::month_bounds(from, to);
    let mut stems = list_month_stems(root)?;
    stems.retain(|stem| stem.as_str() >= first.as_str() && stem.as_str() <= last.as_str());
    stems.sort();
    let mut all = Vec::new();
    for stem in stems {
        read_file(&root.join(format!("{stem}.jsonl")), &mut all)?;
    }
    all.retain(|snapshot| {
        profile.is_none_or(|p| snapshot.profile_id == p || snapshot.profile == p)
            && DateTime::<Utc>::from_timestamp(snapshot.ts, 0)
                .map(|at| {
                    let day = local_date(at);
                    day >= from && day <= to
                })
                .unwrap_or(false)
    });
    all.sort_by(|a, b| {
        a.ts.cmp(&b.ts)
            .then_with(|| a.profile_id.cmp(&b.profile_id))
            .then_with(|| a.gauge_id.cmp(&b.gauge_id))
    });
    all.dedup_by(|a, b| a.ts == b.ts && a.profile_id == b.profile_id && a.gauge_id == b.gauge_id);
    Ok(all)
}
