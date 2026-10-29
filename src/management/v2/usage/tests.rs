use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use serde_json::json;
use serde_json::Value;
use uuid::Uuid;

use super::*;
use crate::management::v2::model::EventCorrelation;
use crate::management::v2::model::EventEnvelope;
use crate::management::v2::model::EventSource;
use crate::management::v2::model::NativeMessage;
use crate::management::v2::model::NativeMessageKind;
use crate::management::v2::model::CONTRACT_VERSION;

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!("cutex-usage-{label}-{}", Uuid::new_v4())))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn envelope(
    event_id: &str,
    sequence: u64,
    runtime_generation: u64,
    received_at: &str,
    method: &str,
    params: Value,
) -> EventEnvelope {
    EventEnvelope {
        contract_version: CONTRACT_VERSION,
        event_id: event_id.to_string(),
        cursor: format!("stream:{sequence}"),
        stream_id: "stream".to_string(),
        sequence,
        received_at: received_at.to_string(),
        cutex_session_id: "cutex.session-a".to_string(),
        host_id: "test-host".to_string(),
        source: EventSource::AppServer,
        sensitivity: "owner".to_string(),
        schema: None,
        correlation: EventCorrelation {
            runtime_generation: Some(runtime_generation),
            ..EventCorrelation::default()
        },
        native: Some(NativeMessage {
            kind: NativeMessageKind::Notification,
            message: json!({ "method": method, "params": params }),
        }),
        cutex: None,
    }
}

fn breakdown(
    total: u64,
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
) -> Value {
    json!({
        "totalTokens": total,
        "inputTokens": input,
        "cachedInputTokens": cached,
        "cacheWriteInputTokens": cache_write,
        "outputTokens": output,
        "reasoningOutputTokens": reasoning
    })
}

fn token_params(total: Value, last: Option<Value>, turn_id: &str) -> Value {
    let mut token_usage = json!({
        "total": total,
        "modelContextWindow": 400_000
    });
    if let Some(last) = last {
        token_usage["last"] = last;
    }
    json!({
        "threadId": "thread-a",
        "turnId": turn_id,
        "tokenUsage": token_usage
    })
}

fn token_params_with_compaction_count(
    total: Value,
    last: Option<Value>,
    turn_id: &str,
    compaction_count: u64,
) -> Value {
    let mut params = token_params(total, last, turn_id);
    params["tokenUsage"]["compactionCount"] = json!(compaction_count);
    params
}

fn usage_samples(ledger: &UsageLedger) -> Vec<&UsageSample> {
    ledger
        .entries
        .iter()
        .filter_map(|entry| match entry {
            UsageLedgerEntry::Usage(sample) => Some(sample.as_ref()),
            UsageLedgerEntry::ResetBoundary(_) => None,
        })
        .collect()
}

fn reset_boundaries(ledger: &UsageLedger) -> Vec<&UsageResetBoundary> {
    ledger
        .entries
        .iter()
        .filter_map(|entry| match entry {
            UsageLedgerEntry::Usage(_) => None,
            UsageLedgerEntry::ResetBoundary(boundary) => Some(boundary),
        })
        .collect()
}

