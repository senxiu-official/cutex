use super::*;

const NOW: i64 = 1_800_000_000;
const HOUR: i64 = 3600;

fn window(label: &str, remaining: f64, reset_in_hours: f64, burn: f64) -> WindowReading {
    WindowReading {
        gauge_id: format!("claude-{label}"),
        label: Some(label.to_string()),
        remaining_percent: remaining,
        reset_at: Some(NOW + (reset_in_hours * HOUR as f64) as i64),
        burn_per_hour: Some(burn),
    }
}

fn candidate(name: &str, windows: Vec<WindowReading>, active_agents: usize) -> Candidate {
    Candidate { profile_id: format!("id-{name}"), profile: name.to_string(), windows, active_agents }
}

fn opus_high() -> Spend {
    Spend::new(Some("claude-opus-4-5"), Some("high"))
}

#[test]
fn requested_profile_values() {
    assert_eq!(requested("GammaProfile"), Requested::Profile);
    assert_eq!(requested("automatic"), Requested::Profile);
    assert_eq!(requested("auto"), Requested::Bare);
    assert_eq!(requested(" AUTO "), Requested::Bare);
    assert_eq!(requested("auto:anthropic"), Requested::Provider("anthropic".into()));
    assert_eq!(requested("Auto:Claude"), Requested::Provider("anthropic".into()));
    assert_eq!(requested("auto:glm"), Requested::Provider("zhipu".into()));
    assert_eq!(requested("auto:custom:api.example.com"), Requested::Provider("custom:api.example.com".into()));
}

#[test]
fn spend_table_and_window_lengths() {
    assert_eq!(opus_high().percent_per_hour(Some(5.0)), 12.0);
    assert_eq!(opus_high().percent_per_hour(Some(168.0)), 1.0);
    assert_eq!(Spend::new(Some("glm-4.6"), None).percent_per_hour(Some(5.0)), 12.0 * 0.6);
    assert_eq!(Spend::new(Some("claude-haiku-4-5"), Some("low")).percent_per_hour(Some(5.0)), 12.0 * 0.25 * 0.5);
    assert_eq!(Spend::new(Some("default"), Some(" ")), Spend::default());
    assert_eq!(window_hours(Some("5h")), Some(5.0));
    assert_eq!(window_hours(Some("7d")), Some(168.0));
    assert_eq!(window_hours(Some("week")), Some(168.0));
    assert_eq!(window_hours(Some("300m")), Some(5.0));
    assert_eq!(window_hours(Some("Claude")), None);
    assert_eq!(window_hours(None), None);
}

/// Design 041 §2 worked example: AlphaProfile wins on its weekly window although
/// GammaProfile and Ceobe look better on 5h remaining alone.
#[test]
fn worked_example_picks_the_latest_wall() {
    let spend = opus_high();
    let candidates = vec![
        candidate("GammaProfile", vec![window("5h", 62.0, 3.0, 10.0), window("7d", 40.0, 96.0, 0.8)], 2),
        candidate("AlphaProfile", vec![window("5h", 85.0, 4.5, 5.0), window("7d", 12.0, 48.0, 0.5)], 1),
        candidate("Ceobe", vec![window("5h", 20.0, 0.5, 25.0), window("7d", 70.0, 120.0, 1.5)], 3),
    ];
    let scored: Vec<_> = candidates.iter().map(|c| score(c, &spend, NOW)).collect();
    assert_eq!(scored[0].hours, 2.8);
    assert_eq!(scored[0].binding_window.as_deref(), Some("5h"));
    assert_eq!(scored[1].hours, 8.0);
    assert_eq!(scored[1].binding_window.as_deref(), Some("7d"));
    assert_eq!(scored[2].hours, 3.2);
    let pick = pick_from("auto:anthropic", "anthropic", &spend, &candidates, Vec::new(), NOW).unwrap();
    assert_eq!(pick.picked, "AlphaProfile");
    assert_eq!(pick.picked_id, "id-AlphaProfile");
    assert_eq!(pick.rule, "latest_wall");
    assert_eq!(pick.candidates.iter().map(|c| c.profile.as_str()).collect::<Vec<_>>(), ["AlphaProfile", "Ceobe", "GammaProfile"]);
    assert!(pick.reason.starts_with("AlphaProfile: 7d window 12% left, about 8.0 h"), "{}", pick.reason);
    let value = serde_json::to_value(&pick).unwrap();
    assert_eq!(value["candidates"][0]["bindingWindow"], "7d");
    assert_eq!(value["pickedId"], "id-AlphaProfile");
}

