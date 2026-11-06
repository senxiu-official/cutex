//! Private IPC scale fixture: two real writer processes, no daemon or network.
use super::*;
use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

thread_local! { static LOCK_NS: Cell<u64> = const { Cell::new(0) }; }
pub(super) fn note_lock(elapsed: Duration) {
    LOCK_NS.with(|n| {
        n.set(
            n.get()
                .saturating_add(elapsed.as_nanos().min(u64::MAX as u128) as u64),
        )
    });
}

fn fixture(root: &Path, history: usize, body_bytes: usize) {
    fs::create_dir_all(root).unwrap();
    let mut db =
        rusqlite::Connection::open(root.join(super::super::hot_store::DATABASE_FILE)).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE hot_store_meta(store TEXT PRIMARY KEY,format_version INTEGER NOT NULL,generation INTEGER NOT NULL,migrated_at TEXT,legacy_sha256 TEXT,verified INTEGER NOT NULL);
        CREATE TABLE hot_store_rows(store TEXT NOT NULL,collection TEXT NOT NULL,row_key TEXT NOT NULL,value_json BLOB NOT NULL,PRIMARY KEY(store,collection,row_key)) WITHOUT ROWID;").unwrap();
    let tx = db.transaction().unwrap();
    tx.execute(
        "INSERT INTO hot_store_meta VALUES ('agent_bus',1,1,NULL,NULL,1)",
        [],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO hot_store_rows VALUES ('agent_bus','state','version',?1)",
        [b"2".as_slice()],
    )
    .unwrap();
    {
        let mut insert = tx
            .prepare("INSERT INTO hot_store_rows VALUES ('agent_bus','message',?1,?2)")
            .unwrap();
        let body = "x".repeat(body_bytes);
        for n in 0..history {
            let id = format!("cold-{n:08}");
            let row = record(&id, "delivered", &body);
            insert
                .execute(rusqlite::params![id, serde_json::to_vec(&row).unwrap()])
                .unwrap();
        }
        for id in ["hot-a", "hot-b"] {
            insert
                .execute(rusqlite::params![
                    id,
                    serde_json::to_vec(&record(id, "pending", "active")).unwrap()
                ])
                .unwrap();
        }
    }
    tx.commit().unwrap();
}

fn record(id: &str, state: &str, body: &str) -> StoredAgentBusMessage {
    serde_json::from_value(json!({
        "ownerCutexSessionId":"cutex.writer","fromRuntimeAgentId":null,"toRuntimeAgentId":null,
        "updatedAt":"2026-11-04T00:00:00+00:00", "snapshot":{
            "messageId":id,"fromCutexSessionId":null,"toCutexSessionId":"cutex.writer",
            "deliveryMode":"after_turn","content":body,"state":state,"error":null
        }
    }))
    .unwrap()
}

fn root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("bus-write-{label}-{}", uuid::Uuid::new_v4()))
}

