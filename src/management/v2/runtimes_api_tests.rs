use std::path::Path;

use serde_json::json;

use super::*;
use crate::session::model::{CutexSessionRecord, CutexSessionStore};

const SHA_R50: &str = "cff0b90dc1a2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_R49: &str = "b49b49b49b49bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_DEV: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

#[test]
fn scratch_registry_and_old_pins_remain_visible_after_managed_selection() {
    let root=std::env::temp_dir().join(format!("managed-view-{}",uuid::Uuid::new_v4()));
    let managed_root=root.join("runtime/native");
    let old=root.join("agent-tmp/Kittiwake/native-r58/codex-app-server").to_string_lossy().into_owned();
    let new=managed_root.join("0.164.0-hash/codex-app-server").to_string_lossy().into_owned();
    let mut registry=registry();
    registry.runtime_paths.clear();
    registry.runtime_paths.insert(old.clone(),registry.runtime_artifacts[SHA_R50].clone());
    registry.runtime_paths.insert(new.clone(),registry.runtime_artifacts[SHA_R50].clone());
    let before=registry.clone();
    let mut sessions=CutexSessionStore::default();
    sessions.sessions.insert("old".into(),pinned("old","external","/old-bundle"));
    sessions.sessions.insert("new".into(),pinned("new","managed","/new-bundle"));
    let read=|path:&Path|Some(PinnedExecutable{sha256:SHA_R50.into(),path:if path==Path::new("/old-bundle"){old.clone()}else{new.clone()}});
    let built=build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha,sessions:&sessions,registry:Some(&registry),
        selected_native:Some(PinnedExecutable{sha256:SHA_R50.into(),path:new.clone()}),
        selected_claude_adapter:None,read_manifest:&read,exists:&|_|true,managed_native_root:Some(&managed_root)});
    let entry=built.native.iter().find(|e|e.selected).unwrap();
    assert_eq!(entry.selected_managed,Some(true));
    assert_eq!(entry.external_agent_count,1);assert_eq!(entry.managed_agent_count,1);
    assert_eq!(entry.agents.iter().find(|a|a.cutex_session_id=="old").unwrap().external_runtime_path,Some(true));
    assert_eq!(entry.agents.iter().find(|a|a.cutex_session_id=="new").unwrap().external_runtime_path,Some(false));
    assert_eq!(registry,before);assert!(!root.exists(),"status must not create the managed cache");
}

fn record(id: &str, name: &str) -> CutexSessionRecord {
    let mut record = CutexSessionRecord::new_at(
        id.into(),
        None,
        "host-a".into(),
        "/work".into(),
        None,
        "2026-10-09T00:00:00Z".into(),
    )
    .unwrap();
    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    record.formal_agent_name = Some(name.into());
    record
}

fn pinned(id: &str, name: &str, manifest: &str) -> CutexSessionRecord {
    let mut record = record(id, name);
    record.explicit_launch = Some(
        serde_json::from_value(json!({
            "version": 4, "native_id": "0189aaaa-0000-7000-8000-000000000001",
            "native_home": "/home/u/.cutex/native", "bundle_manifest": manifest,
            "bundle_sha256": "e".repeat(64),
        }))
        .unwrap(),
    );
    record
}

fn claude(id: &str, name: &str, runtime_dir: &str) -> CutexSessionRecord {
    let mut record = record(id, name);
    record.claude_session = Some(
        serde_json::from_value(json!({
            "deployment": {"python": format!("{runtime_dir}/python"), "adapter_source": format!("{runtime_dir}/cute-claude"),
                "claude_cli": format!("{runtime_dir}/claude"), "claude_home": "/home/u/.cutex/claude-home",
                "tui_binary": format!("{runtime_dir}/tui")},
            "thread_id": "t", "state_dir": "/state", "model": "sonnet"
        }))
        .unwrap(),
    );
    record
}

