use super::*;
use crate::session::model::NativeStopReceipt;
use crate::session::receipt_retention::ExplicitLaunchTombstone;
use crate::session::store::load_cutex_session_store_from_path;
use crate::session::store::save_cutex_session_store_to_path;
use crate::session::store::save_locked_session_store;
use crate::session::store::with_locked_session_store;
use serde_json::json;

fn temp_store(label: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("cutex-ss-{label}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("cutex-sessions.json");
    (root, path)
}

fn record(name: &str) -> CutexSessionRecord {
    CutexSessionRecord::new_at(
        format!("cutex.{name}"),
        Some(format!("thread-{name}")),
        "devbox".to_string(),
        format!("/tmp/{name}"),
        None,
        "2026-10-09T00:00:00Z".to_string(),
    )
    .expect("record")
}

fn runtime_receipt(
    action: &str,
    session: &str,
) -> crate::agent_management::ExplicitLaunchActionReceipt {
    serde_json::from_value(json!({"kind":"runtime","receipt":{
        "action_id":action,"stage":"ready","claim_id":format!("claim-{action}"),
        "runtime_agent_id":"stock.agent","expected_generation":3,"binding":null,
        "publication":null,"error":null,"updated_at":"2026-10-09T00:00:00Z",
        "review":{"subject":{"cutex_session_id":session,"formal_name":"worker","durable_sha256":"a".repeat(64),
            "authority_sha256":"a".repeat(64),"current_project_id":null,"revision":1,"runtime_generation":0},
            "contract":{"version":4,"native_id":"native-1","native_home":"/private/home",
                "bundle_manifest":"/private/manifest","bundle_sha256":"b".repeat(64)},
            "configuration":{"profile_name":"alpha","profile_id":"private","inherited":false,"profile_sha256":"c".repeat(64),
                "account_sha256":"d".repeat(64),"model":"fixture","reasoning":null,"model_provider":"private",
                "provider":{"name":"private","base_url":"http://127.0.0.1:1/v1","wire_api":"responses",
                    "requires_openai_auth":false,"supports_websockets":false},
                "sandbox":"danger-full-access","approval":"never"},
            "restart":false}
    }}))
    .unwrap()
}

/// A legacy store with every kind of entry the import must carry over.
fn legacy_store() -> CutexSessionStore {
    let mut store = CutexSessionStore::default();
    for name in ["a", "b", "c"] {
        store.sessions.insert(format!("cutex.{name}"), record(name));
    }
    store.sessions.get_mut("cutex.b").unwrap().formal_agent_name = Some("bee".into());
    store.sessions.get_mut("cutex.c").unwrap().archive_state =
        crate::session::model::CutexSessionArchiveState::Retired;
    store
        .explicit_launch_receipts
        .insert("action-1".into(), runtime_receipt("action-1", "cutex.a"));
    store
        .explicit_launch_receipts
        .insert("action-2".into(), runtime_receipt("action-2", "cutex.b"));
    store.native_stop_receipts.insert(
        "cutex.a".into(),
        NativeStopReceipt {
            native_id: Some("native-1".into()),
            generation: 4,
            binding: None,
        },
    );
    store.explicit_launch_tombstones.insert(
        "action-0".into(),
        ExplicitLaunchTombstone {
            kind: "runtime".into(),
            session: Some("cutex.a".into()),
            stage: Some("ready".into()),
            sha256: "e".repeat(64),
            pruned_at: "2026-10-09T00:00:00Z".into(),
        },
    );
    store.store_revision.set(2_600_000);
    store
}

fn write_legacy(path: &Path, store: &CutexSessionStore) -> Vec<u8> {
    let bytes = serde_json::to_vec(store).unwrap();
    fs::write(path, &bytes).unwrap();
    bytes
}

fn assert_same(left: &CutexSessionStore, right: &CutexSessionStore) {
    assert_eq!(left.store_revision.get(), right.store_revision.get());
    assert_eq!(left.sessions, right.sessions);
    assert_eq!(
        left.explicit_launch_receipts,
        right.explicit_launch_receipts
    );
    assert_eq!(left.native_stop_receipts, right.native_stop_receipts);
    assert_eq!(left.human_adoption_receipts, right.human_adoption_receipts);
    assert_eq!(left.agent_archive_receipts, right.agent_archive_receipts);
    assert_eq!(left.formal_name_receipts, right.formal_name_receipts);
    assert_eq!(
        left.explicit_launch_tombstones,
        right.explicit_launch_tombstones
    );
    for (key, record) in &left.sessions {
        // The durable digest (`request_sha256`) hashes this serialization.
        assert_eq!(
            serde_json::to_vec(record).unwrap(),
            serde_json::to_vec(&right.sessions[key]).unwrap()
        );
    }
}

fn references(root: &Path) -> Vec<PathBuf> {
    let mut found = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".pre-sqlite-")
        })
        .collect::<Vec<_>>();
    found.sort();
    found
}

