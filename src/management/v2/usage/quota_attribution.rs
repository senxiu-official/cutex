//! Who used a Claude account's quota window (cutex-desktop
//! docs/design/052-quota-attribution.md, capability `quota-attribution`):
//! `GET /v2/usage/quota-attribution?account=<id>|profile=<name>&window=5h|7d`.
//!
//! Computed on request only, never on a send / turn / event path. The
//! existing SQLite usage index selects own ledger samples in the bounded
//! lookback; committed JSONL payloads decode outside the usage writer lock.
//! Per-request 15-minute buckets are derived only; no new durable state,
//! migration, global cache or manual opt-in is introduced.
//!
//! Units approximate how Anthropic counts subscription quota with API price
//! ratios ([`units`]): fresh input 1, cache write 1.25, cache read 0.1,
//! output 5, times a model factor (opus 1, sonnet 0.6, haiku 0.2, other 0.6).
//! Anthropic publishes no formula, so each account is calibrated: the median
//! of (units in a completed window / that window's peak used %) over the
//! last 14 completed 5h windows (4 for 7d) that were read up to their reset
//! and did not hit 100 %. The coefficient is recomputed on every request.
//!
//! Limits the payload states (`coverage`): Claude usage is recorded when a
//! turn ends, so a running turn is not in the rows yet (`inFlight` lists
//! those agents); background subagents are counted in their parent agent,
//! with their part in `backgroundUnits` / `backgroundShare` for rows that
//! carry `backgroundTokens` (claude-r35+; older rows count 0 there); use of
//! the same account outside cutex shows as `unattributedPercent`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;

use chrono::DateTime;
use chrono::Datelike;
use chrono::Utc;
use serde::Serialize;

use super::model::parse_timestamp;
use super::model::TokenUsageBreakdown;
use super::model::UsageLedgerEntry;
use super::quota_history::QuotaSnapshot;
use super::store;

pub const FEATURE: &str = "quota-attribution";
pub const CONTRACT_VERSION: u32 = 1;

const BUCKET_SECS: i64 = 15 * 60;
/// Same freshness budget as profile_auto_pick.
const FRESH_SECS: i64 = 15 * 60;
/// Two readings belong to one window when their resets are this close.
const SAME_CYCLE_SECS: i64 = 10 * 60;
/// A completed window counts for calibration only if it was read this close to its reset.
const EDGE_SECS: i64 = 30 * 60;
/// Windows at or above this are cut off by the limit (lower bound only).
const TRUNCATED_PERCENT: f64 = 99.5;
/// Windows below this carry too little signal to calibrate on.
const MIN_CALIBRATION_PERCENT: f64 = 2.0;
/// Below this gap between observed and attributed use, `unattributed` is noise.
pub const TOLERANCE_PERCENT: f64 = 8.0;
const SLOPE_SECS: i64 = 60 * 60;
/// A turn started longer ago than this without an end is not reported as running.
const IN_FLIGHT_MAX_SECS: i64 = 6 * 3600;

pub const WEIGHT_INPUT: f64 = 1.0;
pub const WEIGHT_CACHE_WRITE: f64 = 1.25;
pub const WEIGHT_CACHE_READ: f64 = 0.1;
pub const WEIGHT_OUTPUT: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    FiveHour,
    SevenDay,
}

impl WindowKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "5h" => Some(Self::FiveHour),
            "7d" => Some(Self::SevenDay),
            _ => None,
        }
    }

    /// The Claude window caption (`profiles::claude_usage`).
    pub fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::SevenDay => "7d",
        }
    }

    pub fn secs(self) -> i64 {
        match self {
            Self::FiveHour => 5 * 3600,
            Self::SevenDay => 7 * 24 * 3600,
        }
    }

    fn lookback_secs(self) -> i64 {
        (self.calibration_windows() as i64 + 1) * self.secs() + 24 * 3600
    }

    fn calibration_windows(self) -> usize {
        match self {
            Self::FiveHour => 14,
            Self::SevenDay => 4,
        }
    }

    /// Gauge id when no live reading names it: `five_hour` and `seven_day`
    /// are the first two windows the usage endpoint returns.
    fn fallback_gauge_id(self) -> &'static str {
        match self {
            Self::FiveHour => "claude-0",
            Self::SevenDay => "claude-1",
        }
    }
}

