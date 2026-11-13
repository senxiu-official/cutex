use super::*;
use crate::session::model::{CutexSessionArchiveState, NativeStopReceipt};
use crate::session::store::{load_cutex_session_store_from_path, save_cutex_session_store_to_path};

fn rehearsal_path() -> PathBuf {
    assert_eq!(std::env::var("CUTEX_S3_REHEARSAL").as_deref(),Ok("1"),"explicit offline rehearsal only");
    let home=PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("marked private home"));
    assert!(home.join(".cutex-test-private-home").is_file());
    crate::session::store::cutex_sessions_path().unwrap()
}
fn receipt_values(store:&CutexSessionStore)->serde_json::Value {
    let mut value=serde_json::to_value(store).unwrap();
    value.as_object_mut().unwrap().remove("sessions");
    value.as_object_mut().unwrap().remove("storeRevision");
    value
}

#[test]
#[ignore="offline exact-previous-build rehearsal: prepare, run pinned previous CLI, verify"]
fn exact_previous_rehearsal_prepare() {
    let path=rehearsal_path();
    assert!(!database_path(&path).exists(),"never overwrite a rehearsal home");
    let f=calibrated_fixture(1);
    let store=load_cutex_session_store_from_path(&f.path).unwrap();
    store.store_revision.set(0); // a new private target, not a stale write to f
    save_cutex_session_store_to_path(&path,&store).unwrap();
    crate::session::store::update_session_row(&path,"cutex.active-0000",|row| {
        row.note=Some("candidate before exact previous build".into());
        row.host_id=crate::platform::host::current_host_name();Ok(())
    }).unwrap().unwrap();
    crate::session::store::compare_and_swap_session_row(&path,"cutex.active-0001",Some(store.sessions.get("cutex.active-0001").unwrap()),None).unwrap();
    crate::session::store::update_receipt_row::<NativeStop>(&path,"candidate-new-receipt",Some(NativeStopReceipt {native_id:Some("synthetic-deleted-native".into()),generation:9,binding:None})).unwrap();
    let current=load_cutex_session_store_from_path(&path).unwrap();
    let cursor=page_at(&path,false,None,PageBudget::LIST_DEFAULT).unwrap().unwrap().change_cursor.encode();
    let expected=serde_json::json!({"receipts":receipt_values(&current),"cursor":cursor});
    fs::write(path.parent().unwrap().join("s3-rehearsal-expected.json"),serde_json::to_vec(&expected).unwrap()).unwrap();
    println!("S3 candidate fixture ready: {} sessions, {} receipts",current.sessions.len(),1549+1);
}

#[test]
#[ignore="offline exact-previous-build rehearsal: run after pinned previous CLI read/write"]
fn exact_previous_rehearsal_verify() {
    let path=rehearsal_path();
    let expected:serde_json::Value=serde_json::from_slice(&fs::read(path.parent().unwrap().join("s3-rehearsal-expected.json")).unwrap()).unwrap();
    let current=load_cutex_session_store_from_path(&path).unwrap();
    assert_eq!(receipt_values(&current),expected["receipts"],"all receipt maps survive the exact previous writer");
    assert!(!current.sessions.contains_key("cutex.active-0001"),"deleted session never resurrects");
    let row=current.sessions.get("cutex.active-0000").unwrap();
    assert_eq!(row.note.as_deref(),Some("candidate before exact previous build"));
    assert_eq!(row.model_defaults.as_deref(),Some("previous-rehearsal"));
    let cursor=ChangeCursor::decode(crate::store::manifest::spec(STORE),expected["cursor"].as_str().unwrap()).unwrap();
    let Some(SessionChanges::Page {changes,..})=changes_at(&path,&cursor,PageBudget::DELTA_DEFAULT).unwrap() else {panic!("previous writer must emit persisted changes")};
    assert!(changes.iter().any(|change|change.key=="cutex.active-0000"&&!change.deleted));
    let c=open_connection(&database_path(&path),false).unwrap();
    assert_eq!(c.pragma_query_value(None,"user_version",|row|row.get::<_,i64>(0)).unwrap(),1);
    println!("PASS: exact previous read/write retained all receipt maps, deletion and candidate state; previous write emitted a delta");
}

