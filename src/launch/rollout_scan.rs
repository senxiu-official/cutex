//! Backward line scans of a native rollout shared by `native_plan` and
//! `native_token_usage`, incremental across appends (r252).
//!
//! An image-generating agent's rollout grows by megabytes per turn; every
//! bootstrap after a change rescanned up to [`SCAN_BUDGET_BYTES`] of it from
//! the end (0.6 s), rebuilding multi-MB lines quadratically. Now each result
//! is kept with a checkpoint (the rollout length at a line end plus its last
//! bytes); when the rollout only grew, just the appended lines are scanned,
//! newest first. If they settle the result it is final; if they hold nothing
//! that matters for older lines (`LineScanner::neutral`) the checkpoint's
//! result stands; otherwise the scan simply continues into the older bytes
//! and base segments as a full scan would. A shrunk or rewritten rollout
//! (checkpoint bytes differ) is scanned in full.
//!
//! The byte budget bounds each scan, not the history: a result found before
//! an append is kept even when the appended bytes push it past the budget.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::ensure;

/// Upper bound on rollout bytes read per derivation (all segments).
pub const SCAN_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const CHUNK_BYTES: usize = 256 * 1024;
const CACHE_ENTRIES: usize = 128;
/// Bytes before a checkpoint compared to detect a rewritten rollout.
const TAIL_BYTES: u64 = 256;

pub(crate) trait LineScanner: Default {
    /// Consumes one line (newest first); true once the result is complete.
    fn line(&mut self, bytes: &[u8]) -> bool;
    /// True while nothing seen changes how older lines are read (the state
    /// equals `Default`).
    fn neutral(&self) -> bool;
}

type Key = (PathBuf, String);
type Stamp = (u64, Option<SystemTime>);

pub(crate) struct Checkpoint<R> {
    length: u64,
    tail: Vec<u8>,
    result: R,
}

pub(crate) struct Entry<R> {
    stamp: Stamp,
    result: R,
    checkpoint: Option<Checkpoint<R>>,
}

pub(crate) type Cache<R> = OnceLock<Mutex<HashMap<Key, Entry<R>>>>;

/// The scan result for `current` (cached per rollout until it changes).
pub(crate) fn latest<S: LineScanner, R: Clone>(
    cache: &'static Cache<R>,
    home: &Path,
    current: &Path,
    thread_id: &str,
    budget: u64,
    finish: impl Fn(S) -> R,
) -> anyhow::Result<R> {
    let cache = cache.get_or_init(Default::default);
    let metadata = std::fs::metadata(current)?;
    let stamp = (metadata.len(), metadata.modified().ok());
    let key = (current.to_path_buf(), thread_id.to_string());
    let checkpoint = match cache.lock() {
        Ok(cache) => match cache.get(&key) {
            Some(entry) if entry.stamp == stamp => return Ok(entry.result.clone()),
            Some(entry) => entry.checkpoint.as_ref().map(|checkpoint| {
                (checkpoint.length, checkpoint.tail.clone(), checkpoint.result.clone())
            }),
            None => None,
        },
        Err(_) => None,
    };
    let length = stamp.0;
    let mut file = std::fs::File::open(current)?;
    let mut scanner = S::default();
    let mut budget = budget;
    let resumed = match checkpoint {
        Some((from, tail, result)) if from <= length && read_tail(&mut file, from)? == tail => {
            Some((from, result))
        }
        _ => None,
    };
    let result = match resumed {
        Some((from, result)) => {
            if scan_segment(&mut file, from, length, &mut scanner, &mut budget)? {
                finish(scanner)
            } else if scanner.neutral() {
                result
            } else {
                scan_chain(home, current, &mut file, from, &mut scanner, &mut budget)?;
                finish(scanner)
            }
        }
        None => {
            scan_chain(home, current, &mut file, length, &mut scanner, &mut budget)?;
            finish(scanner)
        }
    };
    let at_line_end = length == 0 || {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(length - 1))?;
        file.read_exact(&mut last)?;
        last[0] == b'\n'
    };
    let new_checkpoint = if at_line_end {
        Some(Checkpoint { length, tail: read_tail(&mut file, length)?, result: result.clone() })
    } else {
        None
    };
    if let Ok(mut cache) = cache.lock() {
        if cache.len() >= CACHE_ENTRIES && !cache.contains_key(&key) {
            cache.clear();
        }
        let previous = cache.remove(&key).and_then(|entry| entry.checkpoint);
        cache.insert(
            key,
            Entry { stamp, result: result.clone(), checkpoint: new_checkpoint.or(previous) },
        );
    }
    Ok(result)
}

