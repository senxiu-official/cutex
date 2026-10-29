//! Per-provider automatic profile pick (design 041, `profile-auto-pick`).
//!
//! `profile: "auto:<providerFamily>"` in `agent_create`, `member_replace`
//! (successor) and `POST /v2/human/agents` picks, when the agent is created,
//! one of this host's profiles of that provider family whose owner turned on
//! 「参与自动选择」 (`StoredAccount::auto_pick`, default off). The pick never
//! crosses providers and never changes the profile of a running
//! conversation: the concrete profile name is what the agent record and the
//! action record store, so a retry replays it and an older release sees an
//! ordinary profile. Bare `auto` is refused (`provider_required`).
//!
//! Score = hours until the first window runs out at the current spend plus
//! the new agent's estimated spend (a window that resets first counts one
//! more full window), capped at 24 h. Profiles under 1 h are skipped; when
//! every one is under 1 h the soonest reset wins. Scores within 10 % of the
//! best tie: fewer open agents, then the name, win.
//!
//! The explanation (`profilePick`) is part of the Human create response and,
//! for sparse agent management, is kept in
//! `<runtime>/profile-auto-pick/<action>.json` (no older release reads it)
//! and attached to the reply as `result.profile_pick`.
//! `GET /v2/profile-auto-pick?provider=&model=&effort=` previews the ranking
//! without creating anything.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::profiles::model::StoredAccount;

pub const PATH: &str = "/v2/profile-auto-pick";
const PICK_DIR: &str = "profile-auto-pick";
/// Below this many hours to the wall a profile is "about to hit its limit".
const MIN_HOURS: f64 = 1.0;
const CAP_HOURS: f64 = 24.0;
const TIE_SHARE: f64 = 0.10;
/// A gauge read older than this does not count.
const FRESH_SECS: i64 = 15 * 60;
/// The spend slope looks this far back (same reset cycle only).
const SLOPE_WINDOW_SECS: i64 = 60 * 60;
const SLOPE_MIN_SPAN_SECS: i64 = 10 * 60;
/// Two readings belong to the same reset cycle when their resets are this close.
const SAME_CYCLE_SECS: i64 = 10 * 60;
const KEEP_PICK_FILES_SECS: u64 = 90 * 24 * 60 * 60;

/// What a `profile` value asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requested {
    /// An ordinary profile name or ID.
    Profile,
    /// `auto` without a provider (refused).
    Bare,
    /// `auto:<providerFamily>`, canonical family key.
    Provider(String),
}

pub fn requested(profile: &str) -> Requested {
    let profile = profile.trim();
    if profile.eq_ignore_ascii_case("auto") {
        return Requested::Bare;
    }
    match profile.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("auto:") => {
            Requested::Provider(canonical_provider(profile[5..].trim()))
        }
        _ => Requested::Profile,
    }
}

/// `providerFamily` keys (`profiles::providers`) plus a few everyday names.
fn canonical_provider(value: &str) -> String {
    let value = value.to_ascii_lowercase();
    match value.as_str() {
        "claude" => "anthropic".to_string(),
        "chatgpt" | "codex" => "openai-chatgpt".to_string(),
        "glm" | "zai" | "z.ai" => "zhipu".to_string(),
        _ => value,
    }
}

/// A refused pick; `code` is the error code callers report.
#[derive(Debug, Clone, PartialEq)]
pub struct PickRefusal {
    pub code: &'static str,
    pub detail: String,
}

fn refusal(code: &'static str, detail: impl Into<String>) -> PickRefusal {
    PickRefusal { code, detail: detail.into() }
}

/// The new agent, for its estimated spend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spend {
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Spend {
    pub fn new(model: Option<&str>, effort: Option<&str>) -> Self {
        let given = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "default")
                .map(str::to_string)
        };
        Self { model: given(model), effort: given(effort) }
    }

    /// Estimated share of a window (percent) one agent uses per hour.
    /// First version: a fixed table (design 041 §2), a large model at high
    /// effort = 12 %/h of a 5 h window and 1 %/h of a week.
    pub fn percent_per_hour(&self, window_hours: Option<f64>) -> f64 {
        let base = match window_hours {
            Some(hours) if hours > 8.0 * 24.0 => 0.3,
            Some(hours) if hours > 24.0 => 1.0,
            _ => 12.0,
        };
        let model = self.model.as_deref().unwrap_or_default().to_ascii_lowercase();
        let model_factor = if model.contains("opus") {
            1.0
        } else if ["haiku", "mini", "flash", "air", "nano"].iter().any(|small| model.contains(small)) {
            0.25
        } else {
            0.6
        };
        let effort_factor = match self.effort.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("none" | "minimal" | "low") => 0.5,
            Some("medium") => 0.75,
            Some("xhigh" | "max") => 1.3,
            _ => 1.0,
        };
        base * model_factor * effort_factor
    }
}

