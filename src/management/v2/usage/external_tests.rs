//! Extra usage sources: synthetic Claude Code / Codex trees in temp dirs
//! only (never the real ~/.claude or ~/.codex).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde_json::json;

use super::*;
use crate::management::v2::usage::query;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cutex-usage-sources-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-14T12:00:00Z").unwrap().with_timezone(&Utc)
}

fn spec(id: &str, kind: SourceKind, path: &Path) -> SourceSpec {
    SourceSpec {
        id: id.into(),
        kind,
        path: path.to_path_buf(),
        label: format!("{} label", kind.display()),
        billing: Billing::Subscription,
        enabled: true,
    }
}

fn append(path: &Path, lines: &[serde_json::Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    for line in lines {
        writeln!(file, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }
}

fn assistant(id: &str, at: &str, output: u64) -> serde_json::Value {
    json!({
        "parentUuid": "p", "isSidechain": false, "type": "assistant", "timestamp": at,
        "sessionId": "sess-1", "cwd": "/work/demo-app", "effort": "high", "requestId": format!("req-{id}"),
        "message": {"id": id, "model": "claude-opus-5-5", "role": "assistant", "type": "message",
            "content": [{"type": "text", "text": "secret words"}],
            "usage": {"input_tokens": 10, "output_tokens": output, "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 1000, "service_tier": "standard"}}
    })
}

fn user(prompt: &str, at: &str) -> serde_json::Value {
    json!({"type": "user", "promptId": prompt, "timestamp": at, "sessionId": "sess-1",
        "message": {"role": "user", "content": "do the thing"}})
}

fn samples(ledger: &crate::management::v2::usage::UsageLedger) -> Vec<&UsageSample> {
    ledger
        .entries
        .iter()
        .filter_map(|entry| match entry {
            UsageLedgerEntry::Usage(sample) => Some(sample.as_ref()),
            UsageLedgerEntry::ResetBoundary(_) => None,
        })
        .collect()
}

fn load(root: &Path) -> crate::management::v2::usage::UsageLedger {
    store::load_usage_ledger_months_at(root, "2026-01", "2026-12", &HashSet::new()).unwrap()
}

#[test]
fn claude_rows_one_per_message_with_adapter_mapping() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("claude-home");
    let file = home.join("projects/-work-demo-app/sess-1.jsonl");
    append(&file, &[
        user("prompt-a", "2026-10-14T10:00:00Z"),
        // Streamed blocks of one message: the last line wins.
        assistant("msg_1", "2026-10-14T10:00:01Z", 5),
        assistant("msg_1", "2026-10-14T10:00:02Z", 7),
        user("prompt-a", "2026-10-14T10:00:03Z"),
        assistant("msg_2", "2026-10-14T10:00:04Z", 3),
        // Zero usage (API error placeholder) is skipped.
        json!({"type": "assistant", "timestamp": "2026-10-14T10:00:05Z", "sessionId": "sess-1",
            "message": {"id": "msg_err", "model": "<synthetic>", "usage": {"input_tokens": 0, "output_tokens": 0}}}),
    ]);
    // A subagent transcript of the same session.
    append(&home.join("projects/-work-demo-app/sess-1/subagents/agent-x.jsonl"), &[assistant("msg_sub", "2026-10-14T10:00:06Z", 2)]);
    // Never read: settings / credentials next to projects.
    fs::write(home.join(".credentials.json"), "{\"type\":\"assistant\"}\n").unwrap();
    fs::write(home.join("settings.json"), "{}").unwrap();

    let source = spec("claude-code", SourceKind::ClaudeCode, &home);
    let summary = scan_source_at(&root, &source, "host-a", now()).unwrap();
    assert_eq!((summary.files, summary.rows, summary.errors), (2, 3, 0));

    let ledger = load(&root);
    let rows = samples(&ledger);
    assert_eq!(rows.len(), 3);
    let first = rows.iter().find(|row| row.entry_id == "usage:ext:claude:msg_1").unwrap();
    assert_eq!(first.tokens.input_tokens, 1110);
    assert_eq!(first.tokens.cached_input_tokens, 1000);
    assert_eq!(first.tokens.cache_write_input_tokens, 100);
    assert_eq!(first.tokens.output_tokens, 7);
    assert_eq!(first.tokens.total_tokens, 1117);
    assert_eq!(first.attribution.provider.as_deref(), Some("anthropic"));
    assert_eq!(first.attribution.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(first.attribution.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(first.turn_id, "prompt-a");
    assert_eq!(first.thread_id, "sess-1");
    assert_eq!(first.cutex_session_id, "ext:claude-code:-work-demo-app");
    let origin = first.source.as_ref().unwrap();
    assert_eq!(origin.agent_name.as_deref(), Some("Claude Code · demo-app"));
    assert_eq!(origin.project.as_deref(), Some("demo-app"));
    assert_eq!(origin.billing.as_deref(), Some("subscription"));

    // Nothing new: an unchanged tree adds no rows; an appended line does.
    let again = scan_source_at(&root, &source, "host-a", now()).unwrap();
    assert_eq!(again.rows, 0);
    append(&file, &[user("prompt-b", "2026-10-14T10:05:00Z"), assistant("msg_3", "2026-10-14T10:05:01Z", 4)]);
    assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().rows, 1);
    let ledger = load(&root);
    let third = samples(&ledger).into_iter().find(|row| row.entry_id == "usage:ext:claude:msg_3").unwrap().clone();
    assert_eq!(third.turn_id, "prompt-b");
    // The status reflects the last pass.
    let status = status_json(&root, "claude-code");
    assert_eq!(status["files"], 2);
    assert_eq!(status["rows"], 4);
    assert_eq!(status["lastRows"], 1);
}

#[test]
fn claude_trailing_message_waits_until_settled() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("claude-home");
    let file = home.join("projects/p/s.jsonl");
    // 20 s before `now`: may still be streaming.
    append(&file, &[assistant("msg_live", "2026-10-14T11:59:40Z", 1)]);
    let source = spec("cc", SourceKind::ClaudeCode, &home);
    assert_eq!(scan_source_at(&root, &source, "h", now()).unwrap().rows, 0);
    append(&file, &[assistant("msg_live", "2026-10-14T11:59:45Z", 9)]);
    let later = now() + chrono::Duration::seconds(120);
    assert_eq!(scan_source_at(&root, &source, "h", later).unwrap().rows, 1);
    let ledger = load(&root);
    assert_eq!(samples(&ledger)[0].tokens.output_tokens, 9);
    // A partial last line (no newline yet) is never consumed.
    let mut handle = fs::OpenOptions::new().append(true).open(&file).unwrap();
    write!(handle, "{{\"type\":\"assistant\",\"timestamp\":").unwrap();
    assert_eq!(scan_source_at(&root, &source, "h", later).unwrap().rows, 0);
}

fn codex_meta(id: &str, originator: &str) -> serde_json::Value {
    json!({"timestamp": "2026-10-14T09:00:00Z", "type": "session_meta",
        "payload": {"id": id, "session_id": id, "originator": originator, "cwd": "/work/other-app",
            "model_provider": "openai", "base_instructions": {"text": "long"}}})
}

fn codex_turn(turn: &str) -> serde_json::Value {
    json!({"timestamp": "2026-10-14T09:00:01Z", "type": "turn_context",
        "payload": {"turn_id": turn, "cwd": "/work/other-app", "model": "gpt-6-astra", "effort": "high"}})
}

fn usage(input: u64, cached: u64, output: u64) -> serde_json::Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
        "output_tokens": output, "reasoning_output_tokens": 1, "total_tokens": input + output})
}