fn read_tail(file: &mut std::fs::File, end: u64) -> anyhow::Result<Vec<u8>> {
    let start = end.saturating_sub(TAIL_BYTES);
    let mut tail = vec![0u8; (end - start) as usize];
    if file.metadata()?.len() < end {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut tail)?;
    Ok(tail)
}

/// Feeds `current[..end]` and then its base segments (`history_base`) to
/// `scanner`, newest line first, until it is complete or the budget is spent.
fn scan_chain<S: LineScanner>(
    home: &Path,
    current: &Path,
    file: &mut std::fs::File,
    end: u64,
    scanner: &mut S,
    budget: &mut u64,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::from([current.to_path_buf()]);
    if scan_segment(file, 0, end, scanner, budget)? {
        return Ok(());
    }
    let mut segment = super::native_history::history_base(home, current)?;
    while let Some((path, end)) = segment {
        ensure!(seen.insert(path.clone()), "cyclic native history reference");
        let mut file = std::fs::File::open(&path)?;
        if scan_segment(&mut file, 0, end, scanner, budget)? {
            break;
        }
        segment = super::native_history::history_base(home, &path)?;
    }
    Ok(())
}

/// Feeds `file[start..end]` (`start` at a line start) to `scanner` newest
/// line first. Returns true when the scan is complete (or the budget is
/// spent). A line spanning chunks is joined once (its pieces are kept in
/// reading order, not re-copied per chunk).
pub(crate) fn scan_segment<S: LineScanner>(
    file: &mut std::fs::File,
    start: u64,
    end: u64,
    scanner: &mut S,
    budget: &mut u64,
) -> anyhow::Result<bool> {
    let end = end.min(file.metadata()?.len());
    let start = start.min(end);
    let mut position = end;
    // Pieces of the (possibly partial) line ending at the scanned tail, newest
    // piece first.
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut chunk = vec![0u8; CHUNK_BYTES];
    while position > start {
        if *budget == 0 {
            return Ok(true);
        }
        let size = ((position - start).min(CHUNK_BYTES as u64)).min(*budget) as usize;
        position -= size as u64;
        *budget -= size as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut chunk[..size])?;
        let mut tail = size;
        while let Some(newline) = chunk[..tail].iter().rposition(|&b| b == b'\n') {
            let line = join(&chunk[newline + 1..tail], &mut pieces);
            if scanner.line(&line) {
                return Ok(true);
            }
            tail = newline;
        }
        pieces.push(chunk[..tail].to_vec());
    }
    Ok(scanner.line(&join(&[], &mut pieces)))
}

fn join(head: &[u8], pieces: &mut Vec<Vec<u8>>) -> Vec<u8> {
    let mut line = Vec::with_capacity(head.len() + pieces.iter().map(Vec::len).sum::<usize>());
    line.extend_from_slice(head);
    for piece in pieces.drain(..).rev() {
        line.extend_from_slice(&piece);
    }
    line
}

