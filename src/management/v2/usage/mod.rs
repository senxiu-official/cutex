pub mod catalog;
pub mod estimate;
pub mod external;
mod index;
mod model;
mod pricing;
mod prices;
mod query;
mod quota_history;
mod reducer;
mod report;
mod runtime_context;
mod store;

use std::path::Path;

use crate::config::paths::runtime_dir;
use crate::management::v2::model::EventEnvelope;

pub use model::ResetWindowKind;
pub use model::TokenUsageBreakdown;
pub use model::UsageAttribution;
pub use model::UsageLedger;
pub use model::UsageLedgerEntry;
pub use model::UsageResetBoundary;
pub use model::UsageSample;
pub use model::UsageSampleSource;
pub use model::UsageStateSnapshot;
pub use model::UsageTokenSource;
pub use runtime_context::UsageRuntimeContext;
pub use pricing::DerivedUsageEstimate;
pub use pricing::UsageRateCard;
pub use report::build_usage_report;
pub use report::UsagePeriodBucket;
pub use report::UsagePricingGap;
pub use report::UsageReport;
pub use report::UsageReportCoverage;
pub use report::UsageReportGroup;
pub use report::UsageReportGroupBy;
pub use report::UsageReportPeriod;
pub use report::UsageReportQuery;
pub use report::UsageReportRow;
pub use report::UsageReportScope;
pub use report::UsageReportTotals;
pub use prices::estimate_cost;
pub use prices::load_price_table;
pub use prices::load_prices;
pub use prices::LoadedPrices;
pub use estimate::PriceContext;
pub use estimate::PriceOrigin;
pub use query::UnpricedModel;
pub use prices::save_price_table;
pub use prices::ModelPrice;
pub use prices::UsagePriceTable;
pub use query::aggregate_usage;
pub use query::UsageAggregation;
pub use index::ReadStats as UsageReadStats;
pub use index::SyncStats as UsageIndexSync;
pub use store::UsageIndexVerification;
pub use query::UsageGroupKey;
pub use query::UsageLabels;
pub use query::UsageQuery;
pub use query::UsageQueryResult;
pub use query::UsageSourceLabel;
pub use query::MAX_RANGE_DAYS;
pub use quota_history::record_quota_gauges;
pub use quota_history::read_since as read_quota_history_since;
pub use quota_history::QuotaSnapshot;

/// Delete derived per-session state; the append-only usage ledger stays intact.
pub fn forget_deleted_session(id: &str) -> anyhow::Result<()> {
    store::forget_deleted_session_at(&runtime_dir()?.join("management-v2"), id)
}

pub fn record_usage_event(
    envelope: &EventEnvelope,
    launched_profile: Option<&str>,
) -> anyhow::Result<()> {
    let root = runtime_dir()?.join("management-v2");
    record_usage_event_at(&root, envelope, launched_profile)
}

pub fn load_usage_state_snapshot() -> anyhow::Result<UsageStateSnapshot> {
    let root = runtime_dir()?.join("management-v2");
    store::load_usage_state_snapshot_at(&root)
}

pub fn load_usage_ledger() -> anyhow::Result<UsageLedger> {
    let root = runtime_dir()?.join("management-v2");
    store::load_usage_ledger_at(&root)
}

pub fn load_usage_data() -> anyhow::Result<(UsageStateSnapshot, UsageLedger)> {
    let root = runtime_dir()?.join("management-v2");
    store::load_usage_data_at(&root)
}

/// As [`record_usage_event`] for a runtime whose launched profile is known by
/// id: rows store the id plus the name it has now (`profileId`, `profile`).
pub fn record_usage_event_for_profile(
    envelope: &EventEnvelope,
    profile_id: Option<&str>,
    profile_name: Option<&str>,
) -> anyhow::Result<()> {
    let root = runtime_dir()?.join("management-v2");
    record_usage_event_for_profile_at(&root, envelope, profile_id, profile_name)
}

/// Runtime bootstrap evidence is captured once from the native response and
/// the verified launch catalog; it is independent of subsequent profile edits.
pub fn record_usage_event_for_runtime(
    envelope: &EventEnvelope,
    profile_id: Option<&str>,
    profile_name: Option<&str>,
    runtime: &UsageRuntimeContext,
) -> anyhow::Result<()> {
    let root = runtime_dir()?.join("management-v2");
    record_usage_event_for_runtime_at(&root, envelope, profile_id, profile_name, Some(runtime))
}

fn record_usage_event_at(
    root: &Path,
    envelope: &EventEnvelope,
    launched_profile: Option<&str>,
) -> anyhow::Result<()> {
    record_usage_event_for_profile_at(root, envelope, None, launched_profile)
}