/// Window length in hours from its caption (`5h`, `7d`, `week`, `day`, …).
pub fn window_hours(label: Option<&str>) -> Option<f64> {
    let label = label?.trim().to_ascii_lowercase();
    match label.as_str() {
        "day" | "daily" => return Some(24.0),
        "week" | "weekly" => return Some(7.0 * 24.0),
        "month" | "monthly" => return Some(30.0 * 24.0),
        _ => {}
    }
    let (number, unit) = label.split_at(label.find(|c: char| !c.is_ascii_digit())?);
    let number = number.parse::<f64>().ok()?;
    match unit {
        "m" | "min" => Some(number / 60.0),
        "h" => Some(number),
        "d" => Some(number * 24.0),
        "w" => Some(number * 7.0 * 24.0),
        _ => None,
    }
}

/// One subscription window of a candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowReading {
    pub gauge_id: String,
    pub label: Option<String>,
    pub remaining_percent: f64,
    pub reset_at: Option<i64>,
    /// Spend of the profile's running work, percent per hour, from the quota
    /// history; `None` = not enough points.
    pub burn_per_hour: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub profile_id: String,
    pub profile: String,
    pub windows: Vec<WindowReading>,
    pub active_agents: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoredCandidate {
    pub profile: String,
    pub profile_id: String,
    /// Hours until the first window hits its limit (capped at 24).
    pub hours: f64,
    pub binding_window: Option<String>,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<i64>,
    pub active_agents: usize,
    pub burn_per_hour: Option<f64>,
    /// `history` (slope of the last hour) or `agents` (open agents × the
    /// estimate).
    pub burn_source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Excluded {
    pub profile: String,
    /// `not_opted_in` | `requires_approval` | `no_subscription_windows` |
    /// `quota_stale` | `quota_read_failed`.
    pub reason: String,
}

/// `profilePick`: who was picked and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePick {
    pub requested: String,
    pub provider: String,
    pub picked: String,
    pub picked_id: String,
    /// `latest_wall` | `tie_fewer_agents` | `tie_name` | `only_candidate` |
    /// `all_near_wall_soonest_reset`.
    pub rule: String,
    pub reason: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub candidates: Vec<ScoredCandidate>,
    pub excluded: Vec<Excluded>,
    /// RFC 3339.
    pub at: String,
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Score one candidate at `now` (unix seconds).
pub fn score(candidate: &Candidate, spend: &Spend, now: i64) -> ScoredCandidate {
    let mut best: Option<(f64, &WindowReading, f64)> = None;
    let mut history = true;
    for window in &candidate.windows {
        let hours = window_hours(window.label.as_deref());
        let estimate = spend.percent_per_hour(hours);
        let burn = match window.burn_per_hour {
            Some(burn) => burn.max(0.0),
            None => {
                history = false;
                candidate.active_agents as f64 * estimate
            }
        };
        let rate = burn + estimate;
        let remaining = window.remaining_percent.clamp(0.0, 100.0);
        let until_empty = remaining / rate;
        let until_reset = window
            .reset_at
            .map(|reset| (reset - now) as f64 / 3600.0)
            .filter(|hours| *hours > 0.0);
        let wall = match until_reset {
            Some(reset) if until_empty >= reset => reset + 100.0 / rate,
            _ => until_empty,
        };
        if best.is_none_or(|(hours, _, _)| wall < hours) {
            best = Some((wall, window, burn));
        }
    }
    let (hours, window, burn) = match best {
        Some((hours, window, burn)) => (hours.min(CAP_HOURS), Some(window), Some(burn)),
        None => (0.0, None, None),
    };
    ScoredCandidate {
        profile: candidate.profile.clone(),
        profile_id: candidate.profile_id.clone(),
        hours: round1(hours),
        binding_window: window.map(|window| window.label.clone().unwrap_or_else(|| window.gauge_id.clone())),
        remaining_percent: window.map(|window| window.remaining_percent),
        reset_at: window.and_then(|window| window.reset_at),
        active_agents: candidate.active_agents,
        burn_per_hour: burn.map(round1),
        burn_source: if history { "history" } else { "agents" }.to_string(),
    }
}

/// Pick among scored candidates: `(index, rule)`. `None` when empty.
pub fn choose(scored: &[ScoredCandidate]) -> Option<(usize, &'static str)> {
    if scored.is_empty() {
        return None;
    }
    if scored.len() == 1 {
        return Some((0, "only_candidate"));
    }
    let eligible: Vec<usize> = (0..scored.len()).filter(|&index| scored[index].hours >= MIN_HOURS).collect();
    if eligible.is_empty() {
        let index = (0..scored.len())
            .min_by(|&a, &b| {
                let reset = |index: usize| scored[index].reset_at.unwrap_or(i64::MAX);
                reset(a).cmp(&reset(b)).then_with(|| scored[a].profile.cmp(&scored[b].profile))
            })
            .expect("non-empty");
        return Some((index, "all_near_wall_soonest_reset"));
    }
    let best = eligible.iter().map(|&index| scored[index].hours).fold(f64::MIN, f64::max);
    let tied: Vec<usize> =
        eligible.into_iter().filter(|&index| scored[index].hours >= best * (1.0 - TIE_SHARE)).collect();
    let index = *tied
        .iter()
        .min_by(|&&a, &&b| {
            scored[a]
                .active_agents
                .cmp(&scored[b].active_agents)
                .then_with(|| scored[a].profile.cmp(&scored[b].profile))
        })
        .expect("non-empty");
    let rule = if tied.len() == 1 {
        "latest_wall"
    } else if tied.iter().any(|&other| scored[other].active_agents != scored[index].active_agents) {
        "tie_fewer_agents"
    } else {
        "tie_name"
    };
    Some((index, rule))
}

fn reason_text(picked: &ScoredCandidate, rule: &str, count: usize) -> String {
    let window = picked.binding_window.as_deref().unwrap_or("quota");
    let left = picked
        .remaining_percent
        .map(|percent| format!("{window} window {percent:.0}% left"))
        .unwrap_or_else(|| format!("{window} window"));
    let why = match rule {
        "only_candidate" => "the only candidate".to_string(),
        "all_near_wall_soonest_reset" => {
            format!("every one of {count} candidates is under 1 h to its limit; this one resets first")
        }
        "tie_fewer_agents" => format!("tied with another of {count} candidates; fewer open agents"),
        "tie_name" => format!("tied with another of {count} candidates; first by name"),
        _ => format!("latest to hit a limit of {count} candidates"),
    };
    format!("{}: {left}, about {:.1} h to its limit ({why})", picked.profile, picked.hours)
}

/// Rank `candidates` and build the explanation.
pub fn pick_from(
    requested: &str,
    provider: &str,
    spend: &Spend,
    candidates: &[Candidate],
    excluded: Vec<Excluded>,
    now: i64,
) -> Result<ProfilePick, PickRefusal> {
    let mut scored: Vec<ScoredCandidate> = candidates.iter().map(|candidate| score(candidate, spend, now)).collect();
    scored.sort_by(|a, b| b.hours.total_cmp(&a.hours).then_with(|| a.profile.cmp(&b.profile)));
    let Some((index, rule)) = choose(&scored) else {
        let why = if excluded.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                excluded.iter().map(|item| format!("{}: {}", item.profile, item.reason)).collect::<Vec<_>>().join(", ")
            )
        };
        return Err(refusal(
            "no_auto_pick_candidate",
            format!(
                "no '{provider}' profile can be picked automatically{why}; turn on 「参与自动选择」 for one, or pass a profile name"
            ),
        ));
    };
    let picked = scored[index].clone();
    Ok(ProfilePick {
        requested: requested.trim().to_string(),
        provider: provider.to_string(),
        reason: reason_text(&picked, rule, scored.len()),
        picked: picked.profile,
        picked_id: picked.profile_id,
        rule: rule.to_string(),
        model: spend.model.clone(),
        effort: spend.effort.clone(),
        candidates: scored,
        excluded,
        at: chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0).unwrap_or_default().to_rfc3339(),
    })
}