fn record(response: &str, at: &str, thread: &str) -> serde_json::Value {
    json!({"timestamp": at, "type": "token_usage_record",
        "payload": {"thread_id": thread, "turn_id": "turn-1", "session_id": thread, "root_turn_id": "turn-1",
            "response_id": response, "usage": usage(100, 40, 10), "turn_token_usage": usage(100, 40, 10),
            "thread_token_usage": usage(100, 40, 10)}})
}

fn token_count(at: &str, total: u64, last_input: u64) -> serde_json::Value {
    json!({"timestamp": at, "type": "event_msg", "payload": {"type": "token_count",
        "info": {"total_token_usage": {"input_tokens": total, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0, "total_tokens": total},
            "last_token_usage": usage(last_input, 0, 5), "model_context_window": 1000}}})
}

#[test]
fn codex_records_win_over_token_count() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("codex-home");
    let thread = "019a-thread-new";
    append(&home.join("sessions/2026/10/14/rollout-2026-10-14T09-00-00-019a.jsonl"), &[
        codex_meta(thread, "codex_cli_rs"),
        codex_turn("turn-1"),
        json!({"timestamp": "2026-10-14T09:00:02Z", "type": "response_item", "payload": {"type": "message", "content": "token_count text"}}),
        record("resp-1", "2026-10-14T09:00:03Z", thread),
        token_count("2026-10-14T09:00:03Z", 110, 100),
        record("resp-2", "2026-10-14T09:00:04Z", thread),
        token_count("2026-10-14T09:00:04Z", 220, 100),
    ]);
    // Old-format rollout: token_count only, counted when the total moves.
    let old = "018f-thread-old";
    append(&home.join("sessions/2026/10/13/rollout-2026-10-13T09-00-00-018f.jsonl"), &[
        codex_meta(old, "codex_cli_rs"),
        codex_turn("turn-9"),
        token_count("2026-10-13T09:00:03Z", 105, 100),
        token_count("2026-10-13T09:00:03Z", 105, 100),
        token_count("2026-10-13T09:00:04Z", 210, 100),
    ]);
    // Other files in sessions/ and auth next to it are never read.
    fs::write(home.join("auth.json"), "{\"type\":\"token_usage_record\"}\n").unwrap();
    fs::write(home.join("sessions/2026/10/14/notes.jsonl"), "{}\n").unwrap();
    let source = spec("codex", SourceKind::Codex, &home);
    let summary = scan_source_at(&root, &source, "h", now()).unwrap();
    assert_eq!((summary.files, summary.rows), (2, 4));
    let ledger = load(&root);
    let rows = samples(&ledger);
    let ids = rows.iter().map(|row| row.entry_id.as_str()).collect::<HashSet<_>>();
    assert_eq!(
        ids,
        HashSet::from([
            "usage:ext:codex:resp-1",
            "usage:ext:codex:resp-2",
            "usage:ext:codex:018f-thread-old:105",
            "usage:ext:codex:018f-thread-old:210",
        ])
    );
    let first = rows.iter().find(|row| row.entry_id == "usage:ext:codex:resp-1").unwrap();
    assert_eq!(first.tokens.input_tokens, 100);
    assert_eq!(first.tokens.cached_input_tokens, 40);
    assert_eq!(first.attribution.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(first.attribution.provider.as_deref(), Some("openai"));
    assert_eq!(first.thread_id, thread);
    assert_eq!(first.cutex_session_id, "ext:codex:other-app");
    assert_eq!(first.source.as_ref().unwrap().agent_name.as_deref(), Some("Codex · other-app"));
}

