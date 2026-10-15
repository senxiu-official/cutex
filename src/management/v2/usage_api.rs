//! Root-only usage ledger routes:
//! - `GET /v2/usage?from=YYYY-MM-DD&to=YYYY-MM-DD&groupBy=day,profile`
//! - `GET|PUT /v2/usage/prices` (`?library=<substring>` searches the cached library)
//! - `POST /v2/usage/prices/refresh` (fetch the library / rates now)
//! - `GET /v2/usage/quota-history?profile=&from=&to=`
//! - `GET|POST /v2/usage/sources`, `PATCH|DELETE /v2/usage/sources/{id}`
//!   (`?purge=true`), `POST /v2/usage/sources/{id}/rescan`: extra usage
//!   sources (`usage/external.rs`, capability `usage-external-sources`;
//!   kind `zcode`: `usage/external_zcode.rs`, capability `usage-source-zcode`)
//!
//! `GET /v2/usage` also takes `source=cutex|external|<source id>` (filter)
//! and groupBy `source`.
//!
//! Dates are the daemon host's local calendar days, inclusive. Default range
//! is the last 7 days ending today; at most `MAX_RANGE_DAYS` days.

use std::collections::HashMap;
use std::net::TcpStream;

use chrono::DateTime;
use chrono::Duration;
use chrono::Local;
use chrono::NaiveDate;
use chrono::Utc;
use serde_json::json;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::management::v2::usage;
use crate::management::v2::usage::UsageGroupKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageRoute {
    Report,
    Prices,
    PricesRefresh,
    QuotaHistory,
    Sources,
    Source(String),
    SourceRescan(String),
}

pub fn parse_path(path: &str) -> Option<UsageRoute> {
    match path {
        "/v2/usage" => Some(UsageRoute::Report),
        "/v2/usage/prices" => Some(UsageRoute::Prices),
        "/v2/usage/prices/refresh" => Some(UsageRoute::PricesRefresh),
        "/v2/usage/quota-history" => Some(UsageRoute::QuotaHistory),
        "/v2/usage/sources" => Some(UsageRoute::Sources),
        _ => {
            let rest = path.strip_prefix("/v2/usage/sources/")?;
            let (id, rescan) = match rest.strip_suffix("/rescan") {
                Some(id) => (id, true),
                None => (rest, false),
            };
            usage::external::validate_id(id).ok()?;
            Some(if rescan {
                UsageRoute::SourceRescan(id.to_string())
            } else {
                UsageRoute::Source(id.to_string())
            })
        }
    }
}

fn local_date(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Local).date_naive()
}

#[derive(Debug, Default, PartialEq)]
struct Params {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    group_by: Option<Vec<UsageGroupKey>>,
    profile: Option<String>,
    source: Option<String>,
}

fn parse_params(full_path: &str, allowed: &[&str]) -> Result<Params, String> {
    let mut params = Params::default();
    let Some((_, query)) = full_path.split_once('?') else {
        return Ok(params);
    };
    let mut seen = Vec::new();
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let name = name.as_ref();
        if !allowed.contains(&name) {
            return Err(format!("unsupported query parameter: {name}"));
        }
        if seen.contains(&name.to_string()) {
            return Err(format!("query parameter {name} given twice"));
        }
        seen.push(name.to_string());
        match name {
            "from" | "to" => {
                let date = (value.len() == 10)
                    .then(|| NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok())
                    .flatten()
                    .ok_or_else(|| format!("{name} must be YYYY-MM-DD"))?;
                if name == "from" {
                    params.from = Some(date);
                } else {
                    params.to = Some(date);
                }
            }
            "groupBy" => params.group_by = Some(UsageGroupKey::parse_list(&value)?),
            "profile" => {
                let value = value.trim();
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    return Err("profile must be a non-empty profile ID or name".to_string());
                }
                params.profile = Some(value.to_string());
            }
            "source" => {
                let value = value.trim();
                if value != "cutex" && value != "external" && usage::external::validate_id(value).is_err() {
                    return Err("source must be cutex, external or a usage source id".to_string());
                }
                params.source = Some(value.to_string());
            }
            _ => unreachable!("allowed list checked above"),
        }
    }
    Ok(params)
}

