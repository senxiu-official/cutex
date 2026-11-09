//! Surface trace: what the daemon published (`cutex/input/stateChanged`,
//! `cutex/session/deliveryChanged`) and what the reads returned
//! (`cutex/input/list`, `cutex/userInput/queue/list`), with wall-clock
//! timestamps. Saved as JSONL; the desktop (L3) replays the same files.
//! `assert_surface_trace` is the 054 §5.2 common assertion.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub const STATE_CHANGED: &str = "cutex/input/stateChanged";
pub const DELIVERY_CHANGED: &str = "cutex/session/deliveryChanged";
/// §5.2: every record settles within its last deadline + 5 s.
pub const SETTLE_SLACK_MS: i64 = 5_000;
/// `uncertain` is "settled" (cutex does nothing more on its own, §2.3).
pub const SETTLED_STATES: &[&str] = &["inContext", "failed", "rejected", "discarded", "superseded", "uncertain"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TraceEntry {
    /// Milliseconds since the Unix epoch, taken when the recorder saw it.
    pub at_ms: i64,
    /// `session` / `host` (event streams), `snapshot` (a read), `marker`.
    pub source: String,
    /// `stateChanged`, `deliveryChanged`, `inputList`, `inputGet`, `queueList`,
    /// `native`, `legacy`, `restart`, `note`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutex_session_id: Option<String>,
    /// Stream sequence of the envelope (events only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    /// Event params, read result or marker detail.
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trace {
    pub entries: Vec<TraceEntry>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Trace {
    pub fn push(&mut self, entry: TraceEntry) {
        self.entries.push(entry);
    }

    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry).expect("trace entry serializes"));
            out.push('\n');
        }
        out
    }

    pub fn from_jsonl(text: &str) -> Self {
        let entries = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("trace line"))
            .collect();
        Self { entries }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_jsonl())
    }

    /// `stateChanged` records of one input as seen on one stream, in order.
    pub fn records(&self, source: &str, input_id: &str) -> Vec<Value> {
        self.entries
            .iter()
            .filter(|e| e.source == source && e.kind == "stateChanged")
            .filter_map(|e| e.payload.get("record"))
            .filter(|r| r["inputId"] == input_id)
            .cloned()
            .collect()
    }

    /// The newest version of one input: highest `rev` within the newest
    /// `ledgerEpoch` (a rebuilt ledger starts revs again, §2.12).
    pub fn latest(&self, input_id: &str) -> Option<Value> {
        let mut epochs: Vec<String> = Vec::new();
        let mut best: Option<(usize, Value)> = None;
        for entry in &self.entries {
            let mut records: Vec<&Value> = snapshot_records(entry);
            if entry.kind == "stateChanged" {
                records.extend(entry.payload.get("record"));
            }
            let epoch = entry.payload["ledgerEpoch"].as_str().unwrap_or("");
            for record in records {
                if record["inputId"] != input_id {
                    continue;
                }
                if !epoch.is_empty() && !epochs.iter().any(|e| e == epoch) {
                    epochs.push(epoch.to_string());
                }
                let order = epochs.iter().position(|e| e == epoch).unwrap_or(0);
                if best.as_ref().is_none_or(|(o, b)| order > *o || (order == *o && rev(record) > rev(b))) {
                    best = Some((order, record.clone()));
                }
            }
        }
        best.map(|(_, record)| record)
    }

    /// Every record version in the trace (events and reads).
    pub fn all_records(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend(snapshot_records(entry).into_iter().cloned());
            if entry.kind == "stateChanged" {
                if let Some(record) = entry.payload.get("record") {
                    out.push(record.clone());
                }
            }
        }
        out
    }

    /// The §5.2 state sequence of one input on the session stream, as labels
    /// (see [`label`]), consecutive duplicates collapsed.
    pub fn path(&self, input_id: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for record in self.records("session", input_id) {
            let label = label(&record);
            if out.last() != Some(&label) {
                out.push(label);
            }
        }
        out
    }

    pub fn surfaces(&self, input_id: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for record in self.records("session", input_id) {
            let surface = record["surface"].as_str().unwrap_or("?").to_string();
            if out.last() != Some(&surface) {
                out.push(surface);
            }
        }
        out
    }

    pub fn input_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for record in self.all_records() {
            if let Some(id) = record["inputId"].as_str() {
                if !out.iter().any(|known| known == id) {
                    out.push(id.to_string());
                }
            }
        }
        out
    }

    pub fn deliveries(&self, source: &str) -> Vec<Value> {
        self.entries
            .iter()
            .filter(|e| e.source == source && e.kind == "deliveryChanged")
            .filter_map(|e| e.payload.get("delivery").cloned())
            .collect()
    }

    pub fn end_ms(&self) -> i64 {
        self.entries.iter().map(|e| e.at_ms).max().unwrap_or(0)
    }
}

/// `state{reason}[stage](evidence)`, each part only when present.
pub fn label(record: &Value) -> String {
    let mut out = record["state"].as_str().unwrap_or("?").to_string();
    if let Some(reason) = record["reason"].as_str() {
        out.push_str(&format!("{{{reason}}}"));
    }
    if let Some(stage) = record["stage"].as_str() {
        out.push_str(&format!("[{stage}]"));
    }
    if let Some(evidence) = record["evidence"].as_str() {
        out.push_str(&format!("({evidence})"));
    }
    out
}

/// A pattern element matches a label exactly, or as its prefix up to one of
/// the `{[(` qualifiers (`"inContext"` matches `"inContext(turnStartInput)"`).
pub fn label_matches(pattern: &str, label: &str) -> bool {
    label == pattern
        || label
            .strip_prefix(pattern)
            .is_some_and(|rest| rest.starts_with(['{', '[', '(']))
}

pub fn path_matches(patterns: &[&str], labels: &[String]) -> bool {
    patterns.len() == labels.len() && patterns.iter().zip(labels).all(|(p, l)| label_matches(p, l))
}

fn rev(record: &Value) -> u64 {
    record["rev"].as_u64().unwrap_or(0)
}

fn snapshot_records(entry: &TraceEntry) -> Vec<&Value> {
    let field = match entry.kind.as_str() {
        "inputList" | "inputGet" => "records",
        "queueList" => "inputs",
        _ => return Vec::new(),
    };
    entry.payload.get(field).and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default()
}

fn deadline_ms(record: &Value) -> Option<i64> {
    record["deadline"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.timestamp_millis())
}

pub fn is_settled(state: &str) -> bool {
    SETTLED_STATES.contains(&state)
}