#[test]
fn codex_skips_cutex_threads_and_fresh_lines() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("codex-home");
    // cutex's own ledger already has this thread.
    let known = "019b-known";
    let own = UsageLedgerEntry::Usage(Box::new(UsageSample {
        entry_id: "usage:own-1".into(),
        event_id: "own-1".into(),
        observed_at: "2026-10-14T08:00:00Z".into(),
        host_id: "h".into(),
        cutex_session_id: "cutex-session".into(),
        thread_id: known.into(),
        turn_id: "t".into(),
        runtime_generation: 1,
        profile: Some("p".into()),
        profile_id: None,
        profile_deleted: false,
        source: None,
        attribution: UsageAttribution::default(),
        token_source: UsageTokenSource::NativeLast,
        model_context_window: None,
        compaction_count: None,
        tokens: TokenUsageBreakdown { total_tokens: 3, input_tokens: 2, output_tokens: 1, ..Default::default() },
        cumulative_total: TokenUsageBreakdown::default(),
    }));
    crate::management::v2::usage::record_direct_usage_sample_at_for_tests(&root, match own {
        UsageLedgerEntry::Usage(sample) => *sample,
        UsageLedgerEntry::ResetBoundary(_) => unreachable!(),
    })
    .unwrap();
    append(&home.join("sessions/2026/10/14/rollout-a.jsonl"), &[
        codex_meta(known, "codex_cli_rs"),
        record("resp-known", "2026-10-14T09:00:03Z", known),
    ]);
    append(&home.join("sessions/2026/10/14/rollout-b.jsonl"), &[
        codex_meta("019c-cutex", "cutex"),
        record("resp-cutex", "2026-10-14T09:00:03Z", "019c-cutex"),
    ]);
    let fresh = home.join("sessions/2026/10/14/rollout-c.jsonl");
    append(&fresh, &[
        codex_meta("019d-fresh", "codex_cli_rs"),
        record("resp-old", "2026-10-14T11:00:00Z", "019d-fresh"),
        record("resp-fresh", "2026-10-14T11:59:30Z", "019d-fresh"),
    ]);
    let source = spec("codex", SourceKind::Codex, &home);
    assert_eq!(scan_source_at(&root, &source, "h", now()).unwrap().rows, 1);
    let later = now() + chrono::Duration::seconds(90);
    assert_eq!(scan_source_at(&root, &source, "h", later).unwrap().rows, 1);
    let ledger = load(&root);
    let external = samples(&ledger).into_iter().filter(|row| row.source.is_some()).map(|row| row.entry_id.clone()).collect::<HashSet<_>>();
    assert_eq!(external, HashSet::from(["usage:ext:codex:resp-old".to_string(), "usage:ext:codex:resp-fresh".to_string()]));
}