/// Spend per hour from history points `(ts, remainingPercent)` of one window
/// in the current reset cycle, newest last. `None` = not enough points.
pub fn slope(points: &[(i64, f64)]) -> Option<f64> {
    let (first, last) = (points.first()?, points.last()?);
    let span = last.0 - first.0;
    (span >= SLOPE_MIN_SPAN_SECS).then(|| ((first.1 - last.1) / (span as f64 / 3600.0)).max(0.0))
}

// ------------------------------------------------------------------ live

/// How a `profile` value resolves on this host.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Look the name up as before.
    Profile,
    Picked(ProfilePick),
}

/// `refresh_stale`: re-read an opted-in profile whose last gauge read is
/// older than 15 minutes (creation does; the preview does not).
pub fn resolve_live(profile: &str, spend: &Spend, refresh_stale: bool) -> Result<Resolution, PickRefusal> {
    let kind = requested(profile);
    if kind == Requested::Profile {
        return Ok(Resolution::Profile);
    }
    let store = crate::profiles::settings_api::load_store()
        .map_err(|error| refusal("profile_store_unavailable", format!("{error:#}")))?;
    // A profile created before the names were reserved keeps working by name.
    if crate::profiles::settings_api::find(&store, profile.trim()).is_some() {
        return Ok(Resolution::Profile);
    }
    unavailable_reason(&store).map_or(Ok(()), |reason| Err(refusal("auto_pick_unavailable", reason)))?;
    let Requested::Provider(provider) = kind else {
        return Err(refusal(
            "provider_required",
            format!(
                "'auto' needs a provider: pass profile \"auto:<provider>\" ({})",
                provider_hint(&store)
            ),
        ));
    };
    live_pick(&store, profile, &provider, spend, refresh_stale).map(Resolution::Picked)
}

