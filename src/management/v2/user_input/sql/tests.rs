use super::*;
use serde_json::json;

fn params(id: &str) -> UserInputSubmitParams {
    UserInputSubmitParams {
        client_user_message_id: id.into(),
        origin: UserInputOrigin {
            kind: UserInputOriginKind::Other,
            client_id: "fixture".into(),
        },
        strategy: UserInputStrategy::Queue,
        input: vec![json!({"type":"text","text":id})],
        expected_turn_id: None,
        join_delegation: None,
    }
}
fn root() -> PathBuf {
    std::env::temp_dir().join(format!("s5-{}", Uuid::new_v4()))
}
fn sending() -> InputDelivery {
    InputDelivery::Sending {
        thread_id: "thread".into(),
        since: Utc::now().to_rfc3339(),
    }
}

#[test]
fn import_preserves_all_facts_once_without_replay() {
    let source_root = root();
    fs::create_dir_all(&source_root).unwrap();
    let seed = UserInputRepository::open(root()).unwrap();
    seed.enqueue("s", "thread", "request", &params("waiting"))
        .unwrap();
    seed.register_identity("s", &params("accepted")).unwrap();
    let accepted = InputDelivery::Accepted {
        thread_id: "thread".into(),
        turn_id: Some("turn".into()),
        accepted_at: Utc::now().to_rfc3339(),
    };
    seed.set_delivery("s", "accepted", None, Some(accepted.clone()))
        .unwrap();
    seed.register_identity("s", &params("unknown")).unwrap();
    seed.set_delivery(
        "s",
        "unknown",
        None,
        Some(InputDelivery::Unknown {
            thread_id: "thread".into(),
            since: "old".into(),
        }),
    )
    .unwrap();
    seed.pause("s", "manual", None).unwrap();
    let expected = seed.dump();
    let source = serde_json::to_vec(&expected).unwrap();
    let path = source_root.join(USER_INPUT_STATE_FILE);
    fs::write(&path, &source).unwrap();
    let repo = UserInputRepository::open(&source_root).unwrap();
    let report = repo.migration_report().unwrap();
    assert_eq!(report["ready"], true);
    assert_eq!(report["sessions"], 1);
    assert_eq!(report["inputs"], 1);
    assert_eq!(report["identities"], 3);
    assert_eq!(report["sourceFingerprint"].as_str().unwrap().len(), 64);
    assert_eq!(
        serde_json::to_value(repo.dump()).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(repo.delivery("s", "accepted").unwrap(), Some(accepted));
    assert_eq!(
        repo.register_identity("s", &params("accepted")).unwrap(),
        ClientIdentityDecision::Existing
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        source,
        "the source is immutable evidence"
    );
    let head = repo.db.read(|tx| tx.head(store::spec(ID))).unwrap();
    // A stale source may be corrupt or missing after ready: SQL remains canonical.
    fs::write(&path, b"invalid stale evidence").unwrap();
    let reopened = UserInputRepository::open(&source_root).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.dump()).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        reopened.db.read(|tx| tx.head(store::spec(ID))).unwrap(),
        head
    );
    let seed_root = seed.root.clone();
    drop(seed);
    drop(reopened);
    drop(repo);
    fs::remove_dir_all(seed_root).unwrap();
    fs::remove_dir_all(source_root).unwrap();
}