#[test]
fn hidden_and_purged_sources_leave_the_report() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("claude-home");
    append(&home.join("projects/p/s.jsonl"), &[assistant("msg_1", "2026-10-14T10:00:01Z", 5)]);
    let source = spec("cc", SourceKind::ClaudeCode, &home);
    scan_source_at(&root, &source, "h", now()).unwrap();
    assert_eq!(samples(&load(&root)).len(), 1);
    let hidden = HashSet::from(["cc".to_string()]);
    let ledger = store::load_usage_ledger_months_at(&root, "2026-01", "2026-12", &hidden).unwrap();
    assert!(samples(&ledger).is_empty());
    // Out of range months are not read either.
    let other = store::load_usage_ledger_months_at(&root, "2026-11", "2026-12", &HashSet::new()).unwrap();
    assert!(samples(&other).is_empty());
    assert!(store::purge_external_source(&root, "cc").unwrap());
    assert!(samples(&load(&root)).is_empty());
    assert!(!store::purge_external_source(&root, "cc").unwrap());
    // A rescan after a purge starts over (state was purged too).
    assert_eq!(scan_source_at(&root, &source, "h", now()).unwrap().rows, 1);
}

#[test]
fn source_group_and_labels() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("claude-home");
    append(&home.join("projects/p/s.jsonl"), &[assistant("msg_1", "2026-10-14T10:00:01Z", 5)]);
    scan_source_at(&root, &spec("cc", SourceKind::ClaudeCode, &home), "h", now()).unwrap();
    let ledger = load(&root);
    let mut labels = query::UsageLabels::default();
    labels.sources.insert(
        "cc".into(),
        query::UsageSourceLabel { label: "Laptop Claude".into(), kind: "claude-code".into(), billing: "api".into() },
    );
    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap();
    let request = query::UsageQuery {
        from: day,
        to: day,
        group_by: vec![query::UsageGroupKey::Source, query::UsageGroupKey::Agent, query::UsageGroupKey::Project],
    };
    let result = query::aggregate_usage(&ledger, &request, &labels, None, &|at| at.date_naive());
    let value = serde_json::to_value(&result).unwrap();
    let key = &value["rows"][0]["key"];
    assert_eq!(key["source"], "cc");
    assert_eq!(key["sourceLabel"], "Laptop Claude");
    assert_eq!(key["sourceBilling"], "api");
    assert_eq!(key["sourceKind"], "claude-code");
    assert_eq!(key["agentName"], "Claude Code · demo-app");
    assert_eq!(key["project"], "ext:cc:demo-app");
    assert_eq!(key["projectName"], "demo-app");
    // Unconfigured (deleted without purge): the stored label is used.
    let result = query::aggregate_usage(&ledger, &request, &query::UsageLabels::default(), None, &|at| at.date_naive());
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["rows"][0]["key"]["sourceLabel"], "Claude Code label");
    assert_eq!(value["rows"][0]["key"]["sourceBilling"], "subscription");
}

