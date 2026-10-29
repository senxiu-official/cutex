//! Redaction of Kimi account credentials from cutex's own records (r266 B1).
//!
//! Kimi Code, like the official CLI (and Claude Code or Codex), can read its own
//! credential files: an agent's `KIMI_CODE_HOME/credentials/` links to the
//! account's, and a tool call may print them. cutex does not hide them from the
//! model; it keeps them out of everything cutex records or serves (events,
//! queries, timeline, offline history, re-projections, logs). The adapter and
//! the history / session-operation readers do that themselves
//! (`components/kimi-runtime/src/redaction.mjs`) from two sources cutex hands
//! them as paths, never as values:
//!
//! - the agent's linked `credentials/` directory: the current access and
//!   refresh tokens, re-read whenever a refresh rewrites the file (the adapter
//!   also keeps the values it saw before a change, bounded);
//! - the account's redaction **ledger**: SHA-256 fingerprints (`length`,
//!   `sha256`) of every token value cutex or an adapter has seen, bounded to
//!   [`LEDGER_MAX`]. A token-shaped run of text whose length and digest match
//!   is redacted, so rotated values stay redacted after the files no longer
//!   hold them (offline reads, re-projection after a later refresh).
//!
//! A token-shaped run is a maximal run of `[A-Za-z0-9._~+/=-]` or of the
//! narrower `[A-Za-z0-9._~-]`; a rotated value glued to other token characters
//! on both sides is not found by its fingerprint (current values are redacted
//! literally wherever they appear).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::Digest;

pub const REDACTED: &str = "[REDACTED]";
/// Shorter values are never fingerprinted (too many false positives).
pub const MIN_FINGERPRINT_LEN: usize = 8;
/// Fingerprints kept per account (a refresh adds two).
pub const LEDGER_MAX: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Fingerprint {
    pub length: usize,
    pub sha256: String,
}

impl Fingerprint {
    pub fn of(value: &str) -> Option<Self> {
        (value.len() >= MIN_FINGERPRINT_LEN).then(|| Self { length: value.len(), sha256: format!("{:x}", sha2::Sha256::digest(value.as_bytes())) })
    }
}

pub fn fingerprints<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<Fingerprint> {
    values.into_iter().filter_map(Fingerprint::of).collect()
}

/// Paths the adapter (launch file `redaction`) and the readers
/// (`KIMI_CUTEX_REDACT_SOURCES`) read redaction values from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactionPaths {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger: Option<PathBuf>,
}

impl RedactionPaths {
    pub fn is_empty(&self) -> bool {
        self.credentials_dir.is_none() && self.ledger.is_none()
    }
}

/// `<runtime>/kimi/redaction/<account>.json` (outside the account home, so no
/// Kimi process sees it through its links; it holds digests only).
pub fn ledger_path(account_id: &str) -> anyhow::Result<PathBuf> {
    crate::profiles::kimi_accounts::validate_id(account_id)?;
    Ok(crate::config::paths::runtime_dir()?.join("kimi").join("redaction").join(format!("{account_id}.json")))
}

fn ledger_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

/// The fingerprints of the account's ledger (empty when unreadable).
pub fn ledger(account_id: &str) -> BTreeSet<Fingerprint> {
    ledger_path(account_id)
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|raw| serde_json::from_slice::<Vec<Fingerprint>>(&raw).ok())
        .map(|list| list.into_iter().collect())
        .unwrap_or_default()
}