fn roster(sessions: &CutexSessionStore) -> HashSet<String> {
    sessions.sessions.keys().cloned().collect()
}

#[test]
fn host_summary_never_hashes_or_stats_historical_registry_paths() {
    let sessions = CutexSessionStore::default(); let registry = registry();
    let built = build_inventory(&Inputs {sessions:&sessions,managed_agent_ids:&HashSet::new(),
        executable_sha256:&|_|panic!("summary must not request disk verification"),registry:Some(&registry),
        selected_native:Some(PinnedExecutable {sha256:SHA_R49.into(),path:"/selected/exe".into()}),
        selected_claude_adapter:None,read_manifest:&manifests,
        exists:&|path| {assert_eq!(path,"/selected/exe");true},managed_native_root:None},InventoryMode::Summary);
    let summary = summary_from(&built);
    assert_eq!(summary["native"]["selected"],"0.160.0-r49");
    assert_eq!(summary["native"]["latest"],"0.160.0-r50");
    assert_host_summary_schema(summary);
}

#[test]
fn cold_inventory_omits_unverified_history_and_preserves_selected_missing_state() {
    let sessions = CutexSessionStore::default(); let registry = registry();
    let built = build_inventory(&Inputs {sessions:&sessions,managed_agent_ids:&HashSet::new(),
        executable_sha256:&|_|None,registry:Some(&registry),selected_native:None,
        selected_claude_adapter:None,read_manifest:&manifests,
        exists:&|_|panic!("pending inventory must not stat historical files on the request"),managed_native_root:None},InventoryMode::Pending);
    assert!(built.native.is_empty());
    let built = build_inventory(&Inputs {sessions:&sessions,managed_agent_ids:&HashSet::new(),
        executable_sha256:&|_|None,registry:Some(&registry),
        selected_native:Some(PinnedExecutable {sha256:SHA_R50.into(),path:"/selected/missing".into()}),
        selected_claude_adapter:None,read_manifest:&manifests,
        exists:&|path|{assert_eq!(path,"/selected/missing");false},managed_native_root:None},InventoryMode::Pending);
    assert_eq!(built.native.len(),1);
    assert!(built.native[0].selected && built.native[0].missing && built.native[0].verification_pending);
}

#[test]
fn unavailable_path_does_not_mark_verified_rows_pending_or_add_historical_artifacts() {
    let sessions = CutexSessionStore::default(); let mut registry = registry();
    let mut extra = registry.runtime_artifacts[SHA_R50].clone(); extra.version="0.180.0-r99".into();
    registry.runtime_artifacts.insert(SHA_DEV.into(),extra.clone());
    registry.runtime_paths.insert("D:/unavailable/codex.exe".into(),extra);
    let built = build_inventory(&Inputs {sessions:&sessions,managed_agent_ids:&HashSet::new(),
        executable_sha256:&|path| if path.contains("native-r49") {Some(SHA_R49.into())} else {None},
        registry:Some(&registry),selected_native:None,selected_claude_adapter:None,read_manifest:&manifests,
        exists:&|_|panic!("no foreground historical IO"),managed_native_root:None},InventoryMode::Pending);
    assert_eq!(built.native.len(),1);
    assert_eq!(built.native[0].version.as_deref(),Some("0.160.0-r49"));
    assert!(!built.native[0].missing && !built.native[0].verification_pending);
}

#[test]
fn verified_unknown_selected_build_does_not_keep_a_pending_flag() {
    let sessions = CutexSessionStore::default(); let registry = registry();
    let built = build_inventory(&Inputs {sessions:&sessions,managed_agent_ids:&HashSet::new(),
        executable_sha256:&|_|Some(SHA_DEV.into()),registry:Some(&registry),
        selected_native:Some(PinnedExecutable {sha256:SHA_DEV.into(),path:"/selected/dev".into()}),
        selected_claude_adapter:None,read_manifest:&manifests,exists:&|_|true,managed_native_root:None},InventoryMode::Pending);
    let selected = built.native.iter().find(|entry|entry.selected).unwrap();
    assert_eq!(selected.release,"unknown");
    assert!(selected.version.is_none());
    assert!(!selected.verification_pending);
}