fn record_usage_event_for_profile_at(
    root: &Path,
    envelope: &EventEnvelope,
    profile_id: Option<&str>,
    profile_name: Option<&str>,
) -> anyhow::Result<()> {
    record_usage_event_for_runtime_at(root, envelope, profile_id, profile_name, None)
}

fn record_usage_event_for_runtime_at(
    root: &Path,
    envelope: &EventEnvelope,
    profile_id: Option<&str>,
    profile_name: Option<&str>,
    runtime: Option<&UsageRuntimeContext>,
) -> anyhow::Result<()> {
    let Some(event) = reducer::parse_usage_event(envelope)? else {
        return Ok(());
    };
    store::update_usage_state_at(root, &envelope.received_at, |state| {
        reducer::reduce_usage_event(state, envelope, profile_id, profile_name, event, runtime)
    })
}

/// Append one usage sample observed outside a native event stream (the
/// `web_search` cutex tool's separate provider request). Validated like
/// every other row; the store lock makes it safe across processes.
pub fn record_direct_usage_sample(sample: UsageSample) -> anyhow::Result<()> {
    let root = runtime_dir()?.join("management-v2");
    record_direct_usage_sample_at(&root, sample)
}

#[cfg(test)]
pub(crate) fn record_direct_usage_sample_at_for_tests(root: &Path, sample: UsageSample) -> anyhow::Result<()> {
    record_direct_usage_sample_at(root, sample)
}

#[cfg(test)]
pub(crate) fn load_usage_ledger_at_for_tests(root: &Path) -> anyhow::Result<UsageLedger> {
    store::load_usage_ledger_at(root)
}

fn record_direct_usage_sample_at(root: &Path, sample: UsageSample) -> anyhow::Result<()> {
    let session_id = sample.cutex_session_id.clone();
    let entry = UsageLedgerEntry::Usage(Box::new(sample));
    entry.validate()?;
    let observed_at = entry.observed_at().to_string();
    store::update_usage_state_at(root, &observed_at, |state| {
        if state.deleted_sessions.contains(&session_id) {
            return Ok(store::UsageMutation::default());
        }
        Ok(store::UsageMutation { changed: true, entries: vec![entry] })
    })
}

/// Rewrite the profile of every ledger entry to its current attribution
/// (`identity::ProfileNameDirectory`): current name + id, or the historical
/// name with `profile_deleted` set. Done before pricing and grouping.
pub fn canonicalize_ledger_profiles(
    ledger: &mut UsageLedger,
    directory: &crate::profiles::identity::ProfileNameDirectory,
) {
    for entry in &mut ledger.entries {
        match entry {
            UsageLedgerEntry::Usage(sample) => canonicalize_sample_profile(sample, directory),
            UsageLedgerEntry::ResetBoundary(boundary) => {
                if let Some(resolved) = resolve_profile(
                    directory,
                    boundary.profile_id.as_deref(),
                    boundary.profile.as_deref(),
                    &boundary.observed_at,
                ) {
                    boundary.profile_id = resolved.id;
                    boundary.profile = Some(resolved.name);
                }
            }
        }
    }
}

/// [`canonicalize_ledger_profiles`] for one sample.
pub fn canonicalize_sample_profile(
    sample: &mut UsageSample,
    directory: &crate::profiles::identity::ProfileNameDirectory,
) {
    let Some(resolved) = resolve_profile(
        directory,
        sample.profile_id.as_deref(),
        sample.profile.as_deref(),
        &sample.observed_at,
    ) else {
        return;
    };
    sample.profile_id = resolved.id;
    sample.profile = Some(resolved.name);
    sample.profile_deleted = resolved.state == crate::profiles::identity::ProfileState::Deleted;
}

fn resolve_profile(
    directory: &crate::profiles::identity::ProfileNameDirectory,
    id: Option<&str>,
    name: Option<&str>,
    observed: &str,
) -> Option<crate::profiles::identity::ResolvedProfile> {
    let observed_at = model::parse_timestamp(observed)
        .ok()
        .map(|at| at.with_timezone(&chrono::Utc));
    directory.resolve(id, name, observed_at)
}

#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod tests;

/// Which samples [`visit_usage_samples_for_days`] hands over.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageSampleSelection<'a> {
    /// `?source=` of `GET /v2/usage`: `cutex`, `external` or a source id.
    pub source: Option<&'a str>,
    /// Only samples whose (current) profile id is one of these.
    pub profile_ids: Option<&'a [String]>,
}