#[test]
fn deleted_session_late_events_cannot_recreate_baseline_or_duplicate_ledger() {
    let root = TestRoot::new("deleted-late-event");
    let initial = envelope("first", 1, 1, "2026-08-13T01:00:00Z", "thread/tokenUsage/updated",
        token_params(breakdown(100, 80, 0, 0, 20, 0), Some(breakdown(100, 80, 0, 0, 20, 0)), "turn-1"));
    record_usage_event_at(root.path(), &initial, Some("work")).unwrap();
    let before = store::load_usage_ledger_at(root.path()).unwrap();
    store::forget_deleted_session_at(root.path(), "cutex.session-a").unwrap();
    let revision = store::load_usage_state_snapshot_at(root.path()).unwrap().revision;
    for generation in [1, 2, 100] {
        let late = envelope("late", generation + 10, generation, "2026-10-13T01:00:00Z", "thread/tokenUsage/updated",
            token_params(breakdown(200, 160, 0, 0, 40, 0), Some(breakdown(100, 80, 0, 0, 20, 0)), "turn-late"));
        record_usage_event_at(root.path(), &late, Some("work")).unwrap();
    }
    let snapshot = store::load_usage_state_snapshot_at(root.path()).unwrap();
    let persisted: Value = serde_json::from_slice(&fs::read(store::state_path(root.path())).unwrap()).unwrap();
    assert!(persisted["sessions"].get("cutex.session-a").is_none());
    assert_eq!(snapshot.revision, revision);
    assert_eq!(store::load_usage_ledger_at(root.path()).unwrap().entries.len(), before.entries.len());
    // Unrelated sessions continue recording normally.
    let mut other = initial;
    other.cutex_session_id = "cutex.session-b".into();
    record_usage_event_at(root.path(), &other, Some("work")).unwrap();
    let persisted: Value = serde_json::from_slice(&fs::read(store::state_path(root.path())).unwrap()).unwrap();
    assert!(persisted["sessions"].get("cutex.session-b").is_some());
}

#[test]
fn token_usage_baselines_then_records_native_last_with_attribution() {
    let root = TestRoot::new("native-last");
    record_usage_event_at(
        root.path(),
        &envelope(
            "settings-1",
            1,
            1,
            "2026-08-13T01:00:00Z",
            "thread/settings/updated",
            json!({
                "threadId": "thread-a",
                "threadSettings": {
                    "modelProvider": "openai",
                    "model": "gpt-5.6-terra",
                    "effort": "xhigh",
                    "serviceTier": "fast"
                }
            }),
        ),
        Some("work"),
    )
    .expect("record settings");
    record_usage_event_at(
        root.path(),
        &envelope(
            "usage-1",
            2,
            1,
            "2026-08-13T01:01:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(100, 80, 50, 3, 20, 5), None, "turn-a"),
        ),
        Some("work"),
    )
    .expect("establish baseline");
    record_usage_event_at(
        root.path(),
        &envelope(
            "usage-2",
            3,
            1,
            "2026-08-13T01:02:00Z",
            "thread/tokenUsage/updated",
            token_params(
                breakdown(130, 104, 65, 4, 26, 7),
                Some(breakdown(30, 24, 15, 1, 6, 2)),
                "turn-a",
            ),
        ),
        Some("work"),
    )
    .expect("record usage delta");

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let samples = usage_samples(&ledger);
    assert_eq!(samples.len(), 1);
    let sample = samples[0];
    assert_eq!(sample.entry_id, "usage:usage-2");
    assert_eq!(sample.profile.as_deref(), Some("work"));
    assert_eq!(sample.attribution.provider.as_deref(), Some("openai"));
    assert_eq!(sample.attribution.model.as_deref(), Some("gpt-5.6-terra"));
    assert_eq!(
        sample.attribution.reasoning_effort.as_deref(),
        Some("xhigh")
    );
    assert_eq!(sample.attribution.service_tier.as_deref(), Some("fast"));
    assert_eq!(sample.token_source, UsageTokenSource::NativeLast);
    assert_eq!(sample.tokens.input_tokens, 24);
    assert_eq!(sample.tokens.cached_input_tokens, 15);
    assert_eq!(sample.tokens.cache_write_input_tokens, 1);
    assert_eq!(sample.tokens.output_tokens, 6);
    assert_eq!(sample.tokens.reasoning_output_tokens, 2);
    assert_eq!(sample.cumulative_total.total_tokens, 130);
}

