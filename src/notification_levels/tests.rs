//! Notification levels: registry / presets / patch unit tests, the default
//! tables over every current event source (owner_notify, cutex notices,
//! native requests, owner approvals, delegations, phone delivery), and the
//! one-time migration away from the removed `legacy` presets.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use super::*;
use crate::config::notification_levels::NotificationLevelSettings;
use crate::config::owner_notify::OwnerNotifySettings;
use crate::management::v2::summary::{self, NeedSources};
use crate::owner_notify::store::{
    Level as AgentLevel, NewNotification, Origin, OwnerNotification, OwnerNotifyState, State,
};

fn at(minutes: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-09T10:00:00Z").unwrap().with_timezone(&Utc) + Duration::minutes(minutes)
}

fn new(agent: &str, level: AgentLevel, needs_reply: bool) -> NewNotification {
    NewNotification {
        agent_id: agent.into(),
        agent_name: None,
        text: "hello".into(),
        level,
        needs_reply,
        delegation: None,
        job: None,
        turn_id: None,
    }
}

fn patched(value: Value) -> NotificationLevelSettings {
    NotificationLevelSettings::default()
        .patched(&value, &Registry::builtin(), &OwnerNotifySettings::default())
        .unwrap()
}

fn evaluator(settings: &NotificationLevelSettings, owner: &OwnerNotifySettings) -> Evaluator {
    Evaluator::new(Arc::new(Registry::builtin()), settings, owner)
}

// ---------------------------------------------------------------- r216 reference

/// cutex r216 `summary::ask_need_kind`, verbatim.
fn r216_ask_need_kind(ask: &Value) -> Option<&'static str> {
    if !ask["closed"].is_null() {
        return None;
    }
    let execution = ask["execution"].as_str().unwrap_or_default();
    if matches!(execution, "failed" | "rejected" | "cancelled") {
        return None;
    }
    if ask["ownerQuestions"].as_u64().unwrap_or(0) > 0 {
        return Some("question");
    }
    match execution {
        "pendingReview" => Some("delegationReview"),
        "interrupted" if ask.pointer("/attempt/interruptReason").and_then(Value::as_str) == Some("outcomeUnknown") => {
            Some("delegationReview")
        }
        "answered"
            if ask["review"].as_str() == Some("awaitingAcceptance")
                && ask.pointer("/requester/kind").and_then(Value::as_str) == Some("owner") =>
        {
            Some("acceptance")
        }
        _ => None,
    }
}

fn owner_variants() -> Vec<OwnerNotifySettings> {
    let mut variants = Vec::new();
    for delivery in [
        None,
        Some((vec![], vec!["desktop", "phone"], vec!["desktop", "phone", "bigAlert"])),
        Some((vec!["phone"], vec![], vec!["bigAlert", "desktop"])),
        Some((vec!["desktop", "phone", "bigAlert"], vec!["phone"], vec![])),
        Some((vec![], vec![], vec![])),
    ] {
        for sound in [true, false] {
            for (per_hour, urgent_per_hour) in [(12, 3), (3, 1), (2, 0)] {
                for overrides in [None, Some("mute"), Some("force")] {
                    let mut owner = OwnerNotifySettings::default();
                    if let Some((info, attention, urgent)) = &delivery {
                        owner.delivery.info = info.iter().map(|s| s.to_string()).collect();
                        owner.delivery.attention = attention.iter().map(|s| s.to_string()).collect();
                        owner.delivery.urgent = urgent.iter().map(|s| s.to_string()).collect();
                    }
                    owner.big_alert_sound = sound;
                    owner.rate_limit.per_hour = per_hour;
                    owner.rate_limit.urgent_per_hour = urgent_per_hour;
                    if let Some(mode) = overrides {
                        owner.agent_overrides.insert("cutex.a".into(), mode.into());
                    }
                    variants.push(owner);
                }
            }
        }
    }
    variants
}

fn with_state(mut record: OwnerNotification, state: State) -> OwnerNotification {
    record.state = state;
    record
}

// ---------------------------------------------------------------- default tables

#[test]
fn system_notices_stay_records_under_default() {
    for owner in owner_variants() {
        let evaluator = evaluator(&NotificationLevelSettings::default(), &owner);
        let mut state = OwnerNotifyState::default();
        for kind in [Some("quarantine"), Some("deliveryFailed"), Some("idleDelegation"), Some("preActivationSkipped"), None, Some("x")] {
            let record = state.create_decided(
                new("cutex.a", AgentLevel::Info, false),
                Origin::System,
                kind,
                &owner,
                &evaluator,
                Some(None),
                at(0),
            );
            // Level info, no channel, not muted, no downgrade.
            assert_eq!((record.level, record.channels.is_empty(), record.muted, record.downgraded.is_none()), (AgentLevel::Info, true, false, true));
            assert_eq!(record.notification_level, Some(Level::Record));
            assert_eq!(record.system_kind.as_deref(), kind);
            for s in [State::Open, State::Acknowledged] {
                let record = with_state(record.clone(), s);
                assert!(!record.pending() && !record.alerting() && !record.to_phone());
                assert!(summary::message_to_owner("cutex.a", &record.to_json()).is_none());
            }
        }
    }
}

