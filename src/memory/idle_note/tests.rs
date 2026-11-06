use super::transport::{triggers_turn, Hook, Route};
use super::*;
use crate::agent_management::ProjectId;
use serde_json::json;
use std::sync::Arc;

#[path = "protocol_tests.rs"]
mod protocol_tests;

fn scope() -> super::super::ProjectMemoryScope {
    super::super::ProjectMemoryScope {
        project_id: ProjectId::new("memory-test").unwrap(),
        dir: Path::new("/private/memory").into(),
    }
}
fn snapshot(index: &str) -> Snapshot {
    Snapshot::new(&scope(), index)
}
fn root() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("idle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn seventy_minute_boundary_busy_and_clock_rollback() {
    let state = State::seeded(snapshot("old\n"), 0);
    for runtime in ["codex", "claude", "dsh", "kimi"] {
        assert!(
            !state.eligible(IDLE_AFTER_MS - 1, Some(0), false),
            "{runtime}"
        );
        assert!(state.eligible(IDLE_AFTER_MS, Some(0), false), "{runtime}");
        assert!(
            state.eligible(IDLE_AFTER_MS + 1, Some(0), false),
            "{runtime}"
        );
        assert!(
            !state.eligible(IDLE_AFTER_MS + 1, Some(0), true),
            "{runtime}"
        );
        assert!(!state.eligible(-1, Some(0), false), "{runtime}");
    }
}

#[test]
fn unchanged_index_does_not_create_a_note_even_after_long_idle() {
    let store = NoteStore::open(&root()).unwrap();
    let original = snapshot("- [甲](a.md) — 中文\n");
    store.launch("s", original.clone(), 0, false).unwrap();
    assert!(store
        .reserve("s", original, 3 * IDLE_AFTER_MS, Some(0), false)
        .unwrap()
        .is_none());
}

#[test]
fn pending_identity_survives_reopen_and_seen_advances_only_on_matching_receipt() {
    let path = root();
    let pending = {
        let store = NoteStore::open(&path).unwrap();
        store.launch("s", snapshot("old\n"), 0, false).unwrap();
        let p = store
            .reserve("s", snapshot("new\n"), IDLE_AFTER_MS, Some(0), false)
            .unwrap()
            .unwrap();
        assert_eq!(store.get("s").unwrap().unwrap().value.seen.index, "old\n");
        p
    };
    let store = NoteStore::open(&path).unwrap();
    let same = store
        .reserve(
            "s",
            snapshot("even newer\n"),
            IDLE_AFTER_MS + 1,
            Some(0),
            false,
        )
        .unwrap()
        .unwrap();
    assert_eq!(same, pending);
    store
        .delivered("s", "unrelated", IDLE_AFTER_MS + 2)
        .unwrap();
    assert!(store.get("s").unwrap().unwrap().value.pending.is_some());
    store
        .delivered("s", &pending.id, IDLE_AFTER_MS + 3)
        .unwrap();
    let state = store.get("s").unwrap().unwrap().value;
    assert_eq!(state.seen.index, "new\n");
    assert!(state.pending.is_none());
    assert!(store
        .reserve("s", snapshot("new\n"), 9 * IDLE_AFTER_MS, Some(0), false)
        .unwrap()
        .is_none());
}

#[test]
fn deleted_lines_and_changed_hooks_count_without_dumping_another_index() {
    let before = snapshot("- [old](a.md) — old\n- [remove](b.md) — gone\n");
    let rows: String = (0..50)
        .map(|i| format!("- [条目{i}](m{i}.md) — {}\n", "中".repeat(200)))
        .collect();
    let text = note_text(&before, &snapshot(&rows)).unwrap();
    assert!(text.contains("MEMORY.md changed") && text.contains(&before.path));
    assert!(text.contains("Removed/replaced previous index lines: 2"));
    assert_eq!(text.lines().filter(|l| l.starts_with("- [")).count(), 3);
    assert!(text.len() <= MAX_NOTE_BYTES && text.contains("More changes"));
    let removed_only = note_text(&before, &snapshot("")).unwrap();
    assert!(removed_only.contains("Removed/replaced previous index lines: 2"));
}