/// Resolve `[from, to]` with defaults (last 7 days ending `today`).
fn resolve_range(
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    today: NaiveDate,
) -> Result<(NaiveDate, NaiveDate), String> {
    let (from, to) = match (from, to) {
        (Some(from), Some(to)) => (from, to),
        (Some(from), None) => (from, today.max(from)),
        (None, Some(to)) => (to - Duration::days(6), to),
        (None, None) => (today - Duration::days(6), today),
    };
    if from > to {
        return Err("from must not be after to".to_string());
    }
    if (to - from).num_days() + 1 > usage::MAX_RANGE_DAYS {
        return Err(format!(
            "range must span at most {} days",
            usage::MAX_RANGE_DAYS
        ));
    }
    Ok((from, to))
}

fn usage_labels() -> (usage::UsageLabels, Vec<String>) {
    let mut labels = usage::UsageLabels::default();
    let mut warnings = Vec::new();
    match crate::session::store::load_cutex_session_store() {
        Ok(store) => {
            labels.agent_names = store
                .sessions
                .values()
                .map(|record| {
                    (
                        record.cutex_session_id.clone(),
                        crate::session::service::cutex_session_display_name(record),
                    )
                })
                .collect();
        }
        Err(error) => warnings.push(format!("agent names unavailable: {error:#}")),
    }
    match crate::agent_management::AgentManagementStore::open_default()
        .and_then(|store| store.snapshot().map_err(anyhow::Error::new))
    {
        Ok(snapshot) => {
            let mut projects = HashMap::new();
            for (id, agent) in &snapshot.agents {
                if let Some(project_id) =
                    crate::agent_management::current_project_id(&snapshot, agent)
                {
                    let presentation = crate::agent_management::effective_presentation(
                        &project_id,
                        snapshot.project_presentations.get(&project_id),
                    );
                    projects.insert(
                        id.as_str().to_string(),
                        (project_id.as_str().to_string(), presentation.display_name),
                    );
                }
            }
            labels.projects = projects;
        }
        Err(error) => warnings.push(format!("projects unavailable: {error:#}")),
    }
    labels.sources = usage::external::source_labels();
    (labels, warnings)
}