/// A full scan of `current[..end]` and its base segments (no cache).
#[cfg(test)]
pub(crate) fn scan_full<S: LineScanner>(home: &Path, current: &Path, end: u64, budget: u64) -> anyhow::Result<S> {
    let mut scanner = S::default();
    let mut budget = budget;
    scan_chain(home, current, &mut std::fs::File::open(current)?, end, &mut scanner, &mut budget)?;
    Ok(scanner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Plan-like: `out:<id>` accepts the older `call:<id>:<value>`, which is
    /// complete at the next older `turn:<t>`.
    #[derive(Default, Debug)]
    struct Probe {
        accepted: Vec<String>,
        found: Option<(String, Option<String>)>,
    }

    impl LineScanner for Probe {
        fn line(&mut self, bytes: &[u8]) -> bool {
            let text = String::from_utf8_lossy(bytes);
            let mut parts = text.trim().split(':');
            match (parts.next(), parts.next(), parts.next()) {
                (Some("out"), Some(id), None) if self.found.is_none() => self.accepted.push(id.to_string()),
                (Some("call"), Some(id), Some(value)) if self.found.is_none() && self.accepted.iter().any(|a| a == id) => {
                    self.found = Some((value.to_string(), None));
                }
                (Some("turn"), Some(turn), None) => {
                    if let Some(found) = &mut self.found {
                        found.1 = Some(turn.to_string());
                        return true;
                    }
                }
                _ => {}
            }
            false
        }

        fn neutral(&self) -> bool {
            self.accepted.is_empty() && self.found.is_none()
        }
    }

    fn finish(probe: Probe) -> Option<(String, Option<String>)> {
        probe.found
    }

    struct Home(PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rollout() -> (Home, PathBuf) {
        let home = std::env::temp_dir().join(format!("cutex-rollout-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let path = home.join("sessions").join("rollout.jsonl");
        std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n").unwrap();
        (Home(home), path)
    }

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    fn full(home: &Path, path: &Path, budget: u64) -> Option<(String, Option<String>)> {
        let length = std::fs::metadata(path).unwrap().len();
        scan_full::<Probe>(home, path, length, budget).unwrap().found
    }

    #[test]
    fn appends_are_scanned_alone_and_agree_with_a_full_scan() {
        static CACHE: Cache<Option<(String, Option<String>)>> = OnceLock::new();
        let (home, path) = rollout();
        let latest = |budget| latest(&CACHE, &home.0, &path, "t", budget, finish).unwrap();
        for step in [
            "turn:1\ncall:a:1\nout:a\n",
            "noise\nnoise\n",
            "turn:2\n",
            "call:b:2\n",
            // Accepts a call before the checkpoint: the scan continues into older bytes.
            "out:b\n",
            "turn:3\ncall:c:3\nout",
            // The rest of a line written in two parts.
            ":c\n",
            "out:zzz\nnoise\n",
        ] {
            append(&path, step);
            assert_eq!(latest(SCAN_BUDGET_BYTES), full(&home.0, &path, SCAN_BUDGET_BYTES), "after {step:?}");
            assert_eq!(latest(SCAN_BUDGET_BYTES), full(&home.0, &path, SCAN_BUDGET_BYTES), "cached after {step:?}");
        }
        assert_eq!(latest(SCAN_BUDGET_BYTES), Some(("3".into(), Some("3".into()))));
    }

    #[test]
    fn only_the_appended_bytes_are_read() {
        static CACHE: Cache<Option<(String, Option<String>)>> = OnceLock::new();
        let (home, path) = rollout();
        append(&path, "turn:1\ncall:a:1\nout:a\n");
        let found = Some(("1".to_string(), Some("1".to_string())));
        assert_eq!(latest(&CACHE, &home.0, &path, "t", 1024, finish).unwrap(), found);
        // 4 KiB of lines that do not matter: a full scan with a budget the size
        // of the append no longer reaches the call; the incremental scan reads
        // just the append and keeps the result.
        append(&path, &"noise\n".repeat(700));
        let appended = 6 * 700;
        assert_eq!(latest(&CACHE, &home.0, &path, "t", appended, finish).unwrap(), found);
        assert_eq!(full(&home.0, &path, appended), None, "a full scan with the same budget misses it");
    }

    #[test]
    fn a_rewritten_rollout_is_scanned_in_full() {
        static CACHE: Cache<Option<(String, Option<String>)>> = OnceLock::new();
        let (home, path) = rollout();
        append(&path, "turn:1\ncall:a:1\nout:a\nnoise\n");
        assert!(latest(&CACHE, &home.0, &path, "t", SCAN_BUDGET_BYTES, finish).unwrap().is_some());
        // Same prefix length, other bytes, then longer: not an append.
        let meta = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n";
        std::fs::write(&path, format!("{meta}turn:1\ncall:a:1\nout:b\nnoise\nnoise\n")).unwrap();
        assert_eq!(latest(&CACHE, &home.0, &path, "t", SCAN_BUDGET_BYTES, finish).unwrap(), None);
        // Shorter.
        std::fs::write(&path, format!("{meta}turn:9\ncall:z:9\nout:z\n")).unwrap();
        assert_eq!(
            latest(&CACHE, &home.0, &path, "t", SCAN_BUDGET_BYTES, finish).unwrap(),
            Some(("9".into(), Some("9".into())))
        );
    }

    #[test]
    fn lines_over_many_chunks_are_joined_once_in_order() {
        let (home, path) = rollout();
        let value: String = (0..CHUNK_BYTES * 9 + 5).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        append(&path, &format!("turn:1\ncall:a:{value}\nout:a\n"));
        let started = std::time::Instant::now();
        let found = full(&home.0, &path, SCAN_BUDGET_BYTES).unwrap();
        assert_eq!(found.0, value);
        assert_eq!(found.1.as_deref(), Some("1"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
