use std::collections::HashMap;

use chrono::DateTime;
use chrono::Utc;

use super::*;

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("valid timestamp")
        .with_timezone(&Utc)
}

fn tokens(total: u64, input: u64, cached: u64, write: u64, output: u64) -> TokenUsageBreakdown {
    TokenUsageBreakdown {
        total_tokens: total,
        input_tokens: input,
        cached_input_tokens: cached,
        cache_write_input_tokens: write,
        output_tokens: output,
        reasoning_output_tokens: output / 2,
    }
}

#[allow(clippy::too_many_arguments)]
fn sample(
    id: &str,
    observed_at: &str,
    session: &str,
    profile: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    tier: Option<&str>,
    tokens: TokenUsageBreakdown,
) -> UsageLedgerEntry {
    UsageLedgerEntry::Usage(Box::new(UsageSample {
        entry_id: format!("usage:{id}"),
        event_id: id.to_string(),
        observed_at: observed_at.to_string(),
        host_id: "test-host".to_string(),
        cutex_session_id: session.to_string(),
        thread_id: format!("thread-{session}"),
        turn_id: format!("turn-{id}"),
        runtime_generation: 1,
        profile: profile.map(str::to_string),
        profile_id: None,
        profile_deleted: false,
        source: None,
        attribution: UsageAttribution {
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            request_model: None,
            reasoning_effort: Some("xhigh".to_string()),
            service_tier: tier.map(str::to_string),
        },
        token_source: UsageTokenSource::NativeLast,
        model_context_window: Some(400_000),
        compaction_count: None,
        cumulative_total: tokens.clone(),
        tokens,
    }))
}

fn reset(
    id: &str,
    observed_at: &str,
    profile: Option<&str>,
    window: ResetWindowKind,
    duration_mins: i64,
    resets_at: i64,
) -> UsageLedgerEntry {
    UsageLedgerEntry::ResetBoundary(UsageResetBoundary {
        entry_id: format!("reset:{id}"),
        event_id: id.to_string(),
        observed_at: observed_at.to_string(),
        host_id: "test-host".to_string(),
        profile: profile.map(str::to_string),
        profile_id: None,
        limit_id: Some("codex".to_string()),
        window_kind: window,
        window_duration_mins: duration_mins,
        resets_at,
    })
}

fn state() -> UsageStateSnapshot {
    UsageStateSnapshot {
        revision: 7,
        observed_since: Some("2026-08-01T00:00:00Z".to_string()),
        updated_at: Some("2026-08-20T00:00:00Z".to_string()),
    }
}

#[test]
fn day_agent_report_filters_exclusive_bounds_and_resolves_current_names() {
    let ledger = UsageLedger {
        entries: vec![
            sample(
                "before",
                "2026-08-12T23:59:59Z",
                "session-a",
                Some("default"),
                None,
                None,
                None,
                tokens(10, 8, 4, 0, 2),
            ),
            sample(
                "a",
                "2026-08-13T01:00:00Z",
                "session-a",
                Some("default"),
                None,
                None,
                None,
                tokens(20, 16, 8, 0, 4),
            ),
            sample(
                "b",
                "2026-08-13T02:00:00Z",
                "session-b",
                Some("default"),
                None,
                None,
                None,
                tokens(30, 24, 12, 0, 6),
            ),
            sample(
                "at-until",
                "2026-08-14T00:00:00Z",
                "session-a",
                Some("default"),
                None,
                None,
                None,
                tokens(40, 32, 16, 0, 8),
            ),
        ],
        warnings: vec!["preserved ledger warning".to_string()],
    };
    let query = UsageReportQuery {
        period: UsageReportPeriod::Day,
        group_by: UsageReportGroupBy::Agent,
        since: Some(utc("2026-08-13T00:00:00Z")),
        until: Some(utc("2026-08-14T00:00:00Z")),
        reset_window: ResetWindowKind::Primary,
    };
    let labels = HashMap::from([
        ("session-a".to_string(), "alden".to_string()),
        ("session-b".to_string(), "healthbar".to_string()),
    ]);

    let report = build_usage_report(
        &ledger,
        &state(),
        &query,
        &labels,
        utc("2026-08-20T00:00:00Z"),
    )
    .expect("build report");

    assert_eq!(report.rows.len(), 2);
    assert_eq!(report.rows[0].period.label, "2026-08-13");
    assert_eq!(report.rows[0].group.label, "alden");
    assert_eq!(report.rows[1].group.label, "healthbar");
    assert_eq!(report.totals.usage_samples, 2);
    assert_eq!(report.totals.tokens.total_tokens, 50);
    assert_eq!(report.coverage.ledger_usage_samples, 4);
    assert_eq!(report.coverage.selected_samples, 2);
    assert_eq!(report.coverage.reducer_revision, 7);
    assert_eq!(
        report.coverage.first_selected_sample_at.as_deref(),
        Some("2026-08-13T01:00:00Z")
    );
    assert_eq!(
        report.coverage.last_selected_sample_at.as_deref(),
        Some("2026-08-13T02:00:00Z")
    );
    assert_eq!(report.warnings, vec!["preserved ledger warning"]);
}