/// Relative cost of a model's tokens against Opus.
pub fn model_factor(model: Option<&str>) -> f64 {
    let model = model.unwrap_or_default().to_ascii_lowercase();
    if model.contains("opus") {
        1.0
    } else if model.contains("haiku") {
        0.2
    } else {
        0.6
    }
}

/// Quota units of one ledger row (see the module docs).
pub fn units(tokens: &TokenUsageBreakdown, model: Option<&str>) -> f64 {
    let fresh_input = tokens
        .input_tokens
        .saturating_sub(tokens.cached_input_tokens)
        .saturating_sub(tokens.cache_write_input_tokens);
    model_factor(model)
        * (fresh_input as f64 * WEIGHT_INPUT
            + tokens.cache_write_input_tokens as f64 * WEIGHT_CACHE_WRITE
            + tokens.cached_input_tokens as f64 * WEIGHT_CACHE_READ
            + tokens.output_tokens as f64 * WEIGHT_OUTPUT)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Amount {
    pub units: f64,
    /// Part of `units` used by Claude background subagents (rows of claude-r35+).
    pub background_units: f64,
    pub requests: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub last_at: i64,
}

impl Amount {
    fn add(&mut self, other: &Amount) {
        self.units += other.units;
        self.background_units += other.background_units;
        self.requests += other.requests;
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.last_at = self.last_at.max(other.last_at);
    }
}

/// (slot start, profile key, cutex session id, model).
type BucketKey = (i64, String, String, String);

/// Profile key of a ledger row: its stable id, or `name:<name>` for rows
/// written before profile ids.
pub(super) fn profile_key(profile_id: Option<&str>, profile: Option<&str>) -> Option<String> {
    profile_id
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(|| {
            profile
                .filter(|name| !name.is_empty())
                .map(|name| format!("name:{name}"))
        })
}

/// Per-request derived buckets from the existing SQLite index.
#[derive(Debug, Default)]
pub(super) struct Index {
    #[cfg(test)]
    decoded_rows: u64,
    buckets: BTreeMap<BucketKey, Amount>,
    /// Entry id -> slot, so a row written twice counts once.
    seen: HashMap<String, i64>,
    ledger_through: Option<i64>,
}

impl Index {
    /// Fold one ledger row in. Only cutex's own usage rows count.
    pub(super) fn ingest(&mut self, entry: UsageLedgerEntry) {
        let UsageLedgerEntry::Usage(sample) = entry else {
            return;
        };
        if sample.source.is_some() {
            return;
        }
        let Some(profile) = profile_key(sample.profile_id.as_deref(), sample.profile.as_deref())
        else {
            return;
        };
        let Ok(at) = parse_timestamp(&sample.observed_at) else {
            return;
        };
        let at = at.timestamp();
        let slot = at.div_euclid(BUCKET_SECS) * BUCKET_SECS;
        if self.seen.insert(sample.entry_id.clone(), slot).is_some() {
            return;
        }
        let model = sample
            .attribution
            .pricing_model()
            .unwrap_or_default()
            .to_string();
        let tokens = &sample.tokens;
        let amount = Amount {
            units: units(tokens, Some(&model)),
            background_units: sample
                .background_tokens
                .as_ref()
                .map_or(0.0, |background| units(background, Some(&model))),
            requests: sample.model_requests.unwrap_or(1),
            input: tokens
                .input_tokens
                .saturating_sub(tokens.cached_input_tokens)
                .saturating_sub(tokens.cache_write_input_tokens),
            cache_read: tokens.cached_input_tokens,
            cache_write: tokens.cache_write_input_tokens,
            output: tokens.output_tokens,
            last_at: at,
        };
        self.buckets
            .entry((slot, profile, sample.cutex_session_id.clone(), model))
            .or_default()
            .add(&amount);
        self.ledger_through = Some(self.ledger_through.map_or(at, |through| through.max(at)));
    }

    fn prune(&mut self, before: i64) {
        let slot = before.div_euclid(BUCKET_SECS) * BUCKET_SECS;
        self.buckets = self
            .buckets
            .split_off(&(slot, String::new(), String::new(), String::new()));
        self.seen.retain(|_, kept| *kept >= slot);
    }

    /// Buckets of `profiles` with a slot in `[from, to)`.
    fn range<'a>(
        &'a self,
        profiles: &'a BTreeSet<String>,
        from: i64,
        to: i64,
    ) -> impl Iterator<Item = (&'a BucketKey, &'a Amount)> + 'a {
        let from = from.div_euclid(BUCKET_SECS) * BUCKET_SECS;
        self.buckets
            .range((from, String::new(), String::new(), String::new())..)
            .take_while(move |((slot, ..), _)| *slot < to)
            .filter(move |((_, profile, ..), _)| profiles.contains(profile))
    }

    fn units_between(&self, profiles: &BTreeSet<String>, from: i64, to: i64) -> f64 {
        self.range(profiles, from, to)
            .map(|(_, amount)| amount.units)
            .sum()
    }
}