#[test]
fn unavailable_roster_preserves_runtime_entries_and_totals_without_claiming_split_counts() {
    let mut sessions = CutexSessionStore::default();
    sessions.sessions.insert("pinned".into(), pinned("pinned", "agent", "/r50"));
    let mut hidden = pinned("hidden", "hidden", "/r50");
    hidden.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
    sessions.sessions.insert("hidden".into(), hidden);
    let (ids, available) = roster_or_session_ids(&sessions, Err(anyhow::anyhow!("store unavailable")));
    assert!(!available);
    assert_eq!(ids, HashSet::from(["pinned".to_string()]));
    let registry = registry();
    let built = build(&Inputs { sessions: &sessions, managed_agent_ids: &ids,
        executable_sha256: &registry_path_sha, registry: Some(&registry),
        selected_native: Some(PinnedExecutable { sha256: SHA_R50.into(), path: "/r50/exe".into() }),
        selected_claude_adapter: None, read_manifest: &manifests, exists: &|_| true, managed_native_root: None });
    let wire = entries_wire(&built.native, available);
    assert_eq!(wire.as_array().unwrap().len(), built.native.len());
    assert_eq!(built.native.iter().map(|entry| entry.agent_count).sum::<usize>(), 1);
    for entry in wire.as_array().unwrap() {
        assert!(entry.get("activeAgentCount").is_none());
        assert!(entry.get("archivedAgentCount").is_none());
        assert!(entry.get("agentCount").is_some());
    }
    assert_host_summary_schema(summary_from(&built));
    let (_, available) = roster_or_session_ids(&sessions, Ok(HashSet::new()));
    assert!(available);
    assert!(entries_wire(&built.native, available)[0].get("activeAgentCount").is_some());
}

fn registry_path_sha(path: &str) -> Option<String> {
    Some(if path.contains("native-r49") { SHA_R49 } else { SHA_R50 }.into())
}

fn registry() -> NativeVersionRegistry {
    serde_json::from_value(json!({
        "schema": crate::launch::version_registry::REGISTRY_SCHEMA,
        "latest_cli": {"version": "0.160.0-r50", "recorded_at": "2026-10-08T00:00Z"},
        "latest_runtime": {"version": "0.160.0-r50", "recorded_at": "2026-10-08T00:00Z"},
        "runtime_artifacts": {
            SHA_R50: {"version": "0.160.0-r50", "recorded_at": "2026-10-08T00:00Z"},
            SHA_R49: {"version": "0.160.0-r49", "recorded_at": "2026-10-07T00:00Z"},
        },
        "runtime_paths": {
            "D:/Apps/cutex/releases/r216-desktop-20261009/codex.exe": {"version": "0.160.0-r50", "recorded_at": "2026-10-08T00:00Z"},
            "D:/Apps/cutex/releases/native-r49-20261007/codex.exe": {"version": "0.160.0-r49", "recorded_at": "2026-10-07T00:00Z"},
        },
    }))
    .unwrap()
}

