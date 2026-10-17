//! Tests for the desktop usage surface: fresh-thread recording, `/v2/usage`
//! aggregation, owner prices, month rotation and quota snapshots.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::FixedOffset;
use chrono::NaiveDate;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;
use uuid::Uuid;

use super::prices::load_price_table_at;
use super::prices::save_price_table_at;
use super::quota_history::read_quota_history_at;
use super::quota_history::record_snapshots_at;
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
        Self(std::env::temp_dir().join(format!("cutex-usage-q-{label}-{}", Uuid::new_v4())))
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
    session: &str,
    event_id: &str,
    sequence: u64,
    received_at: &str,
    method: &str,
    params: Value,
) -> EventEnvelope {
    EventEnvelope {
        contract_version: CONTRACT_VERSION,
        event_id: event_id.to_string(),
        cursor: format!("stream-{session}:{sequence}"),
        stream_id: format!("stream-{session}"),
        sequence,
        received_at: received_at.to_string(),
        cutex_session_id: session.to_string(),
        host_id: "test-host".to_string(),
        source: EventSource::AppServer,
        sensitivity: "owner".to_string(),
        schema: None,
        correlation: EventCorrelation {
            runtime_generation: Some(1),
            ..EventCorrelation::default()
        },
        native: Some(NativeMessage {
            kind: NativeMessageKind::Notification,
            message: json!({ "method": method, "params": params }),
        }),
        cutex: None,
    }
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    json!({
        "totalTokens": input + output,
        "inputTokens": input,
        "cachedInputTokens": cached,
        "cacheWriteInputTokens": 0,
        "outputTokens": output,
        "reasoningOutputTokens": reasoning
    })
}

fn token_event(
    session: &str,
    event_id: &str,
    sequence: u64,
    at: &str,
    turn: &str,
    total: Value,
    last: Value,
) -> EventEnvelope {
    envelope(
        session,
        event_id,
        sequence,
        at,
        "thread/tokenUsage/updated",
        json!({
            "threadId": format!("thread-{session}"),
            "turnId": turn,
            "tokenUsage": { "total": total, "last": last, "modelContextWindow": 400000 }
        }),
    )
}

fn settings_event(session: &str, sequence: u64, at: &str, model: &str) -> EventEnvelope {
    envelope(
        session,
        &format!("settings-{session}-{sequence}"),
        sequence,
        at,
        "thread/settings/updated",
        json!({
            "threadId": format!("thread-{session}"),
            "threadSettings": { "modelProvider": "openai", "model": model }
        }),
    )
}

fn d(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
}

/// Australian Eastern Daylight Time, so UTC evenings fall on the next day.
fn aedt(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&FixedOffset::east_opt(11 * 3600).unwrap())
        .date_naive()
}

