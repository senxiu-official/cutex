use super::*;
use crate::agent_management::ExplicitLaunchContract;
use serde_json::json;

const NOW: &str = "2026-10-09T12:00:00Z";

fn now() -> DateTime<Utc> {
    parse_time(NOW).unwrap()
}

fn days_ago(days: i64) -> String {
    (now() - chrono::Duration::days(days)).to_rfc3339()
}

fn contract(native: &str, home: &str) -> serde_json::Value {
    json!({"version":4,"native_id":native,"native_home":home,
        "bundle_manifest":"/private/manifest","bundle_sha256":"b".repeat(64)})
}

fn configuration() -> serde_json::Value {
    json!({"profile_name":"alpha","profile_id":"private","inherited":false,"profile_sha256":"c".repeat(64),
        "account_sha256":"d".repeat(64),"model":"fixture","reasoning":null,"model_provider":"private",
        "provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses",
            "requires_openai_auth":false,"supports_websockets":false},
        "sandbox":"danger-full-access","approval":"never"})
}

fn subject(session: &str) -> serde_json::Value {
    json!({"cutex_session_id":session,"formal_name":"worker","durable_sha256":"a".repeat(64),
        "authority_sha256":"a".repeat(64),"current_project_id":null,"revision":1,"runtime_generation":0})
}

struct Runtime<'a> {
    action: &'a str,
    session: &'a str,
    native: &'a str,
    stage: &'a str,
    generation: u64,
    updated_at: String,
    error: Option<&'a str>,
    agent: &'a str,
}

impl<'a> Runtime<'a> {
    fn new(action: &'a str, generation: u64, updated_at: String) -> Self {
        Self {
            action,
            session: "cutex.worker",
            native: "native-1",
            stage: "ready",
            generation,
            updated_at,
            error: None,
            agent: "stock.other",
        }
    }

    fn receipt(&self) -> ExplicitLaunchActionReceipt {
        serde_json::from_value(json!({"kind":"runtime","receipt":{
            "action_id":self.action,"stage":self.stage,"claim_id":format!("claim-{}", self.action),
            "runtime_agent_id":self.agent,"expected_generation":self.generation,"binding":null,
            "publication":null,"error":self.error,"updated_at":self.updated_at,
            "review":{"subject":subject(self.session),"contract":contract(self.native,"/private/home"),
                "configuration":configuration(),"restart":false}
        }}))
        .unwrap()
    }
}

fn activation(action: &str, native: &str, committed_at: String) -> ExplicitLaunchActionReceipt {
    serde_json::from_value(json!({"kind":"activation","receipt":{
        "action_id":action,"activated_revision":2,"committed_at":committed_at,
        "review":{"subject":subject("cutex.worker"),"contract":contract(native,"/private/home"),
            "configuration":configuration()}
    }}))
    .unwrap()
}

fn bootstrap(session: &str) -> ExplicitLaunchActionReceipt {
    serde_json::from_value(json!({"kind":"bootstrap","receipt":{
        "intent_sha256":"e".repeat(64),"native_id":"native-1","cutex_session_id":session,"revision":1
    }}))
    .unwrap()
}

fn record(id: &str, native: &str) -> CutexSessionRecord {
    let mut record = CutexSessionRecord::new_at(
        id.to_string(),
        Some(format!("thread-{id}")),
        "private".to_string(),
        "/private".to_string(),
        None,
        "2026-08-10T00:00:00Z".to_string(),
    )
    .unwrap();
    record.explicit_launch = Some(
        serde_json::from_value::<ExplicitLaunchContract>(contract(native, "/private/home"))
            .unwrap(),
    );
    record
}

fn insert(store: &mut CutexSessionStore, key: &str, receipt: ExplicitLaunchActionReceipt) {
    store
        .explicit_launch_receipts
        .insert(key.to_string(), receipt);
}