#[test]
fn selected_native_version_requires_its_exact_sha() {
    let sessions = CutexSessionStore::default();
    let registry = registry();
    let exists = |_: &str| true;
    // The selected binary may be older or newer than registry.latest_runtime.
    // Neither its path nor the latest record identifies these bytes.
    for (sha, expected) in [(SHA_R49, Some("0.160.0-r49")), (SHA_DEV, None)] {
        let built = build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha,
        managed_native_root: None,
            sessions: &sessions, registry: Some(&registry),
            selected_native: Some(PinnedExecutable { sha256: sha.into(), path: "D:/Apps/r58/codex.exe".into() }),
            selected_claude_adapter: None, read_manifest: &manifests, exists: &exists,
        });
        let selected: Vec<_> = built.native.iter().filter(|entry| entry.selected).collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].version.as_deref(), expected);
        assert_eq!(selected[0].sha256_prefix.as_deref(), Some(&sha[..12]));
        assert!(!selected[0].latest);
        let summary = summary_from(&built);
        assert_eq!(summary["native"]["selected"], json!(expected));
        assert_eq!(summary["native"]["selectedSha256Prefix"], &sha[..12]);
        assert_eq!(summary["native"]["latest"], "0.160.0-r50");
        assert_host_summary_schema(summary);
    }
}

#[test]
fn missing_registry_keeps_selected_sha_unknown() {
    let sessions = CutexSessionStore::default();
    let built = build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha,
        managed_native_root: None,
        sessions: &sessions, registry: None,
        selected_native: Some(PinnedExecutable { sha256: SHA_R50.into(), path: "D:/Apps/r50/codex.exe".into() }),
        selected_claude_adapter: None, read_manifest: &manifests, exists: &|_| true,
    });
    assert_eq!(built.native.len(), 1);
    assert!(built.native[0].selected);
    assert!(built.native[0].version.is_none());
    let summary = summary_from(&built);
    assert!(summary["native"]["selected"].is_null());
    assert!(summary["native"]["latest"].is_null());
    assert_eq!(summary["native"]["selectedSha256Prefix"], &SHA_R50[..12]);
    assert_host_summary_schema(summary);
}

fn assert_host_summary_schema(summary: serde_json::Value) {
    let value = json!({
        "schema": "cutex/host/v1", "hostId": "host-a", "instanceId": "ci-test",
        "platform": "windows", "arch": "x86_64", "osVersion": null,
        "cutexBuild": crate::build_info::build_json(), "runtimes": summary,
    });
    crate::management::v2::schema_export::assert_valid("host", "host", &value);
}

#[test]
fn absent_selected_deployment_does_not_select_latest_registry_entry() {
    let sessions = CutexSessionStore::default();
    let registry = registry();
    let built = build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha,
        managed_native_root: None,
        sessions: &sessions, registry: Some(&registry), selected_native: None,
        selected_claude_adapter: None, read_manifest: &manifests, exists: &|_| true,
    });
    assert!(built.native.iter().all(|entry| !entry.selected));
    let summary = summary_from(&built);
    assert!(summary["native"]["selected"].is_null());
    assert!(summary["native"]["selectedSha256Prefix"].is_null());
    assert_eq!(summary["native"]["latest"], "0.160.0-r50");
}

fn manifests(path: &Path) -> Option<PinnedExecutable> {
    let path = path.to_string_lossy();
    let (sha, exe) = if path.contains("r216") {
        (SHA_R50, "D:/Apps/cutex/releases/r216-desktop-20261009/codex.exe")
    } else if path.contains("r207") {
        (SHA_R49, "D:/Apps/cutex/releases/native-r49-20261007/codex.exe")
    } else if path.contains("dev") {
        (SHA_DEV, "D:/build/r300-dev/codex.exe")
    } else {
        return None;
    };
    Some(PinnedExecutable { sha256: sha.into(), path: exe.into() })
}