#[test]
fn token_usage_compaction_count_is_optional_and_persists() {
    let root = TestRoot::new("compaction-count");
    record_usage_event_at(
        root.path(),
        &envelope(
            "usage-1",
            1,
            1,
            "2026-08-13T01:00:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(100, 80, 50, 0, 20, 4), None, "turn-a"),
        ),
        None,
    )
    .expect("establish legacy baseline");
    let legacy_ledger = store::load_usage_ledger_at(root.path()).expect("load legacy ledger");
    assert!(usage_samples(&legacy_ledger).is_empty());

    record_usage_event_at(
        root.path(),
        &envelope(
            "usage-2",
            2,
            1,
            "2026-08-13T01:01:00Z",
            "thread/tokenUsage/updated",
            token_params_with_compaction_count(
                breakdown(120, 96, 60, 0, 24, 6),
                Some(breakdown(20, 16, 10, 0, 4, 2)),
                "turn-a",
                2,
            ),
        ),
        None,
    )
    .expect("record compacted usage delta");

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let samples = usage_samples(&ledger);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].compaction_count, Some(2));
}

#[test]
fn replayed_total_is_ignored_and_missing_last_uses_cumulative_delta() {
    let root = TestRoot::new("replay");
    for event in [
        envelope(
            "usage-1",
            1,
            1,
            "2026-08-13T02:00:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(100, 80, 50, 0, 20, 4), None, "turn-a"),
        ),
        envelope(
            "usage-2",
            2,
            1,
            "2026-08-13T02:01:00Z",
            "thread/tokenUsage/updated",
            token_params(
                breakdown(130, 104, 65, 0, 26, 6),
                Some(breakdown(30, 24, 15, 0, 6, 2)),
                "turn-a",
            ),
        ),
        envelope(
            "usage-replay",
            3,
            1,
            "2026-08-13T02:02:00Z",
            "thread/tokenUsage/updated",
            token_params(
                breakdown(130, 104, 65, 0, 26, 6),
                Some(breakdown(30, 24, 15, 0, 6, 2)),
                "turn-a",
            ),
        ),
        envelope(
            "usage-3",
            4,
            1,
            "2026-08-13T02:03:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(150, 120, 75, 0, 30, 7), None, "turn-b"),
        ),
    ] {
        record_usage_event_at(root.path(), &event, Some("work")).expect("record event");
    }

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let samples = usage_samples(&ledger);
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].entry_id, "usage:usage-2");
    assert_eq!(samples[1].entry_id, "usage:usage-3");
    assert_eq!(samples[1].token_source, UsageTokenSource::CumulativeDelta);
    assert_eq!(
        samples[1].tokens,
        TokenUsageBreakdown {
            total_tokens: 20,
            input_tokens: 16,
            cached_input_tokens: 10,
            cache_write_input_tokens: 0,
            output_tokens: 4,
            reasoning_output_tokens: 1,
        }
    );
}

#[test]
fn stale_generation_and_counter_regression_do_not_create_usage() {
    let root = TestRoot::new("generation");
    for event in [
        envelope(
            "usage-gen2-baseline",
            1,
            2,
            "2026-08-13T03:00:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(100, 80, 50, 0, 20, 4), None, "turn-a"),
        ),
        envelope(
            "usage-stale",
            99,
            1,
            "2026-08-13T03:01:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(200, 160, 100, 0, 40, 8), None, "turn-a"),
        ),
        envelope(
            "usage-gen3-baseline",
            1,
            3,
            "2026-08-13T03:02:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(50, 40, 25, 0, 10, 2), None, "turn-b"),
        ),
        envelope(
            "usage-gen3-delta",
            2,
            3,
            "2026-08-13T03:03:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(60, 48, 30, 0, 12, 3), None, "turn-b"),
        ),
    ] {
        record_usage_event_at(root.path(), &event, Some("work")).expect("record event");
    }

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let samples = usage_samples(&ledger);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].event_id, "usage-gen3-delta");
    assert_eq!(samples[0].tokens.total_tokens, 10);
}