#[test]
fn asks_need_kinds_differ_from_r216_only_as_designed() {
    let mut checked = 0;
    for closed in [Value::Null, json!("2026-10-09T00:00:00Z")] {
        for execution in ["queued", "running", "pendingReview", "interrupted", "answered", "failed", "rejected", "cancelled", "other"] {
            for questions in [0, 2] {
                for review in [Value::Null, json!("awaitingAcceptance"), json!("accepted")] {
                    for requester in ["owner", "agent", "external"] {
                        for reason in [Value::Null, json!("outcomeUnknown"), json!("runtimeLost")] {
                            let ask = json!({
                                "askId": "ask-1", "target": "cutex.t", "closed": closed, "execution": execution,
                                "ownerQuestions": questions, "review": review, "requester": {"kind": requester},
                                "attempt": {"interruptReason": reason}, "createdAt": "2026-10-09T00:00:00Z",
                            });
                            let expected = r216_ask_need_kind(&ask);
                            // Default tables: only owner work can need the owner
                            // (agent-requested outcome-unknown drops out), and an
                            // owner's failed delegation is a need.
                            let sources = NeedSources { asks: vec![ask.clone()], ..Default::default() };
                            let needs = summary::needs_by_agent(&sources);
                            let got = needs.get("cutex.t").map(|n| n[0]["kind"].as_str().unwrap());
                            match got {
                                Some("delegationFailed") => {
                                    assert!(expected.is_none() && matches!(execution, "failed" | "rejected" | "cancelled") && requester == "owner", "{ask}")
                                }
                                Some(kind) => assert_eq!(Some(kind), expected, "{ask}"),
                                None => assert!(
                                    expected.is_none() || (execution == "interrupted" && requester != "owner"),
                                    "{ask}"
                                ),
                            }
                            assert_eq!(summary::ask_need_kind(&ask), got, "{ask}");
                            if got.is_some() {
                                assert_eq!(needs["cutex.t"][0]["level"], "needsYou");
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 900);
}

#[test]
fn native_requests_and_owner_approvals_always_need_the_owner() {
    let methods = [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/permissions/requestApproval",
        "item/tool/requestUserInput",
        "mcpServer/elicitation/request",
        "some/future/request",
    ];
    let mut owner = OwnerNotifySettings::default();
    // Even with every channel off and the agent muted (a mute keeps the need).
    owner.delivery.info.clear();
    owner.delivery.attention.clear();
    owner.delivery.urgent.clear();
    owner.agent_overrides.insert("cutex.a".into(), "mute".into());
    let sources = NeedSources {
        native: methods
            .iter()
            .enumerate()
            .map(|(id, method)| ("cutex.a".to_string(), json!({"id": id, "method": method, "params": {}}), "2026-10-09T00:00:00Z".to_string()))
            .collect(),
        approvals: vec![json!({"approvalId": "apr-1", "requesterCutexSessionId": "cutex.a", "summary": "deploy", "createdAt": "2026-10-09T00:00:01Z"})],
        evaluator: evaluator(&NotificationLevelSettings::default(), &owner),
        ..Default::default()
    };
    let needs = summary::needs_by_agent(&sources);
    let kinds: Vec<&str> = needs["cutex.a"].iter().map(|n| n["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.len(), methods.len() + 1);
    assert_eq!(kinds.iter().filter(|k| **k == "approval").count(), 4);
    assert_eq!(kinds.iter().filter(|k| **k == "question").count(), 2);
    assert!(kinds.contains(&"ownerApproval"));
    assert!(needs["cutex.a"].iter().all(|n| n["level"] == "needsYou"));
}

#[test]
fn default_is_the_default_and_unchanged_settings_stay_out_of_config() {
    let config = crate::profiles::model::CodezConfig::default();
    assert!(config.notification_levels.is_default());
    let saved = serde_json::to_value(&config).unwrap();
    assert!(saved.get("notification_levels").is_none());
    let evaluator = Evaluator::default();
    assert_eq!(evaluator.event_preset(), DEFAULT_PRESET);
    assert_eq!(evaluator.matrix(), &Registry::builtin().preset_matrix(DEFAULT_PRESET).unwrap());
    // Older config (no key, or the removed legacy) reads as default.
    let old: NotificationLevelSettings =
        serde_json::from_value(json!({"eventPreset": "legacy", "methodPreset": "legacy"})).unwrap();
    let resolved = old.resolve(&Registry::builtin(), &OwnerNotifySettings::default());
    assert_eq!((resolved.event_preset.as_str(), resolved.method_preset.as_str()), ("default", "default"));
    assert_eq!(old.view()["eventPreset"], "default");
    let mut custom = crate::profiles::model::CodezConfig::default();
    custom.notification_levels = patched(json!({"methods": {"preset": "quiet"}}));
    let saved = serde_json::to_value(&custom).unwrap();
    assert_eq!(saved["notification_levels"], json!({"methodPreset": "quiet"}));
    // Choosing default stores it explicitly (older binaries read it too).
    custom.notification_levels = patched(json!({"methods": {"preset": "default"}, "events": {"preset": "default"}}));
    let saved = serde_json::to_value(&custom).unwrap();
    assert_eq!(saved["notification_levels"], json!({"eventPreset": "default", "methodPreset": "default"}));
}

#[test]
fn legacy_migration_stores_default_once_and_keeps_cells() {
    use crate::config::notification_levels::{run_legacy_migration_at, LEGACY_MIGRATION_ARCHIVE};
    let root = std::env::temp_dir().join(format!("cutex-nl-migration-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.json");
    let before = json!({
        "agent_bus_port": 24260,
        "future_field": [1],
        "owner_notify": {"delivery": {"info": [], "attention": ["phone"], "urgent": ["bigAlert"]}, "big_alert_sound": false},
        "notification_levels": {"eventPreset": "default", "eventLevels": {"ownerNotify.info": "inform"},
                                "methodPreset": "legacy", "methods": {"inform": {"phone": true}}},
    });
    std::fs::write(&config, serde_json::to_vec(&before).unwrap()).unwrap();
    let written = run_legacy_migration_at(&root, at(0)).unwrap().unwrap();
    assert_eq!(written, ["methodPreset"]);
    let after: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(after["future_field"], json!([1]), "unknown fields kept");
    assert_eq!(after["notification_levels"]["methodPreset"], "default");
    assert_eq!(after["notification_levels"]["eventPreset"], "default");
    assert_eq!(after["notification_levels"]["methods"], json!({"inform": {"phone": true}}));
    assert_eq!(after["notification_levels"]["eventLevels"], json!({"ownerNotify.info": "inform"}));
    let record: Value =
        serde_json::from_slice(&std::fs::read(root.join(LEGACY_MIGRATION_ARCHIVE).join("migration.json")).unwrap()).unwrap();
    assert_eq!(record["previousNotificationLevels"], before["notification_levels"]);
    assert_eq!(record["previousOwnerNotify"], before["owner_notify"]);
    assert_eq!(record["written"], json!(["methodPreset"]));
    // Once: a legacy written later (older binary after a rollback) is not migrated again
    // by the record, but still reads as default.
    let mut again = after.clone();
    again["notification_levels"]["methodPreset"] = json!("legacy");
    std::fs::write(&config, serde_json::to_vec(&again).unwrap()).unwrap();
    assert_eq!(run_legacy_migration_at(&root, at(1)).unwrap(), None);
    let kept: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(kept["notification_levels"]["methodPreset"], "legacy");
    let settings: NotificationLevelSettings = serde_json::from_value(kept["notification_levels"].clone()).unwrap();
    assert_eq!(settings.resolve(&Registry::builtin(), &OwnerNotifySettings::default()).method_preset, "default");
    let _ = std::fs::remove_dir_all(&root);

    // A home that never chose (absent block) gets both stored; no config = record only.
    let root = std::env::temp_dir().join(format!("cutex-nl-migration-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.json"), b"{\"agent_bus_port\": 24260}").unwrap();
    assert_eq!(run_legacy_migration_at(&root, at(0)).unwrap().unwrap(), ["eventPreset", "methodPreset"]);
    let after: Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).unwrap()).unwrap();
    assert_eq!(after["notification_levels"], json!({"eventPreset": "default", "methodPreset": "default"}));
    let parsed: crate::profiles::model::CodezConfig = serde_json::from_value(after).unwrap();
    assert_eq!(parsed.notification_levels.event_preset.as_deref(), Some("default"));
    let _ = std::fs::remove_dir_all(&root);
    let root = std::env::temp_dir().join(format!("cutex-nl-migration-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    assert_eq!(run_legacy_migration_at(&root, at(0)).unwrap().unwrap(), Vec::<String>::new());
    assert!(!root.join("config.json").exists());
    assert!(root.join(LEGACY_MIGRATION_ARCHIVE).join("migration.json").exists());
    // A broken config is left alone and retried.
    let _ = std::fs::remove_dir_all(&root);
    let root = std::env::temp_dir().join(format!("cutex-nl-migration-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.json"), b"{ broken").unwrap();
    assert!(run_legacy_migration_at(&root, at(0)).is_err());
    assert!(!root.join(LEGACY_MIGRATION_ARCHIVE).join("migration.json").exists());
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------- registry / presets

#[test]
fn builtin_registry_is_consistent() {
    let registry = Registry::builtin();
    let levels: Vec<&str> = registry.levels.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(levels, ["urgent", "needsYou", "inform", "record"]);
    let methods: Vec<&str> = registry.methods.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(methods, ["needsYou", "mark", "desktop", "phone", "sound", "bigAlert"]);
    let mut ids = std::collections::BTreeSet::new();
    for event in &registry.events {
        assert!(valid_event_id(&event.id), "{}", event.id);
        assert!(ids.insert(event.id.clone()), "duplicate {}", event.id);
        assert!(event.levels.contains_key("default") && !event.levels.contains_key(LEGACY), "{}", event.id);
        assert!(event.title.contains_key("en") && event.title.contains_key("zh-CN"), "{}", event.id);
        assert!(!event.applied_by.is_empty(), "{}", event.id);
    }
    let events: Vec<&str> = registry.event_presets.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(events, ["default"]);
    let methods_ids: Vec<&str> = registry.method_presets.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(methods_ids, ["quiet", "default", "all"]);
    for preset in &registry.method_presets {
        let matrix = registry.preset_matrix(&preset.id).unwrap();
        for (level, row) in &registry.fixed_cells {
            for (method, value) in row {
                assert_eq!(matrix[level][method], *value, "{} {level}.{method}", preset.id);
            }
        }
        assert_eq!(matrix.len(), 4);
        assert!(matrix.values().all(|row| row.len() == methods.len()));
    }
}

#[test]
fn method_presets_match_the_design_tables() {
    let registry = Registry::builtin();
    let row = |preset: &str, level: &str| -> Vec<&'static str> {
        let matrix = registry.preset_matrix(preset).unwrap();
        ["needsYou", "mark", "desktop", "phone", "sound", "bigAlert"]
            .into_iter()
            .filter(|method| matrix[level][*method])
            .collect()
    };
    // notification-levels.md §4.2.
    assert_eq!(row("quiet", "urgent"), ["needsYou", "mark", "desktop", "phone", "sound"]);
    assert_eq!(row("quiet", "needsYou"), ["needsYou", "mark"]);
    assert_eq!(row("quiet", "inform"), ["mark"]);
    assert!(row("quiet", "record").is_empty());
    assert_eq!(row("default", "urgent"), ["needsYou", "mark", "desktop", "phone", "sound", "bigAlert"]);
    assert_eq!(row("default", "needsYou"), ["needsYou", "mark", "desktop", "phone"]);
    assert_eq!(row("default", "inform"), ["mark", "desktop"]);
    assert!(row("default", "record").is_empty());
    assert_eq!(row("all", "needsYou"), ["needsYou", "mark", "desktop", "phone", "sound"]);
    assert_eq!(row("all", "inform"), ["needsYou", "mark", "desktop", "phone"]);
    assert_eq!(row("all", "record"), ["mark"]);
    assert!(registry.preset_matrix(LEGACY).is_none(), "legacy removed");
}

#[test]
fn recommended_event_preset_matches_section_2_1() {
    let registry = Registry::builtin();
    let expect = [
        ("ownerNotify.urgent", Level::Urgent),
        ("native.approval", Level::NeedsYou),
        ("native.question", Level::NeedsYou),
        ("ownerApproval.pending", Level::NeedsYou),
        ("delegation.ownerQuestion", Level::NeedsYou),
        ("delegation.pendingReview", Level::NeedsYou),
        ("delegation.awaitingAcceptance.owner", Level::NeedsYou),
        ("delegation.outcomeUnknown.owner", Level::NeedsYou),
        ("delegation.failed.owner", Level::NeedsYou),
        ("ownerNotify.needsReply", Level::NeedsYou),
        ("turn.owner.failed", Level::NeedsYou),
        ("turn.owner.completed", Level::Inform),
        ("ownerNotify.attention", Level::Inform),
        ("runtime.error", Level::Inform),
        ("runtime.error.ownerTurn", Level::Urgent),
        ("job.completed.owner", Level::Inform),
        ("turn.other.completed", Level::Record),
        ("turn.owner.interrupted", Level::Record),
        ("delegation.outcomeUnknown.agent", Level::Record),
        ("delegation.awaitingAcceptance.agent", Level::Record),
        ("delegation.failed.agent", Level::Record),
        ("ownerNotify.info", Level::Record),
        ("ownerNotify.rateLimited", Level::Record),
        ("system.quarantine", Level::Record),
        ("system.deliveryFailed", Level::Record),
        ("system.idleDelegation", Level::Record),
        ("delegation.deliveryFailed", Level::Record),
        ("agentBus.messageFailed", Level::Record),
        ("job.completed.agent", Level::Record),
    ];
    for (event, level) in expect {
        assert_eq!(registry.event(event).unwrap().level_in("default"), level, "{event}");
    }
}

#[test]
fn modules_contribute_events_additively() {
    let mut registry = Registry::builtin();
    let spec: EventSpec = serde_json::from_value(json!({
        "id": "waveline.reminderDue", "module": "waveline", "appliedBy": ["client"],
        "title": {"en": "Reminder due", "zh-CN": "提醒到期"},
        "levels": {"default": "inform"},
    }))
    .unwrap();
    registry.add_event(spec.clone()).unwrap();
    assert_eq!(registry.add_event(spec.clone()).unwrap_err(), "event waveline.reminderDue is already registered");
    let event = registry.event("waveline.reminderDue").unwrap();
    assert_eq!(event.level_in("default"), Level::Inform);
    // A contribution written for older cutex: its legacy level is dropped.
    let older: EventSpec = serde_json::from_value(json!({
        "id": "waveline.older", "module": "waveline", "title": {"en": "Older"},
        "levels": {"default": "record", "legacy": "urgent"},
    }))
    .unwrap();
    registry.add_event(older).unwrap();
    assert_eq!(registry.event("waveline.older").unwrap().levels, BTreeMap::from([("default".to_string(), Level::Record)]));
    for bad in [
        json!({"id": "NoDot", "module": "m", "title": {"en": "x"}, "levels": {"default": "record"}}),
        json!({"id": "m.x", "module": "", "title": {"en": "x"}, "levels": {"default": "record"}}),
        json!({"id": "m.y", "module": "m", "title": {"en": "x"}, "levels": {"legacy": "record"}}),
        json!({"id": "m.z", "module": "m", "title": {"en": "x"}, "levels": {"default": "record", "loud": "urgent"}}),
        json!({"id": "m.w", "module": "m", "title": {"en": "x"}, "levels": {"default": "record"}, "appliedBy": ["phone"]}),
    ] {
        let spec: EventSpec = serde_json::from_value(bad.clone()).unwrap();
        assert!(registry.add_event(spec).is_err(), "{bad}");
    }
    // Data files: one event, a list, and a broken file (skipped, reported).
    let dir = std::env::temp_dir().join(format!("cutex-nl-events-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), r#"{"id":"plugin.one","module":"plugin","title":{"en":"One"},"levels":{"default":"needsYou"}}"#).unwrap();
    std::fs::write(dir.join("b.json"), r#"{"events":[{"id":"plugin.two","module":"plugin","title":{"en":"Two"},"levels":{"default":"record"}},{"id":"plugin.one","module":"plugin","title":{"en":"Dup"},"levels":{"default":"record"}}]}"#).unwrap();
    std::fs::write(dir.join("c.json"), "{not json").unwrap();
    std::fs::write(dir.join("ignored.txt"), "x").unwrap();
    let mut registry = Registry::builtin();
    let errors = load_contributed_events(&mut registry, &dir);
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert_eq!(registry.event("plugin.one").unwrap().level_in("default"), Level::NeedsYou);
    assert!(registry.event("plugin.two").is_some());
    let _ = std::fs::remove_dir_all(dir);
    // A stored level for an event registered later is kept and then applies.
    let mut settings = NotificationLevelSettings::default();
    settings.event_levels.insert("plugin.three".into(), "urgent".into());
    let owner = OwnerNotifySettings::default();
    assert_eq!(Evaluator::new(Arc::new(Registry::builtin()), &settings, &owner).level("plugin.three"), Level::Record);
    let mut later = Registry::builtin();
    later
        .add_event(serde_json::from_value(json!({"id": "plugin.three", "module": "plugin", "title": {"en": "3"}, "levels": {"default": "record"}})).unwrap())
        .unwrap();
    assert_eq!(Evaluator::new(Arc::new(later), &settings, &owner).level("plugin.three"), Level::Urgent);
    let view = view(&Registry::builtin(), &settings, &owner);
    assert_eq!(view["unregisteredEventLevels"], json!({"plugin.three": "urgent"}));
}

// ---------------------------------------------------------------- settings / patch

#[test]
fn patch_presets_cells_and_effective_preset_names() {
    let registry = Registry::builtin();
    let owner = OwnerNotifySettings::default();
    let base = NotificationLevelSettings::default();
    let resolved = base.resolve(&registry, &owner);
    assert_eq!((resolved.event_preset_effective.as_str(), resolved.method_preset_effective.as_str()), ("default", "default"));

    let next = base.patched(&json!({"events": {"preset": "default"}, "methods": {"preset": "default"}}), &registry, &owner).unwrap();
    assert_eq!(next.event_preset.as_deref(), Some("default"));
    assert_eq!(next.method_preset.as_deref(), Some("default"));
    // A cell equal to the preset is not stored; a different one is.
    let next = next
        .patched(&json!({"events": {"levels": {"ownerNotify.info": "record", "ownerNotify.attention": "needsYou"}},
                         "methods": {"cells": {"inform": {"desktop": true, "phone": true}, "urgent": {"needsYou": false}}}}),
            &registry, &owner)
        .unwrap();
    assert_eq!(next.event_levels, BTreeMap::from([("ownerNotify.attention".to_string(), "needsYou".to_string())]));
    assert_eq!(next.methods, BTreeMap::from([("inform".to_string(), BTreeMap::from([("phone".to_string(), true)]))]));
    let resolved = next.resolve(&registry, &owner);
    assert_eq!(resolved.event_preset_effective, "custom");
    assert_eq!(resolved.method_preset_effective, "custom");
    assert!(resolved.matrix["urgent"]["needsYou"], "fixed cell stays on");
    // Back to the preset: null removes; a preset clears changed cells.
    let back = next
        .patched(&json!({"events": {"levels": {"ownerNotify.attention": null}}, "methods": {"cells": {"inform": {"phone": null}}}}), &registry, &owner)
        .unwrap();
    let resolved = back.resolve(&registry, &owner);
    assert_eq!((resolved.event_preset_effective.as_str(), resolved.method_preset_effective.as_str()), ("default", "default"));
    assert!(back.methods.is_empty());
    let reset = next.patched(&json!({"methods": {"preset": "quiet"}}), &registry, &owner).unwrap();
    assert!(reset.methods.is_empty());
    assert_eq!(reset.event_levels.len(), 1, "the events table is untouched");
    // Cells that make the matrix equal another preset report that preset.
    let to_all = NotificationLevelSettings::default()
        .patched(&json!({"methods": {"preset": "default", "cells": {
            "needsYou": {"sound": true}, "inform": {"needsYou": true, "phone": true}, "record": {"mark": true}}}}), &registry, &owner)
        .unwrap();
    assert_eq!(to_all.resolve(&registry, &owner).method_preset_effective, "all");
    // legacy is no longer a preset.
    assert!(to_all.patched(&json!({"methods": {"preset": "legacy"}}), &registry, &owner).is_err());
    assert!(to_all.patched(&json!({"events": {"preset": "legacy"}}), &registry, &owner).is_err());
    // Project mutes merge per key.
    let muted = to_all.patched(&json!({"projectOverrides": {"proj-a": "mute", "__independent__": "mute"}}), &registry, &owner).unwrap();
    let muted = muted.patched(&json!({"projectOverrides": {"proj-a": null}}), &registry, &owner).unwrap();
    assert_eq!(muted.project_overrides, BTreeMap::from([("__independent__".to_string(), "mute".to_string())]));
}

#[test]
fn patch_refuses_bad_input() {
    let registry = Registry::builtin();
    let owner = OwnerNotifySettings::default();
    for bad in [
        json!([]),
        json!({"other": 1}),
        json!({"events": {"preset": "loud"}}),
        json!({"events": {"preset": "legacy"}}),
        json!({"methods": {"preset": "legacy"}}),
        json!({"events": {"preset": 1}}),
        json!({"events": {"levels": {"no.such": "record"}}}),
        json!({"events": {"levels": {"ownerNotify.info": "loud"}}}),
        json!({"events": {"levels": {"ownerNotify.info": 3}}}),
        json!({"events": {"x": 1}}),
        json!({"methods": {"preset": "loud"}}),
        json!({"methods": {"cells": {"loud": {"desktop": true}}}}),
        json!({"methods": {"cells": {"inform": {"fax": true}}}}),
        json!({"methods": {"cells": {"inform": {"desktop": "yes"}}}}),
        json!({"methods": {"cells": []}}),
        json!({"projectOverrides": {"p": "force"}}),
        json!({"projectOverrides": {"": "mute"}}),
    ] {
        assert!(NotificationLevelSettings::default().patched(&bad, &registry, &owner).is_err(), "{bad}");
    }
    // Hand edits that are no longer valid fall back silently.
    let mut edited = NotificationLevelSettings::default();
    edited.event_preset = Some("gone".into());
    edited.method_preset = Some("gone".into());
    edited.event_levels.insert("ownerNotify.info".into(), "loud".into());
    edited.project_overrides.insert("p".into(), "force".into());
    let resolved = edited.resolve(&registry, &owner);
    assert_eq!((resolved.event_preset.as_str(), resolved.method_preset.as_str()), ("default", "default"));
    assert_eq!(resolved.event_levels["ownerNotify.info"], Level::Record);
    assert!(resolved.project_overrides.is_empty());
}

#[test]
fn settings_route_accepts_notification_levels() {
    use crate::management::v2::settings_api;
    let view = settings_api::settings_view(&crate::profiles::model::CodezConfig::default());
    assert_eq!(view["notificationLevels"]["eventPreset"], "default");
    assert_eq!(view["notificationLevels"]["effectiveMethodPreset"], "default");
    let mut config = crate::profiles::model::CodezConfig::default();
    config.notification_levels = patched(json!({"events": {"preset": "default"}, "methods": {"cells": {"inform": {"phone": true}}}}));
    let view = settings_api::settings_view(&config);
    assert_eq!(view["notificationLevels"]["effectiveEventPreset"], "default");
    assert_eq!(view["notificationLevels"]["effectiveMethodPreset"], "custom");
    assert_eq!(view["notificationLevels"]["methods"], json!({"inform": {"phone": true}}));
}

// ---------------------------------------------------------------- presets behaviour

#[test]
fn recommended_presets_change_owner_notify_as_designed() {
    let owner = OwnerNotifySettings::default();
    let settings = patched(json!({"events": {"preset": "default"}, "methods": {"preset": "default"}}));
    let evaluator = evaluator(&settings, &owner);
    let mut state = OwnerNotifyState::default();
    let create = |state: &mut OwnerNotifyState, level, reply, origin, kind| {
        state.create_decided(new("cutex.a", level, reply), origin, kind, &owner, &evaluator, None, at(0))
    };
    // info → record: timeline only.
    let info = create(&mut state, AgentLevel::Info, false, Origin::Agent, None);
    assert_eq!((info.notification_level, info.channels.clone()), (Some(Level::Record), vec![]));
    assert!(!info.pending() && info.alerting() && !info.to_phone());
    // attention → inform: desktop toast, no need.
    let attention = create(&mut state, AgentLevel::Attention, false, Origin::Agent, None);
    assert_eq!((attention.notification_level, attention.channels.clone()), (Some(Level::Inform), vec!["desktop".to_string()]));
    assert!(!attention.pending());
    // needs reply → needsYou (desktop + phone) and pending.
    let ask = create(&mut state, AgentLevel::Info, true, Origin::Agent, None);
    assert_eq!(ask.notification_event.as_deref(), Some("ownerNotify.needsReply"));
    assert_eq!((ask.notification_level, ask.channels.clone()), (Some(Level::NeedsYou), vec!["desktop".to_string(), "phone".to_string()]));
    assert!(ask.pending() && ask.to_phone());
    // urgent wins over needsReply.
    let urgent = create(&mut state, AgentLevel::Urgent, true, Origin::Agent, None);
    assert_eq!(urgent.notification_level, Some(Level::Urgent));
    assert_eq!(urgent.channels, vec!["desktop", "phone", "bigAlert"]);
    assert!(urgent.actions.as_ref().unwrap().contains(&"sound".to_string()));
    // A cutex notice raised by the owner reaches the desktop and counts.
    let raised = patched(json!({"events": {"preset": "default", "levels": {"system.idleDelegation": "needsYou"}}, "methods": {"preset": "default"}}));
    let raised = Evaluator::new(Arc::new(Registry::builtin()), &raised, &owner);
    let notice = state.create_decided(new("cutex.a", AgentLevel::Info, false), Origin::System, Some("idleDelegation"), &owner, &raised, None, at(1));
    assert_eq!(notice.notification_level, Some(Level::NeedsYou));
    assert!(notice.pending() && notice.alerting() && notice.to_phone());
    assert!(summary::message_to_owner("cutex.a", &notice.to_json()).is_some());
    // Still not replyable.
    assert_eq!(state.check_reply("cutex.a", &notice.notification_id).unwrap_err().code, "notification_not_replyable");
}

#[test]
fn mutes_keep_the_need_and_drop_the_noise() {
    let mut owner = OwnerNotifySettings::default();
    owner.agent_overrides.insert("cutex.a".into(), "mute".into());
    let settings = patched(json!({"methods": {"preset": "all"}, "projectOverrides": {"proj": "mute"},
        "events": {"levels": {"ownerNotify.attention": "needsYou"}}}));
    let evaluator = evaluator(&settings, &owner);
    // Agent mute: urgent keeps needsYou / mark, loses toast, phone, sound, big alert.
    let decision = evaluator.decide(&["ownerNotify.urgent"], Scope { agent: Some("cutex.a"), project: None });
    assert!(decision.muted && decision.channels.is_empty());
    assert_eq!(decision.actions, ["needsYou", "mark"]);
    // Inform becomes record under a mute.
    let decision = evaluator.decide(&["ownerNotify.info"], Scope { agent: Some("cutex.a"), project: None });
    assert_eq!(decision.level, Level::Record);
    // Project mute (and only for that project).
    let decision = evaluator.decide(&["ownerNotify.attention"], Scope { agent: Some("cutex.b"), project: Some(Some("proj")) });
    assert!(decision.muted && decision.needs_you() && decision.channels.is_empty());
    let decision = evaluator.decide(&["ownerNotify.attention"], Scope { agent: Some("cutex.b"), project: Some(Some("other")) });
    assert!(!decision.muted && decision.channels == ["desktop", "phone"]);
    // A muted needs-reply message is still pending (as in r216).
    let mut state = OwnerNotifyState::default();
    let record = state.create_decided(new("cutex.a", AgentLevel::Attention, true), Origin::Agent, None, &owner, &evaluator, None, at(0));
    assert!(record.muted && record.pending());
    let record = state.create_decided(new("cutex.a", AgentLevel::Urgent, false), Origin::Agent, None, &owner, &evaluator, None, at(1));
    assert!(record.muted && record.pending(), "a mute keeps the need");
}

#[test]
fn recommended_presets_change_summary_needs_as_designed() {
    let settings = patched(json!({"events": {"preset": "default"}}));
    let ask = |execution: &str, requester: &str, extra: Value| {
        let mut ask = json!({"askId": format!("ask-{execution}-{requester}"), "target": "cutex.t", "closed": null,
            "execution": execution, "requester": {"kind": requester}, "createdAt": "2026-10-09T00:00:00Z"});
        for (key, value) in extra.as_object().unwrap() {
            ask[key] = value.clone();
        }
        ask
    };
    let outcome_unknown = json!({"attempt": {"interruptReason": "outcomeUnknown"}});
    let sources = NeedSources {
        asks: vec![
            ask("interrupted", "agent", outcome_unknown.clone()),
            ask("interrupted", "owner", outcome_unknown),
            ask("failed", "owner", json!({})),
            ask("failed", "agent", json!({})),
            ask("running", "agent", json!({"ownerQuestions": 1})),
        ],
        evaluator: evaluator(&settings, &OwnerNotifySettings::default()),
        ..Default::default()
    };
    let needs = summary::needs_by_agent(&sources);
    let mut got: Vec<(String, String)> = needs["cutex.t"]
        .iter()
        .map(|n| (n["id"].as_str().unwrap().to_string(), n["kind"].as_str().unwrap().to_string()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("ask-failed-owner".to_string(), "delegationFailed".to_string()),
            ("ask-interrupted-owner".to_string(), "delegationReview".to_string()),
            ("ask-running-agent".to_string(), "question".to_string()),
        ]
    );
    // Project mute never removes a need (needsYou stays).
    let muted = patched(json!({"events": {"preset": "default"}, "projectOverrides": {"__independent__": "mute"}}));
    let sources = NeedSources {
        asks: vec![ask("pendingReview", "agent", json!({}))],
        evaluator: evaluator(&muted, &OwnerNotifySettings::default()),
        projects_known: true,
        ..Default::default()
    };
    assert_eq!(summary::needs_by_agent(&sources)["cutex.t"].len(), 1);
    // Lowering an event to record removes it.
    let lowered = patched(json!({"events": {"levels": {"native.question": "record"}}}));
    let sources = NeedSources {
        native: vec![("cutex.a".into(), json!({"id": 1, "method": "item/tool/requestUserInput", "params": {}}), "2026-10-09T00:00:00Z".into())],
        evaluator: evaluator(&lowered, &OwnerNotifySettings::default()),
        ..Default::default()
    };
    assert!(summary::needs_by_agent(&sources).is_empty());
}

#[test]
fn the_matrix_decides_whatever_the_owner_notify_delivery() {
    let mut owner = OwnerNotifySettings::default();
    owner.delivery.info.clear();
    owner.delivery.attention.clear();
    // Default: info is a record; attention informs on the desktop.
    let defaults = evaluator(&NotificationLevelSettings::default(), &owner);
    let decision = defaults.decide(&["ownerNotify.info"], Scope::default());
    assert!(!decision.needs_you() && decision.channels.is_empty());
    let decision = defaults.decide(&["ownerNotify.attention"], Scope::default());
    assert!(!decision.needs_you());
    assert_eq!(decision.channels, ["desktop"]);
    let all = evaluator(&patched(json!({"methods": {"preset": "all"}})), &owner);
    let decision = all.decide(&["ownerNotify.attention"], Scope::default());
    assert!(decision.needs_you());
    assert_eq!(decision.channels, ["desktop", "phone"]);
    // Older records (no actions) keep the r216 rule whatever the settings.
    let mut state = OwnerNotifyState::default();
    let mut old = state.create(new("cutex.a", AgentLevel::Attention, false), &OwnerNotifySettings::default(), at(0));
    old.actions = None;
    old.notification_level = None;
    assert!(old.pending());
    assert_eq!(old.effective_notification_level(), Level::NeedsYou);
    old.channels.clear();
    assert!(!old.pending());
}

/// r219 removed the Task Service events. Owner settings saved by r218 that
/// override them still load; the ids are kept (reported as unregistered) and
/// do not change any registered event's level.
#[test]
fn saved_levels_for_removed_task_events_are_kept_and_ignored() {
    let registry = Registry::builtin();
    for id in ["task.reviewReady", "task.closed"] {
        assert!(registry.event(id).is_none(), "{id}");
    }
    assert!(!registry.events.iter().any(|event| event.module == "taskService"));
    let settings: NotificationLevelSettings = serde_json::from_value(json!({
        "eventPreset": "default",
        "eventLevels": {"task.reviewReady": "urgent", "task.closed": "needsYou", "job.completed.agent": "inform"},
    }))
    .unwrap();
    let owner = OwnerNotifySettings::default();
    let evaluator = Evaluator::new(Arc::new(Registry::builtin()), &settings, &owner);
    assert_eq!(evaluator.level("task.reviewReady"), Level::Record);
    assert_eq!(evaluator.level("job.completed.agent"), Level::Inform);
    let view = view(&registry, &settings, &owner);
    assert_eq!(
        view["unregisteredEventLevels"],
        json!({"task.closed": "needsYou", "task.reviewReady": "urgent"})
    );
    let saved = serde_json::to_value(&settings).unwrap();
    assert_eq!(saved["eventLevels"]["task.closed"], "needsYou");
}