#[test]
fn deferred_codex_launch_counts_as_seen_on_compaction_without_regressing_newer_notes() {
    let store = NoteStore::open(&root()).unwrap();
    store.launch("s", snapshot("old\n"), 0, false).unwrap();
    store
        .launch("s", snapshot("new launch\n"), 5, true)
        .unwrap();
    assert_eq!(store.get("s").unwrap().unwrap().value.seen.index, "old\n");
    store.compacted("s", 10).unwrap();
    assert_eq!(
        store.get("s").unwrap().unwrap().value.seen.index,
        "new launch\n"
    );
    store.launch("s", snapshot("staged\n"), 11, true).unwrap();
    let pending = store
        .reserve(
            "s",
            snapshot("latest\n"),
            IDLE_AFTER_MS + 11,
            Some(0),
            false,
        )
        .unwrap()
        .unwrap();
    store
        .delivered("s", &pending.id, IDLE_AFTER_MS + 12)
        .unwrap();
    store.compacted("s", IDLE_AFTER_MS + 13).unwrap();
    assert_eq!(
        store.get("s").unwrap().unwrap().value.seen.index,
        "latest\n"
    );
}

#[test]
fn activity_uses_a_small_row_and_does_not_rewrite_seen_snapshot() {
    let store = NoteStore::open(&root()).unwrap();
    store
        .launch("s", snapshot(&"index\n".repeat(200)), 0, false)
        .unwrap();
    let revision = store.get("s").unwrap().unwrap().revision;
    store.turn_activity("s", "turn", 10, true).unwrap();
    store.turn_activity("s", "turn", 20, false).unwrap();
    assert_eq!(store.get("s").unwrap().unwrap().revision, revision);
    assert_eq!(store.activity("s").unwrap().unwrap().at, 20);
    assert!(!store.activity("s").unwrap().unwrap().busy);
    store.turn_activity("s", "turn", 300, false).unwrap();
    assert_eq!(
        store.activity("s").unwrap().unwrap().at,
        20,
        "duplicate completion is not new activity"
    );
}

#[test]
fn direct_file_writes_and_index_removal_are_detected_without_an_api_event() {
    let path = root();
    let scope = super::super::ProjectMemoryScope {
        project_id: ProjectId::new("direct").unwrap(),
        dir: path.clone(),
    };
    let file = path.join("MEMORY.md");
    std::fs::write(&file, "- [old](old.md) — original\n").unwrap();
    let old = Snapshot::read(&scope).unwrap();
    let store = NoteStore::open(&path).unwrap();
    store.launch("s", old.clone(), 0, false).unwrap();
    std::fs::write(&file, "- [new](new.md) — 中文新增\n").unwrap();
    let current = Snapshot::read(&scope).unwrap();
    assert_ne!(old.revision, current.revision);
    let note = store
        .reserve("s", current, IDLE_AFTER_MS, Some(0), false)
        .unwrap()
        .unwrap();
    assert!(note.text.contains("中文新增"));
    store.delivered("s", &note.id, IDLE_AFTER_MS + 1).unwrap();
    std::fs::remove_file(&file).unwrap();
    let removed = Snapshot::read(&scope).unwrap();
    let note = store
        .reserve("s", removed, 3 * IDLE_AFTER_MS, Some(IDLE_AFTER_MS), false)
        .unwrap()
        .unwrap();
    assert!(note
        .text
        .contains("Removed/replaced previous index lines: 1"));
    std::fs::write(&file, vec![b'x'; super::super::MAX_MEMORY_INDEX_BYTES + 1]).unwrap();
    assert!(Snapshot::read(&scope).is_err());
}

#[test]
fn steer_and_passive_admissions_never_trigger_a_note() {
    for method in ["turn/steer", "thread/compact/start", "thread/read"] {
        assert!(!triggers_turn(
            &json!({"method":method,"params":{"threadId":"t"}}),
            "t"
        ));
    }
    for (method, params) in [
        (
            "thread/externalInput/submit",
            json!({"threadId":"t","message":{"delivery":"passive"}}),
        ),
        (
            "thread/inter_agent_message",
            json!({"threadId":"t","deliveryMode":"passive"}),
        ),
        (
            "claude/input/submit",
            json!({"threadId":"t","metadata":{"deliveryMode":"passive"}}),
        ),
    ] {
        assert!(!triggers_turn(
            &json!({"method":method,"params":params}),
            "t"
        ));
    }
    for method in [
        "turn/start",
        "dsh/input/submit",
        "thread/externalInput/submit",
        "thread/inter_agent_message",
    ] {
        assert!(triggers_turn(
            &json!({"method":method,"params":{"threadId":"t"}}),
            "t"
        ));
        assert!(!triggers_turn(
            &json!({"method":method,"params":{"threadId":"another"}}),
            "t"
        ));
    }
}