fn leftovers(root: &Path) -> Vec<String> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(IMPORTING_INFIX))
        .collect()
}

fn meta(path: &Path) -> HashMap<String, String> {
    let connection = open_existing(&database_path(path)).unwrap();
    read_meta(&connection).unwrap()
}

fn arm(path: &Path, point: &'static str) {
    FAULTS.lock().unwrap().push((database_path(path), point));
}

fn disarm(path: &Path) {
    let db = database_path(path);
    FAULTS.lock().unwrap().retain(|(armed, _)| *armed != db);
}

#[test]
fn import_is_verified_keeps_the_original_and_is_idempotent() {
    let (root, path) = temp_store("import");
    let legacy = legacy_store();
    let bytes = write_legacy(&path, &legacy);
    let (db, outcome) = ensure_ready_outcome(&path, false, false).unwrap();
    assert_eq!(outcome, OpenOutcome::Imported);
    assert_eq!(db.as_deref(), Some(database_path(&path).as_path()));
    let loaded = load_cutex_session_store_from_path(&path).unwrap();
    assert_same(&loaded, &legacy);
    // The original is kept byte for byte under its reference name.
    let kept = references(&root);
    assert_eq!(kept.len(), 1);
    let today = chrono::Utc::now().format("%Y%m%d").to_string();
    assert_eq!(
        kept[0].file_name().unwrap().to_string_lossy(),
        format!("cutex-sessions.json.pre-sqlite-{today}")
    );
    assert_eq!(fs::read(&kept[0]).unwrap(), bytes);
    // Sentinel at the old path, metadata recorded.
    assert!(is_sentinel(&fs::read(&path).unwrap()));
    let meta = meta(&path);
    assert_eq!(meta["imported_sha256"], sha256_hex(&bytes));
    assert_eq!(meta["imported_sessions"], "3");
    assert_eq!(meta["imported_receipts"], "4");
    assert_eq!(meta["store_revision"], "2600000");
    assert_eq!(
        meta["imported_from"],
        format!("cutex-sessions.json.pre-sqlite-{today}")
    );
    assert!(leftovers(&root).is_empty());
    // Idempotent, in this process and in a fresh one.
    assert_eq!(
        ensure_ready_outcome(&path, true, false).unwrap().1,
        OpenOutcome::Ready
    );
    forget_process_state(&database_path(&path));
    assert_eq!(
        ensure_ready_outcome(&path, true, false).unwrap().1,
        OpenOutcome::Ready
    );
    assert_same(&load_cutex_session_store_from_path(&path).unwrap(), &legacy);
    assert_eq!(references(&root).len(), 1);
    // Writes continue the old revision sequence.
    with_locked_session_store(&path, |store| {
        store.sessions.remove("cutex.c");
        save_locked_session_store(&path, store)
    })
    .unwrap();
    let after = load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(after.store_revision.get(), 2_600_001);
    assert_eq!(after.sessions.len(), 2);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_fresh_home_creates_an_empty_database_only_when_writing() {
    let (root, path) = temp_store("fresh");
    assert_eq!(
        ensure_ready_outcome(&path, false, false).unwrap().1,
        OpenOutcome::Absent
    );
    assert!(load_cutex_session_store_from_path(&path)
        .unwrap()
        .sessions
        .is_empty());
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        0,
        "reads create nothing"
    );
    let store = CutexSessionStore::default();
    store.store_revision.set(0);
    let mut first = store.clone();
    first.sessions.insert("cutex.a".into(), record("a"));
    save_cutex_session_store_to_path(&path, &first).unwrap();
    assert_eq!(first.store_revision.get(), 1);
    assert!(
        is_sentinel(&fs::read(&path).unwrap()),
        "fresh homes get the sentinel too"
    );
    assert!(references(&root).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_imports_leave_the_json_untouched_and_retry_cleanly() {
    for point in ["after-insert", "verify", "before-rename"] {
        let (root, path) = temp_store("import-fault");
        let legacy = legacy_store();
        let bytes = write_legacy(&path, &legacy);
        arm(&path, point);
        let error = load_cutex_session_store_from_path(&path).unwrap_err();
        assert!(
            format!("{error:#}").contains("injected fault"),
            "{point}: {error:#}"
        );
        assert_eq!(fs::read(&path).unwrap(), bytes, "{point}");
        assert!(!database_path(&path).exists(), "{point}");
        assert!(leftovers(&root).is_empty(), "{point}");
        disarm(&path);
        assert_same(&load_cutex_session_store_from_path(&path).unwrap(), &legacy);
        assert!(is_sentinel(&fs::read(&path).unwrap()));
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_crashed_import_resumes() {
    // Killed while building: only the temporary database is left.
    let (root, path) = temp_store("crash-build");
    let legacy = legacy_store();
    write_legacy(&path, &legacy);
    let stale = root.join(format!("cutex-sessions.sqlite3{IMPORTING_INFIX}4242-dead"));
    fs::write(&stale, b"half a database").unwrap();
    assert_same(&load_cutex_session_store_from_path(&path).unwrap(), &legacy);
    assert!(!stale.exists());
    fs::remove_dir_all(root).unwrap();

    // Killed after the database was in place, before the JSON was renamed,
    // and after the rename, before the sentinel.
    for point in ["before-json-rename", "before-sentinel"] {
        let (root, path) = temp_store("crash-finish");
        let bytes = write_legacy(&path, &legacy);
        arm(&path, point);
        assert!(
            load_cutex_session_store_from_path(&path).is_err(),
            "{point}"
        );
        disarm(&path);
        assert!(database_path(&path).is_file(), "{point}");
        forget_process_state(&database_path(&path));
        let (_, outcome) = ensure_ready_outcome(&path, false, false).unwrap();
        assert_eq!(outcome, OpenOutcome::Finished, "{point}");
        assert!(is_sentinel(&fs::read(&path).unwrap()), "{point}");
        let kept = references(&root);
        assert_eq!(kept.len(), 1, "{point}");
        assert_eq!(fs::read(&kept[0]).unwrap(), bytes, "{point}");
        assert_eq!(
            meta(&path)["imported_from"],
            kept[0].file_name().unwrap().to_string_lossy(),
            "{point}"
        );
        assert_same(&load_cutex_session_store_from_path(&path).unwrap(), &legacy);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_json_store_changed_before_the_import_finished_is_imported_again() {
    let (root, path) = temp_store("reimport");
    let legacy = legacy_store();
    write_legacy(&path, &legacy);
    arm(&path, "before-json-rename");
    assert!(load_cutex_session_store_from_path(&path).is_err());
    disarm(&path);
    // An older cutex wrote the JSON before this one came back.
    let mut newer = legacy.clone();
    newer.sessions.insert("cutex.d".into(), record("d"));
    newer.store_revision.set(2_600_005);
    write_legacy(&path, &newer);
    forget_process_state(&database_path(&path));
    assert_eq!(
        ensure_ready_outcome(&path, false, false).unwrap().1,
        OpenOutcome::Imported
    );
    assert_same(&load_cutex_session_store_from_path(&path).unwrap(), &newer);
    let superseded = fs::read_dir(&root)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("cutex-sessions.sqlite3.superseded-")
        })
        .count();
    assert_eq!(
        superseded, 1,
        "the stale database is kept aside, not deleted"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_json_store_next_to_a_used_database_is_refused() {
    let (root, path) = temp_store("conflict");
    let legacy = legacy_store();
    write_legacy(&path, &legacy);
    load_cutex_session_store_from_path(&path).unwrap();
    with_locked_session_store(&path, |store| {
        store.sessions.remove("cutex.c");
        save_locked_session_store(&path, store)
    })
    .unwrap();
    // The imported JSON itself reappears (a restored copy): nothing to
    // lose, it is set aside again and the sentinel restored.
    write_legacy(&path, &legacy);
    forget_process_state(&database_path(&path));
    assert_eq!(
        ensure_ready_outcome(&path, false, false).unwrap().1,
        OpenOutcome::Finished
    );
    assert!(is_sentinel(&fs::read(&path).unwrap()));
    assert_eq!(references(&root).len(), 2);
    assert_eq!(
        load_cutex_session_store_from_path(&path)
            .unwrap()
            .sessions
            .len(),
        2
    );
    // A different JSON store (an older cutex wrote one) is refused.
    let mut older = legacy.clone();
    older.sessions.insert("cutex.e".into(), record("e"));
    let bytes = write_legacy(&path, &older);
    forget_process_state(&database_path(&path));
    let error = load_cutex_session_store_from_path(&path).unwrap_err();
    assert!(
        format!("{error:#}").contains("cutex session store conflict"),
        "{error:#}"
    );
    assert!(save_cutex_session_store_to_path(&path, &legacy).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes, "nothing changed");
    assert_eq!(meta(&path)["store_revision"], "2600001");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_sentinel_without_its_database_never_starts_an_empty_store() {
    let (root, path) = temp_store("lost-db");
    write_legacy(&path, &legacy_store());
    load_cutex_session_store_from_path(&path).unwrap();
    forget_process_state(&database_path(&path));
    fs::remove_file(database_path(&path)).unwrap();
    for read in [true, false] {
        let error = if read {
            load_cutex_session_store_from_path(&path).unwrap_err()
        } else {
            save_cutex_session_store_to_path(&path, &CutexSessionStore::default()).unwrap_err()
        };
        assert!(
            format!("{error:#}").contains("refusing to start an empty store"),
            "{error:#}"
        );
    }
    assert!(!database_path(&path).exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn older_cutex_cannot_parse_the_sentinel() {
    let (root, path) = temp_store("sentinel");
    write_legacy(&path, &legacy_store());
    load_cutex_session_store_from_path(&path).unwrap();
    let sentinel = fs::read(&path).unwrap();
    // Older cutex parses the file into this same struct (no
    // deny_unknown_fields): `storeRevision` must fail, loudly and readably.
    let error = serde_json::from_slice::<CutexSessionStore>(&sentinel).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("MIGRATED TO SQLITE"), "{message}");
    assert!(message.contains("cutex-sessions.sqlite3"), "{message}");
    assert!(message.starts_with("invalid type: string"), "{message}");
    // And it is valid JSON a person can read.
    let value: serde_json::Value = serde_json::from_slice(&sentinel).unwrap();
    assert_eq!(value["cutexSessionStore"], SENTINEL_MARKER);
    assert_eq!(value["database"], "cutex-sessions.sqlite3");
    assert!(value["preSqliteReference"]
        .as_str()
        .unwrap()
        .starts_with("cutex-sessions.json.pre-sqlite-"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshots_refresh_only_rows_written_since() {
    let (root, path) = temp_store("incremental");
    write_legacy(&path, &legacy_store());
    load_cutex_session_store_from_path(&path).unwrap();
    // Another process (no shared cache) changes one session and one receipt.
    let mut other = load_cutex_session_store_from_path(&path).unwrap();
    other.sessions.get_mut("cutex.a").unwrap().note = Some("changed".into());
    other
        .native_stop_receipts
        .get_mut("cutex.a")
        .unwrap()
        .generation = 5;
    other.explicit_launch_receipts.remove("action-2");
    let db = database_path(&path);
    // Simulate the other process: commit, then drop what this process knows
    // about the commit (it only patched its own cache).
    let before = CACHE
        .lock()
        .unwrap()
        .iter()
        .find(|(entry, _)| *entry == db)
        .map(|(_, snapshot)| (snapshot.revision, Arc::clone(&snapshot.store)));
    save_cutex_session_store_to_path(&path, &other).unwrap();
    let (revision, store) = before.unwrap();
    {
        let mut cache = CACHE.lock().unwrap();
        let entry = cache.iter_mut().find(|(entry, _)| *entry == db).unwrap();
        entry.1.revision = revision;
        entry.1.store = store;
        for revs in entry.1.receipt_revs.values_mut() {
            revs.retain(|_, rev| *rev <= revision);
        }
        entry.1.session_revs.retain(|_, rev| *rev <= revision);
        // Rows the other commit changed are back at their old state here.
        entry.1.session_revs.insert("cutex.a".into(), revision);
        entry
            .1
            .receipt_revs
            .get_mut(NativeStop::NAME)
            .unwrap()
            .insert("cutex.a".into(), revision);
        entry
            .1
            .receipt_revs
            .get_mut(ExplicitLaunch::NAME)
            .unwrap()
            .insert("action-2".into(), revision);
    }
    FULL_READS.with(|count| count.set(0));
    PARSED_ROWS.with(|count| count.set(0));
    let refreshed = super::shared(&path).unwrap();
    assert_eq!(FULL_READS.with(|count| count.get()), 0);
    assert_eq!(
        PARSED_ROWS.with(|count| count.get()),
        2,
        "one session and one receipt row"
    );
    assert_eq!(
        refreshed.sessions["cutex.a"].note.as_deref(),
        Some("changed")
    );
    assert_eq!(refreshed.native_stop_receipts["cutex.a"].generation, 5);
    assert!(!refreshed.explicit_launch_receipts.contains_key("action-2"));
    assert_same(&refreshed, &other);
    // A database restored to an older revision is read from scratch.
    let connection = open_existing(&db).unwrap();
    connection
        .execute(
            "UPDATE meta SET value = '7' WHERE key = 'store_revision'",
            [],
        )
        .unwrap();
    drop(connection);
    FULL_READS.with(|count| count.set(0));
    assert_eq!(super::shared(&path).unwrap().store_revision.get(), 7);
    assert_eq!(FULL_READS.with(|count| count.get()), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_commit_writes_only_changed_rows() {
    let (root, path) = temp_store("rows");
    write_legacy(&path, &legacy_store());
    let mut store = load_cutex_session_store_from_path(&path).unwrap();
    store.sessions.get_mut("cutex.b").unwrap().last_seen_at = Some("2026-10-09T01:00:00Z".into());
    save_cutex_session_store_to_path(&path, &store).unwrap();
    let connection = open_existing(&database_path(&path)).unwrap();
    let changed: Vec<String> = connection
        .prepare("SELECT key FROM sessions WHERE rev = 2600001 UNION ALL SELECT key FROM receipts WHERE rev = 2600001")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(changed, vec!["cutex.b".to_string()]);
    let indexed: (Option<String>, String) = connection
        .query_row(
            "SELECT formal_agent_name, archive_state FROM sessions WHERE key = 'cutex.b'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(indexed, (Some("bee".into()), "active".into()));
    drop(connection);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pooled_connections_see_other_writers() {
    let (root, path) = temp_store("pooled");
    let db = database_path(&path);
    POOLED_TEST_PATHS.lock().unwrap().push(db.clone());
    write_legacy(&path, &legacy_store());
    assert_eq!(
        load_cutex_session_store_from_path(&path)
            .unwrap()
            .sessions
            .len(),
        3
    );
    let external = open_existing(&db).unwrap();
    external
        .execute("DELETE FROM sessions WHERE key = 'cutex.c'", [])
        .unwrap();
    external
        .execute(
            "UPDATE meta SET value = '2600001' WHERE key = 'store_revision'",
            [],
        )
        .unwrap();
    drop(external);
    assert_eq!(
        load_cutex_session_store_from_path(&path)
            .unwrap()
            .sessions
            .len(),
        2
    );
    assert!(POOL
        .lock()
        .unwrap()
        .iter()
        .any(|(entry, connections)| *entry == db && !connections.is_empty()));
    POOLED_TEST_PATHS
        .lock()
        .unwrap()
        .retain(|entry| *entry != db);
    forget_process_state(&db);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn database_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (root, path) = temp_store("modes");
    write_legacy(&path, &legacy_store());
    load_cutex_session_store_from_path(&path).unwrap();
    for file in [database_path(&path), path.clone()] {
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    fs::remove_dir_all(root).unwrap();
}

// --- Several processes -----------------------------------------------------

const CHILD_ENV: &str = "CUTEX_SESSION_STORE_TEST_CHILD";

fn spawn_child(
    test: &str,
    path: &Path,
    id: usize,
    extra: &[(&str, String)],
) -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, path)
        .env("CUTEX_SESSION_STORE_TEST_ID", id.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in extra {
        command.env(key, value);
    }
    command.spawn().unwrap()
}

fn child_path() -> Option<(PathBuf, usize)> {
    let path = PathBuf::from(std::env::var_os(CHILD_ENV)?);
    let id = std::env::var("CUTEX_SESSION_STORE_TEST_ID")
        .ok()?
        .parse()
        .ok()?;
    Some((path, id))
}

const WRITES_PER_CHILD: usize = 25;

/// Child: half its writes under the store lock, half as whole-store
/// compare-and-swap saves that retry on conflict (as callers do).
#[test]
#[ignore = "child process of concurrent_writers_in_several_processes_lose_nothing"]
fn child_writer() {
    let Some((path, id)) = child_path() else {
        return;
    };
    let mut conflicts = 0;
    for n in 0..WRITES_PER_CHILD {
        let key = format!("cutex.p{id}-{n}");
        if n % 2 == 0 {
            with_locked_session_store(&path, |store| {
                store
                    .sessions
                    .insert(key.clone(), record(&format!("p{id}-{n}")));
                save_locked_session_store(&path, store)
            })
            .unwrap();
        } else {
            loop {
                let mut store = load_cutex_session_store_from_path(&path).unwrap();
                store
                    .sessions
                    .insert(key.clone(), record(&format!("p{id}-{n}")));
                match save_cutex_session_store_to_path(&path, &store) {
                    Ok(()) => break,
                    Err(error)
                        if error
                            .downcast_ref::<CutexSessionStoreRevisionConflict>()
                            .is_some() =>
                    {
                        conflicts += 1;
                    }
                    Err(error) => panic!("{error:#}"),
                }
            }
        }
    }
    println!("CHILD_DONE {id} conflicts={conflicts}");
}

#[test]
fn concurrent_writers_in_several_processes_lose_nothing() {
    let (root, path) = temp_store("processes");
    let legacy = legacy_store();
    write_legacy(&path, &legacy);
    // The first child to open the store imports it.
    let children = (0..4)
        .map(|id| {
            spawn_child(
                "session::store::sqlite::tests::child_writer",
                &path,
                id,
                &[],
            )
        })
        .collect::<Vec<_>>();
    // This process writes too (threads).
    let threads = (10..12)
        .map(|id| {
            let path = path.clone();
            std::thread::spawn(move || {
                for n in 0..WRITES_PER_CHILD {
                    let key = format!("cutex.p{id}-{n}");
                    with_locked_session_store(&path, |store| {
                        store
                            .sessions
                            .insert(key.clone(), record(&format!("p{id}-{n}")));
                        save_locked_session_store(&path, store)
                    })
                    .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("CHILD_DONE"),
            "child failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let store = load_cutex_session_store_from_path(&path).unwrap();
    let writers = 4 + 2;
    assert_eq!(
        store.sessions.len(),
        legacy.sessions.len() + writers * WRITES_PER_CHILD
    );
    assert_eq!(
        store.store_revision.get(),
        legacy.store_revision.get() + (writers * WRITES_PER_CHILD) as u64,
        "one revision per commit"
    );
    assert_eq!(references(&root).len(), 1, "imported once");
    assert!(leftovers(&root).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "child process of only_one_process_imports"]
fn child_importer() {
    let Some((path, id)) = child_path() else {
        return;
    };
    let (_, outcome) = ensure_ready_outcome(&path, false, false).unwrap();
    let store = load_cutex_session_store_from_path(&path).unwrap();
    println!("CHILD_OUTCOME {id} {outcome:?} {}", store.sessions.len());
}

#[test]
fn only_one_process_imports() {
    let (root, path) = temp_store("import-race");
    write_legacy(&path, &legacy_store());
    let children = (0..6)
        .map(|id| {
            spawn_child(
                "session::store::sqlite::tests::child_importer",
                &path,
                id,
                &[],
            )
        })
        .collect::<Vec<_>>();
    let mut outcomes = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let line = stdout
            .lines()
            .find_map(|line| {
                line.find("CHILD_OUTCOME")
                    .map(|at| line[at..].trim().to_string())
            })
            .unwrap_or_else(|| panic!("{stdout}"));
        assert!(line.ends_with(" 3"), "{line}");
        outcomes.push(line);
    }
    let imported = outcomes
        .iter()
        .filter(|line| line.contains("Imported"))
        .count();
    assert_eq!(imported, 1, "{outcomes:?}");
    assert!(outcomes
        .iter()
        .all(|line| line.contains("Imported") || line.contains("Ready")));
    assert_eq!(references(&root).len(), 1);
    assert!(leftovers(&root).is_empty());
    fs::remove_dir_all(root).unwrap();
}
