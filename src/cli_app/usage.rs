use std::collections::HashMap;

use anyhow::Context;
use chrono::DateTime;
use chrono::Duration;
use chrono::NaiveDate;
use chrono::Utc;
use cutex::cli::args::UsageGroupByArg;
use cutex::cli::args::UsagePeriodArg;
use cutex::cli::args::UsageResetWindowArg;
use cutex::management::v2::usage::build_usage_report;
use cutex::management::v2::usage::load_usage_data;
use cutex::management::v2::usage::DerivedUsageEstimate;
use cutex::management::v2::usage::ResetWindowKind;
use cutex::management::v2::usage::UsageReport;
use cutex::management::v2::usage::UsageReportGroupBy;
use cutex::management::v2::usage::UsageReportPeriod;
use cutex::management::v2::usage::UsageReportQuery;
use cutex::session::service::cutex_session_display_name;
use cutex::session::store::load_cutex_session_store;

pub(super) fn run(
    period: UsagePeriodArg,
    group_by: UsageGroupByArg,
    since: Option<&str>,
    until: Option<&str>,
    last: Option<&str>,
    reset_window: UsageResetWindowArg,
    json: bool,
) -> anyhow::Result<()> {
    let generated_at = Utc::now();
    let query = report_query(
        period,
        group_by,
        since,
        until,
        last,
        reset_window,
        generated_at,
    )?;
    let (state, mut ledger) = load_usage_data()?;
    cutex::management::v2::usage::canonicalize_ledger_profiles(
        &mut ledger,
        &cutex::profiles::identity::ProfileNameDirectory::load(),
    );
    let (agent_labels, label_warning) = agent_labels();
    let mut report = build_usage_report(&ledger, &state, &query, &agent_labels, generated_at)?;
    if let Some(warning) = label_warning {
        report.warnings.push(warning);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn report_query(
    period: UsagePeriodArg,
    group_by: UsageGroupByArg,
    since: Option<&str>,
    until: Option<&str>,
    last: Option<&str>,
    reset_window: UsageResetWindowArg,
    now: DateTime<Utc>,
) -> anyhow::Result<UsageReportQuery> {
    let explicit_until = until
        .map(|value| parse_time_bound(value, "--until"))
        .transpose()?;
    let until = if last.is_some() {
        Some(explicit_until.unwrap_or(now))
    } else {
        explicit_until
    };
    let since = match (since, last) {
        (Some(value), None) => Some(parse_time_bound(value, "--since")?),
        (None, Some(value)) => {
            let end = until.context("usage --last requires an effective upper bound")?;
            Some(
                end.checked_sub_signed(parse_relative_duration(value)?)
                    .context("usage --last range is outside the timestamp range")?,
            )
        }
        (None, None) => None,
        (Some(_), Some(_)) => anyhow::bail!("usage --since conflicts with --last"),
    };
    Ok(UsageReportQuery {
        period: match period {
            UsagePeriodArg::Total => UsageReportPeriod::Total,
            UsagePeriodArg::Hour => UsageReportPeriod::Hour,
            UsagePeriodArg::Day => UsageReportPeriod::Day,
            UsagePeriodArg::Week => UsageReportPeriod::Week,
            UsagePeriodArg::Reset => UsageReportPeriod::Reset,
        },
        group_by: match group_by {
            UsageGroupByArg::Agent => UsageReportGroupBy::Agent,
            UsageGroupByArg::Profile => UsageReportGroupBy::Profile,
            UsageGroupByArg::Model => UsageReportGroupBy::Model,
        },
        since,
        until,
        reset_window: match reset_window {
            UsageResetWindowArg::Primary => ResetWindowKind::Primary,
            UsageResetWindowArg::Secondary => ResetWindowKind::Secondary,
        },
    })
}

fn parse_time_bound(value: &str, option: &str) -> anyhow::Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|timestamp| DateTime::from_naive_utc_and_offset(timestamp, Utc))
            .context("failed to construct UTC date boundary");
    }
    anyhow::bail!("usage {option} must be RFC3339 or YYYY-MM-DD: {value}")
}

fn parse_relative_duration(value: &str) -> anyhow::Result<Duration> {
    let value = value.trim().to_ascii_lowercase();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .context("usage --last requires a unit: h, d, or w")?;
    let (amount, unit) = value.split_at(split);
    let amount = amount
        .parse::<i64>()
        .with_context(|| format!("invalid usage --last duration: {value}"))?;
    if amount <= 0 {
        anyhow::bail!("usage --last duration must be positive");
    }
    match unit {
        "h" => Duration::try_hours(amount),
        "d" => Duration::try_days(amount),
        "w" => amount.checked_mul(7).and_then(Duration::try_days),
        _ => None,
    }
    .with_context(|| format!("invalid usage --last duration: {value}; use h, d, or w"))
}

fn agent_labels() -> (HashMap<String, String>, Option<String>) {
    match load_cutex_session_store() {
        Ok(store) => (
            store
                .sessions
                .values()
                .map(|record| {
                    (
                        record.cutex_session_id.clone(),
                        cutex_session_display_name(record),
                    )
                })
                .collect(),
            None,
        ),
        Err(error) => (
            HashMap::new(),
            Some(format!(
                "agent names unavailable; using cutex_session_id: {error:#}"
            )),
        ),
    }
}