#[test]
fn overlap_guard_and_validation() {
    let temp = TempDir::new();
    let cutex = temp.path().join("home/.cutex");
    let claude = temp.path().join("home/.claude");
    let nested = cutex.join("profiles/a/claude");
    for dir in [&cutex, &claude, &nested] {
        fs::create_dir_all(dir).unwrap();
    }
    let owned = vec![cutex.clone()];
    assert_eq!(check_overlap(&cutex, &owned, &[]), Err(OverlapError::Cutex(cutex.clone())));
    assert_eq!(check_overlap(&nested, &owned, &[]), Err(OverlapError::Cutex(cutex.clone())));
    // A parent of the cutex home holds its runs too.
    assert_eq!(check_overlap(&temp.path().join("home"), &owned, &[]), Err(OverlapError::Cutex(cutex.clone())));
    assert_eq!(check_overlap(&claude, &owned, &[]), Ok(()));
    let others = vec![("cc".to_string(), claude.clone())];
    assert_eq!(check_overlap(&claude.join("projects"), &owned, &others), Err(OverlapError::Source("cc".into())));
    // A sibling whose name starts the same is not inside.
    let sibling = temp.path().join("home/.cutex-other");
    fs::create_dir_all(&sibling).unwrap();
    assert_eq!(check_overlap(&sibling, &owned, &[]), Ok(()));

    assert!(validate_id("claude-code").is_ok());
    assert!(validate_id("codex-2").is_ok());
    for bad in ["", "-a", "A", "a/b", "..", &"a".repeat(41)] {
        assert!(validate_id(bad).is_err(), "{bad}");
    }
    assert!(validate_label("  ").is_err());
    assert_eq!(validate_label(" Laptop ").unwrap(), "Laptop");
    assert!(normalize_source_path("relative/dir").is_err());
    assert!(normalize_source_path(&temp.path().join("missing").to_string_lossy()).is_err());
    let file = temp.path().join("file.txt");
    fs::write(&file, "x").unwrap();
    assert!(normalize_source_path(&file.to_string_lossy()).is_err());
    assert_eq!(normalize_source_path(&claude.to_string_lossy()).unwrap(), claude.canonicalize().unwrap());
}

#[test]
fn config_entries_parse_leniently() {
    let entry = crate::config::usage_pricing::ExtraUsageSource {
        id: "cc".into(),
        kind: "claude-code".into(),
        path: if cfg!(windows) { r"C:\x".into() } else { "/x".into() },
        label: String::new(),
        billing: "weird".into(),
        enabled: false,
    };
    let spec = SourceSpec::from_config(&entry).unwrap();
    assert_eq!(spec.label, "Claude Code");
    assert_eq!(spec.billing, Billing::Subscription);
    assert!(!spec.enabled);
    let future = crate::config::usage_pricing::ExtraUsageSource { kind: "gemini".into(), ..entry.clone() };
    assert!(SourceSpec::from_config(&future).is_none());
    let relative = crate::config::usage_pricing::ExtraUsageSource { path: "x".into(), ..entry };
    assert!(SourceSpec::from_config(&relative).is_none());

    // The /v2/settings usage patch keeps extraSources untouched.
    let settings = crate::config::usage_pricing::UsageSettings {
        extra_sources: vec![crate::config::usage_pricing::ExtraUsageSource { id: "cc".into(), ..Default::default() }],
        ..Default::default()
    };
    let patched = settings.patched(&json!({"priceSource": "manual"})).unwrap();
    assert_eq!(patched.extra_sources.len(), 1);
    assert!(settings.patched(&json!({"extraSources": []})).is_err());
    assert!(settings.view()["usage"].get("extraSources").is_none());
    let stored = serde_json::to_value(&crate::config::usage_pricing::UsageSettings::default()).unwrap();
    assert!(stored.get("extraSources").is_none());
}

#[test]
fn discovery_ignores_links_and_other_trees() {
    let temp = TempDir::new();
    let home = temp.path().join("h");
    append(&home.join("projects/p/a.jsonl"), &[]);
    append(&home.join("todos/x.jsonl"), &[]);
    #[cfg(unix)]
    {
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("b.jsonl"), "").unwrap();
        std::os::unix::fs::symlink(&outside, home.join("projects/linked")).unwrap();
    }
    let found = discover(SourceKind::ClaudeCode, &home).into_iter().map(|(relative, _)| relative).collect::<Vec<_>>();
    assert_eq!(found, vec!["projects/p/a.jsonl".to_string()]);
}