fn month_stem(ts: i64) -> String {
    let at = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default();
    format!("{:04}-{:02}", at.year(), at.month())
}

// r290 P8: reuse the existing SQLite index; no parallel durable state.
pub(super) fn refresh_at(
    index: &mut Index,
    root: &Path,
    now: i64,
    kind: WindowKind,
    profiles: &BTreeSet<String>,
) -> anyhow::Result<Vec<String>> {
    let lookback = kind.lookback_secs();
    use super::index::SampleFilter;
    let first_month = month_stem(now - lookback);
    let last_month = month_stem(now);
    let mut next = Index::default();
    let filter = SampleFilter {
        from_ms: (now - lookback).saturating_mul(1000),
        to_ms: now.saturating_add(1).saturating_mul(1000),
        source: Some("cutex".to_string()),
        profile_ids: Some(
            profiles
                .iter()
                .filter(|key| !key.starts_with("name:"))
                .cloned()
                .collect(),
        ),
    };
    let (warnings, _stats) = store::read_usage_samples_at(
        root,
        (&first_month, &last_month),
        &Default::default(),
        &filter,
        |sample| {
            // NULL-ID legacy candidates need name resolution after decoding.
            if profile_key(sample.profile_id.as_deref(), sample.profile.as_deref())
                .is_some_and(|key| profiles.contains(&key))
            {
                next.ingest(UsageLedgerEntry::Usage(Box::new(sample)));
            }
            Ok(())
        },
    )?;
    #[cfg(test)]
    {
        next.decoded_rows = _stats.rows_decoded;
    }
    next.prune(now - lookback);
    *index = next;
    Ok(warnings)
}

/// One quota reading of the asked window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct HistoryPoint {
    pub ts: i64,
    pub used: f64,
    pub reset_at: Option<i64>,
}