#[test]
fn runtimes_group_agents_by_native_version_and_claude_runtime() {
    let mut sessions = CutexSessionStore::default();
    for record in [
        pinned("cutex.a", "Alpha", "D:/Apps/cutex/releases/r216-desktop-20261009/bundle.json"),
        pinned("cutex.b", "Beta", "D:/Apps/cutex/releases/r207-desktop/bundle.json"),
        pinned("cutex.c", "Gamma", "D:/Apps/cutex/releases/r216-desktop-20261009/bundle.json"),
        pinned("cutex.d", "Dev", "D:/build/dev/bundle.json"),
        pinned("cutex.e", "Gone", "D:/Apps/cutex/releases/r190-old/bundle.json"),
        record("cutex.f", "Unpinned"),
        claude("cutex.g", "Claude A", "D:/Apps/cutex/runtimes/claude-r4"),
        claude("cutex.h", "Claude B", "D:/Apps/cutex/runtimes/claude-r5"),
    ] {
        sessions.sessions.insert(record.cutex_session_id.clone(), record);
    }
    let mut retired = pinned("cutex.z", "Retired", "D:/Apps/cutex/releases/r216-desktop-20261009/bundle.json");
    retired.archive_state = crate::session::model::CutexSessionArchiveState::Retired;
    sessions.sessions.insert("cutex.z".into(), retired);

    let registry = registry();
    let exists = |path: &str| !path.contains("native-r49");
    let built = build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha,
        managed_native_root: None,
        sessions: &sessions,
        registry: Some(&registry),
        selected_native: Some(PinnedExecutable {
            sha256: SHA_R50.into(),
            path: "D:/Apps/cutex/releases/r216-desktop-20261009/codex.exe".into(),
        }),
        selected_claude_adapter: Some("D:/Apps/cutex/runtimes/claude-r5/cute-claude".into()),
        read_manifest: &manifests,
        exists: &exists,
    });
    let native = serde_json::to_value(&built.native).unwrap();
    // Newest registry version first, then unregistered / unreadable pins.
    assert_eq!(native[0]["release"], "r50");
    assert_eq!(native[0]["version"], "0.160.0-r50");
    assert_eq!(native[0]["path"], "r216-desktop-20261009/codex.exe");
    assert_eq!(native[0]["releaseDir"], "r216-desktop-20261009");
    assert_eq!(native[0]["sha256Prefix"], "cff0b90dc1a2");
    assert_eq!(native[0]["selected"], true);
    assert_eq!(native[0]["latest"], true);
    assert_eq!(native[0]["missing"], false);
    assert_eq!(native[0]["agentCount"], 3);
    assert_eq!(native[0]["activeAgentCount"], 2);
    assert_eq!(native[0]["archivedAgentCount"], 1);
    assert_eq!(
        native[0]["agents"],
        json!([{"cutexSessionId": "cutex.a", "name": "Alpha"}, {"cutexSessionId": "cutex.c", "name": "Gamma"},
            {"cutexSessionId": "cutex.z", "name": "Retired"}])
    );
    assert_eq!(native[1]["release"], "r49");
    assert_eq!(native[1]["selected"], false);
    assert!(native[1].get("latest").is_none());
    assert_eq!(native[1]["missing"], true);
    assert_eq!(native[1]["agents"], json!([{"cutexSessionId": "cutex.b", "name": "Beta"}]));
    let unregistered = native.as_array().unwrap().iter().find(|e| e["sha256Prefix"] == "dddddddddddd").unwrap();
    assert!(unregistered.get("version").is_none());
    assert_eq!(unregistered["release"], "unknown");
    assert_eq!(unregistered["releaseDir"], "r300-dev");
    assert_eq!(unregistered["agentCount"], 1);
    let unreadable = native.as_array().unwrap().iter().find(|e| e["releaseDir"] == "r190-old").unwrap();
    assert_eq!(unreadable["missing"], true);
    assert_eq!(unreadable["agents"][0]["name"], "Gone");
    assert_eq!(built.unpinned_agent_count, 1);

    let claude = serde_json::to_value(&built.claude).unwrap();
    assert_eq!(claude[0]["release"], "claude-r5");
    assert_eq!(claude[0]["selected"], true);
    assert_eq!(claude[0]["path"], "cute-claude");
    assert_eq!(claude[0]["agentCount"], 1);
    assert_eq!(claude[1]["release"], "claude-r4");
    assert_eq!(claude[1]["selected"], false);
    assert_eq!(claude[1]["agents"][0]["cutexSessionId"], "cutex.g");

    // Nothing private: no full paths, no claude_home.
    let text = serde_json::to_string(&json!({"native": native, "claude": claude})).unwrap();
    assert!(!text.contains("D:/"), "{text}");
    assert!(!text.contains("claude-home"), "{text}");

    // /v2/host `runtimes`: selection + counts only (no names, no paths).
    let summary = summary_from(&built);
    assert_eq!(summary["native"]["selected"], "0.160.0-r50");
    assert_eq!(summary["native"]["latest"], "0.160.0-r50");
    assert_eq!(summary["native"]["inUse"][0], json!({"release": "r50", "version": "0.160.0-r50", "agents": 3, "selected": true, "missing": false}));
    assert_eq!(summary["native"]["inUse"].as_array().unwrap().iter().map(|u| u["agents"].as_u64().unwrap()).sum::<u64>(), 6);
    assert_eq!(summary["claude"]["selected"], "claude-r5");
    assert_eq!(summary["claude"]["inUse"][1], json!({"release": "claude-r4", "version": null, "agents": 1, "selected": false, "missing": false}));
    assert_eq!(summary["unpinnedAgents"], 1);
    let text = summary.to_string();
    assert!(!text.contains("Alpha") && !text.contains("D:/"), "{text}");
}

