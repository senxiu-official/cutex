//! Agent bus audit log storage and terminal rendering.
//!
//! The live file is `runtime/agent-bus-audit.jsonl`. It is rotated by size
//! (`config.json` `agent_bus_audit`, default 64 MiB): the live file is
//! renamed to `agent-bus-audit-<UTC stamp>.jsonl` (O(1), never read on the
//! write path), a background thread gzips it to `.jsonl.gz`, and only the
//! newest `keepFiles` rotated files are kept. Identical repeated polls (the
//! same agent polling the same un-ACKed message ids) are coalesced. See
//! docs/agent-bus-operations.md.

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::Context;
use fs2::FileExt;
use serde_json::Value;

use crate::agent_bus::delivery::legacy_delivery_mode_label;
use crate::config::agent_bus_audit::AgentBusAuditSettings;
use crate::config::paths::runtime_dir;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";

pub const AUDIT_FILE_NAME: &str = "agent-bus-audit.jsonl";
const AUDIT_LOCK_NAME: &str = "agent-bus-audit.lock";
const ROTATED_PREFIX: &str = "agent-bus-audit-";
/// Poll-coalescing entries kept per process (one per polling agent).
const MAX_POLL_STATES: usize = 4096;
/// Writers rotate slightly before the limit so concurrent appenders from
/// another process do not push the file far past it.
const ROTATE_SLACK_BYTES: u64 = 64 * 1024;

pub fn agent_bus_audit_log_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join(AUDIT_FILE_NAME))
}

static AUDIT_LOG: OnceLock<Mutex<AuditLog>> = OnceLock::new();

fn global_audit_log() -> anyhow::Result<&'static Mutex<AuditLog>> {
    if let Some(log) = AUDIT_LOG.get() {
        return Ok(log);
    }
    let settings = crate::config::store::load_codez_config()
        .agent_bus_audit
        .effective();
    let _ = AUDIT_LOG.set(Mutex::new(AuditLog::new(runtime_dir()?, settings)));
    AUDIT_LOG.get().context("agent audit log initialization raced")
}

fn locked<T>(apply: impl FnOnce(&mut AuditLog) -> anyhow::Result<T>) -> anyhow::Result<T> {
    let log = global_audit_log()?;
    let mut log = log
        .lock()
        .map_err(|_| anyhow::anyhow!("agent audit log lock poisoned"))?;
    apply(&mut log)
}

/// Use these settings for this process (the bus calls it at start with
/// its config). Later calls replace them.
pub fn configure_agent_bus_audit(settings: AgentBusAuditSettings) -> anyhow::Result<()> {
    locked(|log| {
        log.settings = settings.effective();
        Ok(())
    })
}

/// Bus start: rotate an oversized live file out (by rename; it is never
/// read here), then compress / prune rotated files in the background.
/// Handles a pre-rotation multi-GB file from older releases safely.
pub fn agent_bus_audit_startup_maintenance() -> anyhow::Result<()> {
    locked(|log| log.startup_maintenance())
}

pub fn append_agent_bus_audit_record(value: Value) -> anyhow::Result<()> {
    locked(|log| log.append(&value))
}

/// Write a `polled` record unless it repeats the agent's last recorded poll
/// (same message ids) within the coalescing window. Returns whether a line
/// was written.
pub fn record_agent_bus_poll(
    agent_id: &str,
    agent_name: &str,
    ack_mode: bool,
    message_ids: &[String],
    now_epoch_secs: u64,
) -> anyhow::Result<bool> {
    locked(|log| log.record_poll(agent_id, agent_name, ack_mode, message_ids, now_epoch_secs))
}

/// An ACK ends the agent's repeated-poll run: its next poll is written.
pub fn reset_agent_bus_poll_coalescing(agent_id: &str) {
    let _ = locked(|log| {
        log.polls.remove(agent_id);
        Ok(())
    });
}

#[derive(Debug, Clone)]
struct PollState {
    signature: u64,
    recorded_at: u64,
    suppressed: u64,
}

pub struct AuditLog {
    dir: PathBuf,
    settings: AgentBusAuditSettings,
    polls: HashMap<String, PollState>,
}

static MAINTAINING: AtomicBool = AtomicBool::new(false);

impl AuditLog {
    pub fn new(dir: PathBuf, settings: AgentBusAuditSettings) -> Self {
        Self {
            dir,
            settings: settings.effective(),
            polls: HashMap::new(),
        }
    }

    fn live_path(&self) -> PathBuf {
        self.dir.join(AUDIT_FILE_NAME)
    }