// ---------------------------------------------------------------- ZCode

/// Same columns as ZCode's `model_usage` (fake rows only).
const ZCODE_SCHEMA: &str = "CREATE TABLE model_usage (
    id TEXT PRIMARY KEY, logical_request_id TEXT NOT NULL, attempt_index INTEGER NOT NULL,
    session_id TEXT NOT NULL, turn_id TEXT, trace_id TEXT, span_id TEXT, assistant_message_id TEXT,
    parent_user_message_id TEXT, query_source TEXT NOT NULL, provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL, variant TEXT, agent TEXT, mode TEXT, task_type TEXT, status TEXT NOT NULL,
    started_at INTEGER NOT NULL, first_token_at INTEGER, completed_at INTEGER, duration_ms INTEGER,
    time_to_first_token_ms INTEGER, finish_reason TEXT, tool_call_count INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, reasoning_tokens INTEGER NOT NULL,
    cache_creation_input_tokens INTEGER NOT NULL, cache_read_input_tokens INTEGER NOT NULL,
    provider_total_tokens INTEGER, computed_total_tokens INTEGER NOT NULL, retry_count INTEGER NOT NULL,
    retryable INTEGER NOT NULL, cancelled_by_user INTEGER NOT NULL, context_exceeded INTEGER NOT NULL,
    error_type TEXT, error_code TEXT, error_message TEXT, raw_usage_json TEXT, provider_metadata_json TEXT)";

fn zcode_ms(at: &str) -> i64 {
    DateTime::parse_from_rfc3339(at).unwrap().timestamp_millis()
}

/// A WAL-mode fixture database; the returned connection is ZCode's writer.
fn zcode_db(home: &Path) -> rusqlite::Connection {
    let dir = home.join("cli/db");
    fs::create_dir_all(&dir).unwrap();
    let writer = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
    writer.pragma_update(None, "journal_mode", "wal").unwrap();
    // Keep committed rows in the -wal file (no checkpoint).
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    writer.execute_batch(ZCODE_SCHEMA).unwrap();
    // Never read: provider config with keys next to the database.
    fs::write(home.join("provider_config.json"), "{\"apiKey\":\"secret\"}").unwrap();
    writer
}

#[allow(clippy::too_many_arguments)]
fn zcode_row(
    writer: &rusqlite::Connection,
    id: &str,
    status: &str,
    completed_at: Option<&str>,
    model: &str,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
) {
    writer
        .execute(
            "INSERT INTO model_usage (id, logical_request_id, attempt_index, session_id, turn_id, query_source,
                provider_id, model_id, status, started_at, completed_at, tool_call_count, input_tokens,
                output_tokens, reasoning_tokens, cache_creation_input_tokens, cache_read_input_tokens,
                provider_total_tokens, computed_total_tokens, retry_count, retryable, cancelled_by_user,
                context_exceeded, error_message, raw_usage_json)
             VALUES (?1, ?2, 0, 'sess_1', 'turn_1', 'main', 'account:fake-plan', ?3, ?4, ?5, ?6, 0, ?7, ?8, 4,
                ?9, ?10, ?11, ?11, 0, 0, 0, 0, 'secret error', '{\"secret\":1}')",
            rusqlite::params![
                id,
                format!("lr-{id}"),
                model,
                status,
                zcode_ms("2026-10-14T09:00:00Z"),
                completed_at.map(zcode_ms),
                input,
                output,
                cache_write,
                cache_read,
                input + output,
            ],
        )
        .unwrap();
}

