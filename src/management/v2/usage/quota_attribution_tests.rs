use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use super::*;
use crate::management::v2::usage::model::UsageAttribution;
use crate::management::v2::usage::model::UsageSample;
use crate::management::v2::usage::model::UsageTokenSource;

const PROFILE: &str = "00000000-0000-4000-8000-0000000000c0";
/// 2026-10-30T12:00:00Z .. 17:00:00Z, the replayed 5h window.
const START: i64 = 1_793_361_600;
const RESET: i64 = 1_793_379_600;

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("cutex-quota-attr-{name}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("usage-ledger")).unwrap();
    root
}

fn tokens(input: u64, cache_read: u64, cache_write: u64, output: u64) -> TokenUsageBreakdown {
    TokenUsageBreakdown {
        total_tokens: input + cache_read + cache_write + output,
        input_tokens: input + cache_read + cache_write,
        cached_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        output_tokens: output,
        reasoning_output_tokens: 0,
    }
}

fn sample(
    id: &str,
    at: i64,
    session: &str,
    model: &str,
    tokens: TokenUsageBreakdown,
) -> UsageLedgerEntry {
    UsageLedgerEntry::Usage(Box::new(UsageSample {
        entry_id: format!("usage:{id}"),
        event_id: id.to_string(),
        observed_at: DateTime::<Utc>::from_timestamp(at, 0).unwrap().to_rfc3339(),
        host_id: "HOST".to_string(),
        cutex_session_id: session.to_string(),
        thread_id: format!("thread-{session}"),
        turn_id: format!("turn-{id}"),
        runtime_generation: 1,
        profile: Some("Account-C".to_string()),
        profile_id: Some(PROFILE.to_string()),
        profile_deleted: false,
        source: None,
        attribution: UsageAttribution {
            model: Some(model.to_string()),
            ..UsageAttribution::default()
        },
        token_source: UsageTokenSource::CumulativeDelta,
        model_context_window: None,
        compaction_count: None,
        model_requests: None,
        background_tokens: None,
        cumulative_total: tokens.clone(),
        tokens,
    }))
}

fn append(root: &Path, month: &str, entries: &[UsageLedgerEntry]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("usage-ledger").join(format!("{month}.jsonl")))
        .unwrap();
    for entry in entries {
        file.write_all(b"\n").unwrap();
        file.write_all(&serde_json::to_vec(entry).unwrap()).unwrap();
    }
}

fn profiles() -> BTreeSet<String> {
    [PROFILE.to_string()].into_iter().collect()
}