#[test]
fn malformed_import_rolls_back_every_row_and_ready_marker() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    let path = root.join(USER_INPUT_STATE_FILE);
    // The first session is valid and written before parsing the second fails.
    fs::write(
        &path,
        br#"{"version":1,"sessions":{"good":{"queueRevision":8},"broken":null}}"#,
    )
    .unwrap();
    assert!(UserInputRepository::open(&root).is_err());
    let db = StoreDb::open_at(
        &store::domain::OWNER_INPUT,
        &root.join("owner-input.sqlite3"),
    )
    .unwrap();
    db.read(|tx| {
        let exists: bool = tx.sql().query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='owner_sessions')",
            [],
            |r| r.get(0),
        )?;
        if exists {
            assert_eq!(
                tx.sql()
                    .query_row("SELECT count(*) FROM owner_sessions", [], |r| r
                        .get::<_, i64>(0))?,
                0
            );
        }
        assert_eq!(
            tx.sql().query_row(
                "SELECT count(*) FROM store_migrations WHERE store='user-input'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    })
    .unwrap();
    drop(db);
    fs::write(
        &path,
        br#"{"version":1,"sessions":{"good":{"queueRevision":8}}}"#,
    )
    .unwrap();
    let repo = UserInputRepository::open(&root).unwrap();
    assert_eq!(repo.list("good").unwrap().0, 8);
    drop(repo);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_delivery_reservations_have_one_winner() {
    let root = root();
    let repo = UserInputRepository::open(&root).unwrap();
    repo.register_identity("s", &params("one")).unwrap();
    let first = UserInputRepository::open(&root).unwrap();
    let second = UserInputRepository::open(&root).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let wins = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            first
                .set_delivery("s", "one", None, Some(sending()))
                .unwrap()
        });
        let b = scope.spawn(|| {
            barrier.wait();
            second
                .set_delivery("s", "one", None, Some(sending()))
                .unwrap()
        });
        usize::from(a.join().unwrap()) + usize::from(b.join().unwrap())
    });
    assert_eq!(wins, 1);
    assert!(repo.delivery("s", "one").unwrap().is_some());
    drop(first);
    drop(second);
    drop(repo);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn warm_owner_ops_are_constant_with_same_session_and_host_history() {
    fn measured(history: usize) -> Vec<OpCounts> {
        let root = root();
        let repo = UserInputRepository::open(&root).unwrap();
        repo.enqueue("selected", "thread", "request", &params("live"))
            .unwrap();
        repo.db
            .write(|tx| {
                for session in ["selected", "unrelated"] {
                    let (before, rev) = selected(tx.sql(), session, &[], true)?;
                    let mut after = before.clone().unwrap_or_default();
                    // AutoSi clock strings vary between 0/3/6/9 fractional
                    // digits. Keep existing fixture bodies a fixed byte size.
                    let clock = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, false);
                    fn normalize(value: &mut Value, clock: &str) {
                        match value {
                            Value::String(text)
                                if chrono::DateTime::parse_from_rfc3339(text).is_ok() =>
                            {
                                *text = clock.into()
                            }
                            Value::Array(items) => {
                                for item in items {
                                    normalize(item, clock);
                                }
                            }
                            Value::Object(fields) => {
                                for value in fields.values_mut() {
                                    normalize(value, clock);
                                }
                            }
                            _ => {}
                        }
                    }
                    let mut normalized = serde_json::to_value(&after)?;
                    normalize(&mut normalized, &clock);
                    after = serde_json::from_value(normalized)?;
                    for id in 0..history {
                        after.identities.insert(
                            format!("retained-{id:06}"),
                            ClientInputIdentity {
                                canonical_sha256: "a".repeat(64),
                                first_seen_at: clock.clone(),
                                delivery: Some(InputDelivery::Accepted {
                                    thread_id: "t".into(),
                                    turn_id: Some("turn".into()),
                                    accepted_at: clock.clone(),
                                }),
                            },
                        );
                    }
                    apply(tx, session, rev, before.as_ref(), Some(&after))?;
                }
                Ok(())
            })
            .unwrap();
        let mut counts = Vec::new();
        let (result, c) = store::metrics::measure(|| repo.delivery("selected", "live"));
        result.unwrap();
        counts.push(c);
        let (result, c) = store::metrics::measure(|| repo.list("selected"));
        assert_eq!(result.unwrap().1.len(), 1);
        counts.push(c);
        let (result, c) = store::metrics::measure(|| {
            repo.set_delivery(
                "selected",
                "live",
                None,
                Some(InputDelivery::Sending {
                    thread_id: "thread".into(),
                    since: "fixed".into(),
                }),
            )
        });
        assert!(result.unwrap());
        counts.push(c);
        let (result, c) =
            store::metrics::measure(|| repo.register_identity("selected", &params("new-identity")));
        assert_eq!(result.unwrap(), ClientIdentityDecision::New);
        counts.push(c);
        let (result, c) = store::metrics::measure(|| repo.sessions_with_queued_items());
        assert_eq!(result.unwrap(), vec!["selected"]);
        counts.push(c);
        for c in &counts {
            assert_eq!(c.whole_state_copies, 0);
            assert_eq!(c.whole_state_serializations, 0);
        }
        drop(repo);
        fs::remove_dir_all(root).unwrap();
        counts
    }
    let small = measured(2_000);
    let large = measured(20_000);
    for (label, (small, large)) in [
        "delivery_get",
        "queue_list",
        "delivery_CAS",
        "register",
        "queued_sessions",
    ]
    .into_iter()
    .zip(small.iter().zip(&large))
    {
        eprintln!("S5_SCALE {label} 1x={small:?} 10x={large:?}");
        let mut comparable = *large;
        if label == "register" {
            // Only the newly generated clock can differ, by at most ten
            // bytes (AutoSi precision); work/rows/statements must be exact.
            assert!(small.bytes_encoded.abs_diff(large.bytes_encoded) <= 10);
            assert!(small.bytes_written.abs_diff(large.bytes_written) <= 10);
            comparable.bytes_encoded = small.bytes_encoded;
            comparable.bytes_written = small.bytes_written;
        }
        assert_eq!(
            *small, comparable,
            "{label} grows with retained identity history"
        );
    }
}