#[test]
fn rerouted_model_applies_only_to_its_turn() {
    let root = TestRoot::new("reroute");
    for event in [
        envelope(
            "settings",
            1,
            1,
            "2026-08-13T04:00:00Z",
            "thread/settings/updated",
            json!({
                "threadId": "thread-a",
                "threadSettings": { "model": "gpt-default" }
            }),
        ),
        envelope(
            "baseline",
            2,
            1,
            "2026-08-13T04:01:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(100, 80, 50, 0, 20, 4), None, "turn-a"),
        ),
        envelope(
            "reroute",
            3,
            1,
            "2026-08-13T04:02:00Z",
            "model/rerouted",
            json!({
                "threadId": "thread-a",
                "turnId": "turn-a",
                "toModel": "gpt-fallback"
            }),
        ),
        envelope(
            "turn-a-usage",
            4,
            1,
            "2026-08-13T04:03:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(110, 88, 55, 0, 22, 5), None, "turn-a"),
        ),
        envelope(
            "turn-b-usage",
            5,
            1,
            "2026-08-13T04:04:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(120, 96, 60, 0, 24, 6), None, "turn-b"),
        ),
    ] {
        record_usage_event_at(root.path(), &event, Some("work")).expect("record event");
    }

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let samples = usage_samples(&ledger);
    assert_eq!(samples.len(), 2);
    assert_eq!(
        samples[0].attribution.model.as_deref(),
        Some("gpt-fallback")
    );
    assert_eq!(samples[1].attribution.model.as_deref(), Some("gpt-default"));
}

#[test]
fn profile_change_keeps_samples_isolated_across_runtime_generations() {
    let root = TestRoot::new("profiles");
    for (profile, event) in [
        (
            "default",
            envelope(
                "profile-a-baseline",
                1,
                1,
                "2026-08-13T04:10:00Z",
                "thread/tokenUsage/updated",
                token_params(breakdown(100, 80, 50, 0, 20, 4), None, "turn-a"),
            ),
        ),
        (
            "default",
            envelope(
                "profile-a-usage",
                2,
                1,
                "2026-08-13T04:11:00Z",
                "thread/tokenUsage/updated",
                token_params(breakdown(110, 88, 55, 0, 22, 5), None, "turn-a"),
            ),
        ),
        (
            "deepseek",
            envelope(
                "profile-b-replay",
                1,
                2,
                "2026-08-13T04:12:00Z",
                "thread/tokenUsage/updated",
                token_params(breakdown(110, 88, 55, 0, 22, 5), None, "turn-b"),
            ),
        ),
        (
            "deepseek",
            envelope(
                "profile-b-usage",
                2,
                2,
                "2026-08-13T04:13:00Z",
                "thread/tokenUsage/updated",
                token_params(breakdown(120, 96, 60, 0, 24, 6), None, "turn-b"),
            ),
        ),
    ] {
        record_usage_event_at(root.path(), &event, Some(profile)).expect("record event");
    }

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let samples = usage_samples(&ledger);
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].profile.as_deref(), Some("default"));
    assert_eq!(samples[1].profile.as_deref(), Some("deepseek"));
}

#[test]
fn rate_limit_events_store_reset_boundaries_without_subscription_percentage() {
    let root = TestRoot::new("reset");
    for (event_id, sequence, resets_at, used_percent) in [
        ("limits-1", 1, 1_786_600_000_i64, 42.5),
        ("limits-same", 2, 1_786_600_000_i64, 43.0),
        ("limits-2", 3, 1_787_204_800_i64, 2.0),
    ] {
        record_usage_event_at(
            root.path(),
            &envelope(
                event_id,
                sequence,
                1,
                &format!("2026-08-13T05:0{}:00Z", sequence - 1),
                "account/rateLimits/updated",
                json!({
                    "rateLimits": {
                        "limitId": "codex",
                        "primary": {
                            "usedPercent": used_percent,
                            "windowDurationMins": 10_080,
                            "resetsAt": resets_at
                        }
                    }
                }),
            ),
            Some("work"),
        )
        .expect("record reset boundary");
    }

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let boundaries = reset_boundaries(&ledger);
    assert_eq!(boundaries.len(), 2);
    assert_eq!(boundaries[0].profile.as_deref(), Some("work"));
    assert_eq!(boundaries[0].window_kind, ResetWindowKind::Primary);
    assert_eq!(boundaries[0].window_duration_mins, 10_080);
    assert_eq!(boundaries[1].resets_at, 1_787_204_800);

    let state = fs::read_to_string(store::state_path(root.path())).expect("read state");
    let ledger_text =
        fs::read_to_string(root.path().join("usage-ledger/2026-08.jsonl")).expect("read ledger");
    assert!(!state.contains("usedPercent"));
    assert!(!ledger_text.contains("usedPercent"));
}