#[test]
fn fresh_thread_first_request_is_recorded_once() {
    let root = TestRoot::new("fresh");
    let first = token_event(
        "cutex.a",
        "u1",
        2,
        "2026-10-05T01:00:00Z",
        "turn-1",
        usage(100, 0, 10, 2),
        usage(100, 0, 10, 2),
    );
    record_usage_event_at(
        root.path(),
        &settings_event("cutex.a", 1, "2026-10-05T00:59:00Z", "gpt-5.5"),
        Some("work"),
    )
    .unwrap();
    record_usage_event_at(root.path(), &first, Some("work")).unwrap();
    // Replay of the same event and a same-total duplicate add nothing.
    record_usage_event_at(root.path(), &first, Some("work")).unwrap();
    record_usage_event_at(
        root.path(),
        &token_event(
            "cutex.a",
            "u1-dup",
            3,
            "2026-10-05T01:00:01Z",
            "turn-1",
            usage(100, 0, 10, 2),
            usage(100, 0, 10, 2),
        ),
        Some("work"),
    )
    .unwrap();
    // Resumed thread: first observation has total > last -> baseline only.
    record_usage_event_at(
        root.path(),
        &token_event(
            "cutex.b",
            "r1",
            1,
            "2026-10-05T01:00:00Z",
            "turn-x",
            usage(5000, 0, 500, 0),
            usage(100, 0, 10, 0),
        ),
        Some("work"),
    )
    .unwrap();
    let ledger = store::load_usage_ledger_at(root.path()).unwrap();
    let samples = ledger
        .entries
        .iter()
        .filter_map(|entry| match entry {
            UsageLedgerEntry::Usage(sample) => Some(sample),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].cutex_session_id, "cutex.a");
    assert_eq!(samples[0].tokens.input_tokens, 100);
    assert_eq!(samples[0].attribution.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(samples[0].token_source, UsageTokenSource::NativeLast);
}

/// Two sessions, three turns, four requests across two local days and two
/// month files.
fn populated_root(label: &str) -> TestRoot {
    let root = TestRoot::new(label);
    let events = [
        (
            settings_event("cutex.a", 1, "2026-09-30T10:00:00Z", "gpt-5.5"),
            "work",
        ),
        // 2026-09-30T12:30Z is 2026-09-30 23:30 AEDT; month file 2026-09.
        (
            token_event(
                "cutex.a",
                "a1",
                2,
                "2026-09-30T12:30:00Z",
                "t1",
                usage(1000, 400, 100, 20),
                usage(1000, 400, 100, 20),
            ),
            "work",
        ),
        // Same turn, second request, after local midnight (2026-10-01 AEDT).
        (
            token_event(
                "cutex.a",
                "a2",
                3,
                "2026-09-30T13:30:00Z",
                "t1",
                usage(3000, 1400, 300, 60),
                usage(2000, 1000, 200, 40),
            ),
            "work",
        ),
        (
            token_event(
                "cutex.a",
                "a3",
                4,
                "2026-10-01T02:00:00Z",
                "t2",
                usage(3500, 1400, 350, 60),
                usage(500, 0, 50, 0),
            ),
            "work",
        ),
        // Session b: no model known, other profile.
        (
            token_event(
                "cutex.b",
                "b1",
                1,
                "2026-10-01T03:00:00Z",
                "t9",
                usage(200, 0, 20, 0),
                usage(200, 0, 20, 0),
            ),
            "glm",
        ),
    ];
    for (event, profile) in events {
        record_usage_event_at(root.path(), &event, Some(profile)).unwrap();
    }
    root
}

#[test]
fn ledger_rotates_by_month_and_month_filter_reads_only_selected_files() {
    let root = populated_root("rotation");
    let dir = root.path().join("usage-ledger");
    assert!(dir.join("2026-09.jsonl").is_file());
    assert!(dir.join("2026-10.jsonl").is_file());
    let october = store::load_usage_ledger_months_at(root.path(), "2026-10", "2026-10", &Default::default()).unwrap();
    assert_eq!(october.entries.len(), 2);
    let both = store::load_usage_ledger_months_at(root.path(), "2026-09", "2026-10", &Default::default()).unwrap();
    assert_eq!(both.entries.len(), 4);
    assert_eq!(
        query::month_bounds(d("2026-10-01"), d("2026-10-31")),
        ("2026-09".to_string(), "2026-11".to_string())
    );
}

#[test]
fn aggregation_groups_by_local_day_and_profile_and_counts_turns() {
    let root = populated_root("agg");
    let ledger = store::load_usage_ledger_at(root.path()).unwrap();
    let query = UsageQuery {
        from: d("2026-09-30"),
        to: d("2026-10-01"),
        group_by: vec![UsageGroupKey::Day, UsageGroupKey::Profile],
    };
    let result = aggregate_usage(&ledger, &query, &UsageLabels::default(), None, &aedt);
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["contractVersion"], 2);
    assert_eq!(value["groupBy"], json!(["day", "profile"]));
    assert_eq!(value["totals"]["turns"], 3);
    assert_eq!(value["totals"]["requests"], 4);
    assert_eq!(value["totals"]["input"], 3700);
    assert_eq!(value["totals"]["cachedInput"], 1400);
    assert_eq!(value["totals"]["output"], 370);
    assert_eq!(value["totals"]["reasoningOutput"], 60);
    assert_eq!(value["totals"]["total"], 4070);
    assert!(value["totals"].get("estimatedCost").is_none());
    assert!(value.get("currency").is_none());
    let rows = value["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0]["key"],
        json!({ "day": "2026-09-30", "profile": "work", "profileId": null, "profileState": "current" })
    );
    assert_eq!(rows[0]["turns"], 1);
    assert_eq!(rows[0]["input"], 1000);
    // Turn t1 straddles local midnight: counted in both day rows.
    assert_eq!(
        rows[1]["key"],
        json!({ "day": "2026-10-01", "profile": "glm", "profileId": null, "profileState": "current" })
    );
    assert_eq!(
        rows[2]["key"],
        json!({ "day": "2026-10-01", "profile": "work", "profileId": null, "profileState": "current" })
    );
    assert_eq!(rows[2]["turns"], 2);
    assert_eq!(rows[2]["input"], 2500);

    // Range filter by local day.
    let only_first = UsageQuery {
        from: d("2026-09-30"),
        to: d("2026-09-30"),
        group_by: vec![UsageGroupKey::Model],
    };
    let result = aggregate_usage(&ledger, &only_first, &UsageLabels::default(), None, &aedt);
    assert_eq!(result.totals.requests, 1);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        serde_json::to_value(&result.rows[0].key).unwrap(),
        json!({ "model": "gpt-5.5" })
    );
}