#[test]
fn typed_repository_cas_scopes_outbox_and_previous_writers_share_one_feed() {
    use crate::store::{
        ApplyBatch, ChangeKind, ChangesOutcome, Expect, KeyedStore, OutboxItem, StoreError, Wake,
    };
    let f = fixture(12);
    // S0 wake channels identify the local store and scope. Give this private
    // fixture a scope distinct from all other independently generated homes.
    let id = format!("cutex.typed-{}",uuid::Uuid::new_v4());
    let c=open_connection(&database_path(&f.path),false).unwrap();
    c.execute("UPDATE sessions SET key=?1,cutex_session_id=?1,record_json=?2 WHERE key='cutex.active-0000'",params![id,serde_json::to_string(&record(&id)).unwrap()]).unwrap();
    drop(c);
    let repo = SessionRepository::open(&f.path).unwrap();
    let head = repo.change_head(&Scope::Host).unwrap();
    assert_eq!(head.instance, repo.database().instance_id());
    let other = "cutex.active-0001".to_owned();
    let original = repo.get(&Scope::Host, &id).unwrap().unwrap();
    assert_eq!(
        original.seq, 0,
        "pre-extension rows start at the bootstrap head"
    );
    let mut changed = (*original.value).clone();
    changed.note = Some("typed update".into());
    let mut wake = repo.subscribe(&Scope::Session(id.clone()));
    let commit = repo
        .apply(
            ApplyBatch::new(Scope::Host)
                .put(
                    id.clone(),
                    Arc::new(changed),
                    Expect::Revision(original.revision),
                )
                .delete(other.clone(), Expect::Any)
                .outbox(OutboxItem {
                    topic: "s3-test",
                    idem_key: "once".into(),
                    payload: b"audit".to_vec(),
                }),
        )
        .unwrap();
    assert_eq!(commit.seq, 2);
    assert_eq!(wake.wait(Duration::ZERO), Wake::Changed(1));
    let row = repo.get(&Scope::Host, &id).unwrap().unwrap();
    assert_eq!(row.seq, 1);
    assert!(row.value.note.is_some());
    assert!(repo
        .get(&Scope::Session(other.clone()), &id)
        .unwrap()
        .is_none());
    assert!(repo.get(&Scope::Project("foreign".into()), &id).is_err());
    let page = repo
        .list(
            &Scope::Session(id.clone()),
            &SessionFilter::Active,
            None,
            PageBudget::LIST_DEFAULT,
        )
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].key, id);
    let many = repo
        .get_many(
            &Scope::Host,
            &[id.clone(), "absent".into(), "cutex.active-0002".into()],
            1,
        )
        .unwrap();
    assert_eq!(many.found.len(), 1);
    assert_eq!(many.missing, vec!["absent"]);
    assert_eq!(many.deferred, vec!["cutex.active-0002"]);
    let ChangesOutcome::Page { changes, .. } = repo
        .changes(&Scope::Host, &head, PageBudget::DELTA_DEFAULT)
        .unwrap()
    else {
        panic!("feed")
    };
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[1].kind, ChangeKind::Delete);
    let claimed =
        crate::store::outbox::claim(repo.database(), "s3-test", 10, Duration::from_secs(1))
            .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].commit_seq, commit.seq);
    let mut edited = (*row.value).clone();
    edited.note = Some("must roll back".into());
    assert!(matches!(
        repo.apply(
            ApplyBatch::new(Scope::Host)
                .put(id.clone(), Arc::new(edited), Expect::Any)
                .delete("cutex.active-0002".into(), Expect::Absent)
                .outbox(OutboxItem {
                    topic: "s3-test",
                    idem_key: "rolled-back".into(),
                    payload: vec![1]
                })
        ),
        Err(StoreError::Conflict { .. })
    ));
    assert_eq!(repo.change_head(&Scope::Host).unwrap().after_seq(),commit.seq);
    assert_eq!(
        repo.get(&Scope::Host, &id)
            .unwrap()
            .unwrap()
            .value
            .note
            .as_deref(),
        Some("typed update")
    );
    let cursor = repo.change_head(&Scope::Session(id.clone())).unwrap();
    previous_write(&f.path, &id, true);
    let ChangesOutcome::Page { changes, .. } = repo
        .changes(
            &Scope::Session(id.clone()),
            &cursor,
            PageBudget::DELTA_DEFAULT,
        )
        .unwrap()
    else {
        panic!("old writer feed")
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::Delete);
    assert!(load_cutex_session_store_from_path(&f.path)
        .unwrap()
        .native_stop_receipts
        .contains_key("receipt"));
    // Both the row and its outbox must roll back when any later CAS fails.
    crate::store::outbox::ack(repo.database(), &[claimed[0].id]).unwrap();
    assert!(
        crate::store::outbox::claim(repo.database(), "s3-test", 10, Duration::ZERO)
            .unwrap()
            .is_empty()
    );
}