#[test]
fn hour_and_iso_week_buckets_use_utc_boundaries() {
    let ledger = UsageLedger {
        entries: vec![
            sample(
                "sunday",
                "2026-08-16T23:59:59Z",
                "session-a",
                Some("default"),
                None,
                None,
                None,
                tokens(10, 8, 4, 0, 2),
            ),
            sample(
                "monday",
                "2026-08-17T00:00:00Z",
                "session-a",
                Some("default"),
                None,
                None,
                None,
                tokens(20, 16, 8, 0, 4),
            ),
        ],
        warnings: Vec::new(),
    };
    let labels = HashMap::new();
    let week_report = build_usage_report(
        &ledger,
        &state(),
        &UsageReportQuery {
            period: UsageReportPeriod::Week,
            group_by: UsageReportGroupBy::Profile,
            ..UsageReportQuery::default()
        },
        &labels,
        utc("2026-08-20T00:00:00Z"),
    )
    .expect("week report");
    assert_eq!(week_report.rows.len(), 2);
    assert_eq!(week_report.rows[0].period.label, "2026-W33");
    assert_eq!(
        week_report.rows[0].period.start.as_deref(),
        Some("2026-08-10T00:00:00Z")
    );
    assert_eq!(week_report.rows[1].period.label, "2026-W34");
    assert_eq!(
        week_report.rows[1].period.start.as_deref(),
        Some("2026-08-17T00:00:00Z")
    );

    let hour_report = build_usage_report(
        &ledger,
        &state(),
        &UsageReportQuery {
            period: UsageReportPeriod::Hour,
            group_by: UsageReportGroupBy::Profile,
            ..UsageReportQuery::default()
        },
        &labels,
        utc("2026-08-20T00:00:00Z"),
    )
    .expect("hour report");
    assert_eq!(hour_report.rows[0].period.label, "2026-08-16 23:00 UTC");
    assert_eq!(hour_report.rows[1].period.label, "2026-08-17 00:00 UTC");
}

#[test]
fn reset_report_matches_retrospective_boundaries_and_preserves_gaps() {
    let reset_at = utc("2026-08-17T00:00:00Z").timestamp();
    let ledger = UsageLedger {
        entries: vec![
            sample(
                "matched-before-observation",
                "2026-08-12T00:00:00Z",
                "session-a",
                Some("default"),
                None,
                None,
                None,
                tokens(10, 8, 4, 0, 2),
            ),
            reset(
                "weekly",
                "2026-08-13T00:00:00Z",
                Some("default"),
                ResetWindowKind::Primary,
                10_080,
                reset_at,
            ),
            sample(
                "outside",
                "2026-08-18T00:00:00Z",
                "session-a",
                Some("default"),
                None,
                None,
                None,
                tokens(20, 16, 8, 0, 4),
            ),
            sample(
                "other-profile",
                "2026-08-12T00:00:00Z",
                "session-b",
                Some("deepseek"),
                None,
                None,
                None,
                tokens(30, 24, 12, 0, 6),
            ),
        ],
        warnings: Vec::new(),
    };
    let report = build_usage_report(
        &ledger,
        &state(),
        &UsageReportQuery {
            period: UsageReportPeriod::Reset,
            group_by: UsageReportGroupBy::Profile,
            reset_window: ResetWindowKind::Primary,
            ..UsageReportQuery::default()
        },
        &HashMap::new(),
        utc("2026-08-20T00:00:00Z"),
    )
    .expect("reset report");

    assert_eq!(report.rows.len(), 3);
    let matched = report
        .rows
        .iter()
        .find(|row| row.period.reset_observed == Some(true))
        .expect("matched reset row");
    assert_eq!(matched.usage_samples, 1);
    assert_eq!(
        matched.period.start.as_deref(),
        Some("2026-08-10T00:00:00Z")
    );
    assert_eq!(matched.period.end.as_deref(), Some("2026-08-17T00:00:00Z"));
    assert_eq!(
        report
            .rows
            .iter()
            .filter(|row| row.period.reset_observed == Some(false))
            .count(),
        2
    );
    assert_eq!(report.totals.tokens.total_tokens, 60);
    assert!(report.warnings[0].contains("2 usage samples"));
}