    fn open_live(&self) -> anyhow::Result<File> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("Failed to create runtime dir: {}", self.dir.display()))?;
        let path = self.live_path();
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Content previews are private: new files are owner-only.
            options.mode(0o600);
        }
        options
            .open(&path)
            .with_context(|| format!("Failed to open agent audit log: {}", path.display()))
    }

    fn rotation_threshold(&self) -> u64 {
        let max = self.settings.max_file_bytes();
        max.saturating_sub(ROTATE_SLACK_BYTES.min(max / 2)).max(1)
    }

    pub fn append(&mut self, value: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let mut file = self.open_live()?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + line.len() as u64 > self.rotation_threshold() {
            drop(file);
            if let Err(error) = self.rotate() {
                eprintln!("warning: agent audit log rotation failed: {error:#}");
            }
            file = self.open_live()?;
        }
        file.write_all(&line)?;
        Ok(())
    }

    pub fn record_poll(
        &mut self,
        agent_id: &str,
        agent_name: &str,
        ack_mode: bool,
        message_ids: &[String],
        now: u64,
    ) -> anyhow::Result<bool> {
        let signature = poll_signature(ack_mode, message_ids);
        let window = u64::from(self.settings.coalesce_polls_secs);
        let mut suppressed = 0;
        if let Some(state) = self.polls.get_mut(agent_id) {
            if window > 0
                && state.signature == signature
                && now >= state.recorded_at
                && now - state.recorded_at < window
            {
                state.suppressed += 1;
                return Ok(false);
            }
            suppressed = state.suppressed;
        }
        let mut record = serde_json::json!({
            "event": "polled",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "agent_id": agent_id,
            "agent_name": agent_name,
            "ack_mode": ack_mode,
            "count": message_ids.len(),
            "message_ids": message_ids,
        });
        if suppressed > 0 {
            // Identical polls of this agent not written since its last line.
            record["suppressed_repeats"] = serde_json::json!(suppressed);
        }
        self.append(&record)?;
        if window > 0 {
            if self.polls.len() >= MAX_POLL_STATES && !self.polls.contains_key(agent_id) {
                self.polls.clear();
            }
            self.polls.insert(
                agent_id.to_string(),
                PollState {
                    signature,
                    recorded_at: now,
                    suppressed: 0,
                },
            );
        }
        Ok(true)
    }

    pub fn startup_maintenance(&mut self) -> anyhow::Result<()> {
        let size = fs::metadata(self.live_path()).map(|m| m.len()).unwrap_or(0);
        if size >= self.rotation_threshold() {
            self.rotate()?;
        } else {
            self.spawn_compress_and_prune();
        }
        Ok(())
    }

    /// Rename the live file to a rotated name under the cross-process lock
    /// (rechecking the size: another writer may have rotated already).
    fn rotate(&mut self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let lock_path = self.dir.join(AUDIT_LOCK_NAME);
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(&lock_path)
            .with_context(|| format!("Failed to open {}", lock_path.display()))?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("Failed to lock {}", lock_path.display()))?;
        let live = self.live_path();
        let size = fs::metadata(&live).map(|m| m.len()).unwrap_or(0);
        let result = if size > 0 && size >= self.rotation_threshold() {
            let target = self.unique_rotated_path();
            fs::rename(&live, &target).with_context(|| {
                format!("Failed to rotate {} to {}", live.display(), target.display())
            })
        } else {
            Ok(())
        };
        let _ = FileExt::unlock(&lock);
        drop(lock);
        self.spawn_compress_and_prune();
        result
    }

    fn unique_rotated_path(&self) -> PathBuf {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
        for counter in 0..1000u32 {
            let name = if counter == 0 {
                format!("{ROTATED_PREFIX}{stamp}.jsonl")
            } else {
                format!("{ROTATED_PREFIX}{stamp}-{counter:03}.jsonl")
            };
            let path = self.dir.join(&name);
            if !path.exists() && !self.dir.join(format!("{name}.gz")).exists() {
                return path;
            }
        }
        self.dir
            .join(format!("{ROTATED_PREFIX}{stamp}-{}.jsonl", uuid::Uuid::new_v4()))
    }

    fn spawn_compress_and_prune(&self) {
        let dir = self.dir.clone();
        let settings = self.settings;
        if MAINTAINING.swap(true, Ordering::AcqRel) {
            // A running pass is busy; the next rotation runs another one.
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("agent-audit-rotate".into())
            .spawn(move || {
                if let Err(error) = compress_and_prune(&dir, settings) {
                    eprintln!("warning: agent audit log maintenance failed: {error:#}");
                }
                MAINTAINING.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            MAINTAINING.store(false, Ordering::Release);
        }
    }
}

fn poll_signature(ack_mode: bool, message_ids: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut ids = message_ids.iter().map(String::as_str).collect::<Vec<_>>();
    ids.sort_unstable();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ack_mode.hash(&mut hasher);
    ids.hash(&mut hasher);
    hasher.finish()
}

type RotatedFile = (String, Option<PathBuf>, Option<PathBuf>);

/// Rotated files (`agent-bus-audit-<stamp>`), newest first, each with its
/// plain and / or gz path.
fn rotated_files(dir: &Path) -> anyhow::Result<Vec<RotatedFile>> {
    let mut stems: std::collections::BTreeMap<String, (Option<PathBuf>, Option<PathBuf>)> =
        Default::default();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(ROTATED_PREFIX) || !entry.file_type()?.is_file() {
            continue;
        }
        if let Some(stem) = name.strip_suffix(".jsonl.gz") {
            stems.entry(stem.to_string()).or_default().1 = Some(entry.path());
        } else if let Some(stem) = name.strip_suffix(".jsonl") {
            stems.entry(stem.to_string()).or_default().0 = Some(entry.path());
        }
    }
    let mut files = stems
        .into_iter()
        .map(|(stem, (plain, gz))| (stem, plain, gz))
        .collect::<Vec<_>>();
    files.reverse();
    Ok(files)
}

