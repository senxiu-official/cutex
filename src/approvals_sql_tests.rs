use super::*;
use chrono::{Duration, Utc};

#[test]
fn approval_consumer_child() {
    let Some(path) = std::env::var_os("CUTEX_S10_APPROVAL_CHILD") else {
        return;
    };
    let path = PathBuf::from(path);
    let root = path.parent().unwrap();
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    assert!(root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("s10-approvals-"));
    let store = ApprovalStore::at(&path);
    let id = std::env::var("CUTEX_S10_APPROVAL_ID").unwrap();
    let record = store.peek_id(&id).unwrap().unwrap();
    let now = timestamp(&record.created_at).unwrap() + Duration::seconds(2);
    let _: Option<Result<(), ()>> = store
        .try_consume_browser_open(&id, &record.requester_cutex_session_id, now, || {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("admitted"))
                .expect("only one process may perform browser admission");
            Ok(())
        })
        .unwrap();
}

#[test]
fn two_process_browser_consumers_admit_once_after_durable_commit() {
    let (store, dir, record) = fixture(0);
    store
        .with_store(|state| {
            let row = &mut state.approvals[0];
            row.kind = ApprovalKind::BrowserOpen;
            row.state = ApprovalState::Approved;
        })
        .unwrap();
    let spawn = || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "approvals::sql_tests::approval_consumer_child",
                "--nocapture",
            ])
            .env("CUTEX_S10_APPROVAL_CHILD", &store.path)
            .env("CUTEX_S10_APPROVAL_ID", &record.approval_id)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    for child in [first, second] {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert!(dir.join("admitted").is_file());
    assert_eq!(
        store.peek_id(&record.approval_id).unwrap().unwrap().state,
        ApprovalState::Consumed
    );
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

fn fixture(history: usize) -> (ApprovalStore, PathBuf, ApprovalRecord) {
    let dir = std::env::temp_dir().join(format!("s10-approvals-{}", uuid::Uuid::new_v4().simple()));
    let store = ApprovalStore::at(dir.join("approvals.json"));
    let now = timestamp("2026-11-03T12:00:00Z").unwrap();
    let request = ApprovalRequest {
        kind: ApprovalKind::ManagedService,
        reason: "test".into(),
        requester_cutex_session_id: "cutex.00000000-0000-4000-8000-000000000001".into(),
        requester_name: None,
        project_id: None,
        summary: "Selected approval".into(),
        details: json!({}),
        request_sha256: "selected-digest".into(),
    };
    let GateOutcome::Pending { record, .. } = store.gate(request, now).unwrap() else {
        panic!("new pending approval")
    };
    store
        .with_store(|state| {
            for i in 0..history {
                let mut old = record.clone();
                old.approval_id = format!("apr-old-{i:08}");
                // Same caller, unrelated request digests: a requester-only
                // index must not pass this scale guard by skipping the caller.
                old.requester_cutex_session_id = record.requester_cutex_session_id.clone();
                old.request_sha256 = format!("{i:064x}");
                old.state = ApprovalState::Denied;
                old.details = json!({"history":"real historical body".repeat(64)});
                state.approvals.push(old);
            }
        })
        .unwrap();
    (store, dir, record)
}

#[test]
fn keyed_reads_pages_and_mutations_ignore_unrelated_approval_bodies() {
    let mut observations = Vec::new();
    for history in [200, 2000] {
        let (store, dir, record) = fixture(history);
        let now = timestamp(&record.created_at).unwrap() + Duration::seconds(2);
        let (_, get) =
            crate::store::metrics::measure(|| store.peek_id(&record.approval_id).unwrap());
        let ((page, next), list) = crate::store::metrics::measure(|| {
            store
                .list_page(
                    false,
                    None,
                    crate::store::PageBudget {
                        max_rows: 20,
                        max_bytes: 64 * 1024,
                    },
                    now,
                )
                .unwrap()
        });
        assert_eq!(page, vec![record.clone()]);
        assert!(next.is_none());
        let (_, write) = crate::store::metrics::measure(|| {
            store
                .decide(&record.approval_id, true, None, now + Duration::seconds(1))
                .unwrap()
        });
        assert_eq!(
            get.whole_state_copies + list.whole_state_copies + write.whole_state_copies,
            0
        );
        observations.push((get, list, write));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    assert_eq!(observations[0], observations[1]);
}

#[test]
fn request_lookup_plan_seeks_digest_before_ordering_same_caller_history() {
    let (store, dir, record) = fixture(2000);
    store.database().unwrap().read(|tx|{
        let mut query=tx.sql().prepare("EXPLAIN QUERY PLAN SELECT data FROM approval_rows WHERE requester=? AND digest=? AND state IN ('pending','approved','denied') ORDER BY ordinal")?;
        let plans=query.query_map(rusqlite::params![record.requester_cutex_session_id,record.request_sha256],|row|row.get::<_,String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(plans.iter().any(|plan|plan.contains("approval_request_ordered") && plan.contains("requester=? AND digest=?")),"{plans:?}");
        assert!(!plans.iter().any(|plan|plan.contains("TEMP B-TREE")),"{plans:?}");
        Ok(())
    }).unwrap();
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn keyset_pages_do_not_truncate_history_and_reject_a_different_filter_or_store() {
    let (store, dir, record) = fixture(725);
    let budget = crate::store::PageBudget {
        max_rows: 7,
        max_bytes: 8 * 1024,
    };
    let mut cursor = None;
    let mut ids = std::collections::BTreeSet::new();
    let now = timestamp(&record.created_at).unwrap() + Duration::seconds(2);
    loop {
        let (rows, next) = store
            .list_page(true, cursor.as_deref(), budget, now)
            .unwrap();
        assert!(rows.len() <= 7);
        for row in rows {
            assert!(ids.insert(row.approval_id));
        }
        if next.is_none() {
            break;
        }
        cursor = next;
    }
    assert_eq!(ids.len(), 726);
    assert_eq!(store.list(true, now).unwrap().len(), 726);
    let (_, cursor) = store.list_page(true, None, budget, now).unwrap();
    let cursor = cursor.unwrap();
    assert!(store.list_page(false, Some(&cursor), budget, now).is_err());
    let (other, other_dir, _) = fixture(0);
    assert!(other.list_page(true, Some(&cursor), budget, now).is_err());
    drop(store);
    drop(other);
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_dir_all(other_dir).unwrap();
}

#[test]
fn rollback_marker_imports_previous_build_writes_once_and_fences_old_cursors() {
    let (store, dir, record) = fixture(40);
    let (old_cursor, _) = store.observe(None).unwrap();
    store.prepare_rollback().unwrap();
    let mut previous: StoreFile =
        serde_json::from_slice(&std::fs::read(&store.path).unwrap()).unwrap();
    let target = previous
        .approvals
        .iter_mut()
        .find(|r| r.approval_id == record.approval_id)
        .unwrap();
    target.state = ApprovalState::Denied;
    target.note = Some("written by previous build".into());
    crate::config::atomic::write_private_pretty_json_atomic(
        &store.path,
        &previous,
        "R1 old writer fixture",
    )
    .unwrap();
    let next = ApprovalStore::at(&store.path);
    assert_eq!(next.export_state().unwrap(), previous);
    assert!(next.observe(Some(&old_cursor)).is_err());
    std::fs::write(&store.path, b"stale JSON").unwrap();
    assert_eq!(
        ApprovalStore::at(&store.path).export_state().unwrap(),
        previous
    );
    drop(next);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn import_and_r1_preserve_every_rule_nonce_audit_and_override_field() {
    let (seed, dir, _) = fixture(0);
    let mut value = serde_json::to_value(seed.export_state().unwrap()).unwrap();
    value["rules_revision"] = json!(7);
    value["approval_rules"] = json!([{"ruleId":"aar-r1","enabled":true,
        "subject":{"type":"agent","cutexSessionId":"cutex.00000000-0000-4000-8000-000000000001","displayName":"Synthetic"},
        "kinds":["browser_open"],"constraints":{"profileIds":["fixture"]},"expiry":{"mode":"revoked"},
        "revision":3,"createdAt":"2026-11-03T12:00:00Z","updatedAt":"2026-11-03T12:00:01Z",
        "createdBy":"owner","source":"owner","grantedBy":"owner","lastMatchedAt":"2026-11-03T12:00:02Z","expired":false}]);
    value["approval_rule_audit"] = json!([{"auditId":"audit-r1","at":"2026-11-03T12:00:02Z","action":"matched","ruleId":"aar-r1","ruleRevision":3,
        "actor":"auto","requesterCutexSessionId":"cutex.00000000-0000-4000-8000-000000000001","projectId":"fixture","kind":"browser_open",
        "risk":"medium","requestSha256":"synthetic","approvalId":"synthetic","result":"approved","details":{"preserved":true}}]);
    value["approval_rule_confirmations"] = json!([
        {"token":"nonce-unused","draftSha256":"synthetic","expiresAt":"2026-11-03T12:05:00Z","consumed":false},
        {"token":"nonce-used","draftSha256":"other","expiresAt":"2026-11-03T12:05:01Z","consumed":true}]);
    value["director_rule_overrides"] = json!({"director-r1":{"disabledKinds":["browser_open"],"revision":5,"updatedAt":"2026-11-03T12:00:03Z"}});
    let path = dir.join("import/approvals.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let imported = ApprovalStore::at(&path);
    let expected: StoreFile = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(imported.export_state().unwrap(), expected);
    imported.prepare_rollback().unwrap();
    assert_eq!(
        serde_json::from_slice::<StoreFile>(&std::fs::read(&path).unwrap()).unwrap(),
        expected
    );
    value["rules_revision"] = json!(8);
    value["approval_rules"][0]["enabled"] = json!(false);
    value["approval_rule_confirmations"][0]["consumed"] = json!(true);
    value["director_rule_overrides"]["director-r1"]["revision"] = json!(6);
    let bytes = serde_json::to_vec(&value).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let next = ApprovalStore::at(&path);
    assert_eq!(
        next.export_state().unwrap(),
        serde_json::from_value::<StoreFile>(value).unwrap()
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "import never rewrites the previous build's source"
    );
    drop(next);
    drop(imported);
    drop(seed);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn orphan_outbox_is_retryable_and_empty_ticks_decode_no_history() {
    let (store, dir, record) = fixture(200);
    store
        .with_store(|state| {
            let target = state
                .approvals
                .iter_mut()
                .find(|r| r.approval_id == record.approval_id)
                .unwrap();
            target.state = ApprovalState::Approved;
            target.wait = Some(ApprovalWait {
                id: "orphan-wait".into(),
                started_at: (Utc::now() - Duration::minutes(10)).to_rfc3339(),
                until: (Utc::now() - Duration::minutes(5)).to_rfc3339(),
                thread_id: None,
                turn_id: None,
                ended_at: None,
                end: None,
            });
        })
        .unwrap();
    let claims = store.claim_orphans(Utc::now()).unwrap();
    assert_eq!(claims.len(), 1);
    store.finish_orphan(claims[0].0, false).unwrap();
    let retry = store.claim_orphans(Utc::now()).unwrap();
    assert_eq!(retry[0].0, claims[0].0);
    store.finish_orphan(retry[0].0, true).unwrap();
    let (empty, counts) =
        crate::store::metrics::measure(|| store.claim_orphans(Utc::now()).unwrap());
    assert!(empty.is_empty());
    assert_eq!(counts.bytes_decoded, 0);
    assert_eq!(counts.bytes_encoded, 0);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn orphan_staging_and_claims_bound_large_payload_batches() {
    let (store, dir, record) = fixture(0);
    let now = Utc::now();
    store
        .with_store(|state| {
            state.approvals.clear();
            for i in 0..5 {
                let mut row = record.clone();
                row.approval_id = format!("apr-orphan-budget-{i}");
                row.state = ApprovalState::Approved;
                row.details = json!({"text":"x".repeat(if i==4 {400*1024}else{80*1024})});
                row.wait = Some(ApprovalWait {
                    id: format!("wait-{i}"),
                    started_at: (now - Duration::minutes(10)).to_rfc3339(),
                    until: (now - Duration::minutes(5)).to_rfc3339(),
                    thread_id: None,
                    turn_id: None,
                    ended_at: None,
                    end: None,
                });
                state.approvals.push(row);
            }
        })
        .unwrap();
    let (first, counts) = crate::store::metrics::measure(|| store.claim_orphans(now).unwrap());
    assert_eq!(first.len(), 3);
    assert!(
        counts.bytes_decoded <= 2 * 256 * 1024,
        "staging and delivery are separate byte-budgeted phases: {counts:?}"
    );
    for (id, _) in first {
        store.finish_orphan(id, true).unwrap();
    }
    let next = store.claim_orphans(now).unwrap();
    assert_eq!(next.len(), 1);
    store.finish_orphan(next[0].0, true).unwrap();
    let oversized = store.claim_orphans(now).unwrap();
    assert_eq!(oversized.len(), 1);
    assert!(oversized[0].1.details["text"].as_str().unwrap().len() > 256 * 1024);
    store.finish_orphan(oversized[0].0, true).unwrap();
    assert!(store.claim_orphans(now).unwrap().is_empty());
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn request_and_wait_vm_steps_ignore_same_caller_history() {
    let mut observations = Vec::new();
    for history in [200, 2000] {
        let (store, dir, record) = fixture(history);
        let steps=store.database().unwrap().read(|tx|{
            let queries=[
                ("SELECT data FROM approval_rows WHERE requester=?1 AND digest=?2 AND state IN ('pending','approved','denied') ORDER BY ordinal",vec![rusqlite::types::Value::Text(record.requester_cutex_session_id.clone()),rusqlite::types::Value::Text(record.request_sha256.clone())]),
                ("SELECT data FROM approval_rows WHERE requester=?1 AND wait_until IS NOT NULL ORDER BY ordinal",vec![rusqlite::types::Value::Text(record.requester_cutex_session_id.clone())]),
                ("SELECT id,length(data) FROM approval_rows WHERE orphan_queued=0 AND wait_until IS NOT NULL AND wait_until<?1 AND state IN ('approved','denied') ORDER BY wait_until LIMIT 128",vec![rusqlite::types::Value::Integer(Utc::now().timestamp_millis())]),
            ];
            let mut result=Vec::new();
            for (query,args) in queries {
                let mut statement=tx.sql().prepare(query)?;
                {let mut rows=statement.query(rusqlite::params_from_iter(args))?;while rows.next()?.is_some(){}}
                let steps=statement.get_status(rusqlite::StatementStatus::VmStep);
                assert!(steps>0,"the execution counter must be active");
                result.push(steps);
                assert_eq!(statement.get_status(rusqlite::StatementStatus::Sort),0);
                assert_eq!(statement.get_status(rusqlite::StatementStatus::FullscanStep),0);
            }
            Ok(result)
        }).unwrap();
        eprintln!("S10_SQL_VM history={history} steps={steps:?}");
        observations.push(steps);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    assert_eq!(
        observations[0], observations[1],
        "metadata index work must also stay flat"
    );
}

#[test]
fn editing_one_rule_does_not_materialize_other_rules_or_old_audit() {
    let mut observations = Vec::new();
    for history in [20, 200] {
        let (store, dir, _) = fixture(0);
        let now = timestamp("2026-11-03T12:00:00Z").unwrap();
        let draft:crate::approval_rules::ApprovalRuleDraft=serde_json::from_value(json!({
            "enabled":true,"subject":{"type":"agent","cutexSessionId":"cutex.00000000-0000-4000-8000-000000000001"},"kinds":["browser_open"],
            "constraints":{},"expiry":{"mode":"revoked"},"highRiskKindsExplicit":[]
        })).unwrap();
        let (_, rule) = store.create_rule(0, draft.clone(), None, now).unwrap();
        store.with_store(|state|{
            for i in 0..history {
                let mut old=rule.clone();old.rule_id=format!("aar-old-{i:08}");old.subject.cutex_session_id=format!("cutex.00000000-0000-4000-8000-{:012x}",i+2);
                state.approval_rules.push(old);
                state.approval_rule_audit.push(serde_json::from_value(json!({"auditId":format!("audit-old-{i}"),
                    "at":now.to_rfc3339(),"action":"matched","actor":"test","result":"approved","details":{"history":"x".repeat(1024)}})).unwrap());
                state.director_rule_overrides.insert(format!("old-director-{i}"),serde_json::from_value(json!({})).unwrap());
            }
        }).unwrap();
        let mut next = draft;
        next.enabled = false;
        let ((revision, updated), counts) = crate::store::metrics::measure(|| {
            store
                .replace_rule(&rule.rule_id, 1, next, None, now + Duration::seconds(1))
                .unwrap()
        });
        assert_eq!(revision, 2);
        assert!(!updated.enabled);
        assert_eq!(counts.whole_state_copies, 0);
        observations.push(counts);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    assert_eq!(observations[0], observations[1]);
}

#[test]
fn import_keeps_all_decisions_and_indexed_ttl_retention_matches_the_previous_model() {
    let (store, dir, record) = fixture(2000);
    let mut oracle = store.export_state().unwrap().approvals;
    assert_eq!(
        oracle.len(),
        2001,
        "the importer itself never prunes decisions"
    );
    let created = timestamp(&record.created_at).unwrap();
    let later = created + Duration::days(2);
    expire_and_prune(&mut oracle, later);
    let mut expected = oracle.clone();
    expected.reverse();
    assert_eq!(store.list(true, later).unwrap(), expected);
    assert_eq!(expected.len(), MAX_RECORDS);
    let expired = created + Duration::days(10);
    expire_and_prune(&mut oracle, expired);
    assert!(oracle.is_empty());
    assert!(store.list(true, expired).unwrap().is_empty());
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn read_only_obligation_queries_follow_sql_and_leave_business_state_untouched() {
    let (store, dir, record) = fixture(2000);
    store
        .with_store(|state| {
            let row = state
                .approvals
                .iter_mut()
                .find(|r| r.approval_id == record.approval_id)
                .unwrap();
            row.state = ApprovalState::Approved;
            row.decided_at = Some("2000-01-01T00:00:00Z".into());
        })
        .unwrap();
    std::fs::write(&store.path, br#"{"approvals":[]}"#).unwrap();
    let before = store.export_state().unwrap();
    let (blocked, counts) = crate::store::metrics::measure(|| {
        ApprovalStore::existing_active_obligation(&store.path, |r| {
            r.approval_id == record.approval_id
        })
        .unwrap()
    });
    assert_eq!(blocked, Some(true));
    assert_eq!(counts.rows_materialized, 1);
    assert_eq!(
        store.export_state().unwrap(),
        before,
        "discovery must not expire or prune old approvals"
    );
    assert_eq!(std::fs::read(&store.path).unwrap(), br#"{"approvals":[]}"#);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