#[test]
fn deleted_unreferenced_native_artifacts_are_not_listed_as_installed() {
    let sessions = CutexSessionStore::default();
    let registry = registry();
    let built=build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha, managed_native_root:None,sessions:&sessions,registry:Some(&registry),
        selected_native:None,selected_claude_adapter:None,read_manifest:&manifests,exists:&|_|false });
    assert!(built.native.is_empty());
}

#[test]
fn agent_lists_are_capped_but_counted() {
    let mut sessions = CutexSessionStore::default();
    for index in 0..(MAX_AGENTS + 5) {
        let record = pinned(
            &format!("cutex.{index:03}"),
            &format!("Agent {index:03}"),
            "D:/Apps/cutex/releases/r216-desktop-20261009/bundle.json",
        );
        sessions.sessions.insert(record.cutex_session_id.clone(), record);
    }
    let registry = registry();
    let exists = |_: &str| true;
    let built = build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha,
        managed_native_root: None,
        sessions: &sessions,
        registry: Some(&registry),
        selected_native: None,
        selected_claude_adapter: None,
        read_manifest: &manifests,
        exists: &exists,
    });
    let r50 = &built.native[0];
    assert!(!r50.selected, "registry latest is not evidence of a selected deployment");
    assert_eq!(r50.agent_count, MAX_AGENTS + 5);
    assert_eq!(r50.agents.len(), MAX_AGENTS);
    assert_eq!(r50.active_agent_count, MAX_AGENTS + 5);
    assert_eq!(r50.archived_agent_count, 0);
    assert!(built.claude.is_empty());
}

#[test]
fn labels_and_paths() {
    assert_eq!(claude_runtime_label("/opt/cutex/runtimes/claude-r12/cute-claude"), "claude-r12");
    assert_eq!(claude_runtime_label("C:\\x\\claude-rX\\adapter"), "adapter");
    assert_eq!(file_name("D:\\a\\b\\codex.exe").as_deref(), Some("codex.exe"));
    assert!(is_runtimes_path("/v2/runtimes"));
    assert!(!is_runtimes_path("/v2/runtimes-x"));
    assert!(!is_runtimes_path("/v2/runtime-packages"));
}