/// `?source=`: keep cutex's own rows, every extra source's, or one source's.
fn retain_source(ledger: &mut usage::UsageLedger, filter: &str) {
    ledger.entries.retain(|entry| match entry {
        usage::UsageLedgerEntry::Usage(sample) => match (filter, &sample.source) {
            ("cutex", source) => source.is_none(),
            ("external", source) => source.is_some(),
            (id, Some(source)) => source.id == id,
            (_, None) => false,
        },
        usage::UsageLedgerEntry::ResetBoundary(_) => filter == "cutex",
    });
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    route: UsageRoute,
) -> anyhow::Result<()> {
    let bad_request = |stream: &mut TcpStream, message: &str| {
        super::server::write_v2_error(
            stream,
            400,
            "Bad Request",
            "invalid_request",
            message,
            false,
            json!({}),
        )
    };
    let unavailable = |stream: &mut TcpStream, error: anyhow::Error| {
        super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "usage_store_unavailable",
            "the usage store could not be read",
            true,
            json!({ "diagnostic": format!("{error:#}") }),
        )
    };
    let today = Local::now().date_naive();
    match (request.method.as_str(), route) {
        ("GET", UsageRoute::Report) => {
            let params = match parse_params(&request.path, &["from", "to", "groupBy", "source"]) {
                Ok(params) => params,
                Err(message) => return bad_request(stream, &message),
            };
            let (from, to) = match resolve_range(params.from, params.to, today) {
                Ok(range) => range,
                Err(message) => return bad_request(stream, &message),
            };
            let mut ledger = match usage::load_usage_ledger_for_days(from, to) {
                Ok(ledger) => ledger,
                Err(error) => return unavailable(stream, error),
            };
            if let Some(filter) = params.source.as_deref() {
                retain_source(&mut ledger, filter);
            }
            // Rows are attributed by profile id and shown under the current
            // name; untraceable rows keep their name with profileState deleted.
            usage::canonicalize_ledger_profiles(
                &mut ledger,
                &crate::profiles::identity::ProfileNameDirectory::load(),
            );
            let mut price_warnings = Vec::new();
            let prices = match usage::load_prices() {
                Ok(loaded) => {
                    if loaded.origin == "legacy-changed" {
                        price_warnings.push(LEGACY_CHANGED_WARNING.to_string());
                    }
                    Some(loaded.table)
                }
                Err(error) => {
                    price_warnings.push(format!("usage prices unreadable: {error:#}"));
                    None
                }
            };
            let pricing = match usage::load_price_context(prices, profile_categories()) {
                Ok((context, warnings)) => {
                    price_warnings.extend(warnings);
                    Some(context)
                }
                Err(error) => {
                    price_warnings.push(format!("usage pricing unavailable: {error:#}"));
                    None
                }
            };
            let (labels, label_warnings) = usage_labels();
            let query = usage::UsageQuery {
                from,
                to,
                group_by: params.group_by.unwrap_or_else(|| vec![UsageGroupKey::Day]),
            };
            let mut result =
                usage::aggregate_usage(&ledger, &query, &labels, pricing.as_ref(), &local_date);
            result.recording_since = usage::load_usage_state_snapshot()
                .ok()
                .and_then(|state| state.observed_since);
            result.warnings.extend(label_warnings);
            result.warnings.extend(price_warnings);
            write_json_response(stream, 200, "OK", &serde_json::to_value(result)?)
        }
        ("GET", UsageRoute::Prices) => {
            let library_query = match library_query(&request.path) {
                Ok(query) => query,
                Err(message) => return bad_request(stream, &message),
            };
            match usage::load_prices() {
                Ok(loaded) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &prices_resource(&loaded.table, loaded.origin, library_query.as_deref(), &[]),
                ),
                Err(error) => unavailable(stream, error),
            }
        }
        ("POST", UsageRoute::PricesRefresh) => {
            let settings = crate::config::usage_pricing::current();
            let errors = match usage::catalog::cache_dir() {
                Ok(dir) => usage::catalog::refresh_due(
                    &dir,
                    &settings,
                    &usage::catalog::HttpFetcher,
                    true,
                    Utc::now(),
                ),
                Err(error) => vec![format!("{error:#}")],
            };
            match usage::load_prices() {
                Ok(loaded) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &prices_resource(&loaded.table, loaded.origin, None, &errors),
                ),
                Err(error) => unavailable(stream, error),
            }
        }
        ("PUT", UsageRoute::Prices) => {
            let mut table: usage::UsagePriceTable = match serde_json::from_slice(&request.body) {
                Ok(table) => table,
                Err(error) => {
                    return bad_request(
                        stream,
                        &format!("strict body {{currency, models, modelMap?}} required: {error}"),
                    )
                }
            };
            // A client without `usage-model-map` sends no `modelMap`: keep
            // the stored map (minus names the new table prices directly).
            if !body_has_model_map(&request.body) {
                if let Ok(loaded) = usage::load_prices() {
                    let names: std::collections::HashSet<String> = table.index().into_keys().collect();
                    table.model_map = loaded
                        .table
                        .model_map
                        .into_iter()
                        .filter(|(from, _)| !names.contains(&from.to_ascii_lowercase()))
                        .collect();
                }
            }
            if let Err(error) = table.validate() {
                return bad_request(stream, &format!("{error:#}"));
            }
            let rates = current_rates();
            match usage::save_price_table(&table, &rates) {
                Ok(()) => write_json_response(stream, 200, "OK", &prices_resource(&table, "v2", None, &[])),
                Err(error) => super::server::write_v2_error(
                    stream,
                    500,
                    "Internal Server Error",
                    "usage_prices_write_failed",
                    &format!("{error:#}"),
                    true,
                    json!({}),
                ),
            }
        }
        ("GET", UsageRoute::QuotaHistory) => {
            let params = match parse_params(&request.path, &["from", "to", "profile"]) {
                Ok(params) => params,
                Err(message) => return bad_request(stream, &message),
            };
            let (from, to) = match resolve_range(params.from, params.to, today) {
                Ok(range) => range,
                Err(message) => return bad_request(stream, &message),
            };
            // A current name selects the profile by id (snapshots keep the name
            // they were taken under).
            let profile_key = params.profile.as_deref().map(|requested| {
                crate::profiles::identity::id_for_name(requested).unwrap_or_else(|| requested.to_string())
            });
            match usage::load_quota_history(from, to, profile_key.as_deref(), &local_date) {
                Ok(snapshots) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({
                        "contractVersion": 2,
                        "from": from.format("%Y-%m-%d").to_string(),
                        "to": to.format("%Y-%m-%d").to_string(),
                        "profile": params.profile,
                        "snapshots": snapshots
                            .iter()
                            .map(|snapshot| snapshot_resource(snapshot))
                            .collect::<Vec<_>>(),
                    }),
                ),
                Err(error) => unavailable(stream, error),
            }
        }
        (_, route @ (UsageRoute::Sources | UsageRoute::Source(_) | UsageRoute::SourceRescan(_))) => {
            handle_sources(stream, request, route)
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET for /v2/usage and /v2/usage/quota-history, GET or PUT for /v2/usage/prices, POST for /v2/usage/prices/refresh",
            false,
            json!({}),
        ),
    }
}

