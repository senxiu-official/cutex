//! Private synthetic regressions; never resolves a live home or network endpoint.
use super::*;
use crate::management::v2::hot_store::{self, RowChange};
use std::time::Instant;

fn fixture(bytes: usize) -> AskState {
    let mut state = AskState::default();
    let (mut ask, _) = state.create_ask(tests::new_ask("perf-seed", "seed"), Utc::now()).unwrap();
    state.asks.clear(); state.actions.clear(); state.notices.clear();
    ask.body = tests::body(&"representative-text ".repeat(bytes / 1024 / 20));
    for n in 0..1024 {
        ask.ask_id = format!("perf-{n}");
        state.asks.insert(ask.ask_id.clone(), ask.clone());
    }
    for n in 0..9513 {
        let id = format!("art-{n:064x}");
        state.artifacts.insert(id.clone(), ArtifactRecord {
            artifact_id: id, bytes: 36_900, media_type: "text/plain".into(),
            readers: vec![tests::REQUESTER.into()], asks: vec!["perf-0".into()],
            created_at: "2026-10-26T00:00:00Z".into(),
        });
    }
    state
}

#[test]
#[ignore = "private 20MiB text + 9513 metadata records; no artifact blobs read"]
fn representative_row_decode_probe() {
    let state = fixture(20 * 1024 * 1024);
    let old_rows = ask_state_rows(&state).unwrap();
    let before = ask_state_from_rows(&old_rows).unwrap();
    let mut new_rows = old_rows.clone();
    new_rows.insert(("state".into(), "revision".into()), b"42".to_vec());
    let bytes: usize = old_rows.values().map(Vec::len).sum();
    let mut full = Vec::new(); let mut reused = Vec::new();
    for _ in 0..6 {
        // The old production refresh takes ownership, so exclude the test's
        // input copy from the baseline timer (do not overstate the speedup).
        let owned = new_rows.clone();
        let t = Instant::now();
        let decoded = ask_state_from_owned_rows(owned).unwrap();
        full.push(t.elapsed().as_secs_f64() * 1000.);
        let t = Instant::now();
        let delta = row_reuse::decode(&new_rows, Some((&before, &old_rows))).unwrap();
        reused.push(t.elapsed().as_secs_f64() * 1000.);
        assert_eq!(decoded, delta);
    }
    println!("ASK_ROW_REUSE {}", json!({"rows":old_rows.len(),"serializedBytes":bytes,
        "fullDecodeMs":full,"reuseDecodeMs":reused,"artifactBlobsRead":0}));
}

#[test]
fn changed_deleted_added_rows_match_full_decoder_and_reject_corruption() {
    let before = fixture(1024);
    let old = ask_state_rows(&before).unwrap();
    let mut after = before.clone();
    after.asks.remove("perf-0");
    after.asks.get_mut("perf-1").unwrap().target_cutex_session_id = "different-target".into();
    after.artifacts.get_mut("art-0000000000000000000000000000000000000000000000000000000000000000").unwrap().readers.clear();
    after.owner_offline.insert(tests::TARGET.into());
    after.presence.insert(tests::TARGET.into(), Presence { paused: true, ..Default::default() });
    after.wait_cancel.insert(tests::TARGET.into(), 77);
    after.revision += 1;
    let rows = ask_state_rows(&after).unwrap();
    assert_eq!(row_reuse::decode(&rows, Some((&before, &old))).unwrap(), ask_state_from_rows(&rows).unwrap());
    for (key, value) in [(("asks", "perf-1"), b"broken".to_vec()),
        (("future", "key"), b"true".to_vec()), (("owner_offline", "x"), b"false".to_vec())] {
        let mut corrupt = rows.clone(); corrupt.insert((key.0.into(),key.1.into()),value);
        assert!(row_reuse::decode(&corrupt, Some((&before, &old))).is_err());
    }
}

#[test]
fn row_companion_has_explicit_byte_and_entry_bounds() {
    let mut rows = hot_store::Rows::new();
    rows.insert(("state".into(),"notices".into()),vec![0;64*1024*1024]);
    assert!(row_reuse::bounded(rows).is_none());
    let rows = (0..100_001).map(|i| (("x".into(),i.to_string()), Vec::new())).collect();
    assert!(row_reuse::bounded(rows).is_none());
}

