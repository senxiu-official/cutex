use super::*;
use crate::session::store::{
    load_cutex_session_store_from_path, load_cutex_session_store_shared_at,
    save_cutex_session_store_to_path, shared_session_row_at,
};
use serde_json::json;
fn sample(root: &Path) -> CutexSessionRecord {
    std::fs::create_dir_all(root).unwrap();
    let mut record = CutexSessionRecord::new(
        "cutex.private-preboot".into(),
        None,
        crate::platform::host::current_host_name(),
        root.to_string_lossy().into_owned(),
        None,
    )
    .unwrap();
    record.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
    record.runtime_generation = 34;
    record.runtime_pid = Some(std::process::id());
    record.current_runtime_agent_id = Some("claude.original".into());
    record.claude_session=Some(serde_json::from_value(json!({
        "deployment":{"python":root.join("missing-python"),"adapter_source":root.join("adapter"),"claude_cli":root.join("cli"),"claude_home":root.join("account"),"tui_binary":root.join("tui")},
        "thread_id":"thread", "state_dir":root, "model":"sonnet", "instructions":"", "connection_id":"original", "allow_write":false,"allow_exec":false
    })).unwrap());
    record
}
fn documents(record: &CutexSessionRecord, modified: DateTime<Utc>) {
    let root = &record.claude_session.as_ref().unwrap().state_dir;
    std::fs::write(root.join("server.json"),json!({"endpoint":"ws://127.0.0.1:1","runtimeId":"original-adapter","pid":record.runtime_pid}).to_string()).unwrap();
    std::fs::write(root.join("cutex-mcp-binding.json"),json!({"threadId":"thread","runtimeAgentId":"claude.original","generation":34,"token":"private fake only"}).to_string()).unwrap();
    std::fs::File::options()
        .write(true)
        .open(root.join("server.json"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified.into()))
        .unwrap();
}
#[test]
fn occurrence_preboot_reuse_overrides_unknown_but_clock_step_does_not_replace_a_possible_adapter() {
    let root = std::env::temp_dir().join(format!("claude-preboot-proof-{}", uuid::Uuid::new_v4()));
    let record = sample(&root);
    let boot = crate::platform::process::system_boot_time().unwrap();
    documents(&record, boot - chrono::Duration::seconds(30));
    assert!(claim_predates_boot(&record).unwrap());
    assert!(!claim_blocks_start(
        &record,
        |_| None,
        || claim_predates_boot(&record).unwrap()
    ));
    assert!(!claim_blocks_start(
        &record,
        |_| Some(true),
        || claim_predates_boot(&record).unwrap()
    ));
    assert!(
        preboot_claim_evidence_with(&record, boot, |_| anyhow::bail!("access denied"))
            .unwrap()
            .is_some()
    );
    assert!(claim_blocks_start(&record, |_| None, || false));
    assert!(claim_blocks_start(&record, |_| Some(true), || false));
    // A forward wall-clock step can make now-uptime too recent. The real
    // process was born before its descriptor, so it must remain protected.
    let descriptor = Utc::now();
    documents(&record, descriptor);
    let stepped_boot = descriptor + chrono::Duration::minutes(10);
    assert!(preboot_claim_evidence_at(&record, stepped_boot)
        .unwrap()
        .is_none());
    assert!(claim_blocks_start(
        &record,
        |_| Some(true),
        || claim_predates_boot_at(&record, stepped_boot).unwrap()
    ));
    documents(&record, boot - chrono::Duration::seconds(30));
    let mut changed = record.clone();
    changed.runtime_generation += 1;
    assert!(!claim_predates_boot(&changed).unwrap());
    changed = record.clone();
    changed.current_runtime_agent_id = Some("claude.replacement".into());
    assert!(!claim_predates_boot(&changed).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn occurrence_owner_gesture_waits_for_short_settlement_and_background_tries_only() {
    let root = std::env::temp_dir().join(format!("claude-owner-lock-{}", uuid::Uuid::new_v4()));
    let record = sample(&root);
    let session = record.claude_session.as_ref().unwrap();
    let guard = try_controller_lock(session).unwrap();
    let started = Instant::now();
    assert!(try_controller_lock(session).is_err());
    assert!(started.elapsed() < Duration::from_millis(100));
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(35));
        drop(guard);
    });
    let owner =
        lock(session).expect("short background settlement must not refuse the owner gesture");
    worker.join().unwrap();
    drop(owner);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn occurrence_clear_preserves_unknown_fields_other_row_bytes_schema_and_cached_visibility() {
    let root = std::env::temp_dir().join(format!("claude-claim-cache-{}", uuid::Uuid::new_v4()));
    let record = sample(&root);
    let path = root.join("sessions.json");
    let mut other = record.clone();
    other.cutex_session_id = "cutex.unrelated".into();
    other.current_runtime_agent_id = Some("claude.unrelated".into());
    let mut store = crate::session::model::CutexSessionStore::default();
    store
        .sessions
        .insert(record.cutex_session_id.clone(), record.clone());
    store
        .sessions
        .insert(other.cutex_session_id.clone(), other.clone());
    save_cutex_session_store_to_path(&path, &store).unwrap();
    let db = crate::session::store::session_database_path(&path);
    let connection = rusqlite::Connection::open(&db).unwrap();
    let original: String = connection
        .query_row(
            "SELECT record_json FROM sessions WHERE key=?1",
            [&record.cutex_session_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut raw: Value = serde_json::from_str(&original).unwrap();
    raw["future_field"] = json!({"must":"survive","n":7});
    connection
        .execute(
            "UPDATE sessions SET record_json=?1 WHERE key=?2",
            rusqlite::params![raw.to_string(), record.cutex_session_id],
        )
        .unwrap();
    let other_before: String = connection
        .query_row(
            "SELECT record_json FROM sessions WHERE key=?1",
            [&other.cutex_session_id],
            |r| r.get(0),
        )
        .unwrap();
    let schema_before: Vec<(String, String)> = connection
        .prepare("SELECT name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    crate::session::store::close_session_store(&path);
    let cached = load_cutex_session_store_shared_at(&path).unwrap();
    let revision = cached.store_revision.get();
    let point = shared_session_row_at(&path, &record.cutex_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(point.runtime_pid, record.runtime_pid);
    assert!(clear_ended_claim(&path, &record).unwrap());
    let refreshed = load_cutex_session_store_shared_at(&path).unwrap();
    let point = shared_session_row_at(&path, &record.cutex_session_id)
        .unwrap()
        .unwrap();
    assert!(refreshed.store_revision.get() > revision);
    assert!(point.runtime_pid.is_none());
    assert!(refreshed.sessions[&record.cutex_session_id]
        .current_runtime_agent_id
        .is_none());
    assert!(
        cached.sessions[&record.cutex_session_id]
            .runtime_pid
            .is_some(),
        "old Arc remains a stable prior snapshot"
    );
    let stored: String = connection
        .query_row(
            "SELECT record_json FROM sessions WHERE key=?1",
            [&record.cutex_session_id],
            |r| r.get(0),
        )
        .unwrap();
    let after: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(after["future_field"], raw["future_field"]);
    for (key, value) in raw.as_object().unwrap() {
        if !["runtime_pid", "current_runtime_agent_id", "updated_at"].contains(&key.as_str()) {
            assert_eq!(&after[key], value, "unmodified field {key}");
        }
    }
    let other_after: String = connection
        .query_row(
            "SELECT record_json FROM sessions WHERE key=?1",
            [&other.cutex_session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other_before, other_after);
    let schema_after: Vec<(String, String)> = connection
        .prepare("SELECT name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(schema_before, schema_after);
    assert_eq!(
        connection
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        load_cutex_session_store_from_path(&path).unwrap().sessions[&record.cutex_session_id]
            .runtime_generation,
        34
    );
    drop(connection);
    crate::session::store::close_session_store(&path);
    std::fs::remove_dir_all(root).unwrap();
}