#[test]
fn model_report_exposes_partial_versioned_estimates_and_pricing_gaps() {
    let ledger = UsageLedger {
        entries: vec![
            sample(
                "terra",
                "2026-08-13T00:00:00Z",
                "session-a",
                Some("default"),
                Some("openai"),
                Some("gpt-5.6-terra"),
                None,
                tokens(220_000, 200_000, 40_000, 20_000, 20_000),
            ),
            sample(
                "deepseek",
                "2026-08-13T01:00:00Z",
                "session-b",
                Some("deepseek"),
                Some("deepseek"),
                Some("deepseek-chat"),
                None,
                tokens(11_000, 10_000, 0, 0, 1_000),
            ),
        ],
        warnings: Vec::new(),
    };
    let report = build_usage_report(
        &ledger,
        &state(),
        &UsageReportQuery {
            period: UsageReportPeriod::Total,
            group_by: UsageReportGroupBy::Model,
            ..UsageReportQuery::default()
        },
        &HashMap::new(),
        utc("2026-08-20T00:00:00Z"),
    )
    .expect("model report");

    let terra = report
        .rows
        .iter()
        .find(|row| row.group.model.as_deref() == Some("gpt-5.6-terra"))
        .expect("terra row");
    assert_eq!(terra.codex_credits.amount, "13.2");
    assert_eq!(terra.api_equivalent_usd.amount, "0.578");
    assert_eq!(terra.codex_credits.priced_samples, 1);
    assert_eq!(terra.codex_credits.unpriced_samples, 0);

    assert_eq!(report.totals.codex_credits.amount, "13.2");
    assert_eq!(report.totals.codex_credits.priced_samples, 1);
    assert_eq!(report.totals.codex_credits.unpriced_samples, 1);
    assert_eq!(report.totals.api_equivalent_usd.amount, "0.578");
    assert_eq!(report.pricing_gaps.len(), 1);
    assert_eq!(
        report.pricing_gaps[0].attribution.provider.as_deref(),
        Some("deepseek")
    );
    assert_eq!(report.pricing_gaps[0].tokens.total_tokens, 11_000);
    assert_eq!(report.rate_cards.len(), 2);

    let json = serde_json::to_string(&report).expect("serialize report");
    assert!(json.contains("\"amount\":\"0.578\""));
    assert!(!json.contains("usedPercent"));
}

#[test]
fn provider_api_coverage_does_not_create_a_codex_credit_pricing_gap() {
    let ledger = UsageLedger {
        entries: vec![sample(
            "glm",
            "2026-08-13T00:00:00Z",
            "session-glm",
            Some("zai"),
            Some("GLM"),
            Some("glm-5.3-flash"),
            None,
            tokens(220_000, 200_000, 40_000, 20_000, 20_000),
        )],
        warnings: Vec::new(),
    };
    let report = build_usage_report(
        &ledger,
        &state(),
        &UsageReportQuery {
            period: UsageReportPeriod::Total,
            group_by: UsageReportGroupBy::Model,
            ..UsageReportQuery::default()
        },
        &HashMap::new(),
        utc("2026-08-20T00:00:00Z"),
    )
    .expect("GLM report");

    assert_eq!(report.totals.api_equivalent_usd.priced_samples, 1);
    assert_eq!(report.totals.api_equivalent_usd.unpriced_samples, 0);
    assert_eq!(report.totals.codex_credits.unpriced_samples, 1);
    assert!(report.pricing_gaps.is_empty());
}

#[test]
fn invalid_time_range_is_rejected() {
    let timestamp = utc("2026-08-13T00:00:00Z");
    let error = build_usage_report(
        &UsageLedger::default(),
        &UsageStateSnapshot::default(),
        &UsageReportQuery {
            since: Some(timestamp),
            until: Some(timestamp),
            ..UsageReportQuery::default()
        },
        &HashMap::new(),
        utc("2026-08-20T00:00:00Z"),
    )
    .expect_err("reject empty range");
    assert!(error.to_string().contains("--since must be earlier"));
}