#[test]
fn resource_matches_its_schema() {
    let mut sessions = CutexSessionStore::default();
    let record = pinned("cutex.a", "Alpha", "D:/Apps/cutex/releases/r216-desktop-20261009/bundle.json");
    sessions.sessions.insert(record.cutex_session_id.clone(), record);
    let registry = registry();
    let exists = |_: &str| true;
    let built = build(&Inputs { managed_agent_ids: &roster(&sessions), executable_sha256: &registry_path_sha,
        managed_native_root: None,
        sessions: &sessions,
        registry: Some(&registry),
        selected_native: None,
        selected_claude_adapter: Some("/opt/runtimes/claude-r5/cute-claude".into()),
        read_manifest: &manifests,
        exists: &exists,
    });
    let value = json!({
        "contractVersion": 2,
        "hostId": "host-a",
        "generatedAt": "2026-10-10T00:00:00Z",
        "cutex": {"build": crate::build_info::build_json(), "release": "r218", "releaseDir": "r218-desktop", "executable": "cutex.exe"},
        "native": built.native,
        "claude": built.claude,
        "unpinnedAgentCount": built.unpinned_agent_count,
    });
    crate::management::v2::schema_export::assert_valid("runtimes", "runtimes", &value);
}

#[test]
fn counts_use_managed_roster_and_archive_state_before_list_cap() {
    let mut sessions = CutexSessionStore::default();
    for index in 0..(MAX_AGENTS + 8) {
        let mut record = pinned(&format!("managed.{index}"), "Managed", "/r216/bundle");
        if index % 2 == 0 { record.archive_state = crate::session::model::CutexSessionArchiveState::Retired; }
        record.agent_enabled = false; // Offline/disabled does not mean archived.
        sessions.sessions.insert(record.cutex_session_id.clone(), record);
    }
    let ids = roster(&sessions);
    let mut hidden = pinned("hidden", "Hidden", "/r216/bundle");
    hidden.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
    sessions.sessions.insert("hidden".into(), hidden);
    sessions.sessions.insert("orphan".into(), pinned("orphan", "Orphan", "/r216/bundle"));
    sessions.sessions.insert("human".into(), record("human", "Human"));
    let mut hidden_claude = claude("hidden-claude", "Hidden Claude", "/claude-r5");
    hidden_claude.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
    sessions.sessions.insert("hidden-claude".into(), hidden_claude);
    let mut ids = ids;
    ids.insert("hidden".into());
    ids.insert("hidden-claude".into());
    let built = build(&Inputs {
        sessions: &sessions, managed_agent_ids: &ids, executable_sha256: &registry_path_sha,
        registry: None, selected_native: None, selected_claude_adapter: None,
        read_manifest: &manifests, exists: &|_| true, managed_native_root: None,
    });
    let entry = &built.native[0];
    assert_eq!(entry.agent_count, MAX_AGENTS + 8);
    assert_eq!(entry.active_agent_count, (MAX_AGENTS + 8) / 2);
    assert_eq!(entry.archived_agent_count, (MAX_AGENTS + 8) / 2);
    assert_eq!(entry.agents.len(), MAX_AGENTS);
    assert_eq!(built.unpinned_agent_count, 0);
    assert!(built.claude.is_empty());
}

#[test]
fn same_version_distinct_builds_and_stale_path_labels_stay_separate() {
    let sessions = CutexSessionStore::default();
    let mut registry = registry();
    registry.runtime_artifacts.get_mut(SHA_R49).unwrap().version = "0.160.0-r50".into();
    // r177 path claims r50; actual bytes are the distinct older build.
    registry.runtime_paths.clear();
    registry.runtime_paths.insert("D:/r177/codex-app-server.exe".into(), registry.latest_runtime.clone());
    let built = build(&Inputs {
        sessions: &sessions, managed_agent_ids: &HashSet::new(),
        executable_sha256: &|_| Some(SHA_R49.into()), registry: Some(&registry),
        selected_native: Some(PinnedExecutable {sha256: SHA_R50.into(), path: "D:/r286/codex-app-server.exe".into()}),
        selected_claude_adapter: None, read_manifest: &manifests, exists: &|_| true, managed_native_root: None,
    });
    assert_eq!(built.native.len(), 2);
    let old = built.native.iter().find(|e| !e.selected).unwrap();
    assert_eq!(old.sha256_prefix.as_deref(), Some(&SHA_R49[..12]));
    assert_eq!(old.release_dir.as_deref(), Some("r177"));
    assert!(built.native.iter().all(|e| e.version.as_deref() == Some("0.160.0-r50")));
    // Unregistered replacement bytes cannot inherit a version from their path.
    let built = build(&Inputs {
        sessions: &sessions, managed_agent_ids: &HashSet::new(), executable_sha256: &|_| Some(SHA_DEV.into()),
        registry: Some(&registry), selected_native: Some(PinnedExecutable {sha256: SHA_DEV.into(), path: "D:/r177/codex-app-server.exe".into()}),
        selected_claude_adapter: None, read_manifest: &manifests, exists: &|_| true, managed_native_root: None,
    });
    assert_eq!(built.native.len(), 1);
    assert!(built.native[0].version.is_none());
}