/// `/v2/usage/sources[/{id}[/rescan]]` (extra usage sources).
fn handle_sources(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    route: UsageRoute,
) -> anyhow::Result<()> {
    use usage::external::SourceError;
    let root = match usage::external::ledger_root() {
        Ok(root) => root,
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "usage_store_unavailable",
                "the usage store could not be read",
                true,
                json!({ "diagnostic": format!("{error:#}") }),
            )
        }
    };
    let source_error = |stream: &mut TcpStream, error: SourceError| match error {
        SourceError::Invalid(message) => super::server::write_v2_error(
            stream, 400, "Bad Request", "invalid_request", &message, false, json!({}),
        ),
        SourceError::NotFound => super::server::write_v2_error(
            stream, 404, "Not Found", "usage_source_not_found", "no usage source has this id", false, json!({}),
        ),
        SourceError::OverlapsCutex(root) => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "usage_source_overlaps_cutex",
            "this directory holds (or is inside) runs cutex already records; adding it would count them twice",
            false,
            json!({ "cutexPath": root }),
        ),
        SourceError::OverlapsSource(id) => super::server::write_v2_error(
            stream,
            409,
            "Conflict",
            "usage_source_overlaps_source",
            "this directory overlaps another usage source",
            false,
            json!({ "sourceId": id }),
        ),
        SourceError::Conflict(message) => super::server::write_v2_error(
            stream, 409, "Conflict", "usage_source_exists", &message, false, json!({}),
        ),
        SourceError::Internal(message) => super::server::write_v2_error(
            stream, 500, "Internal Server Error", "usage_source_write_failed", &message, true, json!({}),
        ),
    };
    let find = |id: &str| usage::external::configured_sources().into_iter().find(|entry| entry.id == id);
    match (request.method.as_str(), route) {
        ("GET", UsageRoute::Sources) => {
            let sources = usage::external::configured_sources();
            write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "contractVersion": 2,
                    "sources": sources.iter().map(|entry| usage::external::source_json(&root, entry)).collect::<Vec<_>>(),
                    "defaults": usage::external::defaults_json(),
                    "kinds": usage::external::SourceKind::ALL.map(|kind| kind.key()),
                    "billing": usage::external::Billing::ALL.map(|billing| billing.key()),
                    "scanIntervalSecs": usage::external::SCAN_INTERVAL.as_secs(),
                }),
            )
        }
        ("POST", UsageRoute::Sources) => {
            let body: usage::external::NewSource = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(error) => {
                    return source_error(
                        stream,
                        SourceError::Invalid(format!("strict body {{kind, path, label?, billing?, enabled?, id?}} required: {error}")),
                    )
                }
            };
            match usage::external::add_source(body) {
                Ok(spec) => {
                    let _ = usage::external::request_scan(&spec.id);
                    let entry = find(&spec.id).unwrap_or_default();
                    write_json_response(stream, 201, "Created", &usage::external::source_json(&root, &entry))
                }
                Err(error) => source_error(stream, error),
            }
        }
        ("GET", UsageRoute::Source(id)) => match find(&id) {
            Some(entry) => write_json_response(stream, 200, "OK", &usage::external::source_json(&root, &entry)),
            None => source_error(stream, SourceError::NotFound),
        },
        ("PATCH", UsageRoute::Source(id)) => {
            let body: usage::external::SourcePatch = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(error) => {
                    return source_error(
                        stream,
                        SourceError::Invalid(format!("strict body {{path?, label?, billing?, enabled?}} required: {error}")),
                    )
                }
            };
            match usage::external::patch_source(&id, body) {
                Ok(spec) => {
                    if spec.enabled {
                        let _ = usage::external::request_scan(&spec.id);
                    }
                    let entry = find(&spec.id).unwrap_or_default();
                    write_json_response(stream, 200, "OK", &usage::external::source_json(&root, &entry))
                }
                Err(error) => source_error(stream, error),
            }
        }
        ("DELETE", UsageRoute::Source(id)) => {
            let purge = match purge_param(&request.path) {
                Ok(purge) => purge,
                Err(message) => return source_error(stream, SourceError::Invalid(message)),
            };
            match usage::external::remove_source(&root, &id, purge) {
                Ok(purged) => write_json_response(
                    stream,
                    200,
                    "OK",
                    &json!({ "id": id, "deleted": true, "purged": purged }),
                ),
                Err(error) => source_error(stream, error),
            }
        }
        ("POST", UsageRoute::SourceRescan(id)) => {
            let Some(entry) = find(&id) else {
                return source_error(stream, SourceError::NotFound);
            };
            if !entry.enabled {
                return super::server::write_v2_error(
                    stream,
                    409,
                    "Conflict",
                    "usage_source_disabled",
                    "this usage source is disabled",
                    false,
                    json!({}),
                );
            }
            let started = usage::external::request_scan(&id).unwrap_or(false);
            let mut resource = usage::external::source_json(&root, &entry);
            resource["started"] = json!(started);
            write_json_response(stream, 202, "Accepted", &resource)
        }
        _ => super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET or POST for /v2/usage/sources, GET, PATCH or DELETE for /v2/usage/sources/{id}, POST for /v2/usage/sources/{id}/rescan",
            false,
            json!({}),
        ),
    }
}