struct UnusedEnvironment;
impl super::transport::Environment for UnusedEnvironment {
    fn now(&self) -> i64 {
        0
    }
    fn activity(&self, _: &str) -> anyhow::Result<Option<i64>> {
        Ok(Some(0))
    }
    fn snapshot(&self, _: &str) -> anyhow::Result<Option<Snapshot>> {
        Ok(None)
    }
    fn binding(&self, _: &str, _: &str, _: u64) -> anyhow::Result<bool> {
        Ok(true)
    }
}
#[test]
fn all_four_runtime_submissions_are_passive_service_context_not_owner_input() {
    let store = Arc::new(NoteStore::open(&root()).unwrap());
    let pending = Pending {
        captured_serial: 0,
        id: "note-1".into(),
        snapshot: snapshot("new\n"),
        text: "Cutex memory notice: MEMORY.md changed".into(),
    };
    for (runtime, route) in [
        ("codex", Route::Native),
        ("claude", Route::Native),
        ("dsh", Route::Legacy),
        ("kimi", Route::Legacy),
    ] {
        let hook = Hook {
            session: "cutex.s".into(),
            thread: "t".into(),
            generation: 1,
            route,
            store: store.clone(),
            environment: Arc::new(UnusedEnvironment),
        };
        let (method, params) = hook.submission(&pending);
        assert!(
            params.get("input").is_none() && params.get("clientUserMessageId").is_none(),
            "{runtime}"
        );
        match route {
            Route::Native => {
                let envelope: crate::app_server::external_input::Envelope =
                    serde_json::from_value(params.clone()).unwrap();
                envelope.validate().unwrap();
                assert_eq!(params["message"]["source"]["kind"], "service");
                assert_eq!(params["message"]["source"]["id"], "cutex-memory");
                assert_eq!(params["message"]["delivery"], "passive");
            }
            Route::Legacy => {
                assert_eq!(params["author"], "cutex-memory");
                assert_eq!(params["deliveryMode"], "passive");
            }
        }
        assert!(
            !triggers_turn(&json!({"method":method,"params":params}), "t"),
            "{runtime}"
        );
    }
}

#[test]
fn state_process_probe() {
    let Ok(path) = std::env::var("CUTEX_IDLE_PROBE_HOME") else {
        return;
    };
    let store = NoteStore::open(Path::new(&path)).unwrap();
    if std::env::var("CUTEX_IDLE_PROBE_PHASE").as_deref() == Ok("reserve") {
        store.launch("s", snapshot("old\n"), 0, false).unwrap();
        store
            .reserve("s", snapshot("new\n"), IDLE_AFTER_MS, Some(0), false)
            .unwrap()
            .unwrap();
    } else {
        let row = store.get("s").unwrap().unwrap().value;
        assert_eq!(row.seen.index, "old\n");
        store
            .delivered("s", &row.pending.unwrap().id, IDLE_AFTER_MS + 1)
            .unwrap();
    }
}

#[test]
fn a_process_restart_retains_pending_and_a_confirmed_revision_without_false_repeats() {
    let path = root();
    for phase in ["reserve", "confirm"] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "memory::idle_note::tests::state_process_probe",
                "--test-threads=1",
            ])
            .env("CUTEX_IDLE_PROBE_HOME", &path)
            .env("CUTEX_IDLE_PROBE_PHASE", phase)
            .status()
            .unwrap();
        assert!(result.success());
    }
    let store = NoteStore::open(&path).unwrap();
    let row = store.get("s").unwrap().unwrap().value;
    assert!(row.pending.is_none());
    assert_eq!(row.seen.index, "new\n");
    assert!(store
        .reserve("s", snapshot("new\n"), 2 * IDLE_AFTER_MS, Some(0), false)
        .unwrap()
        .is_none());
}

#[test]
fn a_late_receipt_for_an_older_note_cannot_replace_a_newer_compaction_snapshot() {
    for delivered_first in [true, false] {
        let store = NoteStore::open(&root()).unwrap();
        store.launch("s", snapshot("A\n"), 0, false).unwrap();
        let pending = store
            .reserve("s", snapshot("B\n"), IDLE_AFTER_MS, Some(0), false)
            .unwrap()
            .unwrap();
        // Identical clock values deliberately prove ordering is not a wall-clock guess.
        store
            .launch("s", snapshot("C\n"), IDLE_AFTER_MS, true)
            .unwrap();
        if delivered_first {
            store.delivered("s", &pending.id, IDLE_AFTER_MS).unwrap();
        }
        store.compacted("s", IDLE_AFTER_MS).unwrap();
        if !delivered_first {
            store.delivered("s", &pending.id, IDLE_AFTER_MS).unwrap();
        }
        let state = store.get("s").unwrap().unwrap().value;
        assert_eq!(state.seen.index, "C\n");
        assert!(state.pending.is_none());
    }
}