struct Fixture {
    root: PathBuf,
    path: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        forget_process_state(&database_path(&self.path));
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn record(id: &str) -> CutexSessionRecord {
    let mut row = CutexSessionRecord::new_at(
        id.into(),
        None,
        "devbox".into(),
        "/work".into(),
        None,
        "2026-11-04T00:00:00Z".into(),
    )
    .unwrap();
    row.agent_enabled = true;
    row.registration_class = crate::agent_bus::model::AgentRegistrationClass::Persistent;
    row
}

#[test]
fn quick_chat_promote_discovery_does_not_decode_sessions_or_completed_receipts() {
    let f = fixture(1);
    let connection = open_connection(&database_path(&f.path), false).unwrap();
    // Either row would fail a whole-store snapshot parse. The startup lookup
    // needs neither session bodies nor completed promotion payloads.
    connection.execute("UPDATE sessions SET record_json='{}' WHERE key='cutex.active-0000'", []).unwrap();
    connection.execute("INSERT INTO receipts(map,key,rev,json) VALUES('quick_chat_promotion','done',1,?1)",
        [r#"{"result":{"complete":true}}"#]).unwrap();
    assert!(pending_quick_chat_promotions_at(&f.path).unwrap().is_empty());
}
fn fixture(history: usize) -> Fixture {
    fixture_shape(166, history)
}
fn fixture_shape(active: usize, history: usize) -> Fixture {
    let root = std::env::temp_dir().join(format!("s3-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("cutex-sessions.json");
    let mut store = CutexSessionStore::default();
    for i in 0..active {
        let id = format!("cutex.active-{i:04}");
        store.sessions.insert(id.clone(), record(&id));
    }
    for i in 0..history {
        let id = format!("cutex.retired-{i:06}");
        let mut row = record(&id);
        row.archive_state = CutexSessionArchiveState::Retired;
        store.sessions.insert(id, row);
    }
    store.native_stop_receipts.insert(
        "receipt".into(),
        NativeStopReceipt {
            native_id: None,
            generation: 1,
            binding: None,
        },
    );
    fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();
    ensure_ready(&path, false, false).unwrap();
    Fixture { root, path }
}

/// One consistent, aggregate-only EVA capture (2026-11-04): 232 active,
/// 137 retired and 1549 receipts; 7,816,490 canonical JSON bytes. No live
/// content is used. Extra receipt history belongs to retired/deleted subjects.
fn calibrated_fixture(history: usize) -> Fixture {
    use crate::agent_management::{ExplicitLaunchActionReceipt, StockRuntimeStage};
    use serde_json::json;
    let f = fixture_shape(232, 137 * history);
    let mut store = load_cutex_session_store_from_path(&f.path).unwrap();
    for row in store.sessions.values_mut() {
        row.codex_session_id = Some(format!("native-{}", row.cutex_session_id));
    }
    store.native_stop_receipts.clear();
    let subject = |i: usize, base: usize| {
        if i < base {
            format!("cutex.active-{:04}", i % 232)
        } else {
            format!(
                "cutex.retired-{:06}",
                137 + (i - base) % (137 * (history - 1))
            )
        }
    };
    for i in 0..319 * history {
        let sid = subject(i, 319);
        let action = format!("calibrated-runtime-{i:06}");
        let mut receipt = super::super::tests::runtime_receipt(&action, &sid);
        let ExplicitLaunchActionReceipt::Runtime(runtime) = &mut receipt else {
            panic!("runtime receipt")
        };
        runtime.stage = if i == 0 {
            StockRuntimeStage::Prepared
        } else {
            StockRuntimeStage::Abandoned
        };
        runtime.error=Some((0..120).map(|j|format!("Synthetic launch {i:06}, readiness probe {j:03}: ws://127.0.0.1:1 unavailable; reviewed occurrence is absent; transport closed; no child is retained and no retry is scheduled.\n")).collect());
        store.explicit_launch_receipts.insert(action, receipt);
    }
    for i in 0..888 * history {
        store.explicit_launch_tombstones.insert(
            format!("calibrated-pruned-{i:06}"),
            crate::session::receipt_retention::ExplicitLaunchTombstone {
                kind: "runtime".into(),
                session: Some(subject(i, 888)),
                stage: Some("ready".into()),
                sha256: format!("{i:064x}"),
                pruned_at: "2026-11-04T00:00:00Z".into(),
            },
        );
    }
    for i in 0..171 * history {
        store.native_stop_receipts.insert(
            format!("cutex.deleted-proof-{i:06}"),
            NativeStopReceipt {
                native_id: Some(format!("native-proof-{i:06}")),
                generation: 0,
                binding: None,
            },
        );
    }
    for i in 0..71 * history {
        let sid = subject(i, 71);
        let row = store.sessions.get(&sid).unwrap();
        let action = format!("calibrated-adoption-{i:06}");
        let receipt=serde_json::from_value(json!({"request":{"action_id":action,"native_id":row.codex_session_id,"cwd":row.cwd,"formal_name":"fixture-worker"},"record":row,"import_request":null})).unwrap();
        store.human_adoption_receipts.insert(action, receipt);
    }
    for i in 0..100 * history {
        let sid = format!("cutex.retired-{:06}", i % (137 * history));
        let row = store.sessions.get(&sid).unwrap();
        let action = format!("calibrated-archive-{i:06}");
        let receipt=serde_json::from_value(json!({"request":{"action_id":action,"reason":"Synthetic completed archive","review":{"cutex_session_id":sid,"formal_name":"fixture-worker","operation":"archive","current_project_id":null,"revision":1,"runtime_generation":0}},"request_sha256":"f".repeat(64),"stage":"committed","result":row,"error":null,"committed_at":"2026-11-04T00:00:00Z"})).unwrap();
        store.agent_archive_receipts.insert(action, receipt);
    }
    assert_eq!(
        store.explicit_launch_receipts.len()
            + store.explicit_launch_tombstones.len()
            + store.native_stop_receipts.len()
            + store.human_adoption_receipts.len()
            + store.agent_archive_receipts.len(),
        1549 * history
    );
    save_cutex_session_store_to_path(&f.path, &store).unwrap();
    f
}

#[test]
fn calibrated_receipt_and_session_history_does_not_change_warm_work() {
    fn counts(history: usize) -> [OpCounts; 10] {
        use crate::store::{ApplyBatch,Expect,KeyedStore};
        let f = calibrated_fixture(history);
        get_at(&f.path, "cutex.active-0000").unwrap();
        page_at(
            &f.path,
            false,
            None,
            PageBudget {
                max_rows: 20,
                max_bytes: 256 * 1024,
            },
        )
        .unwrap();
        host_page_at(&f.path, "devbox", None, "", 20).unwrap();
        let (_, point) =
            crate::store::metrics::measure(|| get_at(&f.path, "cutex.active-0000").unwrap());
        let (_, page) = crate::store::metrics::measure(|| {
            page_at(
                &f.path,
                false,
                None,
                PageBudget {
                    max_rows: 20,
                    max_bytes: 256 * 1024,
                },
            )
            .unwrap()
        });
        let (_, host) = crate::store::metrics::measure(|| {
            host_page_at(&f.path, "devbox", None, "", 20).unwrap()
        });
        let (_,exact_host)=crate::store::metrics::measure(||host_row_at(&f.path,"devbox","cutex.active-0000").unwrap());
        assert_eq!(exact_host.rows_visited,1);
        assert_eq!(exact_host.sql_statements,2);
        let (_, claim) = crate::store::metrics::measure(|| {
            assert!(has_prepared_launch_at(&f.path, "cutex.active-0000").unwrap())
        });
        let (_, update) = crate::store::metrics::measure(|| {
            crate::session::store::update_session_row(&f.path, "cutex.active-0000", |row| {
                row.note = Some("current".into());
                Ok(())
            })
            .unwrap()
        });
        let repo=SessionRepository::open(&f.path).unwrap();
        let key="cutex.active-0000".to_string();
        repo.get(&Scope::Host,&key).unwrap();
        let (_,typed_point)=crate::store::metrics::measure(||repo.get(&Scope::Host,&key).unwrap());
        let (_,typed_page)=crate::store::metrics::measure(||repo.list(&Scope::Host,&SessionFilter::Active,None,PageBudget {max_rows:20,max_bytes:256*1024}).unwrap());
        let cursor=repo.change_head(&Scope::Session(key.clone())).unwrap();
        let mut row=(*repo.get(&Scope::Host,&key).unwrap().unwrap().value).clone();
        row.note=Some("typed change".into());
        let (_,typed_update)=crate::store::metrics::measure(||repo.apply(ApplyBatch::new(Scope::Session(key.clone())).put(key.clone(),Arc::new(row),Expect::Any)).unwrap());
        let (_,typed_changes)=crate::store::metrics::measure(||repo.changes(&Scope::Session(key),&cursor,PageBudget::DELTA_DEFAULT).unwrap());
        println!(
            "S3 calibrated history={history}: {point:?} {page:?} {host:?} {claim:?} {update:?} {typed_point:?} {typed_page:?} {typed_update:?} {typed_changes:?} {exact_host:?}"
        );
        [point, page, host, claim, update, typed_point, typed_page, typed_update, typed_changes, exact_host]
    }
    let small = counts(1);
    let large = counts(10);
    assert_eq!(small, large);
    for count in small {
        assert_eq!(count.whole_state_copies, 0);
        assert_eq!(count.bytes_decoded, 0);
    }
}
/// Bare v1 SQL, with no S3 helper involved: models an independent previous writer.
fn previous_write(path: &Path, id: &str, delete: bool) {
    let mut c = open_connection(&database_path(path), false).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let rev = read_revision(&tx).unwrap() + 1;
    if delete {
        tx.execute("DELETE FROM sessions WHERE key=?1", [id])
            .unwrap();
    } else {
        let mut row = record(id);
        row.note = Some(format!("rev {rev}"));
        tx.execute(
            "INSERT INTO sessions VALUES(?1,?1,NULL,NULL,'active',?2,?3)
            ON CONFLICT(key) DO UPDATE SET rev=excluded.rev,record_json=excluded.record_json",
            params![id, rev as i64, serde_json::to_string(&row).unwrap()],
        )
        .unwrap();
    }
    tx.execute(
        "UPDATE meta SET value=?1 WHERE key='store_revision'",
        [rev.to_string()],
    )
    .unwrap();
    tx.commit().unwrap();
}

#[test]
fn unmanaged_and_owned_records_keep_their_original_catalog_eligibility() {
    let f = fixture_shape(0, 0);
    let mut unmanaged = record("cutex.unmanaged");
    unmanaged.agent_enabled = false;
    unmanaged.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
    let mut c = open_connection(&database_path(&f.path), false).unwrap();
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "INSERT INTO sessions VALUES(?1,?1,NULL,NULL,'active',1,?2)",
        params![
            unmanaged.cutex_session_id,
            serde_json::to_string(&unmanaged).unwrap()
        ],
    )
    .unwrap();
    tx.execute("UPDATE meta SET value='1' WHERE key='store_revision'", [])
        .unwrap();
    tx.commit().unwrap();
    drop(c);
    // Opening S3 over an existing default/local-only row must not create NULL.
    assert!(get_at(&f.path, "cutex.unmanaged").unwrap().is_some());
    assert!(host_page_at(&f.path, "devbox", None, "", 20)
        .unwrap()
        .0
        .is_empty());
    let mut owned = unmanaged;
    owned.cutex_session_id = "cutex.owned".into();
    owned.runtime_backend = crate::session::model::CutexSessionRuntimeBackend::Dsh;
    crate::session::store::compare_and_swap_session_row(&f.path, "cutex.owned", None, Some(owned))
        .unwrap();
    let (page, _) = host_page_at(&f.path, "devbox", None, "", 20).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].cutex_session_id, "cutex.owned");
    assert!(get_at(&f.path, "cutex.unmanaged").unwrap().is_some());
}

#[test]
fn a_late_old_wal_reader_does_not_replace_the_newer_cached_row() {
    let f = fixture(0);
    let id = "cutex.active-0000";
    let page = page_at(&f.path, false, None, PageBudget::LIST_DEFAULT)
        .unwrap()
        .unwrap();
    let old_json = serde_json::to_string(&record(id)).unwrap();
    previous_write(&f.path, id, false);
    let new = get_at(&f.path, id).unwrap().unwrap();
    let old = decode(
        &database_path(&f.path),
        &page.change_cursor.instance,
        id.into(),
        0,
        old_json,
    )
    .unwrap();
    assert!(old.note.is_none());
    assert!(new.note.is_some());
    assert!(Arc::ptr_eq(&new, &get_at(&f.path, id).unwrap().unwrap()));
    let cache = ROW_CACHE.lock().unwrap();
    assert_eq!(
        cache
            .rows
            .keys()
            .filter(|key| key.path == database_path(&f.path) && key.key == id)
            .count(),
        1
    );
    assert!(cache.rows.len() <= CACHE_ROWS);
    assert!(cache.bytes <= CACHE_BYTES);
    assert_eq!(
        cache.bytes,
        cache.rows.values().map(|row| row.bytes).sum::<usize>()
    );
}

#[test]
fn keyed_cache_pages_and_cross_process_tombstones() {
    let f = fixture(284);
    let a = get_at(&f.path, "cutex.active-0000").unwrap().unwrap();
    let (b, counts) =
        crate::store::metrics::measure(|| get_at(&f.path, "cutex.active-0000").unwrap().unwrap());
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(counts.bytes_decoded, 0);
    assert_eq!(counts.rows_visited, 1);
    let page = page_at(
        &f.path,
        false,
        None,
        PageBudget {
            max_rows: 20,
            max_bytes: 256 * 1024,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(page.rows.len(), 20);
    assert!(page.next.is_some());
    let other = fixture(0);
    assert!(matches!(
        changes_at(&other.path, &page.change_cursor, PageBudget::DELTA_DEFAULT).unwrap(),
        Some(SessionChanges::Resync {
            reason: crate::store::ResyncReason::WrongInstance,
            ..
        })
    ));
    let mut scoped = page.change_cursor.clone();
    scoped.scope = Scope::Session("cutex.active-0000".into());
    assert!(matches!(
        changes_at(&f.path, &scoped, PageBudget::DELTA_DEFAULT).unwrap(),
        Some(SessionChanges::Resync {
            reason: crate::store::ResyncReason::ScopeMismatch,
            ..
        })
    ));
    assert!(page_at(
        &f.path,
        true,
        page.next.as_deref(),
        PageBudget::LIST_DEFAULT
    )
    .is_err());
    previous_write(&f.path, "cutex.active-0000", false);
    previous_write(&f.path, "cutex.active-0001", true);
    let new = get_at(&f.path, "cutex.active-0000").unwrap().unwrap();
    assert!(!Arc::ptr_eq(&a, &new));
    assert!(a.note.is_none());
    assert!(new.note.is_some());
    assert!(get_at(&f.path, "cutex.active-0001").unwrap().is_none());
    let Some(SessionChanges::Page { changes, more, .. }) =
        changes_at(&f.path, &page.change_cursor, PageBudget::DELTA_DEFAULT).unwrap()
    else {
        panic!("delta")
    };
    assert_eq!(changes.len(), 2);
    assert!(!changes[0].deleted);
    assert!(changes[1].deleted);
    assert!(!more);
    let Some(SessionChanges::Page {
        changes,
        cursor,
        more,
        oversize_first,
        ..
    }) = changes_at(
        &f.path,
        &page.change_cursor,
        PageBudget {
            max_rows: 1,
            max_bytes: 1,
        },
    )
    .unwrap()
    else {
        panic!("bounded delta")
    };
    assert_eq!(changes.len(), 1);
    assert!(more);
    assert!(oversize_first);
    let Some(SessionChanges::Page { changes, more, .. }) = changes_at(
        &f.path,
        &cursor,
        PageBudget {
            max_rows: 1,
            max_bytes: 1,
        },
    )
    .unwrap() else {
        panic!("bounded delta tail")
    };
    assert_eq!(changes.len(), 1);
    assert!(changes[0].deleted);
    assert!(!more);
}

#[test]
fn prepared_claim_check_and_receipt_writes_never_load_other_receipts() {
    use crate::agent_management::{ExplicitLaunchActionReceipt, StockRuntimeStage};
    let f = fixture(1180);
    let head = page_at(&f.path, false, None, PageBudget::LIST_DEFAULT)
        .unwrap()
        .unwrap()
        .change_cursor;
    FULL_READS.with(|count| count.set(0));
    let mut receipt = super::super::tests::runtime_receipt("prepared", "cutex.active-0000");
    let ExplicitLaunchActionReceipt::Runtime(runtime) = &mut receipt else {
        panic!("runtime receipt")
    };
    runtime.stage = StockRuntimeStage::Prepared;
    put_receipt_at::<ExplicitLaunch>(&f.path, "prepared", Some(receipt.clone())).unwrap();
    assert!(has_prepared_launch_at(&f.path, "cutex.active-0000").unwrap());
    assert!(!has_prepared_launch_at(&f.path, "cutex.active-0001").unwrap());
    let ExplicitLaunchActionReceipt::Runtime(runtime) = &mut receipt else {
        panic!("runtime receipt")
    };
    runtime.stage = StockRuntimeStage::Ready;
    put_receipt_at::<ExplicitLaunch>(&f.path, "prepared", Some(receipt)).unwrap();
    assert!(!has_prepared_launch_at(&f.path, "cutex.active-0000").unwrap());
    assert_eq!(FULL_READS.with(|count| count.get()), 0);
    let Some(SessionChanges::Page {
        changes,
        watermark,
        more,
        ..
    }) = changes_at(&f.path, &head, PageBudget::DELTA_DEFAULT).unwrap()
    else {
        panic!("receipt watermark")
    };
    assert!(changes.is_empty());
    assert_eq!(watermark, 2);
    assert!(!more);
}

#[test]
fn retained_snapshot_refreshes_session_and_receipt_deletions() {
    let f = fixture(118);
    let old = shared(&f.path).unwrap();
    let mut edited = load_cutex_session_store_from_path(&f.path).unwrap();
    edited.sessions.remove("cutex.active-0001");
    edited.native_stop_receipts.remove("receipt");
    save_cutex_session_store_to_path(&f.path, &edited).unwrap();
    // Force a different-process refresh while retaining a slow old reader.
    forget_process_state(&database_path(&f.path));
    put_cached(
        &database_path(&f.path),
        Snapshot {
            revision: old.store_revision.get(),
            session_revs: Default::default(),
            receipt_revs: Default::default(),
            store: Arc::clone(&old),
        },
    );
    let now = shared(&f.path).unwrap();
    assert!(old.sessions.contains_key("cutex.active-0001"));
    assert!(old.native_stop_receipts.contains_key("receipt"));
    assert!(!now.sessions.contains_key("cutex.active-0001"));
    assert!(!now.native_stop_receipts.contains_key("receipt"));
    previous_write(&f.path, "cutex.previous-after-new", false);
    assert!(shared(&f.path)
        .unwrap()
        .sessions
        .contains_key("cutex.previous-after-new"));
}

#[test]
fn warm_point_page_and_host_page_ignore_tenfold_retired_history() {
    fn counts(history: usize) -> [OpCounts; 4] {
        let f = fixture(history);
        get_at(&f.path, "cutex.active-0000").unwrap();
        page_at(
            &f.path,
            false,
            None,
            PageBudget {
                max_rows: 20,
                max_bytes: 256 * 1024,
            },
        )
        .unwrap();
        host_page_at(&f.path, "devbox", None, "", 20).unwrap();
        let (_, point) =
            crate::store::metrics::measure(|| get_at(&f.path, "cutex.active-0000").unwrap());
        let (_, page) = crate::store::metrics::measure(|| {
            page_at(
                &f.path,
                false,
                None,
                PageBudget {
                    max_rows: 20,
                    max_bytes: 256 * 1024,
                },
            )
            .unwrap()
        });
        let (_, host) = crate::store::metrics::measure(|| {
            host_page_at(&f.path, "devbox", None, "", 20).unwrap()
        });
        let (_, update) = crate::store::metrics::measure(|| {
            crate::session::store::update_session_row(&f.path, "cutex.active-0000", |r| {
                r.note = Some("current".into());
                Ok(())
            })
            .unwrap()
        });
        [point, page, host, update]
    }
    let small = counts(118);
    let large = counts(1180);
    assert_eq!(small, large);
    for (index, op) in small.into_iter().enumerate() {
        assert_eq!(op.whole_state_copies, 0);
        assert_eq!(op.bytes_decoded, 0);
        assert!(op.rows_visited <= if index == 2 { 41 } else { 21 });
    }
    println!("S3 166 active / 118 vs 1180 retired: {small:?}");
}

#[test]
fn one_hundred_updates_with_slow_old_readers_share_untouched_rows() {
    let f = calibrated_fixture(1);
    let old = shared(&f.path).unwrap();
    let mut retained = Vec::new();
    for n in 0..100 {
        crate::session::store::update_session_row(&f.path, "cutex.active-0000", |r| {
            r.note = Some(format!("update {n}"));
            Ok(())
        })
        .unwrap();
        let now = shared(&f.path).unwrap();
        assert!(old.sessions.row_ptr_eq(&now.sessions, "cutex.active-0001"));
        assert!(old
            .explicit_launch_receipts
            .row_ptr_eq(&now.explicit_launch_receipts, "calibrated-runtime-000000"));
        assert!(old
            .sessions
            .get("cutex.active-0000")
            .unwrap()
            .note
            .is_none());
        retained.push(now);
    }
    assert_eq!(retained.len(), 100);
    assert_eq!(
        retained[99]
            .sessions
            .get("cutex.active-0000")
            .unwrap()
            .note
            .as_deref(),
        Some("update 99")
    );
}

#[test]
fn row_and_byte_retention_expire_only_transport_and_request_resync() {
    let f = fixture(0);
    let first = page_at(&f.path, false, None, PageBudget::LIST_DEFAULT)
        .unwrap()
        .unwrap();
    let mut c = open_connection(&database_path(&f.path), false).unwrap();
    for (count, padding) in [(50001usize, 0usize), (10000, 1024)] {
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let rev = read_revision(&tx).unwrap() + 1;
        let mut insert = tx
            .prepare_cached("INSERT INTO receipts VALUES('native_stop',?1,?2,?3)")
            .unwrap();
        for i in 0..count {
            let key = format!("{rev}-{i}-{}", "x".repeat(padding));
            insert
                .execute(params![
                    key,
                    rev as i64,
                    r#"{"native_id":null,"generation":1,"binding":null}"#
                ])
                .unwrap();
        }
        drop(insert);
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key='store_revision'",
            [rev.to_string()],
        )
        .unwrap();
        tx.commit().unwrap();
        let (rows,bytes):(i64,i64)=c.query_row(
            "SELECT COUNT(*),COALESCE(SUM(length(CAST(key AS BLOB))+length(CAST(map AS BLOB))+length(CAST(session_id AS BLOB))+64),0) FROM session_changes",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert!(rows <= 50000);
        assert!(bytes <= 8 * 1024 * 1024);
    }
    let before = c
        .query_row("SELECT COUNT(*) FROM session_changes", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let rev = read_revision(&tx).unwrap() + 1;
    tx.execute(
        "INSERT INTO receipts VALUES('native_stop','tiny',?1,?2)",
        params![
            rev as i64,
            r#"{"native_id":null,"generation":1,"binding":null}"#
        ],
    )
    .unwrap();
    tx.execute(
        "UPDATE meta SET value=?1 WHERE key='store_revision'",
        [rev.to_string()],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM session_changes", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        before + 1,
        "an update that fits in remaining bytes must not expire another row"
    );
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        60003
    );
    let Some(SessionChanges::Page {
        changes,
        cursor: after_receipts,
        ..
    }) = changes_at(&f.path, &first.change_cursor, PageBudget::DELTA_DEFAULT).unwrap()
    else {
        panic!("unrelated receipt retention must not expire session cursor")
    };
    assert!(changes.is_empty());
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let rev = read_revision(&tx).unwrap() + 1;
    let mut update=tx.prepare_cached("UPDATE sessions SET rev=?1,record_json=json_set(record_json,'$.note',?2,'$.revision',?3) WHERE key='cutex.active-0000'").unwrap();
    for i in 0..50001 {
        update
            .execute(params![
                rev as i64,
                format!("synthetic correction {i}"),
                i as i64 + 2
            ])
            .unwrap();
    }
    drop(update);
    tx.execute(
        "UPDATE meta SET value=?1 WHERE key='store_revision'",
        [rev.to_string()],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM session_changes", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        50000
    );
    let Some(SessionChanges::Resync { reason, .. }) =
        changes_at(&f.path, &after_receipts, PageBudget::DELTA_DEFAULT).unwrap()
    else {
        panic!("lost session changes must resync")
    };
    assert_eq!(reason, crate::store::ResyncReason::BelowRetention);
    drop(c);
}