/// Adds the values' fingerprints to the account's ledger (oldest dropped past
/// [`LEDGER_MAX`]). Adapters merge into the same file (atomic replace).
pub fn remember<'a>(account_id: &str, values: impl IntoIterator<Item = &'a str>) {
    let _guard = ledger_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Ok(path) = ledger_path(account_id) else { return };
    let mut list: Vec<Fingerprint> = std::fs::read(&path).ok().and_then(|raw| serde_json::from_slice(&raw).ok()).unwrap_or_default();
    let before = list.len();
    for fingerprint in values.into_iter().filter_map(Fingerprint::of) {
        if !list.contains(&fingerprint) {
            list.push(fingerprint);
        }
    }
    if list.len() == before {
        return;
    }
    if list.len() > LEDGER_MAX {
        list.drain(..list.len() - LEDGER_MAX);
    }
    if let Some(parent) = path.parent() {
        let _ = crate::management::client_tokens::ensure_private_dir(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(&list) {
        if let Err(error) = crate::config::atomic::write_private_bytes_atomic(&path, &bytes) {
            eprintln!("kimi: the redaction ledger of {account_id} was not updated: {error:#}");
        }
    }
}

/// `[A-Za-z0-9._~+/=-]` (base64, JWT, URL-safe) and the narrower
/// `[A-Za-z0-9._~-]` (a token after `=` or inside a path).
const RUN_CLASSES: [fn(u8) -> bool; 2] = [
    |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-'),
    |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'),
];

fn fingerprint_ranges(text: &[u8], fingerprints: &BTreeSet<Fingerprint>) -> Vec<(usize, usize)> {
    if fingerprints.is_empty() {
        return Vec::new();
    }
    let lengths: BTreeSet<usize> = fingerprints.iter().map(|fp| fp.length).collect();
    let mut ranges = Vec::new();
    for class in RUN_CLASSES {
        let mut start = None;
        for index in 0..=text.len() {
            let inside = index < text.len() && class(text[index]);
            match (inside, start) {
                (true, None) => start = Some(index),
                (false, Some(begin)) => {
                    start = None;
                    let length = index - begin;
                    if lengths.contains(&length) {
                        let sha256 = format!("{:x}", sha2::Sha256::digest(&text[begin..index]));
                        if fingerprints.contains(&Fingerprint { length, sha256 }) {
                            ranges.push((begin, index));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    ranges
}

/// Redacts literal values and fingerprinted runs (the same rules as the
/// adapter's `redactText`).
pub fn redact_str(text: &str, literals: &[String], fingerprints: &BTreeSet<Fingerprint>) -> String {
    let bytes = text.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for literal in literals.iter().map(String::as_bytes).filter(|value| !value.is_empty()) {
        let mut from = 0;
        while let Some(position) = bytes[from..].windows(literal.len()).position(|window| window == literal) {
            ranges.push((from + position, from + position + literal.len()));
            from += position + literal.len();
            if from + literal.len() > bytes.len() {
                break;
            }
        }
    }
    ranges.extend(fingerprint_ranges(bytes, fingerprints));
    if ranges.is_empty() {
        return text.to_string();
    }
    ranges.sort();
    let mut out = Vec::with_capacity(bytes.len());
    let (mut at, mut end) = (0usize, None::<usize>);
    for (start, stop) in ranges {
        match end {
            Some(current) if start < current => end = Some(current.max(stop)),
            _ => {
                if let Some(current) = end {
                    at = current;
                }
                out.extend_from_slice(&bytes[at..start]);
                out.extend_from_slice(REDACTED.as_bytes());
                end = Some(stop);
            }
        }
    }
    out.extend_from_slice(&bytes[end.unwrap_or(at)..]);
    // Every boundary is an ASCII byte (a literal's or a run's edge), so the result stays UTF-8.
    String::from_utf8(out).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJjYW5hcnkifQ.c2lnbmF0dXJlLWNhbmFyeQ";

    #[test]
    fn fingerprints_find_a_token_in_json_text_and_after_bearer_without_knowing_it() {
        let fps = fingerprints([TOKEN]);
        for text in [
            format!("{{\"access_token\": \"{TOKEN}\", \"x\": 1}}"),
            format!("Authorization: Bearer {TOKEN}\n"),
            format!("token={TOKEN}&next=1"),
            format!("/tmp/{TOKEN}/x"),
            TOKEN.to_string(),
        ] {
            let out = redact_str(&text, &[], &fps);
            assert!(!out.contains(TOKEN), "{out}");
            assert!(out.contains(REDACTED), "{out}");
        }
        // A different token of the same length is kept.
        let other = TOKEN.replace('c', "d");
        assert_eq!(redact_str(&other, &[], &fps), other);
        assert!(fingerprints(["short"]).is_empty());
    }

    #[test]
    fn literals_and_fingerprints_merge_and_keep_utf8() {
        let fps = fingerprints(["rotated-refresh-token-0001"]);
        let text = "数据 sk-live-key-123456 和 \"rotated-refresh-token-0001\" 结束 sk-live-key-123456";
        let out = redact_str(text, &["sk-live-key-123456".into()], &fps);
        assert_eq!(out, "数据 [REDACTED] 和 \"[REDACTED]\" 结束 [REDACTED]");
    }

    #[test]
    fn the_ledger_is_bounded_private_and_holds_digests_only() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let id = "ka-ledgertest";
        let values: Vec<String> = (0..(LEDGER_MAX + 6)).map(|n| format!("fake-ledger-token-{n:04}")).collect();
        remember(id, values.iter().map(String::as_str));
        remember(id, ["fake-ledger-token-0003"]);
        let ledger = ledger(id);
        assert_eq!(ledger.len(), LEDGER_MAX);
        assert!(ledger.contains(&Fingerprint::of(values.last().unwrap()).unwrap()));
        assert!(!ledger.contains(&Fingerprint::of(&values[0]).unwrap()), "oldest dropped");
        let raw = std::fs::read_to_string(ledger_path(id).unwrap()).unwrap();
        assert!(!raw.contains("fake-ledger-token"), "{raw}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(ledger_path(id).unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(ledger_path("../x").is_err());
    }
}