#[test]
fn aggregation_by_agent_and_project_uses_labels_and_nulls() {
    let root = populated_root("labels");
    let ledger = store::load_usage_ledger_at(root.path()).unwrap();
    let mut labels = UsageLabels::default();
    labels
        .agent_names
        .insert("cutex.a".to_string(), "Alice".to_string());
    labels.projects.insert(
        "cutex.a".to_string(),
        ("proj-1".to_string(), "Project One".to_string()),
    );
    let query = UsageQuery {
        from: d("2026-09-01"),
        to: d("2026-10-31"),
        group_by: vec![
            UsageGroupKey::Project,
            UsageGroupKey::Agent,
            UsageGroupKey::Model,
        ],
    };
    let result = aggregate_usage(&ledger, &query, &labels, None, &aedt);
    let rows = serde_json::to_value(&result.rows).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(
        rows[0]["key"],
        json!({ "project": null, "projectName": null, "agent": "cutex.b", "agentName": "cutex.b", "model": null })
    );
    assert_eq!(
        rows[1]["key"],
        json!({ "project": "proj-1", "projectName": "Project One", "agent": "cutex.a", "agentName": "Alice", "model": "gpt-5.5" })
    );
    assert_eq!(rows[1]["turns"], 2);
}

#[test]
fn owner_prices_validate_estimate_and_round_trip() {
    let root = TestRoot::new("prices");
    let path = root.path().join("usage-prices.json");
    assert_eq!(
        load_price_table_at(&path).unwrap(),
        UsagePriceTable::default()
    );

    let table: UsagePriceTable = serde_json::from_value(json!({
        "currency": "USD",
        "models": { "GPT-5.5": { "input": 2.0, "cachedInput": 0.5, "output": 10.0 } }
    }))
    .unwrap();
    table.validate().unwrap();
    save_price_table_at(&path, &table, &crate::config::usage_pricing::ExchangeRates::default()).unwrap();
    assert_eq!(load_price_table_at(&path).unwrap(), table);
    assert!(table.price_for(Some("openai"), Some("gpt-5.5")).is_some());
    assert!(table.price_for(None, None).is_none());

    let tokens = TokenUsageBreakdown {
        total_tokens: 1_100_000,
        input_tokens: 1_000_000,
        cached_input_tokens: 400_000,
        cache_write_input_tokens: 0,
        output_tokens: 100_000,
        reasoning_output_tokens: 50_000,
    };
    let (_, price) = table.price_for(None, Some("gpt-5.5")).unwrap();
    // 0.6M*2 + 0.4M*0.5 + 0.1M*10 = 1.2 + 0.2 + 1.0
    assert!((estimate_cost(price, &tokens) - 2.4).abs() < 1e-9);

    for bad in [
        json!({ "currency": "", "models": {} }),
        json!({ "currency": "US D", "models": {} }),
        json!({ "currency": "USD", "models": { "m": { "input": -1.0, "output": 1.0 } } }),
        json!({ "currency": "USD", "models": { " m": { "input": 1.0, "output": 1.0 } } }),
        json!({ "currency": "USD", "models": { "a": { "input": 1.0, "output": 1.0 }, "A": { "input": 1.0, "output": 1.0 } } }),
    ] {
        let table: UsagePriceTable = serde_json::from_value(bad.clone()).unwrap();
        assert!(table.validate().is_err(), "{bad}");
    }
    assert!(serde_json::from_value::<UsagePriceTable>(json!({
        "currency": "USD", "models": { "m": { "input": 1.0, "output": 1.0, "extra": 1 } }
    }))
    .is_err());

    // Cost in aggregation: only priced samples; unpriced tokens reported.
    let ledger_root = populated_root("prices-agg");
    let ledger = store::load_usage_ledger_at(ledger_root.path()).unwrap();
    let query = UsageQuery {
        from: d("2026-09-01"),
        to: d("2026-10-31"),
        group_by: vec![UsageGroupKey::Profile],
    };
    let mut manual_settings = crate::config::usage_pricing::UsageSettings::default().effective();
    manual_settings.price_source = crate::config::usage_pricing::PriceSource::Manual;
    let manual_context = super::estimate::PriceContext::new(
        &manual_settings,
        Some(table.clone()),
        None,
        crate::config::usage_pricing::ExchangeRates::default(),
    );
    let result = aggregate_usage(
        &ledger,
        &query,
        &UsageLabels::default(),
        Some(&manual_context),
        &aedt,
    );
    assert_eq!(result.currency.as_deref(), Some("USD"));
    // work: input 3500 (cached 1400) output 350 -> 2100*2 + 1400*0.5 + 350*10 = 8400 per 1M
    let work = result
        .rows
        .iter()
        .find(|row| row.key.profile == Some(Some("work".to_string())))
        .unwrap();
    assert!((work.amounts.estimated_cost.unwrap() - 0.0084).abs() < 1e-9);
    assert_eq!(work.amounts.unpriced_tokens, Some(0));
    assert_eq!(result.totals.unpriced_tokens, Some(220));
    assert_eq!(work.amounts.price_sources, Some(vec!["manual"]));
    assert_eq!(result.totals.price_sources, Some(vec!["manual", "unpriced"]));
    let unpriced = result.unpriced_models.as_ref().unwrap();
    assert!(!unpriced.is_empty() && unpriced.iter().all(|m| m.model.as_deref() != Some("gpt-5.5")));
    assert_eq!(unpriced.iter().map(|m| m.tokens).sum::<u64>(), 220);
}