#[test]
fn zcode_imports_completed_rows_once() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("zcode-home");
    let writer = zcode_db(&home);
    zcode_row(&writer, "mu_1", "completed", Some("2026-10-14T10:00:00Z"), "GLM-5.3", 1000, 50, 800, 0);
    zcode_row(&writer, "mu_2", "completed", Some("2026-10-14T10:05:00Z"), "GLM-5.3-Flash", 200, 10, 0, 0);
    // Anthropic-style counts (input without the cache): cache added to input.
    zcode_row(&writer, "mu_3", "completed", Some("2026-10-14T10:06:00Z"), "GLM-5.3", 10, 5, 300, 20);
    zcode_row(&writer, "mu_run", "running", None, "GLM-5.3", 0, 0, 0, 0);
    zcode_row(&writer, "mu_err", "error", Some("2026-10-14T10:07:00Z"), "GLM-5.3", 500, 0, 0, 0);
    zcode_row(&writer, "mu_cancel", "cancelled", Some("2026-10-14T10:08:00Z"), "GLM-5.3", 500, 9, 0, 0);

    let source = spec("zcode", SourceKind::Zcode, &home);
    let summary = scan_source_at(&root, &source, "host-a", now()).unwrap();
    assert_eq!((summary.files, summary.rows, summary.errors), (1, 3, 0));
    let ledger = load(&root);
    let rows = samples(&ledger);
    assert_eq!(rows.len(), 3);
    let first = rows.iter().find(|row| row.entry_id == "usage:ext:zcode:mu_1").unwrap();
    assert_eq!(first.tokens.input_tokens, 1000);
    assert_eq!(first.tokens.cached_input_tokens, 800);
    assert_eq!(first.tokens.cache_write_input_tokens, 0);
    assert_eq!(first.tokens.output_tokens, 50);
    assert_eq!(first.tokens.reasoning_output_tokens, 4);
    assert_eq!(first.tokens.total_tokens, 1050);
    assert_eq!(first.attribution.provider.as_deref(), Some("account:fake-plan"));
    assert_eq!(first.attribution.model.as_deref(), Some("GLM-5.3"));
    assert_eq!(first.observed_at, "2026-10-14T10:00:00+00:00");
    assert_eq!((first.thread_id.as_str(), first.turn_id.as_str()), ("sess_1", "turn_1"));
    assert_eq!(first.cutex_session_id, "ext:zcode:zcode");
    let origin = first.source.as_ref().unwrap();
    assert_eq!((origin.kind.as_str(), origin.agent_name.as_deref(), origin.project.as_deref()), ("zcode", Some("ZCode"), None));
    let third = rows.iter().find(|row| row.entry_id == "usage:ext:zcode:mu_3").unwrap();
    assert_eq!((third.tokens.input_tokens, third.tokens.cached_input_tokens, third.tokens.cache_write_input_tokens), (330, 300, 20));
    assert_eq!(third.tokens.total_tokens, 335);

    // Re-scan: nothing new.
    assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().rows, 0);
    // A running row completes (with a completed_at before the watermark, as
    // a late commit would) and a new one arrives: both once.
    writer
        .execute(
            "UPDATE model_usage SET status = 'completed', completed_at = ?1, input_tokens = 70, output_tokens = 7 WHERE id = 'mu_run'",
            [zcode_ms("2026-10-14T10:04:00Z")],
        )
        .unwrap();
    zcode_row(&writer, "mu_4", "completed", Some("2026-10-14T10:09:00Z"), "GLM-5.3", 40, 4, 0, 0);
    assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().rows, 2);
    assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().rows, 0);
    let ledger = load(&root);
    assert_eq!(samples(&ledger).len(), 5);
    let raw_lines: usize = fs::read_dir(root.join(store::USAGE_EXTERNAL_DIR).join("zcode"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .map(|entry| fs::read_to_string(entry.path()).unwrap().lines().filter(|line| !line.trim().is_empty()).count())
        .sum();
    assert_eq!(raw_lines, 5, "no row is appended twice");
    let state = load_state(&root, "zcode");
    let cursor = state.zcode.unwrap();
    assert_eq!(cursor.watermark, zcode_ms("2026-10-14T10:09:00Z"));
    assert_eq!(cursor.recent.len(), 5);
    // No secret text reached the ledger.
    let dump = fs::read_dir(root.join(store::USAGE_EXTERNAL_DIR).join("zcode"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect::<String>();
    assert!(!dump.contains("secret"));
    drop(writer);
}

#[test]
fn zcode_reads_while_a_wal_writer_is_open() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("zcode-home");
    let writer = zcode_db(&home);
    zcode_row(&writer, "mu_1", "completed", Some("2026-10-14T10:00:00Z"), "GLM-5.3", 100, 5, 0, 0);
    let db = home.join("cli/db/db.sqlite");
    assert!(db.with_extension("sqlite-wal").exists());
    let main_before = fs::read(&db).unwrap();
    // ZCode holds an open write transaction while cutex reads.
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    zcode_row(&writer, "mu_2", "completed", Some("2026-10-14T10:01:00Z"), "GLM-5.3", 100, 5, 0, 0);
    let source = spec("zcode", SourceKind::Zcode, &home);
    let started = std::time::Instant::now();
    let summary = scan_source_at(&root, &source, "host-a", now()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "the reader never waits for the writer");
    assert_eq!((summary.rows, summary.errors), (1, 0), "committed WAL rows only");
    writer.execute_batch("COMMIT").unwrap();
    assert_eq!(fs::read(&db).unwrap(), main_before, "the reader never checkpoints or writes");
    assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().rows, 1);
    // The writer can still write (no lock left behind).
    zcode_row(&writer, "mu_3", "completed", Some("2026-10-14T10:02:00Z"), "GLM-5.3", 100, 5, 0, 0);
    assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().rows, 1);
}