/// Delete all but the newest `keep_files` rotations, then gzip the kept
/// plain ones (streaming; a file is never loaded into memory). Unfinished
/// `.gz.tmp-*` leftovers of a crashed pass are removed first.
pub fn compress_and_prune(dir: &Path, settings: AgentBusAuditSettings) -> anyhow::Result<()> {
    let settings = settings.effective();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(ROTATED_PREFIX) && name.contains(".gz.tmp-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let keep = settings.keep_files as usize;
    let files = rotated_files(dir)?;
    for (_, plain, gz) in files.iter().skip(keep) {
        for path in [plain, gz].into_iter().flatten() {
            if let Err(error) = fs::remove_file(path) {
                eprintln!("warning: could not remove {}: {error}", path.display());
            }
        }
    }
    if settings.gzip {
        for (_, plain, gz) in files.iter().take(keep) {
            let Some(plain) = plain else {
                continue;
            };
            if gz.is_some() {
                // Finished earlier; the plain copy outlived a crash.
                let _ = fs::remove_file(plain);
            } else {
                gzip_file(plain)?;
            }
        }
    }
    Ok(())
}

fn gzip_file(plain: &Path) -> anyhow::Result<PathBuf> {
    let gz = PathBuf::from(format!("{}.gz", plain.display()));
    let tmp = PathBuf::from(format!("{}.gz.tmp-{}", plain.display(), std::process::id()));
    let input = File::open(plain).with_context(|| format!("open {}", plain.display()))?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let output = options
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    let mut encoder = flate2::write::GzEncoder::new(
        std::io::BufWriter::with_capacity(1 << 20, output),
        flate2::Compression::default(),
    );
    let finished = std::io::copy(&mut BufReader::with_capacity(1 << 20, input), &mut encoder)
        .and_then(|_| encoder.finish())
        .and_then(|mut writer| {
            writer.flush()?;
            writer.get_ref().sync_all()
        });
    if let Err(error) = finished {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow::Error::new(error).context(format!("gzip {}", plain.display())));
    }
    fs::rename(&tmp, &gz).with_context(|| format!("rename {}", tmp.display()))?;
    fs::remove_file(plain).with_context(|| format!("remove {}", plain.display()))?;
    Ok(gz)
}

/// The last `limit` lines (optionally filtered) across the live file and
/// the rotated ones, oldest first. Rotated files are read only while the
/// newer ones hold fewer than `limit` matching lines.
pub fn read_recent_agent_bus_audit_lines(
    dir: &Path,
    limit: usize,
    filter: &dyn Fn(&str) -> bool,
) -> anyhow::Result<Vec<String>> {
    let limit = limit.max(1);
    let mut paths = Vec::new();
    let live = dir.join(AUDIT_FILE_NAME);
    if live.is_file() {
        paths.push((live, false));
    }
    for (_, plain, gz) in rotated_files(dir)? {
        if let Some(gz) = gz {
            paths.push((gz, true));
        } else if let Some(plain) = plain {
            paths.push((plain, false));
        }
    }
    let mut collected: Vec<Vec<String>> = Vec::new();
    let mut total = 0usize;
    for (path, gz) in paths {
        let file = match File::open(&path) {
            Ok(file) => file,
            // Rotated / compressed / pruned meanwhile.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!("open {}", path.display())))
            }
        };
        let reader: Box<dyn Read> = if gz {
            Box::new(flate2::read::MultiGzDecoder::new(file))
        } else {
            Box::new(file)
        };
        let mut tail = std::collections::VecDeque::new();
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() || !filter(&line) {
                continue;
            }
            tail.push_back(line);
            if tail.len() > limit {
                tail.pop_front();
            }
        }
        total += tail.len();
        collected.push(tail.into_iter().collect());
        if total >= limit {
            break;
        }
    }
    let mut lines = collected.into_iter().rev().flatten().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    Ok(lines.split_off(start))
}