#[test]
fn quota_snapshots_are_throttled_per_gauge_and_filtered() {
    let root = TestRoot::new("quota");
    let snapshot = |ts: i64, profile: &str, gauge: &str, pct: f64| QuotaSnapshot {
        ts,
        profile_id: format!("id-{profile}"),
        profile: profile.to_string(),
        gauge_id: gauge.to_string(),
        label: Some("5h".to_string()),
        kind: Some("window".to_string()),
        remaining_percent: Some(pct),
        balance: None,
        reset_at: None,
    };
    let base = DateTime::parse_from_rfc3339("2026-10-05T00:00:00Z")
        .unwrap()
        .timestamp();
    assert_eq!(
        record_snapshots_at(
            root.path(),
            &[
                snapshot(base, "glm", "primary", 90.0),
                snapshot(base, "glm", "extra-0", 50.0)
            ]
        )
        .unwrap(),
        2
    );
    // Same observation and one 5 minutes later: throttled.
    assert_eq!(
        record_snapshots_at(root.path(), &[snapshot(base, "glm", "primary", 90.0)]).unwrap(),
        0
    );
    assert_eq!(
        record_snapshots_at(root.path(), &[snapshot(base + 300, "glm", "primary", 88.0)]).unwrap(),
        0
    );
    // 15 minutes later: recorded; other profile independent.
    assert_eq!(
        record_snapshots_at(
            root.path(),
            &[
                snapshot(base + 900, "glm", "primary", 85.0),
                snapshot(base + 300, "kimi", "primary", 10.0)
            ]
        )
        .unwrap(),
        2
    );
    let all =
        read_quota_history_at(root.path(), d("2026-10-05"), d("2026-10-05"), None, &aedt).unwrap();
    assert_eq!(all.len(), 4);
    assert!(all.windows(2).all(|pair| pair[0].ts <= pair[1].ts));
    let glm = read_quota_history_at(
        root.path(),
        d("2026-10-05"),
        d("2026-10-05"),
        Some("glm"),
        &aedt,
    )
    .unwrap();
    assert_eq!(glm.len(), 3);
    let by_id = read_quota_history_at(
        root.path(),
        d("2026-10-05"),
        d("2026-10-05"),
        Some("id-kimi"),
        &aedt,
    )
    .unwrap();
    assert_eq!(by_id.len(), 1);
    let none =
        read_quota_history_at(root.path(), d("2026-10-06"), d("2026-10-07"), None, &aedt).unwrap();
    assert!(none.is_empty());
}