#[test]
fn ties_prefer_fewer_agents_then_name() {
    let spend = opus_high();
    let busy = candidate("Alpha", vec![window("5h", 90.0, 4.0, 0.0)], 3);
    let idle = candidate("Beta", vec![window("5h", 88.0, 4.0, 0.0)], 0);
    let pick = pick_from("auto:anthropic", "anthropic", &spend, &[busy.clone(), idle.clone()], Vec::new(), NOW).unwrap();
    assert_eq!((pick.picked.as_str(), pick.rule.as_str()), ("Beta", "tie_fewer_agents"));
    let twin = Candidate { profile: "Aaron".into(), profile_id: "id-Aaron".into(), ..idle.clone() };
    let pick = pick_from("auto:anthropic", "anthropic", &spend, &[idle, twin], Vec::new(), NOW).unwrap();
    assert_eq!((pick.picked.as_str(), pick.rule.as_str()), ("Aaron", "tie_name"));
}

#[test]
fn near_wall_profiles_are_skipped_and_all_near_wall_takes_the_soonest_reset() {
    let spend = opus_high();
    let nearly_out = candidate("Low", vec![window("5h", 5.0, 3.0, 10.0)], 0);
    let fine = candidate("Fine", vec![window("5h", 30.0, 3.0, 10.0)], 0);
    let pick = pick_from("auto:anthropic", "anthropic", &spend, &[nearly_out.clone(), fine], Vec::new(), NOW).unwrap();
    assert_eq!(pick.picked, "Fine");
    let soon = candidate("Soon", vec![window("5h", 4.0, 0.2, 10.0)], 0);
    // 4 % at 22 %/h runs out in 0.18 h, before the reset at 0.2 h.
    let pick = pick_from("auto:anthropic", "anthropic", &spend, &[nearly_out, soon], Vec::new(), NOW).unwrap();
    assert_eq!((pick.picked.as_str(), pick.rule.as_str()), ("Soon", "all_near_wall_soonest_reset"));
}

#[test]
fn burn_falls_back_to_open_agents_and_scores_cap_at_a_day() {
    let spend = opus_high();
    let mut unknown = window("5h", 60.0, 10.0, 0.0);
    unknown.burn_per_hour = None;
    let scored = score(&candidate("A", vec![unknown], 2), &spend, NOW);
    // (2 agents + the new one) × 12 %/h = 36 %/h: 60 / 36 = 1.7 h.
    assert_eq!((scored.hours, scored.burn_per_hour, scored.burn_source.as_str()), (1.7, Some(24.0), "agents"));
    let roomy = score(&candidate("B", vec![window("7d", 100.0, 100.0, 0.0)], 0), &spend, NOW);
    assert_eq!(roomy.hours, 24.0);
}

#[test]
fn no_candidate_is_refused_with_the_reasons() {
    let excluded = vec![Excluded { profile: "ZetaProfile".into(), reason: "not_opted_in".into() }];
    let refused = pick_from("auto:zhipu", "zhipu", &Spend::default(), &[], excluded, NOW).unwrap_err();
    assert_eq!(refused.code, "no_auto_pick_candidate");
    assert!(refused.detail.contains("ZetaProfile: not_opted_in"), "{}", refused.detail);
}

#[test]
fn slope_needs_ten_minutes_of_the_same_cycle() {
    assert_eq!(slope(&[(NOW, 50.0)]), None);
    assert_eq!(slope(&[(NOW - 300, 52.0), (NOW, 50.0)]), None);
    assert_eq!(slope(&[(NOW - HOUR, 60.0), (NOW - 1800, 55.0), (NOW, 50.0)]), Some(10.0));
    // Remaining went up (a refill): no negative spend.
    assert_eq!(slope(&[(NOW - HOUR, 40.0), (NOW, 50.0)]), Some(0.0));
    let current = window("5h", 50.0, 2.0, 0.0);
    let snapshot = |ts: i64, pct: f64, reset_at: i64| crate::management::v2::usage::QuotaSnapshot {
        ts,
        profile_id: "id-A".into(),
        profile: "A".into(),
        gauge_id: current.gauge_id.clone(),
        label: None,
        kind: Some("window".into()),
        remaining_percent: Some(pct),
        balance: None,
        reset_at: Some(reset_at),
    };
    let reset = current.reset_at.unwrap();
    let history = vec![
        // The previous cycle (reset 5 h earlier) does not count.
        snapshot(NOW - 3000, 95.0, reset - 5 * HOUR),
        snapshot(NOW - 1800, 56.0, reset + 60),
        snapshot(NOW - 900, 53.0, reset),
    ];
    assert_eq!(history_slope(&history, "id-A", &current, NOW), Some(12.0));
    assert_eq!(history_slope(&history, "id-B", &current, NOW), None);
}