#[test]
fn zcode_not_running_leaves_no_wal_or_shm_behind() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("zcode-home");
    let writer = zcode_db(&home);
    zcode_row(&writer, "mu_1", "completed", Some("2026-10-14T10:00:00Z"), "GLM-5.3", 100, 5, 0, 0);
    // ZCode quits: its last connection checkpoints and removes -wal / -shm.
    drop(writer);
    let db = home.join("cli/db/db.sqlite");
    let sidecars = |db: &Path| ["db.sqlite-wal", "db.sqlite-shm"].map(|name| db.with_file_name(name).exists());
    assert_eq!(sidecars(&db), [false, false]);
    let main_before = fs::read(&db).unwrap();
    let source = spec("zcode", SourceKind::Zcode, &home);
    let summary = scan_source_at(&root, &source, "host-a", now()).unwrap();
    assert_eq!((summary.rows, summary.errors), (1, 0));
    assert_eq!(sidecars(&db), [false, false], "a read-only scan creates no -wal / -shm");
    assert_eq!(fs::read(&db).unwrap(), main_before);
    // ZCode starts again and writes: the locked read sees its WAL rows.
    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    zcode_row(&writer, "mu_2", "completed", Some("2026-10-14T10:01:00Z"), "GLM-5.3", 100, 5, 0, 0);
    assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().rows, 1);
}

#[test]
fn zcode_missing_database_and_kind_metadata() {
    let temp = TempDir::new();
    let root = temp.path().join("mv2");
    let home = temp.path().join("empty-zcode");
    fs::create_dir_all(&home).unwrap();
    let source = spec("zcode", SourceKind::Zcode, &home);
    let summary = scan_source_at(&root, &source, "host-a", now()).unwrap();
    assert_eq!((summary.files, summary.rows, summary.errors), (0, 0, 1));
    assert!(load_state(&root, "zcode").errors[0].starts_with("cli/db/db.sqlite: "));
    #[cfg(unix)]
    {
        // A linked database is refused.
        let outside = temp.path().join("outside");
        zcode_db(&outside);
        fs::create_dir_all(home.join("cli")).unwrap();
        std::os::unix::fs::symlink(outside.join("cli/db"), home.join("cli/db")).unwrap();
        assert_eq!(scan_source_at(&root, &source, "host-a", now()).unwrap().errors, 1);
    }

    assert_eq!(SourceKind::parse("zcode"), Some(SourceKind::Zcode));
    assert_eq!(SourceKind::ALL.map(SourceKind::key), ["claude-code", "codex", "zcode"]);
    assert!(defaults_json().as_array().unwrap().iter().any(|item| item["kind"] == "zcode"
        && item["path"].as_str().is_some_and(|path| path.ends_with(".zcode"))));
    assert_eq!(zcode::timestamp(1_792_028_037_665).as_deref(), Some("2026-10-15T01:33:57.665+00:00"));
    assert_eq!(zcode::timestamp(1_792_028_037).as_deref(), Some("2026-10-15T01:33:57+00:00"));
    let entry = crate::config::usage_pricing::ExtraUsageSource {
        id: "zc".into(),
        kind: "zcode".into(),
        path: if cfg!(windows) { r"C:\x".into() } else { "/x".into() },
        ..Default::default()
    };
    assert_eq!(SourceSpec::from_config(&entry).unwrap().label, "ZCode");
}