pub fn content_preview(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

pub fn agent_audit_record_matches(value: &Value, agent: &str) -> bool {
    [
        "from",
        "to",
        "to_name",
        "agent_id",
        "agent_name",
        "message_id",
    ]
    .iter()
    .any(|key| value.get(*key).and_then(Value::as_str) == Some(agent))
        || value
            .get("message_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(agent)))
}

pub fn print_agent_audit_record(value: &Value) {
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("-");
    match value.get("event").and_then(Value::as_str).unwrap_or("-") {
        "sent" => {
            let id = value
                .get("message_id")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let from = value.get("from").and_then(Value::as_str).unwrap_or("-");
            let to = value
                .get("to_name")
                .and_then(Value::as_str)
                .or_else(|| value.get("to").and_then(Value::as_str))
                .unwrap_or("-");
            let trigger = value
                .get("trigger_turn")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let delivery_mode = value
                .get("delivery_mode")
                .and_then(Value::as_str)
                .unwrap_or_else(|| legacy_delivery_mode_label(trigger));
            let deduplicated = value
                .get("deduplicated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let preview = value
                .get("content_preview")
                .and_then(Value::as_str)
                .unwrap_or("");
            println!(
                "{DIM}{ts}{RESET} {GREEN}sent{RESET} {id} {from} -> {to} mode={delivery_mode} trigger_turn={trigger} deduplicated={deduplicated} {preview}"
            );
        }
        "polled" => {
            let agent_id = value.get("agent_id").and_then(Value::as_str).unwrap_or("-");
            let agent_name = value
                .get("agent_name")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
            let repeats = value
                .get("suppressed_repeats")
                .and_then(Value::as_u64)
                .map(|n| format!(" (+{n} identical polls not logged)"))
                .unwrap_or_default();
            println!(
                "{DIM}{ts}{RESET} {CYAN}polled{RESET} agent={agent_name} id={agent_id} count={count}{repeats}"
            );
        }
        "acked" => {
            let agent_id = value.get("agent_id").and_then(Value::as_str).unwrap_or("-");
            let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
            println!("{DIM}{ts}{RESET} {GREEN}acked{RESET} agent={agent_id} count={count}");
        }
        event => println!("{DIM}{ts}{RESET} {event} {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_preview_truncates_on_character_boundaries() {
        assert_eq!(content_preview("abcdef", 3), "abc...");
        assert_eq!(content_preview("ab", 3), "ab");
    }

    #[test]
    fn audit_record_match_checks_agent_fields_and_message_ids() {
        let value = serde_json::json!({
            "from": "agent-a",
            "message_ids": ["msg-1", "msg-2"]
        });

        assert!(agent_audit_record_matches(&value, "agent-a"));
        assert!(agent_audit_record_matches(&value, "msg-2"));
        assert!(!agent_audit_record_matches(&value, "agent-b"));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cutex-audit-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn small(keep: u32, gzip: bool) -> AgentBusAuditSettings {
        AgentBusAuditSettings {
            max_file_mb: 1,
            keep_files: keep,
            gzip,
            coalesce_polls_secs: 600,
        }
    }

    fn wait_for_maintenance() {
        for _ in 0..500 {
            if !MAINTAINING.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("audit maintenance did not finish");
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("agent-bus-audit"))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    // Rotation, startup handling and pruning share the process-wide
    // maintenance flag: one test covers them in sequence.
    #[test]
    fn rotation_by_size_startup_rotation_gzip_prune_and_reader() {
        let dir = temp_dir("rotate");
        // A pre-rotation oversized live file (stands in for the 745 MB one).
        let live = dir.join(AUDIT_FILE_NAME);
        {
            let mut file = File::create(&live).unwrap();
            let line = format!("{}\n", serde_json::json!({"event": "sent", "message_id": "legacy", "pad": "x".repeat(200)}));
            for _ in 0..(1_200_000 / line.len()) {
                file.write_all(line.as_bytes()).unwrap();
            }
        }
        let mut log = AuditLog::new(dir.clone(), small(2, true));
        log.startup_maintenance().unwrap();
        wait_for_maintenance();
        assert!(!live.exists(), "oversized live file rotated out at start");
        let after_start = names(&dir);
        assert_eq!(after_start.len(), 2, "{after_start:?}"); // gz + lock
        assert!(after_start.iter().any(|n| n.ends_with(".jsonl.gz")), "{after_start:?}");

        // Writes past the limit rotate again; only 2 rotations are kept.
        let pad = "y".repeat(1000);
        for round in 0..3 {
            for index in 0..1100 {
                log.append(&serde_json::json!({"event": "sent", "message_id": format!("m-{round}-{index}"), "pad": pad}))
                    .unwrap();
            }
            wait_for_maintenance();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        wait_for_maintenance();
        compress_and_prune(&dir, small(2, true)).unwrap();
        let rotated = rotated_files(&dir).unwrap();
        assert_eq!(rotated.len(), 2, "{:?}", names(&dir));
        assert!(rotated.iter().all(|(_, plain, gz)| plain.is_none() && gz.is_some()));
        assert!(fs::metadata(&live).unwrap().len() <= 1024 * 1024);
        let everything = read_recent_agent_bus_audit_lines(&dir, 1_000_000, &|_| true).unwrap();
        assert!(!everything.iter().any(|l| l.contains("legacy")), "oldest rotation pruned");

        // The reader spans the live file and the gz rotations, oldest first.
        let lines = read_recent_agent_bus_audit_lines(&dir, 2000, &|_| true).unwrap();
        assert_eq!(lines.len(), 2000);
        let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["message_id"], "m-2-1099");
        let filtered =
            read_recent_agent_bus_audit_lines(&dir, 5, &|line| line.contains("\"m-1-")).unwrap();
        assert_eq!(filtered.len(), 5);
        assert!(filtered[4].contains("m-1-1099"));

        // keepFiles 0 + gzip off: rotations are dropped, nothing compressed.
        compress_and_prune(&dir, small(0, false)).unwrap();
        assert!(rotated_files(&dir).unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn crash_leftovers_are_cleaned() {
        let dir = temp_dir("leftover");
        let stem = "agent-bus-audit-20261009T000000.000Z";
        fs::write(dir.join(format!("{stem}.jsonl")), "{}\n").unwrap();
        fs::write(dir.join(format!("{stem}.jsonl.gz")), b"").unwrap();
        fs::write(dir.join(format!("{stem}.jsonl.gz.tmp-1")), b"partial").unwrap();
        compress_and_prune(&dir, small(4, true)).unwrap();
        assert_eq!(names(&dir), vec![format!("{stem}.jsonl.gz")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn identical_polls_are_coalesced_until_the_set_changes_or_window_ends() {
        let dir = temp_dir("polls");
        let mut log = AuditLog::new(dir.clone(), small(4, false));
        let ids = vec!["b".to_string(), "a".to_string()];
        let same = vec!["a".to_string(), "b".to_string()];
        assert!(log.record_poll("agent-1", "one", true, &ids, 1000).unwrap());
        for t in 0..100 {
            assert!(!log.record_poll("agent-1", "one", true, &same, 1005 + t).unwrap());
        }
        // Another agent is independent.
        assert!(log.record_poll("agent-2", "two", true, &ids, 1010).unwrap());
        // A changed set is written at once and reports the suppressed run.
        assert!(log.record_poll("agent-1", "one", true, &ids[..1], 1200).unwrap());
        // The window ends: the same set is written again.
        assert!(!log.record_poll("agent-1", "one", true, &ids[..1], 1700).unwrap());
        assert!(log.record_poll("agent-1", "one", true, &ids[..1], 1800).unwrap());
        let lines = read_recent_agent_bus_audit_lines(&dir, 100, &|_| true).unwrap();
        let values = lines
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 4);
        assert!(values[0].get("suppressed_repeats").is_none());
        assert_eq!(values[2]["suppressed_repeats"], 100);
        assert_eq!(values[3]["suppressed_repeats"], 1);
        // Window 0 writes every poll.
        let mut every = AuditLog::new(
            dir.clone(),
            AgentBusAuditSettings {
                coalesce_polls_secs: 0,
                ..small(4, false)
            },
        );
        assert!(every.record_poll("agent-3", "three", true, &ids, 1).unwrap());
        assert!(every.record_poll("agent-3", "three", true, &ids, 2).unwrap());
        let _ = fs::remove_dir_all(dir);
    }
}