/// The account's readings of one gauge, oldest first, one per timestamp
/// (profiles on the same account record the same reading).
pub(super) fn history_points(
    snapshots: &[QuotaSnapshot],
    profile_ids: &BTreeSet<String>,
    gauge_id: &str,
) -> Vec<HistoryPoint> {
    let mut points: Vec<HistoryPoint> = snapshots
        .iter()
        .filter(|snapshot| {
            snapshot.gauge_id == gauge_id && profile_ids.contains(&snapshot.profile_id)
        })
        .filter_map(|snapshot| {
            Some(HistoryPoint {
                ts: snapshot.ts,
                used: 100.0
                    - snapshot
                        .remaining_percent
                        .filter(|value| value.is_finite())?,
                reset_at: snapshot.reset_at,
            })
        })
        .collect();
    points.sort_by_key(|point| point.ts);
    points.dedup_by_key(|point| point.ts);
    points
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Calibration {
    /// Units per 1 % of the window; `None` = not enough completed windows.
    pub units_per_percent: Option<f64>,
    pub windows: usize,
    /// `high` (≥ 5 windows, interquartile range < half the median),
    /// `medium` (≥ 3) or `low`.
    pub confidence: &'static str,
}

fn median(sorted: &[f64]) -> f64 {
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn quartile(sorted: &[f64], q: f64) -> f64 {
    let position = q * (sorted.len() - 1) as f64;
    let (low, high) = (position.floor() as usize, position.ceil() as usize);
    sorted[low] + (sorted[high] - sorted[low]) * (position - low as f64)
}

/// Completed windows: `(reset, peak used, last reading)`, newest first.
fn completed_windows(history: &[HistoryPoint], now: i64) -> Vec<(i64, f64, i64)> {
    let mut by_reset: Vec<(i64, f64, i64)> = Vec::new();
    let mut points: Vec<&HistoryPoint> = history
        .iter()
        .filter(|point| point.reset_at.is_some())
        .collect();
    points.sort_by_key(|point| point.reset_at);
    for point in points {
        let reset = point.reset_at.unwrap_or_default();
        match by_reset.last_mut() {
            Some(window) if reset - window.0 <= SAME_CYCLE_SECS => {
                window.0 = window.0.max(reset);
                window.1 = window.1.max(point.used);
                window.2 = window.2.max(point.ts);
            }
            _ => by_reset.push((reset, point.used, point.ts)),
        }
    }
    by_reset.retain(|(reset, ..)| *reset <= now);
    by_reset.reverse();
    by_reset
}

pub(super) fn calibrate(
    index: &Index,
    profiles: &BTreeSet<String>,
    kind: WindowKind,
    history: &[HistoryPoint],
    now: i64,
) -> Calibration {
    let mut ratios: Vec<f64> = completed_windows(history, now)
        .into_iter()
        .filter(|(reset, peak, last)| {
            reset - last <= EDGE_SECS
                && *peak < TRUNCATED_PERCENT
                && *peak >= MIN_CALIBRATION_PERCENT
        })
        .filter_map(|(reset, peak, _)| {
            let units = index.units_between(profiles, reset - kind.secs(), reset);
            (units > 0.0).then_some(units / peak)
        })
        .take(kind.calibration_windows())
        .collect();
    ratios.sort_by(f64::total_cmp);
    if ratios.is_empty() {
        return Calibration {
            units_per_percent: None,
            windows: 0,
            confidence: "low",
        };
    }
    let middle = median(&ratios);
    let spread = quartile(&ratios, 0.75) - quartile(&ratios, 0.25);
    let confidence = if ratios.len() >= 5 && spread < middle * 0.5 {
        "high"
    } else if ratios.len() >= 3 {
        "medium"
    } else {
        "low"
    };
    Calibration {
        units_per_percent: Some(middle),
        windows: ratios.len(),
        confidence,
    }
}

/// The asked window as last read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WindowNow {
    pub used: f64,
    pub reset_at: Option<i64>,
    pub observed_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Forecast {
    /// `exhausted` | `runsOut` (before the reset) | `lasts` | `unknown`.
    pub state: &'static str,
    pub percent_per_hour: Option<f64>,
    pub runs_out_at: Option<i64>,
    pub basis: &'static str,
}

pub(super) fn forecast(window: &WindowNow, history: &[HistoryPoint], now: i64) -> Forecast {
    let mut forecast = Forecast {
        state: "unknown",
        percent_per_hour: None,
        runs_out_at: None,
        basis: "history60m",
    };
    if window.observed_at > now
        || now - window.observed_at > FRESH_SECS
        || window.reset_at.is_some_and(|reset| reset <= now)
    {
        return forecast;
    }
    if window.used >= TRUNCATED_PERCENT {
        forecast.state = "exhausted";
        return forecast;
    }
    let same_cycle = |reset: Option<i64>| match (reset, window.reset_at) {
        (Some(a), Some(b)) => (a - b).abs() <= SAME_CYCLE_SECS,
        _ => false,
    };
    let mut points: Vec<(i64, f64)> = history
        .iter()
        .filter(|point| {
            point.ts >= window.observed_at - SLOPE_SECS && point.ts < window.observed_at
        })
        .filter(|point| same_cycle(point.reset_at))
        .map(|point| (point.ts, 100.0 - point.used))
        .collect();
    points.push((window.observed_at, 100.0 - window.used));
    let Some(rate) = crate::management::v2::profile_auto_pick::slope(&points) else {
        return forecast;
    };
    forecast.percent_per_hour = Some(round1(rate));
    if rate <= 0.0 {
        forecast.state = "lasts";
        return forecast;
    }
    let runs_out = window.observed_at + ((100.0 - window.used) / rate * 3600.0) as i64;
    if runs_out <= now {
        forecast.percent_per_hour = None;
        return forecast;
    }
    let before_reset = window.reset_at.is_none_or(|reset| runs_out < reset);
    forecast.state = if before_reset { "runsOut" } else { "lasts" };
    forecast.runs_out_at = before_reset.then_some(runs_out);
    forecast
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub cutex_session_id: String,
    pub agent_name: Option<String>,
    pub session_deleted: bool,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub units: f64,
    /// Share of the attributed units, percent.
    pub share: f64,
    /// Estimated share of the window, percent (`None` without calibration).
    pub percent: Option<f64>,
    /// Units of this agent's background subagents (part of `units`).
    pub background_units: f64,
    /// `background_units` / `units`, percent.
    pub background_share: f64,
    pub requests: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub last_at: i64,
    pub running: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InFlight {
    pub cutex_session_id: String,
    pub agent_name: Option<String>,
    pub started_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowView {
    pub label: &'static str,
    pub start_at: i64,
    pub reset_at: Option<i64>,
    pub used_percent: Option<f64>,
    pub observed_at: Option<i64>,
}

/// Everything about the account a computation needs besides the buckets.
pub(super) struct Inputs<'a> {
    pub kind: WindowKind,
    pub now: i64,
    pub profiles: &'a BTreeSet<String>,
    pub window: Option<WindowNow>,
    pub history: &'a [HistoryPoint],
    /// Running sessions of the account: `(cutexSessionId, startedAt)`.
    pub in_flight: &'a [(String, i64)],
    /// cutexSessionId -> display name (`None` = session gone).
    pub agent_name: &'a dyn Fn(&str) -> Option<String>,
    /// Profile key -> current profile name.
    pub profile_name: &'a dyn Fn(&str) -> Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attribution {
    pub window: WindowView,
    pub forecast: Forecast,
    pub calibration: Calibration,
    pub rows: Vec<Row>,
    pub total_units: f64,
    pub attributed_percent: Option<f64>,
    /// Observed use minus attributed use, ≥ 0 (`None` without calibration
    /// or reading). Below [`TOLERANCE_PERCENT`] it is noise.
    pub unattributed_percent: Option<f64>,
    pub in_flight: Vec<InFlight>,
    pub ledger_through: Option<i64>,
}

pub(super) fn compute(index: &Index, inputs: &Inputs<'_>) -> Attribution {
    let kind = inputs.kind;
    let window = inputs.window.filter(|w| {
        w.observed_at <= inputs.now && w.reset_at.is_none_or(|reset| reset > inputs.now)
    });
    let start_at = window
        .and_then(|window| window.reset_at)
        .map_or(inputs.now - kind.secs(), |reset| reset - kind.secs());
    let calibration = calibrate(index, inputs.profiles, kind, inputs.history, inputs.now);
    let mut grouped: BTreeMap<(String, String), (Amount, String)> = BTreeMap::new();
    for ((_, profile, session, model), amount) in
        index.range(inputs.profiles, start_at, inputs.now + 1)
    {
        let row = grouped
            .entry((session.clone(), model.clone()))
            .or_insert_with(|| (Amount::default(), profile.clone()));
        row.0.add(amount);
    }
    let total_units: f64 = grouped.values().map(|(amount, _)| amount.units).sum();
    let running: BTreeSet<&str> = inputs
        .in_flight
        .iter()
        .map(|(session, _)| session.as_str())
        .collect();
    let to_percent = |units: f64| {
        window
            .and(calibration.units_per_percent)
            .map(|k| round1(units / k))
    };
    let mut rows: Vec<Row> = grouped
        .into_iter()
        .map(|((session, model), (amount, profile))| {
            let agent_name = (inputs.agent_name)(&session);
            Row {
                running: running.contains(session.as_str()),
                session_deleted: agent_name.is_none(),
                agent_name,
                profile: (inputs.profile_name)(&profile),
                model: Some(model).filter(|model| !model.is_empty()),
                share: if total_units > 0.0 {
                    round1(amount.units / total_units * 100.0)
                } else {
                    0.0
                },
                percent: to_percent(amount.units),
                background_units: amount.background_units.round(),
                background_share: if amount.units > 0.0 {
                    round1(amount.background_units / amount.units * 100.0)
                } else {
                    0.0
                },
                units: amount.units.round(),
                requests: amount.requests,
                input_tokens: amount.input,
                cache_read_tokens: amount.cache_read,
                cache_write_tokens: amount.cache_write,
                output_tokens: amount.output,
                last_at: amount.last_at,
                cutex_session_id: session,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.units
            .total_cmp(&a.units)
            .then_with(|| a.cutex_session_id.cmp(&b.cutex_session_id))
    });
    let attributed_percent = to_percent(total_units);
    let unattributed_percent = match (attributed_percent, window) {
        (Some(attributed), Some(window)) => Some(round1((window.used - attributed).max(0.0))),
        _ => None,
    };
    let in_flight = inputs
        .in_flight
        .iter()
        .map(|(session, started_at)| InFlight {
            cutex_session_id: session.clone(),
            agent_name: (inputs.agent_name)(session),
            started_at: *started_at,
        })
        .collect();
    Attribution {
        window: WindowView {
            label: kind.label(),
            start_at,
            reset_at: window.and_then(|window| window.reset_at),
            used_percent: window.map(|window| window.used),
            observed_at: window.map(|window| window.observed_at),
        },
        forecast: window.map_or(
            Forecast {
                state: "unknown",
                percent_per_hour: None,
                runs_out_at: None,
                basis: "history60m",
            },
            |window| forecast(&window, inputs.history, inputs.now),
        ),
        calibration,
        rows,
        total_units: total_units.round(),
        attributed_percent,
        unattributed_percent,
        in_flight,
        ledger_through: index.ledger_through,
    }
}

// ------------------------------------------------------------------- live

/// What the request names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Account(String),
    Profile(String),
}

/// A refused request: HTTP status, error code, message.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

fn refuse(status: u16, code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        code,
        message: message.into(),
    }
}

fn rfc3339_secs(value: Option<&str>) -> Option<i64> {
    value
        .and_then(|value| parse_timestamp(value).ok())
        .map(|at| at.timestamp())
}

/// `GET /v2/usage/quota-attribution` body.
// Current-session discovery is paged; historical display names are point reads.
// No full session-store snapshot is built or retained by attribution.
fn in_flight_at(
    sessions_path: &Path,
    activity_root: &Path,
    account_profiles: &BTreeSet<String>,
    now: i64,
) -> anyhow::Result<Vec<(String, i64)>> {
    let repository = crate::management::v2::activity::activity_repository(activity_root)?;
    let mut cursor = None;
    let mut running = Vec::new();
    loop {
        let Some(page) = crate::session::store::session_page_at(
            sessions_path,
            false,
            cursor.as_deref(),
            crate::store::PageBudget::LIST_DEFAULT,
        )?
        else {
            break;
        };
        for record in page.rows {
            if !record
                .profile
                .as_ref()
                .is_some_and(|p| account_profiles.contains(p))
            {
                continue;
            }
            let Some(state) = repository.get(&record.cutex_session_id)? else {
                continue;
            };
            let started = rfc3339_secs(state.last_turn_started_at.as_deref());
            let completed = rfc3339_secs(state.last_turn_completed_at.as_deref());
            if let Some(started) = started.filter(|start| {
                completed.is_none_or(|done| done < *start)
                    && (0..=IN_FLIGHT_MAX_SECS).contains(&(now - start))
            }) {
                running.push((record.cutex_session_id.clone(), started));
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    running.sort();
    Ok(running)
}

fn session_labels_at(
    sessions_path: &Path,
    ids: &BTreeSet<String>,
) -> anyhow::Result<HashMap<String, Option<String>>> {
    ids.iter()
        .map(|id| {
            let name = crate::session::store::shared_session_row_at(sessions_path, id)?
                .map(|record| crate::session::service::cutex_session_display_name(&record));
            Ok((id.clone(), name))
        })
        .collect()
}

pub fn live(selector: &Selector, kind: WindowKind) -> Result<serde_json::Value, Refusal> {
    use crate::management::v2::profile_quota_claude as claude_quota;
    use crate::profiles::model::CliKind;
    use crate::runtime::claude_accounts::{self, ClaudeAccountKind, DEFAULT_ACCOUNT_ID};

    let unavailable =
        |error: anyhow::Error| refuse(503, "usage_store_unavailable", format!("{error:#}"));
    let store = crate::profiles::settings_api::load_store().map_err(unavailable)?;
    let account = match selector {
        Selector::Profile(name) => {
            let profile = crate::profiles::settings_api::find(&store, name)
                .ok_or_else(|| refuse(404, "profile_not_found", format!("no profile '{name}'")))?;
            if profile.cli_kind != CliKind::Claude {
                return Err(refuse(
                    409,
                    "not_a_claude_profile",
                    format!("profile '{name}' is not a Claude profile"),
                ));
            }
            claude_quota::profile_account(profile)
        }
        Selector::Account(id) => id.clone(),
    };
    let registry = claude_accounts::load_cached();
    let entry = registry.find(&account);
    let profiles: Vec<_> = store
        .accounts
        .iter()
        .filter(|profile| {
            profile.cli_kind == CliKind::Claude && claude_quota::profile_account(profile) == account
        })
        .collect();
    if entry.is_none() && account != DEFAULT_ACCOUNT_ID && profiles.is_empty() {
        return Err(refuse(
            404,
            "claude_account_not_found",
            format!("no Claude account '{account}'"),
        ));
    }
    if entry.is_some_and(|entry| entry.kind == ClaudeAccountKind::Api) {
        return Err(refuse(
            409,
            "no_quota_windows",
            format!("Claude account '{account}' is an API account: it has no quota windows"),
        ));
    }
    let profile_ids: BTreeSet<String> = profiles.iter().map(|profile| profile.id.clone()).collect();
    let mut keys = profile_ids.clone();
    keys.extend(
        profiles
            .iter()
            .map(|profile| format!("name:{}", profile.name)),
    );
    let names: HashMap<String, String> = profiles
        .iter()
        .flat_map(|profile| {
            [
                (profile.id.clone(), profile.name.clone()),
                (format!("name:{}", profile.name), profile.name.clone()),
            ]
        })
        .collect();

    let now = Utc::now().timestamp();
    let snapshot = crate::management::v2::profile_quota_native::store_dir()
        .ok()
        .and_then(|dir| claude_quota::load_at(&dir, &account));
    let mut warnings = Vec::new();
    let (gauge_id, window) = snapshot_window(snapshot.as_ref(), kind, now, &mut warnings);
    let lookback = kind.lookback_secs();
    let snapshots = super::read_quota_history_since(now - lookback).map_err(unavailable)?;
    let history = history_points(&snapshots, &profile_ids, &gauge_id);

    let root = crate::config::paths::runtime_dir()
        .map_err(unavailable)?
        .join("management-v2");
    let sessions_path = crate::session::store::cutex_sessions_path().map_err(unavailable)?;
    let session_profiles: BTreeSet<_> = profiles
        .iter()
        .flat_map(|profile| [profile.id.clone(), profile.name.clone()])
        .collect();
    let in_flight = match in_flight_at(&sessions_path, &root, &session_profiles, now) {
        Ok(running) => running,
        Err(error) => {
            warnings.push(format!("running agents unknown: {error:#}"));
            Vec::new()
        }
    };
    let mut index = Index::default();
    warnings.extend(refresh_at(&mut index, &root, now, kind, &keys).map_err(unavailable)?);
    let start_at = window
        .and_then(|window| window.reset_at)
        .map_or(now - kind.secs(), |reset| reset - kind.secs());
    let mut ids: BTreeSet<String> = index
        .range(&keys, start_at, now + 1)
        .map(|((_, _, session, _), _)| session.clone())
        .collect();
    ids.extend(in_flight.iter().map(|(id, _)| id.clone()));
    let labels = session_labels_at(&sessions_path, &ids).map_err(unavailable)?;
    let agent_name = |id: &str| labels.get(id).cloned().flatten();
    let profile_name = |key: &str| {
        names
            .get(key)
            .cloned()
            .or_else(|| key.strip_prefix("name:").map(str::to_string))
    };
    let attribution = compute(
        &index,
        &Inputs {
            kind,
            now,
            profiles: &keys,
            window,
            history: &history,
            in_flight: &in_flight,
            agent_name: &agent_name,
            profile_name: &profile_name,
        },
    );
    warnings.truncate(20);
    let label = entry
        .map(|entry| entry.label.clone())
        .or_else(|| profiles.first().map(|profile| profile.name.clone()));
    Ok(response(&account, label, attribution, warnings))
}

pub(super) fn snapshot_window(
    snapshot: Option<&crate::management::v2::profile_quota_claude::ClaudeQuotaSnapshot>,
    kind: WindowKind,
    now: i64,
    warnings: &mut Vec<String>,
) -> (String, Option<WindowNow>) {
    let fallback = kind.fallback_gauge_id().to_string();
    let Some(snapshot) = snapshot else {
        return (fallback, None);
    };
    let Some((position, reading)) = snapshot
        .windows
        .iter()
        .enumerate()
        .find(|(_, w)| w.label == kind.label())
    else {
        return (fallback, None);
    };
    let gauge = format!("claude-{position}");
    let Some(observed_at) = snapshot.fetched_at else {
        return (gauge, None);
    };
    if snapshot.last_error.is_some() || snapshot.credentials_expired_at.is_some() {
        warnings
            .push("quota reading unavailable after a failed refresh or expired credentials".into());
        return (gauge, None);
    }
    if reading.reset_at.is_some_and(|reset| reset <= now) || observed_at > now {
        warnings
            .push("quota reading belongs to an expired window or has a future timestamp".into());
        return (gauge, None);
    }
    if now - observed_at > FRESH_SECS {
        warnings.push("quota reading is older than 15 minutes; values are as of observedAt and forecast is unknown".into());
    }
    (
        gauge,
        Some(WindowNow {
            used: reading.used_percent,
            reset_at: reading.reset_at,
            observed_at,
        }),
    )
}

pub(super) fn response(
    account: &str,
    label: Option<String>,
    attribution: Attribution,
    warnings: Vec<String>,
) -> serde_json::Value {
    let mut body = serde_json::to_value(&attribution).unwrap_or_default();
    body["contractVersion"] = CONTRACT_VERSION.into();
    body["account"] = serde_json::json!({"id": account, "label": label});
    body["tolerancePercent"] = TOLERANCE_PERCENT.into();
    body["weights"] = serde_json::json!({
        "input": WEIGHT_INPUT,
        "cacheWrite": WEIGHT_CACHE_WRITE,
        "cacheRead": WEIGHT_CACHE_READ,
        "output": WEIGHT_OUTPUT,
        "models": {"opus": 1.0, "sonnet": 0.6, "haiku": 0.2, "other": 0.6},
    });
    body["coverage"] = serde_json::json!({
        "ledgerThrough": attribution.ledger_through,
        "accounting": "turnEnd",
        "backgroundSubagents": "inParentWithShare",
        "warnings": warnings,
    });
    if let Some(object) = body.as_object_mut() {
        object.remove("ledgerThrough");
    }
    body
}

#[cfg(test)]
#[path = "quota_attribution_tests.rs"]
mod tests;
