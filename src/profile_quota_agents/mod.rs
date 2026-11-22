//! Secret-free quota planning for managing agents.
pub mod alerts;
pub mod attribution;
pub mod load;
mod snapshot;
pub use snapshot::*;
#[cfg(test)]
mod tests;

pub const FEATURE: &str = "profile-quota-agents-v1";
pub fn manager(roster: &crate::agent_management::AgentManagementSnapshot, caller: &str) -> bool {
    roster
        .projects
        .values()
        .any(|p| p.authorized_director_session.as_str() == caller)
        || roster
            .operator_grants
            .values()
            .any(|g| g.keys().any(|id| id.as_str() == caller))
}
pub fn tool() -> serde_json::Value {
    serde_json::json!({"name":"profile_list","description":"Read profile quota windows, freshness, busy members and estimated exhaustion. Directors and project operators only. Unknown readings are not healthy quota. Optional model and reasoning preview automatic placement without creating an agent.","inputSchema":{"type":"object","properties":{"provider":{"type":"string"},"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":50},"model":{"type":"string"},"reasoning":{"type":"string"}},"additionalProperties":false}})
}

pub fn list(caller: &str, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    use anyhow::ensure;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        provider: Option<String>,
        cursor: Option<String>,
        limit: Option<usize>,
        model: Option<String>,
        reasoning: Option<String>,
    }
    let args: Args = serde_json::from_value(args.clone())?;
    ensure!(
        args.limit.is_none_or(|n| (1..=50).contains(&n)),
        "limit must be 1..50"
    );
    ensure!(
        [&args.provider, &args.cursor, &args.model, &args.reasoning]
            .iter()
            .all(|s| s.as_ref().is_none_or(|s| s.len() <= 200)),
        "argument too long"
    );
    let directory =
        crate::asks::directory::Directory::load().map_err(|e| anyhow::anyhow!("{}", e.detail))?;
    let roster = crate::asks::directory::Directory::director_snapshot_shared()
        .map_err(|e| anyhow::anyhow!("{}", e.detail))?;
    ensure!(
        manager(&roster, caller),
        "Director or project operator required"
    );
    let now = chrono::Utc::now().timestamp();
    let profiles = profiles()?;
    let members = members()?;
    let history =
        crate::management::v2::usage::read_quota_history_cached_since(now - 72 * 3600).unwrap_or_default();
    let mut rows = Vec::new();
    let mut more = false;
    for account in profiles
        .iter()
        .filter(|a| args.cursor.as_ref().is_none_or(|c| a.id > *c))
    {
        let provider = crate::profiles::providers::provider_family(account).0;
        if args.provider.as_ref().is_some_and(|p| p != &provider) {
            continue;
        }
        if rows.len() == args.limit.unwrap_or(25) {
            more = true;
            break;
        }
        let mut row = view(account, &profiles, &members, now, &history)?;
        row.members.retain(|m| {
            directory.may_interact(caller, &m.id) && directory.same_domain(caller, &m.id)
        });
        row.members_truncated = row.members.len() > 20;
        row.members.truncate(20);
        row.top_consumers = attribution::read(&account.name).map(|value| {
            attribution::visible_rows(value, |id| {
                directory.may_interact(caller, id) && directory.same_domain(caller, id)
            })
        });
        // A pool count may include other projects; names always follow caller visibility.
        rows.push(row);
    }
    let ranking = args.provider.as_ref().map(|provider| {
        crate::management::v2::profile_auto_pick::resolve_live(
            &format!("auto:{provider}"),
            &crate::management::v2::profile_auto_pick::Spend::new(
                args.model.as_deref(),
                args.reasoning.as_deref(),
            ),
            false,
        )
        .map(|r| match r {
            crate::management::v2::profile_auto_pick::Resolution::Picked(p) => serde_json::json!(p),
            _ => serde_json::Value::Null,
        })
        .unwrap_or_else(|e| serde_json::json!({"state":"unknown","reason":e.code}))
    });
    Ok(
        serde_json::json!({"observedAt":now,"data":rows,"nextCursor":if more{rows.last().map(|r|r.id.clone())}else{None},"ranking":ranking}),
    )
}