/// One session (`cutex.worker`, current native `native-1`) with a receipt
/// for every keep rule and some that should go.
fn fixture() -> CutexSessionStore {
    let mut store = CutexSessionStore::default();
    let mut worker = record("cutex.worker", "native-1");
    worker.last_runtime_agent_id = Some("stock.last".into());
    store.sessions.insert("cutex.worker".into(), worker);
    // A second session whose contract names a migration action.
    let mut migrated = record("cutex.migrated", "native-9");
    migrated
        .explicit_launch
        .as_mut()
        .unwrap()
        .migration_action_id =
        Some(crate::agent_management::AgentActionId::new("migration-action".to_string()).unwrap());
    store.sessions.insert("cutex.migrated".into(), migrated);

    insert(
        &mut store,
        "old-ready",
        Runtime::new("old-ready", 1, days_ago(30)).receipt(),
    );
    insert(
        &mut store,
        "newest-ready",
        Runtime::new("newest-ready", 4, days_ago(30)).receipt(),
    );
    insert(
        &mut store,
        "recent-ready",
        Runtime::new("recent-ready", 3, days_ago(1)).receipt(),
    );
    insert(
        &mut store,
        "failed",
        Runtime {
            error: Some("spawn failed"),
            ..Runtime::new("failed", 2, days_ago(40))
        }
        .receipt(),
    );
    insert(
        &mut store,
        "spawned",
        Runtime {
            stage: "spawned",
            ..Runtime::new("spawned", 2, days_ago(40))
        }
        .receipt(),
    );
    insert(
        &mut store,
        "abandoned",
        Runtime {
            stage: "abandoned",
            ..Runtime::new("abandoned", 2, days_ago(40))
        }
        .receipt(),
    );
    insert(
        &mut store,
        "last-occurrence",
        Runtime {
            agent: "stock.last",
            ..Runtime::new("last-occurrence", 0, days_ago(40))
        }
        .receipt(),
    );
    // Newest ready of an older native of the same session.
    insert(
        &mut store,
        "old-native",
        Runtime {
            native: "native-0",
            ..Runtime::new("old-native", 1, days_ago(40))
        }
        .receipt(),
    );
    insert(
        &mut store,
        "migration-action",
        Runtime {
            session: "cutex.migrated",
            native: "native-9",
            ..Runtime::new("migration-action", 1, days_ago(90))
        }
        .receipt(),
    );
    // Superseded by a newer ready receipt of migrated/native-9 (pruned).
    insert(
        &mut store,
        "migrated-old",
        Runtime {
            session: "cutex.migrated",
            native: "native-9",
            ..Runtime::new("migrated-old", 0, days_ago(90))
        }
        .receipt(),
    );
    insert(
        &mut store,
        "unknown-age",
        Runtime::new("unknown-age", 2, "not a time".to_string()).receipt(),
    );
    insert(
        &mut store,
        "activation-current",
        activation("activation-current", "native-1", days_ago(60)),
    );
    insert(
        &mut store,
        "activation-old",
        activation("activation-old", "native-0", days_ago(60)),
    );
    insert(
        &mut store,
        "activation-recent",
        activation("activation-recent", "native-0", days_ago(2)),
    );
    insert(&mut store, "bootstrap-adopt:x", bootstrap("cutex.worker"));
    // Receipts of a session that no longer exists: pruned even when
    // otherwise kept.
    insert(
        &mut store,
        "gone-spawned",
        Runtime {
            session: "cutex.gone",
            stage: "spawned",
            ..Runtime::new("gone-spawned", 1, days_ago(0))
        }
        .receipt(),
    );
    insert(&mut store, "gone-bootstrap", bootstrap("cutex.gone"));
    store
}

#[test]
fn every_keep_rule_and_the_pruned_rest() {
    let store = fixture();
    let plan = plan(&store, now(), 7);
    let expected = [
        ("newest-ready", KeepReason::NewestReady),
        ("recent-ready", KeepReason::Recent),
        ("failed", KeepReason::NotTerminal),
        ("spawned", KeepReason::NotTerminal),
        ("abandoned", KeepReason::NotTerminal),
        ("last-occurrence", KeepReason::LiveRuntime),
        ("old-native", KeepReason::NewestReady),
        ("migration-action", KeepReason::MigrationReference),
        ("unknown-age", KeepReason::UnknownAge),
        ("activation-current", KeepReason::CurrentActivation),
        ("activation-recent", KeepReason::Recent),
        ("bootstrap-adopt:x", KeepReason::Bootstrap),
    ];
    assert_eq!(
        plan.keep,
        expected
            .iter()
            .map(|(k, r)| (k.to_string(), *r))
            .collect::<BTreeMap<_, _>>()
    );
    let pruned: Vec<_> = plan.prune.keys().cloned().collect();
    assert_eq!(
        pruned,
        [
            "activation-old",
            "gone-bootstrap",
            "gone-spawned",
            "migrated-old",
            "old-ready"
        ]
    );
    let (tombstone, orphaned) = &plan.prune["gone-spawned"];
    assert!(*orphaned);
    assert_eq!(tombstone.kind, "runtime");
    assert_eq!(tombstone.stage.as_deref(), Some("spawned"));
    assert_eq!(tombstone.session.as_deref(), Some("cutex.gone"));
    assert_eq!(tombstone.pruned_at, "2026-10-09T12:00:00Z");
    assert_eq!(tombstone.sha256.len(), 64);
    assert!(!plan.prune["old-ready"].1);
    assert_eq!(plan.prune["activation-old"].0.stage, None);
}