/// Usage samples that can fall on local days `[from, to]` (the month files
/// the old reader opened: one day of slack each side), selected through the
/// usage index and decoded one at a time in ledger order, with profiles
/// canonicalized as [`canonicalize_ledger_profiles`]. Rows of a disabled
/// extra source are kept but not counted. Returns the invalid-line warnings
/// of those month files and the work counters.
pub fn visit_usage_samples_for_days(
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    selection: UsageSampleSelection<'_>,
    visit: impl FnMut(&UsageSample),
) -> anyhow::Result<(Vec<String>, UsageReadStats)> {
    let root = runtime_dir()?.join("management-v2");
    let hidden = external::disabled_source_ids();
    let directory = crate::profiles::identity::ProfileNameDirectory::load();
    visit_usage_samples_for_days_at(&root, &hidden, &directory, from, to, selection, visit)
}

fn visit_usage_samples_for_days_at(
    root: &Path,
    hidden: &std::collections::HashSet<String>,
    directory: &crate::profiles::identity::ProfileNameDirectory,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    selection: UsageSampleSelection<'_>,
    mut visit: impl FnMut(&UsageSample),
) -> anyhow::Result<(Vec<String>, UsageReadStats)> {
    let (first, last) = query::month_bounds(from, to);
    // The instants of local days [from, to] (the daemon reports in
    // chrono::Local), with an hour of margin for offset changes; the
    // aggregation applies the exact local day.
    let filter = index::SampleFilter {
        from_ms: local_midnight_ms(from) - MARGIN_MS,
        to_ms: local_midnight_ms(to + chrono::Duration::days(1)) + MARGIN_MS,
        source: selection.source.map(str::to_string),
        profile_ids: selection.profile_ids.map(<[String]>::to_vec),
    };
    store::read_usage_samples_at(root, (&first, &last), hidden, &filter, |mut sample| {
        canonicalize_sample_profile(&mut sample, directory);
        let wanted = selection.profile_ids.is_none_or(|ids| {
            sample.profile_id.as_ref().is_some_and(|id| ids.iter().any(|wanted| wanted == id))
        });
        if wanted {
            visit(&sample);
        }
        Ok(())
    })
}

/// Build or catch up the usage index now (kit migration step); reports do
/// this themselves on every read.
pub fn sync_usage_index() -> anyhow::Result<UsageIndexSync> {
    store::sync_usage_index_at(&runtime_dir()?.join("management-v2"))
}

/// Compare the index with the old full reader (kit verification after
/// [`sync_usage_index`]); fails on the first difference.
pub fn verify_usage_index() -> anyhow::Result<UsageIndexVerification> {
    store::verify_usage_index_at(&runtime_dir()?.join("management-v2"))
}

/// Path of the usage index database (kit disk accounting).
pub fn usage_index_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(runtime_dir()?.join("management-v2").join(index::USAGE_INDEX_FILE))
}

pub fn load_quota_history(
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    profile: Option<&str>,
    local_date: &dyn Fn(chrono::DateTime<chrono::Utc>) -> chrono::NaiveDate,
) -> anyhow::Result<Vec<QuotaSnapshot>> {
    quota_history::read_quota_history_at(&quota_history::history_root()?, from, to, profile, local_date)
}

#[cfg(test)]
mod ledger_query_tests;

/// Pricing context of the daemon: `config.json` `usage` settings, the
/// manual table, the cached library and the effective exchange rates.
/// `manual` is the already-loaded table (`None` when unreadable).
pub fn load_price_context(
    manual: Option<UsagePriceTable>,
    profile_categories: std::collections::HashMap<String, crate::profiles::provider_category::ProviderCategory>,
) -> anyhow::Result<(PriceContext, Vec<String>)> {
    let settings = crate::config::usage_pricing::current();
    let (mut context, warnings) = PriceContext::load_at(&catalog::cache_dir()?, &settings, manual);
    context.profile_categories = profile_categories;
    Ok((context, warnings))
}

const MARGIN_MS: i64 = 3_600_000;

/// First instant of local `day` (ms). A midnight skipped by a clock change
/// falls back to the widest offset, so the bound only ever widens.
fn local_midnight_ms(day: chrono::NaiveDate) -> i64 {
    use chrono::TimeZone;
    let naive = day.and_time(chrono::NaiveTime::MIN);
    match chrono::Local.from_local_datetime(&naive).earliest() {
        Some(at) => at.timestamp_millis(),
        None => naive.and_utc().timestamp_millis() - 14 * MARGIN_MS,
    }
}

/// Daemon start: bring the usage index up to the ledger in the background
/// (normally a stat per file; a full build if the kit step did not run), so
/// the first report does not pay for it. Reports still catch up themselves.
pub fn start_index_catch_up() {
    let spawned = std::thread::Builder::new()
        .name("cutex-usage-index".to_string())
        .spawn(|| {
            if let Err(error) = sync_usage_index() {
                eprintln!("usage index: startup catch-up failed: {error:#}");
            }
        });
    if let Err(error) = spawned {
        eprintln!("usage index: could not start catch-up: {error}");
    }
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod index_tests;