#[test]
fn detailed_zero_counter_updates_are_treated_as_synthetic_baselines() {
    let root = TestRoot::new("synthetic");
    for event in [
        envelope(
            "synthetic-1",
            1,
            1,
            "2026-08-13T06:00:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(100, 0, 0, 0, 0, 0), None, "turn-a"),
        ),
        envelope(
            "synthetic-2",
            2,
            1,
            "2026-08-13T06:01:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(200, 0, 0, 0, 0, 0), None, "turn-a"),
        ),
    ] {
        record_usage_event_at(root.path(), &event, None).expect("record synthetic event");
    }

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    assert!(usage_samples(&ledger).is_empty());
}

#[test]
fn pending_entries_recover_after_interrupted_append_and_reader_deduplicates() {
    let root = TestRoot::new("pending");
    fs::create_dir_all(root.path()).expect("create root");
    let entry = UsageLedgerEntry::Usage(Box::new(UsageSample {
        entry_id: "usage:pending".to_string(),
        event_id: "pending".to_string(),
        observed_at: "2026-08-13T07:00:00Z".to_string(),
        host_id: "test-host".to_string(),
        cutex_session_id: "cutex.session-a".to_string(),
        thread_id: "thread-a".to_string(),
        turn_id: "turn-a".to_string(),
        runtime_generation: 1,
        profile: Some("work".to_string()),
        profile_id: None,
        profile_deleted: false,
        source: None,
        attribution: UsageAttribution::default(),
        token_source: UsageTokenSource::CumulativeDelta,
        model_context_window: None,
        compaction_count: None,
        model_requests: None,
        tokens: TokenUsageBreakdown {
            total_tokens: 10,
            input_tokens: 8,
            cached_input_tokens: 5,
            cache_write_input_tokens: 0,
            output_tokens: 2,
            reasoning_output_tokens: 1,
        },
        cumulative_total: TokenUsageBreakdown {
            total_tokens: 110,
            input_tokens: 88,
            cached_input_tokens: 55,
            cache_write_input_tokens: 0,
            output_tokens: 22,
            reasoning_output_tokens: 5,
        },
    }));
    let mut state = model::UsageStateFile::default();
    state
        .mark_changed("2026-08-13T07:00:00Z")
        .expect("mark state changed");
    state.pending_entries.push(entry.clone());
    store::write_usage_state(&store::state_path(root.path()), &state).expect("write pending state");

    let first = store::load_usage_ledger_at(root.path()).expect("recover pending entry");
    assert_eq!(first.entries, vec![entry.clone()]);
    assert!(store::load_usage_state(&store::state_path(root.path()))
        .expect("load recovered state")
        .pending_entries
        .is_empty());

    state.pending_entries.push(entry.clone());
    store::write_usage_state(&store::state_path(root.path()), &state)
        .expect("simulate uncleared pending state");
    let second = store::load_usage_ledger_at(root.path()).expect("recover duplicate entry");
    assert_eq!(second.entries, vec![entry]);
    assert!(second.warnings.is_empty());
}