#[test]
fn minimal_set_and_long_window() {
    let store = fixture();
    let minimal = plan(&store, now(), 0);
    assert!(minimal.prune.contains_key("recent-ready"));
    assert!(minimal.prune.contains_key("activation-recent"));
    assert_eq!(minimal.keep.len(), 10);
    let long = plan(&store, now(), 365);
    // The window never saves receipts of removed sessions.
    assert_eq!(
        long.prune.keys().cloned().collect::<Vec<_>>(),
        ["gone-bootstrap", "gone-spawned"]
    );
}

#[test]
fn live_occurrence_by_current_agent_claim_or_binding() {
    let mut store = fixture();
    let mut runtime = Runtime::new("live", 0, days_ago(40));
    runtime.agent = "stock.current";
    insert(&mut store, "live", runtime.receipt());
    assert!(plan(&store, now(), 7).prune.contains_key("live"));
    store
        .sessions
        .get_mut("cutex.worker")
        .unwrap()
        .current_runtime_agent_id = Some("stock.current".into());
    assert_eq!(plan(&store, now(), 7).keep["live"], KeepReason::LiveRuntime);
    let worker = store.sessions.get_mut("cutex.worker").unwrap();
    worker.current_runtime_agent_id = None;
    worker.app_server_launch_claim_id = Some("claim-live".into());
    assert_eq!(plan(&store, now(), 7).keep["live"], KeepReason::LiveRuntime);
}

#[test]
fn pruned_actions_answer_pruned_not_new() {
    let mut store = fixture();
    let report = prune(&mut store, now(), 7);
    assert_eq!(report.receipts_before, 17);
    assert_eq!(
        (report.kept, report.pruned, report.pruned_orphaned),
        (12, 5, 2)
    );
    assert_eq!(report.tombstones_added, 5);
    assert!(report.changed());
    let error = explicit_launch_receipt(&store, "old-ready")
        .unwrap_err()
        .to_string();
    assert!(error.starts_with(PRUNED_ERROR), "{error}");
    assert!(error.contains("runtime action old-ready"), "{error}");
    assert!(ensure_action_not_pruned(&store, "gone-bootstrap").is_err());
    assert!(explicit_launch_receipt(&store, "never-existed")
        .unwrap()
        .is_none());
    assert!(explicit_launch_receipt(&store, "newest-ready")
        .unwrap()
        .is_some());
}

#[test]
fn pruning_is_idempotent() {
    let mut store = fixture();
    prune(&mut store, now(), 7);
    let first = serde_json::to_value(&store).unwrap();
    let again = prune(&mut store, now(), 7);
    assert!(!again.changed(), "{again:?}");
    assert_eq!(again.pruned, 0);
    assert_eq!(serde_json::to_value(&store).unwrap(), first);
    // Later runs keep the original prune time of a tombstone.
    let later = now() + chrono::Duration::days(1);
    prune(&mut store, later, 7);
    assert_eq!(
        store.explicit_launch_tombstones["old-ready"].pruned_at,
        "2026-10-09T12:00:00Z"
    );
}

#[test]
fn tombstones_expire_and_yield_to_a_receipt() {
    let mut store = fixture();
    prune(&mut store, now(), 7);
    let count = store.explicit_launch_tombstones.len();
    let at_89 = now() + chrono::Duration::days(89);
    let report = prune(&mut store, at_89, 365);
    assert_eq!(report.tombstones_expired, 0);
    assert_eq!(store.explicit_launch_tombstones.len(), count);
    // A receipt written again under a pruned key wins over its tombstone.
    insert(
        &mut store,
        "old-ready",
        Runtime::new("old-ready", 1, days_ago(0)).receipt(),
    );
    assert!(explicit_launch_receipt(&store, "old-ready")
        .unwrap()
        .is_some());
    let report = prune(&mut store, now(), 7);
    assert_eq!(report.tombstones_superseded, 1);
    assert!(!store.explicit_launch_tombstones.contains_key("old-ready"));
    let at_91 = now() + chrono::Duration::days(91);
    let report = prune(&mut store, at_91, 365);
    assert_eq!(report.tombstones_expired, count - 1);
    assert!(store.explicit_launch_tombstones.is_empty());
}