/// Why auto-pick is closed on this host: a profile already has a reserved
/// name (an older release would resolve `auto:…` to it).
pub fn unavailable_reason(store: &crate::profiles::model::AccountsStore) -> Option<String> {
    let names: Vec<&str> = store
        .accounts
        .iter()
        .filter(|account| crate::profiles::lifecycle::is_reserved_profile_name(&account.name))
        .map(|account| account.name.as_str())
        .collect();
    (!names.is_empty()).then(|| {
        format!(
            "rename profile {} first: 'auto' and 'auto:…' are reserved for the automatic pick",
            names.iter().map(|name| format!("'{name}'")).collect::<Vec<_>>().join(", ")
        )
    })
}

fn family(account: &StoredAccount) -> String {
    crate::profiles::providers::provider_family(account).0
}

/// `auto:<family>` choices that have an opted-in profile on this host.
fn provider_hint(store: &crate::profiles::model::AccountsStore) -> String {
    let mut families: Vec<String> =
        store.accounts.iter().filter(|account| account.auto_pick).map(family).collect();
    families.sort();
    families.dedup();
    if families.is_empty() {
        "no profile has 「参与自动选择」 turned on".to_string()
    } else {
        format!(
            "with 「参与自动选择」 profiles: {}",
            families.iter().map(|family| format!("auto:{family}")).collect::<Vec<_>>().join(", ")
        )
    }
}

fn live_pick(
    store: &crate::profiles::model::AccountsStore,
    requested: &str,
    provider: &str,
    spend: &Spend,
    refresh_stale: bool,
) -> Result<ProfilePick, PickRefusal> {
    let accounts: Vec<&StoredAccount> =
        store.accounts.iter().filter(|account| family(account) == provider).collect();
    if accounts.is_empty() {
        return Err(refusal(
            "unknown_provider",
            format!("no profile on this host is from provider '{provider}' ({})", provider_hint(store)),
        ));
    }
    let now = chrono::Utc::now().timestamp();
    let open_agents = open_agents_by_profile();
    let history = crate::management::v2::usage::read_quota_history_since(now - SLOPE_WINDOW_SECS - FRESH_SECS)
        .unwrap_or_default();
    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    for account in accounts {
        let exclude = |reason: &str| Excluded { profile: account.name.clone(), reason: reason.to_string() };
        if !account.auto_pick {
            excluded.push(exclude("not_opted_in"));
            continue;
        }
        if crate::profiles::settings_api::agent_use_requires_approval(account) {
            excluded.push(exclude("requires_approval"));
            continue;
        }
        let windows = match live_windows(account, now, refresh_stale) {
            Ok(windows) => windows,
            Err(reason) => {
                excluded.push(exclude(reason));
                continue;
            }
        };
        let windows = windows
            .into_iter()
            .map(|(mut window, observed_at)| {
                window.burn_per_hour = history_slope(&history, &account.id, &window, observed_at);
                window
            })
            .collect();
        candidates.push(Candidate {
            profile_id: account.id.clone(),
            profile: account.name.clone(),
            windows,
            active_agents: open_agents
                .get(&account.name)
                .or_else(|| open_agents.get(&account.id))
                .copied()
                .unwrap_or(0),
        });
    }
    pick_from(requested, provider, spend, &candidates, excluded, now)
}

