use super::*;
use crate::management::v2::model::{EventCorrelation, EventEnvelope, EventSource, NativeMessage, NativeMessageKind, CONTRACT_VERSION};
use crate::management::v2::usage::{record_usage_event_for_runtime_at, store, UsageLedgerEntry, UsageSample};
use serde_json::json;
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let home = PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test home"));
        assert!(home.join(".cutex-test-private-home").is_file());
        let dir = home.join(format!("usage-model-bindings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn catalog(&self) -> crate::launch::stock::VerifiedFile {
        let path = self.0.join("models.json");
        let bytes = serde_json::to_vec(&json!({"models":[
            {"slug":"gpt-6-astra"},
            {"slug":"Astra [400K]","request_model":"gpt-6-astra"},
            {"slug":"Astra [285K]","request_model":"gpt-6-astra"}
        ]})).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        crate::launch::stock::VerifiedFile {
            path,
            sha256: crate::role_revision::Sha256::new(format!("{:x}", sha2::Sha256::digest(&bytes))).unwrap(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { std::fs::remove_dir_all(&self.0).unwrap(); }
}

fn receipts(catalog: &crate::launch::stock::VerifiedFile) -> CutexSessionStore {
    let receipt = serde_json::from_value(json!({"kind":"runtime","receipt":{
        "action_id":"usage-fixture","stage":"spawned","claim_id":"fixture",
        "runtime_agent_id":"stock.usage.fixture","expected_generation":1,"binding":null,
        "error":null,"updated_at":"2026-10-10T00:00:00Z",
        "review":{"subject":{"cutex_session_id":"cutex.usage-fixture","formal_name":"worker",
            "durable_sha256":"a".repeat(64),"authority_sha256":"a".repeat(64),
            "current_project_id":null,"revision":1,"runtime_generation":0},
            "contract":{"version":4,"native_id":"thread-a","native_home":"/fixture/home",
                "bundle_manifest":"/fixture/manifest","bundle_sha256":"b".repeat(64)},
            "configuration":{
                "selected_projection":{"version":2,"route":"chatgpt_file",
                    "auth":{"path":"/fixture/auth.json","parent_device":1,"parent_inode":2,"owner":3,
                        "account":"a".repeat(64),"api_file":null},
                    "settings":{},"catalog":catalog,"requires_job":false},
                "profile_name":"fixture","profile_id":"fixture-id","inherited":false,
                "profile_sha256":"c".repeat(64),"account_sha256":"d".repeat(64),
                "model":"different-profile-default","reasoning":null,"model_provider":"openai",
                "provider":{"name":"openai","base_url":"http://127.0.0.1:1/v1","wire_api":"responses",
                    "requires_openai_auth":true,"supports_websockets":false},
                "sandbox":"danger-full-access","approval":"never"},
            "restart":false}
    }})).unwrap();
    let mut store = CutexSessionStore::default();
    store.explicit_launch_receipts.insert("usage-fixture".into(), receipt);
    store
}

fn context(store: &CutexSessionStore, model: &str) -> UsageRuntimeContext {
    let bindings = launch_bindings(store, "cutex.usage-fixture", "thread-a", 1, Some("fixture-id")).unwrap();
    UsageRuntimeContext::from_settings(&json!({"model":model,"modelProvider":"openai","serviceTier":"fast"}), bindings).unwrap()
}

fn envelope(sequence: u64, generation: u64, method: &str, params: Value) -> EventEnvelope {
    EventEnvelope {
        contract_version: CONTRACT_VERSION, event_id: format!("usage-{sequence}"), cursor: format!("stream:{sequence}"),
        stream_id: "stream".into(), sequence, received_at: "2026-10-10T01:00:00Z".into(),
        cutex_session_id: "cutex.usage-fixture".into(), host_id: "fixture".into(), source: EventSource::AppServer,
        sensitivity: "owner".into(), schema: None, cutex: None,
        correlation: EventCorrelation { runtime_generation: Some(generation), ..Default::default() },
        native: Some(NativeMessage { kind: NativeMessageKind::Notification, message: json!({"method":method,"params":params}) }),
    }
}

fn token(sequence: u64, generation: u64, count: u64) -> EventEnvelope {
    let total = json!({"totalTokens":count * 2,"inputTokens":count,"cachedInputTokens":0,
        "cacheWriteInputTokens":0,"outputTokens":count,"reasoningOutputTokens":0});
    envelope(sequence, generation, "thread/tokenUsage/updated", json!({
        "threadId":"thread-a","turnId":format!("turn-{generation}"),"tokenUsage":{"total":total,"last":total}
    }))
}

fn record(f: &Fixture, event: &EventEnvelope, context: &UsageRuntimeContext) {
    record_usage_event_for_runtime_at(&f.0.join("usage"), event, Some("fixture-id"), Some("fixture"), Some(context)).unwrap();
}

fn samples(f: &Fixture) -> Vec<UsageSample> {
    store::load_usage_ledger_at(&f.0.join("usage")).unwrap().entries.into_iter()
        .filter_map(|entry| match entry { UsageLedgerEntry::Usage(sample) => Some(*sample), _ => None }).collect()
}

#[test]
fn launch_binding_matches_occurrence_and_validates_frozen_bytes() {
    let f = Fixture::new();
    let catalog = f.catalog();
    let mut receipts = receipts(&catalog);
    assert_eq!(context(&receipts, "Astra [400K]").initial.request_model.as_deref(), Some("gpt-6-astra"));
    for (session, thread, generation, profile) in [
        ("other", "thread-a", 1, "fixture-id"),
        ("cutex.usage-fixture", "other", 1, "fixture-id"),
        ("cutex.usage-fixture", "thread-a", 2, "fixture-id"),
        ("cutex.usage-fixture", "thread-a", 1, "other"),
    ] {
        assert!(launch_bindings(&receipts, session, thread, generation, Some(profile)).unwrap().is_empty());
    }
    let ExplicitLaunchActionReceipt::Runtime(receipt) = receipts.explicit_launch_receipts.values_mut().next().unwrap() else { panic!() };
    receipt.stage = StockRuntimeStage::Ready;
    assert_eq!(context(&receipts, "Astra [285K]").initial.request_model.as_deref(), Some("gpt-6-astra"));
    std::fs::write(&catalog.path, "{}").unwrap();
    assert!(launch_bindings(&receipts, "cutex.usage-fixture", "thread-a", 1, Some("fixture-id")).is_err());
}

#[test]
fn actual_resume_selection_switches_and_history_survive_catalog_and_profile_deletion() {
    let f = Fixture::new();
    let catalog = f.catalog();
    let mut receipts = receipts(&catalog);
    let runtime = context(&receipts, "Astra [400K]");
    // Bootstrap response wins over the different profile default in the receipt.
    record(&f, &token(1, 1, 1000), &runtime);
    let switch = envelope(2, 1, "thread/settings/updated", json!({"threadId":"thread-a","threadSettings":{"model":"Astra [285K]"}}));
    record(&f, &switch, &runtime);
    // Deleting/editing the profile (and its old catalog) cannot rewrite this
    // running occurrence's binding or any already-persisted usage sample.
    std::fs::remove_file(&catalog.path).unwrap();
    receipts.explicit_launch_receipts.clear();
    record(&f, &token(3, 1, 2000), &runtime);
    let samples = samples(&f);
    assert_eq!(samples.len(), 2);
    for (sample, model) in samples.iter().zip(["Astra [400K]", "Astra [285K]"]) {
        assert_eq!(sample.attribution.model.as_deref(), Some(model));
        assert_eq!(sample.attribution.request_model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(sample.attribution.service_tier.as_deref(), Some("fast"));
        let value = serde_json::to_value(sample).unwrap();
        assert_eq!(value["requestModel"], "gpt-6-astra");
        let reloaded: UsageSample = serde_json::from_value(value).unwrap();
        assert_eq!(*sample, reloaded);
        assert!(super::super::pricing::derive_sample_estimates(&sample.attribution, &sample.tokens).api_equivalent_usd_nanos.is_some());
    }
    let mut old = serde_json::to_value(&samples[0]).unwrap();
    old.as_object_mut().unwrap().remove("requestModel");
    let old: UsageSample = serde_json::from_value(old).unwrap();
    assert_eq!(old.attribution.request_model, None);
    assert!(serde_json::to_value(old).unwrap().get("requestModel").is_none());
}

#[test]
fn new_generation_drops_old_binding_and_settings_override_bootstrap() {
    let f = Fixture::new();
    let runtime = context(&receipts(&f.catalog()), "Astra [400K]");
    record(&f, &token(1, 1, 1000), &runtime);
    // A reconnect with no observed bootstrap selection must remain unknown.
    let unknown = UsageRuntimeContext::default();
    record(&f, &token(2, 2, 2000), &unknown);
    let samples = samples(&f);
    assert_eq!(samples[1].attribution.model, None);
    assert_eq!(samples[1].attribution.request_model, None);
    let switch = envelope(3, 3, "thread/settings/updated", json!({"threadId":"thread-a","threadSettings":{"model":"Astra [285K]"}}));
    record(&f, &switch, &runtime);
    record(&f, &token(4, 3, 3000), &runtime);
    let samples = self::samples(&f);
    assert_eq!(samples[2].attribution.model.as_deref(), Some("Astra [285K]"));
    assert_eq!(samples[2].attribution.request_model.as_deref(), Some("gpt-6-astra"));
}

#[test]
fn reroute_and_explicit_base_switch_clear_variant_binding() {
    let f = Fixture::new();
    let runtime = context(&receipts(&f.catalog()), "Astra [400K]");
    record(&f, &token(1, 1, 1000), &runtime);
    let reroute = envelope(2, 1, "model/rerouted", json!({"threadId":"thread-a","turnId":"turn-1","toModel":"gpt-5.6-terra"}));
    record(&f, &reroute, &runtime);
    record(&f, &token(3, 1, 2000), &runtime);
    assert_eq!(samples(&f)[1].attribution.model.as_deref(), Some("gpt-5.6-terra"));
    assert_eq!(samples(&f)[1].attribution.request_model, None);
    let switch = envelope(4, 1, "thread/settings/updated", json!({"threadId":"thread-a","threadSettings":{"model":"gpt-6-astra"}}));
    record(&f, &switch, &runtime);
    let mut event = token(5, 1, 3000);
    event.native.as_mut().unwrap().message["params"]["turnId"] = json!("base-turn");
    record(&f, &event, &runtime);
    assert_eq!(samples(&f)[2].attribution.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(samples(&f)[2].attribution.request_model, None);
}

#[test]
fn reconnect_same_generation_uses_new_actual_bootstrap_selection() {
    let f = Fixture::new();
    let receipts = receipts(&f.catalog());
    let initial = context(&receipts, "Astra [400K]");
    record(&f, &token(1, 1, 1000), &initial);
    let resumed = context(&receipts, "Astra [285K]");
    record(&f, &token(2, 1, 2000), &resumed);
    let samples = samples(&f);
    assert_eq!(samples[1].attribution.model.as_deref(), Some("Astra [285K]"));
    assert_eq!(samples[1].attribution.request_model.as_deref(), Some("gpt-6-astra"));
}

#[test]
fn active_turn_keeps_its_selection_through_settings_and_reconnect() {
    let f = Fixture::new();
    let receipts = receipts(&f.catalog());
    let runtime = context(&receipts, "Astra [400K]");
    let start = envelope(1, 1, "turn/started", json!({"threadId":"thread-a","turn":{"id":"turn-1"}}));
    record(&f, &start, &runtime);
    let switch = envelope(2, 1, "thread/settings/updated", json!({"threadId":"thread-a","threadSettings":{"model":"gpt-5.6-terra"}}));
    record(&f, &switch, &runtime);
    // Replayed start must not reset the frozen attribution to the new selection.
    record(&f, &start, &runtime);
    let duplicate = envelope(3, 1, "turn/started", json!({"threadId":"thread-a","turn":{"id":"turn-1"}}));
    record(&f, &duplicate, &runtime);
    let resumed = context(&receipts, "gpt-5.6-terra");
    record(&f, &token(4, 1, 1000), &resumed);
    let first = &samples(&f)[0];
    assert_eq!(first.attribution.model.as_deref(), Some("Astra [400K]"));
    assert_eq!(first.attribution.request_model.as_deref(), Some("gpt-6-astra"));
    let next = envelope(5, 1, "turn/started", json!({"threadId":"thread-a","turn":{"id":"turn-2"}}));
    record(&f, &next, &resumed);
    let mut next_token = token(6, 1, 2000);
    next_token.native.as_mut().unwrap().message["params"]["turnId"] = json!("turn-2");
    record(&f, &next_token, &resumed);
    let second = &samples(&f)[1];
    assert_eq!(second.attribution.model.as_deref(), Some("gpt-5.6-terra"));
    assert_eq!(second.attribution.request_model, None);
}

#[test]
fn claude_adapter_learns_actual_model_after_turn_started() {
    use crate::management::v2::model::{AppServerSchema, AppServerSchemaChannel};
    let f = Fixture::new();
    let events = [
        token(1, 1, 0),
        envelope(2, 1, "thread/settings/updated", json!({"threadId":"thread-a","threadSettings":{"model":"sonnet","modelProvider":"anthropic"}})),
        envelope(3, 1, "turn/started", json!({"threadId":"thread-a","turn":{"id":"turn-1"}})),
        envelope(4, 1, "thread/settings/updated", json!({"threadId":"thread-a","threadSettings":{"model":"claude-sonnet-4-5-20250929"}})),
        token(5, 1, 1000),
    ];
    for mut event in events {
        event.schema = Some(AppServerSchema {
            protocol: "claude-app-server-subset".into(), major_version: 1,
            version: "fixture".into(), sha256: "a".repeat(64),
            channel: AppServerSchemaChannel::Experimental,
            capabilities: json!({}), extensions: vec!["claude-runtime-v1".into()],
        });
        record_usage_event_for_runtime_at(&f.0.join("usage"), &event, Some("fixture-id"), Some("fixture"), None).unwrap();
    }
    let samples = samples(&f);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].attribution.model.as_deref(), Some("claude-sonnet-4-5-20250929"));
    assert_eq!(samples[0].attribution.request_model, None);
}