/// §2.5 / invariant 3: `nextUp → timeline`, `pendingNotes → timeline`,
/// `* → none`; the only way back is the owner's own retry of an
/// `uncertain` / `failed` record (`→ waiting{ownerRetry}`, surface by the
/// creation rule again).
pub fn surface_move_allowed(from: &str, to: &str, prev_state: &str, next_state: &str, next_reason: Option<&str>) -> bool {
    if from == to || to == "none" {
        return true;
    }
    if to == "timeline" && matches!(from, "nextUp" | "pendingNotes") {
        return true;
    }
    next_state == "waiting" && next_reason == Some("ownerRetry") && matches!(prev_state, "uncertain" | "failed")
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AssertOptions {
    /// Every record seen must be settled when the trace ends.
    pub require_settled_at_end: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraceSummary {
    pub inputs: usize,
    pub events: usize,
    pub snapshots: usize,
}

struct Version {
    /// The `ledgerEpoch` this version belongs to (`rev` restarts in a new one).
    epoch: String,
    rev: u64,
    state: String,
    surface: String,
    deadline_ms: Option<i64>,
    at_ms: i64,
}

/// The §5.2 common assertions. Returns every violation, not just the first.
pub fn check_surface_trace(trace: &Trace, options: AssertOptions) -> Result<TraceSummary, Vec<String>> {
    let mut errors = Vec::new();
    let mut summary = TraceSummary::default();
    let mut known: HashMap<(String, String), Version> = HashMap::new();
    // (stream, session) -> (epoch, last stateRevision)
    let mut revisions: HashMap<(String, String), (String, u64)> = HashMap::new();
    // (stream, session, ledgerEpoch, input) -> last rev on that stream. A
    // new epoch (ledger lost, §2.12; announced by cutex/stream/reset) starts
    // every record's rev again, so the epoch is part of the key.
    let mut stream_revs: HashMap<(String, String, String, String), u64> = HashMap::new();
    // Epochs per session in first-seen order (§2.8: a different epoch means
    // "drop the session's input state and read in full").
    let mut epochs = Epochs::default();
    // A stream that was resubscribed from "now" (cursor expired, stream reset)
    // may skip revisions once: the client fills them with `sinceRevision`.
    let mut resubscribed: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    // (stream, session) -> (stateRevision, inputs) of the stateChanged run
    // at the last revision (an open batch).
    let mut batches: HashMap<(String, String), (u64, std::collections::HashSet<String>)> = HashMap::new();
    // The outbox can append again after a publisher dies before deleting its
    // rows. Only the same complete params on the same stream are a replay;
    // envelope sequence/time may differ. Keep every identity, including older
    // batch members, so a replayed prefix cannot move the revision head back.
    let mut published: HashMap<(String, String, String, u64, String, Option<String>), Vec<u8>> = HashMap::new();

    for (index, entry) in trace.entries.iter().enumerate() {
        let at = format!("#{index} {}/{}", entry.source, entry.kind);
        match entry.kind.as_str() {
            "stateChanged" | "deliveryChanged" => {
                summary.events += 1;
                let p = &entry.payload;
                let Some(session) = p["cutexSessionId"].as_str() else {
                    errors.push(format!("{at}: event without cutexSessionId"));
                    continue;
                };
                let epoch = p["ledgerEpoch"].as_str().unwrap_or("").to_string();
                let Some(revision) = p["stateRevision"].as_u64() else {
                    errors.push(format!("{at}: event without stateRevision"));
                    continue;
                };
                let input = (entry.kind == "stateChanged").then(|| p["record"]["inputId"].as_str()).flatten();
                let identity = (entry.source.clone(), session.to_string(), epoch.clone(), revision,
                    entry.kind.clone(), input.map(str::to_string));
                // Trace payloads are parsed JSON: compare all serialized params,
                // never just stateRevision/record.rev or selected state fields.
                let bytes = serde_json::to_vec(p).expect("trace payload serializes");
                if let Some(before) = published.get(&identity) {
                    if *before != bytes {
                        errors.push(format!("{at}: {session} conflicting replay at stateRevision {revision}"));
                    }
                    continue;
                }
                published.insert(identity, bytes);
                let key = (entry.source.clone(), session.to_string());
                let jump = resubscribed.remove(&key);
                // A batch write (`input_ledger::record_batch`, e.g. queue merge)
                // publishes one stateChanged per changed input, all at the
                // batch's one stateRevision, back to back: the same revision
                // again is fine for a DISTINCT input of the open batch. A
                // deliveryChanged opens a batch too: the records whose
                // `actions` follow the delivery (`interruptSend`), and the
                // first transition of an interruptSend written with its
                // delivery (`transition_batch_with_delivery`), come right after
                // it at its revision (§2.8: clients apply them by `rev`).
                let same_epoch = revisions.get(&key).is_some_and(|(last_epoch, _)| *last_epoch == epoch);
                let batch_member = same_epoch
                    && input.is_some_and(|input| {
                        batches.get(&key).is_some_and(|(r, inputs)| *r == revision && !inputs.contains(input))
                    });
                match input {
                    Some(input) if batch_member => {
                        batches.get_mut(&key).unwrap().1.insert(input.to_string());
                    }
                    Some(input) => {
                        batches.insert(key.clone(), (revision, std::collections::HashSet::from([input.to_string()])));
                    }
                    None => {
                        batches.insert(key.clone(), (revision, std::collections::HashSet::new()));
                    }
                }
                match revisions.get(&key) {
                    Some(_) if batch_member => {}
                    Some((last_epoch, last)) if *last_epoch == epoch => {
                        if revision <= *last {
                            errors.push(format!("{at}: {session} stateRevision {revision} not above {last}"));
                        } else if revision != last + 1 && !jump {
                            errors.push(format!("{at}: {session} stateRevision hole {last} -> {revision}"));
                        }
                    }
                    _ => {}
                }
                revisions.insert(key, (epoch.clone(), revision));
                if entry.kind == "stateChanged" {
                    let record = &p["record"];
                    let Some(input) = record["inputId"].as_str() else {
                        errors.push(format!("{at}: stateChanged without record.inputId"));
                        continue;
                    };
                    if record["cutexSessionId"].as_str() != Some(session) {
                        errors.push(format!("{at}: {input} record session differs from event session"));
                    }
                    if record["stateRevision"].as_u64() != Some(revision) {
                        errors.push(format!("{at}: {input} record.stateRevision {} != event {revision}", record["stateRevision"]));
                    }
                    let stream_key = (entry.source.clone(), session.to_string(), epoch.clone(), input.to_string());
                    let r = rev(record);
                    if let Some(last) = stream_revs.get(&stream_key) {
                        if r <= *last {
                            errors.push(format!("{at}: {input} rev {r} not above {last} on the {} stream", entry.source));
                        }
                    }
                    stream_revs.insert(stream_key, r);
                    epochs.see(session, &epoch);
                    apply(&mut known, &mut errors, &epochs, &at, session, &epoch, record, entry.at_ms);
                }
            }
            "resubscribed" | "streamReset" => {
                let session = entry.cutex_session_id.clone().filter(|_| entry.source == "session").unwrap_or_default();
                if session.is_empty() {
                    for (source, s) in revisions.keys().filter(|(source, _)| *source == entry.source) {
                        resubscribed.insert((source.clone(), s.clone()));
                    }
                } else {
                    resubscribed.insert((entry.source.clone(), session));
                }
            }
            "inputList" | "inputGet" | "queueList" => {
                summary.snapshots += 1;
                let records = snapshot_records(entry);
                let mut seen: Vec<&str> = Vec::new();
                let snapshot_revision = entry.payload["stateRevision"].as_u64();
                let snapshot_epoch = entry.payload["ledgerEpoch"].as_str().unwrap_or("").to_string();
                for record in records {
                    let Some(input) = record["inputId"].as_str() else {
                        errors.push(format!("{at}: record without inputId"));
                        continue;
                    };
                    if seen.contains(&input) {
                        errors.push(format!("{at}: {input} listed twice in one read"));
                    }
                    seen.push(input);
                    if let (Some(read), Some(own)) = (snapshot_revision, record["stateRevision"].as_u64()) {
                        if own > read {
                            errors.push(format!("{at}: {input} stateRevision {own} newer than the read's {read}"));
                        }
                    }
                    if entry.kind == "queueList" {
                        let surface = record["surface"].as_str().unwrap_or("?");
                        let state = record["state"].as_str().unwrap_or("?");
                        if !matches!(surface, "nextUp" | "pendingNotes") || is_settled(state) {
                            errors.push(format!("{at}: queue/list inputs holds {input} on {surface} in {state}"));
                        }
                    }
                    let session = record["cutexSessionId"].as_str().or(entry.cutex_session_id.as_deref()).unwrap_or("");
                    epochs.see(session, &snapshot_epoch);
                    apply(&mut known, &mut errors, &epochs, &at, session, &snapshot_epoch, record, entry.at_ms);
                }
            }
            _ => {}
        }
    }

    let end = trace.end_ms();
    for ((session, input), version) in &known {
        // A lost ledger's records are gone (§2.12): only the latest epoch counts.
        if is_settled(&version.state) || !epochs.is_latest(session, &version.epoch) {
            continue;
        }
        if let Some(deadline) = version.deadline_ms {
            if end > deadline + SETTLE_SLACK_MS {
                errors.push(format!(
                    "{session}/{input}: still {} {} ms after its deadline",
                    version.state,
                    end - deadline
                ));
                continue;
            }
        }
        if options.require_settled_at_end {
            errors.push(format!("{session}/{input}: unsettled at trace end ({} on {})", version.state, version.surface));
        }
    }
    summary.inputs = known.len();
    if errors.is_empty() { Ok(summary) } else { Err(errors) }
}

/// Each session's `ledgerEpoch`s in the order the trace first shows them.
#[derive(Default)]
struct Epochs(HashMap<String, Vec<String>>);

impl Epochs {
    fn see(&mut self, session: &str, epoch: &str) {
        let seen = self.0.entry(session.to_string()).or_default();
        if !epoch.is_empty() && !seen.iter().any(|e| e == epoch) {
            seen.push(epoch.to_string());
        }
    }

    /// Order of `epoch` for `session` (an unknown or empty epoch is 0).
    fn order(&self, session: &str, epoch: &str) -> usize {
        self.0.get(session).and_then(|seen| seen.iter().position(|e| e == epoch)).unwrap_or(0)
    }

    fn is_latest(&self, session: &str, epoch: &str) -> bool {
        self.0.get(session).and_then(|seen| seen.last()).is_none_or(|last| epoch.is_empty() || last == epoch)
    }
}

#[allow(clippy::too_many_arguments)]
fn apply(
    known: &mut HashMap<(String, String), Version>,
    errors: &mut Vec<String>,
    epochs: &Epochs,
    at: &str,
    session: &str,
    epoch: &str,
    record: &Value,
    at_ms: i64,
) {
    let input = record["inputId"].as_str().unwrap_or("?").to_string();
    let next = Version {
        epoch: epoch.to_string(),
        rev: rev(record),
        state: record["state"].as_str().unwrap_or("?").to_string(),
        surface: record["surface"].as_str().unwrap_or("?").to_string(),
        deadline_ms: deadline_ms(record),
        at_ms,
    };
    let key = (session.to_string(), input.clone());
    let Some(prev) = known.get(&key) else {
        known.insert(key, next);
        return;
    };
    if prev.epoch != next.epoch && !prev.epoch.is_empty() && !next.epoch.is_empty() {
        // A record of a newer epoch replaces the old one without comparing
        // revs or surfaces (the ledger was rebuilt); one of an older epoch
        // arriving late is dropped (§2.8 client rule 1).
        if epochs.order(session, &next.epoch) > epochs.order(session, &prev.epoch) {
            known.insert(key, next);
        }
        return;
    }
    if next.rev < prev.rev {
        // An older read (§2.8: clients drop it); never a violation by itself.
        return;
    }
    if next.rev == prev.rev {
        if next.state != prev.state || next.surface != prev.surface {
            errors.push(format!(
                "{at}: {input} rev {} seen as {} on {} and as {} on {}",
                next.rev, prev.state, prev.surface, next.state, next.surface
            ));
        }
        return;
    }
    if !surface_move_allowed(&prev.surface, &next.surface, &prev.state, &next.state, record["reason"].as_str()) {
        errors.push(format!(
            "{at}: {input} surface {} -> {} ({} -> {}) is not a §2.5 move",
            prev.surface, next.surface, prev.state, next.state
        ));
    }
    // r274.2 §9.4 (owner rule 2026-11-03): a record reaches the timeline
    // from nextUp / pendingNotes only with read evidence. An `accepted` move
    // carries it (`offered`, or `details.readSignal`); `accepted` + `turnId`
    // alone is a steer the runtime only queued.
    if matches!(prev.surface.as_str(), "nextUp" | "pendingNotes") && next.surface == "timeline" && next.state == "accepted"
        && record["stage"] != "offered" && !record["details"]["readSignal"].is_string()
    {
        errors.push(format!("{at}: {input} moved to the timeline as accepted without read evidence: {record}"));
    }
    if next.state != prev.state && !is_settled(&prev.state) {
        if let Some(deadline) = prev.deadline_ms {
            if at_ms > deadline + SETTLE_SLACK_MS {
                errors.push(format!(
                    "{at}: {input} left {} {} ms after its deadline",
                    prev.state,
                    at_ms - deadline
                ));
            }
        }
    }
    let _ = prev.at_ms;
    known.insert(key, next);
}

#[track_caller]
pub fn assert_surface_trace(trace: &Trace) -> TraceSummary {
    assert_surface_trace_with(trace, AssertOptions { require_settled_at_end: true })
}

#[track_caller]
pub fn assert_surface_trace_with(trace: &Trace, options: AssertOptions) -> TraceSummary {
    match check_surface_trace(trace, options) {
        Ok(summary) => summary,
        Err(errors) => panic!("surface trace violates 054 §5.2:\n  {}", errors.join("\n  ")),
    }
}

#[track_caller]
pub fn assert_path(trace: &Trace, input_id: &str, expected: &[&str]) {
    let actual = trace.path(input_id);
    assert!(path_matches(expected, &actual), "{input_id}: expected {expected:?}, got {actual:?}");
}

/// One of several acceptable paths (e.g. S8: `inContext` or `uncertain`).
#[track_caller]
pub fn assert_path_any(trace: &Trace, input_id: &str, expected: &[&[&str]]) {
    let actual = trace.path(input_id);
    assert!(
        expected.iter().any(|p| path_matches(p, &actual)),
        "{input_id}: expected one of {expected:?}, got {actual:?}"
    );
}

#[track_caller]
pub fn assert_surfaces(trace: &Trace, input_id: &str, expected: &[&str]) {
    let actual = trace.surfaces(input_id);
    assert_eq!(actual, expected, "{input_id}: surfaces");
}

#[track_caller]
pub fn assert_never(trace: &Trace, input_id: &str, label_prefix: &str) {
    let actual = trace.path(input_id);
    assert!(
        !actual.iter().any(|l| label_matches(label_prefix, l)),
        "{input_id}: {label_prefix} must never appear, got {actual:?}"
    );
}

// ---------------------------------------------------------------------------
// Hand-made traces (unit tests of the assertion itself).

// ---------------------------------------------------------------------------
// Deadline-miss report (S25, r274): appended to a failing §5.2 assertion so
// the next run shows WHY a record stayed past its deadline: its record
// history, the owner route's legacy events and submit marker, the session's
// deliveries meanwhile, the longest gap in settle-thread writes, and the
// Management slow-lock / slow-path lines whose interval overlaps the stay
// (`cutex-asks-lock-slow`, `cutex-control-slow`, `cutex-submit-slow`,
// `cutex-lock-accounting`). Test-only; read-only.

/// Inputs named by deadline violations (`#<entry> …: <input> left … after
/// its deadline`, `<session>/<input>: still … after its deadline`), first
/// five, in order, with the trace entry of a `left` violation.
pub fn deadline_inputs(errors: &[String]) -> Vec<(String, Option<usize>)> {
    let mut inputs: Vec<(String, Option<usize>)> = Vec::new();
    for error in errors.iter().filter(|e| e.ends_with("after its deadline")) {
        let (input, entry) = if let Some((head, _)) = error.split_once(" left ") {
            let entry = error.strip_prefix('#').and_then(|rest| rest.split(' ').next()).and_then(|n| n.parse().ok());
            (head.rsplit(": ").next().unwrap_or("").to_string(), entry)
        } else {
            (error.split(": still ").next().and_then(|head| head.rsplit('/').next()).unwrap_or("").to_string(), None)
        };
        if !input.is_empty() && !inputs.iter().any(|(known, _)| *known == input) {
            inputs.push((input, entry));
        }
        if inputs.len() == 5 {
            break;
        }
    }
    inputs
}

fn iso_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map_or_else(|| ms.to_string(), |at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// One Management log line's `(start, end, duration)` in ms, when it is a
/// timed `cutex-*` JSON diagnostic of at least `min_ms`.
fn log_interval(line: &str, min_ms: i64) -> Option<(i64, i64, i64)> {
    let value: Value = serde_json::from_str(&line[line.find('{')?..]).ok()?;
    let end = value["timestamp"]
        .as_str()
        .or(value["observedAt"].as_str())
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())?
        .timestamp_millis();
    let ns = value["totalNs"]
        .as_u64()
        .unwrap_or(0)
        .max(value["holdNs"].as_u64().unwrap_or(0) + value["waitNs"].as_u64().unwrap_or(0));
    let ms = (ns / 1_000_000) as i64;
    (ms >= min_ms).then_some((end - ms, end, ms))
}

fn record_line(source: &str, p: &Value) -> String {
    let r = &p["record"];
    format!(
        "{source} rev {} {}{} on {} since {} deadline {} (stateRevision {})",
        r["rev"],
        r["state"].as_str().unwrap_or("?"),
        r["reason"].as_str().map(|reason| format!("{{{reason}}}")).unwrap_or_default(),
        r["surface"].as_str().unwrap_or("?"),
        r["since"].as_str().unwrap_or("-"),
        r["deadline"].as_str().unwrap_or("-"),
        p["stateRevision"]
    )
}

pub fn deadline_report(trace: &Trace, errors: &[String], management_log: Option<&str>) -> String {
    let mut out = String::new();
    for (input, violation) in deadline_inputs(errors) {
        let mut history = Vec::new();
        let mut session = String::new();
        let mut request_ids: Vec<String> = Vec::new();
        let (mut first, mut last) = (i64::MAX, i64::MIN);
        for entry in &trace.entries {
            let p = &entry.payload;
            let line = match entry.kind.as_str() {
                "stateChanged" if entry.source != "host" && p["record"]["inputId"] == input.as_str() => {
                    session = p["record"]["cutexSessionId"].as_str().unwrap_or("").to_string();
                    if let Some(created) = p["record"]["createdAt"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
                        first = first.min(created.timestamp_millis());
                    }
                    record_line(&entry.source, p)
                }
                "legacy" if entry.source != "host" && p["params"]["clientUserMessageId"] == input.as_str() => {
                    if let Some(id) = p["params"]["managementRequestId"].as_str() {
                        if !request_ids.iter().any(|known| known == id) {
                            request_ids.push(id.to_string());
                        }
                    }
                    let facts: Vec<String> = ["state", "disposition", "queuedReason", "reason", "code"]
                        .iter()
                        .filter_map(|key| p["params"][*key].as_str().map(|v| format!("{key}={v}")))
                        .collect();
                    format!("legacy {} {}", p["method"].as_str().unwrap_or("?"), facts.join(" "))
                }
                "ownerSubmit" if p["clientUserMessageId"] == input.as_str() => {
                    let ok = &p["result"]["Ok"];
                    format!(
                        "ownerSubmit returned: disposition={} queuedReason={} runtimeStarting={} error={}",
                        ok["disposition"], ok["queuedReason"], ok["runtimeStarting"], p["result"]["Err"]["code"]
                    )
                }
                _ => continue,
            };
            first = first.min(entry.at_ms);
            last = last.max(entry.at_ms);
            history.push(format!("    {} {line}", iso_ms(entry.at_ms)));
        }
        out.push_str(&format!("\n{input} ({session}): history (recorder time, record fields)\n{}", history.join("\n")));
        if first == i64::MAX {
            continue;
        }
        // The stay: from creation to the violating event (or the trace end).
        let until = violation.and_then(|index| trace.entries.get(index)).map_or(trace.end_ms().max(last), |e| e.at_ms);
        let session_deliveries: Vec<&TraceEntry> = trace
            .entries
            .iter()
            .filter(|e| e.kind == "deliveryChanged" && e.source == "session" && e.payload["cutexSessionId"] == session.as_str())
            .collect();
        let before = session_deliveries.iter().rposition(|e| e.at_ms < first);
        let deliveries: Vec<String> = session_deliveries
            .iter()
            .enumerate()
            .filter(|(i, e)| Some(*i) == before || (e.at_ms >= first && e.at_ms <= until + 1_000))
            .map(|(_, e)| {
                let delivery = &e.payload["delivery"];
                format!(
                    "    {} {}{}",
                    iso_ms(e.at_ms),
                    delivery["state"].as_str().unwrap_or("?"),
                    delivery["reason"].as_str().map(|r| format!("{{{r}}}")).unwrap_or_default()
                )
            })
            .collect();
        out.push_str(&format!(
            "\n  session deliveries (last before the stay, then during it):\n{}",
            if deliveries.is_empty() { "    none".to_string() } else { deliveries.join("\n") }
        ));
        // What only the settle thread writes (deliveryChanged, stalled marks)
        // on ANY session during the stay: a long gap = a starved settle pass
        // (its deadline pass did not run either).
        let settle: Vec<i64> = trace
            .entries
            .iter()
            .filter(|e| e.source == "session" && e.at_ms >= first && e.at_ms <= until)
            .filter(|e| e.kind == "deliveryChanged" || (e.kind == "stateChanged" && !e.payload["record"]["stalled"].is_null()))
            .map(|e| e.at_ms)
            .collect();
        let bounds: Vec<i64> = std::iter::once(first).chain(settle.iter().copied()).chain(std::iter::once(until)).collect();
        if let Some((gap, at)) = bounds.windows(2).map(|w| (w[1] - w[0], w[0])).max() {
            out.push_str(&format!(
                "\n  settle-thread writes (deliveryChanged / stalled, any session) during the stay: {}; longest gap {gap} ms from {}",
                settle.len(),
                iso_ms(at)
            ));
        }
        let Some(log) = management_log else { continue };
        let mut slow: Vec<(i64, String)> = log
            .lines()
            .filter(|line| line.starts_with("cutex-"))
            .filter_map(|line| {
                if line.starts_with("cutex-submit-slow") {
                    return request_ids.iter().any(|id| line.contains(id.as_str())).then(|| (i64::MAX, line.to_string()));
                }
                let (start, end, ms) = log_interval(line, 1_000)?;
                (start <= until + 1_000 && end >= first - 1_000).then(|| (ms, line.to_string()))
            })
            .collect();
        slow.sort_by(|a, b| b.0.cmp(&a.0));
        let total = slow.len();
        let lines: Vec<String> =
            slow.into_iter().take(30).map(|(_, line)| format!("    {}", line.chars().take(600).collect::<String>())).collect();
        out.push_str(&format!(
            "\n  Management slow lines (>= 1 s) overlapping {} .. {} ({total}, longest first):\n{}",
            iso_ms(first),
            iso_ms(until),
            if lines.is_empty() { "    none".to_string() } else { lines.join("\n") }
        ));
    }
    out
}

#[cfg(test)]
pub mod fixtures {
    use super::*;
    use serde_json::json;

    pub const SESSION: &str = "cutex.11111111-1111-4111-8111-111111111111";
    pub const EPOCH: &str = "epoch-a";
    pub const T0: i64 = 1_800_000_000_000;

    pub fn iso(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(input: &str, state: &str, reason: Option<&str>, surface: &str, rev: u64, revision: u64, deadline: Option<i64>) -> Value {
        json!({
            "inputId": input, "cutexSessionId": SESSION, "kind": "owner",
            "state": state, "reason": reason, "surface": surface,
            "rev": rev, "stateRevision": revision,
            "deadline": deadline.map(iso), "stage": null,
            "evidence": if state == "inContext" { json!("turnStartInput") } else { Value::Null },
        })
    }

    pub fn changed(at: i64, source: &str, record: Value) -> TraceEntry {
        TraceEntry {
            at_ms: at,
            source: source.into(),
            kind: "stateChanged".into(),
            cutex_session_id: Some(SESSION.into()),
            sequence: None,
            payload: json!({"cutexSessionId": SESSION, "ledgerEpoch": EPOCH,
                "stateRevision": record["stateRevision"], "record": record}),
        }
    }

    pub fn delivery(at: i64, revision: u64, state: &str) -> TraceEntry {
        TraceEntry {
            at_ms: at,
            source: "session".into(),
            kind: "deliveryChanged".into(),
            cutex_session_id: Some(SESSION.into()),
            sequence: None,
            payload: json!({"cutexSessionId": SESSION, "ledgerEpoch": EPOCH,
                "stateRevision": revision, "delivery": {"state": state}}),
        }
    }

    pub fn snapshot(at: i64, kind: &str, revision: u64, records: Vec<Value>) -> TraceEntry {
        let field = if kind == "queueList" { "inputs" } else { "records" };
        TraceEntry {
            at_ms: at,
            source: "snapshot".into(),
            kind: kind.into(),
            cutex_session_id: Some(SESSION.into()),
            sequence: None,
            payload: json!({"ledgerEpoch": EPOCH, "stateRevision": revision, field: records}),
        }
    }

    /// S1 shape: received → sending → inContext on the timeline, plus a
    /// delivery change in between (one gapless revision sequence).
    pub fn good_s1() -> Trace {
        let mut t = Trace::default();
        t.push(changed(T0, "session", record("m1", "received", None, "timeline", 1, 1, Some(T0 + 5_000))));
        t.push(delivery(T0 + 10, 2, "turn"));
        t.push(changed(T0 + 20, "session", record("m1", "sending", None, "timeline", 2, 3, Some(T0 + 30_000))));
        t.push(changed(T0 + 40, "session", record("m1", "inContext", None, "timeline", 3, 4, None)));
        t.push(snapshot(T0 + 50, "inputList", 4, vec![record("m1", "inContext", None, "timeline", 3, 4, None)]));
        t
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    fn errors(trace: &Trace) -> Vec<String> {
        check_surface_trace(trace, AssertOptions { require_settled_at_end: true }).err().unwrap_or_default()
    }

    #[test]
    fn good_trace_passes_and_round_trips_jsonl() {
        let trace = good_s1();
        let summary = assert_surface_trace(&trace);
        assert_eq!(summary, TraceSummary { inputs: 1, events: 4, snapshots: 1 });
        assert_path(&trace, "m1", &["received", "sending", "inContext"]);
        assert_eq!(trace.path("m1"), vec!["received", "sending", "inContext(turnStartInput)"]);
        assert_surfaces(&trace, "m1", &["timeline"]);
        assert_eq!(Trace::from_jsonl(&trace.to_jsonl()), trace);
    }

    #[test]
    fn next_up_to_timeline_and_queue_list_inputs_pass() {
        let mut t = Trace::default();
        t.push(changed(T0, "session", record("q1", "waiting", Some("explicitQueue"), "nextUp", 1, 1, None)));
        t.push(snapshot(T0 + 5, "queueList", 1, vec![record("q1", "waiting", Some("explicitQueue"), "nextUp", 1, 1, None)]));
        t.push(changed(T0 + 10, "session", record("q1", "sending", None, "nextUp", 2, 2, Some(T0 + 30_000))));
        t.push(changed(T0 + 20, "session", record("q1", "inContext", None, "timeline", 3, 3, None)));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));
        assert_surfaces(&t, "q1", &["nextUp", "timeline"]);
    }

    #[test]
    fn timeline_back_to_next_up_is_rejected() {
        let mut t = Trace::default();
        t.push(changed(T0, "session", record("m1", "accepted", None, "timeline", 1, 1, Some(T0 + 60_000))));
        t.push(changed(T0 + 10, "session", record("m1", "waiting", Some("returned"), "nextUp", 2, 2, None)));
        t.push(changed(T0 + 20, "session", record("m1", "inContext", None, "timeline", 3, 3, None)));
        let errors = errors(&t);
        assert!(errors.iter().any(|e| e.contains("timeline -> nextUp")), "{errors:?}");
    }

    #[test]
    fn owner_retry_may_return_to_next_up() {
        let mut t = Trace::default();
        t.push(changed(T0, "session", record("m1", "uncertain", Some("turnInterrupted"), "timeline", 1, 1, None)));
        t.push(changed(T0 + 10, "session", record("m1", "waiting", Some("ownerRetry"), "nextUp", 2, 2, None)));
        t.push(changed(T0 + 20, "session", record("m1", "inContext", None, "timeline", 3, 3, None)));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));
        // The same move without the owner's retry is not allowed.
        let mut bad = Trace::default();
        bad.push(changed(T0, "session", record("m1", "uncertain", Some("turnInterrupted"), "timeline", 1, 1, None)));
        bad.push(changed(T0 + 10, "session", record("m1", "waiting", Some("returned"), "nextUp", 2, 2, None)));
        assert!(!super::tests::errors(&bad).is_empty());
    }

    #[test]
    fn pending_notes_to_next_up_and_none_back_are_rejected() {
        let mut t = Trace::default();
        t.push(changed(T0, "session", record("n1", "waiting", Some("passive"), "pendingNotes", 1, 1, None)));
        t.push(changed(T0 + 10, "session", record("n1", "waiting", Some("afterTurn"), "nextUp", 2, 2, None)));
        t.push(changed(T0 + 20, "session", record("n1", "discarded", Some("ownerDiscarded"), "none", 3, 3, None)));
        assert!(errors(&t).iter().any(|e| e.contains("pendingNotes -> nextUp")));
    }

    #[test]
    fn revision_holes_and_unseen_regressions_are_rejected() {
        let mut hole = good_s1();
        hole.entries.remove(1); // the deliveryChanged at revision 2
        assert!(errors(&hole).iter().any(|e| e.contains("hole 1 -> 3")), "{:?}", errors(&hole));

        let mut regression = good_s1();
        regression.push(delivery(T0 + 60, 3, "idle"));
        assert!(errors(&regression).iter().any(|e| e.contains("stateRevision 3 not above 4")), "{:?}", errors(&regression));
    }

    #[test]
    fn an_identical_outbox_replay_prefix_keeps_the_revision_head() {
        let mut t = good_s1();
        let prefix = t.entries[..3].to_vec();
        for (i, mut entry) in prefix.into_iter().enumerate() {
            entry.at_ms = T0 + 60 + i as i64;
            entry.sequence = Some(100 + i as u64);
            t.push(entry);
        }
        // Includes both stateChanged and deliveryChanged, replayed after a
        // later record version; replay must not regress either version head.
        t.push(delivery(T0 + 70, 5, "idle"));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));
        let mut hole = t;
        hole.push(delivery(T0 + 80, 7, "idle"));
        assert!(errors(&hole).iter().any(|e| e.contains("hole 5 -> 7")), "{:?}", errors(&hole));
    }

    #[test]
    fn a_replay_with_different_payload_at_the_same_revision_is_rejected() {
        for index in [1, 2] { // deliveryChanged and stateChanged
            let mut t = good_s1();
            let mut conflict = t.entries[index].clone();
            conflict.at_ms = T0 + 60;
            conflict.sequence = Some(100);
            if conflict.kind == "stateChanged" {
                conflict.payload["record"]["reason"] = "different-reason".into();
            } else {
                conflict.payload["delivery"]["state"] = "idle".into();
            }
            t.push(conflict);
            assert!(errors(&t).iter().any(|e| e.contains("conflicting replay")), "{:?}", errors(&t));
        }
    }

    #[test]
    fn an_event_seen_on_another_stream_cannot_hide_a_gap() {
        let mut t = good_s1();
        let mut first = delivery(T0 + 60, 1, "idle");
        first.source = "host".into();
        t.push(first);
        let mut unseen = t.entries[2].clone();
        unseen.source = "host".into();
        unseen.at_ms = T0 + 70;
        t.push(unseen);
        assert!(errors(&t).iter().any(|e| e.contains("hole 1 -> 3")), "{:?}", errors(&t));
    }

    /// `record_batch` (queue merge): one revision, one stateChanged per
    /// distinct input, back to back.
    fn batch() -> Trace {
        let mut t = Trace::default();
        for (i, id) in ["q0", "q1", "q2"].into_iter().enumerate() {
            t.push(changed(T0 + i as i64, "session", record(id, "waiting", Some("explicitQueue"), "nextUp", 1, i as u64 + 1, None)));
        }
        t.push(changed(T0 + 10, "session", record("q1", "superseded", Some("mergedInsert"), "none", 2, 4, None)));
        t.push(changed(T0 + 10, "session", record("q2", "superseded", Some("mergedInsert"), "none", 2, 4, None)));
        t.push(changed(T0 + 10, "session", record("q0", "waiting", Some("explicitQueue"), "nextUp", 2, 4, None)));
        t
    }

    #[test]
    fn a_batch_shares_one_revision_across_distinct_inputs() {
        let mut t = batch();
        t.push(changed(T0 + 20, "session", record("q0", "inContext", None, "timeline", 3, 5, None)));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));

        // A different record version for the same input/revision conflicts,
        // batch or not (an identical outbox replay is allowed).
        let mut twice = batch();
        twice.push(changed(T0 + 11, "session", record("q1", "superseded", Some("mergedInsert"), "none", 3, 4, None)));
        assert!(errors(&twice).iter().any(|e| e.contains("conflicting replay at stateRevision 4")), "{:?}", errors(&twice));

        // After the batch the next revision must still be gapless.
        let mut hole = batch();
        hole.push(changed(T0 + 20, "session", record("q0", "inContext", None, "timeline", 3, 6, None)));
        assert!(errors(&hole).iter().any(|e| e.contains("hole 4 -> 6")), "{:?}", errors(&hole));

        // A deliveryChanged never shares a revision.
        let mut delivery_same = batch();
        delivery_same.push(delivery(T0 + 11, 4, "idle"));
        assert!(errors(&delivery_same).iter().any(|e| e.contains("stateRevision 4 not above 4")), "{:?}", errors(&delivery_same));

        // A deliveryChanged closes the batch and opens its own: records of
        // its revision follow it (actions refresh, interruptSend's first
        // transition), each input once; a second delivery cannot share it.
        let mut led = batch();
        led.push(delivery(T0 + 11, 5, "finishing"));
        led.push(changed(T0 + 12, "session", record("q9", "waiting", Some("explicitQueue"), "nextUp", 1, 5, None)));
        led.push(changed(T0 + 12, "session", record("q0", "waiting", Some("explicitQueue"), "nextUp", 3, 5, None)));
        let led_errors: Vec<String> = errors(&led).into_iter().filter(|e| !e.contains("unsettled at trace end")).collect();
        assert!(led_errors.is_empty(), "{led_errors:?}");
        let mut led_twice = led.clone();
        led_twice.push(changed(T0 + 13, "session", record("q9", "waiting", Some("explicitQueue"), "nextUp", 2, 5, None)));
        assert!(errors(&led_twice).iter().any(|e| e.contains("conflicting replay at stateRevision 5")), "{:?}", errors(&led_twice));
        let mut second_delivery = led.clone();
        second_delivery.push(delivery(T0 + 13, 5, "idle"));
        assert!(errors(&second_delivery).iter().any(|e| e.contains("conflicting replay at stateRevision 5")), "{:?}", errors(&second_delivery));
        // Records of an older revision after the delivery are still rejected.
        let mut stale = batch();
        stale.push(delivery(T0 + 11, 5, "idle"));
        stale.push(changed(T0 + 12, "session", record("q9", "waiting", Some("explicitQueue"), "nextUp", 1, 4, None)));
        assert!(errors(&stale).iter().any(|e| e.contains("stateRevision 4 not above 5")), "{:?}", errors(&stale));
    }

    #[test]
    fn revisions_continue_across_a_restart_marker_and_reset_on_a_new_epoch() {
        let mut t = good_s1();
        t.push(TraceEntry { at_ms: T0 + 60, source: "marker".into(), kind: "restart".into(), cutex_session_id: None, sequence: None, payload: Value::Null });
        t.push(delivery(T0 + 70, 5, "idle"));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));

        let mut restarted_at_one = good_s1();
        restarted_at_one.push(delivery(T0 + 70, 1, "idle"));
        assert!(!errors(&restarted_at_one).is_empty(), "a restart must not reset stateRevision");

        let mut new_epoch = good_s1();
        let mut entry = delivery(T0 + 70, 1, "idle");
        entry.payload["ledgerEpoch"] = "epoch-b".into();
        new_epoch.push(entry);
        assert!(errors(&new_epoch).is_empty(), "{:?}", errors(&new_epoch));
    }

    /// S21: the ledger file is lost; the daemon starts a new `ledgerEpoch`,
    /// rebuilds the unsettled records (rev 1 again) and sends
    /// `cutex/stream/reset`. Per-record revs restart; within one epoch holes
    /// and duplicates are still violations.
    fn ledger_lost() -> Trace {
        let mut t = Trace::default();
        t.push(changed(T0, "session", record("q1", "received", None, "nextUp", 1, 1, Some(T0 + 5_000))));
        t.push(changed(T0 + 10, "session", record("q1", "waiting", Some("explicitQueue"), "nextUp", 2, 2, None)));
        t.push(snapshot(T0 + 20, "inputList", 2, vec![record("q1", "waiting", Some("explicitQueue"), "nextUp", 2, 2, None)]));
        let b = |mut entry: TraceEntry| { entry.payload["ledgerEpoch"] = "epoch-b".into(); entry };
        t.push(b(changed(T0 + 100, "session", record("q1", "waiting", Some("explicitQueue"), "nextUp", 1, 1, None))));
        t.push(TraceEntry { at_ms: T0 + 110, source: "session".into(), kind: "streamReset".into(),
            cutex_session_id: Some(SESSION.into()), sequence: None, payload: serde_json::json!({"reason": "state_lost", "resyncRequired": true}) });
        t.push(b(snapshot(T0 + 120, "inputList", 1, vec![record("q1", "waiting", Some("explicitQueue"), "nextUp", 1, 1, None)])));
        t
    }

    #[test]
    fn a_new_epoch_restarts_record_revs() {
        let mut t = ledger_lost();
        let b = |mut entry: TraceEntry| { entry.payload["ledgerEpoch"] = "epoch-b".into(); entry };
        t.push(b(changed(T0 + 200, "session", record("q1", "sending", None, "nextUp", 2, 2, Some(T0 + 30_000)))));
        // A late read of the lost epoch neither overrides nor fails.
        t.push(snapshot(T0 + 210, "inputList", 2, vec![record("q1", "waiting", Some("explicitQueue"), "nextUp", 2, 2, None)]));
        t.push(b(changed(T0 + 300, "session", record("q1", "inContext", None, "timeline", 3, 3, None))));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));
        assert_eq!(t.latest("q1").unwrap()["state"], "inContext");
        assert_eq!(t.path("q1"), ["received", "waiting{explicitQueue}", "sending", "inContext(turnStartInput)"]);

        // The lost epoch's unsettled records are not "unsettled at the end".
        let mut dropped = ledger_lost();
        dropped.push(b(changed(T0 + 300, "session", record("q1", "inContext", None, "timeline", 2, 2, None))));
        dropped.push(changed(T0 + 5, "session", record("lost", "waiting", Some("explicitQueue"), "nextUp", 1, 3, None)));
        assert!(!errors(&dropped).iter().any(|e| e.contains("lost")), "{:?}", errors(&dropped));
    }

    #[test]
    fn within_the_new_epoch_duplicates_and_holes_are_still_rejected() {
        let b = |mut entry: TraceEntry| { entry.payload["ledgerEpoch"] = "epoch-b".into(); entry };
        let mut duplicate = ledger_lost();
        duplicate.push(b(changed(T0 + 200, "session", record("q1", "sending", None, "nextUp", 1, 2, None))));
        assert!(errors(&duplicate).iter().any(|e| e.contains("rev 1 not above 1")), "{:?}", errors(&duplicate));

        let mut hole = ledger_lost();
        // (the reset notice lets the first event after it skip once, like a resubscribe)
        hole.push(b(changed(T0 + 200, "session", record("q1", "sending", None, "nextUp", 2, 2, None))));
        hole.push(b(changed(T0 + 300, "session", record("q1", "inContext", None, "timeline", 3, 4, None))));
        assert!(errors(&hole).iter().any(|e| e.contains("hole 2 -> 4")), "{:?}", errors(&hole));

        let mut back = ledger_lost();
        back.push(b(changed(T0 + 200, "session", record("q1", "inContext", None, "timeline", 2, 2, None))));
        back.push(b(changed(T0 + 300, "session", record("q1", "waiting", Some("explicitQueue"), "nextUp", 3, 3, None))));
        assert!(errors(&back).iter().any(|e| e.contains("timeline -> nextUp")), "{:?}", errors(&back));
    }

    #[test]
    fn a_resubscribed_stream_may_skip_revisions_once() {
        let mut t = good_s1();
        t.push(TraceEntry { at_ms: T0 + 60, source: "session".into(), kind: "resubscribed".into(),
            cutex_session_id: Some(SESSION.into()), sequence: None, payload: Value::Null });
        t.push(delivery(T0 + 70, 9, "idle"));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));
        t.push(delivery(T0 + 80, 11, "idle"));
        assert!(errors(&t).iter().any(|e| e.contains("hole 9 -> 11")));
    }

    #[test]
    fn streams_are_checked_independently() {
        let mut t = good_s1();
        // The host stream sees the same changes (without preview/actions).
        for entry in good_s1().entries.into_iter().filter(|e| e.source == "session") {
            t.push(TraceEntry { source: "host".into(), ..entry });
        }
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));
    }

    #[test]
    fn two_surfaces_at_one_rev_and_duplicates_in_a_read_are_rejected() {
        let mut t = good_s1();
        t.push(snapshot(T0 + 60, "inputList", 4, vec![
            record("m1", "inContext", None, "nextUp", 3, 4, None),
        ]));
        assert!(errors(&t).iter().any(|e| e.contains("rev 3 seen as")));

        let mut d = good_s1();
        let r = record("m1", "inContext", None, "timeline", 3, 4, None);
        d.push(snapshot(T0 + 60, "inputList", 4, vec![r.clone(), r]));
        assert!(errors(&d).iter().any(|e| e.contains("listed twice")));
    }

    #[test]
    fn queue_list_inputs_hold_only_unsettled_next_up_and_pending_notes() {
        let mut t = good_s1();
        t.push(snapshot(T0 + 60, "queueList", 4, vec![record("m1", "inContext", None, "timeline", 3, 4, None)]));
        assert!(errors(&t).iter().any(|e| e.contains("queue/list inputs holds m1")));
    }

    #[test]
    fn an_older_read_never_overrides_and_is_not_a_violation() {
        let mut t = good_s1();
        t.push(snapshot(T0 + 60, "inputList", 1, vec![record("m1", "received", None, "timeline", 1, 1, Some(T0 + 5_000))]));
        assert!(errors(&t).is_empty(), "{:?}", errors(&t));
        assert_eq!(t.latest("m1").unwrap()["state"], "inContext");
    }

    #[test]
    fn late_settlement_and_unsettled_records_are_rejected() {
        let mut late = Trace::default();
        late.push(changed(T0, "session", record("m1", "sending", None, "timeline", 1, 1, Some(T0 + 30_000))));
        late.push(changed(T0 + 36_000, "session", record("m1", "uncertain", Some("outcomeUnknown"), "timeline", 2, 2, None)));
        assert!(errors(&late).iter().any(|e| e.contains("left sending")), "{:?}", errors(&late));

        let mut stuck = Trace::default();
        stuck.push(changed(T0, "session", record("m1", "accepted", None, "timeline", 1, 1, Some(T0 + 60_000))));
        stuck.push(delivery(T0 + 70_000, 2, "idle"));
        assert!(errors(&stuck).iter().any(|e| e.contains("after its deadline")));

        let mut waiting = Trace::default();
        waiting.push(changed(T0, "session", record("q1", "waiting", Some("explicitQueue"), "nextUp", 1, 1, None)));
        assert!(errors(&waiting).iter().any(|e| e.contains("unsettled at trace end")));
        assert!(check_surface_trace(&waiting, AssertOptions { require_settled_at_end: false }).is_ok());
    }

    #[test]
    fn a_deadline_miss_report_shows_history_route_and_overlapping_slow_lines() {
        let mut t = Trace::default();
        let mut created = record("m1", "received", None, "nextUp", 1, 1, Some(T0 + 5_000));
        created["createdAt"] = json!(iso(T0));
        t.push(TraceEntry { at_ms: T0 - 5_000, source: "session".into(), kind: "legacy".into(), cutex_session_id: Some(SESSION.into()),
            sequence: None, payload: json!({"method": "cutex/userInput/routeAccepted",
                "params": {"clientUserMessageId": "m1", "managementRequestId": "req-1", "state": "routed_to_cutex"}}) });
        t.push(changed(T0, "session", created));
        t.push(delivery(T0 + 100, 2, "offline"));
        t.push(changed(T0 + 16_000, "session", record("m1", "waiting", Some("agentOffline"), "nextUp", 2, 3, None)));
        let errors_of = errors;
        let errors = errors_of(&t);
        assert!(errors.iter().any(|e| e.contains("m1 left received")), "{errors:?}");
        assert_eq!(deadline_inputs(&errors), vec![("m1".to_string(), Some(3))]);
        let log = [
            format!("cutex-asks-lock-slow {}", json!({"timestamp": iso(T0 + 15_000), "holdNs": 16_000_000_000u64, "waitNs": 1_000_000u64, "site": "src/x.rs:1"})),
            format!("cutex-control-slow {}", json!({"timestamp": iso(T0 - 60_000), "totalNs": 9_000_000_000u64})),
            format!("cutex-control-slow {}", json!({"timestamp": iso(T0 + 1_000), "totalNs": 10_000_000u64})),
            "cutex-submit-slow request=req-1 total_ms=22517".to_string(),
            "cutex-submit-slow request=req-2 total_ms=9".to_string(),
        ].join("
");
        let report = deadline_report(&t, &errors, Some(&log));
        assert!(report.contains("m1 (") && report.contains("rev 1 received on nextUp"), "{report}");
        assert!(report.contains("waiting{agentOffline}") && report.contains("legacy cutex/userInput/routeAccepted"), "{report}");
        assert!(report.contains("offline") && report.contains("during the stay: 1; longest gap 15900 ms"), "{report}");
        assert!(report.contains("src/x.rs:1") && report.contains("request=req-1"), "{report}");
        assert!(!report.contains("9000000000") && !report.contains("req-2") && !report.contains("10000000"), "{report}");

        let mut stuck = Trace::default();
        stuck.push(changed(T0, "session", record("m2", "sending", None, "timeline", 1, 1, Some(T0 + 1_000))));
        stuck.push(delivery(T0 + 70_000, 2, "idle"));
        assert_eq!(deadline_inputs(&errors_of(&stuck)), vec![("m2".to_string(), None)]);
    }

    #[test]
    fn record_and_event_revisions_must_agree() {
        let mut t = Trace::default();
        let mut entry = changed(T0, "session", record("m1", "received", None, "timeline", 1, 1, None));
        entry.payload["stateRevision"] = 2.into();
        t.push(entry);
        assert!(errors(&t).iter().any(|e| e.contains("record.stateRevision")));
    }

    #[test]
    fn labels_and_path_patterns() {
        let mut r = record("m1", "accepted", None, "timeline", 1, 1, None);
        r["stage"] = "offered".into();
        assert_eq!(label(&r), "accepted[offered]");
        assert!(label_matches("accepted", "accepted[offered]"));
        assert!(label_matches("waiting{turnFinishing}", "waiting{turnFinishing}"));
        assert!(!label_matches("waiting", "waitingX"));
        assert!(!path_matches(&["received"], &["received".into(), "sending".into()]));
    }
}