fn render_text(report: &UsageReport) -> String {
    let period_width = report
        .rows
        .iter()
        .map(|row| row.period.label.chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 29);
    let group_width = report
        .rows
        .iter()
        .map(|row| row.group.label.chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 38);
    let mut output = String::new();
    output.push_str("Cutex usage (UTC)\n");
    output.push_str(&format!(
        "period={} group={} range={}..{} samples={}/{}\n",
        enum_json(&report.scope.period),
        enum_json(&report.scope.group_by),
        report.scope.since.as_deref().unwrap_or("recorded-start"),
        report.scope.until.as_deref().unwrap_or("recorded-end"),
        report.coverage.selected_samples,
        report.coverage.ledger_usage_samples,
    ));
    output.push_str(&format!(
        "{:<period_width$}  {:<group_width$}  {:>7}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}  {:>11}  {:>11}\n",
        "PERIOD",
        "GROUP",
        "SAMPLES",
        "INPUT",
        "CACHED",
        "WRITE",
        "OUTPUT",
        "REASON",
        "TOTAL",
        "CREDITS",
        "API USD",
    ));
    for row in &report.rows {
        output.push_str(&format!(
            "{:<period_width$}  {:<group_width$}  {:>7}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}  {:>11}  {:>11}\n",
            truncate(&row.period.label, period_width),
            truncate(&row.group.label, group_width),
            row.usage_samples,
            format_count(row.tokens.input_tokens),
            format_count(row.tokens.cached_input_tokens),
            format_count(row.tokens.cache_write_input_tokens),
            format_count(row.tokens.output_tokens),
            format_count(row.tokens.reasoning_output_tokens),
            format_count(row.tokens.total_tokens),
            format_estimate(&row.codex_credits),
            format_estimate(&row.api_equivalent_usd),
        ));
    }
    if report.rows.is_empty() {
        output.push_str("No usage samples matched this range.\n");
    }
    output.push_str(&format!(
        "{:<period_width$}  {:<group_width$}  {:>7}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}  {:>12}  {:>11}  {:>11}\n",
        "TOTAL",
        "",
        report.totals.usage_samples,
        format_count(report.totals.tokens.input_tokens),
        format_count(report.totals.tokens.cached_input_tokens),
        format_count(report.totals.tokens.cache_write_input_tokens),
        format_count(report.totals.tokens.output_tokens),
        format_count(report.totals.tokens.reasoning_output_tokens),
        format_count(report.totals.tokens.total_tokens),
        format_estimate(&report.totals.codex_credits),
        format_estimate(&report.totals.api_equivalent_usd),
    ));
    output.push_str("INPUT includes CACHED/WRITE; REASON is a subset of OUTPUT.\n");
    output.push_str(&format!(
        "rate cards: {}, {}\n",
        report.totals.codex_credits.rate_card_id, report.totals.api_equivalent_usd.rate_card_id
    ));
    if report.totals.codex_credits.unpriced_samples > 0
        || report.totals.api_equivalent_usd.unpriced_samples > 0
    {
        output.push_str("* derived estimate is partial; use --json for pricing gaps.\n");
    }
    for warning in &report.warnings {
        output.push_str(&format!("warning: {warning}\n"));
    }
    output
}

fn enum_json(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn format_estimate(estimate: &DerivedUsageEstimate) -> String {
    if estimate.priced_samples == 0 {
        return "-".to_string();
    }
    if estimate.unpriced_samples > 0 {
        format!("{}*", estimate.amount)
    } else {
        estimate.amount.clone()
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 3 {
        return value.chars().take(width).collect();
    }
    let mut output = value.chars().take(width - 3).collect::<String>();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use cutex::cli::args::Cli;
    use cutex::cli::args::CommandKind;

    use super::*;

    #[test]
    fn usage_cli_parses_time_group_and_json_options() {
        let cli = Cli::try_parse_from([
            "cutex",
            "usage",
            "--period",
            "reset",
            "--group-by",
            "profile",
            "--last",
            "8w",
            "--reset-window",
            "secondary",
            "--json",
        ])
        .expect("parse usage command");
        assert!(matches!(
            cli.command,
            Some(CommandKind::Usage {
                period: UsagePeriodArg::Reset,
                group_by: UsageGroupByArg::Profile,
                last: Some(value),
                reset_window: UsageResetWindowArg::Secondary,
                json: true,
                ..
            }) if value == "8w"
        ));
    }

    #[test]
    fn report_query_supports_dates_rfc3339_and_relative_ranges() {
        let now = parse_time_bound("2026-08-13T12:00:00Z", "now").expect("parse now");
        let query = report_query(
            UsagePeriodArg::Week,
            UsageGroupByArg::Agent,
            None,
            Some("2026-08-13"),
            Some("7d"),
            UsageResetWindowArg::Primary,
            now,
        )
        .expect("build query");
        assert_eq!(query.since, Some(utc("2026-08-06T00:00:00Z")));
        assert_eq!(query.until, Some(utc("2026-08-13T00:00:00Z")));

        let query = report_query(
            UsagePeriodArg::Day,
            UsageGroupByArg::Model,
            Some("2026-08-01T10:00:00+10:00"),
            None,
            None,
            UsageResetWindowArg::Primary,
            now,
        )
        .expect("build absolute query");
        assert_eq!(query.since, Some(utc("2026-08-01T00:00:00Z")));

        let query = report_query(
            UsagePeriodArg::Hour,
            UsageGroupByArg::Agent,
            None,
            None,
            Some("24h"),
            UsageResetWindowArg::Primary,
            now,
        )
        .expect("build current relative query");
        assert_eq!(query.since, Some(utc("2026-08-12T12:00:00Z")));
        assert_eq!(query.until, Some(now));
    }

    #[test]
    fn relative_range_rejects_missing_unknown_and_zero_units() {
        for value in ["7", "7m", "0d", "-1d"] {
            assert!(parse_relative_duration(value).is_err(), "{value}");
        }
    }

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("valid timestamp")
            .with_timezone(&Utc)
    }
}