/// `?purge=true|false` (only parameter of DELETE /v2/usage/sources/{id}).
fn purge_param(full_path: &str) -> Result<bool, String> {
    let Some((_, query)) = full_path.split_once('?') else {
        return Ok(false);
    };
    let mut purge = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match (name.as_ref(), value.as_ref(), purge) {
            ("purge", "true", None) => purge = Some(true),
            ("purge", "false", None) => purge = Some(false),
            ("purge", _, None) => return Err("purge must be true or false".into()),
            ("purge", _, Some(_)) => return Err("query parameter purge given twice".into()),
            (other, _, _) => return Err(format!("unsupported query parameter: {other}")),
        }
    }
    Ok(purge.unwrap_or(false))
}

const LEGACY_CHANGED_WARNING: &str = "usage-prices.json was changed by an older cutex (or by hand) after the last save here; its table is used (aliases and per-model currencies of usage-prices-v2.json are ignored until the next save)";
const LIBRARY_MATCH_LIMIT: usize = 200;

/// `?library=<substring>`: search the cached library (read-only).
fn library_query(full_path: &str) -> Result<Option<String>, String> {
    let Some((_, query)) = full_path.split_once('?') else {
        return Ok(None);
    };
    let mut found = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match name.as_ref() {
            "library" if found.is_none() => {
                if value.len() > 160 || value.chars().any(char::is_control) {
                    return Err("library must be at most 160 characters".to_string());
                }
                found = Some(value.trim().to_ascii_lowercase());
            }
            "library" => return Err("query parameter library given twice".to_string()),
            other => return Err(format!("unsupported query parameter: {other}")),
        }
    }
    Ok(found)
}