#[test]
fn claude_counts_include_archived_and_offline_beyond_display_limit() {
    let mut sessions = CutexSessionStore::default();
    for index in 0..(MAX_AGENTS + 7) {
        let mut record = claude(&format!("claude.{index}"), "Claude", "/claude-r5");
        record.agent_enabled = false;
        if index < 4 { record.archive_state = crate::session::model::CutexSessionArchiveState::Retired; }
        sessions.sessions.insert(record.cutex_session_id.clone(), record);
    }
    let built = build(&Inputs {
        sessions: &sessions, managed_agent_ids: &roster(&sessions), executable_sha256: &|_| None,
        registry: None, selected_native: None, selected_claude_adapter: Some("/claude-r6/adapter".into()),
        read_manifest: &manifests, exists: &|_| true, managed_native_root: None,
    });
    let selected = built.claude.iter().find(|e| e.selected).unwrap();
    assert_eq!((selected.active_agent_count, selected.archived_agent_count), (0, 0));
    let used = built.claude.iter().find(|e| e.agent_count > 0).unwrap();
    assert_eq!(used.active_agent_count, MAX_AGENTS + 3);
    assert_eq!(used.archived_agent_count, 4);
    assert_eq!(used.agent_count, used.active_agent_count + used.archived_agent_count);
    assert_eq!(used.agents.len(), MAX_AGENTS);
}

#[test]
fn stale_path_version_yields_current_sha_version() {
    let sessions = CutexSessionStore::default();
    let registry = registry();
    let built = build(&Inputs {
        sessions: &sessions, managed_agent_ids: &HashSet::new(), executable_sha256: &|_| Some(SHA_R49.into()),
        registry: Some(&registry), selected_native: None, selected_claude_adapter: None,
        read_manifest: &manifests, exists: &|_| true, managed_native_root: None,
    });
    assert_eq!(built.native.len(), 1);
    assert_eq!(built.native[0].version.as_deref(), Some("0.160.0-r49"));
    assert_eq!(built.native[0].sha256_prefix.as_deref(), Some(&SHA_R49[..12]));
}

#[test]
fn registry_label_updates_apply_without_invalidating_byte_digest() {
    let sessions = CutexSessionStore::default();
    let mut registry = registry();
    for version in ["0.164.0", "0.164.0-r58"] {
        registry.runtime_artifacts.get_mut(SHA_R50).unwrap().version = version.into();
        let built = build(&Inputs {
            sessions: &sessions, managed_agent_ids: &HashSet::new(), executable_sha256: &registry_path_sha,
            registry: Some(&registry), selected_native: Some(PinnedExecutable {sha256: SHA_R50.into(), path: "/r286/codex".into()}),
            selected_claude_adapter: None, read_manifest: &manifests, exists: &|_| true, managed_native_root: None,
        });
        assert_eq!(built.native.iter().find(|e| e.selected).unwrap().version.as_deref(), Some(version));
    }
}