#[test]
fn sqlite_refresh_is_request_scoped_and_excludes_external_or_future_usage() {
    let first = temp_root("scoped-a");
    let second = temp_root("scoped-b");
    let now = START + 300;
    append(
        &first,
        "2026-10",
        &[sample("a", START, "cutex.a", "opus", tokens(11, 0, 0, 0))],
    );
    append(
        &first,
        "2026-10",
        &[sample(
            "future",
            now + 60,
            "cutex.future",
            "opus",
            tokens(999, 0, 0, 0),
        )],
    );
    let UsageLedgerEntry::Usage(mut external) = sample(
        "external",
        START,
        "cutex.external",
        "opus",
        tokens(999, 0, 0, 0),
    ) else {
        unreachable!()
    };
    external.source = Some(crate::management::v2::usage::model::UsageSampleSource {
        id: "cc".into(),
        kind: "claude-code".into(),
        label: None,
        billing: None,
        agent_name: None,
        project: None,
    });
    append(&first, "2026-10", &[UsageLedgerEntry::Usage(external)]);
    append(
        &second,
        "2026-10",
        &[sample("b", START, "cutex.b", "opus", tokens(7, 0, 0, 0))],
    );
    let mut index = Index::default();
    refresh_at(&mut index, &first, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert_eq!(index.units_between(&profiles(), START, now), 11.0);
    refresh_at(&mut index, &second, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert_eq!(index.units_between(&profiles(), START, now), 7.0);
    refresh_at(&mut index, &first, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert_eq!(index.units_between(&profiles(), START, now), 11.0);
    for root in [first, second] {
        assert!(root.join("usage-index.sqlite3").exists());
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn sqlite_weekly_calibration_keeps_four_whole_completed_windows() {
    let root = temp_root("weekly-lookback");
    let day = 86400;
    let now = START + 6 * day;
    let mut history = Vec::new();
    for n in 0..4 {
        let reset = START - n * 7 * day;
        append(
            &root,
            "2026-10",
            &[sample(
                &format!("week-{n}"),
                reset - 6 * day,
                "cutex.a",
                "opus",
                tokens((n as u64 + 1) * 1000, 0, 0, 0),
            )],
        );
        history.push(HistoryPoint {
            ts: reset - 60,
            used: 10.0,
            reset_at: Some(reset),
        });
    }
    let mut index = Index::default();
    refresh_at(&mut index, &root, now, WindowKind::SevenDay, &profiles()).unwrap();
    let calibration = calibrate(&index, &profiles(), WindowKind::SevenDay, &history, now);
    assert_eq!(calibration.windows, 4);
    assert_eq!(calibration.units_per_percent, Some(250.0));
    fs::remove_dir_all(root).unwrap();
}

fn names(id: &str) -> Option<String> {
    id.strip_prefix("cutex.").map(str::to_string)
}

fn profile_name(_: &str) -> Option<String> {
    Some("Account-C".to_string())
}

fn inputs<'a>(
    kind: WindowKind,
    now: i64,
    profiles: &'a BTreeSet<String>,
    window: Option<WindowNow>,
    history: &'a [HistoryPoint],
    in_flight: &'a [(String, i64)],
) -> Inputs<'a> {
    Inputs {
        kind,
        now,
        profiles,
        window,
        history,
        in_flight,
        agent_name: &names,
        profile_name: &profile_name,
    }
}

#[test]
fn units_follow_api_price_ratios_and_model_factors() {
    let row = tokens(1_000, 10_000, 100, 10);
    // 1000 + 0.1 × 10000 + 1.25 × 100 + 5 × 10
    assert_eq!(units(&row, Some("claude-opus-5-5")), 2_175.0);
    assert_eq!(units(&row, Some("opus")), 2_175.0);
    assert!((units(&row, Some("claude-sonnet-5-5")) - 2_175.0 * 0.6).abs() < 1e-9);
    assert!((units(&row, Some("claude-haiku-5-5")) - 2_175.0 * 0.2).abs() < 1e-9);
    assert!((units(&row, None) - 2_175.0 * 0.6).abs() < 1e-9);
    assert_eq!(WindowKind::parse("5h"), Some(WindowKind::FiveHour));
    assert_eq!(WindowKind::parse("7d"), Some(WindowKind::SevenDay));
    assert_eq!(WindowKind::parse("week"), None);
}

#[test]
fn ingest_buckets_once_per_entry_and_skips_external_and_reset_rows() {
    let mut index = Index::default();
    let row = sample(
        "a",
        START + 60,
        "cutex.agent-a",
        "opus",
        tokens(100, 0, 0, 0),
    );
    index.ingest(row.clone());
    index.ingest(row);
    let UsageLedgerEntry::Usage(mut external) = sample(
        "b",
        START + 60,
        "cutex.agent-a",
        "opus",
        tokens(100, 0, 0, 0),
    ) else {
        unreachable!()
    };
    external.source = Some(crate::management::v2::usage::model::UsageSampleSource {
        id: "cc".into(),
        kind: "claude-code".into(),
        label: None,
        billing: None,
        agent_name: None,
        project: None,
    });
    index.ingest(UsageLedgerEntry::Usage(external));
    assert_eq!(index.units_between(&profiles(), START, START + 900), 100.0);
    assert_eq!(index.units_between(&profiles(), START + 900, RESET), 0.0);
    // Pre-r218 rows without a profile id are keyed by name.
    assert_eq!(profile_key(None, Some("Old")), Some("name:Old".to_string()));
    assert_eq!(profile_key(Some(""), None), None);
}

#[test]
fn refresh_uses_sqlite_snapshot_outside_usage_lock_and_rebuilds_after_replacement() {
    let root = temp_root("cursor");
    let now = START + 3600;
    append(
        &root,
        "2026-10",
        &[sample(
            "1",
            START + 60,
            "cutex.agent-a",
            "opus",
            tokens(100, 0, 0, 0),
        )],
    );
    let before = store::locked_parse_bytes();
    let mut index = Index::default();
    refresh_at(&mut index, &root, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert_eq!(index.units_between(&profiles(), START, now), 100.0);

    append(
        &root,
        "2026-10",
        &[sample(
            "2",
            START + 120,
            "cutex.agent-b",
            "opus",
            tokens(50, 0, 0, 0),
        )],
    );
    // A torn tail (crash mid-write) is skipped; the next row starts after it.
    {
        use std::io::Write;
        let path = root.join("usage-ledger").join("2026-10.jsonl");
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"\n{\"kind\":\"usage\",\"entr")
            .unwrap();
    }
    append(
        &root,
        "2026-10",
        &[sample(
            "3",
            START + 180,
            "cutex.agent-b",
            "opus",
            tokens(25, 0, 0, 0),
        )],
    );
    let warnings = refresh_at(&mut index, &root, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(index.units_between(&profiles(), START, now), 175.0);
    // Repeated reads preserve totals without accumulating derived state.
    refresh_at(&mut index, &root, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert_eq!(index.units_between(&profiles(), START, now), 175.0);
    assert_eq!(
        store::locked_parse_bytes(),
        before,
        "no ledger bytes are decoded under the usage lock"
    );

    // A replaced (shorter) file starts over instead of reading from a stale offset.
    fs::write(root.join("usage-ledger").join("2026-10.jsonl"), b"").unwrap();
    append(
        &root,
        "2026-10",
        &[sample(
            "4",
            START + 60,
            "cutex.agent-c",
            "opus",
            tokens(7, 0, 0, 0),
        )],
    );
    refresh_at(&mut index, &root, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert_eq!(index.units_between(&profiles(), START, now), 7.0);

    // Months older than the lookback are neither read nor kept.
    append(
        &root,
        "2026-08",
        &[sample(
            "old",
            START - 60 * 86400,
            "cutex.agent-a",
            "opus",
            tokens(9, 0, 0, 0),
        )],
    );
    refresh_at(&mut index, &root, now, WindowKind::SevenDay, &profiles()).unwrap();
    assert!(
        root.join("usage-index.sqlite3").exists(),
        "existing SQLite index is reused"
    );
    assert_eq!(index.units_between(&profiles(), START, now), 7.0);
    fs::remove_dir_all(root).unwrap();
}

fn window_points(reset: i64, used: &[(i64, f64)]) -> Vec<HistoryPoint> {
    used.iter()
        .map(|(offset, used)| HistoryPoint {
            ts: reset - 5 * 3600 + offset,
            used: *used,
            reset_at: Some(reset),
        })
        .collect()
}

#[test]
fn calibration_takes_the_median_of_completed_unclipped_windows() {
    let mut index = Index::default();
    let mut history = Vec::new();
    // Six past windows, 200 units per percent except one outlier at 400.
    for (n, ratio) in [200.0, 200.0, 400.0, 200.0, 200.0, 200.0]
        .into_iter()
        .enumerate()
    {
        let reset = START - (n as i64 + 1) * 5 * 3600;
        let peak = 40.0;
        index.ingest(sample(
            &format!("w{n}"),
            reset - 3 * 3600,
            "cutex.agent-a",
            "opus",
            tokens((ratio * peak) as u64, 0, 0, 0),
        ));
        history.extend(window_points(
            reset,
            &[(600, 5.0), (3 * 3600, peak), (5 * 3600 - 300, peak)],
        ));
    }
    // Not counted: hit the limit, read too early, and idle.
    let clipped = START - 7 * 5 * 3600;
    index.ingest(sample(
        "clipped",
        clipped - 3600,
        "cutex.agent-a",
        "opus",
        tokens(1_000, 0, 0, 0),
    ));
    history.extend(window_points(
        clipped,
        &[(600, 30.0), (4 * 3600, 100.0), (5 * 3600 - 60, 100.0)],
    ));
    let unread_end = START - 8 * 5 * 3600;
    index.ingest(sample(
        "unread",
        unread_end - 4 * 3600,
        "cutex.agent-a",
        "opus",
        tokens(1_000, 0, 0, 0),
    ));
    history.extend(window_points(unread_end, &[(600, 1.0), (3600, 1.0)]));
    history.sort_by_key(|point| point.ts);

    let calibration = calibrate(
        &index,
        &profiles(),
        WindowKind::FiveHour,
        &history,
        START + 60,
    );
    assert_eq!(calibration.windows, 6);
    assert_eq!(calibration.units_per_percent, Some(200.0));
    assert_eq!(calibration.confidence, "high");

    let one: Vec<HistoryPoint> = history
        .iter()
        .copied()
        .filter(|point| point.reset_at == Some(START - 5 * 3600))
        .collect();
    let few = calibrate(&index, &profiles(), WindowKind::FiveHour, &one, START + 60);
    assert_eq!((few.windows, few.confidence), (1, "low"));
    let none = calibrate(
        &Index::default(),
        &profiles(),
        WindowKind::FiveHour,
        &history,
        START + 60,
    );
    assert_eq!((none.units_per_percent, none.windows), (None, 0));
}

#[test]
fn compute_splits_the_window_and_reports_unattributed_and_running_agents() {
    let mut index = Index::default();
    let mut history = Vec::new();
    for n in 0..5 {
        let reset = START - (n + 1) * 5 * 3600;
        index.ingest(sample(
            &format!("cal{n}"),
            reset - 3600,
            "cutex.agent-x",
            "opus",
            tokens(10_000, 0, 0, 0),
        ));
        history.extend(window_points(reset, &[(600, 1.0), (5 * 3600 - 120, 50.0)]));
    }
    // 200 units per percent. Current window: a = 4000 (20 %), b = 2000 (10 %).
    index.ingest(sample(
        "a",
        START + 600,
        "cutex.agent-a",
        "opus",
        tokens(4_000, 0, 0, 0),
    ));
    index.ingest(sample(
        "b",
        START + 1200,
        "cutex.agent-b",
        "claude-opus-5-5",
        tokens(2_000, 0, 0, 0),
    ));
    // Before the window and on another profile: not counted.
    index.ingest(sample(
        "before",
        START - 60,
        "cutex.agent-a",
        "opus",
        tokens(9_999, 0, 0, 0),
    ));
    let UsageLedgerEntry::Usage(mut other) = sample(
        "other",
        START + 60,
        "cutex.agent-z",
        "opus",
        tokens(9_999, 0, 0, 0),
    ) else {
        unreachable!()
    };
    other.profile_id = Some("someone-else".into());
    index.ingest(UsageLedgerEntry::Usage(other));
    history.extend(window_points(RESET, &[(600, 10.0), (3600, 25.0)]));
    history.sort_by_key(|point| point.ts);

    let now = START + 2 * 3600;
    let window = WindowNow {
        used: 45.0,
        reset_at: Some(RESET),
        observed_at: now,
    };
    let running = vec![("cutex.agent-c".to_string(), now - 600)];
    let profiles = profiles();
    let result = compute(
        &index,
        &inputs(
            WindowKind::FiveHour,
            now,
            &profiles,
            Some(window),
            &history,
            &running,
        ),
    );
    assert_eq!(result.window.start_at, START);
    assert_eq!(result.calibration.units_per_percent, Some(200.0));
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].cutex_session_id, "cutex.agent-a");
    assert_eq!(result.rows[0].agent_name.as_deref(), Some("agent-a"));
    assert_eq!(result.rows[0].percent, Some(20.0));
    assert_eq!(result.rows[0].share, 66.7);
    assert_eq!(result.rows[1].percent, Some(10.0));
    assert_eq!(result.attributed_percent, Some(30.0));
    assert_eq!(result.unattributed_percent, Some(15.0));
    assert_eq!(result.in_flight.len(), 1);
    assert_eq!(result.in_flight[0].agent_name.as_deref(), Some("agent-c"));
    // 25 % at +1 h, 45 % at +2 h: 20 %/h, out at +4 h 45 min, before the 5 h reset.
    assert_eq!(result.forecast.state, "runsOut");
    assert_eq!(result.forecast.percent_per_hour, Some(20.0));
    assert_eq!(
        result.forecast.runs_out_at,
        Some(now + (55.0 / 20.0 * 3600.0) as i64)
    );

    // Without a reading: the last window length, shares only.
    let bare = compute(
        &index,
        &inputs(WindowKind::FiveHour, now, &profiles, None, &[], &[]),
    );
    assert_eq!(bare.window.start_at, now - 5 * 3600);
    assert_eq!(bare.unattributed_percent, None);
    assert!(bare.rows.iter().all(|row| row.percent.is_none()));
    assert_eq!(bare.forecast.state, "unknown");
}

#[test]
fn background_subagent_units_stay_with_their_parent_agent() {
    let mut index = Index::default();
    let UsageLedgerEntry::Usage(mut row) = sample(
        "bg",
        START + 60,
        "cutex.agent-a",
        "opus",
        tokens(0, 10_000, 0, 1_000),
    ) else {
        unreachable!()
    };
    // 9 000 cache reads and 900 output tokens of the row were the background task's.
    row.background_tokens = Some(tokens(0, 9_000, 0, 900));
    index.ingest(UsageLedgerEntry::Usage(row));
    index.ingest(sample(
        "main",
        START + 120,
        "cutex.agent-a",
        "opus",
        tokens(0, 0, 0, 100),
    ));
    let profiles = profiles();
    let result = compute(
        &index,
        &inputs(
            WindowKind::FiveHour,
            START + 3600,
            &profiles,
            None,
            &[],
            &[],
        ),
    );
    let [row] = result.rows.as_slice() else {
        panic!("{:?}", result.rows)
    };
    // units: 0.1 × 10 000 + 5 × 1 100 = 6 500; background: 0.1 × 9 000 + 5 × 900 = 5 400.
    assert_eq!(
        (row.units, row.background_units, row.background_share),
        (6_500.0, 5_400.0, 83.1)
    );
}

#[test]
fn forecast_states() {
    let window = |used| WindowNow {
        used,
        reset_at: Some(RESET),
        observed_at: START + 3600,
    };
    let history = window_points(RESET, &[(0, 0.0), (1800, 5.0)]);
    assert_eq!(
        forecast(&window(100.0), &history, START + 3600).state,
        "exhausted"
    );
    // 10 %/h from 10 %: 9 h to go, the window resets first.
    let slow = forecast(&window(10.0), &history, START + 3600);
    assert_eq!((slow.state, slow.runs_out_at), ("lasts", None));
    // A fresh snapshot can already have a projected exhaustion time in the past.
    let steep = window_points(RESET, &[(1800, 40.0), (3000, 80.0)]);
    let past = forecast(&window(99.0), &steep, START + 3600 + 120);
    assert_eq!(
        (past.state, past.runs_out_at, past.percent_per_hour),
        ("unknown", None, None)
    );
    // Too few points for a slope.
    assert_eq!(forecast(&window(10.0), &[], START + 3600).state, "unknown");
}

#[test]
fn history_points_keep_one_reading_per_time_across_profiles_of_an_account() {
    let snapshot = |profile_id: &str, ts: i64, gauge: &str, remaining: f64| QuotaSnapshot {
        ts,
        profile_id: profile_id.into(),
        profile: "x".into(),
        gauge_id: gauge.into(),
        label: None,
        kind: Some("window".into()),
        remaining_percent: Some(remaining),
        balance: None,
        reset_at: Some(RESET),
    };
    let snapshots = vec![
        snapshot("p1", 10, "claude-0", 90.0),
        snapshot("p2", 10, "claude-0", 90.0),
        snapshot("p1", 20, "claude-1", 50.0),
        snapshot("p3", 30, "claude-0", 10.0),
        snapshot("p2", 40, "claude-0", 80.0),
    ];
    let ids: BTreeSet<String> = ["p1".to_string(), "p2".to_string()].into_iter().collect();
    let points = history_points(&snapshots, &ids, "claude-0");
    assert_eq!(
        points
            .iter()
            .map(|point| (point.ts, point.used))
            .collect::<Vec<_>>(),
        vec![(10, 10.0), (40, 20.0)]
    );
}

/// Replay of the host-a window that ran out on 2026-10-30 (sanitized: ids,
/// names, host and profile replaced; tokens, models and times kept). The
/// account hit 100 % at 14:49 UTC; three Opus agents used it.
#[test]
fn replays_the_host_quota_window_of_2026_10_30() {
    let root = temp_root("replay");
    fs::write(
        root.join("usage-ledger").join("2026-10.jsonl"),
        include_str!("fixtures/quota-attribution-ledger.jsonl"),
    )
    .unwrap();
    let snapshots: Vec<QuotaSnapshot> = include_str!("fixtures/quota-attribution-history.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let history = history_points(&snapshots, &profiles(), "claude-0");
    let exhausted_at = 1_793_371_757;
    let mut index = Index::default();
    let warnings = refresh_at(
        &mut index,
        &root,
        exhausted_at,
        WindowKind::SevenDay,
        &profiles(),
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");

    let window = WindowNow {
        used: 100.0,
        reset_at: Some(RESET),
        observed_at: exhausted_at,
    };
    let profiles = profiles();
    let result = compute(
        &index,
        &inputs(
            WindowKind::FiveHour,
            exhausted_at,
            &profiles,
            Some(window),
            &history,
            &[],
        ),
    );
    assert_eq!(result.window.start_at, START);
    let shares: Vec<(&str, f64)> = result
        .rows
        .iter()
        .map(|row| (row.cutex_session_id.as_str(), row.share))
        .collect();
    assert_eq!(
        shares,
        vec![
            ("cutex.agent-a", 54.5),
            ("cutex.agent-b", 38.8),
            ("cutex.agent-c", 6.7)
        ]
    );
    assert_eq!(
        result.rows.iter().map(|row| row.requests).sum::<u64>(),
        474,
        "SDK steps of the 22 turns"
    );
    assert!(
        (result.total_units / 1e6 - 24.1).abs() < 0.05,
        "{}",
        result.total_units
    );
    // The only window in the fixture is the one that hit the limit: no calibration.
    assert_eq!(result.calibration.units_per_percent, None);
    assert_eq!(result.unattributed_percent, None);
    assert_eq!(result.forecast.state, "exhausted");

    // With a coefficient from other windows (≈ 0.26 M units per 1 %), the
    // agents add up to about the observed 100 %.
    let rescaled = Calibration {
        units_per_percent: Some(260_000.0),
        windows: 5,
        confidence: "high",
    };
    let percent: f64 = result.total_units / rescaled.units_per_percent.unwrap();
    assert!((percent - 92.7).abs() < 1.0, "{percent}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn response_carries_contract_fields() {
    let profiles = profiles();
    let result = compute(
        &Index::default(),
        &inputs(WindowKind::SevenDay, START, &profiles, None, &[], &[]),
    );
    let body = response(
        "ca-test",
        Some("Account-C".into()),
        result,
        vec!["w".into()],
    );
    assert_eq!(body["contractVersion"], 1);
    assert_eq!(body["account"]["id"], "ca-test");
    assert_eq!(body["window"]["label"], "7d");
    assert_eq!(body["window"]["startAt"], START - 7 * 24 * 3600);
    assert_eq!(body["tolerancePercent"], TOLERANCE_PERCENT);
    assert_eq!(body["weights"]["cacheRead"], 0.1);
    assert_eq!(body["coverage"]["accounting"], "turnEnd");
    assert_eq!(body["coverage"]["backgroundSubagents"], "inParentWithShare");
    assert_eq!(body["coverage"]["warnings"][0], "w");
    assert!(body.get("ledgerThrough").is_none());
    assert!(body["rows"].as_array().unwrap().is_empty());
    assert!(body["inFlight"].as_array().unwrap().is_empty());
}

#[test]
fn running_agents_and_names_use_paged_and_point_sqlite_reads() {
    use crate::management::v2::activity::SessionActivityState;
    use crate::session::model::{CutexSessionRecord, CutexSessionStore};
    let root = temp_root("session-selectors");
    let path = root.join("cutex-sessions.json");
    let now = START + 300;
    let ids: Vec<_> = (0..4)
        .map(|_| format!("cutex.{}", uuid::Uuid::new_v4()))
        .collect();
    let mut store = CutexSessionStore::default();
    for (n, id) in ids.iter().enumerate() {
        let mut record = CutexSessionRecord::new(
            id.clone(),
            None,
            "HOST".into(),
            root.to_string_lossy().into(),
            Some(if n == 2 { "Other" } else { PROFILE }.into()),
        )
        .unwrap();
        record.formal_agent_name = Some(format!("agent-{n}"));
        store.sessions.insert(id.clone(), record);
    }
    crate::session::store::save_cutex_session_store_to_path(&path, &store).unwrap();
    for (n, id) in ids.iter().enumerate() {
        crate::management::v2::activity::write_activity_test_row(
            &root,
            id,
            SessionActivityState {
                last_turn_started_at: Some(
                    DateTime::<Utc>::from_timestamp(if n == 3 { now + 60 } else { now - 60 }, 0)
                        .unwrap()
                        .to_rfc3339(),
                ),
                last_turn_completed_at: (n == 1).then(|| {
                    DateTime::<Utc>::from_timestamp(now, 0)
                        .unwrap()
                        .to_rfc3339()
                }),
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        in_flight_at(&path, &root, &profiles(), now).unwrap(),
        vec![(ids[0].clone(), now - 60)]
    );
    let missing = format!("cutex.{}", uuid::Uuid::new_v4());
    let selected: BTreeSet<_> = [ids[0].clone(), missing.clone()].into_iter().collect();
    let labels = session_labels_at(&path, &selected).unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[&ids[0]], Some("agent-0".to_string()));
    assert_eq!(labels[&missing], None);
    crate::session::store::close_session_store(&path);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshot_freshness_expiry_and_refresh_failures() {
    use crate::management::v2::profile_quota_claude::{ClaudeQuotaSnapshot, StoredWindow};
    let now = START + 3600;
    let mut snapshot = ClaudeQuotaSnapshot {
        fetched_at: Some(now),
        windows: vec![StoredWindow {
            label: "5h".into(),
            used_percent: 60.0,
            reset_at: Some(RESET),
        }],
        ..Default::default()
    };
    let read = |snapshot: &ClaudeQuotaSnapshot, now| {
        let mut warnings = Vec::new();
        let (gauge, window) =
            snapshot_window(Some(snapshot), WindowKind::FiveHour, now, &mut warnings);
        assert_eq!(gauge, "claude-0");
        (window, warnings)
    };
    assert!(read(&snapshot, now).1.is_empty());
    assert!(read(&snapshot, now + FRESH_SECS).1.is_empty());
    let (old, warnings) = read(&snapshot, now + FRESH_SECS + 1);
    assert_eq!(old.unwrap().observed_at, now);
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        forecast(&old.unwrap(), &[], now + FRESH_SECS + 1).state,
        "unknown"
    );
    let (expired, warnings) = read(&snapshot, RESET);
    assert!(expired.is_none());
    assert_eq!(warnings.len(), 1);
    snapshot.last_error = Some("private provider detail".into());
    let (failed, warnings) = read(&snapshot, now);
    assert!(failed.is_none());
    assert!(!warnings[0].contains("private provider detail"));
    snapshot.last_error = None;
    snapshot.credentials_expired_at = Some(now - 1);
    assert!(read(&snapshot, now).0.is_none());
    snapshot.credentials_expired_at = None;
    snapshot.fetched_at = Some(now + 1);
    assert!(read(&snapshot, now).0.is_none());
}

#[test]
fn expired_window_nulls_percentages_even_with_historical_calibration() {
    let mut index = Index::default();
    let reset = START;
    index.ingest(sample(
        "cal-expired",
        reset - 3600,
        "cutex.cal",
        "opus",
        tokens(10000, 0, 0, 0),
    ));
    index.ingest(sample(
        "current-expired",
        RESET - 60,
        "cutex.current",
        "opus",
        tokens(4000, 0, 0, 0),
    ));
    let history = window_points(reset, &[(600, 1.0), (5 * 3600 - 120, 50.0)]);
    let keys = profiles();
    let old = WindowNow {
        used: 60.0,
        reset_at: Some(RESET),
        observed_at: START + 3600,
    };
    let result = compute(
        &index,
        &inputs(WindowKind::FiveHour, RESET, &keys, Some(old), &history, &[]),
    );
    assert!(result.calibration.units_per_percent.is_some());
    assert!(!result.rows.is_empty());
    assert!(result.rows.iter().all(|row| row.percent.is_none()));
    assert_eq!(result.window.used_percent, None);
    assert_eq!(result.attributed_percent, None);
    assert_eq!(result.unattributed_percent, None);
    assert_eq!(result.forecast.state, "unknown");
    assert_eq!(result.window.start_at, RESET - 5 * 3600);
    for used in [10.0, 100.0] {
        let stale = WindowNow { used, ..old };
        assert_eq!(
            forecast(&stale, &history, START + 3600 + FRESH_SECS + 1).state,
            "unknown"
        );
        assert_eq!(forecast(&stale, &history, RESET).state, "unknown");
    }
}

#[test]
fn refresh_filters_account_ids_before_decoding_and_preserves_legacy_names() {
    let root = temp_root("account-filter");
    let now = START + 300;
    let mut entries = vec![sample(
        "owned",
        START,
        "cutex.owned",
        "opus",
        tokens(11, 0, 0, 0),
    )];
    for (id, profile_id, name) in [
        ("other", Some("other-profile"), "Account-C"),
        ("legacy-owned", None, "Account-C"),
        ("legacy-other", None, "Other"),
    ] {
        let UsageLedgerEntry::Usage(mut row) = sample(id, START, id, "opus", tokens(10, 0, 0, 0))
        else {
            unreachable!()
        };
        row.profile_id = profile_id.map(str::to_string);
        row.profile = Some(name.into());
        entries.push(UsageLedgerEntry::Usage(row));
    }
    entries.push(sample(
        "too-old",
        now - 5 * 24 * 3600,
        "cutex.old",
        "opus",
        tokens(999, 0, 0, 0),
    ));
    append(&root, "2026-10", &entries);
    let keys = [PROFILE.to_string(), "name:Account-C".to_string()]
        .into_iter()
        .collect();
    let mut index = Index::default();
    refresh_at(&mut index, &root, now, WindowKind::FiveHour, &keys).unwrap();
    // Modern foreign ID and out-of-window sample are filtered by SQL; legacy names need decoding.
    assert_eq!(index.decoded_rows, 3);
    assert_eq!(
        index
            .range(&keys, now - 5 * 3600, now + 1)
            .map(|(_, amount)| amount.units)
            .sum::<f64>(),
        21.0
    );
    assert_eq!(WindowKind::FiveHour.lookback_secs(), 99 * 3600);
    assert_eq!(WindowKind::SevenDay.lookback_secs(), 36 * 24 * 3600);
    fs::remove_dir_all(root).unwrap();
}