#[test]
fn invalid_ledger_lines_are_reported_without_hiding_valid_history() {
    let root = TestRoot::new("invalid-line");
    record_usage_event_at(
        root.path(),
        &envelope(
            "baseline",
            1,
            1,
            "2026-08-13T08:00:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(100, 80, 50, 0, 20, 4), None, "turn-a"),
        ),
        None,
    )
    .expect("record baseline");
    record_usage_event_at(
        root.path(),
        &envelope(
            "usage",
            2,
            1,
            "2026-08-13T08:01:00Z",
            "thread/tokenUsage/updated",
            token_params(breakdown(110, 88, 55, 0, 22, 5), None, "turn-a"),
        ),
        None,
    )
    .expect("record sample");
    let ledger_path = root.path().join("usage-ledger/2026-08.jsonl");
    fs::OpenOptions::new()
        .append(true)
        .open(&ledger_path)
        .expect("open ledger")
        .write_all(b"\n{not-json}\n")
        .expect("append invalid line");

    let ledger = store::load_usage_ledger_at(root.path()).expect("load partial ledger");
    assert_eq!(usage_samples(&ledger).len(), 1);
    assert_eq!(ledger.warnings.len(), 1);
    assert!(ledger.warnings[0].contains("ignored invalid usage ledger JSON"));
}

#[test]
fn corrupt_reducer_state_is_not_silently_replaced() {
    let root = TestRoot::new("corrupt-state");
    fs::create_dir_all(root.path()).expect("create root");
    fs::write(store::state_path(root.path()), b"{not-json}").expect("write corrupt state");

    let error = store::load_usage_ledger_at(root.path()).expect_err("reject corrupt state");
    assert!(format!("{error:#}").contains("Failed to parse management v2 usage"));
    assert_eq!(
        fs::read(store::state_path(root.path())).expect("read corrupt state"),
        b"{not-json}"
    );
}

#[test]
fn claude_adapter_usage_counts_the_cumulative_delta_not_context_occupancy() {
    let root = TestRoot::new("claude-adapter");
    let claude = |event_id: &str, sequence: u64, params: Value| {
        let mut event = envelope(
            event_id,
            sequence,
            1,
            "2026-10-09T13:34:09Z",
            "thread/tokenUsage/updated",
            params,
        );
        event.schema = Some(crate::management::v2::model::AppServerSchema {
            protocol: "claude-app-server-subset".to_string(),
            major_version: 1,
            version: "0.3.0+cutex".to_string(),
            sha256: "0".repeat(64),
            channel: crate::management::v2::model::AppServerSchemaChannel::Experimental,
            capabilities: json!({}),
            extensions: vec!["claude-runtime-v1".to_string()],
        });
        event
    };
    // Baseline, then one turn of two model requests: `last` is the context
    // size of the final request (no output), `total` the billing sum.
    record_usage_event_at(
        root.path(),
        &claude("usage-1", 1, token_params(breakdown(1_000, 900, 600, 0, 100, 0), None, "turn-a")),
        Some("deepseek"),
    )
    .expect("baseline");
    record_usage_event_at(
        root.path(),
        &claude("usage-2", 2, {
            let mut params = token_params(
                breakdown(1_430, 1_310, 900, 0, 120, 0),
                Some(breakdown(410, 410, 0, 0, 0, 0)),
                "turn-b",
            );
            // The turn's SDK steps (token audit 2026-10-22).
            params["tokenUsage"]["modelRequests"] = json!(2);
            params
        }),
        Some("deepseek"),
    )
    .expect("record turn");

    let ledger = store::load_usage_ledger_at(root.path()).expect("load ledger");
    let samples = usage_samples(&ledger);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].token_source, UsageTokenSource::CumulativeDelta);
    assert_eq!(samples[0].tokens.input_tokens, 410);
    assert_eq!(samples[0].tokens.cached_input_tokens, 300);
    assert_eq!(samples[0].tokens.output_tokens, 20);
    assert_eq!(samples[0].model_requests, Some(2), "the turn's SDK steps are kept");
}