#[test]
fn reserved_profile_names() {
    use crate::profiles::lifecycle::{is_reserved_profile_name, validate_profile_name};
    for name in ["auto", "AUTO", " auto ", "auto:anthropic", "Auto:x", "auto:"] {
        assert!(is_reserved_profile_name(name), "{name:?}");
        assert!(validate_profile_name(name).is_err(), "{name:?}");
    }
    for name in ["automatic", "auto-glm", "my auto", "autopilot:x"] {
        assert!(!is_reserved_profile_name(name), "{name:?}");
        assert!(validate_profile_name(name).is_ok(), "{name:?}");
    }
}

#[test]
fn a_store_with_a_reserved_name_closes_auto_pick() {
    let mut store: crate::profiles::model::AccountsStore = serde_json::from_value(serde_json::json!({
        "version": 1,
        "accounts": [],
    }))
    .unwrap();
    assert_eq!(unavailable_reason(&store), None);
    let account: StoredAccount = serde_json::from_value(serde_json::json!({
        "id": "p1", "name": "auto:anthropic", "email": null, "plan_type": null, "last_used_at": null
    }))
    .unwrap();
    store.accounts.push(account);
    let reason = unavailable_reason(&store).unwrap();
    assert!(reason.contains("'auto:anthropic'"), "{reason}");
    // The existing profile keeps its name through the CLI uniqueness check.
    assert!(crate::profiles::lookup::ensure_unique_name(&store, "auto:anthropic", Some("p1")).is_ok());
    assert!(crate::profiles::lookup::ensure_unique_name(&store, "auto:zhipu", Some("p1")).is_err());
    assert!(crate::profiles::lookup::ensure_unique_name(&store, "auto", None).is_err());
}

/// Rollback: the opt-in is omitted when off, so the store an older release
/// reads is byte-for-byte what it wrote; an older save drops it (back to off).
#[test]
fn auto_pick_opt_in_is_additive_and_drops_to_off() {
    let mut account: StoredAccount = serde_json::from_value(serde_json::json!({
        "id": "p1", "name": "AlphaProfile", "email": null, "plan_type": null, "last_used_at": null
    }))
    .unwrap();
    assert!(!account.auto_pick);
    assert!(serde_json::to_value(&account).unwrap().get("auto_pick").is_none());
    account.auto_pick = true;
    let saved = serde_json::to_value(&account).unwrap();
    assert_eq!(saved["auto_pick"], true);
    let reread: StoredAccount = serde_json::from_value(saved.clone()).unwrap();
    assert!(reread.auto_pick);
    // An older release keeps only the fields it knows.
    let mut older = saved;
    older.as_object_mut().unwrap().remove("auto_pick");
    let after_old_save: StoredAccount = serde_json::from_value(older).unwrap();
    assert!(!after_old_save.auto_pick);
}

#[test]
fn explanation_round_trips_and_attaches_to_completed_replies() {
    let spend = opus_high();
    let candidates = vec![candidate("AlphaProfile", vec![window("5h", 80.0, 4.0, 1.0)], 0)];
    let pick = pick_from("auto:anthropic", "anthropic", &spend, &candidates, Vec::new(), NOW).unwrap();
    assert_eq!(pick.rule, "only_candidate");
    let stored: ProfilePick = serde_json::from_value(serde_json::to_value(&pick).unwrap()).unwrap();
    assert_eq!(stored, pick);
    let mut pending = serde_json::json!({ "outcome": { "status": "pending" } });
    attach_to_reply(&mut pending, "never-picked");
    assert!(pending["outcome"].get("receipt").is_none());
}

#[test]
fn thread_pick_is_taken_once() {
    let candidates = vec![candidate("AlphaProfile", vec![window("5h", 80.0, 4.0, 1.0)], 0)];
    let pick = pick_from("auto:anthropic", "anthropic", &Spend::default(), &candidates, Vec::new(), NOW).unwrap();
    clear_thread_pick();
    assert_eq!(take_thread_pick(), None);
    set_thread_pick(pick.clone());
    assert_eq!(take_thread_pick(), Some(pick));
    assert_eq!(take_thread_pick(), None);
}