#[test]
fn unrelated_domain_commit_does_not_invalidate_asks_snapshot() {
    let (store, dir) = tests::temp_store("domain-generation");
    store.with_state(|s| s.revision=3).unwrap();
    let first = store.shared().unwrap();
    let generation = store.store_generation().unwrap();
    hot_store::commit(&store.database_path(),"unrelated",0,vec![RowChange::Put {
        collection:"state".into(),key:"revision".into(),value:b"5".to_vec(),
    }]).unwrap();
    assert_eq!(generation,store.store_generation().unwrap());
    assert_eq!(first,store.shared().unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn refreshed_permissions_and_corrupt_rows_never_fall_back_to_stale_cache() {
    let (store, dir) = tests::temp_store("row-reuse-authority");
    let mut seed = fixture(1024);
    seed.owner_offline.insert(tests::TARGET.into());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(store.state_path(),serde_json::to_vec(&seed).unwrap()).unwrap();
    let original = store.shared().unwrap();
    // Same SQL writer interface used by a different process, with no cache publication.
    let key = ("artifacts".to_string(),format!("art-{:064x}",0));
    std::fs::create_dir_all(dir.join("artifacts")).unwrap();
    std::fs::write(store.artifact_path(&key.1).unwrap(),b"private fixture").unwrap();
    assert!(store.read_artifact(&key.1,Some(tests::REQUESTER)).unwrap().is_some());
    let mut artifact = original.artifacts[&key.1].clone();
    artifact.readers.clear();
    artifact.asks.clear();
    let generation = store.store_generation().unwrap().unwrap();
    hot_store::commit(&store.database_path(),"asks",generation,vec![
        RowChange::Put { collection:key.0.clone(),key:key.1.clone(),value:encode_row(&artifact).unwrap() },
        RowChange::Delete { collection:"owner_offline".into(),key:tests::TARGET.into() },
        RowChange::Put { collection:"state".into(),key:"revision".into(),value:encode_row(&(original.revision+1)).unwrap() },
    ]).unwrap();
    let updated = store.shared().unwrap();
    assert!(updated.artifacts[&key.1].readers.is_empty());
    assert!(!updated.owner_offline.contains(tests::TARGET));
    assert_eq!(updated.revision,original.revision+1);
    assert!(!original.artifacts[&key.1].readers.is_empty());
    assert!(store.read_artifact(&key.1,Some(tests::REQUESTER)).unwrap().is_none());
    assert!(store.read_artifact(&key.1,None).unwrap().is_some());
    store.with_state(|s| s.next_seq+=1).unwrap();
    let generation = store.store_generation().unwrap().unwrap();
    hot_store::commit(&store.database_path(),"asks",generation,vec![RowChange::Put {
        collection:key.0.clone(),key:key.1.clone(),value:b"corrupt".to_vec(),
    }]).unwrap();
    assert!(store.shared().is_err());assert!(store.shared().is_err());
    hot_store::commit(&store.database_path(),"asks",generation+1,vec![RowChange::Put {
        collection:key.0,key:key.1.clone(),value:encode_row(&artifact).unwrap(),
    }]).unwrap();
    assert!(store.shared().unwrap().artifacts[&key.1].readers.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "child only; parent supplies a private fixture path"]
fn external_writer_child() {
    let dir = std::env::var_os("CUTEX_PERF_CHILD_DIR").expect("private fixture required");
    AskStore::at(PathBuf::from(dir)).with_state(|s| {
        s.owner_offline.insert(tests::TARGET.into());
        s.presence.insert(tests::TARGET.into(),Presence { paused:true,..Default::default() });
        s.next_seq += 9;
    }).unwrap();
}

#[test]
fn real_child_write_is_observed_and_old_snapshot_stays_immutable() {
    let (store, dir) = tests::temp_store("cross-process-read");
    store.with_state(|s| s.next_seq=11).unwrap();
    let first = store.shared().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["asks::store::read_perf_tests::external_writer_child","--exact","--ignored","--test-threads=1"])
        .env("CUTEX_PERF_CHILD_DIR",&dir).output().unwrap();
    assert!(child.status.success(),"{}",String::from_utf8_lossy(&child.stderr));
    let second = store.shared().unwrap();
    assert_eq!(second.next_seq,20);assert_eq!(second.revision,first.revision+1);
    assert!(second.owner_offline.contains(tests::TARGET));assert!(second.presence[tests::TARGET].paused);
    assert_eq!(first.next_seq,11);assert!(!first.owner_offline.contains(tests::TARGET));
    store.with_state(|s| s.next_seq+=1).unwrap();
    assert_eq!(store.shared().unwrap().next_seq,21);
    std::fs::remove_dir_all(dir).unwrap();
}