#[test]
fn store_file_prune_writes_once_and_round_trips_tombstones() {
    let root =
        std::env::temp_dir().join(format!("cutex-receipt-retention-{}", uuid::Uuid::new_v4()));
    let path = root.join("cutex-sessions.json");
    crate::session::store::with_locked_session_store(&path, |store| {
        *store = fixture();
        crate::session::store::save_locked_session_store(&path, store)
    })
    .unwrap();
    let revision = |path: &Path| {
        crate::session::store::load_cutex_session_store_from_path(path)
            .unwrap()
            .store_revision
            .get()
    };
    assert_eq!(revision(&path), 1);
    let report = prune_store_at(&path, 7, now()).unwrap();
    assert_eq!(report.pruned, 5);
    assert_eq!(revision(&path), 2);
    let again = prune_store_at(&path, 7, now()).unwrap();
    assert_eq!(again, ReceiptPruneReport::default());
    assert_eq!(revision(&path), 2, "nothing to do: no write");
    let loaded = crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(loaded.explicit_launch_receipts.len(), 12);
    assert_eq!(loaded.explicit_launch_tombstones.len(), 5);
    let raw: serde_json::Value =
        serde_json::from_slice(&crate::session::store::session_store_json_bytes(&path).unwrap()).unwrap();
    let tombstone = &raw["explicitLaunchTombstones"]["old-ready"];
    assert_eq!(
        {
            let mut keys = tombstone
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            keys
        },
        ["kind", "prunedAt", "session", "sha256", "stage"]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stores_without_tombstones_omit_the_map_and_unknown_maps_are_tolerated() {
    // No tombstones: byte form unchanged from older cutex (no new key).
    let store = CutexSessionStore::default();
    let value = serde_json::to_value(&store).unwrap();
    assert!(value.get("explicitLaunchTombstones").is_none());
    // Older cutex parses the same struct without this field; it relies on
    // the store not denying unknown fields, which this pins.
    let mut value = serde_json::to_value(fixture()).unwrap();
    value["explicitLaunchTombstones"] =
        json!({"x": {"kind":"runtime","sha256":"0","prunedAt":NOW}});
    value["someFutureMap"] = json!({"y": 1});
    let parsed: CutexSessionStore = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.explicit_launch_tombstones.len(), 1);
}

/// Size simulation on a copy of a real store (never the live file):
/// `CUTEX_RETENTION_SIM_INPUT=<copy> cargo test --lib simulate_on_store_copy -- --ignored --nocapture`.
/// Prints counts and byte sizes only. Maintenance receipts (Linux-only
/// variant) are classified here by the same rule as on Linux.
#[test]
#[ignore]
fn simulate_on_store_copy() {
    let Ok(input) = std::env::var("CUTEX_RETENTION_SIM_INPUT") else {
        return;
    };
    let bytes = std::fs::read(&input).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let compact_len = |value: &serde_json::Value| serde_json::to_vec(value).unwrap().len();
    let pretty_len = |value: &serde_json::Value| serde_json::to_vec_pretty(value).unwrap().len();
    let receipts = original["explicit_launch_receipts"].as_object().unwrap();
    println!(
        "input: file {} B, compact {} B, pretty {} B, receipts {} (sessions {})",
        bytes.len(),
        compact_len(&original),
        pretty_len(&original),
        receipts.len(),
        original["sessions"].as_object().unwrap().len()
    );
    for days in [7u32, 0] {
        let mut value = original.clone();
        let sessions: std::collections::BTreeSet<String> = value["sessions"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let referenced: std::collections::BTreeSet<String> = value["sessions"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(|r| {
                r["explicit_launch"]["migration_action_id"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        let all = value["explicit_launch_receipts"].as_object_mut().unwrap();
        let maintenance: Vec<(String, serde_json::Value)> = all
            .iter()
            .filter(|(_, r)| r["kind"] == "maintenance")
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();
        all.retain(|_, r| r["kind"] != "maintenance");
        let mut store: CutexSessionStore = serde_json::from_value(value).unwrap();
        let report = prune(&mut store, Utc::now(), days);
        let mut after = serde_json::to_value(&store).unwrap();
        let mut maintenance_kept = 0;
        for (key, receipt) in maintenance {
            let session = receipt["receipt"]["review"]["subject"]["cutex_session_id"]
                .as_str()
                .unwrap_or("");
            let expires = receipt["receipt"]["review"]["expires_at_unix"]
                .as_i64()
                .unwrap_or(i64::MAX);
            let open = expires > Utc::now().timestamp() || receipt["receipt"]["phase"] != "applied";
            let recent =
                expires >= (Utc::now() - chrono::Duration::days(i64::from(days))).timestamp();
            if referenced.contains(&key) || (sessions.contains(session) && (open || recent)) {
                after["explicit_launch_receipts"][&key] = receipt;
                maintenance_kept += 1;
            }
        }
        let tombstones = serde_json::to_vec(&after["explicitLaunchTombstones"])
            .unwrap()
            .len();
        println!(
            "retention {days} d: receipts {} -> {} (runtime/activation/bootstrap kept {}, maintenance kept {maintenance_kept}), pruned {} ({} orphaned), compact {} B, pretty {} B, tombstones {} B",
            receipts.len(),
            after["explicit_launch_receipts"].as_object().unwrap().len(),
            report.kept,
            report.pruned,
            report.pruned_orphaned,
            compact_len(&after),
            pretty_len(&after),
            tombstones
        );
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        for reason in plan(&store, Utc::now(), days).keep.values() {
            *reasons.entry(format!("{reason:?}")).or_default() += 1;
        }
        println!("  keep reasons: {reasons:?}");
    }
}