#[test]
fn startup_migration_hook_does_not_retain_verified_message_history() {
    for history in [20, 200] {
        let root = root("startup-keyed");
        fixture(&root, history, 4096);
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        let (_, counts) = crate::store::metrics::measure(|| repo.migrate_legacy_v1().unwrap());
        assert_eq!(counts.bytes_decoded, 0, "{counts:?}");
        assert_eq!(counts.rows_materialized, 0, "{counts:?}");
        assert!(repo.cache.lock().unwrap().is_none());
        assert!(repo.snapshot_by_message_id("hot-a").unwrap().is_some());
        drop(repo);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn keyed_writes_leave_unrelated_history_unparsed_and_catch_up_index() {
    let root = root("keyed");
    fixture(&root, 200, 4096);
    let a = AgentBusMessageRepository::open(&root).unwrap();
    let b = AgentBusMessageRepository::open(&root).unwrap();
    a.refresh_admission_if_changed().unwrap();
    b.refresh_admission_if_changed().unwrap();
    for n in 0..6 {
        let writer = if n % 2 == 0 { &a } else { &b };
        let id = if n % 2 == 0 { "hot-a" } else { "hot-b" };
        let (_, counts) = crate::store::metrics::measure(|| {
            writer
                .record_a2_submission("cutex.writer", id, &format!("a2-{n}"), Utc::now())
                .unwrap()
        });
        assert!(counts.bytes_decoded < 64 * 1024, "{counts:?}");
        assert!(counts.rows_visited < 50, "{counts:?}");
    }
    a.refresh_admission_if_changed().unwrap();
    assert_eq!(
        a.admission_index.lock().unwrap().as_ref().unwrap().sessions["cutex.writer"].len(),
        2
    );
    assert!(!a.refresh_admission_if_changed().unwrap());
    assert!(
        a.cache.lock().unwrap().is_none(),
        "hot writes/admission must not retain canonical history"
    );
    drop(a);
    drop(b);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tombstones_recreation_and_recipient_changes_update_only_contributions() {
    let root = root("delete");
    fixture(&root, 40, 128);
    let a = AgentBusMessageRepository::open(&root).unwrap();
    let b = AgentBusMessageRepository::open(&root).unwrap();
    a.refresh_admission_if_changed().unwrap();
    b.mutate_message("hot-a", |store| {
        let row = store.messages.get_mut("hot-a").unwrap();
        row.owner_cutex_session_id = "cutex.other".into();
        row.snapshot.to_cutex_session_id = "cutex.other".into();
        Ok(())
    })
    .unwrap();
    a.refresh_admission_if_changed().unwrap();
    assert_eq!(
        a.admission_index.lock().unwrap().as_ref().unwrap().sessions["cutex.other"].len(),
        1
    );
    b.mutate_message("hot-a", |store| {
        store.messages.remove("hot-a");
        Ok(())
    })
    .unwrap();
    a.refresh_admission_if_changed().unwrap();
    assert!(!a
        .admission_index
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .sessions
        .contains_key("cutex.other"));
    b.mutate_message("hot-a", |store| {
        store
            .messages
            .insert("hot-a".into(), record("hot-a", "pending", "new"));
        Ok(())
    })
    .unwrap();
    a.refresh_admission_if_changed().unwrap();
    assert_eq!(
        a.admission_index.lock().unwrap().as_ref().unwrap().sessions["cutex.writer"].len(),
        2
    );
    // Generic full-history readers catch up by the same cursor, including deletes.
    a.read(|store| {
        assert_eq!(store.messages["hot-a"].snapshot.content, "new");
        Ok(())
    })
    .unwrap();
    b.mutate_message("hot-a", |store| {
        store.messages.remove("hot-a");
        Ok(())
    })
    .unwrap();
    let (_, counts) = crate::store::metrics::measure(|| {
        a.read(|store| {
            assert!(!store.messages.contains_key("hot-a"));
            Ok(())
        })
        .unwrap()
    });
    assert!(counts.bytes_decoded < 4096, "{counts:?}");
    drop(a);
    drop(b);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn callback_error_and_external_cas_failure_do_not_publish_partial_changes() {
    let root = root("rollback");
    fixture(&root, 20, 128);
    let repo = AgentBusMessageRepository::open(&root).unwrap();
    repo.refresh_admission_if_changed().unwrap();
    let database = bus_database_path(&root.join(AGENT_BUS_STATE_FILE));
    let generation = super::super::hot_store::generation(&database, "agent_bus").unwrap();
    let result: anyhow::Result<()> = repo.mutate_message("hot-a", |store| {
        store.messages.get_mut("hot-a").unwrap().snapshot.content = "half written".into();
        anyhow::bail!("deliberate callback failure")
    });
    assert!(result.is_err());
    assert_eq!(
        super::super::hot_store::generation(&database, "agent_bus").unwrap(),
        generation
    );
    assert_eq!(repo.message_content("hot-a").unwrap().unwrap(), "active");
    let engine = super::super::hot_store::bus_changes::open(&database).unwrap();
    let before = engine
        .read(|tx| tx.head(crate::store::spec("bus-messages")))
        .unwrap();
    assert!(super::super::hot_store::commit(
        &database,
        "agent_bus",
        999,
        vec![super::super::hot_store::RowChange::Delete {
            collection: "message".into(),
            key: "hot-a".into()
        }]
    )
    .is_err());
    assert_eq!(
        engine
            .read(|tx| tx.head(crate::store::spec("bus-messages")))
            .unwrap(),
        before
    );
    drop(engine);
    drop(repo);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_writer_epoch_and_retention_resync_only_scheduling_candidates() {
    let root = root("resync");
    fixture(&root, 500, 1024);
    let repo = AgentBusMessageRepository::open(&root).unwrap();
    repo.refresh_admission_if_changed().unwrap();
    let database = bus_database_path(&root.join(AGENT_BUS_STATE_FILE));
    // A predecessor updates rows + its generation, without the S0 feed.
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch("BEGIN IMMEDIATE;
        UPDATE hot_store_rows SET value_json=CAST(json_set(CAST(value_json AS TEXT),'$.snapshot.toCutexSessionId','cutex.legacy') AS BLOB)
          WHERE store='agent_bus' AND collection='message' AND row_key='hot-a';
        UPDATE hot_store_meta SET generation=generation+1 WHERE store='agent_bus'; COMMIT;").unwrap();
    let (_, counts) =
        crate::store::metrics::measure(|| repo.refresh_admission_if_changed().unwrap());
    assert!(counts.bytes_decoded < 4096, "{counts:?}");
    assert_eq!(
        repo.admission_index
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .sessions["cutex.legacy"]
            .len(),
        1
    );
    repo.record_a2_submission("cutex.writer", "hot-b", "revision", Utc::now())
        .unwrap();
    let engine = super::super::hot_store::bus_changes::open(&database).unwrap();
    engine
        .write(|tx| {
            tx.reset_epoch(crate::store::spec("bus-messages"))
                .map(|_| ())
        })
        .unwrap();
    let (_, counts) =
        crate::store::metrics::measure(|| repo.refresh_admission_if_changed().unwrap());
    assert!(counts.bytes_decoded < 4096, "{counts:?}");
    assert!(!repo.refresh_admission_if_changed().unwrap());
    let writer = AgentBusMessageRepository::open(&root).unwrap();
    writer
        .mutate_message("hot-a", |store| {
            store.messages.remove("hot-a");
            Ok(())
        })
        .unwrap();
    repo.refresh_admission_if_changed().unwrap();
    writer
        .record_a2_submission("cutex.writer", "hot-b", "after-prune", Utc::now())
        .unwrap();
    writer
        .record_a2_submission("cutex.writer", "hot-b", "after-prune-2", Utc::now())
        .unwrap();
    // Simulate transport retention past this reader's cursor. The remaining
    // head summary still matches canonical generation; low_water forces resync.
    db.execute_batch("BEGIN IMMEDIATE;
        DELETE FROM store_changes WHERE store='bus-messages' AND seq < (SELECT seq FROM store_head WHERE store='bus-messages');
        UPDATE store_head SET low_water=seq-1 WHERE store='bus-messages'; COMMIT;").unwrap();
    let (_, counts) =
        crate::store::metrics::measure(|| repo.refresh_admission_if_changed().unwrap());
    assert!(counts.bytes_decoded < 4096, "{counts:?}");
    assert!(!repo
        .admission_index
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .messages
        .contains_key("hot-a"));
    assert_eq!(
        repo.admission_index
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .messages
            .len(),
        1
    );
    drop(writer);
    drop(db);
    drop(engine);
    drop(repo);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn shared_turn_contributions_survive_batch_removal_and_recipient_move() {
    let session = format!("cutex.turn-count-{}", uuid::Uuid::new_v4());
    let mut a = record("a", "delivered", "");
    a.snapshot.to_cutex_session_id = session.clone();
    a.admission_started_idle = true;
    a.snapshot.local_inbox_receipt = Some(json!({"turnId":"shared"}));
    let mut b = a.clone();
    b.snapshot.message_id = "b".into();
    let mut index = AdmissionIndex::from_projection(
        BTreeMap::from([
            ("a".into(), AdmissionMessageProjection::from(&a)),
            ("b".into(), AdmissionMessageProjection::from(&b)),
        ]),
        None,
    );
    super::super::admission::take_change(&session);
    index.apply(vec![("a".into(), None)]);
    assert!(index.started_turns[&session].contains("shared"));
    assert!(!super::super::admission::take_change(&session));
    b.snapshot.to_cutex_session_id = format!("{session}-moved");
    index.apply(vec![(
        "b".into(),
        Some(AdmissionMessageProjection::from(&b)),
    )]);
    assert!(!index.started_turns.contains_key(&session));
    assert!(super::super::admission::take_change(&session));
    assert!(super::super::admission::take_change(
        &b.snapshot.to_cutex_session_id
    ));
    index.apply(vec![("b".into(), None)]);
    assert!(index.turn_counts.is_empty());
}

struct Writer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Writer {
    fn start(root: &Path, id: &str, log: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "management::v2::agent_bus_state::write_tests::scale_writer_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("BUS_WRITE_CHILD_ROOT", root)
            .env("BUS_WRITE_CHILD_ID", id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(log).unwrap())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut writer = Self {
            child,
            input,
            output,
        };
        writer.reply();
        writer
    }
    fn reply(&mut self) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "writer exited; inspect stderr"
            );
            if let Some((_, text)) = line.split_once("BUS_WRITE_REPLY ") {
                return serde_json::from_str(text.trim()).unwrap();
            }
        }
    }
    fn command(&mut self, command: &str) -> Value {
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        self.reply()
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "IPC helper; needs explicit private fixture root"]
fn scale_writer_child() {
    let Some(root) = std::env::var_os("BUS_WRITE_CHILD_ROOT") else {
        return;
    };
    let id = std::env::var("BUS_WRITE_CHILD_ID").unwrap();
    let repo = AgentBusMessageRepository::open(PathBuf::from(root)).unwrap();
    repo.refresh_admission_if_changed().unwrap();
    println!(
        "BUS_WRITE_REPLY {}",
        json!({"ready":true,"pid":std::process::id()})
    );
    for command in std::io::stdin().lock().lines() {
        let command = command.unwrap();
        if command == "SYNC" {
            repo.refresh_admission_if_changed().unwrap();
            println!("BUS_WRITE_REPLY {}", json!({"synced":true}));
            continue;
        }
        if command == "QUIT" {
            println!("BUS_WRITE_REPLY {}", json!({"quit":true}));
            break;
        }
        let number = command.parse::<u32>().unwrap();
        LOCK_NS.with(|value| value.set(0));
        let started = Instant::now();
        let (_, counts) = crate::store::metrics::measure(|| {
            repo.record_a2_submission(
                "cutex.writer",
                &id,
                &format!("a2-{number:04}"),
                DateTime::from_timestamp(1_800_000_000 + number as i64, 0).unwrap(),
            )
            .unwrap()
        });
        println!(
            "BUS_WRITE_REPLY {}",
            json!({"pid":std::process::id(),"step":number,"message":id,
            "decodedBytes":counts.bytes_decoded,"rowsVisited":counts.rows_visited,"bytesRead":counts.bytes_read,
            "rowsMaterialized":counts.rows_materialized,"sqlStatements":counts.sql_statements,
            "wholeCopies":counts.whole_state_copies,"wholeSerializations":counts.whole_state_serializations,
            "lockHoldNs":LOCK_NS.with(Cell::get),"totalNs":started.elapsed().as_nanos()})
        );
    }
}

#[test]
#[ignore = "explicit two-process 1x/10x history scale gate"]
fn alternating_writer_scale() {
    let output =
        PathBuf::from(std::env::var_os("BUS_WRITE_SCALE_OUT").expect("scale output directory"));
    fs::create_dir_all(&output).unwrap();
    let history = std::env::var("BUS_WRITE_SCALE_ROWS")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(1846usize);
    let body = std::env::var("BUS_WRITE_SCALE_BODY")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(18_500usize);
    let mut result = json!({"historyRows1x":history,"bodyBytes":body,"pending":2,"cases":[]});
    for multiplier in [1, 10] {
        let root = output.join(format!("fixture-{multiplier}"));
        fixture(&root, history * multiplier, body);
        let mut a = Writer::start(
            &root,
            "hot-a",
            &output.join(format!("writer-{multiplier}-a.log")),
        );
        let mut b = Writer::start(
            &root,
            "hot-b",
            &output.join(format!("writer-{multiplier}-b.log")),
        );
        a.command("0");
        b.command("0");
        a.command("SYNC");
        b.command("SYNC");
        let mut steps = Vec::new();
        for n in 1..=8 {
            steps.push(a.command(&n.to_string()));
            steps.push(b.command(&n.to_string()));
        }
        a.command("QUIT");
        b.command("QUIT");
        assert!(a.child.wait().unwrap().success());
        assert!(b.child.wait().unwrap().success());
        result["cases"]
            .as_array_mut()
            .unwrap()
            .push(json!({"multiplier":multiplier,"steps":steps}));
        fs::write(
            output.join("results.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
    }
    if std::env::var_os("BUS_WRITE_SCALE_ASSERT").is_some() {
        let cases = result["cases"].as_array().unwrap();
        for (a, b) in cases[0]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(cases[1]["steps"].as_array().unwrap())
        {
            for key in [
                "decodedBytes",
                "rowsVisited",
                "wholeCopies",
                "wholeSerializations",
            ] {
                assert_eq!(a[key], b[key], "{key}: {a} / {b}");
            }
            assert!(a["decodedBytes"].as_u64().unwrap() > 0);
            assert_eq!(a["wholeCopies"], 0);
            assert_eq!(a["wholeSerializations"], 0);
        }
    }
}

#[test]
fn startup_start_scan_and_empty_settlement_outbox_leave_history_unparsed() {
    let mut measured = Vec::new();
    for history in [20,200] {
        let root = root("startup-scopes");fixture(&root,history,4096);
        let mut start = record("selected-start","quarantined","selected instructions");
        start.canonical_envelope = Some(serde_json::from_value(json!({
            "id":"selected-start","from":crate::agent_management::AGENT_MANAGEMENT_SYSTEM_SENDER,
            "to":"cutex.writer","triggerTurn":false,"createdAtEpochSecs":1,
            "controlType":crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE,
            "externalMessageId":"start-external","content":"selected instructions"
        })).unwrap());
        let db = rusqlite::Connection::open(root.join(super::super::hot_store::DATABASE_FILE)).unwrap();
        db.execute("INSERT INTO hot_store_rows VALUES ('agent_bus','message',?1,?2)",rusqlite::params!["selected-start",serde_json::to_vec(&start).unwrap()]).unwrap();drop(db);
        let repo = AgentBusMessageRepository::open(&root).unwrap();repo.migrate_legacy_v1().unwrap();
        let (_, counts) = crate::store::metrics::measure(|| {
            let starts = repo.management_start_messages().unwrap();
            assert_eq!(starts.len(),1);assert_eq!(starts[0].message_id,"selected-start");
            assert_eq!(starts[0].external_message_id,"start-external");
            repo.drain_settlement_outbox(&|_,_,_|panic!("empty outbox must not publish")).unwrap();
        });
        assert!(counts.bytes_decoded>0,"positive selected read counter");
        assert_eq!(counts.whole_state_copies,0);
        assert!(repo.cache.lock().unwrap().is_none(),"startup scopes must not warm canonical history");
        measured.push((counts.bytes_decoded,counts.rows_materialized));
        drop(repo);fs::remove_dir_all(root).unwrap();
    }
    assert_eq!(measured[0],measured[1]);
}

#[test]
fn startup_retention_selector_keeps_exact_rfc3339_boundary_and_history_lazy() {
    let cutoff = chrono::DateTime::parse_from_rfc3339("2026-11-03T00:00:01Z").unwrap().with_timezone(&Utc);
    let mut measured = Vec::new();
    for history in [20,200] {
        let root = root("startup-retention");fixture(&root,history,4096);
        let db = rusqlite::Connection::open(root.join(super::super::hot_store::DATABASE_FILE)).unwrap();
        let cases = [
            ("before","2026-11-03T00:00:00.999999999Z"),
            ("equal","2026-11-03T00:00:01Z"),
            ("offset","2026-11-03T01:00:00+01:00"),
            ("leap","2016-12-31T23:59:60Z"),
            ("invalid","not-a-date"),
            ("dynamic-now","now"),
            ("dynamic-subsec","subsec"),
            ("dynamic-subsecond","subsecond"),
            ("after","2026-11-03T00:00:01.000000001Z")
        ];
        let mut expected = Vec::new();
        for (id, at) in cases {
            let mut row = record(id,"delivered","retention body");row.updated_at=at.into();
            if finished(&row,cutoff).is_some() {expected.push(id.to_owned());}
            db.execute("INSERT INTO hot_store_rows VALUES ('agent_bus','message',?1,?2)",rusqlite::params![id,serde_json::to_vec(&row).unwrap()]).unwrap();
        }
        drop(db);expected.sort();assert_eq!(expected,vec!["before","leap","offset"]);
        let repo = AgentBusMessageRepository::open(&root).unwrap();repo.migrate_legacy_v1().unwrap();
        let (selected,counts) = crate::store::metrics::measure(||repo.finished_before(cutoff).unwrap());
        let actual=selected.iter().map(|r|r.message_id.clone()).collect::<Vec<_>>();assert_eq!(actual,expected);
        assert!(counts.bytes_decoded>0);assert_eq!(counts.whole_state_copies,0);
        assert!(repo.cache.lock().unwrap().is_none());
        measured.push((counts.bytes_decoded,counts.rows_materialized));
        drop(repo);fs::remove_dir_all(root).unwrap();
    }
    assert_eq!(measured[0],measured[1]);
}

#[test]
fn settled_owner_carry_lookup_is_keyed_and_survives_reopen() {
    let mut measured=Vec::new();
    for history in [20,200] {
        let root=root("owner-carry-index");fixture(&root,history,4096);
        let db=rusqlite::Connection::open(root.join(super::super::hot_store::DATABASE_FILE)).unwrap();
        let mut row=record("carried-settled","delivered","frozen owner notice");
        row.owner_carry=Some("owner-input".into());
        db.execute("INSERT INTO hot_store_rows VALUES ('agent_bus','message',?1,?2)",rusqlite::params!["carried-settled",serde_json::to_vec(&row).unwrap()]).unwrap();drop(db);
        for _ in 0..2 {
            let repo=AgentBusMessageRepository::open(&root).unwrap();repo.migrate_legacy_v1().unwrap();
            let (_,counts)=crate::store::metrics::measure(|| {
                assert_eq!(repo.owner_carry_ids("cutex.writer","owner-input").unwrap(),vec!["carried-settled"]);
                assert!(repo.owner_carry_ids("cutex.other","owner-input").unwrap().is_empty());
                assert!(repo.owner_carry_ids("cutex.writer","other-input").unwrap().is_empty());
            });
            assert_eq!(counts.bytes_decoded,0);assert_eq!(counts.whole_state_copies,0);
            assert!(repo.cache.lock().unwrap().is_none());
            measured.push((counts.rows_visited,counts.rows_materialized));
        }
        fs::remove_dir_all(root).unwrap();
    }
    assert!(measured.iter().all(|counts|counts==&measured[0]));
}
