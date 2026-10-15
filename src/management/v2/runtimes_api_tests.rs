use std::path::Path;

use serde_json::json;

use super::*;
use crate::session::model::{CutexSessionRecord, CutexSessionStore};

const SHA_R50: &str = "cff0b90dc1a2aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_R49: &str = "b49b49b49b49bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_DEV: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

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
    let built = build(&Inputs {
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
    assert_eq!(native[0]["agentCount"], 2);
    assert_eq!(
        native[0]["agents"],
        json!([{"cutexSessionId": "cutex.a", "name": "Alpha"}, {"cutexSessionId": "cutex.c", "name": "Gamma"}])
    );
    assert_eq!(native[1]["release"], "r49");
    assert_eq!(native[1]["selected"], false);
    assert!(native[1].get("latest").is_none());
    assert_eq!(native[1]["missing"], true);
    assert_eq!(native[1]["agents"], json!([{"cutexSessionId": "cutex.b", "name": "Beta"}]));
    let unregistered = native.as_array().unwrap().iter().find(|e| e["sha256Prefix"] == "dddddddddddd").unwrap();
    assert!(unregistered.get("version").is_none());
    assert_eq!(unregistered["release"], "r300");
    assert_eq!(unregistered["agentCount"], 1);
    let unreadable = native.as_array().unwrap().iter().find(|e| e["release"] == "r190").unwrap();
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
    assert_eq!(summary["native"]["inUse"][0], json!({"release": "r50", "version": "0.160.0-r50", "agents": 2, "selected": true, "missing": false}));
    assert_eq!(summary["native"]["inUse"].as_array().unwrap().iter().map(|u| u["agents"].as_u64().unwrap()).sum::<u64>(), 5);
    assert_eq!(summary["claude"]["selected"], "claude-r5");
    assert_eq!(summary["claude"]["inUse"][1], json!({"release": "claude-r4", "version": null, "agents": 1, "selected": false, "missing": false}));
    assert_eq!(summary["unpinnedAgents"], 1);
    let text = summary.to_string();
    assert!(!text.contains("Alpha") && !text.contains("D:/"), "{text}");
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
    let built = build(&Inputs {
        sessions: &sessions,
        registry: Some(&registry),
        selected_native: None,
        selected_claude_adapter: None,
        read_manifest: &manifests,
        exists: &exists,
    });
    let r50 = &built.native[0];
    assert!(r50.selected, "registry latest is selected without a deployment");
    assert_eq!(r50.agent_count, MAX_AGENTS + 5);
    assert_eq!(r50.agents.len(), MAX_AGENTS);
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
    let built = build(&Inputs {
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
