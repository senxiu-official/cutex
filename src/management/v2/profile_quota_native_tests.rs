use super::*;

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cutex-native-quota-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn account(source: &str) -> StoredAccount {
    serde_json::from_value(json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "name": "orchid",
        "source": source,
    }))
    .unwrap()
}

/// Shape of a real `account/rateLimits/read` result (ChatGPT Pro, values
/// altered; account id / upsell removed by the normalizer).
fn sample_read() -> Value {
    json!({
        "ordinaryUsageAllowed": true,
        "accountId": "acct-should-not-be-stored",
        "rateLimits": {
            "limitId": "codex",
            "limitName": null,
            "normalModelSlug": null,
            "primary": { "usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1791500000 },
            "secondary": null,
            "credits": { "hasCredits": true, "unlimited": false, "balance": "62419.1834" },
            "individualLimit": null,
            "spendControlReached": null,
            "planType": "pro",
            "rateLimitReachedType": null
        },
        "rateLimitsByLimitId": {
            "codex": { "limitId": "codex", "primary": { "usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1791500000 } }
        },
        "rateLimitResetCredits": { "availableCount": 2, "credits": null },
        "rateLimitUpsell": null
    })
}

#[test]
fn normalizes_a_rate_limits_read_response() {
    let read = sample_read();
    let gauges = normalize(&read["rateLimits"], 1_791_000_000);
    let ids: Vec<_> = gauges.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, [GAUGE_PRIMARY, GAUGE_CREDITS]);
    let primary = &gauges[0];
    assert_eq!(primary.kind, "window");
    assert_eq!(primary.used_percent, Some(18.0));
    assert_eq!(primary.remaining_percent, Some(82.0));
    assert_eq!(primary.reset_at, Some(1_791_500_000));
    assert_eq!(primary.window_label.as_deref(), Some("week"));
    assert_eq!(primary.observed_at, Some(1_791_000_000));
    assert_eq!(primary.source, Some("native"));
    let credits = &gauges[1];
    assert_eq!(credits.kind, "balance");
    assert_eq!(credits.balance.as_deref(), Some("62419.18"));
    let view = serde_json::to_value(primary).unwrap();
    assert_eq!(view["usedPercent"], 18.0);
    assert_eq!(view["windowLabel"], "week");
    assert_eq!(view["source"], "native");

    // Both windows (5h + week), clamped percent, unlimited credits.
    let gauges = normalize(
        &json!({
            "primary": { "usedPercent": 130, "windowDurationMins": 300, "resetsAt": 10 },
            "secondary": { "usedPercent": 2.5, "windowDurationMins": 10080, "resetsAt": null },
            "credits": { "hasCredits": true, "unlimited": true, "balance": null }
        }),
        5,
    );
    assert_eq!(gauges.len(), 3);
    assert_eq!(gauges[0].window_label.as_deref(), Some("5h"));
    assert_eq!(gauges[0].remaining_percent, Some(0.0));
    assert_eq!(gauges[1].id, GAUGE_SECONDARY);
    assert_eq!(gauges[1].reset_at, None);
    assert_eq!(gauges[2].balance.as_deref(), Some("unlimited"));
    // No credits balance and not unlimited → no credits gauge.
    assert!(normalize(&json!({ "credits": { "hasCredits": false, "unlimited": false, "balance": null } }), 1).is_empty());
    assert_eq!(window_caption(1440), "day");
    assert_eq!(window_caption(2880), "2d");
    assert_eq!(window_caption(45), "45m");
}

#[test]
fn quota_config_gauges_keep_their_shape() {
    // HTTP reader gauges serialize without the native-only fields.
    let mut gauge = normalize(&sample_read()["rateLimits"], 1).remove(0);
    gauge.used_percent = None;
    gauge.source = None;
    let view = serde_json::to_value(&gauge).unwrap();
    assert!(view.get("usedPercent").is_none());
    assert!(view.get("source").is_none());
}

#[test]
fn snapshot_persists_and_round_trips_without_account_ids() {
    let dir = temp_dir();
    let account = account("official");
    let (snapshot, changed) = apply_read_at(&dir, &account, Ok(sample_read()), 1000).unwrap();
    assert!(changed.content);
    assert_eq!(snapshot.fetched_at, Some(1000));
    assert_eq!(snapshot.source.as_deref(), Some("read"));
    assert_eq!(snapshot.ordinary_usage_allowed, Some(true));
    assert_eq!(snapshot.reset_credits_available, Some(2));
    let loaded = load_at(&dir, &account.id).expect("persisted snapshot");
    assert_eq!(loaded, snapshot);
    let raw = std::fs::read_to_string(dir.join(format!("native-{}.json", account.id))).unwrap();
    assert!(!raw.contains("acct-should-not-be-stored"));
    assert!(!raw.contains("normalModelSlug"));
    // Served after "restart" with fetchedAt; stale once old.
    let gauges = loaded.gauges(1000 + 60, 300);
    assert_eq!(gauges.len(), 2);
    assert!(!gauges[0].stale);
    assert!(loaded.gauges(1000 + 3600, 300)[0].stale);
    assert_eq!(loaded.meta(1060, 300)["planType"], "pro");

    // Same data again: no content change; a failure keeps the gauges, marks
    // them stale and is a change (failing flag).
    let (_, changed) = apply_read_at(&dir, &account, Ok(sample_read()), 1100).unwrap();
    assert!(!changed.content);
    let (failed, changed) = apply_read_at(&dir, &account, Err("boom".into()), 1200).unwrap();
    assert!(changed.content);
    assert_eq!(failed.consecutive_failures, 1);
    assert_eq!(failed.fetched_at, Some(1100));
    assert!(failed.gauges(1201, 300)[0].stale);
    let (failed, _) = apply_read_at(&dir, &account, Err("x".repeat(5000)), 1250).unwrap();
    assert_eq!(failed.last_error.unwrap().chars().count(), MAX_ERROR_CHARS + 1);
    let (recovered, _) = apply_read_at(&dir, &account, Ok(sample_read()), 1300).unwrap();
    assert_eq!(recovered.consecutive_failures, 0);
    assert!(recovered.last_error.is_none());

    // A snapshot under another profile's id is not served.
    assert!(load_at(&dir, &uuid::Uuid::new_v4().to_string()).is_none());
    assert!(load_at(&dir, "../escape").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_notifications_merge_into_the_snapshot() {
    let dir = temp_dir();
    let account = account("official");
    apply_read_at(&dir, &account, Ok(sample_read()), 1000).unwrap();
    // Sparse live update: new primary usage, no credits → credits kept.
    let (snapshot, changed) = apply_notification_at(
        &dir,
        &account,
        &json!({ "rateLimits": {
            "limitId": "codex",
            "primary": { "usedPercent": 25, "windowDurationMins": 10080, "resetsAt": 1791500000 },
            "secondary": null, "credits": null, "planType": null
        } }),
        2000,
    )
    .unwrap()
    .expect("codex bucket applies");
    assert!(changed.content);
    assert_eq!(snapshot.source.as_deref(), Some("notification"));
    assert_eq!(snapshot.last_notification_at, Some(2000));
    let gauges = snapshot.gauges(2001, 300);
    assert_eq!(gauges[0].used_percent, Some(25.0));
    assert_eq!(gauges[0].observed_at, Some(2000));
    assert_eq!(gauges[1].balance.as_deref(), Some("62419.18"));
    assert_eq!(snapshot.rate_limits.as_ref().unwrap()["planType"], "pro");

    // Same values again: stored (fresh fetchedAt) but not a change.
    let (_, changed) = apply_notification_at(
        &dir,
        &account,
        &json!({ "rateLimits": { "primary": { "usedPercent": 25, "windowDurationMins": 10080, "resetsAt": 1791500000 } } }),
        2100,
    )
    .unwrap()
    .unwrap();
    assert!(!changed.content);
    // Other buckets and empty updates are ignored.
    for params in [
        json!({ "rateLimits": { "limitId": "codex_other", "primary": { "usedPercent": 99 } } }),
        json!({ "rateLimits": { "primary": null, "secondary": null } }),
        json!({}),
    ] {
        assert!(apply_notification_at(&dir, &account, &params, 2200).unwrap().is_none());
    }
    assert_eq!(load_at(&dir, &account.id).unwrap().fetched_at, Some(2100));

    // A notification alone (no prior read) creates the snapshot.
    let fresh = super::tests::account("official");
    let (snapshot, _) = apply_notification_at(
        &dir,
        &fresh,
        &json!({ "rateLimits": { "primary": { "usedPercent": 1, "windowDurationMins": 300 } } }),
        3000,
    )
    .unwrap()
    .unwrap();
    assert_eq!(snapshot.gauges(3000, 300)[0].window_label.as_deref(), Some("5h"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn observed_plan_and_its_time_are_tracked() {
    let dir = temp_dir();
    let account = account("official");
    let (snapshot, change) = apply_read_at(&dir, &account, Ok(sample_read()), 1000).unwrap();
    assert!(change.plan, "first observation of the plan");
    assert_eq!(snapshot.plan_observation(), Some(("pro".into(), 1000)));
    assert_eq!(snapshot.meta(1000, 300)["planTypeObservedAt"], 1000);
    // Same plan again: content may change, the plan does not.
    let (_, change) = apply_read_at(&dir, &account, Ok(sample_read()), 1100).unwrap();
    assert!(!change.plan);
    // A sparse notification keeps the plan and when it was observed.
    let (snapshot, change) = apply_notification_at(
        &dir,
        &account,
        &json!({ "rateLimits": { "primary": { "usedPercent": 40, "windowDurationMins": 10080 }, "planType": null } }),
        1200,
    )
    .unwrap()
    .unwrap();
    assert!(change.content && !change.plan);
    assert_eq!(snapshot.plan_observation(), Some(("pro".into(), 1100)));
    // The account was downgraded: a read reports another plan.
    let mut downgraded = sample_read();
    downgraded["rateLimits"]["planType"] = json!("free");
    let (snapshot, change) = apply_read_at(&dir, &account, Ok(downgraded), 1300).unwrap();
    assert!(change.content && change.plan);
    assert_eq!(snapshot.plan_observation(), Some(("free".into(), 1300)));
    // A notification carrying the plan stamps it; `unknown` is no plan.
    let (snapshot, change) = apply_notification_at(
        &dir,
        &account,
        &json!({ "rateLimits": { "primary": { "usedPercent": 41, "windowDurationMins": 10080 }, "planType": "unknown" } }),
        1400,
    )
    .unwrap()
    .unwrap();
    assert!(change.plan);
    assert_eq!(snapshot.plan_observation(), None);
    // A failed read changes nothing about the plan.
    let (_, change) = apply_read_at(&dir, &account, Err("boom".into()), 1500).unwrap();
    assert!(!change.plan);

    // Snapshots written before planTypeObservedAt existed use fetchedAt.
    let legacy = NativeQuotaSnapshot {
        fetched_at: Some(900),
        rate_limits: Some(json!({ "planType": "plus" })),
        ..Default::default()
    };
    assert_eq!(legacy.plan_observation(), Some(("plus".into(), 900)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_notification_envelopes_are_recognized() {
    use super::super::model::{EventCorrelation, NativeMessage};
    let mut envelope: EventEnvelope = serde_json::from_value(json!({
        "contractVersion": 2, "eventId": "e", "cursor": "c", "streamId": "s", "sequence": 1,
        "receivedAt": "2026-10-07T00:00:00Z", "cutexSessionId": "cutex.x", "hostId": "h",
        "source": "app_server", "sensitivity": "private",
        "correlation": serde_json::to_value(EventCorrelation::default()).unwrap(),
    }))
    .unwrap();
    envelope.native = Some(NativeMessage {
        kind: NativeMessageKind::Notification,
        message: json!({ "method": RATE_LIMITS_UPDATED_METHOD, "params": { "rateLimits": {} } }),
    });
    assert!(rate_limits_notification(&envelope).is_some());
    envelope.native.as_mut().unwrap().message["method"] = json!("thread/tokenUsage/updated");
    assert!(rate_limits_notification(&envelope).is_none());
}

#[test]
fn explicit_quota_config_takes_precedence() {
    let mut account = account("official");
    assert!(!has_explicit_quota_config(&account));
    assert!(openai_codex_profile(&account));
    account.options.quota.enabled = true;
    assert!(has_explicit_quota_config(&account));
    assert!(!native_quota_applies(&account));
    account.options.quota.enabled = false;
    let mut extra = crate::profiles::quota::Config::default();
    extra.enabled = true;
    account.options.quotas.push(extra);
    assert!(has_explicit_quota_config(&account));
    // API-key profiles never use native quota.
    assert!(!openai_codex_profile(&super::tests::account("api-key")));
}

#[test]
fn refresh_is_throttled_backed_off_and_single_flight() {
    struct Counting(std::sync::atomic::AtomicUsize, Result<Value, String>);
    impl RateLimitReader for Counting {
        fn read(&self, _: &StoredAccount) -> Result<Value, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.1.clone()
        }
    }
    let dir = temp_dir();
    let account = account("official");
    let reader = Counting(Default::default(), Ok(sample_read()));
    let (snapshot, changed) = refresh_at(&dir, &account, &reader, 1000).unwrap();
    assert!(changed.content);
    assert_eq!(snapshot.unwrap().fetched_at, Some(1000));
    // Within the forced minimum interval: served from the snapshot.
    let (_, changed) = refresh_at(&dir, &account, &reader, 1000 + MIN_FORCED_INTERVAL_SECS - 1).unwrap();
    assert!(!changed.content);
    assert_eq!(reader.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    refresh_at(&dir, &account, &reader, 1000 + MIN_FORCED_INTERVAL_SECS).unwrap();
    assert_eq!(reader.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    // Concurrent claim is refused.
    let claim = InFlight::claim(&account.id).unwrap();
    assert!(InFlight::claim(&account.id).is_none());
    assert!(in_flight(&account.id));
    let (_, changed) = refresh_at(&dir, &account, &reader, 5000).unwrap();
    assert!(!changed.content);
    assert_eq!(reader.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    drop(claim);
    assert!(!in_flight(&account.id));

    // Due logic: interval, live freshness, exponential backoff.
    assert!(refresh_due(None, 0, 300));
    let mut snapshot = NativeQuotaSnapshot {
        last_attempt_at: Some(1000),
        fetched_at: Some(1000),
        ..Default::default()
    };
    assert!(!refresh_due(Some(&snapshot), 1299, 300));
    assert!(refresh_due(Some(&snapshot), 1300, 300));
    snapshot.fetched_at = Some(1250); // live notification
    assert!(!refresh_due(Some(&snapshot), 1300, 300));
    snapshot.consecutive_failures = 2; // 4x interval
    assert!(!refresh_due(Some(&snapshot), 1000 + 1199, 300));
    assert!(refresh_due(Some(&snapshot), 1000 + 1200, 300));
    snapshot.consecutive_failures = 30; // capped at MAX_BACKOFF_SECS
    assert!(refresh_due(Some(&snapshot), 1000 + MAX_BACKOFF_SECS, 300));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recently_used_counts_launches_and_live_notifications() {
    let mut account = account("official");
    let now = 2_000_000_000;
    assert!(!recently_used(&account, None, now));
    account.last_used_at = chrono::DateTime::from_timestamp(now - 3600, 0);
    assert!(recently_used(&account, None, now));
    account.last_used_at = chrono::DateTime::from_timestamp(now - RECENT_USE_SECS - 1, 0);
    assert!(!recently_used(&account, None, now));
    let snapshot = NativeQuotaSnapshot { last_notification_at: Some(now - 60), ..Default::default() };
    assert!(recently_used(&account, Some(&snapshot), now));
}

#[test]
fn quota_updated_event_matches_the_method_schema() {
    let dir = temp_dir();
    let account = account("official");
    let (snapshot, change) = apply_read_at(&dir, &account, Ok(sample_read()), 1000).unwrap();
    let params = event_params(&snapshot, &snapshot.gauges(1001, 300), 1001, 300, change.plan);
    assert_eq!(params["gauges"][0]["id"], GAUGE_PRIMARY);
    assert_eq!(params["planChanged"], true);
    super::super::contract_validation::validate_cutex_event_message(
        &json!({ "method": PROFILE_QUOTA_UPDATED_EVENT, "params": params }),
    )
    .unwrap();
    assert!(super::super::contract_validation::validate_cutex_event_message(
        &json!({ "method": PROFILE_QUOTA_UPDATED_EVENT, "params": { "profile": {} } }),
    )
    .is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