fn current_rates() -> crate::config::usage_pricing::ExchangeRates {
    let settings = crate::config::usage_pricing::current();
    match usage::catalog::cache_dir() {
        Ok(dir) => usage::catalog::effective_rates(&dir, &settings).0,
        Err(_) => settings.exchange_rates,
    }
}

/// Effective provider category of every profile (library key prefixes).
fn profile_categories() -> HashMap<String, crate::profiles::provider_category::ProviderCategory> {
    crate::profiles::settings_api::load_store()
        .map(|store| {
            store
                .accounts
                .iter()
                .map(|account| {
                    (
                        account.name.clone(),
                        crate::profiles::settings_api::provider_category_of(account),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Usage totals of `profile_ids` over the last 1 / 7 / 30 local days,
/// priced as `GET /v2/usage` (the quota of a Claude API account: API usage
/// instead of 5h / 7d windows). `{today, last7Days, last30Days, currency}`.
pub(super) fn profile_usage_windows(profile_ids: &[String]) -> anyhow::Result<serde_json::Value> {
    let today = Local::now().date_naive();
    let first = today - Duration::days(29);
    let mut ledger = usage::load_usage_ledger_for_days(first, today)?;
    usage::canonicalize_ledger_profiles(&mut ledger, &crate::profiles::identity::ProfileNameDirectory::load());
    ledger.entries.retain(|entry| match entry {
        usage::UsageLedgerEntry::Usage(sample) => sample
            .profile_id
            .as_ref()
            .is_some_and(|id| profile_ids.iter().any(|wanted| wanted == id)),
        usage::UsageLedgerEntry::ResetBoundary(_) => false,
    });
    let prices = usage::load_prices().ok().map(|loaded| loaded.table);
    let pricing = usage::load_price_context(prices, profile_categories()).ok().map(|(context, _)| context);
    let labels = usage::UsageLabels::default();
    let window = |days: i64| {
        let query = usage::UsageQuery { from: today - Duration::days(days - 1), to: today, group_by: vec![UsageGroupKey::Profile] };
        usage::aggregate_usage(&ledger, &query, &labels, pricing.as_ref(), &local_date)
    };
    let (day, week, month) = (window(1), window(7), window(30));
    Ok(json!({
        "today": day.totals,
        "last7Days": week.totals,
        "last30Days": month.totals,
        "currency": month.currency,
        "from": month.from,
        "to": month.to,
    }))
}

fn body_has_model_map(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.as_object().map(|object| object.contains_key("modelMap")))
        .unwrap_or(false)
}

/// `GET|PUT /v2/usage/prices`, `POST /v2/usage/prices/refresh`: the
/// manual table (`currency`, `models`, as before plus per-model
/// `currency` / `cacheWriteInput` / `aliases`), the settings in effect and
/// the library cache state (`pricing`), optional library matches.
fn prices_resource(
    table: &usage::UsagePriceTable,
    origin: &str,
    library_query: Option<&str>,
    refresh_errors: &[String],
) -> serde_json::Value {
    let settings = crate::config::usage_pricing::current();
    let dir = usage::catalog::cache_dir().ok();
    let library = dir
        .as_deref()
        .filter(|_| settings.price_source != crate::config::usage_pricing::PriceSource::Manual)
        .and_then(|dir| {
            usage::catalog::load_catalog_cached(&usage::catalog::catalog_file(dir, settings.price_source))
        });
    let (rates, rates_source, fx) = match dir.as_deref() {
        Some(dir) => usage::catalog::effective_rates(dir, &settings),
        None => (settings.exchange_rates, "config", None),
    };
    let mut warnings = Vec::new();
    if origin == "legacy-changed" {
        warnings.push(LEGACY_CHANGED_WARNING.to_string());
    }
    let library_matches = library_query.map(|query| {
        let models = library.as_ref().map(|cache| &cache.models);
        let matches: serde_json::Map<String, serde_json::Value> = models
            .into_iter()
            .flatten()
            .filter(|(key, _)| key.contains(query))
            .take(LIBRARY_MATCH_LIMIT)
            .map(|(key, price)| (key.clone(), serde_json::to_value(price).unwrap_or_default()))
            .collect();
        let total = models
            .map(|models| models.keys().filter(|key| key.contains(query)).count())
            .unwrap_or(0);
        json!({
            "source": settings.price_source.key(),
            "currency": "USD",
            "query": query,
            "models": matches,
            "total": total,
            "truncated": total > LIBRARY_MATCH_LIMIT,
        })
    });
    json!({
        "contractVersion": 2,
        "currency": table.currency,
        "models": table.models,
        "modelMap": table.model_map,
        "currencies": crate::config::usage_pricing::Currency::ALL.map(|c| c.code()),
        "pricing": usage::estimate::pricing_info(&settings, library.as_deref(), &rates, rates_source, fx.as_ref()),
        "library": library_matches,
        "refreshErrors": refresh_errors,
        "warnings": warnings,
    })
}

fn snapshot_resource(snapshot: &usage::QuotaSnapshot) -> serde_json::Value {
    let date = DateTime::<Utc>::from_timestamp(snapshot.ts, 0)
        .map(|at| local_date(at).format("%Y-%m-%d").to_string());
    json!({
        "ts": snapshot.ts,
        "date": date,
        "profile": snapshot.profile,
        "profileId": snapshot.profile_id,
        "gaugeId": snapshot.gauge_id,
        "label": snapshot.label,
        "kind": snapshot.kind,
        "remainingPercent": snapshot.remaining_percent,
        "balance": snapshot.balance,
        "resetAt": snapshot.reset_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn usage_paths_are_exact() {
        assert_eq!(parse_path("/v2/usage"), Some(UsageRoute::Report));
        assert_eq!(parse_path("/v2/usage/prices"), Some(UsageRoute::Prices));
        assert_eq!(parse_path("/v2/usage/prices/refresh"), Some(UsageRoute::PricesRefresh));
        assert_eq!(library_query("/v2/usage/prices?library=DeepSeek"), Ok(Some("deepseek".into())));
        assert_eq!(library_query("/v2/usage/prices"), Ok(None));
        assert!(library_query("/v2/usage/prices?other=1").is_err());
        assert_eq!(
            parse_path("/v2/usage/quota-history"),
            Some(UsageRoute::QuotaHistory)
        );
        assert_eq!(parse_path("/v2/usage/"), None);
        assert_eq!(parse_path("/v2/usage/other"), None);
        assert_eq!(parse_path("/v2/usage/sources"), Some(UsageRoute::Sources));
        assert_eq!(parse_path("/v2/usage/sources/claude-code"), Some(UsageRoute::Source("claude-code".into())));
        assert_eq!(
            parse_path("/v2/usage/sources/codex-2/rescan"),
            Some(UsageRoute::SourceRescan("codex-2".into()))
        );
        for bad in ["/v2/usage/sources/", "/v2/usage/sources/..", "/v2/usage/sources/A", "/v2/usage/sources/a/b", "/v2/usage/sources/a/rescan/x"] {
            assert_eq!(parse_path(bad), None, "{bad}");
        }
        assert_eq!(purge_param("/v2/usage/sources/a"), Ok(false));
        assert_eq!(purge_param("/v2/usage/sources/a?purge=true"), Ok(true));
        assert!(purge_param("/v2/usage/sources/a?purge=1").is_err());
        assert!(purge_param("/v2/usage/sources/a?other=1").is_err());
        assert!(purge_param("/v2/usage/sources/a?purge=true&purge=true").is_err());
    }

    #[test]
    fn usage_source_filter_param() {
        let allowed = ["source"];
        for value in ["cutex", "external", "claude-code"] {
            let params = parse_params(&format!("/v2/usage?source={value}"), &allowed).unwrap();
            assert_eq!(params.source.as_deref(), Some(value));
        }
        assert!(parse_params("/v2/usage?source=Bad%20Id", &allowed).is_err());
        let groups = parse_params("/v2/usage?groupBy=day,source", &["groupBy"]).unwrap();
        assert_eq!(groups.group_by, Some(vec![UsageGroupKey::Day, UsageGroupKey::Source]));
    }

    #[test]
    fn usage_params_are_strict() {
        let params = parse_params(
            "/v2/usage?from=2026-10-01&to=2026-10-06&groupBy=day,profile",
            &["from", "to", "groupBy"],
        )
        .unwrap();
        assert_eq!(params.from, Some(d("2026-10-01")));
        assert_eq!(params.to, Some(d("2026-10-06")));
        assert_eq!(
            params.group_by,
            Some(vec![UsageGroupKey::Day, UsageGroupKey::Profile])
        );
        assert!(parse_params("/v2/usage?groupBy=", &["groupBy"]).is_err());
        assert!(parse_params("/v2/usage?groupBy=day,day", &["groupBy"]).is_err());
        assert!(parse_params("/v2/usage?groupBy=week", &["groupBy"]).is_err());
        assert!(parse_params("/v2/usage?from=2026-1-1", &["from"]).is_err());
        assert!(parse_params("/v2/usage?other=1", &["from"]).is_err());
        assert!(parse_params("/v2/usage?from=2026-10-01&from=2026-10-02", &["from"]).is_err());
        assert!(parse_params("/v2/usage?profile=GLM", &["from"]).is_err());
        assert_eq!(
            parse_params("/v2/usage/quota-history?profile=my%20glm", &["profile"])
                .unwrap()
                .profile
                .as_deref(),
            Some("my glm")
        );
    }

    #[test]
    fn usage_range_defaults_and_caps() {
        let today = d("2026-10-06");
        assert_eq!(
            resolve_range(None, None, today),
            Ok((d("2026-09-30"), today))
        );
        assert_eq!(
            resolve_range(Some(d("2026-10-01")), None, today),
            Ok((d("2026-10-01"), today))
        );
        assert_eq!(
            resolve_range(None, Some(d("2026-09-10")), today),
            Ok((d("2026-09-04"), d("2026-09-10")))
        );
        assert!(resolve_range(Some(d("2026-10-07")), Some(d("2026-10-06")), today).is_err());
        assert!(resolve_range(Some(d("2025-10-06")), Some(d("2026-10-06")), today).is_ok());
        assert!(resolve_range(Some(d("2025-10-05")), Some(d("2026-10-06")), today).is_err());
    }
}