/// Fresh subscription windows of a profile with their observation time.
fn live_windows(
    account: &StoredAccount,
    now: i64,
    refresh_stale: bool,
) -> Result<Vec<(WindowReading, i64)>, &'static str> {
    let fresh = |gauges: &[crate::profiles::quota::Gauge]| -> Vec<(WindowReading, i64)> {
        gauges
            .iter()
            .filter(|gauge| gauge.kind == "window" && gauge.error.is_none() && !gauge.stale)
            .filter_map(|gauge| {
                let observed = gauge.observed_at?;
                (now - observed <= FRESH_SECS).then_some(())?;
                Some((
                    WindowReading {
                        gauge_id: gauge.id.clone(),
                        label: gauge.window_label.clone(),
                        remaining_percent: gauge.remaining_percent.filter(|value| value.is_finite())?,
                        reset_at: gauge.reset_at,
                        burn_per_hour: None,
                    },
                    observed,
                ))
            })
            .collect()
    };
    let read = crate::management::v2::profile_quota::read_gauges(account, false).map_err(|_| "quota_read_failed")?;
    // Claude subscription and ChatGPT login profiles always have windows,
    // even before their first reading.
    let subscription = matches!(read.source, Some("claude" | "native"));
    let windows_known = subscription || read.gauges.iter().any(|gauge| gauge.kind == "window");
    if !windows_known {
        return Err("no_subscription_windows");
    }
    let windows = fresh(&read.gauges);
    if !windows.is_empty() || !refresh_stale {
        return if windows.is_empty() { Err("quota_stale") } else { Ok(windows) };
    }
    let read = crate::management::v2::profile_quota::read_gauges(account, true).map_err(|_| "quota_read_failed")?;
    let windows = fresh(&read.gauges);
    if windows.is_empty() {
        Err("quota_stale")
    } else {
        Ok(windows)
    }
}

fn history_slope(
    history: &[crate::management::v2::usage::QuotaSnapshot],
    profile_id: &str,
    window: &WindowReading,
    observed_at: i64,
) -> Option<f64> {
    let same_cycle = |reset: Option<i64>| match (reset, window.reset_at) {
        (Some(a), Some(b)) => (a - b).abs() <= SAME_CYCLE_SECS,
        (None, None) => true,
        _ => false,
    };
    let mut points: Vec<(i64, f64)> = history
        .iter()
        .filter(|snapshot| snapshot.profile_id == profile_id && snapshot.gauge_id == window.gauge_id)
        .filter(|snapshot| snapshot.ts >= observed_at - SLOPE_WINDOW_SECS && snapshot.ts < observed_at)
        .filter(|snapshot| same_cycle(snapshot.reset_at))
        .filter_map(|snapshot| Some((snapshot.ts, snapshot.remaining_percent?)))
        .collect();
    points.sort_by_key(|point| point.0);
    points.push((observed_at, window.remaining_percent));
    slope(&points)
}

/// Open (not retired, agent-enabled) sessions by profile name or ID.
fn open_agents_by_profile() -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    let Ok(sessions) = crate::session::store::load_cutex_session_store_shared() else {
        return counts;
    };
    for record in sessions.sessions.values() {
        if record.archive_state != crate::session::model::CutexSessionArchiveState::Active || !record.agent_enabled {
            continue;
        }
        if let Some(profile) = record.profile.as_deref().filter(|profile| !profile.is_empty()) {
            *counts.entry(profile.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

// ------------------------------------------------------- explanation file

fn pick_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join(PICK_DIR))
}

fn pick_file(action_id: &str) -> anyhow::Result<PathBuf> {
    let digest = crate::asks::sha256_hex(action_id.as_bytes());
    Ok(pick_dir()?.join(format!("{}.json", &digest[..32])))
}

/// Keep the explanation of the pick that resolved `action_id`.
pub fn record(action_id: &str, pick: &ProfilePick) {
    let result = pick_file(action_id).and_then(|path| {
        prune_pick_files();
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            &json!({ "actionId": action_id, "profilePick": pick }),
            "profile auto-pick explanation",
        )
    });
    if let Err(error) = result {
        eprintln!("\x1b[33mwarning:\x1b[0m failed to keep the profile auto-pick explanation: {error:#}");
    }
}