#[test]
fn profile_rows_aggregate_by_id_across_a_rename_and_flag_deleted_profiles() {
    use crate::profiles::identity::{NameHistory, ProfileNameDirectory, RenameEntry};
    let root = TestRoot::new("rename");
    let p1 = "11111111-1111-4111-8111-111111111111";
    let p2 = "22222222-2222-4222-8222-222222222222";
    // r217 row (name only), then a live runtime on the same thread before
    // and after the rename (id + the name it had at that moment).
    record_usage_event_at(
        root.path(),
        &token_event("cutex.a", "a1", 1, "2026-10-04T01:00:00Z", "t1", usage(100, 0, 10, 0), usage(100, 0, 10, 0)),
        Some("old"),
    )
    .unwrap();
    record_usage_event_for_profile_at(
        root.path(),
        &token_event("cutex.a", "a2", 2, "2026-10-05T01:00:00Z", "t2", usage(300, 0, 30, 0), usage(200, 0, 20, 0)),
        Some(p1),
        Some("old"),
    )
    .unwrap();
    record_usage_event_for_profile_at(
        root.path(),
        &token_event("cutex.a", "a3", 3, "2026-10-06T01:00:00Z", "t3", usage(600, 0, 60, 0), usage(300, 0, 30, 0)),
        Some(p1),
        Some("new"),
    )
    .unwrap();
    // A deleted profile and a profile that still exists, both name only.
    record_usage_event_at(
        root.path(),
        &token_event("cutex.b", "b1", 1, "2026-10-04T02:00:00Z", "t9", usage(50, 0, 5, 0), usage(50, 0, 5, 0)),
        Some("GLM"),
    )
    .unwrap();
    record_usage_event_at(
        root.path(),
        &token_event("cutex.c", "c1", 1, "2026-10-04T03:00:00Z", "t8", usage(7, 0, 1, 0), usage(7, 0, 1, 0)),
        Some("keep"),
    )
    .unwrap();
    record_usage_event_at(
        root.path(),
        &token_event("cutex.d", "d1", 1, "2026-10-04T04:00:00Z", "t7", usage(9, 0, 1, 0), usage(9, 0, 1, 0)),
        Some("hazel"),
    )
    .unwrap();

    let mut ledger = store::load_usage_ledger_at(root.path()).unwrap();
    // New rows store the id next to the name at the time (r217 ignores it).
    let stored = fs::read_to_string(root.path().join("usage-ledger").join("2026-10.jsonl")).unwrap();
    assert!(stored.contains(&format!("\"profileId\":\"{p1}\"")), "{stored}");
    let accounts: crate::profiles::model::AccountsStore = serde_json::from_value(json!({
        "version": crate::profiles::model::STORE_VERSION,
        "active_account_id": null,
        "accounts": [
            {"id": p1, "name": "new", "email": null, "plan_type": null, "last_used_at": null},
            {"id": p2, "name": "keep", "email": null, "plan_type": null, "last_used_at": null},
        ],
    }))
    .unwrap();
    let history = NameHistory {
        version: 1,
        renames: vec![RenameEntry {
            profile_id: p1.into(),
            from: "old".into(),
            to: "new".into(),
            renamed_at: "2026-10-05T12:00:00Z".into(),
        }],
    };
    let directory = ProfileNameDirectory::from_parts(&accounts, &history, &[("p9".into(), "GLM".into())]);
    canonicalize_ledger_profiles(&mut ledger, &directory);
    let query = UsageQuery { from: d("2026-10-01"), to: d("2026-10-31"), group_by: vec![UsageGroupKey::Profile] };
    let value = serde_json::to_value(aggregate_usage(&ledger, &query, &UsageLabels::default(), None, &aedt)).unwrap();
    let rows = value["rows"].as_array().unwrap();
    let row = |name: &str| rows.iter().find(|row| row["key"]["profile"] == name).unwrap_or_else(|| panic!("{name}: {rows:#?}"));
    assert_eq!(rows.len(), 4, "{rows:#?}");
    let renamed = row("new");
    assert_eq!(renamed["key"]["profileId"], p1);
    assert_eq!(renamed["key"]["profileState"], "current");
    assert_eq!(renamed["profileState"], "current");
    assert_eq!(renamed["requests"], 3);
    assert_eq!(renamed["input"], 600);
    assert_eq!(row("keep")["key"]["profileId"], p2);
    let deleted = row("GLM");
    assert_eq!(deleted["key"]["profileState"], "deleted");
    assert_eq!(deleted["profileState"], "deleted");
    assert_eq!(deleted["key"]["profileId"], "p9");
    let unknown = row("hazel");
    assert_eq!(unknown["profileState"], "deleted");
    assert_eq!(unknown["key"]["profileId"], Value::Null);
    assert!(rows.iter().all(|row| row["key"]["profile"] != "old"));
}

