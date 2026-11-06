use super::super::{activity, session};
use super::*;
use serde_json::{json, Value};

fn root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("s7-{}", uuid::Uuid::new_v4()));
    crate::management::client_tokens::ensure_private_dir(&root).unwrap();
    root
}

#[test]
fn activity_import_is_atomic_validated_and_keeps_source_and_deletion_fences() {
    let root = root();
    let source = root.join("session-activity-state.json");
    let bytes=br#"{"deletedSessions":["gone"],"version":1,"sessions":{"a":{"revision":7,"lastConversationAt":"2026-11-04T01:02:03Z"}}}"#;
    std::fs::write(&source, bytes).unwrap();
    let repo = activity::activity_repository(&root).unwrap();
    let row = repo.get("a").unwrap().unwrap();
    assert_eq!(row.revision, 7);
    assert_eq!(
        row.conversation_at().as_deref(),
        Some("2026-11-04T01:02:03Z")
    );
    assert!(repo.get("gone").unwrap().is_none());
    repo.update("gone", |deleted, _| {
        assert!(deleted);
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    drop(repo);
    // A stale/invalid JSON file after the switch cannot overwrite current SQL.
    std::fs::write(&source, b"invalid stale source").unwrap();
    assert_eq!(
        activity::activity_repository(&root)
            .unwrap()
            .get("a")
            .unwrap()
            .unwrap(),
        row
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_sources_never_publish_partial_rows_and_retry_cleanly() {
    for bad in [
        r#"{"version":9,"sessions":{}}"#,
        r#"{"deletedSessions":["a"],"sessions":{"a":{"revision":2}}}"#,
        r#"{"sessions":{"a":{"revision":2},"b":{"revision":9007199254740992}}}"#,
        r#"{"sessions":{"a":{"revision":2},"a":{"revision":3}}}"#,
        r#"{"sessions":{"a":{"lastConversationAt":"invalid"}}}"#,
        r#"{"sessions":{"a":{"revision":2}}} trailing"#,
    ] {
        let root = root();
        let source = root.join("session-activity-state.json");
        std::fs::write(&source, bad).unwrap();
        assert!(activity::activity_repository(&root).is_err(), "{bad}");
        std::fs::write(
            &source,
            br#"{"version":1,"sessions":{"fixed":{"revision":3}}}"#,
        )
        .unwrap();
        let repo = activity::activity_repository(&root).unwrap();
        assert!(repo.get("a").unwrap().is_none());
        assert_eq!(repo.get("fixed").unwrap().unwrap().revision, 3);
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn projection_row(revision: u64, fingerprint: &str) -> Value {
    json!({"revision":revision,"fingerprint":fingerprint,"focus":{"revision":4,"owner":"pc","mobileMuted":true,"source":"test","updatedAt":"2026-11-04T01:02:03Z"}})
}

#[test]
fn projection_preserves_focus_revision_cas_error_noop_and_other_writer() {
    let root = root();
    let source = root.join("session-projection-state.json");
    let legacy = json!({"version":1,"sessions":{"a":projection_row(12,"old"),"b":projection_row(18,"other")}});
    std::fs::write(&source, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let repo = session::projection_repository(&root).unwrap();
    let initial = repo.get("a").unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&initial).unwrap(),
        legacy["sessions"]["a"]
    );
    let (_, counts) = store::metrics::measure(|| repo.update("a", |_, _| Ok(())).unwrap());
    assert_eq!(counts.bytes_encoded, 0);
    let failed: anyhow::Result<()> = repo.update("a", |_, row| {
        *row = Some(serde_json::from_value(projection_row(13, "failed")).unwrap());
        anyhow::bail!("CAS conflict")
    });
    assert!(failed.is_err());
    assert_eq!(repo.get("a").unwrap(), Some(initial));
    let other = session::projection_repository(&root).unwrap();
    other
        .update("a", |_, row| {
            *row = Some(serde_json::from_value(projection_row(13, "committed")).unwrap());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(repo.get("a").unwrap()).unwrap()["fingerprint"],
        "committed"
    );
    assert_eq!(
        serde_json::to_value(repo.get("b").unwrap()).unwrap(),
        legacy["sessions"]["b"]
    );
    let invalid: anyhow::Result<()> = repo.update("a", |_, row| {
        *row = Some(serde_json::from_value(projection_row(0, "invalid")).unwrap());
        Ok(())
    });
    assert!(invalid.is_err());
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(source).unwrap()).unwrap(),
        legacy
    );
    drop(other);
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stores_switch_independently_and_never_touch_delivery_state() {
    let root = root();
    std::fs::write(
        root.join("user-input-state.json"),
        b"untouched pending delivery",
    )
    .unwrap();
    std::fs::write(
        root.join("session-projection-state.json"),
        br#"{"version":99}"#,
    )
    .unwrap();
    let activity = activity::activity_repository(&root).unwrap();
    assert!(session::projection_repository(&root).is_err());
    activity
        .update("a", |_, row| {
            *row = Some(Default::default());
            Ok(())
        })
        .unwrap();
    assert!(activity.get("a").unwrap().is_some());
    assert_eq!(
        std::fs::read(root.join("user-input-state.json")).unwrap(),
        b"untouched pending delivery"
    );
    drop(activity);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_rows_scale_fixed_point_update_and_bounded_page_at_1x_10x() {
    for id in ["session-activity", "session-projection"] {
        let mut measured = Vec::new();
        for rows in [470, 4700] {
            let root = root();
            // Real v1 shapes; typed import is tested above, the generic engine's
            // counters are measured here with equally sized session rows.
            let row = if id == "session-activity" {
                json!({"revision":1,"lastConversationAt":"2026-11-04T01:02:03Z"})
            } else {
                projection_row(1, "x")
            };
            let repo =
                Repository::<Value>::open(&root, id, &format!("{id}-state.json"), |_| Ok(()))
                    .unwrap();
            repo.db
                .write(|tx| {
                    for n in 0..rows {
                        repo.put(tx, &format!("s{n:06}"), Some(&row))?;
                    }
                    Ok(())
                })
                .unwrap();
            let (_, counts) = store::metrics::measure(|| {
                repo.get("s000000").unwrap();
                repo.page("", 20, 4096).unwrap();
                repo.update("s000000", |_, row| {
                    row.as_mut().unwrap()["revision"] = json!(2);
                    Ok(())
                })
                .unwrap();
                repo.update("s000000", |_, _| Ok(())).unwrap();
            });
            assert_eq!(counts.whole_state_copies, 0);
            assert_eq!(counts.whole_state_serializations, 0);
            assert!(counts.rows_materialized <= 24);
            eprintln!("S7 {id} rows={rows} counts={counts:?}");
            measured.push(counts);
            drop(repo);
            std::fs::remove_dir_all(root).unwrap();
        }
        assert_eq!(measured[0], measured[1], "{id}");
    }
}

#[test]
fn session_rows_wal_old_reader_observes_snapshot_without_blocking_commit() {
    let root = root();
    let repo = activity::activity_repository(&root).unwrap();
    repo.update("a", |_, row| {
        *row = Some(Default::default());
        Ok(())
    })
    .unwrap();
    let reader = rusqlite::Connection::open(root.join("session-activity.sqlite3")).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let old: Vec<u8> = reader
        .query_row("SELECT data FROM session_rows WHERE id='a'", [], |r| {
            r.get(0)
        })
        .unwrap();
    repo.update("a", |_, row| {
        row.as_mut().unwrap().revision = 9;
        Ok(())
    })
    .unwrap();
    let still_old: Vec<u8> = reader
        .query_row("SELECT data FROM session_rows WHERE id='a'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(old, still_old);
    assert_eq!(repo.get("a").unwrap().unwrap().revision, 9);
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_pages_respect_bytes_keys_and_deleted_fences_without_truncating_rows() {
    let root=root();
    let repo=Repository::<Value>::open(&root,"session-activity","session-activity-state.json",|_|Ok(())).unwrap();
    repo.update("a",|_,row|{*row=Some(json!({"revision":1,"body":"x".repeat(2048)}));Ok(())}).unwrap();
    repo.forget("b").unwrap();
    repo.update("c",|_,row|{*row=Some(json!({"revision":2}));Ok(())}).unwrap();
    let (first,counts)=store::metrics::measure(||repo.page("",20,1).unwrap());
    assert_eq!(first.len(),1);
    assert_eq!(first[0].0,"a");
    assert_eq!(first[0].1.as_ref().unwrap()["body"].as_str().unwrap().len(),2048);
    assert_eq!(counts.rows_materialized,1);
    assert_eq!(counts.rows_visited,2);
    let rest=repo.page("a",20,256).unwrap();
    assert_eq!(rest.iter().map(|(key,_)|key.as_str()).collect::<Vec<_>>(),vec!["b","c"]);
    assert!(rest[0].1.is_none());
    assert!(repo.page("",0,256).is_err());
    assert!(repo.page("",1,0).is_err());
    drop(repo);std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn activity_and_turn_notice_metadata_commit_or_rollback_together() {
    let root=root();
    let repo=activity::activity_repository(&root).unwrap();
    repo.update("a",|_,row| { row.get_or_insert_with(Default::default).revision=1;Ok(()) }).unwrap();
    let failed=repo.update_then::<(),()>("a",|_,row| {
        row.as_mut().unwrap().revision=2;Ok(())
    },|sql,()| {
        sql.execute("INSERT INTO turn_error_notices VALUES ('profile','2026-11-04T01:00:00Z')",[])?;
        anyhow::bail!("metadata callback refused")
    });
    assert!(failed.is_err());
    assert_eq!(repo.get("a").unwrap().unwrap().revision,1);
    repo.update_then("a",|_,row| {row.as_mut().unwrap().revision=3;Ok(())},|sql,()| {
        let count:i64=sql.query_row("SELECT count(*) FROM turn_error_notices",[],|r|r.get(0))?;
        assert_eq!(count,0,"failed metadata write must roll back");
        let revision:i64=sql.query_row("SELECT json_extract(CAST(data AS TEXT),'$.revision') FROM session_rows WHERE id='a'",[],|r|r.get(0))?;
        assert_eq!(revision,3,"callback must see the changed row in its transaction");
        sql.execute("INSERT INTO turn_error_notices VALUES ('profile','2026-11-04T02:00:00Z')",[])?;
        Ok(())
    }).unwrap();
    assert_eq!(repo.get("a").unwrap().unwrap().revision,3);
    drop(repo);std::fs::remove_dir_all(root).unwrap();
}