/// The explanation kept for `action_id`, if its profile was auto-picked.
pub fn load(action_id: &str) -> Option<ProfilePick> {
    let text = std::fs::read_to_string(pick_file(action_id).ok()?).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    (value["actionId"] == action_id).then(|| serde_json::from_value(value["profilePick"].clone()).ok())?
}

fn prune_pick_files() {
    let Ok(dir) = pick_dir() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() > KEEP_PICK_FILES_SECS);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Adds `profile_pick` to a completed create / replace reply whose profile
/// was auto-picked for `action_id`.
pub fn attach_to_reply(response: &mut Value, action_id: &str) {
    if response["outcome"]["status"] != "complete" {
        return;
    }
    let result = &mut response["outcome"]["receipt"]["result"];
    if !matches!(result["kind"].as_str(), Some("created" | "replaced")) {
        return;
    }
    if let Some(pick) = load(action_id) {
        result["profile_pick"] = serde_json::to_value(pick).unwrap_or(Value::Null);
    }
}

thread_local! {
    /// The pick of the Human create running on this thread (executor →
    /// response).
    static LAST_PICK: RefCell<Option<ProfilePick>> = const { RefCell::new(None) };
}

pub fn set_thread_pick(pick: ProfilePick) {
    LAST_PICK.with(|slot| *slot.borrow_mut() = Some(pick));
}

pub fn clear_thread_pick() {
    LAST_PICK.with(|slot| slot.borrow_mut().take());
}

pub fn take_thread_pick() -> Option<ProfilePick> {
    LAST_PICK.with(|slot| slot.borrow_mut().take())
}

// ------------------------------------------------------------------ route

/// `GET /v2/profile-auto-pick[?provider=&model=&effort=]`: the ranking per
/// provider (one provider, or each provider that has profiles on this host,
/// never compared across providers). Reads cached gauges only.
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    let bad_request = |stream: &mut TcpStream, message: String| {
        super::server::write_v2_error(stream, 400, "Bad Request", "invalid_request", &message, false, json!({}))
    };
    if request.method != "GET" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "only GET is supported",
            false,
            json!({}),
        );
    }
    let (mut provider, mut model, mut effort) = (None, None, None);
    if let Some((_, query)) = request.path.split_once('?') {
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let value = Some(value.into_owned()).filter(|value| !value.trim().is_empty());
            match name.as_ref() {
                "provider" => provider = value.map(|value| canonical_provider(value.trim())),
                "model" => model = value,
                "effort" => effort = value,
                other => return bad_request(stream, format!("unsupported query parameter: {other}")),
            }
        }
    }
    let store = match crate::profiles::settings_api::load_store() {
        Ok(store) => store,
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "profile_store_unavailable",
                "the profile store could not be read",
                true,
                json!({ "diagnostic": format!("{error:#}") }),
            )
        }
    };
    let spend = Spend::new(model.as_deref(), effort.as_deref());
    let unavailable = unavailable_reason(&store);
    let mut providers: Vec<(String, String)> =
        store.accounts.iter().map(crate::profiles::providers::provider_family).collect();
    providers.sort();
    providers.dedup();
    if let Some(provider) = &provider {
        providers.retain(|(family, _)| family == provider);
        if providers.is_empty() {
            providers.push((provider.clone(), provider.clone()));
        }
    }
    let entries: Vec<Value> = providers
        .into_iter()
        .map(|(family, label)| {
            let profiles = store.accounts.iter().filter(|account| self::family(account) == family);
            let (count, opted_in) =
                profiles.fold((0, 0), |(count, opted), account| (count + 1, opted + usize::from(account.auto_pick)));
            let outcome = match &unavailable {
                Some(reason) => Err(refusal("auto_pick_unavailable", reason.clone())),
                None => live_pick(&store, &format!("auto:{family}"), &family, &spend, false),
            };
            let (pick, refused) = match outcome {
                Ok(pick) => (serde_json::to_value(pick).unwrap_or(Value::Null), Value::Null),
                Err(refused) => (Value::Null, json!({ "code": refused.code, "detail": refused.detail })),
            };
            json!({
                "provider": family,
                "providerLabel": label,
                "profiles": count,
                "optedIn": opted_in,
                "profilePick": pick,
                "refusal": refused,
            })
        })
        .collect();
    write_json_response(
        stream,
        200,
        "OK",
        &json!({
            "contractVersion": 2,
            "available": unavailable.is_none(),
            "unavailableReason": unavailable,
            "providers": entries,
        }),
    )
}

#[cfg(test)]
#[path = "profile_auto_pick_tests.rs"]
mod tests;
