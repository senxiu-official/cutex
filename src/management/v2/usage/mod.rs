pub mod catalog;
pub mod estimate;
pub mod external;
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
pub use query::UsageGroupKey;
pub use query::UsageLabels;
pub use query::UsageQuery;
pub use query::UsageQueryResult;
pub use query::UsageSourceLabel;
pub use query::MAX_RANGE_DAYS;
pub use quota_history::record_quota_gauges;
pub use quota_history::QuotaSnapshot;

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
    let entry = UsageLedgerEntry::Usage(Box::new(sample));
    entry.validate()?;
    let observed_at = entry.observed_at().to_string();
    store::update_usage_state_at(root, &observed_at, |_| {
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
        let (id, name, deleted, observed) = match entry {
            UsageLedgerEntry::Usage(sample) => (
                &mut sample.profile_id,
                &mut sample.profile,
                Some(&mut sample.profile_deleted),
                sample.observed_at.clone(),
            ),
            UsageLedgerEntry::ResetBoundary(boundary) => (
                &mut boundary.profile_id,
                &mut boundary.profile,
                None,
                boundary.observed_at.clone(),
            ),
        };
        let observed_at = model::parse_timestamp(&observed)
            .ok()
            .map(|at| at.with_timezone(&chrono::Utc));
        let Some(resolved) = directory.resolve(id.as_deref(), name.as_deref(), observed_at) else {
            continue;
        };
        *id = resolved.id;
        *name = Some(resolved.name);
        if let Some(deleted) = deleted {
            *deleted = resolved.state == crate::profiles::identity::ProfileState::Deleted;
        }
    }
}

#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod tests;

/// Ledger entries from the month files that can hold local days `[from, to]`.
pub fn load_usage_ledger_for_days(
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<UsageLedger> {
    let root = runtime_dir()?.join("management-v2");
    let (first, last) = query::month_bounds(from, to);
    // Rows of a disabled extra source are kept but not counted.
    let hidden = external::disabled_source_ids();
    store::load_usage_ledger_months_at(&root, &first, &last, &hidden)
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