#[test]
fn reset_windows_follow_a_rename_by_id() {
    let root = TestRoot::new("rename-reset");
    let p1 = "11111111-1111-4111-8111-111111111111";
    let limits = |resets_at: i64| {
        json!({"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 1.0, "windowDurationMins": 300, "resetsAt": resets_at}}})
    };
    let event = |id: &str, seq: u64, resets_at: i64| {
        envelope("cutex.a", id, seq, "2026-10-05T01:00:00Z", "account/rateLimits/updated", limits(resets_at))
    };
    record_usage_event_for_profile_at(root.path(), &event("r1", 1, 1_800_000_000), Some(p1), Some("old")).unwrap();
    record_usage_event_for_profile_at(root.path(), &event("r2", 2, 1_800_000_000), Some(p1), Some("new")).unwrap();
    let ledger = store::load_usage_ledger_at(root.path()).unwrap();
    let boundaries: Vec<_> = ledger
        .entries
        .iter()
        .filter_map(|entry| match entry {
            UsageLedgerEntry::ResetBoundary(boundary) => Some(boundary),
            _ => None,
        })
        .collect();
    // Same window under the new name: no second boundary.
    assert_eq!(boundaries.len(), 1, "{boundaries:#?}");
    assert_eq!(boundaries[0].profile_id.as_deref(), Some(p1));
}

#[test]
fn model_rows_and_unpriced_models_ignore_the_model_ids_case() {
    // The Claude runtime reports `GLM-5.3`, Codex `glm-5.3`: one model, one row.
    let root = TestRoot::new("model-case");
    let sessions = [
        ("cutex.a", "GLM-5.3"),
        ("cutex.b", "glm-5.3"),
        ("cutex.c", "GLM-5.3-Flash"),
        ("cutex.d", "glm-5.3-flash"),
    ];
    for (session, model) in sessions {
        record_usage_event_at(root.path(), &settings_event(session, 1, "2026-10-04T01:00:00Z", model), Some("glm")).unwrap();
        record_usage_event_at(
            root.path(),
            &token_event(session, &format!("x-{session}"), 2, "2026-10-04T02:00:00Z", "t1", usage(100, 0, 10, 0), usage(100, 0, 10, 0)),
            Some("glm"),
        )
        .unwrap();
    }
    let stored = fs::read_to_string(root.path().join("usage-ledger").join("2026-10.jsonl")).unwrap();
    assert!(stored.contains("\"GLM-5.3\"") && stored.contains("\"glm-5.3\""), "samples keep their spelling: {stored}");

    let ledger = store::load_usage_ledger_at(root.path()).unwrap();
    let query = UsageQuery { from: d("2026-10-01"), to: d("2026-10-31"), group_by: vec![UsageGroupKey::Model] };
    let mut settings = crate::config::usage_pricing::UsageSettings::default().effective();
    settings.price_source = crate::config::usage_pricing::PriceSource::Manual;
    let context = super::estimate::PriceContext::new(
        &settings,
        Some(UsagePriceTable::default()),
        None,
        crate::config::usage_pricing::ExchangeRates::default(),
    );
    let result = aggregate_usage(&ledger, &query, &UsageLabels::default(), Some(&context), &aedt);
    let rows: Vec<_> = result
        .rows
        .iter()
        .map(|row| (row.key.model.clone().flatten(), row.amounts.requests, row.amounts.input))
        .collect();
    assert_eq!(
        rows,
        vec![(Some("glm-5.3".to_string()), 2, 200), (Some("glm-5.3-flash".to_string()), 2, 200)]
    );
    let unpriced: Vec<_> = result
        .unpriced_models
        .unwrap()
        .into_iter()
        .map(|m| (m.model, m.requests, m.tokens))
        .collect();
    assert_eq!(
        unpriced,
        vec![(Some("glm-5.3".to_string()), 2, 220), (Some("glm-5.3-flash".to_string()), 2, 220)]
    );
}