#[test]
fn sql_expiry_matches_previous_bookkeeping_policy_and_rolls_back_on_error() {
    let root = root();
    let repo = UserInputRepository::open(&root).unwrap();
    repo.enqueue("s", "thread", "request", &params("queued"))
        .unwrap();
    let old = (Utc::now() - chrono::Duration::days(40)).to_rfc3339();
    repo.mutate(|store| {
        let state = store.sessions.get_mut("s").unwrap();
        for (id, delivery) in [
            ("expired", None),
            (
                "accepted-old",
                Some(InputDelivery::Accepted {
                    thread_id: "t".into(),
                    turn_id: None,
                    accepted_at: old.clone(),
                }),
            ),
            (
                "sending",
                Some(InputDelivery::Sending {
                    thread_id: "t".into(),
                    since: old.clone(),
                }),
            ),
            (
                "unknown",
                Some(InputDelivery::Unknown {
                    thread_id: "t".into(),
                    since: old.clone(),
                }),
            ),
            ("queued", None),
        ] {
            state.identities.insert(
                id.into(),
                ClientInputIdentity {
                    canonical_sha256: "hash".into(),
                    first_seen_at: old.clone(),
                    delivery,
                },
            );
        }
        Ok(())
    })
    .unwrap();
    let failed: anyhow::Result<()> =
        repo.mutate_session("s", &[], true, |_| anyhow::bail!("refused mutation"));
    assert!(failed.is_err());
    assert!(repo.dump().sessions["s"].identities.contains_key("expired"));
    repo.register_identity("other", &params("trigger")).unwrap();
    let state = repo.dump();
    let identities = &state.sessions["s"].identities;
    for id in ["expired", "accepted-old"] {
        assert!(!identities.contains_key(id));
    }
    for id in ["sending", "unknown", "queued"] {
        assert!(identities.contains_key(id));
    }
    drop(repo);
    fs::remove_dir_all(root).unwrap();
}

pub(crate) fn test_dump(sql: &Connection) -> anyhow::Result<UserInputStateStore> {
    let mut store = UserInputStateStore::default();
    let mut stmt = sql.prepare("SELECT session FROM owner_sessions ORDER BY session")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let (state, _) = selected(sql, &id, &[], true)?;
        let mut state = state.unwrap();
        let mut stmt =
            sql.prepare("SELECT id,data FROM owner_identities WHERE session=?1 ORDER BY id")?;
        let mut rows = stmt.query([&id])?;
        while let Some(row) = rows.next()? {
            state.identities.insert(row.get(0)?, decode(row.get(1)?)?);
        }
        store.sessions.insert(id, state);
    }
    Ok(store)
}

#[test]
fn session_revision_has_no_delete_recreate_aba_and_ignores_other_sessions() {
    let root = root();
    let repo = UserInputRepository::open(&root).unwrap();
    repo.register_identity("s", &params("one")).unwrap();
    let (before, rev) = repo
        .db
        .read(|tx| selected(tx.sql(), "s", &[], true))
        .unwrap();
    repo.register_identity("other", &params("two")).unwrap();
    assert_eq!(repo.db.read(|tx| revision(tx.sql(), "s")).unwrap(), rev);
    repo.clear_session("s").unwrap();
    repo.register_identity("s", &params("recreated")).unwrap();
    assert!(repo.db.read(|tx| revision(tx.sql(), "s")).unwrap() > rev);
    let stale = repo
        .db
        .write(|tx| apply(tx, "s", rev, before.as_ref(), before.as_ref()))
        .unwrap_err();
    assert!(stale.downcast_ref::<QueueRevisionConflict>().is_some());
    assert_eq!(
        repo.register_identity("s", &params("recreated")).unwrap(),
        ClientIdentityDecision::Existing
    );
    drop(repo);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn admission_fallback_read_never_expires_identity_bookkeeping() {
    let root = root();
    let repo = UserInputRepository::open(&root).unwrap();
    repo.register_identity("s", &params("old")).unwrap();
    repo.mutate(|store| {
        store
            .sessions
            .get_mut("s")
            .unwrap()
            .identities
            .get_mut("old")
            .unwrap()
            .first_seen_at = (Utc::now() - chrono::Duration::days(40)).to_rfc3339();
        Ok(())
    })
    .unwrap();
    let asks = crate::asks::store::AskState::default();
    repo.list_admission_in("s", &asks).unwrap();
    assert!(
        repo.dump().sessions["s"].identities.contains_key("old"),
        "a listing is not an expiry mutation"
    );
    let (result, counts) = store::metrics::measure(|| repo.list_admission_in("s", &asks));
    result.unwrap();
    assert_eq!(
        counts.bytes_encoded, 0,
        "unchanged fallback must not encode a row"
    );
    repo.register_identity("other", &params("real-mutation"))
        .unwrap();
    assert!(!repo.dump().sessions["s"].identities.contains_key("old"));
    drop(repo);
    fs::remove_dir_all(root).unwrap();
}
