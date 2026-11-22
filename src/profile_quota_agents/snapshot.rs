use crate::profiles::model::{CliKind, StoredAccount};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub id: String,
    pub name: Option<String>,
    pub profile: String,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub busy: bool,
    pub queued: usize,
    pub started_at: Option<i64>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub id: String,
    pub label: Option<String>,
    pub state: String,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<i64>,
    pub observed_at: Option<i64>,
    pub forecast: Forecast,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Forecast {
    pub state: String,
    pub runs_out_at: Option<i64>,
    pub percent_per_hour: Option<f64>,
}
impl Default for Forecast {
    fn default() -> Self {
        Self {
            state: "unknown".into(),
            runs_out_at: None,
            percent_per_hour: None,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub pool_id: String,
    pub auto_pick: bool,
    pub state: String,
    pub windows: Vec<Window>,
    pub busy_member_count: usize,
    pub queued_member_count: usize,
    pub members: Vec<Member>,
    pub members_truncated: bool,
    pub top_consumers: Option<Vec<Value>>,
}

// r290 P12: profiles are bounded configuration, not a new canonical account store.
pub fn profiles() -> anyhow::Result<Vec<StoredAccount>> {
    let mut profiles = crate::profiles::settings_api::load_store()?.accounts;
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(profiles)
}
/// Never use credentials as a pool identifier. Known shared auth custody and
/// nonsecret account metadata join profiles; an unknown identity stays separate.
pub fn pool_id(account: &StoredAccount) -> String {
    let key = if account.cli_kind == CliKind::Claude {
        format!(
            "claude:{}",
            crate::management::v2::profile_quota_claude::profile_account(account)
        )
    } else if account.private_chatgpt_auth_file {
        "native:private-chatgpt-auth".into()
    } else if crate::management::v2::profile_quota_native::openai_codex_profile(account) {
        match account
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(email) => format!("native:{}", email.to_lowercase()),
            None => format!("profile:{}", account.id),
        }
    } else {
        format!("profile:{}", account.id)
    };
    format!("pool-{}", crate::asks::sha256_hex(key.as_bytes()))
}
pub fn members() -> anyhow::Result<Vec<Member>> {
    let asks = crate::asks::store::AskStore::open_default()?;
    let path = crate::session::store::cutex_sessions_path()?;
    let mut cursor = None;
    let mut rows = Vec::new();
    loop {
        let Some(page) = crate::session::store::session_page_at(
            &path,
            false,
            cursor.as_deref(),
            crate::store::PageBudget::LIST_DEFAULT,
        )?
        else {
            break;
        };
        asks.inspect(|state| {
            for record in &page.rows {
                if record.is_retired() || !record.agent_enabled {
                    continue;
                }
                let Some(profile) = record.profile.as_ref() else {
                    continue;
                };
                let presence = state.presence.get(&record.cutex_session_id);
                let pending = record.app_server_launch_claim_id.is_some();
                let queued = (presence.map_or(0, |p| p.owner_queue)
                    + state.waiting_for_target(&record.cutex_session_id).len())
                .max(usize::from(pending));
                let busy = presence.is_some_and(|p| p.online && p.busy && !p.paused);
                if !super::load::working(
                    presence.is_some_and(|p| p.online),
                    busy,
                    presence.is_some_and(|p| p.paused || p.agent_pause.is_some()),
                    queued,
                    pending,
                ) {
                    continue;
                }
                rows.push(Member {
                    id: record.cutex_session_id.clone(),
                    name: record.formal_agent_name.clone(),
                    profile: profile.clone(),
                    model: record.model_defaults.clone(),
                    reasoning: record.reasoning_defaults.clone(),
                    busy,
                    queued,
                    started_at: None,
                });
            }
        })?;
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    // Point reads only for busy members, never an activity-state full snapshot.
    for member in &mut rows {
        if member.busy {
            member.started_at = crate::management::v2::activity::session_activity(&member.id)?
                .and_then(|a| a.last_turn_started_at)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.timestamp());
        }
    }
    Ok(rows)
}
pub fn forecast(
    gauge: &crate::profiles::quota::Gauge,
    points: &[crate::management::v2::usage::QuotaSnapshot],
    profile: &str,
    now: i64,
) -> Forecast {
    let Some(left) = gauge.remaining_percent.filter(|v| v.is_finite()) else {
        return Forecast::default();
    };
    if gauge.stale
        || gauge.error.is_some()
        || gauge.observed_at.is_none_or(|at| now - at > 900)
        || gauge.past_unobserved_reset(now)
    {
        return Forecast::default();
    }
    if left <= 0.0 {
        return Forecast {
            state: "exhausted".into(),
            ..Forecast::default()
        };
    }
    let weekly =
        crate::management::v2::profile_auto_pick::window_hours(gauge.window_label.as_deref())
            .is_some_and(|h| h >= 48.0);
    let lookback = if weekly { 72 * 3600 } else { 3600 };
    let minspan = if weekly { 24 * 3600 } else { 600 };
    let observed = gauge.observed_at.unwrap_or(now);
    let mut points: Vec<_> = points
        .iter()
        .filter(|p| {
            p.profile_id == profile
                && p.gauge_id == gauge.id
                && p.ts >= observed - lookback
                && p.ts < observed
        })
        .filter(|p| match (p.reset_at, gauge.reset_at) {
            (Some(a), Some(b)) => (a - b).abs() <= 600,
            (None, None) => true,
            _ => false,
        })
        .filter_map(|p| {
            p.remaining_percent
                .filter(|v| v.is_finite())
                .map(|v| (p.ts, v))
        })
        .collect();
    points.sort_by_key(|p| p.0);
    points.push((observed, left));
    let Some(first) = points.first().filter(|p| observed - p.0 >= minspan) else {
        return Forecast::default();
    };
    let rate = ((first.1 - left) / ((observed - first.0) as f64 / 3600.0)).max(0.0);
    let at = (rate > 0.0).then(|| observed + (left / rate * 3600.0).ceil() as i64);
    let runs = at.filter(|at| gauge.reset_at.is_none_or(|reset| *at < reset));
    Forecast {
        state: if runs.is_some() { "runsOut" } else { "lasts" }.into(),
        runs_out_at: runs,
        percent_per_hour: Some(rate),
    }
}
pub fn window(
    g: &crate::profiles::quota::Gauge,
    history: &[crate::management::v2::usage::QuotaSnapshot],
    profile: &str,
    now: i64,
) -> Window {
    let known = !g.stale
        && g.error.is_none()
        && g.observed_at
            .is_some_and(|at| (0..=900).contains(&(now - at)))
        && !g.past_unobserved_reset(now)
        && g.remaining_percent
            .is_some_and(|v| v.is_finite() && (0.0..=100.0).contains(&v));
    Window {
        id: g.id.clone(),
        label: g.window_label.clone(),
        state: if known { "known" } else { "unknown" }.into(),
        remaining_percent: if known { g.remaining_percent } else { None },
        reset_at: g.reset_at,
        observed_at: g.observed_at,
        forecast: if known {
            forecast(g, history, profile, now)
        } else {
            Forecast::default()
        },
    }
}
pub fn view(
    account: &StoredAccount,
    profiles: &[StoredAccount],
    members: &[Member],
    now: i64,
    history: &[crate::management::v2::usage::QuotaSnapshot],
) -> anyhow::Result<View> {
    let pool = pool_id(account);
    let pool_profiles: Vec<_> = profiles.iter().filter(|a| pool_id(a) == pool).collect();
    let members: Vec<_> = members
        .iter()
        .filter(|m| {
            pool_profiles
                .iter()
                .any(|a| m.profile == a.id || m.profile == a.name)
        })
        .cloned()
        .collect();
    let mut merged = std::collections::BTreeMap::<String, Window>::new();
    let mut no_windows = false;
    for alias in &pool_profiles {
        let gauges = crate::management::v2::profile_quota::cached_gauges(alias).ok();
        no_windows |= gauges.as_ref().is_some_and(|g| {
            g.source == Some("claudeApi")
                || (!g.gauges.is_empty() && g.gauges.iter().all(|w| w.kind == "balance"))
        });
        for g in gauges
            .iter()
            .flat_map(|g| g.gauges.iter())
            .filter(|g| g.kind == "window")
        {
            let next = window(g, history, &alias.id, now);
            let replace = merged.get(&g.id).is_none_or(|old| {
                match (old.state.as_str(), next.state.as_str()) {
                    ("unknown", "known") => true,
                    ("known", "unknown") => false,
                    _ => {
                        next.observed_at > old.observed_at
                            || (next.observed_at == old.observed_at
                                && next.remaining_percent < old.remaining_percent)
                    }
                }
            });
            if replace {
                merged.insert(g.id.clone(), next);
            }
        }
    }
    let windows: Vec<_> = merged.into_values().collect();
    Ok(View {
        id: account.id.clone(),
        name: account.name.clone(),
        provider: crate::profiles::providers::provider_family(account).0,
        pool_id: pool,
        auto_pick: account.auto_pick,
        state: if windows.is_empty() && no_windows {
            "notApplicable"
        } else if windows.is_empty() || windows.iter().any(|w| w.state == "unknown") {
            "unknown"
        } else {
            "known"
        }
        .into(),
        busy_member_count: members.iter().filter(|m| m.busy).count(),
        queued_member_count: members.iter().filter(|m| m.queued > 0).count(),
        members,
        members_truncated: false,
        windows,
        top_consumers: None,
    })
}
